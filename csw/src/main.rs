//! csw: the camera software running on each field's Raspberry Pi.
//!
//! Registers with the base station, streams previews to it,
//! records the play of each match on its field, uploads each finished point
//! while the match goes on, and the rest once the match is over.

mod audio;
mod base;
mod camera;
mod clock;
mod discovery;
mod dma;
mod pairing;
mod preview;
mod recorder;
mod setup;
mod stats;
mod upload;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use protocol::{
    DEFAULT_AUDIO_DEVICE, DEFAULT_BUFFER, DEFAULT_MARGINS, DEFAULT_PREVIEW_QUALITY,
    DEFAULT_RECORD_QUALITY, Directive, Focus, Margins, Registration, Timestamp, rig,
};
use tokio::sync::{Notify, watch};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;

use crate::base::BaseStation;
use crate::clock::Clock;
use crate::recorder::{Input, Recorder};
use crate::setup::KeptSetup;
use crate::stats::{FrameCounters, Reporter};

const POLL: Duration = Duration::from_secs(1);
const RESYNC: Duration = Duration::from_secs(300);
/// How long instruction polls may fail before the camera looks for the base
/// station again. A busy Pi misses a poll or two; rediscovering on each of
/// those restarts the preview.
const REDISCOVER_AFTER: Duration = Duration::from_secs(15);
/// Chunks of live audio (about 21 ms each) held while the preview stream
/// catches up.
const LIVE_AUDIO_BUFFER: usize = 50;

#[derive(Parser)]
struct Args {
    /// Base station URL, e.g. `http://10.42.0.1:7700`. Discovered on the local
    /// network by default.
    #[arg(long, env = "CSW_BASE_URL")]
    base_url: Option<String>,
    /// Where the rig's setup is kept, and recordings until they are
    /// uploaded.
    #[arg(long, env = "CSW_DATA_DIR", default_value = "/var/lib/csw")]
    data_dir: PathBuf,
    /// The rig's calibration (a `stitching::geometry::Calibration` JSON
    /// file).
    #[arg(
        long,
        env = "CSW_CALIBRATION",
        default_value = "/etc/csw/calibration.json"
    )]
    calibration: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    let camera = std::fs::read_to_string("/proc/sys/kernel/hostname")?
        .trim()
        .to_owned();
    let calibration: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&args.calibration)?)?;
    let setup = KeptSetup::load(args.data_dir.join(setup::FILE));
    let discover = args.base_url.is_none();
    let base = BaseStation::new(args.base_url.unwrap_or_default(), camera);
    let clock = Arc::new(Clock::default());
    connect(&base, discover, &clock, &calibration, setup.current(), true).await;

    let recordings = args.data_dir.join("recordings");
    std::fs::create_dir_all(&recordings)?;
    let recording = Arc::new(AtomicBool::new(false));
    let capturing_audio = Arc::new(AtomicBool::new(false));
    let uploads = Arc::new(Notify::new());
    let (inputs, inputs_rx) = mpsc::channel();
    let (margins, margins_rx) = watch::channel(DEFAULT_MARGINS);
    let (buffer, buffer_rx) = watch::channel(DEFAULT_BUFFER);
    Recorder {
        root: recordings.clone(),
        audio: capturing_audio.clone(),
        recording: recording.clone(),
        uploads: uploads.clone(),
        margins: margins_rx,
        buffer: buffer_rx,
    }
    .spawn(inputs_rx);
    let (audio_device, audio_device_rx) = watch::channel(DEFAULT_AUDIO_DEVICE.to_owned());
    let (live_audio, live_audio_rx) = tokio::sync::mpsc::channel(LIVE_AUDIO_BUFFER);
    audio::spawn(
        audio_device_rx,
        clock.clone(),
        audio::Outputs {
            recorder: inputs.clone(),
            live: live_audio,
            capturing: capturing_audio,
        },
    );
    let (previews, previews_rx) = tokio::sync::mpsc::channel(4);
    let record_quality = Arc::new(AtomicU8::new(DEFAULT_RECORD_QUALITY));
    let (preview_size, preview_size_rx) = watch::channel(rig::DEFAULT_PREVIEW_SIZE);
    let preview_quality = Arc::new(AtomicU8::new(DEFAULT_PREVIEW_QUALITY));
    let (focus, focus_rx) = watch::channel(None);
    let (autofocus, autofocus_rx) = watch::channel(None);
    let counters = Arc::new(FrameCounters::default());
    camera::spawn(
        clock.clone(),
        camera::Outputs {
            recording: recording.clone(),
            record_quality: record_quality.clone(),
            preview_size: preview_size_rx,
            preview_quality: preview_quality.clone(),
            focus: focus_rx,
            autofocus,
            frames: inputs.clone(),
            previews,
            counters: counters.clone(),
        },
    );
    tokio::spawn(preview::run(
        base.clone(),
        preview::Inputs {
            frames: previews_rx,
            audio: live_audio_rx,
        },
    ));
    tokio::spawn(upload::run(base.clone(), recordings, uploads));

    let controlled = Controlled {
        recorder: inputs,
        audio_device,
        record_quality,
        preview_size,
        preview_quality,
        focus,
        margins,
        buffer,
        recording,
    };
    let reporter = Reporter::new(counters, autofocus_rx);
    follow_instructions(&base, discover, &clock, &calibration, setup, controlled, reporter).await
}

