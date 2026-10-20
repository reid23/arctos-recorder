//! User settings, persisted as JSON in the data directory.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use protocol::{
    DEFAULT_BUFFER, DEFAULT_MARGINS, DEFAULT_PREVIEW_QUALITY, DEFAULT_RECORD_QUALITY, Margins, rig,
};
use serde::{Deserialize, Serialize};
use stitching::encode::VideoQuality;
use stitching::geometry::{AspectRatio, Blend, Calibration, Orientation, StitchConfig, View};
use stitching::overlay::LiveOverlay;
use stitching::restream::Destination;
pub use stitching::overlay::{Align, Overlay};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub arctos: ArctosSettings,
    pub stitching: Stitching,
    pub cloud: CloudSettings,
    pub workflow: Workflow,
    pub live: LiveSettings,
    pub recording: RecordingSettings,
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

/// How every camera's live preview is stitched and restreamed.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveSettings {
    /// Of the stitched previews, in pixels; their height follows from each
    /// camera's shape.
    pub width: u32,
    /// Of the restreams' video, in megabits a second.
    pub bitrate: f64,
    /// Of the previews the cameras send, one of [`rig::PREVIEW_SIZES`].
    pub camera_size: (u32, u32),
    /// JPEG quality (1 to 100) of the previews the cameras send.
    pub camera_quality: u8,
}

impl LiveSettings {
    pub const WIDTHS: RangeInclusive<u32> = 320..=3840;
    pub const BITRATES: RangeInclusive<f64> = 0.5..=50.0;
    pub const CAMERA_QUALITIES: RangeInclusive<u8> = 20..=100;
}

impl Default for LiveSettings {
    fn default() -> Self {
        LiveSettings {
            width: 1280,
            bitrate: 4.0,
            camera_size: rig::DEFAULT_PREVIEW_SIZE,
            camera_quality: DEFAULT_PREVIEW_QUALITY,
        }
    }
}

/// What the cameras record of each match, and how finely recordings are
/// kept: by the cameras, and once stitched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    /// JPEG quality (1 to 100) of the frames the cameras record.
    pub camera_quality: u8,
    pub video_quality: VideoQuality,
    pub margins: Margins,
    /// How long the cameras hold frames (see [`protocol::Instructions::buffer`]).
    pub buffer: Duration,
}

impl RecordingSettings {
    pub const CAMERA_QUALITIES: RangeInclusive<u8> = 50..=100;
    /// Of each margin, in seconds.
    pub const MARGINS: RangeInclusive<f64> = 0.0..=20.0;
    /// In seconds. The buffer takes about 16 MB of a camera's memory a
    /// second at the default JPEG quality, and the cameras have 2 GB.
    pub const BUFFERS: RangeInclusive<f64> = 1.0..=30.0;
}

impl Default for RecordingSettings {
    fn default() -> Self {
        RecordingSettings {
            camera_quality: DEFAULT_RECORD_QUALITY,
            video_quality: VideoQuality::default(),
            margins: DEFAULT_MARGINS,
            buffer: DEFAULT_BUFFER,
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
    pub performance: bool,
    /// Width in pixels of the column holding Settings and Jobs, next to
    /// Cameras.
    pub side_width: f64,
    /// Settings' share of that column's height, when Jobs is open too.
    pub settings_share: f64,
    /// Width in pixels of each camera's box in Cameras.
    pub camera_width: f64,
    /// Width in pixels of Performance, on the left.
    pub performance_width: f64,
}

impl Default for Panes {
    fn default() -> Self {
        Panes {
            cameras: true,
            jobs: false,
            settings: false,
            performance: false,
            side_width: 380.0,
            settings_share: 0.5,
            camera_width: 480.0,
            performance_width: 260.0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraSettings {
    /// Name of the Arctos field this camera records.
    pub field: Option<String>,
    /// URL of the RTMP server to restream the stitched live preview to.
    pub stream_url: Option<String>,
    /// The key the RTMP server gave for the stream, which ends its URL.
    pub stream_key: Option<String>,
    /// Whether the camera's left and right feeds are the other way round.
    pub swap_sides: bool,
    /// ALSA PCM name of the microphone; `None` for the ALSA default.
    pub audio_device: Option<String>,
    /// How the cameras are meant to be mounted (indexed by
    /// [`protocol::Side::index`]), which aligning them starts from; `None`
    /// for the orientations of the calibration the camera sends.
    pub mount: Option<[Orientation; 2]>,
    /// Found by calibrating, used instead of the one the camera sends.
    pub calibration: Option<Calibration>,
    /// `None` to fit the view to the calibration.
    pub view: Option<View>,
    pub blend: Blend,
    pub aspect_ratio: AspectRatio,
    /// Where autofocusing put the camera's lenses, which they are held at
    /// (see [`protocol::Focus::Fixed`]). Indexed by the camera's own
    /// [`protocol::Side::index`], however the sides are shown.
    pub lens_positions: Option<[f32; 2]>,
}

impl CameraSettings {
    /// `values` indexed by the camera's own sides, indexed instead as the
    /// sides are shown.
    pub fn as_shown<T>(&self, mut values: [T; 2]) -> [T; 2] {
        if self.swap_sides {
            values.reverse();
        }
        values
    }

    /// How to stitch the camera's footage, given the calibration it sends.
    pub fn stitch_config(&self, reported: &Calibration) -> StitchConfig {
        let mut calibration = self.calibration.clone().unwrap_or_else(|| reported.clone());
        if let Some(lens_positions) = self.lens_positions {
            for (camera, position) in calibration.cameras.iter_mut().zip(self.as_shown(lens_positions)) {
                camera.lens = camera.lens.refocused(f64::from(position));
            }
        }
        StitchConfig {
            view: self.view.unwrap_or_else(|| View::fitting(&calibration)),
            blend: self.blend,
            aspect_ratio: self.aspect_ratio,
            calibration,
        }
    }
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
    pub fn stream_destination(&self, camera: &str) -> Option<Destination> {
        if !self.workflow.live_stream {
            return None;
        }
        let camera = self.cameras.get(camera)?;
        Some(Destination {
            server: camera.stream_url.clone().filter(|url| !url.is_empty())?,
            key: camera.stream_key.clone().unwrap_or_default(),
        })
    }

    /// Size of a camera's stitched live preview.
    pub fn live_size(&self, camera: &str) -> (u32, u32) {
        let aspect_ratio = self.cameras.get(camera).map(|c| c.aspect_ratio).unwrap_or_default();
        aspect_ratio.size_with_width(self.live.width as f64)
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

#[cfg(test)]
mod tests {
    use stitching::geometry::{DEFAULT_FOCUS, Lens, magnification};

    use super::*;

    #[test]
    fn stitching_follows_where_the_lenses_are_held() {
        let reported = Calibration::nominal([Orientation::default(); 2]);
        let mut settings = CameraSettings::default();
        assert_eq!(settings.stitch_config(&reported).calibration, reported);

        settings.lens_positions = Some([9.5, 4.0]);
        settings.swap_sides = true;
        let [left, right] = settings.stitch_config(&reported).calibration.cameras.map(|camera| camera.lens);
        let nominal = Lens::nominal();
        assert_eq!((left.focus, right.focus), (4.0, 9.5));
        assert!((left.fx - nominal.fx * magnification(DEFAULT_FOCUS, 4.0)).abs() < 1e-9);
        assert!((right.fy - nominal.fy * magnification(DEFAULT_FOCUS, 9.5)).abs() < 1e-9);
    }
}
