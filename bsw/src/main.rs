//! bsw: the base station desktop app.
//!
//! Runs an HTTP server the field cameras connect to, relays the Arctos
//! scoreboard to them so they know what to record, shows their live feeds,
//! and stitches (locally or in the cloud) and uploads the match recordings
//! they send back.

mod cloud;
mod discovery;
mod fields;
mod hub;
mod jobs;
mod live;
mod restream;
mod server;
mod settings;
mod ui;

use std::sync::Arc;

use anyhow::{Context, Result};
use arctos::Arctos;
use stitching::recording::RecordingKey;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use crate::hub::{Hub, Paths};
use crate::settings::Settings;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu=warn,naga=warn".into()),
        )
        .init();

    let paths = Paths::new(
        dirs::data_dir()
            .context("no data directory")?
            .join("arctos-recorder"),
    )?;
    let settings = Settings::load(&paths.settings())?;
    let (job_queue, jobs) = mpsc::unbounded_channel();
    let hub = Arc::new(Hub::new(paths, Arctos::from_env()?, settings, job_queue));

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.spawn(backend(hub.clone(), jobs));
    ui::launch(hub, runtime.handle().clone());
    Ok(())
}

async fn backend(hub: Arc<Hub>, jobs: mpsc::UnboundedReceiver<RecordingKey>) {
    if let Err(e) = jobs::restore(&hub) {
        tracing::error!("restoring jobs: {e:#}");
    }
    tokio::spawn(jobs::run(hub.clone(), jobs));
    tokio::spawn(fields::run(hub.clone()));
    tokio::spawn(hub.clone().track_presence());
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
