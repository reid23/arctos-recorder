//! User settings, persisted as JSON in the data directory.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use arctos::Credentials;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub arctos: ArctosSettings,
    pub stitching: Stitching,
    pub cloud: CloudSettings,
    pub overlay: Overlay,
    pub panes: Panes,
    /// By camera id.
    pub cameras: BTreeMap<String, CameraSettings>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArctosSettings {
    /// The tournament's URL slug.
    pub tournament: String,
    /// A tournament organizer account, used to upload footage.
    #[serde(flatten)]
    pub credentials: Credentials,
}

/// Where recordings and live previews are stitched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stitching {
    /// On this computer's GPU.
    #[default]
    Local,
    /// On an `ssw` server.
    Cloud,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudSettings {
    /// e.g. `http://stitcher.example.org:7800`
    pub url: String,
    pub token: String,
}

/// Placement of the scoreboard overlay on the camera views.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overlay {
    pub enabled: bool,
    pub horizontal: Align,
    pub vertical: Align,
    /// Offsets from the anchor, in percent of the view's width/height,
    /// towards the center.
    pub x: f64,
    pub y: f64,
    /// In percent of the view's width.
    pub width: f64,
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay {
            enabled: true,
            horizontal: Align::Start,
            vertical: Align::Start,
            x: 2.0,
            y: 3.0,
            width: 35.0,
        }
    }
}

/// Left/top, center, or right/bottom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

impl Align {
    pub const ALL: [Align; 3] = [Align::Start, Align::Center, Align::End];
}

/// Which panes of the main window are open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Panes {
    pub cameras: bool,
    pub jobs: bool,
    pub settings: bool,
}

impl Default for Panes {
    fn default() -> Self {
        Panes {
            cameras: true,
            jobs: false,
            settings: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraSettings {
    /// Name of the Arctos field this camera records.
    pub field: Option<String>,
    /// RTMP URL to restream the stitched live preview to.
    pub stream_url: Option<String>,
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(json) => Ok(serde_json::from_slice(&json)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.cameras.values().filter_map(|c| c.field.as_deref())
    }

    pub fn stream_url(&self, camera: &str) -> Option<&str> {
        self.cameras
            .get(camera)?
            .stream_url
            .as_deref()
            .filter(|url| !url.is_empty())
    }
}
