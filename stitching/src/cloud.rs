//! HTTP API of `ssw`, the cloud stitching server. Every request carries
//! `Authorization: Bearer <token>`.
//!
//! - `PUT {job}` with a [`JobRequest`] creates the job (or retries a failed
//!   one) and returns its [`JobState`]. It starts once all of the files
//!   listed in [`JobState::missing`] have been uploaded.
//! - `PUT {job}/files/{segment}/{file}` uploads one file of the recording.
//! - `GET {job}` returns the [`JobState`]. Once it is
//!   [`JobStatus::Done`](crate::JobStatus::Done) the video is on Arctos and the
//!   server has deleted the recording.
//! - `GET {live}` is a WebSocket for forwarding a camera's live previews to
//!   be stitched and restreamed. The first message is a text [`LiveConfig`];
//!   the rest are binary chunks of an MPEG-TS stream of H.264-encoded
//!   [`StereoYuv`](crate::live::StereoYuv) frames.

use arctos::Credentials;
use protocol::Segment;
use reco_core::calibration::MatchCalibration;
use serde::{Deserialize, Serialize};

use crate::JobStatus;
use crate::recording::{Recording, RecordingKey};

pub const PORT: u16 = 7800;

pub fn job(key: &RecordingKey) -> String {
    format!("/api/jobs/{}/{}", key.match_id, key.camera)
}

pub fn file(key: &RecordingKey, segment: &Segment, file: &str) -> String {
    format!("{}/files/{}/{file}", job(key), segment.dir_name())
}

pub fn live(camera: &str) -> String {
    format!("/api/live/{camera}")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobRequest {
    pub recording: Recording,
    /// For uploading the video to Arctos.
    pub credentials: Credentials,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobState {
    pub status: JobStatus,
    /// Files of the recording the server doesn't have yet.
    pub missing: Vec<(Segment, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveConfig {
    pub calibration: MatchCalibration,
    /// RTMP URL to restream the stitched preview to.
    pub stream_url: String,
}
