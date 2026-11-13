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
    /// Live preview resolutions each camera can be told to send: scaled by
    /// the ISP from the same 16:9 sensor mode, so all with the same field of
    /// view.
    pub const PREVIEW_SIZES: [(u32, u32); 1] = [(1536, 864)];
    pub const DEFAULT_PREVIEW_SIZE: (u32, u32) = (1536, 864);
    /// Sensor frame rate.
    pub const FPS: u32 = 56;
    /// Every `IDLE_PREVIEW_DECIMATION`th sensor frame is encoded while idle.
    pub const IDLE_PREVIEW_DECIMATION: u32 = 4;
    /// Every `RECORD_PREVIEW_DECIMATION`th recording JPEG is also sent live.
    pub const RECORD_PREVIEW_DECIMATION: u32 = 2;
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

    /// The encoded video that replaces [`Self::file_name`].
    pub fn video_file_name(self) -> &'static str {
        match self {
            Side::Left => LEFT_VIDEO,
            Side::Right => RIGHT_VIDEO,
        }
    }
}

/// Sent by a camera when it (re)connects to the base station.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Registration {
    /// Identifies the exact camera software build. Older cameras omit it.
    #[serde(default)]
    pub software_id: Option<String>,
    /// The rig's calibration (a `stitching::geometry::Calibration`). Opaque
    /// to the camera.
    pub calibration: serde_json::Value,
    /// The setup the camera keeps (see [`Instructions::setup`]); `None` if
    /// it has never been given one.
    pub setup: Option<serde_json::Value>,
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
/// The JPEG quality cameras record at unless told otherwise.
pub const DEFAULT_RECORD_QUALITY: u8 = 80;
/// The JPEG quality of the previews unless told otherwise.
pub const DEFAULT_PREVIEW_QUALITY: u8 = 50;
/// The footage cameras keep around each point unless told otherwise.
pub const DEFAULT_MARGINS: Margins = Margins {
    before: Duration::from_secs(10),
    after: Duration::from_secs(10),
};
/// How long cameras keep undecided frames in memory unless told otherwise
/// (see [`Instructions::buffer`]). Older undecided frames spill to disk.
pub const DEFAULT_BUFFER: Duration = Duration::from_secs(6);

/// What the base station currently wants a camera to do, polled at 1 Hz.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Instructions {
    pub directive: Directive,
    /// Base-station time of the Arctos read behind [`Self::directive`].
    /// Frames newer than this are not discarded. It stays put when that read
    /// failed, or was of some match other than the one being recorded.
    pub confirmed_at: Timestamp,
    /// ALSA PCM name of the microphone to capture.
    pub audio_device: String,
    /// JPEG quality (1 to 100) of the frames recorded.
    pub record_quality: u8,
    /// One of [`rig::PREVIEW_SIZES`]. Changing it restarts capturing, so
    /// the camera waits until it isn't recording.
    pub preview_size: (u32, u32),
    /// JPEG quality (1 to 100) of the previews.
    pub preview_quality: u8,
    pub focus: Focus,
    pub margins: Margins,
    /// How long undecided frames stay in memory. Older ones are kept on disk
    /// until a confirmation says whether they fall in a point.
    pub buffer: Duration,
    /// How the rig is set up (a `bsw` `RigSetup`: its calibration, view,
    /// blend, and so on), for the camera to keep and send back in
    /// [`Registration::setup`] so that any base station stitches it the same
    /// way. Opaque to the camera.
    pub setup: serde_json::Value,
}

/// How much footage a camera keeps around each point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Margins {
    /// From before the point starts.
    pub before: Duration,
    /// From after the point ends.
    pub after: Duration,
}

/// Where a camera's lenses are focused.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Focus {
    /// Held at these positions, indexed by [`Side::index`]: libcamera's
    /// `LensPosition`, which is higher the closer the lens focuses.
    Fixed { lens_positions: [f32; 2] },
    /// Found by autofocusing once the camera isn't recording, and reported
    /// in [`CameraStats::autofocus`]. A request made at another time asks
    /// again.
    Auto { requested: Timestamp },
}

/// How a camera is doing, sent with each poll for its instructions. Each
/// value is `None` if the camera couldn't tell.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CameraStats {
    /// Share of the CPU's time in use since the last poll, from 0 to 1.
    pub cpu: Option<f64>,
    pub memory: Option<Memory>,
    /// Of the CPU, in degrees Celsius.
    pub temperature: Option<f64>,
    /// Since the camera software started, indexed by [`Side::index`].
    pub frames: [FrameCounts; 2],
    /// The last autofocus asked for since the camera software started.
    pub autofocus: Option<AutofocusOutcome>,
    /// The link the camera reaches the base station on, if it could be read.
    #[serde(default)]
    pub link: Option<Link>,
}

/// Bytes on a network link since the machine booted, and how fast it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Megabits a second the link can carry. Absent when the operating
    /// system won't say, as with Wi-Fi.
    pub megabits: Option<u32>,
    /// Sent from this machine.
    pub sent: u64,
    /// Received by this machine.
    pub received: u64,
}

