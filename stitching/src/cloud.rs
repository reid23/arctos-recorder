//! HTTP API of `ssw`, the cloud stitching server. Every request carries
//! `Authorization: Bearer <token>`.
//!
//! - `PUT {job}` with a [`JobRequest`] creates the job (or retries a failed
//!   one) and returns its [`JobState`]. It starts once all of the files
//!   listed in [`JobState::missing`] have been uploaded.
//! - `PUT {job}/files/{segment}/{file}` uploads one file of the recording.
//! - `PUT {job}/token` with an Arctos access token as the body gives the
//!   server a (fresh) token for uploading the video. The server only keeps
//!   it in memory, so it must be sent again whenever [`JobState::has_token`]
//!   is false.
//! - `GET {job}` returns the [`JobState`]. Once it is
//!   [`JobStatus::Done`](crate::JobStatus::Done) the video is on Arctos and the
//!   server has deleted the recording. The server needn't keep its jobs
//!   across restarts: a job it doesn't know about must be sent again.
//! - `GET {live}` is a WebSocket for forwarding a camera's live previews to
//!   be stitched and restreamed. The first message is a text [`LiveConfig`];
//!   the rest are binary chunks of an MPEG-TS stream of H.264-encoded
//!   [`StereoYuv`](crate::live::StereoYuv) frames.

use protocol::Segment;
use reco_core::calibration::MatchCalibration;
use serde::{Deserialize, Serialize};

use crate::JobStatus;
use crate::overlay::{LiveOverlay, Overlay};
use crate::recording::{Recording, RecordingKey};

pub const PORT: u16 = 7800;

pub fn job(key: &RecordingKey) -> String {
    format!("/api/jobs/{}/{}", key.match_id, key.camera)
}

pub fn file(key: &RecordingKey, segment: &Segment, file: &str) -> String {
    format!("{}/files/{}/{file}", job(key), segment.dir_name())
}

pub fn token(key: &RecordingKey) -> String {
    format!("{}/token", job(key))
}

pub fn live(camera: &str) -> String {
    format!("/api/live/{camera}")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobRequest {
    pub recording: Recording,
    /// Where to draw the scoreboard overlay, if at all.
    pub overlay: Option<Overlay>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobState {
    pub status: JobStatus,
    /// Files of the recording the server doesn't have yet.
    pub missing: Vec<(Segment, String)>,
    pub has_token: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveConfig {
    pub calibration: MatchCalibration,
    /// RTMP URL to restream the stitched preview to.
    pub stream_url: String,
    pub overlay: Option<LiveOverlay>,
}
