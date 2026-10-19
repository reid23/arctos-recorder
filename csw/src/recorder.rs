//! Turns the stream of frames into per-match recordings of just the play.
//!
//! While a match is on, stereo frames wait in a ring buffer for [`DELAY`]. By
//! the time a frame leaves the buffer the base station has told us about any
//! point that started up to [`MARGIN`] after it, so we can decide whether the
//! frame is within [`MARGIN`] of play. Kept frames are appended to the open
//! segment and discarding a frame closes it, so each segment covers one
//! stretch of play. Frames the cameras failed to deliver are filled in by
//! repeating the previous one, so segments play back in real time at the
//! rig's frame rate.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::{Result, bail};
use protocol::{AUDIO_FILE, Directive, MatchManifest, Segment, Side, Span, Timestamp, rig};
use tokio::sync::Notify;

use crate::pairing::{Pairer, StereoFrame};

/// How much footage to keep before and after each point.
const MARGIN: Duration = Duration::from_secs(10);
/// [`MARGIN`] plus slack for the base station's polling latency.
const DELAY: Duration = Duration::from_secs(12);

pub const MANIFEST_FILE: &str = "manifest.json";

pub enum Input {
    Frame(CameraFrame),
    Audio(AudioChunk),
    Directive(Directive),
}

pub struct CameraFrame {
    pub side: Side,
    pub timestamp: Timestamp,
    pub jpeg: Vec<u8>,
}

pub struct AudioChunk {
    /// Capture time of the first sample.
    pub start: Timestamp,
    pub samples: Vec<i16>,
}

pub struct Recorder {
    /// Each match is recorded into `root/<match id>/`.
    pub root: PathBuf,
    /// Whether the microphone is being captured; matches that start while
    /// it is get an audio track.
    pub audio: Arc<AtomicBool>,
    /// Tells the cameras to produce full-resolution frames.
    pub recording: Arc<AtomicBool>,
    /// Woken when a finished match is ready to upload.
    pub uploads: Arc<Notify>,
}

impl Recorder {
    pub fn spawn(self, inputs: Receiver<Input>) {
        std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || {
                let error = State::new(self).run(inputs).unwrap_err();
                tracing::error!("recorder failed: {error:#}");
                std::process::exit(1);
            })
            .expect("spawn recorder thread");
    }
}

struct State {
    config: Recorder,
    pairer: Pairer,
    frames: VecDeque<StereoFrame>,
    audio: VecDeque<AudioChunk>,
    session: Option<Session>,
}

impl State {
    fn new(config: Recorder) -> Self {
        State {
            config,
            pairer: Pairer::default(),
            frames: VecDeque::new(),
            audio: VecDeque::new(),
            session: None,
        }
    }

    fn run(mut self, inputs: Receiver<Input>) -> Result<()> {
        for input in inputs {
            match input {
                Input::Directive(directive) => self.on_directive(directive)?,
                Input::Frame(frame) if self.session.is_some() => {
                    self.frames.extend(self.pairer.push(frame));
                    if let Some(newest) = self.frames.back() {
                        self.release_before(newest.timestamp - DELAY)?;
                    }
                }
                Input::Audio(chunk) if self.session.is_some() => self.audio.push_back(chunk),
                // Stragglers from before the match started or after it ended.
                Input::Frame(_) | Input::Audio(_) => {}
            }
        }
        bail!("input channel closed")
    }

    fn on_directive(&mut self, directive: Directive) -> Result<()> {
        match directive {
            Directive::Idle => self.finish(),
            Directive::Record { match_id, play } => {
                if self
                    .session
                    .as_ref()
                    .is_some_and(|s| s.match_id != match_id)
                {
                    self.finish()?;
                }
                match &mut self.session {
                    Some(session) => session.play = play,
                    None => {
                        tracing::info!("recording match {match_id}");
                        let dir = self.config.root.join(&match_id);
                        fs::create_dir_all(&dir)?;
                        self.session = Some(Session {
                            match_id,
                            play,
                            dir,
                            audio: self.config.audio.load(Ordering::Relaxed),
                            segments: Vec::new(),
                            open: None,
                        });
                        self.config.recording.store(true, Ordering::Relaxed);
                    }
                }
                Ok(())
            }
        }
    }

    fn release_before(&mut self, cutoff: Timestamp) -> Result<()> {
        let session = self
            .session
            .as_mut()
            .expect("frames are only buffered during a session");
        while self.frames.front().is_some_and(|f| f.timestamp < cutoff) {
            let frame = self.frames.pop_front().expect("checked non-empty");
            session.write(frame, &mut self.audio)?;
        }
        Ok(())
    }

    /// Flushes the buffer through the final list of points and hands the
    /// recording to the uploader.
    fn finish(&mut self) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        self.config.recording.store(false, Ordering::Relaxed);
        self.release_before(Timestamp(i64::MAX))?;
        let mut session = self.session.take().expect("checked above");
        session.close()?;
        self.pairer = Pairer::default();
        self.audio.clear();