/// Where autofocusing put a camera's lenses.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutofocusOutcome {
    /// When it was asked for (see [`Focus::Auto`]).
    pub requested: Timestamp,
    /// Each lens's position, as in [`Focus::Fixed`]; `None` for one that
    /// found nothing to focus on.
    pub lens_positions: [Option<f32>; 2],
}

/// Bytes of memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub used: u64,
    pub total: u64,
}

/// Running totals of one camera's frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameCounts {
    /// Delivered by the sensor.
    pub captured: u64,
    pub dropped: u64,
    /// Previews due, including those dropped.
    pub previews: u64,
    /// Previews dropped because the camera's encoding or the base station
    /// wasn't keeping up.
    pub previews_dropped: u64,
}

/// A stretch of play, e.g. one point. `end` is `None` while it is ongoing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: Timestamp,
    pub end: Option<Timestamp>,
}

impl Span {
    /// Whether `t` lies within this span, widened by `margins`.
    pub fn covers(&self, t: Timestamp, margins: Margins) -> bool {
        t >= self.start - margins.before && self.end.is_none_or(|end| t <= end + margins.after)
    }
}

/// What to record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Directive {
    /// No match is in progress. Ignored while one is open: a match ends only
    /// through [`Directive::Finalize`] or because another match starts.
    Idle,
    /// Keep footage near any of `play`. A different open match is finished
    /// first; footage that `play` does not cover is left for the new one.
    Record { match_id: String, play: Vec<Span> },
    /// The match is over. Finish it, keeping `play`. A repeat for a match
    /// that is no longer open is ignored, so losing one poll still ends it.
    Finalize { match_id: String, play: Vec<Span> },
}

/// One contiguous run of frames, stored as a directory named after its start
/// time containing [`LEFT_FILE`], [`RIGHT_FILE`] and (if the match has audio)
/// [`AUDIO_FILE`]. The base station replaces the JPEG files with
/// [`LEFT_VIDEO`] and [`RIGHT_VIDEO`] before stitching or uploading.
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
/// The left camera's frames encoded as H.264, which replaces [`LEFT_FILE`].
pub const LEFT_VIDEO: &str = "left.mp4";
/// The right camera's frames encoded as H.264, which replaces [`RIGHT_FILE`].
pub const RIGHT_VIDEO: &str = "right.mp4";
/// Raw PCM audio (see [`rig::AUDIO_SAMPLE_RATE`]) starting at the segment start.
pub const AUDIO_FILE: &str = "audio.pcm";

/// Sent by a camera before it uploads a finished match's files.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct UploadStart {
    /// Of all the files together.
    pub bytes: u64,
}

/// Describes a finished match recording; sent after all of its files are
/// uploaded. The base station acknowledges it once everything is on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchManifest {
    pub match_id: String,
    pub audio: bool,
    pub segments: Vec<Segment>,
}

