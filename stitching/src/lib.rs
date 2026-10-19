//! Stitching shared by the base station (local stitching) and the cloud
//! server (cloud stitching).

pub mod browser;
pub mod calibrate;
pub mod cloud;
pub mod live;
pub mod overlay;
pub mod recording;
pub mod restream;

use std::process::Command;

use anyhow::{Result, bail};
pub use reco_core::calibration::MatchCalibration;
use serde::{Deserialize, Serialize};

/// Progress of processing a recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    /// Saved to be stitched when the user says so.
    StitchLater,
    /// Stitched and saved to be uploaded when the user says so.
    UploadLater,
    /// Sending the recording to the cloud server, progress in `[0, 1]`.
    Sending(f32),
    Stitching(f32),
    Uploading(f32),
    Done,
    Failed(String),
}

fn ffmpeg() -> Command {
    let mut command = Command::new("ffmpeg");
    command.args(["-y", "-nostdin", "-loglevel", "error"]);
    command
}

fn run_ffmpeg(mut command: Command) -> Result<()> {
    let output = command.output()?;
    if !output.status.success() {
        bail!(
            "{command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
