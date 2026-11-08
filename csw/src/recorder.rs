//! Turns the stream of frames into per-match recordings of just the play.
//!
//! A frame is discarded only once a confirmation from the base station is
//! newer than it, no open point covers it, and it lies outside every finished
//! point widened by the margins. Until then it is kept: in memory for the
//! buffer interval, and on disk after that, so a long point or a lost link
//! does not fill the camera's memory or drop the footage. A new match
//! finalizes the one being recorded. Footage the old points cover stays with
//! that match, and the rest stays for the new one.

use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufWriter, ErrorKind, Write};
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::{Result, bail};
use protocol::{
    AUDIO_FILE, DEFAULT_MARGINS, Directive, Margins, MatchManifest, Segment, Side, Span, Timestamp, rig,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};

use crate::pairing::{Pairer, StereoFrame};

pub const MANIFEST_FILE: &str = "manifest.json";
/// Segments finished so far, written so they can upload before the match ends.
pub const CLOSED_FILE: &str = "closed.json";
/// Frames kept past the memory buffer, outside any match directory so a
/// finished match can be deleted while a later one still needs them.
const HOLD_DIR: &str = ".hold";

pub enum Input {
    Frame(CameraFrame),
    Audio(AudioChunk),
    /// What to record, and the base-station time of the read it came from.
    Guide { directive: Directive, confirmed_at: Timestamp },
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
    /// Woken when a finished segment, or the finished match, is ready to upload.
    pub uploads: Arc<Notify>,
    /// The footage to keep around each point.
    pub margins: watch::Receiver<Margins>,
    /// How long undecided frames stay in memory before spilling to disk.
    pub buffer: watch::Receiver<Duration>,
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
    frames: VecDeque<Held>,
    audio: VecDeque<AudioChunk>,
    session: Option<Session>,
    /// Newest confirmation applied to the open match. Frames at or after it
    /// are still undecided.
    confirmed_at: Timestamp,
    /// Matches already finished. A later record of one of them is ignored.
    finished: BTreeSet<String>,
}

enum Held {
    Memory(StereoFrame),
    Disk(Timestamp),
}

impl Held {
    fn timestamp(&self) -> Timestamp {
        match self {
            Held::Memory(frame) => frame.timestamp,
            Held::Disk(timestamp) => *timestamp,
        }
    }
}

/// What to do with a frame whose time is `t`.
#[derive(Debug, PartialEq, Eq)]
enum Fate {
    /// A later confirmation could still cover it, or stop covering it.
    Wait,
    Keep,
    Drop,
}

fn fate(timestamp: Timestamp, play: &[Span], margins: Margins, confirmed_at: Timestamp) -> Fate {
    if timestamp >= confirmed_at {
        return Fate::Wait;
    }
    let mut covered = false;
    let mut open = false;
    for span in play {
        if span.covers(timestamp, margins) {
            covered = true;
            if span.end.is_none() {
                open = true;
            }
        }
    }
    if !covered {
        Fate::Drop
    } else if open {
        Fate::Wait
    } else {
        Fate::Keep
    }
}

impl State {
    fn new(config: Recorder) -> Self {
        // An open recording doesn't survive a restart, so neither do the
        // frames it was holding.
        let _ = fs::remove_dir_all(config.root.join(HOLD_DIR));
        State {
            config,
            pairer: Pairer::default(),
            frames: VecDeque::new(),
            audio: VecDeque::new(),
            session: None,
            confirmed_at: Timestamp(0),
            finished: BTreeSet::new(),
        }
    }

    fn run(mut self, inputs: Receiver<Input>) -> Result<()> {
        for input in inputs {
            match input {
                Input::Guide { directive, confirmed_at } => self.on_guide(directive, confirmed_at)?,
                Input::Frame(frame) if self.session.is_some() => {
                    for stereo in self.pairer.push(frame) {
                        self.frames.push_back(Held::Memory(stereo));
                    }
                    self.release()?;
                }
                Input::Audio(chunk) if self.session.is_some() => self.audio.push_back(chunk),
                // Stragglers from before the match started or after it ended.
                Input::Frame(_) | Input::Audio(_) => {}
            }
        }
        bail!("input channel closed")
    }

