//! Captures both cameras with libcamera and JPEG-encodes their frames.
//!
//! The ISP produces two streams per frame: the full recording resolution and
//! a preview of the size the base station asks for. One thread owns
//! libcamera and recycles requests; a pool of workers encodes them (the Pi 5
//! has no hardware JPEG encoder).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use crossbeam_channel::{Receiver, Sender, unbounded};
use libcamera::camera::{ActiveCamera, CameraConfigurationStatus};
use libcamera::camera_manager::CameraManager;
use libcamera::color_space::ColorSpace;
use libcamera::control::ControlList;
use libcamera::controls::{AfMode, AfState, AfTrigger, FrameDurationLimits, LensPosition, SensorTimestamp};
use libcamera::formats;
use libcamera::geometry::Size;
use libcamera::request::{Request, RequestStatus, ReuseFlag};
use libcamera::stream::{Stream, StreamRole};
use protocol::{AutofocusOutcome, Focus, PreviewFrame, Side, Timestamp, rig};
use tokio::sync::watch;
use turbojpeg::{Compressor, Subsamp, YuvPlanesImage};

use crate::clock::Clock;
use crate::dma::{self, Buffer, Heap};
use crate::recorder::{CameraFrame, Input};
use crate::stats::FrameCounters;

/// Frame buffers per stream per camera; bounds how far encoding may lag.
const BUFFERS: u32 = 12;
const ENCODERS: usize = 4;
/// How long autofocusing may take before the cameras still scanning count
/// as having found nothing to focus on.
const AUTOFOCUS_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Outputs {
    /// Whether full-resolution frames are wanted (only while a match is on).
    pub recording: Arc<AtomicBool>,
    /// JPEG quality of the full-resolution frames.
    pub record_quality: Arc<AtomicU8>,
    /// The size the previews should be. Changing it restarts capturing, so
    /// it waits until nothing is being recorded.
    pub preview_size: watch::Receiver<(u32, u32)>,
    pub preview_quality: Arc<AtomicU8>,
    /// Where the lenses should be focused; `None` until the base station
    /// says.
    pub focus: watch::Receiver<Option<Focus>>,
    pub autofocus: watch::Sender<Option<AutofocusOutcome>>,
    pub frames: std::sync::mpsc::Sender<Input>,
    pub previews: tokio::sync::mpsc::Sender<PreviewFrame<Vec<u8>>>,
    pub counters: Arc<FrameCounters>,
}

/// Starts capturing in the background. Camera failures are unrecoverable, so
/// they terminate the process (systemd restarts it).
pub fn spawn(clock: Arc<Clock>, outputs: Outputs) {
    thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let error = capture(Arc::new(Encoder { clock, outputs })).unwrap_err();
            tracing::error!("capture failed: {error:#}");
            std::process::exit(1);
        })
        .expect("spawn capture thread");
}

#[derive(Clone, Copy)]
struct Streams {
    record: Stream,
    preview: Stream,
    preview_size: (u32, u32),
}

enum Event {
    Completed(Side, Request),
    Encoded(Side, Request),
}

struct Job {
    side: Side,
    streams: Streams,
    request: Request,
}