impl MatchManifest {
    /// The files the camera uploads: a Motion JPEG for each camera, and the
    /// audio, if recorded.
    pub fn files(&self) -> impl Iterator<Item = (Segment, &'static str)> + '_ {
        let audio: &[&str] = if self.audio { &[AUDIO_FILE] } else { &[] };
        self.segments.iter().flat_map(move |&segment| {
            [LEFT_FILE, RIGHT_FILE]
                .iter()
                .chain(audio)
                .map(move |&file| (segment, file))
        })
    }

    /// The files kept once the JPEGs have been encoded: an H.264 MP4 for each
    /// camera, and the audio, if recorded.
    pub fn video_files(&self) -> impl Iterator<Item = (Segment, &'static str)> + '_ {
        let audio: &[&str] = if self.audio { &[AUDIO_FILE] } else { &[] };
        self.segments.iter().flat_map(move |&segment| {
            [LEFT_VIDEO, RIGHT_VIDEO]
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
    pub timestamp: Timestamp,
    pub jpeg: B,
}

/// A message of the live preview WebSocket, sent as one binary message:
/// `[frame tag: u8][timestamp: i64 LE][jpeg...]` for frames, or
/// `[AUDIO_TAG: u8][pcm...]` for audio.
#[derive(Clone, Debug)]
pub enum PreviewMessage<B> {
    Frame(PreviewFrame<B>),
    /// Microphone audio, as in [`AUDIO_FILE`], continuing the previous
    /// audio message.
    Audio(B),
}

const AUDIO_TAG: u8 = 2;
const FRAME_TAG_BASE: u8 = 3;

impl<B: AsRef<[u8]>> PreviewMessage<B> {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            PreviewMessage::Frame(frame) => {
                let jpeg = frame.jpeg.as_ref();
                let mut out = Vec::with_capacity(9 + jpeg.len());
                out.push(FRAME_TAG_BASE + frame.side as u8);
                out.extend_from_slice(&frame.timestamp.0.to_le_bytes());
                out.extend_from_slice(jpeg);
                out
            }
            PreviewMessage::Audio(pcm) => {
                let pcm = pcm.as_ref();
                let mut out = Vec::with_capacity(1 + pcm.len());
                out.push(AUDIO_TAG);
                out.extend_from_slice(pcm);
                out
            }
        }
    }
}

/// The interface in a Linux `/proc/net/route` table that carries packets to
/// `address`, or the default route when `address` is absent. Absent when
/// the table has no usable route.
pub fn route_interface(table: &str, address: Option<std::net::Ipv4Addr>) -> Option<&str> {
    let wanted = address.map(u32::from);
    let mut best: Option<(&str, u32)> = None;
    for line in table.lines().skip(1) {
        let Some((interface, destination, mask)) = route_row(line) else {
            continue;
        };
        let matches = match wanted {
            Some(address) => address & mask == destination,
            None => destination == 0,
        };
        if matches && best.is_none_or(|(_, best_mask)| mask >= best_mask) {
            best = Some((interface, mask));
        }
    }
    best.map(|(interface, _)| interface)
}

/// One data row of `/proc/net/route`: interface, destination and mask, in
/// host byte order.
fn route_row(line: &str) -> Option<(&str, u32, u32)> {
    let mut fields = line.split_whitespace();
    let interface = fields.next()?;
    if interface.is_empty() || interface.contains('/') {
        return None;
    }
    let destination = u32::from_str_radix(fields.next()?, 16).ok()?.swap_bytes();
    let mask = u32::from_str_radix(fields.nth(5)?, 16).ok()?.swap_bytes();
    Some((interface, destination, mask))
}

impl PreviewMessage<Bytes> {
    /// Splits an encoded message; `None` if it is malformed.
    pub fn decode(message: Bytes) -> Option<Self> {
        let tag = *message.first()?;
        if tag == AUDIO_TAG {
            return Some(PreviewMessage::Audio(message.slice(1..)));
        }
        Some(match tag {
            0 => PreviewMessage::Frame(PreviewFrame {
                side: Side::Left,
                timestamp: Timestamp(0),
                jpeg: message.slice(1..),
            }),
            1 => PreviewMessage::Frame(PreviewFrame {
                side: Side::Right,
                timestamp: Timestamp(0),
                jpeg: message.slice(1..),
            }),
            FRAME_TAG_BASE => PreviewMessage::Frame(PreviewFrame {
                side: Side::Left,
                timestamp: Timestamp(i64::from_le_bytes(message.get(1..9)?.try_into().ok()?)),
                jpeg: message.slice(9..),
            }),
            tag if tag == FRAME_TAG_BASE + 1 => PreviewMessage::Frame(PreviewFrame {
                side: Side::Right,
                timestamp: Timestamp(i64::from_le_bytes(message.get(1..9)?.try_into().ok()?)),
                jpeg: message.slice(9..),
            }),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_picks_the_most_specific_interface() {
        let table = "\
Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT
wlan0 00000000 0100A8C0 0003 0 0 200 00000000 0 0 0
eth0 00002A0A 00000000 0001 0 0 100 00FFFFFF 0 0 0
";
        let camera_network = std::net::Ipv4Addr::new(10, 42, 0, 5);
        assert_eq!(route_interface(table, Some(camera_network)), Some("eth0"));
        assert_eq!(
            route_interface(table, Some(std::net::Ipv4Addr::new(1, 2, 3, 4))),
            Some("wlan0")
        );
        assert_eq!(route_interface(table, None), Some("wlan0"));
    }

    #[test]
    fn an_old_camera_report_has_no_link() {
        let stats: CameraStats = serde_json::from_str(
            r#"{"cpu":null,"memory":null,"temperature":null,"frames":[
                {"captured":0,"dropped":0,"previews":0,"previews_dropped":0},
                {"captured":0,"dropped":0,"previews":0,"previews_dropped":0}
            ],"autofocus":null}"#,
        )
        .unwrap();
        assert_eq!(stats.link, None);
    }

    #[test]
    fn spans_cover_their_margins() {
        let second = |seconds: i64| Timestamp(seconds * 1_000_000);
        let margins = Margins {
            before: Duration::from_secs(2),
            after: Duration::from_secs(5),
        };
        let point = Span {
            start: second(100),
            end: Some(second(110)),
        };
        assert!(!point.covers(second(97), margins));
        assert!(point.covers(second(98), margins));
        assert!(point.covers(second(115), margins));
        assert!(!point.covers(second(116), margins));
        let ongoing = Span { end: None, ..point };
        assert!(ongoing.covers(second(1_000), margins));
    }

    #[test]
    fn finalize_is_an_end() {
        let directive = Directive::Finalize {
            match_id: "m".to_owned(),
            play: vec![Span {
                start: Timestamp(1),
                end: None,
            }],
        };
        let json = serde_json::to_value(&directive).unwrap();
        assert_eq!(json["state"], "finalize");
        assert_eq!(serde_json::from_value::<Directive>(json).unwrap(), directive);
    }

    #[test]
    fn registrations_from_cameras_that_keep_no_setup_parse() {
        let registration: Registration =
            serde_json::from_str(r#"{"calibration": {}, "audio_devices": []}"#).unwrap();
        assert_eq!(registration.setup, None);
    }
}
