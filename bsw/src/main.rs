//! bsw: the base station desktop app.
//!
//! Runs an HTTP server the field cameras connect to, relays the Arctos
//! scoreboard to them so they know what to record, shows their live feeds,
//! and stitches (locally or in the cloud) and uploads the match recordings
//! they send back.

mod account;
mod calibrator;
mod camera_port;
mod cloud;
mod discovery;
mod fields;
mod hub;
mod jobs;
mod live;
mod performance;
mod restream;
mod server;
mod settings;
mod ui;

use std::sync::Arc;

use anyhow::{Context, Result};
use arctos::Arctos;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use crate::account::Account;
use crate::hub::{Hub, Paths};
use crate::jobs::Order;
use crate::settings::Settings;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu=warn,naga=warn".into()),
        )
        .init();

    let paths = Paths::new(
        dirs::data_local_dir()
            .context("no data directory")?
            .join("arctos-recorder"),
    )?;
    let settings = Settings::load(&paths.settings())?;
    let arctos = Arctos::from_env()?;
    let account = Account::load(arctos.clone(), paths.account())?;
    let (job_queue, jobs) = mpsc::unbounded_channel();
    let hub = Arc::new(Hub::new(paths, arctos, account, settings, job_queue));
    if let Err(e) = keep_awake() {
        tracing::warn!("the computer may sleep while cameras record: {e:#}");
    }

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.spawn(backend(hub.clone(), jobs));
    ui::launch(hub, runtime.handle().clone());
    Ok(())
}

/// Keeps the computer from sleeping until the app exits, as cameras record
/// with nobody touching it. Must be called from the main thread.
fn keep_awake() -> Result<()> {
    #[cfg(target_os = "linux")]
    std::process::Command::new("systemd-inhibit")
        .args(["--what=sleep:idle", "--who=Arctos Recorder", "--why=Cameras may be recording"])
        .args(["tail", &format!("--pid={}", std::process::id()), "--follow", "/dev/null"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .context("starting systemd-inhibit")?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("caffeinate")
        .args(["-i", "-w", &std::process::id().to_string()])
        .spawn()
        .context("starting caffeinate")?;
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
        // SAFETY: only sets this thread's power request, which lasts while it does.
        if unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) } == 0 {
            anyhow::bail!("Windows refused to keep the computer awake");
        }
    }
    Ok(())
}

async fn backend(hub: Arc<Hub>, jobs: mpsc::UnboundedReceiver<Order>) {
    if let Err(e) = jobs::restore(&hub) {
        tracing::error!("restoring jobs: {e:#}");
    }
    tokio::spawn(jobs::run(hub.clone(), jobs));
    tokio::spawn(fields::run(hub.clone()));
    tokio::spawn(hub.clone().track_presence());
    tokio::spawn(performance::track_computer(hub.clone()));
    if cfg!(target_os = "linux") {
        tokio::spawn(camera_port::follow(hub.clone()));
    }
    tokio::spawn(async {
        if let Err(e) = discovery::run().await {
            tracing::error!("camera discovery stopped: {e:#}");
        }
    });
    if let Err(e) = server::serve(hub).await {
        tracing::error!("server stopped: {e:#}");
        std::process::exit(1);
    }
}
