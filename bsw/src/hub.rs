//! State shared between the backend services and the UI.
//!
//! Everything the UI displays lives in a `watch` channel so components can
//! subscribe to changes.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arctos::Arctos;
use bytes::Bytes;
use protocol::{AudioDevice, Directive, Instructions, MatchManifest, Side};
use stitching::recording::{RECORDING_FILE, Recording, RecordingKey};
use stitching::{JobStatus, MatchCalibration};
use tokio::sync::{broadcast, mpsc, watch};

use crate::account::{ACCOUNT_FILE, Account};
use crate::jobs::{Job, Order};
use crate::settings::Settings;
use crate::{live, restream};

const ONLINE_TIMEOUT: Duration = Duration::from_secs(3);
/// Chunks of audio (about 21 ms each) kept for listeners that fall behind.
const AUDIO_BACKLOG: usize = 64;
/// Time between the preview pairs calibration uses, so they show the scene
/// moving.
const CALIBRATION_INTERVAL: Duration = Duration::from_millis(250);
const CALIBRATION_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Hub {
    pub paths: Paths,
    pub arctos: Arctos,
    pub account: Arc<Account>,
    pub settings: watch::Sender<Settings>,
    /// The fields of the configured tournament, or why they couldn't be fetched.
    pub arctos_fields: watch::Sender<Result<Vec<String>, String>>,
    /// By camera id.
    pub cameras: watch::Sender<BTreeMap<String, CameraStatus>>,
    /// By field name; only fields that a camera is assigned to.
    pub fields: watch::Sender<BTreeMap<String, FieldStatus>>,
    pub jobs: watch::Sender<Vec<Job>>,
    /// By camera id; cameras calibrated since the app started.
    pub calibrations: watch::Sender<BTreeMap<String, Calibration>>,
    links: Mutex<HashMap<String, Arc<CameraLink>>>,
    last_seen: Mutex<HashMap<String, Instant>>,
    /// Display names of matches seen on the scoreboard, by match id.
    match_titles: Mutex<HashMap<String, String>>,
    job_queue: mpsc::UnboundedSender<Order>,
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
}

