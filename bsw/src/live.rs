//! A camera's live previews: stitched into a panorama preview here when
//! stitching locally, or forwarded to the cloud server to be stitched and
//! restreamed there.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use protocol::Side;
use stitching::cloud::LiveConfig;
use stitching::live::{self, LiveStitcher, StereoYuv};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use turbojpeg::Decompressor;

use crate::cloud::Cloud;
use crate::hub::{CameraLink, Hub};
use crate::settings::{Settings, Stitching};

/// Bit rate of the previews forwarded to the cloud server.
const FORWARD_BITRATE: &str = "6M";

pub fn spawn(hub: Arc<Hub>, camera: String, link: Arc<CameraLink>) {
    let runtime = Handle::current();
    let (thread_hub, thread_link) = (hub.clone(), link.clone());
    std::thread::Builder::new()
        .name(format!("live-{camera}"))
        .spawn(move || {
            if let Err(e) = stitch_locally(&runtime, &thread_hub, &thread_link) {
                tracing::error!("live stitching stopped: {e:#}");
            }
        })
        .expect("spawn live stitching thread");
    tokio::spawn(forward_to_cloud(hub, camera, link));
}

/// Stitches the previews into [`CameraLink::panorama`] while stitching
/// locally. The GPU is only set up once it is needed.
fn stitch_locally(runtime: &Handle, hub: &Hub, link: &CameraLink) -> Result<()> {
    let mut calibration = link.calibration.subscribe();
    let mut left = link.previews[Side::Left.index()].subscribe();
    let right = link.previews[Side::Right.index()].subscribe();
    let mut stitcher: Option<LiveStitcher> = None;
    let mut decompressor = Decompressor::new()?;
    let mut frame = StereoYuv::new();

    loop {
        // Paced by the left camera; the right one is never more than a frame apart.
        runtime.block_on(left.changed())?;
        if hub.settings.borrow().stitching != Stitching::Local {
            continue;
        }
        let stitcher = match &mut stitcher {
            Some(stitcher) => {
                if calibration.has_changed()? {
                    stitcher.set_calibration(&calibration.borrow_and_update());
                }
                stitcher
            }
            None => stitcher.insert(LiveStitcher::new(&calibration.borrow_and_update())?),
        };
        let Some(previews) = latest(&mut left, &right) else {
            continue;
        };
        decode(&mut decompressor, &mut frame, &previews)?;
        if let Some(jpeg) = stitcher.stitch(&frame)? {
            link.panorama.send_replace(Some(jpeg));
        }
    }
}

type Preview = watch::Receiver<Option<Bytes>>;

/// The latest pair of previews; `None` until both cameras have sent one.
fn latest(left: &mut Preview, right: &Preview) -> Option<[Bytes; 2]> {
    Some([left.borrow_and_update().clone()?, right.borrow().clone()?])
}

fn decode(
    decompressor: &mut Decompressor,
    frame: &mut StereoYuv,
    [left, right]: &[Bytes; 2],
) -> Result<()> {
    frame.decode(decompressor, Side::Left, left)?;
    frame.decode(decompressor, Side::Right, right)
}

/// Where to forward the previews to, if anywhere.
#[derive(Clone, PartialEq)]
struct Forwarding {
    cloud: crate::settings::CloudSettings,
    stream_url: String,
}

impl Forwarding {
    fn from_settings(settings: &Settings, camera: &str) -> Option<Self> {
        (settings.stitching == Stitching::Cloud).then_some(())?;
        Some(Forwarding {
            cloud: settings.cloud.clone(),
            stream_url: settings.stream_url(camera)?.to_owned(),
        })
    }
}

/// Forwards the previews to the cloud server while stitching in the cloud
/// and restreaming, reconnecting whenever the settings or the calibration
/// change.
async fn forward_to_cloud(hub: Arc<Hub>, camera: String, link: Arc<CameraLink>) {
    let mut settings = hub.settings.subscribe();
    let mut calibration = link.calibration.subscribe();
    loop {
        let forwarding = Forwarding::from_settings(&settings.borrow_and_update(), &camera);
        calibration.mark_unchanged();
        let current = forwarding.clone();
        let changed = async {
            loop {
                tokio::select! {
                    _ = settings.changed() => {}
                    _ = calibration.changed() => return,
                }
                if Forwarding::from_settings(&settings.borrow_and_update(), &camera) != current {
                    return;
                }
            }
        };
        match forwarding {
            None => changed.await,
            Some(forwarding) => tokio::select! {
                result = forward(&camera, &link, &forwarding) => {
                    if let Err(e) = result {
                        tracing::warn!("forwarding {camera} to the cloud: {e:#}");
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                _ = changed => {}
            },
        }
    }
}

async fn forward(camera: &str, link: &CameraLink, forwarding: &Forwarding) -> Result<()> {
    let cloud = Cloud::new(&forwarding.cloud);
    let (mut socket, _) = tokio_tungstenite::connect_async(cloud.live_request(camera)?).await?;
    let config = LiveConfig {
        calibration: (**link.calibration.borrow()).clone(),
        stream_url: forwarding.stream_url.clone(),
    };
    socket
        .send(Message::text(serde_json::to_string(&config)?))
        .await?;
    tracing::info!("forwarding {camera} to the cloud");

    let (width, height) = StereoYuv::SIZE;
    let fps = live::FPS.to_string();
    let mut encoder = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-use_wallclock_as_timestamps", "1"])
        .args([
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-color_range",
            "pc",
        ])
        .args(["-video_size", &format!("{width}x{height}"), "-i", "-"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
        ])
        .args(["-b:v", FORWARD_BITRATE, "-g", &fps, "-color_range", "pc"])
        .args(["-fps_mode", "passthrough", "-f", "mpegts", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg")?;
    let mut raw = encoder.stdin.take().expect("piped stdin");
    let mut encoded = encoder.stdout.take().expect("piped stdout");

    let (mut sink, mut incoming) = socket.split();
    let compose = async {
        let mut left = link.previews[Side::Left.index()].subscribe();
        let right = link.previews[Side::Right.index()].subscribe();
        let mut decoder = (Decompressor::new()?, StereoYuv::new());
        loop {
            left.changed().await?;
            let Some(previews) = latest(&mut left, &right) else {
                continue;
            };
            let decoded;
            (decoded, decoder) = tokio::task::spawn_blocking(move || {
                let (mut decompressor, mut frame) = decoder;
                let decoded = decode(&mut decompressor, &mut frame, &previews);
                (decoded, (decompressor, frame))
            })
            .await?;
            decoded?;
            raw.write_all(decoder.1.bytes())
                .await
                .context("encoder stopped")?;
        }
    };
    let send = async {
        let mut chunk = vec![0; 1 << 16];
        loop {
            let len = encoded.read(&mut chunk).await?;
            anyhow::ensure!(len > 0, "encoder stopped");
            sink.send(Message::binary(chunk[..len].to_vec())).await?;
        }
    };
    let receive = async {
        while let Some(message) = incoming.next().await {
            message?;
        }
        anyhow::bail!("the cloud server closed the connection")
    };
    tokio::select! {
        result = compose => result,
        result = send => result,
        result = receive => result,
    }
}
