//! HTTP API of `ssw`, the cloud stitching server. Every request carries
//! `Authorization: Bearer <token>`.
//!
//! - `PUT {job}` with a [`JobRequest`] creates the job (or retries a failed
//!   one) and returns its [`JobState`]. It starts once all of the files
//!   listed in [`JobState::missing`] have been uploaded.
//! - `PUT {job}/files/{segment}/{file}?offset=…&size=…` uploads part of one
//!   file of the recording (see [`FileChunk`]), in order and at most
//!   [`CHUNK_BYTES`] at a time. The file counts as uploaded once its last
//!   byte is; a chunk at offset 0 starts it over.
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
//!   [`StereoYuv`](crate::live::StereoYuv) frames, or text
//!   [`StitchConfig`]s changing how they are stitched.

use protocol::Segment;
use serde::{Deserialize, Serialize};

use crate::JobStatus;
use crate::encode::VideoQuality;
use crate::geometry::StitchConfig;
use crate::overlay::{LiveOverlay, Overlay};
use crate::recording::{Recording, RecordingKey};
use crate::restream::Destination;

pub const PORT: u16 = 7800;
/// Most bytes of a file one upload request carries: Cloudflare's proxy,
/// which the server may sit behind, rejects request bodies over 100 MB.
pub const CHUNK_BYTES: u64 = 64 << 20;

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
    pub quality: VideoQuality,
    /// Where to draw the scoreboard overlay, if at all.
    pub overlay: Option<Overlay>,
}

/// Where an upload request's body goes in its file, as its query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunk {
    /// Of the body's first byte in the file.
    pub offset: u64,
    /// Of the whole file, in bytes.
    pub size: u64,
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
    pub stitch: StitchConfig,
    /// Size of each camera's previews; a change means a new connection.
    pub preview_size: (u32, u32),
    /// Size of the stitched preview.
    pub size: (u32, u32),
    /// Where to restream the stitched preview.
    pub destination: Destination,
    /// Of the restream's video, in megabits a second.
    pub bitrate: f64,
    pub overlay: Option<LiveOverlay>,
}
