//! State shared between the backend services and the UI.
//!
//! Everything the UI displays lives in a `watch` channel so components can
//! subscribe to changes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arctos::{Arctos, timestamp};
use bytes::Bytes;
use chrono::Utc;
use protocol::{
    AudioDevice, AutofocusOutcome, CameraStats, Directive, Focus, Instructions, MatchManifest,
    Timestamp,
};
use stitching::JobStatus;
use stitching::browser::Chromium;
use stitching::geometry::{Calibration, StitchConfig};
use stitching::recording::{RECORDING_FILE, Recording, RecordingKey};
use tokio::sync::{broadcast, mpsc, watch};

use crate::account::{ACCOUNT_FILE, Account};
use crate::camera_port::CameraPort;
use crate::jobs::{Job, Order};
use crate::performance::{CameraUsage, FrameHistory, Performance};
use crate::settings::{RigSetup, Settings};
use crate::{live, restream};

const ONLINE_TIMEOUT: Duration = Duration::from_secs(3);
/// Chunks of audio (about 21 ms each) kept for listeners that fall behind.
const AUDIO_BACKLOG: usize = 64;

pub struct Hub {
    pub paths: Paths,
    pub arctos: Arctos,
    pub chromium: Chromium,
    pub account: Arc<Account>,
    pub settings: watch::Sender<Settings>,
    /// The fields of the configured tournament, or why they couldn't be fetched.
    pub arctos_fields: watch::Sender<Result<Vec<String>, String>>,
    /// By camera id.
    pub cameras: watch::Sender<BTreeMap<String, CameraStatus>>,
    /// By field name; only fields that a camera is assigned to.
    pub fields: watch::Sender<BTreeMap<String, FieldStatus>>,
    pub jobs: watch::Sender<Vec<Job>>,
    /// Cameras being calibrated, whose previews are stitched here even when
    /// stitching in the cloud.
    pub calibrating: watch::Sender<BTreeSet<String>>,
    pub performance: watch::Sender<Performance>,
    /// Only kept on Linux.
    pub camera_port: watch::Sender<CameraPort>,
    links: Mutex<HashMap<String, Arc<CameraLink>>>,
    last_seen: Mutex<HashMap<String, Instant>>,
    /// The frame counts each camera reported lately.
    frame_histories: Mutex<HashMap<String, FrameHistory>>,
    /// Display names of matches seen on the scoreboard, by match id.
    match_titles: Mutex<HashMap<String, String>>,
    /// Recordings arriving from the cameras.
    receiving: Mutex<HashMap<RecordingKey, Transfer>>,
    job_queue: mpsc::UnboundedSender<Order>,
}

/// How much of a recording has arrived.
struct Transfer {
    received: u64,
    total: u64,
}

impl Transfer {
    fn percent(&self) -> u64 {
        (self.received * 100).checked_div(self.total).unwrap_or(100)
    }
}

pub struct Paths {
    root: PathBuf,
}

