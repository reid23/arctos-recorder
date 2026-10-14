//! Turns uploaded match recordings into stitched videos on Arctos, either
//! locally or on the cloud server.
//!
//! Each recording lives in `recordings/<match id>/<camera>/` (see
//! [`stitching::recording`]). Its media is deleted only once the video is on
//! Arctos, leaving [`RECORDING_FILE`](stitching::recording::RECORDING_FILE)
//! and [`UPLOADED_FILE`] behind as a record.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use stitching::JobStatus;
use stitching::cloud::JobRequest;
use stitching::recording::{Recording, RecordingKey, render};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::cloud::Cloud;
use crate::hub::Hub;
use crate::settings::Stitching;

const UPLOADED_FILE: &str = "uploaded";
/// How often the cloud server is asked how a job is doing.
const CLOUD_POLL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub key: RecordingKey,
    pub title: String,
    pub status: JobStatus,
}

/// Lists recordings from previous runs, queueing the unfinished ones.
pub fn restore(hub: &Hub) -> Result<()> {
    for match_dir in std::fs::read_dir(hub.paths.recordings())? {
        let match_dir = match_dir?.path();
        for camera_dir in std::fs::read_dir(&match_dir)? {
            let camera_dir = camera_dir?.path();
            // Without a recording file the camera never finished uploading
            // it; it will upload it again.
            let Ok(recording) = Recording::load(&camera_dir) else {
                continue;
            };
            let key = RecordingKey {
                match_id: recording.manifest.match_id.clone(),
                camera: file_name(&camera_dir),
            };
            if camera_dir.join(UPLOADED_FILE).exists() {
                hub.jobs.send_modify(|jobs| {
                    jobs.push(Job {
                        key,
                        title: recording.title,
                        status: JobStatus::Done,
                    });
                });
            } else {
                hub.enqueue(key, recording.title);
            }
        }
    }
    Ok(())
}

pub async fn run(hub: Arc<Hub>, mut queue: UnboundedReceiver<RecordingKey>) {
    while let Some(key) = queue.recv().await {
        let stitching = hub.settings.borrow().stitching;
        let result = match stitching {
            Stitching::Local => process_locally(&hub, &key).await,
            Stitching::Cloud => send_to_cloud(&hub, &key).await,
        };
        if let Err(e) = result {
            fail(&hub, &key, e);
        }
    }
}

fn fail(hub: &Hub, key: &RecordingKey, error: anyhow::Error) {
    tracing::error!("processing {key:?}: {error:#}");
    hub.set_job_status(key, JobStatus::Failed(format!("{error:#}")));
}

async fn process_locally(hub: &Arc<Hub>, key: &RecordingKey) -> Result<()> {
    let dir = hub.paths.recording(key);
    let recording = Recording::load(&dir)?;

    hub.set_job_status(key, JobStatus::Stitching(0.0));
    let progress = {
        let (hub, key) = (hub.clone(), key.clone());
        Arc::new(move |p| hub.set_job_status(&key, JobStatus::Stitching(p)))
    };
    let (render_dir, render_recording) = (dir.clone(), recording.clone());
    let video =
        tokio::task::spawn_blocking(move || render(&render_dir, &render_recording, progress))
            .await??;

    hub.set_job_status(key, JobStatus::Uploading(0.0));
    let credentials = hub.settings.borrow().arctos.credentials.clone();
    let anchors = recording.anchors();
    hub.arctos
        .upload_footage(
            &credentials,
            &recording.footage(key, &video, &anchors),
            |p| hub.set_job_status(key, JobStatus::Uploading(p)),
        )
        .await?;
    finish(hub, key, &dir, &recording)
}

/// Sends the recording to the cloud server, then follows its progress in the
/// background so the next recording can be sent meanwhile.
async fn send_to_cloud(hub: &Arc<Hub>, key: &RecordingKey) -> Result<()> {
    let dir = hub.paths.recording(key);
    let recording = Recording::load(&dir)?;
    let cloud = Cloud::new(&hub.settings.borrow().cloud);
    let request = JobRequest {
        recording: recording.clone(),
        credentials: hub.settings.borrow().arctos.credentials.clone(),
    };
    let state = cloud.put_job(key, &request).await?;

    let paths: Vec<_> = state
        .missing
        .iter()
        .map(|(segment, file)| dir.join(segment.dir_name()).join(file))
        .collect();
    let total: u64 = paths
        .iter()
        .map(|path| Ok(std::fs::metadata(path)?.len()))
        .sum::<Result<_>>()?;
    let sent = Arc::new(AtomicU64::new(0));
    hub.set_job_status(key, JobStatus::Sending(0.0));
    for ((segment, file), path) in state.missing.iter().zip(&paths) {
        let (hub, key, sent) = (hub.clone(), key.clone(), sent.clone());
        cloud
            .upload_file(
                &stitching::cloud::file(&key, segment, file),
                path,
                move |bytes| {
                    let sent = sent.fetch_add(bytes, Ordering::Relaxed) + bytes;
                    hub.set_job_status(&key, JobStatus::Sending(sent as f32 / total as f32));
                },
            )
            .await?;
    }

    let (hub, key) = (hub.clone(), key.clone());
    tokio::spawn(async move {
        if let Err(e) = follow_cloud_job(&hub, &cloud, &key, &dir, &recording).await {
            fail(&hub, &key, e);
        }
    });
    Ok(())
}

async fn follow_cloud_job(
    hub: &Hub,
    cloud: &Cloud,
    key: &RecordingKey,
    dir: &Path,
    recording: &Recording,
) -> Result<()> {
    loop {
        match cloud.job(key).await?.status {
            JobStatus::Done => return finish(hub, key, dir, recording),
            JobStatus::Failed(error) => bail!("cloud stitching failed: {error}"),
            status => hub.set_job_status(key, status),
        }
        tokio::time::sleep(CLOUD_POLL).await;
    }
}

/// Called once the video is on Arctos.
fn finish(hub: &Hub, key: &RecordingKey, dir: &Path, recording: &Recording) -> Result<()> {
    std::fs::write(dir.join(UPLOADED_FILE), b"")?;
    recording.delete_media(dir)?;
    hub.set_job_status(key, JobStatus::Done);
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}
