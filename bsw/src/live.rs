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
use stitching::encode::LiveEncoding;
use stitching::live::{self, LiveStitcher, StereoYuv};
use stitching::overlay::{self, LiveOverlay, PlacedImage};
use stitching::restream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use turbojpeg::Decompressor;

use crate::cloud::Cloud;
use crate::hub::{CameraLink, Hub};
use crate::server::Feed;
use crate::settings::{Settings, Stitching};

/// Bit rate of the previews forwarded to the cloud server.
const FORWARD_BITRATE: &str = "6M";

pub fn spawn(hub: Arc<Hub>, camera: String, link: Arc<CameraLink>) {
    let runtime = Handle::current();
    let (overlay_images, overlay_images_rx) = watch::channel(None);
    let (thread_hub, thread_camera, thread_link) = (hub.clone(), camera.clone(), link.clone());
    std::thread::Builder::new()
        .name(format!("live-{camera}"))
        .spawn(move || {
            let stitched = stitch_locally(
                &runtime,
                &thread_hub,
                &thread_camera,
                &thread_link,
                &overlay_images_rx,
            );
            if let Err(e) = stitched {
                tracing::error!("live stitching stopped: {e:#}");
            }
        })
        .expect("spawn live stitching thread");
    tokio::spawn(show_overlay(hub.clone(), camera.clone(), overlay_images));
    tokio::spawn(forward_to_cloud(hub, camera, link));
}

/// Keeps `images` showing the scoreboard to draw over the locally stitched
/// preview.
async fn show_overlay(
    hub: Arc<Hub>,
    camera: String,
    images: watch::Sender<Option<Arc<PlacedImage>>>,
) {
    let wanted = |settings: &Settings| {
        settings
            .live_overlay(&camera)
            .filter(|_| settings.stitching == Stitching::Local)
            .map(|overlay| (overlay, settings.live_size(&camera)))
    };
    let mut settings = hub.settings.subscribe();
    let (config, config_rx) = watch::channel(wanted(&settings.borrow_and_update()));
    let follow_settings = async {
        while settings.changed().await.is_ok() {
            let overlay = wanted(&settings.borrow_and_update());
            config.send_if_modified(|current| {
                let changed = *current != overlay;
                *current = overlay;
                changed
            });
        }
    };
    tokio::join!(
        follow_settings,
        overlay::follow_live(&hub.arctos, &hub.chromium, config_rx, &images)
    );
}

/// Stitches the previews into [`CameraLink::panorama`] while stitching
/// locally or calibrating the camera. The GPU is only set up once it is
/// needed.
fn stitch_locally(
    runtime: &Handle,
    hub: &Hub,
    camera: &str,
    link: &CameraLink,
    overlay: &watch::Receiver<Option<Arc<PlacedImage>>>,
) -> Result<()> {
    let mut config = link.stitch.subscribe();
    let mut left = link.previews[Side::Left.index()].subscribe();
    let right = link.previews[Side::Right.index()].subscribe();
    let mut stitcher: Option<LiveStitcher> = None;
    let mut decompressors = [Decompressor::new()?, Decompressor::new()?];
    let mut frame: Option<StereoYuv> = None;

    loop {
        // Paced by the left camera; the right one is never more than a frame apart.
        runtime.block_on(left.changed())?;
        if hub.settings.borrow().stitching != Stitching::Local
            && !hub.calibrating.borrow().contains(camera)
        {
            continue;
        }
        let Some(previews) = latest(&mut left, &right) else {
            continue;
        };
        let Some(preview_size) = live::pair_size([&previews[0], &previews[1]])? else {
            continue;
        };
        let size = hub.settings.borrow().live_size(camera);
        if stitcher.as_ref().is_some_and(|stitcher| {
            (stitcher.preview_size(), stitcher.output_size()) != (preview_size, size)
        }) {
            stitcher = None;
        }
        let stitcher = match &mut stitcher {
            Some(stitcher) => {
                if config.has_changed()?
                    && let Err(e) = stitcher.configure(&config.borrow_and_update())
                {
                    tracing::warn!("keeping the last stitching config: {e:#}");
                }
                stitcher
            }
            None => stitcher.insert(LiveStitcher::new(
                &config.borrow_and_update(),
                preview_size,
                size,
            )?),
        };
        if frame.as_ref().is_some_and(|frame| frame.preview_size() != preview_size) {
            frame = None;
        }
        let frame = frame.get_or_insert_with(|| StereoYuv::new(preview_size));
        decode(&mut decompressors, frame, &previews)?;
        let overlay = overlay.borrow().clone();
        let jpeg = stitcher.stitch(frame, overlay.as_ref())?;
        link.panorama.send_replace(Some(jpeg));
    }
}

type Preview = watch::Receiver<Option<Bytes>>;

/// The latest pair of previews; `None` until both cameras have sent one.
fn latest(left: &mut Preview, right: &Preview) -> Option<[Bytes; 2]> {
    Some([left.borrow_and_update().clone()?, right.borrow().clone()?])
}