fn capture(encoder: Arc<Encoder>) -> Result<()> {
    let manager = CameraManager::new()?;
    let cameras = manager.cameras();
    ensure!(
        cameras.len() == 2,
        "expected two cameras, found {}",
        cameras.len()
    );

    let (events_tx, events_rx) = unbounded();
    let (jobs_tx, jobs_rx) = unbounded();
    for i in 0..ENCODERS {
        let (jobs, events, encoder) = (jobs_rx.clone(), events_tx.clone(), encoder.clone());
        thread::Builder::new()
            .name(format!("encode-{i}"))
            .spawn(move || {
                let error = encoder.run(jobs, events).unwrap_err();
                tracing::error!("encoder failed: {error:#}");
                std::process::exit(1);
            })?;
    }

    let heap = Heap::open()?;
    let mut preview_size = rig::DEFAULT_PREVIEW_SIZE;
    let (mut rigs, requests): (Vec<_>, Vec<_>) = Side::BOTH
        .into_iter()
        .map(|side| {
            let camera = cameras.get(side.index()).expect("checked camera count");
            tracing::info!("{side:?} camera: {}", camera.id());
            Rig::open(camera.acquire()?, side, &heap, events_tx.clone(), preview_size)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .unzip();
    start_all(&mut rigs, requests)?;

    let mut stats = [Stats::default(), Stats::default()];
    let mut focuser = Focuser::default();
    // Preview sizes the cameras couldn't be set up for, which aren't tried
    // again.
    let mut refused = Vec::new();
    // While the cameras are stopped to change the previews' size: the
    // requests each has back, and the size wanted.
    let mut stopped: Option<([Vec<Request>; 2], (u32, u32))> = None;
    for event in events_rx {
        match (event, &mut stopped) {
            (Event::Completed(side, request), None) => {
                let sensor_nanos = *request.metadata().get::<SensorTimestamp>()?;
                let dropped = stats[side.index()].record(side, sensor_nanos);
                encoder.outputs.counters.count_capture(side, dropped);
                if let Some(outcome) = focuser.observe(side, request.metadata()) {
                    encoder.outputs.autofocus.send_replace(Some(outcome));
                }
                let streams = rigs[side.index()].streams;
                jobs_tx
                    .send(Job {
                        side,
                        streams,
                        request,
                    })
                    .map_err(|_| anyhow!("encoders stopped"))?;
            }
            (Event::Encoded(side, mut request), None) => {
                request.reuse(ReuseFlag::REUSE_BUFFERS);
                let rig = &mut rigs[side.index()];
                if let Some(command) = rig.lens_command.take() {
                    command.add_to(request.controls_mut())?;
                }
                rig.camera.queue_request(request).map_err(|(_, e)| e)?;
            }
            (Event::Completed(side, request) | Event::Encoded(side, request), Some((returned, _))) => {
                returned[side.index()].push(request);
            }
        }
        match &stopped {
            None => {
                let recording = encoder.outputs.recording.load(Ordering::Relaxed);
                focuser.follow(*encoder.outputs.focus.borrow(), recording, &mut rigs);
                let wanted = *encoder.outputs.preview_size.borrow();
                if wanted != preview_size && !refused.contains(&wanted) && !recording {
                    tracing::info!("changing the previews to {} by {}", wanted.0, wanted.1);
                    // The requests queued come back cancelled.
                    for rig in &mut rigs {
                        rig.camera.stop()?;
                    }
                    stopped = Some((Default::default(), wanted));
                }
            }
            Some((returned, wanted)) if returned.iter().all(|requests| requests.len() == BUFFERS as usize) => {
                let wanted = *wanted;
                // Frees the old buffers, making room for the new ones.
                stopped = None;
                let reconfigured = rigs
                    .iter_mut()
                    .map(|rig| rig.reconfigure(&heap, wanted))
                    .collect::<Result<Vec<_>>>();
                let requests = match reconfigured {
                    Ok(requests) => {
                        preview_size = wanted;
                        requests
                    }
                    Err(e) => {
                        tracing::error!(
                            "the cameras can't send {} by {} previews, so they stay {} by {}: {e:#}",
                            wanted.0,
                            wanted.1,
                            preview_size.0,
                            preview_size.1
                        );
                        refused.push(wanted);
                        rigs.iter_mut()
                            .map(|rig| rig.reconfigure(&heap, preview_size))
                            .collect::<Result<Vec<_>>>()?
                    }
                };
                focuser.restarting(&mut rigs);
                start_all(&mut rigs, requests)?;
                stats = [Stats::default(), Stats::default()];
            }
            Some(_) => {}
        }
    }
    bail!("event channel closed")
}

/// Starts each rig with its requests.
fn start_all(rigs: &mut [Rig], requests: Vec<Vec<Request>>) -> Result<()> {
    for (rig, requests) in rigs.iter_mut().zip(requests) {
        rig.start(requests)?;
    }
    Ok(())
}

/// Periodically logs how many frames a camera delivered, and how many it
/// dropped because all its buffers were still being encoded.
#[derive(Default)]
struct Stats {
    first: Option<i64>,
    last: i64,
    frames: u32,
    dropped: i64,
}

impl Stats {
    const INTERVAL_NANOS: i64 = 30_000_000_000;

    /// Records a frame, returning how many were dropped since the last.
    fn record(&mut self, side: Side, sensor_nanos: i64) -> u64 {
        let frame_nanos = rig::frame_micros() * 1000;
        let dropped = match self.first {
            None => {
                self.first = Some(sensor_nanos);
                0
            }
            Some(_) => {
                let gap = sensor_nanos - self.last;
                ((gap + frame_nanos / 2) / frame_nanos - 1).max(0)
            }
        };
        self.dropped += dropped;
        self.last = sensor_nanos;
        self.frames += 1;
        let elapsed = sensor_nanos - self.first.expect("set above");
        if elapsed >= Self::INTERVAL_NANOS {
            let fps = f64::from(self.frames) * 1e9 / elapsed as f64;
            tracing::info!(
                "{side:?} camera: {fps:.1} fps, {} frames dropped",
                self.dropped
            );
            // Keeps `last`, so frames dropped between intervals still count.
            self.first = Some(sensor_nanos);
            self.frames = 0;
            self.dropped = 0;
        }
        dropped as u64
    }
}

/// Follows the base station's focus instructions: holds the lenses where
/// told, or autofocuses them once for each request.
#[derive(Default)]
struct Focuser {
    /// Where the lenses were last told to stay.
    held: Option<[f32; 2]>,
    /// The last autofocus request started.
    requested: Option<Timestamp>,
    scan: Option<Scan>,
}

/// An autofocus of both cameras under way.
struct Scan {
    requested: Timestamp,
    started: Instant,
    progress: [ScanProgress; 2],
}

#[derive(Clone, Copy)]
enum ScanProgress {
    /// Not seen scanning yet: frames asked for before the scan report the
    /// state from before it.
    Waiting,
    Scanning,
    Focused(f32),
    Failed,
}

/// Focus controls to send with a camera's next request.
#[derive(Clone, Copy)]
enum LensCommand {
    Hold(f32),
    Autofocus,
}

impl LensCommand {
    fn add_to(self, controls: &mut ControlList) -> Result<()> {
        match self {
            LensCommand::Hold(position) => {
                controls.set(AfMode::Manual)?;
                controls.set(LensPosition(position))?;
            }
            LensCommand::Autofocus => {
                controls.set(AfMode::Auto)?;
                controls.set(AfTrigger::Start)?;
            }
        }
        Ok(())
    }
}

impl Focuser {
    /// Acts on the base station's latest instructions. Autofocusing waits
    /// until nothing is being recorded.
    fn follow(&mut self, focus: Option<Focus>, recording: bool, rigs: &mut [Rig]) {
        match focus {
            Some(Focus::Fixed { lens_positions }) if self.held != Some(lens_positions) => {
                tracing::info!("holding the lenses at {lens_positions:?}");
                for (rig, position) in rigs.iter_mut().zip(lens_positions) {
                    rig.lens_command = Some(LensCommand::Hold(position));
                }
                self.held = Some(lens_positions);
                self.scan = None;
            }
            Some(Focus::Auto { requested }) if self.requested != Some(requested) && !recording => {
                tracing::info!("autofocusing");
                for rig in rigs.iter_mut() {
                    rig.lens_command = Some(LensCommand::Autofocus);
                }
                self.held = None;
                self.requested = Some(requested);
                self.scan = Some(Scan {
                    requested,
                    started: Instant::now(),
                    progress: [ScanProgress::Waiting; 2],
                });
            }
            _ => {}
        }
    }

    /// Follows an autofocus in the metadata of `side`'s frames, returning
    /// its outcome once both cameras have finished.
    fn observe(&mut self, side: Side, metadata: &ControlList) -> Option<AutofocusOutcome> {
        let scan = self.scan.as_mut()?;
        let progress = &mut scan.progress[side.index()];
        if let Ok(state) = metadata.get::<AfState>() {
            *progress = match (*progress, state) {
                (ScanProgress::Waiting, AfState::Scanning) => ScanProgress::Scanning,
                (ScanProgress::Scanning, AfState::Focused) => match metadata.get::<LensPosition>() {
                    Ok(position) => ScanProgress::Focused(*position),
                    Err(_) => ScanProgress::Failed,
                },
                (ScanProgress::Scanning, AfState::Failed) => ScanProgress::Failed,
                (progress, _) => progress,
            };
        }
        let finished = scan
            .progress
            .iter()
            .all(|progress| matches!(progress, ScanProgress::Focused(_) | ScanProgress::Failed));
        if !finished && scan.started.elapsed() < AUTOFOCUS_TIMEOUT {
            return None;
        }
        let outcome = AutofocusOutcome {
            requested: scan.requested,
            lens_positions: scan.progress.map(|progress| match progress {
                ScanProgress::Focused(position) => Some(position),
                _ => None,
            }),
        };
        tracing::info!("autofocused the lenses to {:?}", outcome.lens_positions);
        self.scan = None;
        Some(outcome)
    }

    /// Before the cameras restart, which cuts an autofocus short: has them
    /// start with the lenses where they were told to stay, and has an
    /// autofocus cut short done again.
    fn restarting(&mut self, rigs: &mut [Rig]) {
        if self.scan.take().is_some() {
            self.requested = None;
        }
        for (rig, side) in rigs.iter_mut().zip(Side::BOTH) {
            rig.lens_command = self.held.map(|positions| LensCommand::Hold(positions[side.index()]));
        }
    }
}

/// One configured camera.
struct Rig<'d> {
    camera: ActiveCamera<'d>,
    streams: Streams,
    /// Focus controls to send with the next request.
    lens_command: Option<LensCommand>,
}

impl<'d> Rig<'d> {
    /// Sets `camera` up for `preview_size` previews, returning it with the
    /// requests to start it with.
    fn open(
        mut camera: ActiveCamera<'d>,
        side: Side,
        heap: &Heap,
        events: Sender<Event>,
        preview_size: (u32, u32),
    ) -> Result<(Self, Vec<Request>)> {
        camera.on_request_completed(move |request| {
            // Only fails during shutdown.
            let _ = events.send(Event::Completed(side, request));
        });
        let streams = configure(&mut camera, preview_size)?;
        let mut rig = Rig {
            camera,
            streams,
            lens_command: None,
        };
        let requests = rig.create_requests(heap)?;
        Ok((rig, requests))
    }

    /// Sets the stopped camera up for `preview_size` previews, returning the
    /// requests to start it with.
    fn reconfigure(&mut self, heap: &Heap, preview_size: (u32, u32)) -> Result<Vec<Request>> {
        self.streams = configure(&mut self.camera, preview_size)?;
        self.create_requests(heap)
    }

    fn create_requests(&mut self, heap: &Heap) -> Result<Vec<Request>> {
        (0..BUFFERS)
            .map(|i| {
                let mut request = self
                    .camera
                    .create_request(Some(u64::from(i)))
                    .context("create request")?;
                request.add_buffer(&self.streams.record, heap.yuv420(rig::RECORD_SIZE)?)?;
                request.add_buffer(&self.streams.preview, heap.yuv420(self.streams.preview_size)?)?;
                Ok(request)
            })
            .collect()
    }

    fn start(&mut self, requests: Vec<Request>) -> Result<()> {
        // Controls without an info map are silently dropped on their way to
        // the (out-of-process) IPA.
        let mut controls =
            ControlList::from_info_map(self.camera.controls()).context("creating control list")?;
        controls.set(FrameDurationLimits([rig::frame_micros(); 2]))?;
        if let Some(command) = self.lens_command.take() {
            command.add_to(&mut controls)?;
        }
        self.camera.start(Some(&*controls))?;
        for request in requests {
            self.camera.queue_request(request).map_err(|(_, e)| e)?;
        }
        Ok(())
    }
}

/// Configures the stopped `camera` for full-size frames to record and
/// `preview_size` previews.
fn configure(camera: &mut ActiveCamera, preview_size: (u32, u32)) -> Result<Streams> {
    let mut config = camera
        .generate_configuration(&[StreamRole::VideoRecording, StreamRole::ViewFinder])
        .context("camera has no two-stream configuration")?;
    let sizes = [rig::RECORD_SIZE, preview_size];
    for (i, (width, height)) in sizes.into_iter().enumerate() {
        let mut stream = config.get_mut(i).expect("two streams requested");
        stream.set_pixel_format(formats::YUV420);
        stream.set_size(Size { width, height });
        stream.set_buffer_count(BUFFERS);
        // Full-range BT.601 is what JPEG decoders assume.
        stream.set_color_space(Some(ColorSpace::sycc()));
    }
    ensure!(
        !matches!(config.validate(), CameraConfigurationStatus::Invalid),
        "invalid camera configuration"
    );
    for (i, (width, height)) in sizes.into_iter().enumerate() {
        let stream = config.get(i).expect("two streams requested");
        let size = stream.get_size();
        ensure!(
            stream.get_pixel_format() == formats::YUV420
                && (size.width, size.height) == (width, height)
                && stream.get_stride() == width,
            "camera adjusted stream {i}: {:?}",
            stream
        );
    }
    camera.configure(&mut config)?;

    let stream = |i: usize| config.get(i).and_then(|s| s.stream()).expect("configured");
    Ok(Streams {
        record: stream(0),
        preview: stream(1),
        preview_size,
    })
}

struct Encoder {
    clock: Arc<Clock>,
    outputs: Outputs,
}

impl Encoder {
    fn run(&self, jobs: Receiver<Job>, events: Sender<Event>) -> Result<()> {
        let mut compressor = Compressor::new()?;
        for Job {
            side,
            streams,
            request,
        } in jobs
        {
            if request.status() == RequestStatus::Complete {
                self.encode(&mut compressor, side, streams, &request)?;
            }
            events
                .send(Event::Encoded(side, request))
                .map_err(|_| anyhow!("capture stopped"))?;
        }
        bail!("job channel closed")
    }

    fn encode(
        &self,
        compressor: &mut Compressor,
        side: Side,
        streams: Streams,
        request: &Request,
    ) -> Result<()> {
        let sensor_nanos = *request.metadata().get::<SensorTimestamp>()?;
        let timestamp = self.clock.boottime_to_timestamp(sensor_nanos / 1000);

        if self.outputs.recording.load(Ordering::Relaxed) {
            let buffer = request
                .buffer(&streams.record)
                .expect("record buffer attached");
            let quality = self.outputs.record_quality.load(Ordering::Relaxed).into();
            let jpeg = compress(compressor, buffer, rig::RECORD_SIZE, quality)?;
            self.outputs.frames.send(Input::Frame(CameraFrame {
                side,
                timestamp,
                jpeg,
            }))?;
        }

        if request.sequence().is_multiple_of(rig::PREVIEW_DECIMATION) {
            let buffer = request
                .buffer(&streams.preview)
                .expect("preview buffer attached");
            let quality = self.outputs.preview_quality.load(Ordering::Relaxed).into();
            let jpeg = compress(compressor, buffer, streams.preview_size, quality)?;
            // Previews are dropped while the base station isn't keeping up.
            let sent = self.outputs.previews.try_send(PreviewFrame { side, jpeg }).is_ok();
            self.outputs.counters.count_preview(side, sent);
        }
        Ok(())
    }
}

fn compress(
    compressor: &mut Compressor,
    buffer: &Buffer,
    (width, height): (u32, u32),
    quality: i32,
) -> Result<Vec<u8>> {
    let (width, height) = (width as usize, height as usize);
    compressor.set_quality(quality)?;
    let jpeg = dma::read(buffer, |[y_plane, u_plane, v_plane]| {
        compressor.compress_yuv_planes_to_vec(&YuvPlanesImage {
            y_plane,
            u_plane,
            v_plane,
            width,
            height,
            y_stride: width,
            u_stride: width / 2,
            v_stride: width / 2,
            subsamp: Subsamp::Sub2x2,
        })
    })??;
    Ok(jpeg)
}
