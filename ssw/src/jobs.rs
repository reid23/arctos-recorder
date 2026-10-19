//! Recordings sent by base stations, processed one at a time.
//!
//! Each job lives in `<root>/<match id>/<camera>/`: [`REQUEST_FILE`] plus the
//! recording's segment directories as they are uploaded. Once the video is on
//! Arctos the directory is emptied except for [`DONE_FILE`]. Arctos access
//! tokens are only kept in memory; base stations send them again when
//! needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use arctos::{Arctos, TokenSource};
use futures_util::future::BoxFuture;
use protocol::Segment;
use stitching::JobStatus;
use stitching::cloud::{JobRequest, JobState};
use stitching::recording::{OverlayImages, RecordingKey, VIDEO_FILE, render};
use tokio::sync::{mpsc, watch};

const REQUEST_FILE: &str = "job.json";
const DONE_FILE: &str = "done";

pub struct Jobs {
    root: PathBuf,
    arctos: Arctos,
    statuses: Mutex<HashMap<RecordingKey, JobStatus>>,
    /// The latest Arctos access token sent for each job.
    tokens: Mutex<HashMap<RecordingKey, watch::Sender<Option<String>>>>,
    queue: mpsc::UnboundedSender<RecordingKey>,
}

impl Jobs {
    /// Picks up the jobs left by a previous run, queueing the ones that have
    /// all of their files.
    pub fn restore(
        root: PathBuf,
        arctos: Arctos,
    ) -> Result<(Self, mpsc::UnboundedReceiver<RecordingKey>)> {
        std::fs::create_dir_all(&root)?;
        let (queue, queue_rx) = mpsc::unbounded_channel();
        let jobs = Jobs {
            root,
            arctos,
            statuses: Mutex::default(),
            tokens: Mutex::default(),
            queue,
        };
        for match_dir in std::fs::read_dir(&jobs.root)? {
            let match_dir = match_dir?;
            for camera_dir in std::fs::read_dir(match_dir.path())? {
                let key = RecordingKey {
                    match_id: match_dir.file_name().to_string_lossy().into_owned(),
                    camera: camera_dir?.file_name().to_string_lossy().into_owned(),
                };
                jobs.refresh(&key)?;
            }
        }
        Ok((jobs, queue_rx))
    }

    pub fn arctos(&self) -> &Arctos {
        &self.arctos
    }

    pub fn dir(&self, key: &RecordingKey) -> PathBuf {
        self.root.join(&key.match_id).join(&key.camera)
    }

    pub fn state(&self, key: &RecordingKey) -> Result<Option<JobState>> {
        let Some(status) = self.statuses.lock().unwrap().get(key).cloned() else {
            return Ok(None);
        };
        let missing = match status {
            JobStatus::Sending(_) => missing_files(&self.dir(key))?,
            _ => Vec::new(),
        };
        let has_token = self.token(key).borrow().is_some();
        Ok(Some(JobState {
            status,
            missing,
            has_token,
        }))
    }

