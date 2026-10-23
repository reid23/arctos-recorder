//! A camera's live previews: stitched into a panorama preview here when
//! stitching locally, or forwarded to the cloud server to be stitched and
//! restreamed there.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use protocol::Side;
use stitching::cloud::LiveConfig;
use stitching::encode::LiveEncoding;
use stitching::geometry::StitchConfig;
use stitching::gpu::GpuChoice;
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
use crate::performance::PreviewDelivery;
use crate::server::Feed;
use crate::settings::{Settings, Stitching};

/// Bit rate of the previews forwarded to the cloud server.
const FORWARD_BITRATE: &str = "6M";
/// How often the live stitching steps are logged.
const STITCH_TIMING: Duration = Duration::from_secs(5);

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
///
/// Decoding, stitching and compressing each run on their own thread, and
/// the next frame is uploaded while the previous one finishes on the GPU,
/// so the preview keeps up with whichever step is slowest.
fn stitch_locally(
    runtime: &Handle,
    hub: &Hub,
    camera: &str,
    link: &CameraLink,
    overlay: &watch::Receiver<Option<Arc<PlacedImage>>>,
) -> Result<()> {
    let (decoded_tx, decoded_rx) = mpsc::sync_channel(1);
    let (stitched_tx, stitched_rx) = mpsc::sync_channel(1);
    let (errors_tx, errors_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let stitch_errors = errors_tx.clone();
        std::thread::Builder::new()
            .name(format!("stitch-{camera}"))
            .spawn_scoped(scope, move || {
                if let Err(error) = stitch_decoded(decoded_rx, stitched_tx) {
                    let _ = stitch_errors.send(error);
                }
            })
            .context("starting stitching")?;
        let panoramas = link.panorama.clone();
        let timing_camera = camera.to_owned();
        std::thread::Builder::new()
            .name(format!("compress-{camera}"))
            .spawn_scoped(scope, move || {
                if let Err(error) = compress_panoramas(&timing_camera, stitched_rx, &panoramas) {
                    let _ = errors_tx.send(error);
                }
            })
            .context("starting compressing")?;
        feed_previews(runtime, hub, camera, link, overlay, decoded_tx, &errors_rx)
    })
}

struct DecodedPreview {
    frame: StereoYuv,
    config: Arc<StitchConfig>,
    output_size: (u32, u32),
    gpu: GpuChoice,
    overlay: Option<Arc<PlacedImage>>,
    decode: Duration,
}

struct StitchedPreview {
    yuv: Vec<u8>,
    size: (u32, u32),
    decode: Duration,
    stitch: Duration,
}

/// A frame submitted to the GPU and not yet read back.
struct HeldFrame {
    decode: Duration,
    upload: Duration,
    output_size: (u32, u32),
}

struct StitchState {
    stitcher: LiveStitcher,
    config: Arc<StitchConfig>,
    gpu: GpuChoice,
    output_size: (u32, u32),
    preview_size: (u32, u32),
    held: Option<HeldFrame>,
}

impl StitchState {
    fn open(preview: &DecodedPreview) -> Result<Self> {
        let preview_size = preview.frame.preview_size();
        Ok(StitchState {
            stitcher: LiveStitcher::new(&preview.config, preview_size, preview.output_size, preview.gpu)?,
            config: Arc::clone(&preview.config),
            gpu: preview.gpu,
            output_size: preview.output_size,
            preview_size,
            held: None,
        })
    }
}

