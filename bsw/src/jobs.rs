//! Turns uploaded match recordings into stitched videos on Arctos, either
//! locally or on the cloud server.
//!
//! Each recording lives in `recordings/<match id>/<camera>/` (see
//! [`stitching::recording`]). A recording is stitched (locally into
//! [`VIDEO_FILE`], or on the cloud server, which uploads it too), then the
//! video is uploaded. Either step can be held back by the settings until the
//! user releases it. The media is deleted only once the video is on Arctos,
//! leaving [`RECORDING_FILE`](stitching::recording::RECORDING_FILE) and
//! [`UPLOADED_FILE`] behind as a record.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use arctos::TokenSource;
use stitching::JobStatus;
use stitching::cloud::JobRequest;
use stitching::recording::{OverlayImages, Recording, RecordingKey, VIDEO_FILE, render};
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

/// A request to move a job along.
pub struct Order {
    pub key: RecordingKey,
    /// Do the job's next step even if the settings say it should wait.
    pub release: bool,
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
                hub.enqueue(key, recording.title, false);
            }
        }
    }
    Ok(())
}

pub async fn run(hub: Arc<Hub>, mut queue: UnboundedReceiver<Order>) {
    while let Some(order) = queue.recv().await {
        if let Err(e) = process(&hub, &order).await {
            fail(&hub, &order.key, e);
        }
    }
}

fn fail(hub: &Hub, key: &RecordingKey, error: anyhow::Error) {
    tracing::error!("processing {key:?}: {error:#}");
    hub.set_job_status(key, JobStatus::Failed(format!("{error:#}")));
}

async fn process(hub: &Arc<Hub>, order: &Order) -> Result<()> {
    let key = &order.key;
    let dir = hub.paths.recording(key);
    let recording = Recording::load(&dir)?;
    let settings = hub.settings.borrow().clone();
    let mut release = order.release;

    let video = dir.join(VIDEO_FILE);
    if !video.exists() {
        if settings.stitches_later() && !release {
            hub.set_job_status(key, JobStatus::StitchLater);
            return Ok(());
        }
        release = false;
        match settings.stitching {
            Stitching::Local => stitch(hub, key, &dir, &recording).await?,
            Stitching::Cloud => return send_to_cloud(hub, key, recording).await,
        }
    }

    if settings.workflow.upload_later && !release {
        hub.set_job_status(key, JobStatus::UploadLater);
        return Ok(());
    }
    hub.set_job_status(key, JobStatus::Uploading(0.0));
    let anchors = recording.anchors();
    hub.arctos
        .upload_footage(
            hub.account.as_ref(),
            &recording.footage(key, &video, &anchors),
            |p| hub.set_job_status(key, JobStatus::Uploading(p)),
        )
        .await?;
    finish(hub, key, &dir, &recording)
}

async fn stitch(
    hub: &Arc<Hub>,
    key: &RecordingKey,
    dir: &Path,
    recording: &Recording,
) -> Result<()> {
    hub.set_job_status(key, JobStatus::Stitching(0.0));
    let placement = hub.settings.borrow().overlay.clone();
    let overlay = if placement.enabled {
        Some(OverlayImages::fetch(&hub.arctos, recording, &placement).await?)
    } else {
        None
    };
    let progress = {
        let (hub, key) = (hub.clone(), key.clone());
        Arc::new(move |p| hub.set_job_status(&key, JobStatus::Stitching(p)))
    };
    let (dir, recording) = (dir.to_owned(), recording.clone());
    tokio::task::spawn_blocking(move || render(&dir, &recording, overlay.as_ref(), progress))
        .await??;
    Ok(())
}

/// Sends the recording to the cloud server, then follows its progress in the
/// background so the next recording can be sent meanwhile.
async fn send_to_cloud(hub: &Arc<Hub>, key: &RecordingKey, recording: Recording) -> Result<()> {
    let cloud = Cloud::new(&hub.settings.borrow().cloud);
    send_recording(hub, &cloud, key, &recording).await?;
    let (hub, key) = (hub.clone(), key.clone());
    tokio::spawn(async move {
        if let Err(e) = follow_cloud_job(&hub, &cloud, &key, &recording).await {
            fail(&hub, &key, e);
        }
    });
    Ok(())
}

/// Creates the job on the cloud server and sends it the files it is missing.
async fn send_recording(
    hub: &Arc<Hub>,
    cloud: &Cloud,
    key: &RecordingKey,
    recording: &Recording,
) -> Result<()> {
    let dir = hub.paths.recording(key);
    let overlay = hub.settings.borrow().overlay.clone();
    let request = JobRequest {
        recording: recording.clone(),
        overlay: overlay.enabled.then_some(overlay),
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
    Ok(())
}

/// Follows the job on the cloud server until the video is on Arctos, keeping
/// the server supplied with a fresh access token and sending the job again
/// if the server lost it.
async fn follow_cloud_job(
    hub: &Arc<Hub>,
    cloud: &Cloud,
    key: &RecordingKey,
    recording: &Recording,
) -> Result<()> {
    let mut token_sent = None;
    loop {
        let Some(state) = cloud.job(key).await? else {
            send_recording(hub, cloud, key, recording).await?;
            token_sent = None;
            continue;
        };
        match state.status {
            JobStatus::Done => return finish(hub, key, &hub.paths.recording(key), recording),
            JobStatus::Failed(error) => bail!("cloud stitching failed: {error}"),
            status => hub.set_job_status(key, status),
        }
        let token = hub.account.access_token().await?;
        if !state.has_token || token_sent.as_ref() != Some(&token) {
            cloud.put_token(key, &token).await?;
            token_sent = Some(token);
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