fn decode(
    decompressors: &mut [Decompressor; 2],
    frame: &mut StereoYuv,
    [left, right]: &[Bytes; 2],
) -> Result<()> {
    frame.decode(decompressors, [left, right])
}

/// Where to forward the previews to, if anywhere.
#[derive(Clone, PartialEq)]
struct Forwarding {
    cloud: crate::settings::CloudSettings,
    destination: restream::Destination,
    size: (u32, u32),
    bitrate: f64,
    overlay: Option<LiveOverlay>,
}

impl Forwarding {
    fn from_settings(settings: &Settings, camera: &str) -> Option<Self> {
        (settings.stitching == Stitching::Cloud).then_some(())?;
        Some(Forwarding {
            cloud: settings.cloud.clone(),
            destination: settings.stream_destination(camera)?,
            size: settings.live_size(camera),
            bitrate: settings.live.bitrate,
            overlay: settings.live_overlay(camera),
        })
    }
}

/// Forwards the previews to the cloud server while stitching in the cloud
/// and restreaming, reconnecting whenever where to forward them or their
/// size changes.
async fn forward_to_cloud(hub: Arc<Hub>, camera: String, link: Arc<CameraLink>) {
    let mut settings = hub.settings.subscribe();
    loop {
        let forwarding = Forwarding::from_settings(&settings.borrow_and_update(), &camera);
        let current = forwarding.clone();
        let changed = async {
            while settings.changed().await.is_ok() {
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
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
                _ = changed => {}
            },
        }
    }
}

/// Forwards the previews until something fails, or until their size changes
/// and the cloud server must be told.
async fn forward(camera: &str, link: &CameraLink, forwarding: &Forwarding) -> Result<()> {
    let mut left = link.previews[Side::Left.index()].subscribe();
    let right = link.previews[Side::Right.index()].subscribe();
    let preview_size = loop {
        left.changed().await?;
        if let Some(previews) = latest(&mut left, &right)
            && let Some(preview_size) = live::pair_size([&previews[0], &previews[1]])?
        {
            break preview_size;
        }
    };

    let cloud = Cloud::new(&forwarding.cloud);
    let (mut socket, _) = tokio_tungstenite::connect_async(cloud.live_request(camera)?).await?;
    let mut stitch = link.stitch.subscribe();
    let config = LiveConfig {
        stitch: (**stitch.borrow_and_update()).clone(),
        preview_size,
        size: forwarding.size,
        destination: forwarding.destination.clone(),
        bitrate: forwarding.bitrate,
        overlay: forwarding.overlay.clone(),
    };
    socket
        .send(Message::text(serde_json::to_string(&config)?))
        .await?;
    tracing::info!("forwarding {camera} to the cloud");

    let frame = StereoYuv::new(preview_size);
    let (width, height) = frame.stacked_size();
    let fps = live::FPS.to_string();
    let encoding = LiveEncoding::find().await?;
    let mut encoder = Command::new(stitching::ffmpeg_program())
        .args(["-loglevel", "error"])
        .args(encoding.input_args)
        .args(["-use_wallclock_as_timestamps", "1"])
        .args([
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-color_range",
            "pc",
        ])
        .args(["-video_size", &format!("{width}x{height}"), "-i", "-"])
        .args(restream::Audio::Pcm(&Feed::Audio.url(camera)).input_args())
        .args(["-map", "0:v"])
        .args(restream::AUDIO_OUTPUT_ARGS)
        .args(encoding.output_args)
        .args(["-b:v", FORWARD_BITRATE, "-g", &fps, "-color_range", "pc"])
        .args(["-fps_mode", "passthrough", "-f", "mpegts", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg")?;
    let mut raw = encoder.stdin.take().expect("piped stdin");
    let mut encoded = encoder.stdout.take().expect("piped stdout");

    let (sink, mut incoming) = socket.split();
    let sink = tokio::sync::Mutex::new(sink);
    let compose = async {
        let mut decoder = ([Decompressor::new()?, Decompressor::new()?], frame);
        loop {
            left.changed().await?;
            let Some(previews) = latest(&mut left, &right) else {
                continue;
            };
            match live::pair_size([&previews[0], &previews[1]])? {
                Some(size) if size == preview_size => {}
                Some((width, height)) => {
                    tracing::info!("{camera}'s previews are now {width} by {height}");
                    return Ok(());
                }
                None => continue,
            }
            let decoded;
            (decoded, decoder) = tokio::task::spawn_blocking(move || {
                let (mut decompressors, mut frame) = decoder;
                let decoded = decode(&mut decompressors, &mut frame, &previews);
                (decoded, (decompressors, frame))
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
            sink.lock()
                .await
                .send(Message::binary(chunk[..len].to_vec()))
                .await?;
        }
    };
    let update_config = async {
        while stitch.changed().await.is_ok() {
            let config = serde_json::to_string(&**stitch.borrow_and_update())?;
            sink.lock().await.send(Message::text(config)).await?;
        }
        anyhow::Ok(())
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
        result = update_config => result,
        result = receive => result,
    }
}