/// Paced by the left camera; the right one is never more than a frame apart.
fn feed_previews(
    runtime: &Handle,
    hub: &Hub,
    camera: &str,
    link: &CameraLink,
    overlay: &watch::Receiver<Option<Arc<PlacedImage>>>,
    decoded: mpsc::SyncSender<DecodedPreview>,
    errors: &mpsc::Receiver<anyhow::Error>,
) -> Result<()> {
    let mut config = link.stitch.subscribe();
    let mut left = link.previews[Side::Left.index()].subscribe();
    let right = link.previews[Side::Right.index()].subscribe();
    let mut decompressors = [Decompressor::new()?, Decompressor::new()?];
    let mut baseline = link.preview_frames.load(Ordering::Acquire);
    let mut arrived_total = 0u64;
    let mut delivered_total = 0u64;
    let mut preview_delivery = PreviewDelivery::default();
    let mut publishing = false;

    loop {
        runtime.block_on(left.changed())?;
        if let Ok(error) = errors.try_recv() {
            return Err(error);
        }
        let (publishes, output_size, gpu) = {
            let settings = hub.settings.borrow();
            (
                settings.stitching == Stitching::Local,
                settings.live_size(camera),
                settings.gpu,
            )
        };
        let active = publishes || hub.calibrating.borrow().contains(camera);
        if publishes != publishing {
            publish_dropped_previews(hub, camera, None);
            preview_delivery.clear();
        }
        publishing = publishes;

        if !active {
            baseline = link.preview_frames.load(Ordering::Acquire);
            continue;
        }
        let Some(previews) = latest(&mut left, &right) else {
            continue;
        };
        let Some(preview_size) = live::pair_size([&previews[0], &previews[1]])? else {
            continue;
        };
        let frames_now = link.preview_frames.load(Ordering::Acquire);
        let mut frame = StereoYuv::new(preview_size);
        let decode_started = Instant::now();
        decode(&mut decompressors, &mut frame, &previews)?;
        let preview = DecodedPreview {
            frame,
            config: Arc::clone(&config.borrow_and_update()),
            output_size,
            gpu,
            overlay: overlay.borrow().clone(),
            decode: decode_started.elapsed(),
        };
        let arrived = frames_now.saturating_sub(baseline);
        baseline = frames_now;
        if decoded.send(preview).is_err() {
            return Err(stitching_stopped(errors));
        }
        arrived_total += arrived;
        delivered_total += 1;
        if publishes
            && let Some(dropped) = preview_delivery.dropped(Instant::now(), arrived_total, delivered_total)
        {
            publish_dropped_previews(hub, camera, Some(dropped));
        }
    }
}

fn stitch_decoded(
    decoded: mpsc::Receiver<DecodedPreview>,
    stitched: mpsc::SyncSender<StitchedPreview>,
) -> Result<()> {
    let mut current: Option<StitchState> = None;
    while let Ok(preview) = decoded.recv() {
        let reusable = current.as_ref().is_some_and(|stitching| {
            stitching.gpu == preview.gpu
                && stitching.output_size == preview.output_size
                && stitching.preview_size == preview.frame.preview_size()
        });
        if !reusable {
            if let Some(stitching) = &mut current {
                finish_held(stitching, &stitched)?;
            }
            current = Some(StitchState::open(&preview)?);
        }
        let stitching = current.as_mut().expect("the stitcher was just opened");
        if stitching.config != preview.config {
            finish_held(stitching, &stitched)?;
            if let Err(error) = stitching.stitcher.configure(&preview.config) {
                tracing::warn!("keeping the last stitching config: {error:#}");
            }
            stitching.config = Arc::clone(&preview.config);
        }
        let upload_started = Instant::now();
        stitching.stitcher.start(&preview.frame, preview.overlay.as_ref())?;
        let upload = upload_started.elapsed();
        if let Some(held) = stitching.held.take() {
            let wait_started = Instant::now();
            let yuv = stitching.stitcher.take()?;
            stitched
                .send(StitchedPreview {
                    yuv,
                    size: held.output_size,
                    decode: held.decode,
                    stitch: held.upload + wait_started.elapsed(),
                })
                .map_err(|_| anyhow!("compressing the panorama stopped"))?;
        }
        stitching.held = Some(HeldFrame {
            decode: preview.decode,
            upload,
            output_size: preview.output_size,
        });
    }
    if let Some(stitching) = &mut current {
        finish_held(stitching, &stitched)?;
    }
    Ok(())
}

fn finish_held(stitching: &mut StitchState, stitched: &mpsc::SyncSender<StitchedPreview>) -> Result<()> {
    let Some(held) = stitching.held.take() else {
        return Ok(());
    };
    let wait_started = Instant::now();
    let yuv = stitching.stitcher.take()?;
    stitched
        .send(StitchedPreview {
            yuv,
            size: held.output_size,
            decode: held.decode,
            stitch: held.upload + wait_started.elapsed(),
        })
        .map_err(|_| anyhow!("compressing the panorama stopped"))?;
    Ok(())
}