impl Paths {
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(root.join("recordings"))?;
        Ok(Paths { root })
    }

    pub fn settings(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    pub fn account(&self) -> PathBuf {
        self.root.join(ACCOUNT_FILE)
    }

    pub fn recordings(&self) -> PathBuf {
        self.root.join("recordings")
    }

    /// Where Chromium is downloaded to when no browser is installed.
    pub fn chromium(&self) -> PathBuf {
        self.root.join("chromium")
    }

    /// Where the calibration window keeps a camera's lens pictures and last
    /// alignment between sessions.
    pub fn calibration(&self, camera: &str) -> PathBuf {
        self.root.join("calibration").join(camera)
    }

    pub fn recording(&self, key: &RecordingKey) -> PathBuf {
        self.recordings().join(&key.match_id).join(&key.camera)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CameraStatus {
    pub address: IpAddr,
    /// Whether it has polled for its instructions recently.
    pub online: bool,
    /// Whether its microphone's audio is arriving.
    pub hearing_audio: bool,
    pub audio_devices: Vec<AudioDevice>,
    /// Share, from 0 to 1, of preview frames dropped over the last
    /// [`LOSS_WINDOW`](crate::performance::LOSS_WINDOW) because stitching
    /// could not keep up. `None` when this computer is not stitching the
    /// camera's preview.
    pub previews_dropped: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldStatus {
    pub directive: Directive,
    pub match_title: Option<String>,
    /// Set while Arctos can't be reached; the last directive stays in effect.
    pub error: Option<String>,
}

/// Live media of a registered camera.
pub struct CameraLink {
    /// How the camera's footage is stitched now: the camera's calibration
    /// with the user's settings (see
    /// [`RigSetup::stitch_config`](crate::settings::RigSetup::stitch_config)).
    pub stitch: watch::Sender<Arc<StitchConfig>>,
    /// The calibration the camera sent when it registered.
    pub reported_calibration: watch::Sender<Calibration>,
    /// Latest preview of each camera, indexed by [`protocol::Side::index`].
    pub previews: [watch::Sender<Option<Bytes>>; 2],
    /// Latest stitched preview JPEG.
    pub panorama: watch::Sender<Option<Bytes>>,
    /// The microphone's audio as it arrives, as in [`protocol::AUDIO_FILE`].
    pub audio: broadcast::Sender<Bytes>,
    pub last_audio: Mutex<Option<Instant>>,
    pub autofocus: watch::Sender<AutofocusState>,
    /// Preview frames received from the left camera, which pace the stitched preview.
    pub preview_frames: AtomicU64,
}

/// Where autofocusing a camera's lenses is up to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum AutofocusState {
    #[default]
    Idle,
    /// Asked for at this time; waiting for the camera.
    Requested(Timestamp),
    /// Some lens found nothing to focus on.
    Failed(AutofocusOutcome),
}

impl AutofocusState {
    /// Takes in what the camera reports, returning where its lenses were
    /// focused if that answers the request.
    fn answer(&mut self, outcome: AutofocusOutcome) -> Option<[f32; 2]> {
        if *self != AutofocusState::Requested(outcome.requested) {
            return None;
        }
        match outcome.lens_positions {
            [Some(left), Some(right)] => {
                *self = AutofocusState::Idle;
                Some([left, right])
            }
            _ => {
                *self = AutofocusState::Failed(outcome);
                None
            }
        }
    }

    /// Where to have the camera focus, given where its lenses are held;
    /// `None` if they never were and no autofocus was asked for.
    fn focus(&self, lens_positions: Option<[f32; 2]>) -> Option<Focus> {
        match (*self, lens_positions) {
            (AutofocusState::Requested(requested), _) => Some(Focus::Auto { requested }),
            (_, Some(lens_positions)) => Some(Focus::Fixed { lens_positions }),
            (AutofocusState::Failed(outcome), None) => Some(Focus::Auto {
                requested: outcome.requested,
            }),
            (AutofocusState::Idle, None) => None,
        }
    }
}

impl Hub {
    pub fn new(
        paths: Paths,
        arctos: Arctos,
        account: Account,
        settings: Settings,
        job_queue: mpsc::UnboundedSender<Order>,
    ) -> Self {
        Hub {
            chromium: Chromium::installed_or_downloaded_into(paths.chromium()),
            paths,
            arctos,
            account: Arc::new(account),
            settings: watch::Sender::new(settings),
            arctos_fields: watch::Sender::new(Ok(Vec::new())),
            cameras: watch::Sender::default(),
            fields: watch::Sender::default(),
            jobs: watch::Sender::default(),
            calibrating: watch::Sender::default(),
            performance: watch::Sender::default(),
            camera_port: watch::Sender::default(),
            links: Mutex::default(),
            last_seen: Mutex::default(),
            frame_histories: Mutex::default(),
            match_titles: Mutex::default(),
            receiving: Mutex::default(),
            job_queue,
        }
    }

    pub fn update_settings(&self, update: impl FnOnce(&mut Settings)) {
        let changed = self.settings.send_if_modified(|settings| {
            let before = settings.clone();
            update(settings);
            *settings != before
        });
        if changed && let Err(e) = self.settings.borrow().save(&self.paths.settings()) {
            tracing::error!("saving settings: {e:#}");
        }
    }

    /// Records a (re)connecting camera, starting its live pipeline the first
    /// time it is seen. The setup the camera keeps, if any, replaces this
    /// computer's copy.
    pub fn register(
        self: &Arc<Self>,
        camera: &str,
        address: IpAddr,
        calibration: Calibration,
        setup: Option<RigSetup>,
        audio_devices: Vec<AudioDevice>,
    ) {
        if let Some(setup) = setup {
            self.update_settings(|settings| settings.cameras.entry(camera.to_owned()).or_default().setup = setup);
        }
        let mut links = self.links.lock().unwrap();
        match links.get(camera) {
            Some(link) => {
                link.reported_calibration.send_if_modified(|reported| {
                    let changed = *reported != calibration;
                    *reported = calibration;
                    changed
                });
            }
            None => {
                let stitch = self.stitch_config(camera, &calibration);
                let link = Arc::new(CameraLink {
                    stitch: watch::Sender::new(Arc::new(stitch)),
                    reported_calibration: watch::Sender::new(calibration),
                    previews: Default::default(),
                    panorama: watch::Sender::new(None),
                    audio: broadcast::channel(AUDIO_BACKLOG).0,
                    last_audio: Mutex::new(None),
                    autofocus: watch::Sender::default(),
                    preview_frames: AtomicU64::new(0),
                });
                live::spawn(self.clone(), camera.to_owned(), link.clone());
                tokio::spawn(restream::run(self.clone(), camera.to_owned(), link.clone()));
                tokio::spawn(follow_stitch_config(
                    self.clone(),
                    camera.to_owned(),
                    link.clone(),
                ));
                links.insert(camera.to_owned(), link);
            }
        }
        drop(links);
        self.last_seen
            .lock()
            .unwrap()
            .insert(camera.to_owned(), Instant::now());
        self.cameras.send_modify(|cameras| {
            let hearing_audio = cameras.get(camera).is_some_and(|c| c.hearing_audio);
            cameras.insert(
                camera.to_owned(),
                CameraStatus {
                    address,
                    online: true,
                    hearing_audio,
                    audio_devices,
                    previews_dropped: None,
                },
            );
        });
        tracing::info!("camera {camera} registered from {address}");
    }

    /// How to stitch `camera`'s footage given the calibration it sends.
    fn stitch_config(&self, camera: &str, reported: &Calibration) -> StitchConfig {
        self.settings
            .borrow()
            .cameras
            .get(camera)
            .map(|camera| camera.setup.clone())
            .unwrap_or_default()
            .stitch_config(reported)
    }

    /// Keeps [`CameraStatus::online`] up to date.
    pub async fn track_presence(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            ticker.tick().await;
            let last_seen = self.last_seen.lock().unwrap().clone();
            let links = self.links.lock().unwrap().clone();
            self.cameras.send_if_modified(|cameras| {
                let mut changed = false;
                for (camera, status) in cameras.iter_mut() {
                    let online = last_seen[camera].elapsed() < ONLINE_TIMEOUT;
                    let hearing_audio = links.get(camera).is_some_and(|link| {
                        link.last_audio
                            .lock()
                            .unwrap()
                            .is_some_and(|last| last.elapsed() < ONLINE_TIMEOUT)
                    });
                    changed |= status.online != online || status.hearing_audio != hearing_audio;
                    status.online = online;
                    status.hearing_audio = hearing_audio;
                }
                changed
            });
        }
    }

    pub fn link(&self, camera: &str) -> Option<Arc<CameraLink>> {
        self.links.lock().unwrap().get(camera).cloned()
    }

    /// Has `camera` autofocus its lenses, once it isn't recording.
    pub fn autofocus(&self, camera: &str) {
        if let Some(link) = self.link(camera) {
            link.autofocus.send_replace(AutofocusState::Requested(timestamp(Utc::now())));
        }
    }

    /// Takes in how `camera` is doing and says what it should be doing;
    /// `None` if it hasn't registered.
    pub fn instructions(&self, camera: &str, stats: &CameraStats) -> Result<Option<Instructions>> {
        let now = Instant::now();
        let Some(link) = self.link(camera) else {
            return Ok(None);
        };
        self.last_seen.lock().unwrap().insert(camera.to_owned(), now);
        if let Some(outcome) = stats.autofocus {
            let mut focused = None;
            link.autofocus.send_if_modified(|state| {
                let before = *state;
                focused = state.answer(outcome);
                *state != before
            });
            if let Some(lens_positions) = focused {
                self.update_settings(|settings| {
                    settings.cameras.entry(camera.to_owned()).or_default().setup.lens_positions = Some(lens_positions);
                });
            }
        }
        let settings = self.settings.borrow();
        let lens_positions = settings.cameras.get(camera).and_then(|c| c.setup.lens_positions);
        let focus = link.autofocus.borrow().focus(lens_positions);
        let focus = focus.unwrap_or_else(|| {
            let requested = timestamp(Utc::now());
            link.autofocus.send_replace(AutofocusState::Requested(requested));
            Focus::Auto { requested }
        });
        let mut losses = self
            .frame_histories
            .lock()
            .unwrap()
            .entry(camera.to_owned())
            .or_default()
            .add(now, stats.frames);
        if settings.swaps_sides(camera) {
            losses.reverse();
        }
        self.performance.send_modify(|performance| {
            performance
                .cameras
                .insert(camera.to_owned(), CameraUsage::new(stats, losses));
        });
        let field = settings.cameras.get(camera).and_then(|c| c.field.as_ref());
        let setup = settings.cameras.get(camera).map(|c| c.setup.clone()).unwrap_or_default();
        let fields = self.fields.borrow();
        Ok(Some(Instructions {
            directive: field
                .and_then(|field| fields.get(field))
                .map_or(Directive::Idle, |status| status.directive.clone()),
            audio_device: settings.audio_device(camera).to_owned(),
            record_quality: settings.recording.camera_quality,
            preview_size: settings.live.camera_size,
            preview_quality: settings.live.camera_quality,
            focus,
            margins: settings.recording.margins,
            buffer: settings.recording.buffer,
            setup: serde_json::to_value(setup)?,
        }))
    }

    pub fn remember_match_title(&self, match_id: &str, title: String) {
        self.match_titles
            .lock()
            .unwrap()
            .insert(match_id.to_owned(), title);
    }

    /// A match's display name; its id if it wasn't seen on the scoreboard.
    fn match_title(&self, match_id: &str) -> String {
        self.match_titles
            .lock()
            .unwrap()
            .get(match_id)
            .cloned()
            .unwrap_or_else(|| match_id.to_owned())
    }

    /// Shows a recording a camera is about to send, `bytes` long, as a job.
    pub fn start_receiving(&self, key: RecordingKey, bytes: u64) {
        if self.paths.recording(&key).join(RECORDING_FILE).exists() {
            // The camera didn't hear our acknowledgement and sends it again.
            return;
        }
        let title = self.match_title(&key.match_id);
        self.receiving.lock().unwrap().insert(
            key.clone(),
            Transfer {
                received: 0,
                total: bytes,
            },
        );
        self.show_job(key, title, JobStatus::Receiving(0.0));
    }

    /// Counts `bytes` more of a recording as arrived.
    pub fn received(&self, key: &RecordingKey, bytes: u64) {
        let percent = {
            let mut receiving = self.receiving.lock().unwrap();
            let Some(transfer) = receiving.get_mut(key) else {
                return;
            };
            let before = transfer.percent();
            transfer.received += bytes;
            let percent = transfer.percent();
            // Every chunk would be too many updates for the UI.
            (percent != before).then_some(percent)
        };
        if let Some(percent) = percent {
            self.set_job_status(key, JobStatus::Receiving(percent as f32 / 100.0));
        }
    }

    /// Stores the description of a fully uploaded recording and queues it for
    /// processing.
    pub fn submit_recording(&self, camera: &str, manifest: MatchManifest) -> Result<()> {
        let key = RecordingKey {
            match_id: manifest.match_id.clone(),
            camera: camera.to_owned(),
        };
        let dir = self.paths.recording(&key);
        if dir.join(RECORDING_FILE).exists() {
            // The camera didn't hear our acknowledgement and sent it again.
            return Ok(());
        }
        let link = self.link(camera).context("camera is not registered")?;
        let recording = Recording {
            tournament: self.settings.borrow().arctos.tournament.clone(),
            title: self.match_title(&key.match_id),
            stitch: (**link.stitch.borrow()).clone(),
            manifest,
        };
        recording.save(&dir)?;
        self.receiving.lock().unwrap().remove(&key);
        self.enqueue(key, recording.title, false);
        Ok(())
    }

    /// Adds a job (or resets an existing one) and queues it. `release` lets
    /// its next step go ahead even if the settings say it should wait.
    pub fn enqueue(&self, key: RecordingKey, title: String, release: bool) {
        self.show_job(key.clone(), title, JobStatus::Queued);
        self.job_queue
            .send(Order { key, release })
            .expect("job worker outlives the hub");
    }

    /// Adds a job, or sets an existing one's status.
    fn show_job(&self, key: RecordingKey, title: String, status: JobStatus) {
        self.jobs
            .send_modify(|jobs| match jobs.iter_mut().find(|job| job.key == key) {
                Some(job) => job.status = status,
                None => jobs.push(Job { key, title, status }),
            });
    }

    pub fn set_job_status(&self, key: &RecordingKey, status: JobStatus) {
        self.jobs.send_modify(|jobs| {
            if let Some(job) = jobs.iter_mut().find(|job| &job.key == key) {
                job.status = status;
            }
        });
    }
}

/// Keeps [`CameraLink::stitch`] up to date with the settings and the
/// calibration the camera sends.
async fn follow_stitch_config(hub: Arc<Hub>, camera: String, link: Arc<CameraLink>) {
    let mut settings = hub.settings.subscribe();
    let mut reported = link.reported_calibration.subscribe();
    loop {
        let config = hub.stitch_config(&camera, &reported.borrow_and_update());
        link.stitch.send_if_modified(|current| {
            let changed = **current != config;
            if changed {
                *current = Arc::new(config);
            }
            changed
        });
        let changed = tokio::select! {
            changed = settings.changed() => changed,
            changed = reported.changed() => changed,
        };
        if changed.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use stitching::geometry::Orientation;

    use super::*;

    #[tokio::test]
    async fn the_setup_a_camera_keeps_replaces_this_computers_copy() {
        let root = std::env::temp_dir().join(format!("bsw-hub-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::new(root).unwrap();
        let arctos = Arctos::from_env().unwrap();
        let account = Account::load(arctos.clone(), paths.account()).unwrap();
        let ours = RigSetup {
            swap_sides: true,
            ..RigSetup::default()
        };
        let mut settings = Settings::default();
        settings.cameras.entry("ar-1".to_owned()).or_default().setup = ours.clone();
        let (job_queue, _jobs) = mpsc::unbounded_channel();
        let hub = Arc::new(Hub::new(paths, arctos, account, settings, job_queue));
        let address = IpAddr::from([10, 42, 0, 2]);
        let calibration = Calibration::nominal([Orientation::default(); 2]);
        let stats = CameraStats {
            cpu: None,
            memory: None,
            temperature: None,
            frames: Default::default(),
            autofocus: None,
        };
        let given = || {
            let instructions = hub.instructions("ar-1", &stats).unwrap().unwrap();
            serde_json::from_value::<RigSetup>(instructions.setup).unwrap()
        };

        hub.register("ar-1", address, calibration.clone(), None, Vec::new());
        assert_eq!(given(), ours, "a camera that keeps no setup is given this computer's");

        let theirs = RigSetup {
            audio_device: Some("plughw:CARD=USB,DEV=0".to_owned()),
            ..RigSetup::default()
        };
        hub.register("ar-1", address, calibration, Some(theirs.clone()), Vec::new());
        assert_eq!(hub.settings.borrow().cameras["ar-1"].setup, theirs);
        assert_eq!(given(), theirs);
    }

    #[test]
    fn autofocus_takes_only_the_answer_to_its_request() {
        let asked = Timestamp(1_000);
        let mut state = AutofocusState::Requested(asked);
        assert_eq!(state.focus(Some([1.0, 1.0])), Some(Focus::Auto { requested: asked }));
        let stale = AutofocusOutcome {
            requested: Timestamp(500),
            lens_positions: [Some(3.0), Some(3.0)],
        };
        assert_eq!(state.answer(stale), None);
        assert_eq!(state, AutofocusState::Requested(asked));

        let found = AutofocusOutcome {
            requested: asked,
            lens_positions: [Some(9.5), Some(9.75)],
        };
        assert_eq!(state.answer(found), Some([9.5, 9.75]));
        assert_eq!(state, AutofocusState::Idle);
        assert_eq!(state.answer(found), None, "it was answered already");
        assert_eq!(
            state.focus(Some([9.5, 9.75])),
            Some(Focus::Fixed { lens_positions: [9.5, 9.75] })
        );
        assert_eq!(state.focus(None), None);
    }

    #[test]
    fn lenses_that_fail_to_autofocus_go_back_or_wait_to_be_asked_again() {
        let asked = Timestamp(1_000);
        let failed = AutofocusOutcome {
            requested: asked,
            lens_positions: [Some(9.5), None],
        };
        let mut state = AutofocusState::Requested(asked);
        assert_eq!(state.answer(failed), None);
        assert_eq!(state, AutofocusState::Failed(failed));
        assert_eq!(
            state.focus(Some([1.0, 2.0])),
            Some(Focus::Fixed { lens_positions: [1.0, 2.0] })
        );
        assert_eq!(state.focus(None), Some(Focus::Auto { requested: asked }));
    }

    #[test]
    fn transfers_count_whole_percents() {
        let percent = |received, total| Transfer { received, total }.percent();
        assert_eq!(percent(0, 0), 100, "nothing to receive");
        assert_eq!(percent(0, 3), 0);
        assert_eq!(percent(2, 3), 66);
        assert_eq!(percent(3, 3), 100);
    }
}