    /// Creates a job, or retries it if it failed.
    pub fn put(&self, key: &RecordingKey, request: &JobRequest) -> Result<JobState> {
        let status = self.statuses.lock().unwrap().get(key).cloned();
        if matches!(status, None | Some(JobStatus::Failed(_))) {
            let dir = self.dir(key);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join(REQUEST_FILE), serde_json::to_vec(request)?)?;
            self.refresh(key)?;
        }
        Ok(self.state(key)?.expect("job exists"))
    }

    /// Whether `file` of `segment` belongs to the job and is still missing.
    pub fn expects(&self, key: &RecordingKey, segment: &str, file: &str) -> Result<bool> {
        if !matches!(self.status(key), Some(JobStatus::Sending(_))) {
            return Ok(false);
        }
        Ok(missing_files(&self.dir(key))?
            .iter()
            .any(|(s, f)| s.dir_name() == segment && f == file))
    }

    /// Called when a file has been stored; starts the job once it is
    /// complete.
    pub fn received(&self, key: &RecordingKey) -> Result<()> {
        self.refresh(key)
    }

    pub fn set_token(&self, key: &RecordingKey, token: String) {
        self.token(key).send_replace(Some(token));
    }

    fn token(&self, key: &RecordingKey) -> watch::Sender<Option<String>> {
        let mut tokens = self.tokens.lock().unwrap();
        tokens
            .entry(key.clone())
            .or_insert_with(|| watch::channel(None).0)
            .clone()
    }

    fn status(&self, key: &RecordingKey) -> Option<JobStatus> {
        self.statuses.lock().unwrap().get(key).cloned()
    }

    fn set_status(&self, key: &RecordingKey, status: JobStatus) {
        self.statuses.lock().unwrap().insert(key.clone(), status);
    }

    /// Derives a job's status from its directory, queueing it if it has
    /// everything.
    fn refresh(&self, key: &RecordingKey) -> Result<()> {
        let dir = self.dir(key);
        if dir.join(DONE_FILE).exists() {
            self.set_status(key, JobStatus::Done);
        } else if dir.join(REQUEST_FILE).exists() {
            let request = load_request(&dir)?;
            let total = request.recording.manifest.files().count();
            let missing = missing_files(&dir)?.len();
            if missing == 0 {
                self.set_status(key, JobStatus::Queued);
                self.queue.send(key.clone()).expect("job worker is running");
            } else {
                let received = (total - missing) as f32 / total as f32;
                self.set_status(key, JobStatus::Sending(received));
            }
        }
        Ok(())
    }

    pub async fn run(self: Arc<Self>, mut queue: mpsc::UnboundedReceiver<RecordingKey>) {
        while let Some(key) = queue.recv().await {
            let status = match self.process(&key).await {
                Ok(()) => JobStatus::Done,
                Err(e) => {
                    tracing::error!("processing {key:?}: {e:#}");
                    JobStatus::Failed(format!("{e:#}"))
                }
            };
            self.set_status(&key, status);
        }
    }

    async fn process(self: &Arc<Self>, key: &RecordingKey) -> Result<()> {
        tracing::info!("processing {key:?}");
        let dir = self.dir(key);
        let request = load_request(&dir)?;
        let recording = &request.recording;
        let token = JobToken(self.token(key).subscribe());

        // Left by an earlier attempt whose upload failed.
        let video = dir.join(VIDEO_FILE);
        if !video.exists() {
            self.set_status(key, JobStatus::Stitching(0.0));
            let overlay = match &request.overlay {
                Some(placement) => {
                    Some(OverlayImages::fetch(&self.arctos, recording, placement).await?)
                }
                None => None,
            };
            let progress = {
                let (jobs, key) = (self.clone(), key.clone());
                Arc::new(move |p| jobs.set_status(&key, JobStatus::Stitching(p)))
            };
            let (render_dir, render_recording) = (dir.clone(), recording.clone());
            tokio::task::spawn_blocking(move || {
                render(&render_dir, &render_recording, overlay.as_ref(), progress)
            })
            .await??;
        }

        self.set_status(key, JobStatus::Uploading(0.0));
        let anchors = recording.anchors();
        self.arctos
            .upload_footage(&token, &recording.footage(key, &video, &anchors), |p| {
                self.set_status(key, JobStatus::Uploading(p))
            })
            .await?;

        tokio::fs::remove_dir_all(&dir).await?;
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(dir.join(DONE_FILE), b"").await?;
        self.tokens.lock().unwrap().remove(key);
        tracing::info!("finished {key:?}");
        Ok(())
    }
}

/// The latest token sent for a job, waiting for one if there is none.
struct JobToken(watch::Receiver<Option<String>>);

impl TokenSource for JobToken {
    fn access_token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let mut token = self.0.clone();
            let token = token.wait_for(Option::is_some).await?;
            Ok(token.clone().expect("waited for a token"))
        })
    }
}

fn load_request(dir: &Path) -> Result<JobRequest> {
    let path = dir.join(REQUEST_FILE);
    let json = std::fs::read(&path).with_context(|| path.display().to_string())?;
    Ok(serde_json::from_slice(&json)?)
}

fn missing_files(dir: &Path) -> Result<Vec<(Segment, String)>> {
    let request = load_request(dir)?;
    Ok(request
        .recording
        .manifest
        .files()
        .filter(|(segment, file)| !dir.join(segment.dir_name()).join(file).exists())
        .map(|(segment, file)| (segment, file.to_owned()))
        .collect())
}
