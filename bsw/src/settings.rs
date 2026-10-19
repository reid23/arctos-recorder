//! User settings, persisted as JSON in the data directory.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use stitching::overlay::LiveOverlay;
pub use stitching::overlay::{Align, Overlay};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub arctos: ArctosSettings,
    pub stitching: Stitching,
    pub cloud: CloudSettings,
    pub workflow: Workflow,
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

/// When recordings are processed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workflow {
    /// Restream the cameras' live previews to their stream URLs.
    pub live_stream: bool,
    /// Keep recordings to be stitched when the user says so.
    pub stitch_later: bool,
    /// Keep stitched videos to be uploaded when the user says so.
    pub upload_later: bool,
}

impl Default for Workflow {
    fn default() -> Self {
        Workflow {
            live_stream: true,
            stitch_later: false,
            upload_later: false,
        }
    }
}

/// Which panes of the main window are open, and their sizes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Panes {
    pub cameras: bool,
    pub jobs: bool,
    pub settings: bool,
    /// Width in pixels of the column holding Settings and Jobs, next to
    /// Cameras.
    pub side_width: f64,
    /// Settings' share of that column's height, when Jobs is open too.
    pub settings_share: f64,
    /// Width in pixels of each camera's box in Cameras.
    pub camera_width: f64,
}

impl Default for Panes {
    fn default() -> Self {
        Panes {
            cameras: true,
            jobs: false,
            settings: false,
            side_width: 380.0,
            settings_share: 0.5,
            camera_width: 480.0,
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
    /// Whether the camera's left and right feeds are the other way round.
    pub swap_sides: bool,
    /// ALSA PCM name of the microphone; `None` for the ALSA default.
    pub audio_device: Option<String>,
    /// A `reco` `MatchCalibration` found with the Calibrate button, used
    /// instead of the one the camera sends.
    pub calibration: Option<serde_json::Value>,
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

    /// Where to restream a camera's live preview, if anywhere.
    pub fn stream_url(&self, camera: &str) -> Option<&str> {
        if !self.workflow.live_stream {
            return None;
        }
        self.cameras
            .get(camera)?
            .stream_url
            .as_deref()
            .filter(|url| !url.is_empty())
    }

    pub fn swaps_sides(&self, camera: &str) -> bool {
        self.cameras.get(camera).is_some_and(|c| c.swap_sides)
    }

    /// ALSA PCM name of the microphone `camera` should capture.
    pub fn audio_device(&self, camera: &str) -> &str {
        self.cameras
            .get(camera)
            .and_then(|c| c.audio_device.as_deref())
            .unwrap_or(protocol::DEFAULT_AUDIO_DEVICE)
    }

    /// The scoreboard to show over a camera's live preview, if any.
    pub fn live_overlay(&self, camera: &str) -> Option<LiveOverlay> {
        Some(LiveOverlay {
            tournament: self.arctos.tournament.clone(),
            field: self.cameras.get(camera)?.field.clone()?,
            placement: self.overlay.clone(),
        })
        .filter(|_| self.overlay.enabled)
    }

    /// Whether recordings wait to be stitched. Sending a recording to the
    /// cloud server gets it uploaded too, so that waits along with
    /// uploading.
    pub fn stitches_later(&self) -> bool {
        self.workflow.stitch_later
            || (self.stitching == Stitching::Cloud && self.workflow.upload_later)
    }
}