    fn on_guide(&mut self, directive: Directive, confirmed_at: Timestamp) -> Result<()> {
        match directive {
            Directive::Idle => Ok(()),
            Directive::Record { match_id, play } => {
                if self.finished.contains(&match_id) {
                    return Ok(());
                }
                self.record(match_id, play, confirmed_at)
            }
            Directive::Finalize { match_id, play } => {
                if self.session.as_ref().is_none_or(|session| session.match_id != match_id) {
                    return Ok(());
                }
                self.confirm(confirmed_at);
                self.finalize(play)
            }
        }
    }

    fn confirm(&mut self, confirmed_at: Timestamp) {
        if confirmed_at > self.confirmed_at {
            self.confirmed_at = confirmed_at;
        }
    }

    fn record(&mut self, match_id: String, play: Vec<Span>, confirmed_at: Timestamp) -> Result<()> {
        if self.session.as_ref().is_some_and(|session| session.match_id != match_id) {
            self.confirm(confirmed_at);
            return self.hand_off(match_id, play);
        }
        if self.session.is_none() {
            // The previous match's confirmation is on whatever clock the base
            // station had then. A step between matches must not decide this one.
            self.confirmed_at = confirmed_at;
            self.start(match_id, play)?;
        } else {
            self.confirm(confirmed_at);
            self.session.as_mut().expect("checked").play = play;
        }
        self.release()
    }

    /// Ends the open match at the current confirmation and starts `match_id`.
    /// Frames the old points do not cover stay in the buffer.
    fn hand_off(&mut self, match_id: String, play: Vec<Span>) -> Result<()> {
        self.close_open_spans();
        self.flush_pairer();
        self.commit_covered()?;
        self.finish_session()?;
        self.start(match_id, play)?;
        self.release()
    }

    fn finalize(&mut self, play: Vec<Span>) -> Result<()> {
        self.session.as_mut().expect("an open match").play = play;
        self.close_open_spans();
        self.decide_all()?;
        self.config.recording.store(false, Ordering::Relaxed);
        self.finish_session()?;
        self.pairer = Pairer::default();
        self.audio.clear();
        Ok(())
    }

    fn start(&mut self, match_id: String, play: Vec<Span>) -> Result<()> {
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
        Ok(())
    }

    /// Gives an open point an end, so frames after the confirmation can belong
    /// to the next match instead of being covered forever.
    fn close_open_spans(&mut self) {
        let confirmed_at = self.confirmed_at;
        let Some(session) = &mut self.session else {
            return;
        };
        for span in &mut session.play {
            if span.end.is_none() {
                span.end = Some(confirmed_at.max(span.start));
            }
        }
    }

    fn flush_pairer(&mut self) {
        for frame in self.pairer.flush() {
            self.frames.push_back(Held::Memory(frame));
        }
    }

    /// Keeps frames the open match covers and leaves the rest, in order.
    fn commit_covered(&mut self) -> Result<()> {
        let margins = *self.config.margins.borrow();
        let play = self.session.as_ref().expect("session").play.clone();
        let mut rest = VecDeque::new();
        let mut skipping = false;
        while let Some(held) = self.frames.pop_front() {
            if play.iter().any(|span| span.covers(held.timestamp(), margins)) {
                if skipping {
                    self.close_segment()?;
                    skipping = false;
                }
                let frame = self.realize(held)?;
                self.write_frame(frame)?;
            } else {
                skipping = true;
                rest.push_back(held);
            }
        }
        self.frames = rest;
        Ok(())
    }

