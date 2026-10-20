//! Stitching shared by the base station (local stitching) and the cloud
//! server (cloud stitching).

pub mod browser;
pub mod cloud;
pub mod encode;
pub mod geometry;
pub mod gpu;
pub mod live;
pub mod mjpeg;
pub mod overlay;
mod priority;
pub mod recording;
pub mod restream;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// Progress of processing a recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum JobStatus {
    /// Arriving from the camera, progress in `[0, 1]`.
    Receiving(f32),
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

/// ffmpeg: the one installed alongside this program, if any, else the one
/// on the `PATH`.
pub fn ffmpeg_program() -> &'static Path {
    static PROGRAM: OnceLock<PathBuf> = OnceLock::new();
    PROGRAM.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|program| {
                let folder = program.parent()?;
                Some(folder.join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX)))
            })
            .filter(|installed| installed.is_file())
            .unwrap_or_else(|| PathBuf::from("ffmpeg"))
    })
}

fn ffmpeg() -> Command {
    let mut command = Command::new(ffmpeg_program());
    command.args(["-y", "-nostdin", "-loglevel", "error"]);
    command
}