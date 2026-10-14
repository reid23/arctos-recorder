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
use protocol::{Directive, Registration};
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;

use crate::base::BaseStation;
use crate::clock::Clock;
use crate::recorder::{Input, Recorder};

const POLL: Duration = Duration::from_secs(1);
const RESYNC: Duration = Duration::from_secs(300);

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
    /// ALSA capture device of the microphone, e.g. `plughw:CARD=Device`.
    #[arg(long, env = "CSW_AUDIO_DEVICE")]
    audio_device: Option<String>,
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
    let registration = Registration {
        calibration: serde_json::from_slice(&std::fs::read(&args.calibration)?)?,
    };
    let discover = args.base_url.is_none();
    let base = BaseStation::new(args.base_url.unwrap_or_default(), camera);
    let clock = Arc::new(Clock::default());
    connect(&base, discover, &clock, &registration).await;

    let recordings = args.data_dir.join("recordings");
    std::fs::create_dir_all(&recordings)?;
    let recording = Arc::new(AtomicBool::new(false));
    let uploads = Arc::new(Notify::new());
    let (inputs, inputs_rx) = mpsc::channel();
    Recorder {
        root: recordings.clone(),
        audio: args.audio_device.is_some(),
        recording: recording.clone(),
        uploads: uploads.clone(),
    }
    .spawn(inputs_rx);
    if let Some(device) = args.audio_device {
        audio::spawn(device, clock.clone(), inputs.clone());
    }
    let (previews, previews_rx) = tokio::sync::mpsc::channel(4);
    camera::spawn(
        clock.clone(),
        camera::Outputs {
            recording,
            frames: inputs.clone(),
            previews,
        },
    );
    tokio::spawn(preview::run(base.clone(), previews_rx));
    tokio::spawn(upload::run(base.clone(), recordings, uploads));

    follow_directives(&base, discover, &clock, &registration, inputs).await
}

/// Syncs clocks and registers, retrying until the base station is reachable.
async fn connect(base: &BaseStation, discover: bool, clock: &Clock, registration: &Registration) {
    loop {
        let attempt = async {
            if discover {
                base.set_url(discovery::base_station_url().await?);
            }
            base.sync_clock(clock).await?;
            base.register(registration).await
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

/// Polls the base station for what to record and forwards changes to the
/// recorder. If the base station goes away, the last directive stays in
/// effect until it is back.
async fn follow_directives(
    base: &BaseStation,
    discover: bool,
    clock: &Clock,
    registration: &Registration,
    recorder: Sender<Input>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut current = Directive::Idle;
    let mut synced = Instant::now();
    loop {
        ticker.tick().await;
        match base.directive().await {
            Ok(directive) if directive != current => {
                recorder.send(Input::Directive(directive.clone()))?;
                current = directive;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("lost the base station: {e:#}");
                connect(base, discover, clock, registration).await;
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