    /// Keeps or drops every held frame. Open points have already been closed.
    fn decide_all(&mut self) -> Result<()> {
        self.flush_pairer();
        let margins = *self.config.margins.borrow();
        let play = self.session.as_ref().expect("session").play.clone();
        let mut skipping = false;
        while let Some(held) = self.frames.pop_front() {
            if play.iter().any(|span| span.covers(held.timestamp(), margins)) {
                if skipping {
                    self.close_segment()?;
                    skipping = false;
                }
                let frame = self.realize(held)?;
                self.write_frame(frame)?;
            } else {
                skipping = true;
                self.discard(held)?;
                self.close_segment()?;
            }
        }
        Ok(())
    }

    fn release(&mut self) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        let margins = *self.config.margins.borrow();
        let play = self.session.as_ref().expect("session").play.clone();
        let confirmed_at = self.confirmed_at;
        loop {
            let Some(timestamp) = self.frames.front().map(Held::timestamp) else {
                break;
            };
            match fate(timestamp, &play, margins, confirmed_at) {
                Fate::Wait => break,
                Fate::Drop => {
                    let held = self.frames.pop_front().expect("front");
                    self.discard(held)?;
                    self.close_segment()?;
                }
                Fate::Keep => {
                    let held = self.frames.pop_front().expect("front");
                    let frame = self.realize(held)?;
                    self.write_frame(frame)?;
                }
            }
        }
        self.spill()
    }

    fn spill(&mut self) -> Result<()> {
        let limit = self.config.buffer.borrow().as_micros() as i64;
        loop {
            let newest = self.frames.iter().rev().find_map(|held| match held {
                Held::Memory(frame) => Some(frame.timestamp),
                Held::Disk(_) => None,
            });
            let Some(newest) = newest else {
                break;
            };
            let Some(index) = self.frames.iter().position(|held| matches!(held, Held::Memory(_))) else {
                break;
            };
            if newest.micros_since(self.frames[index].timestamp()) <= limit {
                break;
            }
            let held = self.frames.remove(index).expect("the frame is in the buffer");
            let Held::Memory(frame) = held else {
                self.frames.insert(index, held);
                break;
            };
            if let Err(error) = self.spill_frame(&frame) {
                self.frames.insert(index, Held::Memory(frame));
                return Err(error);
            }
            self.frames.insert(index, Held::Disk(frame.timestamp));
        }
        Ok(())
    }

    fn write_frame(&mut self, frame: StereoFrame) -> Result<()> {
        let margins = *self.config.margins.borrow();
        let session = self.session.as_mut().expect("a frame is written during a session");
        let before = session.segments.len();
        session.write(frame, margins, &mut self.audio)?;
        if session.segments.len() != before {
            self.config.uploads.notify_one();
        }
        Ok(())
    }

    fn close_segment(&mut self) -> Result<()> {
        let Some(session) = &mut self.session else {
            return Ok(());
        };
        let before = session.segments.len();
        session.close()?;
        if session.segments.len() != before {
            self.config.uploads.notify_one();
        }
        Ok(())
    }

    fn finish_session(&mut self) -> Result<()> {
        let Some(mut session) = self.session.take() else {
            return Ok(());
        };
        session.close()?;
        self.finished.insert(session.match_id.clone());
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

    fn hold_paths(&self, timestamp: Timestamp) -> [PathBuf; 2] {
        let dir = self.config.root.join(HOLD_DIR);
        [
            dir.join(format!("{}.left", timestamp.0)),
            dir.join(format!("{}.right", timestamp.0)),
        ]
    }

    fn spill_frame(&self, frame: &StereoFrame) -> Result<()> {
        fs::create_dir_all(self.config.root.join(HOLD_DIR))?;
        for (path, jpeg) in self.hold_paths(frame.timestamp).into_iter().zip(&frame.jpegs) {
            fs::write(path, jpeg)?;
        }
        Ok(())
    }

    fn realize(&self, held: Held) -> Result<StereoFrame> {
        match held {
            Held::Memory(frame) => Ok(frame),
            Held::Disk(timestamp) => {
                let paths = self.hold_paths(timestamp);
                let jpegs = [fs::read(&paths[0])?, fs::read(&paths[1])?];
                self.discard(Held::Disk(timestamp))?;
                Ok(StereoFrame { timestamp, jpegs })
            }
        }
    }

    fn discard(&self, held: Held) -> Result<()> {
        if let Held::Disk(timestamp) = held {
            for path in self.hold_paths(timestamp) {
                if let Err(error) = fs::remove_file(&path)
                    && error.kind() != ErrorKind::NotFound
                {
                    return Err(error.into());
                }
            }
        }
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
    fn write(&mut self, frame: StereoFrame, margins: Margins, audio: &mut VecDeque<AudioChunk>) -> Result<()> {
        let timestamp = frame.timestamp;
        let keep = self.play.iter().any(|span| span.covers(timestamp, margins));
        if keep {
            match &mut self.open {
                Some(open) => open.append(frame)?,
                None => self.open = Some(OpenSegment::create(&self.dir, frame, self.audio)?),
            }
        } else {
            self.close()?;
        }
        self.write_audio_before(timestamp + Duration::from_micros(rig::frame_micros() as u64), audio)
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
            self.write_closed()?;
        }
        Ok(())
    }

    fn write_closed(&self) -> Result<()> {
        let closed = ClosedSegments {
            audio: self.audio,
            segments: self.segments.clone(),
        };
        let temp = self.dir.join(format!("{CLOSED_FILE}.tmp"));
        fs::write(&temp, serde_json::to_vec(&closed)?)?;
        fs::rename(temp, self.dir.join(CLOSED_FILE))?;
        Ok(())
    }
}