/// What the base station's instructions control.
struct Controlled {
    recorder: Sender<Input>,
    audio_device: watch::Sender<String>,
    record_quality: Arc<AtomicU8>,
    preview_size: watch::Sender<(u32, u32)>,
    preview_quality: Arc<AtomicU8>,
    focus: watch::Sender<Option<Focus>>,
    margins: watch::Sender<Margins>,
    buffer: watch::Sender<Duration>,
    /// Set while a match is being recorded. The clock is not stepped then.
    recording: Arc<AtomicBool>,
}

/// Syncs clocks and registers, retrying until the base station is reachable.
async fn connect(
    base: &BaseStation,
    discover: bool,
    clock: &Clock,
    calibration: &serde_json::Value,
    setup: Option<&serde_json::Value>,
    sync_clock: bool,
) {
    loop {
        let attempt = async {
            if discover {
                base.set_url(discovery::base_station_url().await?);
            }
            if sync_clock {
                base.sync_clock(clock).await?;
            }
            base.register(&Registration {
                calibration: calibration.clone(),
                setup: setup.cloned(),
                audio_devices: audio::devices(),
            })
            .await
        };
        match attempt.await {
            Ok(()) => {
                tracing::info!(
                    "registered with the base station at {} as {}",
                    base.url(),
                    base.camera()
                );
                return;
            }
            Err(e) => {
                tracing::warn!("waiting for the base station: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Polls the base station for what to do, telling it how the camera is
/// doing, forwards changes to what the instructions control, and keeps the
/// rig's setup they carry. A failed poll keeps the last instructions and
/// retries; the base station is only rediscovered once it has stayed silent.
/// The clock is not stepped while a match is recording, so a correction
/// cannot reorder that match's frames.
async fn follow_instructions(
    base: &BaseStation,
    discover: bool,
    clock: &Clock,
    calibration: &serde_json::Value,
    mut setup: KeptSetup,
    controlled: Controlled,
    mut reporter: Reporter,
) -> Result<()> {
    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut current = Directive::Idle;
    let mut confirmed_at = Timestamp(0);
    let mut synced = Instant::now();
    let mut lost_at: Option<Instant> = None;
    loop {
        ticker.tick().await;
        match base.instructions(&reporter.stats()).await {
            Ok(instructions) => {
                lost_at = None;
                if instructions.directive != current || instructions.confirmed_at != confirmed_at {
                    controlled.recorder.send(Input::Guide {
                        directive: instructions.directive.clone(),
                        confirmed_at: instructions.confirmed_at,
                    })?;
                    current = instructions.directive;
                    confirmed_at = instructions.confirmed_at;
                }
                controlled.audio_device.send_if_modified(|device| {
                    let changed = *device != instructions.audio_device;
                    *device = instructions.audio_device;
                    changed
                });
                controlled
                    .record_quality
                    .store(instructions.record_quality.clamp(1, 100), Ordering::Relaxed);
                if rig::PREVIEW_SIZES.contains(&instructions.preview_size) {
                    controlled.preview_size.send_replace(instructions.preview_size);
                }
                controlled
                    .preview_quality
                    .store(instructions.preview_quality.clamp(1, 100), Ordering::Relaxed);
                controlled.focus.send_replace(Some(instructions.focus));
                controlled.margins.send_replace(instructions.margins);
                controlled.buffer.send_replace(instructions.buffer);
                if let Err(e) = setup.keep(instructions.setup) {
                    tracing::warn!("keeping the rig's setup: {e:#}");
                }
            }
            Err(error) => {
                tracing::warn!("instruction poll failed: {error:#}");
                let lost = lost_at.get_or_insert_with(Instant::now);
                if lost.elapsed() >= REDISCOVER_AFTER {
                    tracing::warn!(
                        "the base station has not answered for {}s; reconnecting",
                        REDISCOVER_AFTER.as_secs()
                    );
                    let recording = controlled.recording.load(Ordering::Relaxed);
                    connect(base, discover, clock, calibration, setup.current(), !recording).await;
                    lost_at = None;
                    if !recording {
                        synced = Instant::now();
                    }
                }
            }
        }
        if synced.elapsed() > RESYNC && !controlled.recording.load(Ordering::Relaxed) {
            if let Err(error) = base.sync_clock(clock).await {
                tracing::warn!("clock sync failed: {error:#}");
            }
            synced = Instant::now();
        }
    }
}
