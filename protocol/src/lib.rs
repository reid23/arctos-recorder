//! Wire types shared by `csw` (the camera software on each field's Pi) and
//! `bsw` (the base station desktop app).
//!
//! All timestamps are on the base station's clock: each camera estimates its
//! offset to the base station when it registers and stamps everything it
//! sends accordingly.

use std::ops::{Add, Sub};
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub mod api;

/// The fixed video format of every camera rig.
pub mod rig {
    /// Recording resolution of each camera (full-sensor 2x2 binned IMX708).
    pub const RECORD_SIZE: (u32, u32) = (2304, 1296);
    /// Live preview resolution of each camera (ISP-scaled, same field of view).
    pub const PREVIEW_SIZE: (u32, u32) = (1536, 864);
    /// Sensor frame rate.
    pub const FPS: u32 = 56;
    /// Every `PREVIEW_DECIMATION`th sensor frame is sent as a preview frame.
    pub const PREVIEW_DECIMATION: u32 = 2;
    /// Audio is recorded as mono signed 16-bit little-endian PCM at this rate.
    pub const AUDIO_SAMPLE_RATE: u32 = 48_000;

    /// Nominal duration of one sensor frame.
    pub const fn frame_micros() -> i64 {
        1_000_000 / FPS as i64
    }
}

/// Microseconds since the Unix epoch on the base station's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn micros_since(self, earlier: Timestamp) -> i64 {
        self.0 - earlier.0
    }
}

impl Add<Duration> for Timestamp {
    type Output = Timestamp;
    fn add(self, rhs: Duration) -> Timestamp {
        Timestamp(self.0 + rhs.as_micros() as i64)
    }
}

impl Sub<Duration> for Timestamp {
    type Output = Timestamp;
    fn sub(self, rhs: Duration) -> Timestamp {
        Timestamp(self.0 - rhs.as_micros() as i64)
    }
}

/// Which of the rig's two cameras a frame came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Side {
    Left = 0,
    Right = 1,
}

impl Side {
    pub const BOTH: [Side; 2] = [Side::Left, Side::Right];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Side::Left => LEFT_FILE,
            Side::Right => RIGHT_FILE,
        }
    }
}

/// Sent by a camera when it (re)connects to the base station.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Registration {
    /// The rig's stitching calibration (a `reco` `MatchCalibration` for
    /// [`rig::RECORD_SIZE`]). Opaque to the camera.
    pub calibration: serde_json::Value,
    /// The ALSA capture devices the camera can record audio from.
    pub audio_devices: Vec<AudioDevice>,
}

/// An ALSA capture device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    /// ALSA PCM name, e.g. `plughw:CARD=USB,DEV=0`.
    pub name: String,
    /// e.g. `Jabra SPEAK 410 USB, USB Audio`.
    pub description: String,
}

/// The ALSA PCM cameras capture audio from unless told otherwise.
pub const DEFAULT_AUDIO_DEVICE: &str = "default";

/// What the base station currently wants a camera to do, polled at 1 Hz.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instructions {
    pub directive: Directive,
    /// ALSA PCM name of the microphone to capture.
    pub audio_device: String,
}

/// A stretch of play, e.g. one point. `end` is `None` while it is ongoing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: Timestamp,
    pub end: Option<Timestamp>,
}

impl Span {
    /// Whether `t` lies within `margin` of this span.
    pub fn covers(&self, t: Timestamp, margin: Duration) -> bool {
        t >= self.start - margin && self.end.is_none_or(|end| t <= end + margin)
    }
}

/// What to record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Directive {
    /// No match is in progress on the camera's field.
    Idle,
    /// A match is in progress; keep footage near any of `play`.
    Record { match_id: String, play: Vec<Span> },
}

/// One contiguous run of frames, stored as a directory named after its start
/// time containing [`LEFT_FILE`], [`RIGHT_FILE`] and (if the match has audio)
/// [`AUDIO_FILE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    /// Timestamp of the first frame.
    pub start: Timestamp,
    /// Number of frames in each camera's file.
    pub frames: u32,
}

impl Segment {
    pub fn dir_name(&self) -> String {
        self.start.0.to_string()
    }

    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.frames as f64 / rig::FPS as f64)
    }
}

/// Concatenated baseline JPEG frames from the left camera.
pub const LEFT_FILE: &str = "left.mjpeg";
/// Concatenated baseline JPEG frames from the right camera.
pub const RIGHT_FILE: &str = "right.mjpeg";
/// Raw PCM audio (see [`rig::AUDIO_SAMPLE_RATE`]) starting at the segment start.
pub const AUDIO_FILE: &str = "audio.pcm";

/// Describes a finished match recording; sent after all of its files are
/// uploaded. The base station acknowledges it once everything is on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchManifest {
    pub match_id: String,
    pub audio: bool,
    pub segments: Vec<Segment>,
}

impl MatchManifest {
    pub fn files(&self) -> impl Iterator<Item = (Segment, &'static str)> + '_ {
        let audio: &[&str] = if self.audio { &[AUDIO_FILE] } else { &[] };
        self.segments.iter().flat_map(move |&segment| {
            [LEFT_FILE, RIGHT_FILE]
                .iter()
                .chain(audio)
                .map(move |&file| (segment, file))
        })
    }
}

/// A live preview JPEG of one camera.
#[derive(Clone, Debug)]
pub struct PreviewFrame<B> {
    pub side: Side,
    pub jpeg: B,
}

/// A message of the live preview WebSocket, sent as one binary message:
/// `[tag: u8][payload...]`. The tag is the frame's [`Side`] for frames and
/// [`AUDIO_TAG`] for audio.
#[derive(Clone, Debug)]
pub enum PreviewMessage<B> {
    Frame(PreviewFrame<B>),
    /// Microphone audio, as in [`AUDIO_FILE`], continuing the previous
    /// audio message.
    Audio(B),
}

const AUDIO_TAG: u8 = 2;

impl<B: AsRef<[u8]>> PreviewMessage<B> {
    pub fn encode(&self) -> Vec<u8> {
        let (tag, payload) = match self {
            PreviewMessage::Frame(frame) => (frame.side as u8, frame.jpeg.as_ref()),
            PreviewMessage::Audio(pcm) => (AUDIO_TAG, pcm.as_ref()),
        };
        let mut out = Vec::with_capacity(1 + payload.len());
        out.push(tag);
        out.extend_from_slice(payload);
        out
    }
}

impl PreviewMessage<Bytes> {
    /// Splits an encoded message; `None` if it is malformed.
    pub fn decode(message: Bytes) -> Option<Self> {
        let payload = message.slice(1..);
        Some(match *message.first()? {
            0 => PreviewMessage::Frame(PreviewFrame {
                side: Side::Left,
                jpeg: payload,
            }),
            1 => PreviewMessage::Frame(PreviewFrame {
                side: Side::Right,
                jpeg: payload,
            }),
            AUDIO_TAG => PreviewMessage::Audio(payload),
            _ => return None,
        })
    }
}
