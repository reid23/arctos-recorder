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
use protocol::{Directive, MatchManifest};
use stitching::recording::{RECORDING_FILE, Recording, RecordingKey};
use stitching::{JobStatus, MatchCalibration};
use tokio::sync::{mpsc, watch};

use crate::jobs::Job;
use crate::settings::Settings;
use crate::{live, restream};

const ONLINE_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Hub {
    pub paths: Paths,
    pub arctos: Arctos,
    pub settings: watch::Sender<Settings>,
    /// The fields of the configured tournament, or why they couldn't be fetched.
    pub arctos_fields: watch::Sender<Result<Vec<String>, String>>,
    /// By camera id.
    pub cameras: watch::Sender<BTreeMap<String, CameraStatus>>,
    /// By field name; only fields that a camera is assigned to.
    pub fields: watch::Sender<BTreeMap<String, FieldStatus>>,
    pub jobs: watch::Sender<Vec<Job>>,
    links: Mutex<HashMap<String, Arc<CameraLink>>>,
    last_seen: Mutex<HashMap<String, Instant>>,
    /// Display names of matches seen on the scoreboard, by match id.
    match_titles: Mutex<HashMap<String, String>>,
    job_queue: mpsc::UnboundedSender<RecordingKey>,
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
    /// Whether it has polled for its directive recently.
    pub online: bool,
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
    pub calibration: watch::Sender<Arc<MatchCalibration>>,
    /// Latest preview of each camera, indexed by [`protocol::Side::index`].
    pub previews: [watch::Sender<Option<Bytes>>; 2],
    /// Latest stitched preview JPEG.
    pub panorama: watch::Sender<Option<Bytes>>,
}

impl Hub {
    pub fn new(
        paths: Paths,
        arctos: Arctos,
        settings: Settings,
        job_queue: mpsc::UnboundedSender<RecordingKey>,
    ) -> Self {
        Hub {
            paths,
            arctos,
            settings: watch::Sender::new(settings),
            arctos_fields: watch::Sender::new(Ok(Vec::new())),
            cameras: watch::Sender::default(),
            fields: watch::Sender::default(),
            jobs: watch::Sender::default(),
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
    ) {
        let calibration = Arc::new(calibration);
        let mut links = self.links.lock().unwrap();
        match links.get(camera) {
            Some(link) => {
                link.calibration.send_replace(calibration);
            }
            None => {
                let link = Arc::new(CameraLink {
                    calibration: watch::Sender::new(calibration),
                    previews: Default::default(),
                    panorama: watch::Sender::new(None),
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
            cameras.insert(
                camera.to_owned(),
                CameraStatus {
                    address,
                    online: true,
                },
            );
        });
        tracing::info!("camera {camera} registered from {address}");
    }

    /// Keeps [`CameraStatus::online`] up to date.
    pub async fn track_presence(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            ticker.tick().await;
            let last_seen = self.last_seen.lock().unwrap().clone();
            self.cameras.send_if_modified(|cameras| {
                let mut changed = false;
                for (camera, status) in cameras.iter_mut() {
                    let online = last_seen[camera].elapsed() < ONLINE_TIMEOUT;
                    changed |= status.online != online;
                    status.online = online;
                }
                changed
            });
        }
    }

    pub fn link(&self, camera: &str) -> Option<Arc<CameraLink>> {
        self.links.lock().unwrap().get(camera).cloned()
    }

    /// What `camera` should be doing; `None` if it hasn't registered.
    pub fn directive(&self, camera: &str) -> Option<Directive> {
        *self.last_seen.lock().unwrap().get_mut(camera)? = Instant::now();
        let settings = self.settings.borrow();
        let field = settings.cameras.get(camera).and_then(|c| c.field.as_ref());
        let fields = self.fields.borrow();
        Some(
            field
                .and_then(|field| fields.get(field))
                .map_or(Directive::Idle, |status| status.directive.clone()),
        )
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
        self.enqueue(key, recording.title);
        Ok(())
    }

    /// Adds a job (or resets an existing one) and queues it.
    pub fn enqueue(&self, key: RecordingKey, title: String) {
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
            .send(key)
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