/// Progress of calibrating a camera with the Calibrate button.
#[derive(Clone, Debug, PartialEq)]
pub enum Calibration {
    Running,
    /// reco's confidence in the result, in `[0, 1]`.
    Done(f64),
    Failed(String),
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
    /// The calibration in use: the user's from the settings, or else the
    /// camera's.
    pub calibration: watch::Sender<Arc<MatchCalibration>>,
    /// The calibration the camera sent when it registered.
    reported_calibration: Mutex<Arc<MatchCalibration>>,
    /// Latest preview of each camera, indexed by [`protocol::Side::index`].
    pub previews: [watch::Sender<Option<Bytes>>; 2],
    /// Latest stitched preview JPEG.
    pub panorama: watch::Sender<Option<Bytes>>,
    /// The microphone's audio as it arrives, as in [`protocol::AUDIO_FILE`].
    pub audio: broadcast::Sender<Bytes>,
    pub last_audio: Mutex<Option<Instant>>,
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
            paths,
            arctos,
            account: Arc::new(account),
            settings: watch::Sender::new(settings),
            arctos_fields: watch::Sender::new(Ok(Vec::new())),
            cameras: watch::Sender::default(),
            fields: watch::Sender::default(),
            jobs: watch::Sender::default(),
            calibrations: watch::Sender::default(),
            links: Mutex::default(),
            last_seen: Mutex::default(),
            match_titles: Mutex::default(),
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
    /// time it is seen.
    pub fn register(
        self: &Arc<Self>,
        camera: &str,
        address: IpAddr,
        calibration: MatchCalibration,
        audio_devices: Vec<AudioDevice>,
    ) {
        let reported = Arc::new(calibration);
        let calibration = self
            .chosen_calibration(camera)
            .unwrap_or_else(|| reported.clone());
        let mut links = self.links.lock().unwrap();
        match links.get(camera) {
            Some(link) => {
                *link.reported_calibration.lock().unwrap() = reported;
                link.calibration.send_replace(calibration);
            }
            None => {
                let link = Arc::new(CameraLink {
                    calibration: watch::Sender::new(calibration),
                    reported_calibration: Mutex::new(reported),
                    previews: Default::default(),
                    panorama: watch::Sender::new(None),
                    audio: broadcast::channel(AUDIO_BACKLOG).0,
                    last_audio: Mutex::new(None),
                });
                live::spawn(self.clone(), camera.to_owned(), link.clone());
                tokio::spawn(restream::run(self.clone(), camera.to_owned(), link.clone()));
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
                },
            );
        });
        tracing::info!("camera {camera} registered from {address}");
    }

    /// The calibration the user chose for `camera`, if any.
    fn chosen_calibration(&self, camera: &str) -> Option<Arc<MatchCalibration>> {
        let settings = self.settings.borrow();
        let chosen = settings.cameras.get(camera)?.calibration.clone()?;
        match serde_json::from_value(chosen) {
            Ok(calibration) => Some(Arc::new(calibration)),
            Err(e) => {
                tracing::error!("ignoring the saved calibration of {camera}: {e}");
                None
            }
        }
    }

    /// Calibrates `camera` from its previews, using the result from now on.
    pub fn calibrate(self: &Arc<Self>, camera: &str) {
        let Some(link) = self.link(camera) else {
            return;
        };
        let set_status = {
            let (hub, camera) = (self.clone(), camera.to_owned());
            move |status| {
                hub.calibrations.send_modify(|calibrations| {
                    calibrations.insert(camera.clone(), status);
                })
            }
        };
        set_status(Calibration::Running);
        let (hub, camera) = (self.clone(), camera.to_owned());
        tokio::spawn(async move {
            let result = async {
                let previews = tokio::time::timeout(CALIBRATION_TIMEOUT, collect_previews(&link))
                    .await
                    .context("the camera isn't sending previews")?;
                let current = link.calibration.borrow().clone();
                tokio::task::spawn_blocking(move || {
                    stitching::calibrate::calibrate(&current, &previews)
                })
                .await?
            }
            .await;
            match result {
                Ok(calibrated) => {
                    let saved = serde_json::to_value(&calibrated.calibration)
                        .expect("calibrations serialize");
                    hub.update_settings(|s| {
                        s.cameras.entry(camera.clone()).or_default().calibration = Some(saved)
                    });
                    link.calibration
                        .send_replace(Arc::new(calibrated.calibration));
                    set_status(Calibration::Done(calibrated.confidence));
                }
                Err(e) => {
                    tracing::warn!("calibrating {camera}: {e:#}");
                    set_status(Calibration::Failed(format!("{e:#}")));
                }
            }
        });
    }

    /// Goes back to the calibration `camera` sends.
    pub fn use_camera_calibration(&self, camera: &str) {
        self.update_settings(|s| {
            if let Some(settings) = s.cameras.get_mut(camera) {
                settings.calibration = None;
            }
        });
        if let Some(link) = self.link(camera) {
            let reported = link.reported_calibration.lock().unwrap().clone();
            link.calibration.send_replace(reported);
        }
        self.calibrations.send_modify(|calibrations| {
            calibrations.remove(camera);
        });
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

    /// What `camera` should be doing; `None` if it hasn't registered.
    pub fn instructions(&self, camera: &str) -> Option<Instructions> {
        *self.last_seen.lock().unwrap().get_mut(camera)? = Instant::now();
        let settings = self.settings.borrow();
        let field = settings.cameras.get(camera).and_then(|c| c.field.as_ref());
        let fields = self.fields.borrow();
        Some(Instructions {
            directive: field
                .and_then(|field| fields.get(field))
                .map_or(Directive::Idle, |status| status.directive.clone()),
            audio_device: settings.audio_device(camera).to_owned(),
        })
    }

    pub fn remember_match_title(&self, match_id: &str, title: String) {
        self.match_titles
            .lock()
            .unwrap()
            .insert(match_id.to_owned(), title);
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
        let title = self
            .match_titles
            .lock()
            .unwrap()
            .get(&key.match_id)
            .cloned()
            .unwrap_or_else(|| key.match_id.clone());
        let recording = Recording {
            tournament: self.settings.borrow().arctos.tournament.clone(),
            title,
            calibration: (**link.calibration.borrow()).clone(),
            manifest,
        };
        recording.save(&dir)?;
        self.enqueue(key, recording.title, false);
        Ok(())
    }

    /// Adds a job (or resets an existing one) and queues it. `release` lets
    /// its next step go ahead even if the settings say it should wait.
    pub fn enqueue(&self, key: RecordingKey, title: String, release: bool) {
        self.jobs
            .send_modify(|jobs| match jobs.iter_mut().find(|job| job.key == key) {
                Some(job) => job.status = JobStatus::Queued,
                None => jobs.push(Job {
                    key: key.clone(),
                    title,
                    status: JobStatus::Queued,
                }),
            });
        self.job_queue
            .send(Order { key, release })
            .expect("job worker outlives the hub");
    }

    pub fn set_job_status(&self, key: &RecordingKey, status: JobStatus) {
        self.jobs.send_modify(|jobs| {
            if let Some(job) = jobs.iter_mut().find(|job| &job.key == key) {
                job.status = status;
            }
        });
    }
}

/// [`stitching::calibrate::PAIRS`] pairs of the camera's latest previews,
/// [`CALIBRATION_INTERVAL`] apart.
async fn collect_previews(link: &CameraLink) -> Vec<[Bytes; 2]> {
    let mut ticker = tokio::time::interval(CALIBRATION_INTERVAL);
    let mut pairs = Vec::with_capacity(stitching::calibrate::PAIRS);
    while pairs.len() < stitching::calibrate::PAIRS {
        ticker.tick().await;
        let [left, right] = Side::BOTH.map(|side| link.previews[side.index()].borrow().clone());
        if let (Some(left), Some(right)) = (left, right) {
            pairs.push([left, right]);
        }
    }
    pairs
}
