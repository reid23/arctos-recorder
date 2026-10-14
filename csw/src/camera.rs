//! Captures both cameras with libcamera and JPEG-encodes their frames.
//!
//! The ISP produces two streams per frame: the full recording resolution and
//! a downscaled preview. One thread owns libcamera and recycles requests; a
//! pool of workers encodes them (the Pi 5 has no hardware JPEG encoder).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result, anyhow, bail, ensure};
use crossbeam_channel::{Receiver, Sender, unbounded};
use libcamera::camera::{ActiveCamera, CameraConfigurationStatus};
use libcamera::camera_manager::CameraManager;
use libcamera::color_space::ColorSpace;
use libcamera::control::ControlList;
use libcamera::controls::{FrameDurationLimits, SensorTimestamp};
use libcamera::formats;
use libcamera::geometry::Size;
use libcamera::request::{Request, RequestStatus, ReuseFlag};
use libcamera::stream::{Stream, StreamRole};
use protocol::{PreviewFrame, Side, rig};
use turbojpeg::{Compressor, Subsamp, YuvPlanesImage};

use crate::clock::Clock;
use crate::dma::{self, Buffer, Heap};
use crate::recorder::{CameraFrame, Input};

const RECORD_QUALITY: i32 = 80;
const PREVIEW_QUALITY: i32 = 50;
/// Frame buffers per stream per camera; bounds how far encoding may lag.
const BUFFERS: u32 = 12;
const ENCODERS: usize = 4;

pub struct Outputs {
    /// Whether full-resolution frames are wanted (only while a match is on).
    pub recording: Arc<AtomicBool>,
    pub frames: std::sync::mpsc::Sender<Input>,
    pub previews: tokio::sync::mpsc::Sender<PreviewFrame<Vec<u8>>>,
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
    let rigs = Side::BOTH
        .into_iter()
        .map(|side| {
            let camera = cameras.get(side.index()).expect("checked camera count");
            tracing::info!("{side:?} camera: {}", camera.id());
            Rig::start(camera.acquire()?, side, &heap, events_tx.clone())
        })
        .collect::<Result<Vec<_>>>()?;

    let mut stats = [Stats::default(), Stats::default()];
    for event in events_rx {
        match event {
            Event::Completed(side, request) => {
                let sensor_nanos = *request.metadata().get::<SensorTimestamp>()?;
                stats[side.index()].record(side, sensor_nanos);
                let streams = rigs[side.index()].streams;
                jobs_tx
                    .send(Job {
                        side,
                        streams,
                        request,
                    })
                    .map_err(|_| anyhow!("encoders stopped"))?;
            }
            Event::Encoded(side, mut request) => {
                request.reuse(ReuseFlag::REUSE_BUFFERS);
                rigs[side.index()]
                    .camera
                    .queue_request(request)
                    .map_err(|(_, e)| e)?;
            }
        }
    }
    bail!("event channel closed")
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

    fn record(&mut self, side: Side, sensor_nanos: i64) {
        let frame_nanos = rig::frame_micros() * 1000;
        match self.first {
            None => self.first = Some(sensor_nanos),
            Some(_) => {
                let gap = sensor_nanos - self.last;
                self.dropped += (gap + frame_nanos / 2) / frame_nanos - 1;
            }
        }
        self.last = sensor_nanos;
        self.frames += 1;
        let elapsed = sensor_nanos - self.first.expect("set above");
        if elapsed >= Self::INTERVAL_NANOS {
            let fps = f64::from(self.frames) * 1e9 / elapsed as f64;
            tracing::info!(
                "{side:?} camera: {fps:.1} fps, {} frames dropped",
                self.dropped
            );
            *self = Stats::default();
        }
    }
}

/// One configured, running camera.
struct Rig<'d> {
    camera: ActiveCamera<'d>,
    streams: Streams,
}

impl<'d> Rig<'d> {
    fn start(
        mut camera: ActiveCamera<'d>,
        side: Side,
        heap: &Heap,
        events: Sender<Event>,
    ) -> Result<Self> {
        let mut config = camera
            .generate_configuration(&[StreamRole::VideoRecording, StreamRole::ViewFinder])
            .context("camera has no two-stream configuration")?;
        let sizes = [rig::RECORD_SIZE, rig::PREVIEW_SIZE];
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
        let streams = Streams {
            record: stream(0),
            preview: stream(1),
        };

        let requests = (0..BUFFERS)
            .map(|i| {
                let mut request = camera
                    .create_request(Some(u64::from(i)))
                    .context("create request")?;
                request.add_buffer(&streams.record, heap.yuv420(rig::RECORD_SIZE)?)?;
                request.add_buffer(&streams.preview, heap.yuv420(rig::PREVIEW_SIZE)?)?;
                Ok(request)
            })
            .collect::<Result<Vec<_>>>()?;

        camera.on_request_completed(move |request| {
            // Only fails during shutdown.
            let _ = events.send(Event::Completed(side, request));
        });

        // Controls without an info map are silently dropped on their way to
        // the (out-of-process) IPA.
        let mut controls =
            ControlList::from_info_map(camera.controls()).context("creating control list")?;
        controls.set(FrameDurationLimits([rig::frame_micros(); 2]))?;
        camera.start(Some(&*controls))?;
        for request in requests {
            camera.queue_request(request).map_err(|(_, e)| e)?;
        }

        Ok(Rig { camera, streams })
    }
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
            let jpeg = compress(compressor, buffer, rig::RECORD_SIZE, RECORD_QUALITY)?;
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
            let jpeg = compress(compressor, buffer, rig::PREVIEW_SIZE, PREVIEW_QUALITY)?;
            // Previews are dropped while the base station isn't keeping up.
            let _ = self.outputs.previews.try_send(PreviewFrame { side, jpeg });
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
