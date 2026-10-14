//! Match recordings and rendering them into one stitched video.
//!
//! A recording lives in a directory holding the segment directories sent by
//! the camera, plus [`RECORDING_FILE`] describing it. It is rendered by
//! stitching every segment with reco and concatenating the results into
//! [`VIDEO_FILE`], which is uploaded to Arctos with an anchor at the start of
//! each segment so viewers can jump to points.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use arctos::{Anchor, Footage, datetime};
use protocol::{AUDIO_FILE, MatchManifest, Side, rig};
use reco_core::calibration::MatchCalibration;
use reco_io::StitchJob;
use reco_io::output::AudioMode;
use serde::{Deserialize, Serialize};

use crate::{ffmpeg, run_ffmpeg};

pub const RECORDING_FILE: &str = "recording.json";
pub const VIDEO_FILE: &str = "panorama.mp4";
const WORK_DIR: &str = "work";
const OUTPUT_SIZE: (u32, u32) = (1920, 1080);

/// Identifies a recording: one camera's footage of one match.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordingKey {
    pub match_id: String,
    pub camera: String,
}

/// Everything needed to process a recording besides its media.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recording {
    pub tournament: String,
    pub title: String,
    pub calibration: MatchCalibration,
    pub manifest: MatchManifest,
}

impl Recording {
    pub fn load(dir: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(
            dir.join(RECORDING_FILE),
        )?)?)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::write(dir.join(RECORDING_FILE), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn anchors(&self) -> Vec<Anchor> {
        let mut offset = 0.0;
        self.manifest
            .segments
            .iter()
            .map(|segment| {
                let anchor = Anchor {
                    world_time: datetime(segment.start),
                    video_offset: offset,
                };
                offset += segment.duration().as_secs_f64();
                anchor
            })
            .collect()
    }

    /// The rendered video's description for uploading it to Arctos.
    pub fn footage<'a>(
        &'a self,
        key: &'a RecordingKey,
        video: &'a Path,
        anchors: &'a [Anchor],
    ) -> Footage<'a> {
        Footage {
            tournament: &self.tournament,
            match_id: &key.match_id,
            camera: &key.camera,
            video,
            anchors,
        }
    }

    /// Deletes the media (segments and rendered video) from `dir`, keeping
    /// [`RECORDING_FILE`].
    pub fn delete_media(&self, dir: &Path) -> Result<()> {
        for segment in &self.manifest.segments {
            remove_dir(&dir.join(segment.dir_name()))?;
        }
        remove_dir(&dir.join(WORK_DIR))?;
        match std::fs::remove_file(dir.join(VIDEO_FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

fn remove_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Stitches every segment in `dir` and concatenates them into
/// [`VIDEO_FILE`], reporting progress in `[0, 1]`. Blocks for a long time.
pub fn render(
    dir: &Path,
    recording: &Recording,
    progress: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<PathBuf> {
    let work = dir.join(WORK_DIR);
    std::fs::create_dir_all(&work)?;
    let segments = &recording.manifest.segments;
    let total_frames: u32 = segments.iter().map(|s| s.frames).sum();
    let mut stitched_frames = 0;
    let mut parts = Vec::new();

    for segment in segments {
        let name = segment.dir_name();
        let source = dir.join(&name);
        let [left, right] = Side::BOTH.map(|side| work.join(format!("{name}-{side:?}.mkv")));
        remux(&source, Side::Left, recording.manifest.audio, &left)?;
        remux(&source, Side::Right, false, &right)?;

        let part = work.join(format!("{name}.mp4"));
        let progress = progress.clone();
        let done = stitched_frames;
        StitchJob::with_calibration(left, right, recording.calibration.clone(), &part)
            .resolution(OUTPUT_SIZE.0, OUTPUT_SIZE.1)
            .audio(if recording.manifest.audio {
                AudioMode::CopyFrom(0)
            } else {
                AudioMode::Disabled
            })
            .on_progress(move |p| {
                progress((done as u64 + p.frames_completed) as f32 / total_frames as f32);
            })
            .run(&AtomicBool::new(false))?;
        stitched_frames += segment.frames;
        parts.push(part);
    }

    let video = dir.join(VIDEO_FILE);
    concat(&parts, &work, &video)?;
    std::fs::remove_dir_all(&work)?;
    Ok(video)
}

/// Wraps one camera's raw JPEG stream (plus the audio) in a Matroska file.
fn remux(segment: &Path, side: Side, audio: bool, output: &Path) -> Result<()> {
    let mut command = ffmpeg();
    command
        .args(["-f", "mjpeg", "-framerate", &rig::FPS.to_string(), "-i"])
        .arg(segment.join(side.file_name()));
    if audio {
        command
            .args([
                "-f",
                "s16le",
                "-ar",
                &rig::AUDIO_SAMPLE_RATE.to_string(),
                "-ac",
                "1",
                "-i",
            ])
            .arg(segment.join(AUDIO_FILE))
            .args(["-c:a", "aac", "-shortest"]);
    }
    command.args(["-c:v", "copy"]).arg(output);
    run_ffmpeg(command)
}

fn concat(parts: &[PathBuf], work: &Path, output: &Path) -> Result<()> {
    let list = work.join("parts.txt");
    let lines: String = parts
        .iter()
        .map(|p| format!("file '{}'\n", p.display()))
        .collect();
    std::fs::write(&list, lines)?;
    let mut command = ffmpeg();
    command
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(output);
    run_ffmpeg(command)
}
