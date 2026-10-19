//! csw: the camera software running on each field's Raspberry Pi.
//!
//! Registers with the base station, streams low-resolution previews to it,
//! records the play of each match on its field, and uploads the recordings
//! once the match is over.

mod audio;
mod base;
mod camera;
mod clock;
mod discovery;
mod dma;
mod pairing;
mod preview;
mod recorder;
mod upload;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use protocol::{DEFAULT_AUDIO_DEVICE, Directive, Registration};
use tokio::sync::{Notify, watch};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;

use crate::base::BaseStation;
use crate::clock::Clock;
use crate::recorder::{Input, Recorder};

const POLL: Duration = Duration::from_secs(1);
const RESYNC: Duration = Duration::from_secs(300);
/// Chunks of live audio (about 21 ms each) held while the preview stream
/// catches up.
const LIVE_AUDIO_BUFFER: usize = 50;

#[derive(Parser)]
struct Args {
    /// Base station URL, e.g. `http://10.42.0.1:7700`. Discovered on the local
    /// network by default.
    #[arg(long, env = "CSW_BASE_URL")]
    base_url: Option<String>,
    /// Where recordings are kept until they are uploaded.
    #[arg(long, env = "CSW_DATA_DIR", default_value = "/var/lib/csw")]
    data_dir: PathBuf,
    /// The rig's stitching calibration (a reco `MatchCalibration` JSON file).
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
    let discover = args.base_url.is_none();
    let base = BaseStation::new(args.base_url.unwrap_or_default(), camera);
    let clock = Arc::new(Clock::default());
    connect(&base, discover, &clock, &calibration).await;

    let recordings = args.data_dir.join("recordings");
    std::fs::create_dir_all(&recordings)?;
    let recording = Arc::new(AtomicBool::new(false));
    let capturing_audio = Arc::new(AtomicBool::new(false));
    let uploads = Arc::new(Notify::new());
    let (inputs, inputs_rx) = mpsc::channel();
    Recorder {
        root: recordings.clone(),
        audio: capturing_audio.clone(),
        recording: recording.clone(),
        uploads: uploads.clone(),
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
    camera::spawn(
        clock.clone(),
        camera::Outputs {
            recording,
            frames: inputs.clone(),
            previews,
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

    follow_instructions(&base, discover, &clock, &calibration, inputs, audio_device).await
}

/// Syncs clocks and registers, retrying until the base station is reachable.
async fn connect(
    base: &BaseStation,
    discover: bool,
    clock: &Clock,
    calibration: &serde_json::Value,
) {
    loop {
        let attempt = async {
            if discover {
                base.set_url(discovery::base_station_url().await?);
            }
            base.sync_clock(clock).await?;
            base.register(&Registration {
                calibration: calibration.clone(),
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

/// Polls the base station for what to do and forwards changes to the
/// recorder and the microphone. If the base station goes away, the last
/// instructions stay in effect until it is back.
async fn follow_instructions(
    base: &BaseStation,
    discover: bool,
    clock: &Clock,
    calibration: &serde_json::Value,
    recorder: Sender<Input>,
    audio_device: watch::Sender<String>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut current = Directive::Idle;
    let mut synced = Instant::now();
    loop {
        ticker.tick().await;
        match base.instructions().await {
            Ok(instructions) => {
                if instructions.directive != current {
                    recorder.send(Input::Directive(instructions.directive.clone()))?;
                    current = instructions.directive;
                }
                audio_device.send_if_modified(|device| {
                    let changed = *device != instructions.audio_device;
                    *device = instructions.audio_device;
                    changed
                });
            }
            Err(e) => {
                tracing::warn!("lost the base station: {e:#}");
                connect(base, discover, clock, calibration).await;
                synced = Instant::now();
            }
        }
        if synced.elapsed() > RESYNC {
            if let Err(e) = base.sync_clock(clock).await {
                tracing::warn!("clock sync failed: {e:#}");
            }
            synced = Instant::now();
        }
    }
}
