//! Match recordings and rendering them into one stitched video.
//!
//! A recording lives in a directory holding the segment directories sent by
//! the camera, plus [`RECORDING_FILE`] describing it. It is rendered by
//! stitching every segment's frames one after another into [`VIDEO_FILE`],
//! which is uploaded to Arctos with an anchor at the start of each segment
//! so viewers can jump to points.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arctos::{Anchor, Arctos, Footage, OverlayFrame, datetime};
use chrono::{DateTime, Utc};
use protocol::{AUDIO_FILE, MatchManifest, Segment, Side, rig};
use serde::{Deserialize, Serialize};

use crate::encode::{self, Encoder, VideoQuality};
use crate::geometry::StitchConfig;
use crate::gpu::{Gpu, Output, OverlayImage, Stitcher};
use crate::mjpeg::{PairDecoder, PairedFiles};
use crate::overlay::{Overlay, RgbaImage};
use crate::priority;

pub const RECORDING_FILE: &str = "recording.json";
pub const VIDEO_FILE: &str = "panorama.mp4";
const WORK_DIR: &str = "work";
/// Frames between progress reports.
const PROGRESS_INTERVAL: u64 = rig::FPS as u64;

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
    pub stitch: StitchConfig,
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

/// The size a recording stitched with `stitch` is rendered at: with as many
/// pixels per degree as the cameras' recordings have in their middles, or as
/// big as this machine's encoder takes. Blocks while trying the encoder.
pub fn output_size(stitch: &StitchConfig) -> Result<(u32, u32)> {
    encode::largest_size(stitch.aspect_ratio, stitch.native_width(rig::RECORD_SIZE))
}

/// The scoreboard overlay of a recording's match.
pub struct OverlayImages {
    pub placement: Overlay,
    /// In the order they take effect.
    pub frames: Vec<OverlayFrame>,
}

impl OverlayImages {
    /// Fetches the overlay of the recording's match from Arctos, for a
    /// `video` sized render.
    pub async fn fetch(
        arctos: &Arctos,
        recording: &Recording,
        placement: &Overlay,
        video: (u32, u32),
    ) -> Result<Self> {
        let frames = arctos
            .scoreboard_overlay(
                &recording.tournament,
                &recording.manifest.match_id,
                placement.image_size(video),
            )
            .await
            .context("fetching the scoreboard overlay")?;
        Ok(OverlayImages {
            placement: placement.clone(),
            frames,
        })
    }
}

/// Stitches every segment in `dir` into a `size` [`VIDEO_FILE`], with the
/// overlay (if any) drawn on top and the audio (if recorded), reporting
/// progress in `[0, 1]`. Blocks for a long time, giving way to live
/// previews for the GPU (see [`gpu`](crate::gpu)) and to everything else
/// for the processor.
pub fn render(
    dir: &Path,
    recording: &Recording,
    size: (u32, u32),
    quality: VideoQuality,
    overlay: Option<&OverlayImages>,
    progress: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<PathBuf> {
    // Its own thread, as its lowered priority can't be raised again.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("render".into())
            .spawn_scoped(scope, || {
                priority::lower_this_thread();
                render_on_this_thread(dir, recording, size, quality, overlay, progress)
            })?
            .join()
            .map_err(|_| anyhow::anyhow!("rendering panicked"))?
    })
}

