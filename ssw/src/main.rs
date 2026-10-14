//! ssw: the cloud stitching server.
//!
//! Base stations without a capable GPU send it their recordings, which it
//! renders and uploads to Arctos, and their cameras' live previews, which it
//! stitches and restreams over RTMP. See [`stitching::cloud`] for the API.

mod jobs;
mod live;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use arctos::Arctos;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::jobs::Jobs;

#[derive(Parser)]
struct Args {
    /// Token base stations must present as `Authorization: Bearer <token>`.
    #[arg(long, env = "SSW_TOKEN", hide_env_values = true)]
    token: String,
    /// Where recordings are kept while they are processed.
    #[arg(long, env = "SSW_DATA_DIR", default_value = "/data")]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu=warn,naga=warn".into()),
        )
        .init();
    let args = Args::parse();

    let (jobs, queue) = Jobs::restore(args.data_dir.join("jobs"), Arctos::from_env()?)?;
    let jobs = Arc::new(jobs);
    tokio::spawn(jobs.clone().run(queue));
    server::serve(jobs, args.token).await
}