        if session.segments.is_empty() {
            tracing::info!("match {} had no play; discarding", session.match_id);
            fs::remove_dir_all(&session.dir)?;
            return Ok(());
        }
        let manifest = MatchManifest {
            match_id: session.match_id,
            audio: session.audio,
            segments: session.segments,
        };
        tracing::info!(
            "finished match {} with {} segments",
            manifest.match_id,
            manifest.segments.len()
        );
        let temp = session.dir.join(format!("{MANIFEST_FILE}.tmp"));
        fs::write(&temp, serde_json::to_vec(&manifest)?)?;
        fs::rename(&temp, session.dir.join(MANIFEST_FILE))?;
        self.config.uploads.notify_one();
        Ok(())
    }
}

/// The recording of one match in progress.
struct Session {
    match_id: String,
    play: Vec<Span>,
    dir: PathBuf,
    audio: bool,
    segments: Vec<Segment>,
    open: Option<OpenSegment>,
}

impl Session {
    fn write(&mut self, frame: StereoFrame, audio: &mut VecDeque<AudioChunk>) -> Result<()> {
        let timestamp = frame.timestamp;
        let keep = self.play.iter().any(|span| span.covers(timestamp, MARGIN));
        if keep {
            match &mut self.open {
                Some(open) => open.append(frame)?,
                None => self.open = Some(OpenSegment::create(&self.dir, frame, self.audio)?),
            }
        } else {
            self.close()?;
        }
        self.write_audio_before(
            timestamp + Duration::from_micros(rig::frame_micros() as u64),
            audio,
        )
    }

    /// Consumes buffered audio up to `t`, writing whatever falls inside the
    /// open segment.
    fn write_audio_before(&mut self, t: Timestamp, audio: &mut VecDeque<AudioChunk>) -> Result<()> {
        while let Some(chunk) = audio.front_mut() {
            let n = samples_between(chunk.start, t).min(chunk.samples.len());
            if n == 0 {
                break;
            }
            let start = chunk.start;
            let samples: Vec<i16> = chunk.samples.drain(..n).collect();
            chunk.start = start + samples_duration(n);
            if chunk.samples.is_empty() {
                audio.pop_front();
            }
            if let Some(open) = &mut self.open
                && let Some(writer) = &mut open.audio
            {
                let skip = samples_between(start, open.segment.start).min(n);
                for sample in &samples[skip..] {
                    writer.write_all(&sample.to_le_bytes())?;
                }
            }
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(open) = self.open.take() {
            self.segments.push(open.finish()?);
        }
        Ok(())
    }
}

fn samples_between(from: Timestamp, to: Timestamp) -> usize {
    let micros = to.micros_since(from).max(0) as u64;
    (micros * rig::AUDIO_SAMPLE_RATE as u64 / 1_000_000) as usize
}

fn samples_duration(samples: usize) -> Duration {
    Duration::from_micros(samples as u64 * 1_000_000 / rig::AUDIO_SAMPLE_RATE as u64)
}

struct OpenSegment {
    segment: Segment,
    last: StereoFrame,
    /// Indexed by [`Side::index`].
    video: [BufWriter<File>; 2],
    audio: Option<BufWriter<File>>,
}

impl OpenSegment {
    fn create(match_dir: &Path, first: StereoFrame, audio: bool) -> Result<Self> {
        let segment = Segment {
            start: first.timestamp,
            frames: 0,
        };
        let dir = match_dir.join(segment.dir_name());
        fs::create_dir(&dir)?;
        let create = |name: &str| -> Result<BufWriter<File>> {
            Ok(BufWriter::with_capacity(
                1 << 20,
                File::create(dir.join(name))?,
            ))
        };
        let mut open = OpenSegment {
            segment,
            last: StereoFrame {
                timestamp: first.timestamp,
                jpegs: Default::default(),
            },
            video: [
                create(Side::Left.file_name())?,
                create(Side::Right.file_name())?,
            ],
            audio: audio.then(|| create(AUDIO_FILE)).transpose()?,
        };
        open.append(first)?;
        Ok(open)
    }

    /// How many frame periods are missing between the last frame and `t`.
    fn missing_before(&self, t: Timestamp) -> i64 {
        let frame = rig::frame_micros();
        ((t.micros_since(self.last.timestamp) + frame / 2) / frame - 1).max(0)
    }

    fn append(&mut self, frame: StereoFrame) -> Result<()> {
        let missing = self.missing_before(frame.timestamp) as usize;
        for jpegs in iter::repeat_n(&self.last.jpegs, missing).chain([&frame.jpegs]) {
            for (writer, jpeg) in self.video.iter_mut().zip(jpegs) {
                writer.write_all(jpeg)?;
            }
            self.segment.frames += 1;
        }
        self.last = frame;
        Ok(())
    }

    fn finish(self) -> Result<Segment> {
        for writer in self.video.into_iter().chain(self.audio) {
            writer.into_inner()?.sync_all()?;
        }
        Ok(self.segment)
    }
}
