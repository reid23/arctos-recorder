//! A camera's live previews: stitched into a panorama preview here when
//! stitching locally, or forwarded to the cloud server to be stitched and
//! restreamed there.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use protocol::rig;
use stitching::cloud::{CloudTraffic, LiveConfig, LiveUpdate};
use stitching::encode::LiveEncoding;
use stitching::live::{self, LiveStitcher, StereoYuv};
use stitching::overlay::{self, PlacedImage};
use stitching::restream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use turbojpeg::Decompressor;

use crate::cloud::Cloud;
use crate::hub::{CameraLink, Hub, StereoPreview};
use crate::server::Feed;
use crate::settings::{Settings, Stitching};

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
    let mut previews = link.live.subscribe();
    let mut stitcher: Option<LiveStitcher> = None;
    let mut decompressors = [Decompressor::new()?, Decompressor::new()?];
    let mut frame: Option<StereoYuv> = None;

    loop {
        let preview = match runtime.block_on(previews.recv()) {
            Ok(preview) => preview,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let Some(preview) = newest_preview(&mut previews) else {
                    continue;
                };
                preview
            }
            Err(error) => return Err(error.into()),
        };
        if hub.settings.borrow().stitching != Stitching::Local
            && !hub.calibrating.borrow().contains(camera)
        {
            continue;
        }
        let Some(preview_size) = live::pair_size([&preview.jpegs[0], &preview.jpegs[1]])? else {
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
        decode(&mut decompressors, frame, &preview.jpegs)?;
        let overlay = overlay.borrow().clone();
        let jpeg = stitcher.stitch(frame, overlay.as_ref())?;
        link.panorama.send_replace(Some(jpeg));
    }
}

fn newest_preview(receiver: &mut tokio::sync::broadcast::Receiver<StereoPreview>) -> Option<StereoPreview> {
    let mut newest = None;
    while let Ok(preview) = receiver.try_recv() {
        newest = Some(preview);
    }
    newest
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
    forward_bitrate: f64,
}

impl Forwarding {
    fn from_settings(settings: &Settings, camera: &str) -> Option<Self> {
        (settings.stitching == Stitching::Cloud).then_some(())?;
        Some(Forwarding {
            cloud: settings.cloud.clone(),
            destination: settings.stream_destination(camera)?,
            size: settings.live_size(camera),
            bitrate: settings.live.bitrate,
            forward_bitrate: settings.live.forward_bitrate,
        })
    }
}