/// Segments of a match that are finished and can upload while it goes on.
#[derive(Serialize, Deserialize)]
pub struct ClosedSegments {
    pub audio: bool,
    pub segments: Vec<Segment>,
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
            Ok(BufWriter::with_capacity(1 << 20, File::create(dir.join(name))?))
        };
        let mut open = OpenSegment {
            segment,
            last: StereoFrame {
                timestamp: first.timestamp,
                jpegs: Default::default(),
            },
            video: [create(Side::Left.file_name())?, create(Side::Right.file_name())?],
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    fn at(seconds: i64) -> Timestamp {
        Timestamp(seconds * 1_000_000)
    }

    fn margins() -> Margins {
        Margins {
            before: Duration::from_secs(10),
            after: Duration::from_secs(10),
        }
    }

    fn state(root: &Path, buffer: Duration) -> State {
        let (_margins, margins) = watch::channel(DEFAULT_MARGINS);
        let (_buffer, buffer) = watch::channel(buffer);
        State::new(Recorder {
            root: root.to_owned(),
            audio: Arc::new(AtomicBool::new(false)),
            recording: Arc::new(AtomicBool::new(false)),
            uploads: Arc::new(Notify::new()),
            margins,
            buffer,
        })
    }

    fn temp(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("csw-recorder-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn record(match_id: &str, play: Vec<Span>) -> Directive {
        Directive::Record {
            match_id: match_id.to_owned(),
            play,
        }
    }

    fn feed(state: &mut State, timestamp: Timestamp) -> Result<()> {
        for side in Side::BOTH {
            for frame in state.pairer.push(CameraFrame {
                side,
                timestamp,
                jpeg: vec![1, 2, 3],
            }) {
                state.frames.push_back(Held::Memory(frame));
            }
        }
        if state.session.is_some() {
            state.release()?;
        }
        Ok(())
    }

    #[test]
    fn a_frame_waits_until_it_is_confirmed() {
        let play = [Span {
            start: at(0),
            end: Some(at(10)),
        }];
        assert_eq!(fate(at(5), &play, margins(), at(5)), Fate::Wait);
        assert_eq!(fate(at(5), &play, margins(), at(6)), Fate::Keep);
    }

    #[test]
    fn an_open_point_holds_its_frames() {
        let play = [Span { start: at(0), end: None }];
        assert_eq!(fate(at(100), &play, margins(), at(1_000)), Fate::Wait);
    }

    #[test]
    fn a_confirmed_gap_is_discarded() {
        let play = [Span {
            start: at(0),
            end: Some(at(10)),
        }];
        assert_eq!(fate(at(15), &play, margins(), at(40)), Fate::Keep);
        assert_eq!(fate(at(21), &play, margins(), at(40)), Fate::Drop);
    }

    #[test]
    fn idle_does_not_finish_an_open_match() -> Result<()> {
        let root = temp("idle");
        let mut state = state(&root, Duration::from_secs(12));
        state.on_guide(record("a", vec![Span { start: at(0), end: None }]), at(1))?;
        state.on_guide(Directive::Idle, at(2))?;
        assert!(state.session.is_some());
        assert!(!root.join("a").join(MANIFEST_FILE).exists());
        Ok(())
    }

    #[test]
    fn a_finished_match_is_not_recorded_again() -> Result<()> {
        let root = temp("finished");
        let mut state = state(&root, Duration::from_secs(12));
        let play = vec![Span {
            start: at(0),
            end: Some(at(1)),
        }];
        state.on_guide(record("a", play.clone()), at(1))?;
        state.on_guide(
            Directive::Finalize {
                match_id: "a".to_owned(),
                play,
            },
            at(2),
        )?;
        assert!(state.session.is_none());
        state.on_guide(record("a", vec![Span { start: at(5), end: None }]), at(6))?;
        assert!(state.session.is_none());
        assert!(!root.join("a").exists());
        Ok(())
    }

    #[test]
    fn switching_matches_keeps_frames_the_old_point_does_not_cover() -> Result<()> {
        let root = temp("switch");
        let mut state = state(&root, Duration::from_secs(12));
        state.on_guide(
            record(
                "a",
                vec![Span {
                    start: at(0),
                    end: Some(at(1)),
                }],
            ),
            at(2),
        )?;
        feed(&mut state, at(0))?;
        feed(&mut state, at(1))?;
        feed(&mut state, at(20))?;
        let play = vec![Span {
            start: at(20),
            end: Some(at(21)),
        }];
        state.on_guide(record("b", play.clone()), at(30))?;
        state.on_guide(
            Directive::Finalize {
                match_id: "b".to_owned(),
                play,
            },
            at(31),
        )?;
        let manifest = |id: &str| -> MatchManifest {
            let bytes = fs::read(root.join(id).join(MANIFEST_FILE)).unwrap();
            serde_json::from_slice(&bytes).unwrap()
        };
        let old = manifest("a");
        assert_eq!(old.segments.len(), 1);
        assert_eq!(old.segments[0].start, at(0));
        let new = manifest("b");
        assert_eq!(new.segments.len(), 1);
        assert_eq!(new.segments[0].start, at(20));
        Ok(())
    }

    #[test]
    fn frames_past_the_memory_window_spill_and_can_still_be_kept() -> Result<()> {
        let root = temp("spill");
        let mut state = state(&root, Duration::ZERO);
        state.frames.push_back(Held::Memory(StereoFrame {
            timestamp: at(0),
            jpegs: [vec![7], vec![8]],
        }));
        state.frames.push_back(Held::Memory(StereoFrame {
            timestamp: at(2),
            jpegs: [vec![9], vec![10]],
        }));
        state.spill()?;
        assert!(matches!(state.frames.front(), Some(Held::Disk(_))));
        let play = vec![Span {
            start: at(0),
            end: Some(at(2)),
        }];
        state.on_guide(record("a", play.clone()), at(3))?;
        state.on_guide(
            Directive::Finalize {
                match_id: "a".to_owned(),
                play,
            },
            at(4),
        )?;
        let left = fs::read(root.join("a").join(at(0).0.to_string()).join(Side::Left.file_name()))?;
        assert!(left.windows(1).any(|bytes| bytes == [7]));
        assert!(left.windows(1).any(|bytes| bytes == [9]));
        assert!(!root.join(HOLD_DIR).join(format!("{}.left", at(0).0)).exists());
        Ok(())
    }
}