fn render_on_this_thread(
    dir: &Path,
    recording: &Recording,
    size: (u32, u32),
    quality: VideoQuality,
    overlay: Option<&OverlayImages>,
    progress: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<PathBuf> {
    let work = dir.join(WORK_DIR);
    std::fs::create_dir_all(&work)?;
    let segments = &recording.manifest.segments;
    let audio = if recording.manifest.audio {
        let audio = work.join(AUDIO_FILE);
        join_audio(dir, segments, &audio).context("joining the audio")?;
        Some(audio)
    } else {
        None
    };
    let overlay_images: Vec<RgbaImage> = overlay
        .map_or(&[][..], |overlay| &overlay.frames)
        .iter()
        .map(|frame| RgbaImage::decode_png(&frame.png))
        .collect::<Result<_>>()
        .context("decoding the scoreboard overlay")?;
    let mut overlay_changes = overlay
        .map(|overlay| overlay_schedule(recording, overlay))
        .unwrap_or_default()
        .into_iter()
        .peekable();
    let overlay_position = overlay.map_or((0, 0), |overlay| overlay.placement.position(size));

    let gpu = Gpu::shared()?;
    let mut stitcher = Stitcher::new(&gpu, rig::RECORD_SIZE, size, Output::Video, &recording.stitch)?;
    tracing::info!(width = size.0, height = size.1, "rendering a recording");
    // Rendered under a temporary name so a partial render never counts as
    // done.
    let partial = work.join(VIDEO_FILE);
    let mut encoder = Encoder::start(&partial, size, rig::FPS, quality, audio.as_deref())?;
    let stretches = segments
        .iter()
        .map(|segment| {
            let source = dir.join(segment.dir_name());
            PairedFiles {
                files: Side::BOTH.map(|side| source.join(side.file_name())),
                frames: segment.frames,
            }
        })
        .collect();
    let mut decoder = PairDecoder::start(stretches, rig::RECORD_SIZE);
    let frames = decoder.frames();
    let mut index = 0;
    while let Some(pair) = decoder.next_pair() {
        let pair = pair?;
        while let Some((_, shown)) = overlay_changes.next_if(|&(start, _)| start <= index) {
            stitcher.set_overlay(shown.map(|shown| OverlayImage {
                image: &overlay_images[shown],
                position: overlay_position,
            }));
        }
        let [left, right] = &pair;
        if let Some(stitched) = stitcher.submit([left.planes(), right.planes()])? {
            encoder.write(stitched)?;
        }
        decoder.recycle(pair);
        index += 1;
        if index % PROGRESS_INTERVAL == 0 {
            progress(index as f32 / frames as f32);
        }
    }
    while let Some(stitched) = stitcher.finish()? {
        encoder.write(stitched)?;
    }
    encoder.finish()?;
    let video = dir.join(VIDEO_FILE);
    std::fs::rename(&partial, &video)?;
    std::fs::remove_dir_all(&work)?;
    Ok(video)
}

/// Joins the segments' audio into `output`, each segment's trimmed or
/// padded with silence to last exactly as long as its frames.
fn join_audio(dir: &Path, segments: &[Segment], output: &Path) -> Result<()> {
    const SAMPLE_BYTES: u64 = 2;
    let mut writer = BufWriter::new(File::create(output)?);
    let (mut frames, mut written) = (0, 0);
    for segment in segments {
        frames += segment.frames as u64;
        let end = frames * rig::AUDIO_SAMPLE_RATE as u64 / rig::FPS as u64 * SAMPLE_BYTES;
        let wanted = end - written;
        let path = dir.join(segment.dir_name()).join(AUDIO_FILE);
        let copied = match File::open(&path) {
            Ok(file) => std::io::copy(&mut file.take(wanted), &mut writer)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(segment = segment.dir_name(), "a segment has no audio");
                0
            }
            Err(error) => return Err(error).context(format!("opening {}", path.display())),
        };
        std::io::copy(&mut std::io::repeat(0).take(wanted - copied), &mut writer)?;
        written = end;
    }
    writer.flush()?;
    Ok(())
}

/// The frame each overlay image starts showing at; `None` shows nothing.
fn overlay_schedule(recording: &Recording, overlay: &OverlayImages) -> Vec<(u64, Option<usize>)> {
    let segments: Vec<(Anchor, f64)> = recording
        .anchors()
        .into_iter()
        .zip(&recording.manifest.segments)
        .map(|(anchor, segment)| (anchor, segment.duration().as_secs_f64()))
        .collect();
    let effective: Vec<DateTime<Utc>> = overlay.frames.iter().map(|f| f.effective_at).collect();
    overlay_timeline(&segments, &effective)
        .into_iter()
        .map(|(start, shown)| ((start * rig::FPS as f64).round() as u64, shown))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_audio_keeps_each_segment_in_time_with_its_frames() {
        let dir = std::env::temp_dir().join(format!("join-audio-test-{}", std::process::id()));
        let segment = |start, frames| Segment {
            start: protocol::Timestamp(start),
            frames,
        };
        // 56 frames last a second: 48000 samples.
        let segments = [segment(1, 56), segment(2, 28), segment(3, 7)];
        let write = |segment: &Segment, samples: usize, value: u8| {
            let path = dir.join(segment.dir_name());
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(AUDIO_FILE), vec![value; samples * 2]).unwrap();
        };
        write(&segments[0], 47_000, 1);
        write(&segments[1], 30_000, 2);
        let output = dir.join("joined.pcm");
        join_audio(&dir, &segments, &output).unwrap();
        let joined = std::fs::read(&output).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let expected: Vec<u8> = [(47_000, 1), (1_000, 0), (24_000, 2), (6_000, 0)]
            .into_iter()
            .flat_map(|(samples, value)| vec![value; samples * 2])
            .collect();
        assert_eq!(joined.len(), expected.len());
        assert!(joined == expected);
    }

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