fn compress_panoramas(
    camera: &str,
    stitched: mpsc::Receiver<StitchedPreview>,
    panoramas: &watch::Sender<Option<Bytes>>,
) -> Result<()> {
    let mut compressor = live::panorama_compressor()?;
    let mut timings = StageTimings::default();
    while let Ok(preview) = stitched.recv() {
        let started = Instant::now();
        let jpeg = live::panorama_jpeg(&mut compressor, &preview.yuv, preview.size)?;
        timings.record(camera, preview.decode, preview.stitch, started.elapsed());
        panoramas.send_replace(Some(jpeg));
    }
    Ok(())
}

#[derive(Default)]
struct StageTimings {
    decode: Duration,
    stitch: Duration,
    compress: Duration,
    frames: u32,
    started: Option<Instant>,
}

impl StageTimings {
    fn record(&mut self, camera: &str, decode: Duration, stitch: Duration, compress: Duration) {
        self.decode += decode;
        self.stitch += stitch;
        self.compress += compress;
        self.frames += 1;
        let started = *self.started.get_or_insert_with(Instant::now);
        if started.elapsed() < STITCH_TIMING {
            return;
        }
        let frames = f64::from(self.frames);
        let millis = |duration: Duration| duration.as_secs_f64() * 1e3 / frames;
        tracing::info!(
            camera,
            decode_ms = millis(self.decode),
            stitch_ms = millis(self.stitch),
            compress_ms = millis(self.compress),
            fps = frames / started.elapsed().as_secs_f64(),
            "live stitching"
        );
        *self = StageTimings::default();
    }
}

fn publish_dropped_previews(hub: &Hub, camera: &str, dropped: Option<f64>) {
    hub.cameras.send_if_modified(|cameras| {
        let Some(status) = cameras.get_mut(camera) else {
            return false;
        };
        let changed = match (status.previews_dropped, dropped) {
            (Some(before), Some(now)) => (before - now).abs() >= 0.001,
            (before, now) => before.is_some() != now.is_some(),
        };
        if changed {
            status.previews_dropped = dropped;
        }
        changed
    });
}

fn stitching_stopped(errors: &mpsc::Receiver<anyhow::Error>) -> anyhow::Error {
    errors
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or_else(|_| anyhow!("live stitching stopped"))
}

/// Clears the dropped-preview readout when forwarding to the cloud ends,
/// unless stitching has since moved onto this computer.
struct ClearDroppedPreviews<'a> {
    hub: &'a Hub,
    camera: &'a str,
}

impl Drop for ClearDroppedPreviews<'_> {
    fn drop(&mut self) {
        if self.hub.settings.borrow().stitching == Stitching::Cloud {
            publish_dropped_previews(self.hub, self.camera, None);
        }
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
                result = forward(&hub, &camera, &link, &forwarding) => {
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
async fn forward(hub: &Hub, camera: &str, link: &CameraLink, forwarding: &Forwarding) -> Result<()> {
    let _clear = ClearDroppedPreviews { hub, camera };
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
        let mut baseline = link.preview_frames.load(Ordering::Acquire);
        let mut arrived_total = 0u64;
        let mut delivered_total = 0u64;
        let mut preview_delivery = PreviewDelivery::default();
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
            let frames_now = link.preview_frames.load(Ordering::Acquire);
            let arrived = frames_now.saturating_sub(baseline);
            baseline = frames_now;
            let decoded;
            (decoded, decoder) = tokio::task::spawn_blocking(move || {
                let (mut decompressors, mut frame) = decoder;
                let decoded = decode(&mut decompressors, &mut frame, &previews);
                (decoded, (decompressors, frame))
            })
            .await?;
            decoded?;
            arrived_total += arrived;
            delivered_total += 1;
            if let Some(dropped) = preview_delivery.dropped(Instant::now(), arrived_total, delivered_total) {
                publish_dropped_previews(hub, camera, Some(dropped));
            }
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
