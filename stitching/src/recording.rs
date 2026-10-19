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

use anyhow::{Context, Result};
use arctos::{Anchor, Arctos, Footage, OverlayFrame, datetime};
use chrono::{DateTime, Utc};
use protocol::{AUDIO_FILE, MatchManifest, Side, rig};
use reco_core::calibration::MatchCalibration;
use reco_io::StitchJob;
use reco_io::output::AudioMode;
use serde::{Deserialize, Serialize};

use crate::overlay::Overlay;
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

/// The scoreboard overlay of a recording's match.
pub struct OverlayImages {
    pub placement: Overlay,
    /// In the order they take effect.
    pub frames: Vec<OverlayFrame>,
}

impl OverlayImages {
    /// Fetches the overlay of the recording's match from Arctos.
    pub async fn fetch(
        arctos: &Arctos,
        recording: &Recording,
        placement: &Overlay,
    ) -> Result<Self> {
        let frames = arctos
            .scoreboard_overlay(
                &recording.tournament,
                &recording.manifest.match_id,
                placement.image_size(OUTPUT_SIZE),
            )
            .await
            .context("fetching the scoreboard overlay")?;
        Ok(OverlayImages {
            placement: placement.clone(),
            frames,
        })
    }
}

/// Stitches every segment in `dir`, concatenates them and draws the overlay
/// (if any) into [`VIDEO_FILE`], reporting progress in `[0, 1]`. Blocks for
/// a long time.
pub fn render(
    dir: &Path,
    recording: &Recording,
    overlay: Option<&OverlayImages>,
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

    // Rendered under a temporary name so a partial render never counts as
    // done.
    let partial = work.join(VIDEO_FILE);
    match overlay {
        Some(overlay) if !overlay.frames.is_empty() => {
            let stitched = work.join("stitched.mp4");
            concat(&parts, &work, &stitched)?;
            draw_overlay(&stitched, recording, overlay, &work, &partial)?;
        }
        _ => concat(&parts, &work, &partial)?,
    }
    let video = dir.join(VIDEO_FILE);
    std::fs::rename(&partial, &video)?;
    std::fs::remove_dir_all(&work)?;
    Ok(video)
}

/// Draws the scoreboard overlay onto `video`, showing each state of the
/// scoreboard from when it took effect.
fn draw_overlay(
    video: &Path,
    recording: &Recording,
    overlay: &OverlayImages,
    work: &Path,
    output: &Path,
) -> Result<()> {
    let (width, height) = overlay.placement.image_size(OUTPUT_SIZE);
    let blank = work.join("overlay-blank.png");
    write_transparent_png(&blank, (width, height))?;
    let images: Vec<PathBuf> = overlay
        .frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let path = work.join(format!("overlay-{index}.png"));
            std::fs::write(&path, &frame.png)?;
            Ok(path)
        })
        .collect::<Result<_>>()?;

    let segments: Vec<(Anchor, f64)> = recording
        .anchors()
        .into_iter()
        .zip(&recording.manifest.segments)
        .map(|(anchor, segment)| (anchor, segment.duration().as_secs_f64()))
        .collect();
    let effective: Vec<DateTime<Utc>> = overlay.frames.iter().map(|f| f.effective_at).collect();
    let timeline = overlay_timeline(&segments, &effective);
    let mut list = String::from("ffconcat version 1.0\n");
    for (index, (start, shown)) in timeline.iter().enumerate() {
        let image = shown.map_or(&blank, |shown| &images[shown]);
        list += &format!("file '{}'\n", image.display());
        if let Some((next, _)) = timeline.get(index + 1) {
            list += &format!("duration {:.3}\n", next - start);
        }
    }
    let list_path = work.join("overlay.txt");
    std::fs::write(&list_path, list)?;

    let (x, y) = overlay.placement.position(OUTPUT_SIZE);
    let mut command = ffmpeg();
    command
        .arg("-i")
        .arg(video)
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list_path)
        .args([
            "-filter_complex",
            &format!("[0:v][1:v]overlay=x={x}:y={y}:eof_action=repeat[v]"),
        ])
        .args(["-map", "[v]", "-map", "0:a?", "-c:a", "copy"])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p",
        ])
        .args(["-movflags", "+faststart"])
        .arg(output);
    run_ffmpeg(command)
}

/// When each overlay frame starts showing in the video, in seconds; `None`
/// shows nothing. `segments` are the video's segments with their durations
/// and `effective` when each frame takes effect, in order.
fn overlay_timeline(
    segments: &[(Anchor, f64)],
    effective: &[DateTime<Utc>],
) -> Vec<(f64, Option<usize>)> {
    let mut timeline: Vec<(f64, Option<usize>)> = Vec::new();
    let mut push = |start: f64, shown: Option<usize>| {
        if timeline.last().is_none_or(|&(_, last)| last != shown) {
            timeline.push((start, shown));
        }
    };
    for (anchor, duration) in segments {
        let shown_at_start = effective.iter().rposition(|&at| at <= anchor.world_time);
        push(anchor.video_offset, shown_at_start);
        for (index, &at) in effective.iter().enumerate() {
            let into_segment = (at - anchor.world_time).as_seconds_f64();
            if into_segment > 0.0 && into_segment < *duration {
                push(anchor.video_offset + into_segment, Some(index));
            }
        }
    }
    timeline
}

fn write_transparent_png(path: &Path, (width, height): (u32, u32)) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(&vec![0; (width * height * 4) as usize])?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_timeline_follows_the_scoreboard_through_segments() {
        let time = |seconds: i64| DateTime::from_timestamp(1_000_000 + seconds, 0).unwrap();
        let segment = |start, video_offset, duration| {
            (
                Anchor {
                    world_time: time(start),
                    video_offset,
                },
                duration,
            )
        };
        // Points at 0-10 s and 30-40 s; the scoreboard changes at 5, 20 and 35 s.
        let segments = [segment(0, 0.0, 10.0), segment(30, 10.0, 10.0)];
        let effective = [time(5), time(20), time(35)];
        assert_eq!(
            overlay_timeline(&segments, &effective),
            vec![
                (0.0, None),
                (5.0, Some(0)),
                (10.0, Some(1)),
                (15.0, Some(2))
            ]
        );
    }
}
