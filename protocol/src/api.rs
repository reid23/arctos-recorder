//! Base station HTTP API paths used by cameras.
//!
//! | Method | Path | Body | Response |
//! |---|---|---|---|
//! | GET  | [`TIME`] | | [`Timestamp`](crate::Timestamp) |
//! | PUT  | [`camera`] | [`Registration`](crate::Registration) | 204 |
//! | POST | [`instructions`] | [`CameraStats`](crate::CameraStats) | [`Instructions`](crate::Instructions) |
//! | GET  | [`preview`] | WebSocket of [`PreviewMessage`](crate::PreviewMessage)s | |
//! | PUT  | [`recording_file`] | raw file contents | 204 |
//! | PUT  | [`recording`] | [`MatchManifest`](crate::MatchManifest) | 204 once stored |
//!
//! Cameras find the base station by broadcasting [`DISCOVERY_BEACON`] to UDP
//! port [`DISCOVERY_PORT`]; the base station answers with
//! [`DISCOVERY_REPLY`] from the address it serves the API on.

use crate::Segment;

pub const PORT: u16 = 7700;

pub const DISCOVERY_PORT: u16 = 7701;
pub const DISCOVERY_BEACON: &[u8] = b"arctos-recorder camera";
pub const DISCOVERY_REPLY: &[u8] = b"arctos-recorder base station";

/// Whether `id` is acceptable as a camera or match id, which become
/// directory names.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub const TIME: &str = "/api/time";

pub fn camera(camera: &str) -> String {
    format!("/api/cameras/{camera}")
}

pub fn instructions(camera: &str) -> String {
    format!("/api/cameras/{camera}/instructions")
}

pub fn preview(camera: &str) -> String {
    format!("/api/cameras/{camera}/preview")
}

pub fn recording(camera: &str, match_id: &str) -> String {
    format!("/api/cameras/{camera}/recordings/{match_id}")
}

pub fn recording_file(camera: &str, match_id: &str, segment: &Segment, file: &str) -> String {
    format!(
        "/api/cameras/{camera}/recordings/{match_id}/{}/{file}",
        segment.dir_name()
    )
}