/// Forwards the previews to the cloud server while stitching in the cloud
/// and restreaming, reconnecting whenever where to forward them or their
/// size changes. Overlay changes go over the open connection.
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
                result = forward(
                    &camera,
                    &link,
                    &forwarding,
                    hub.settings.subscribe(),
                    hub.network.clone(),
                ) => {
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

/// Forwards previews until something fails or the destination changes.
async fn forward(
    camera: &str,
    link: &CameraLink,
    forwarding: &Forwarding,
    mut settings: watch::Receiver<Settings>,
    network: Arc<crate::network::Network>,
) -> Result<()> {
    let mut previews = link.live.subscribe();
    let first = loop {
        match previews.recv().await {
            Ok(preview) => break preview,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                if let Some(preview) = newest_preview(&mut previews) {
                    break preview;
                }
            }
            Err(error) => return Err(error.into()),
        }
    };
    let Some(source_size) = live::pair_size([&first.jpegs[0], &first.jpegs[1]])? else {
        anyhow::bail!("the camera preview sizes differ");
    };
    let preview_size = rig::DEFAULT_PREVIEW_SIZE;

    let cloud = Cloud::new(&forwarding.cloud, network.cloud());
    let (mut socket, _) = tokio_tungstenite::connect_async(cloud.live_request(camera)?).await?;
    let mut stitch = link.stitch.subscribe();
    let mut overlay = settings.borrow_and_update().live_overlay(camera);
    let config = LiveConfig {
        stitch: (**stitch.borrow_and_update()).clone(),
        preview_size,
        size: forwarding.size,
        destination: forwarding.destination.clone(),
        bitrate: forwarding.bitrate,
        overlay: overlay.clone(),
    };
    let config = serde_json::to_string(&config)?;
    network.cloud().add_sent(config.len() as u64);
    socket.send(Message::text(config)).await?;
    tracing::info!("forwarding {camera} to the cloud");

    let frame = StereoYuv::new(source_size);
    let (width, height) = frame.stacked_size();
    let fps = live::FPS.to_string();
    let encoding = LiveEncoding::find().await?;
    let scale = format!(
        "scale={}:{}:in_range=pc:out_range=pc",
        preview_size.0,
        preview_size.1 * 2
    );
    let mut output_args: Vec<String> = encoding.output_args.iter().map(|argument| argument.to_string()).collect();
    if let Some(filter) = output_args.iter().position(|argument| argument == "-vf") {
        output_args[filter + 1] = format!("{scale},{}", output_args[filter + 1]);
    } else {
        output_args.splice(0..0, ["-vf".to_owned(), scale]);
    }
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
            "-framerate",
            "1000",
        ])
        .args(["-video_size", &format!("{width}x{height}"), "-i", "-"])
        .args(restream::Audio::Pcm(&Feed::Audio.url(camera)).input_args())
        .args(["-map", "0:v"])
        .args(restream::AUDIO_OUTPUT_ARGS)
        .args(output_args)
        .args(restream::bitrate_args(forwarding.forward_bitrate))
        .args(["-g", &fps, "-color_range", "pc"])
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
        let mut first = Some(first);
        loop {
            let preview = match first.take() {
                Some(preview) => preview,
                None => match previews.recv().await {
                    Ok(preview) => preview,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let Some(preview) = newest_preview(&mut previews) else {
                            continue;
                        };
                        preview
                    }
                    Err(error) => return Err(error.into()),
                },
            };
            match live::pair_size([&preview.jpegs[0], &preview.jpegs[1]])? {
                Some(size) if size == source_size => {}
                Some((width, height)) => {
                    tracing::info!("{camera}'s previews are now {width} by {height}");
                    return Ok(());
                }
                None => continue,
            }
            let decoded;
            (decoded, decoder) = tokio::task::spawn_blocking(move || {
                let (mut decompressors, mut frame) = decoder;
                let decoded = decode(&mut decompressors, &mut frame, &preview.jpegs);
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
            network.cloud().add_sent(len as u64);
            sink.lock()
                .await
                .send(Message::binary(chunk[..len].to_vec()))
                .await?;
        }
    };
    let update_config = async {
        loop {
            tokio::select! {
                changed = stitch.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    let update = LiveUpdate::Stitch((**stitch.borrow_and_update()).clone());
                    let config = serde_json::to_string(&update)?;
                    network.cloud().add_sent(config.len() as u64);
                    sink.lock().await.send(Message::text(config)).await?;
                }
                changed = settings.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    let next = settings.borrow().live_overlay(camera);
                    if next == overlay {
                        continue;
                    }
                    overlay = next;
                    let update = LiveUpdate::Overlay { overlay: overlay.clone() };
                    let text = serde_json::to_string(&update)?;
                    network.cloud().add_sent(text.len() as u64);
                    sink.lock().await.send(Message::text(text)).await?;
                }
            }
        }
    };
    let receive = async {
        while let Some(message) = incoming.next().await {
            let message = message?;
            let bytes = match &message {
                Message::Binary(data) => data.len(),
                Message::Text(text) => text.len(),
                _ => 0,
            };
            network.cloud().add_received(bytes as u64);
            if let Message::Text(text) = &message
                && let Ok(traffic) = serde_json::from_str::<CloudTraffic>(text)
            {
                network.note_cloud(camera, traffic);
            }
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
