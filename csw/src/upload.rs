//! Uploads match recordings to the base station, deleting one once the base
//! station has acknowledged it.
//!
//! A segment uploads once it is finished, while later points of the match
//! are still being recorded. An upload waits while the Pi is short on CPU
//! or memory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context as AnyhowContext, Result};
use protocol::{MatchManifest, UploadStart};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::time::Sleep;

use crate::base::BaseStation;
use crate::recorder::{CLOSED_FILE, ClosedSegments, MANIFEST_FILE};
use crate::stats::Pressure;

const RETRY: Duration = Duration::from_secs(10);
/// How often an upload checks whether the Pi can spare it.
const PRESSURE_INTERVAL: Duration = Duration::from_secs(1);

pub async fn run(base: BaseStation, root: PathBuf, ready: Arc<tokio::sync::Notify>) {
    let mut registrations = base.registrations();
    let mut uploaded = HashSet::new();
    let pressure = Arc::new(Mutex::new(Pressure::new()));
    loop {
        registrations.mark_unchanged();
        tokio::select! {
            result = upload_all(&base, &root, &mut uploaded, &pressure) => {
                if let Err(e) = result {
                    tracing::warn!("upload failed: {e:#}");
                    tokio::time::sleep(RETRY).await;
                    continue;
                }
            }
            _ = registrations.changed() => {
                tracing::info!("registered again; starting uploads over");
                continue;
            }
        }
        ready.notified().await;
    }
}

async fn upload_all(
    base: &BaseStation,
    root: &Path,
    uploaded: &mut HashSet<PathBuf>,
    pressure: &Arc<Mutex<Pressure>>,
) -> Result<()> {
    let mut entries = tokio::fs::read_dir(root).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let Some(match_id) = dir.file_name().and_then(|name| name.to_str()).map(str::to_owned) else {
            continue;
        };
        // Frames still waiting on a confirmation live in `.hold`, not in a match.
        if match_id.starts_with('.') {
            continue;
        };
        if tokio::fs::try_exists(dir.join(MANIFEST_FILE)).await? {
            let bytes = tokio::fs::read(dir.join(MANIFEST_FILE)).await?;
            let manifest: MatchManifest = serde_json::from_slice(&bytes)?;
            tracing::info!("uploading match {}", manifest.match_id);
            upload_files(base, &manifest, &dir, uploaded, pressure).await?;
            base.upload_manifest(&manifest).await?;
            tokio::fs::remove_dir_all(&dir).await?;
            uploaded.retain(|path| !path.starts_with(&dir));
            tracing::info!("uploaded match {}", manifest.match_id);
        } else if tokio::fs::try_exists(dir.join(CLOSED_FILE)).await? {
            let bytes = tokio::fs::read(dir.join(CLOSED_FILE)).await?;
            let closed: ClosedSegments = serde_json::from_slice(&bytes)?;
            let manifest = MatchManifest {
                match_id,
                audio: closed.audio,
                segments: closed.segments,
            };
            let pending = manifest.files().any(|(segment, file)| {
                !uploaded.contains(&dir.join(segment.dir_name()).join(file))
            });
            if pending {
                tracing::info!("uploading finished segments of match {}", manifest.match_id);
                upload_files(base, &manifest, &dir, uploaded, pressure).await?;
            }
        }
    }
    Ok(())
}

async fn upload_files(
    base: &BaseStation,
    manifest: &MatchManifest,
    dir: &Path,
    uploaded: &mut HashSet<PathBuf>,
    pressure: &Arc<Mutex<Pressure>>,
) -> Result<()> {
    let files: Vec<_> = manifest
        .files()
        .map(|(segment, file)| (segment, file, dir.join(segment.dir_name()).join(file)))
        .collect();
    let mut bytes = 0;
    for (_, _, path) in &files {
        bytes += tokio::fs::metadata(path)
            .await
            .with_context(|| path.display().to_string())?
            .len();
    }
    if bytes > 0 {
        base.start_upload(&manifest.match_id, &UploadStart { bytes }).await?;
    }
    for (segment, file, path) in &files {
        if uploaded.contains(path) {
            continue;
        }
        let reader = Throttled::open(path, Arc::clone(pressure)).await?;
        base.upload_file(&manifest.match_id, segment, file, reader)
            .await
            .with_context(|| format!("upload {}", path.display()))?;
        uploaded.insert(path.clone());
    }
    Ok(())
}

/// A file whose reads wait while the Pi is short on CPU or memory.
struct Throttled {
    file: tokio::fs::File,
    pressure: Arc<Mutex<Pressure>>,
    last_check: Instant,
    pause: Option<Pin<Box<Sleep>>>,
    announced: bool,
}

impl Throttled {
    async fn open(path: &Path, pressure: Arc<Mutex<Pressure>>) -> Result<Self> {
        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        Ok(Throttled {
            file,
            pressure,
            last_check: Instant::now()
                .checked_sub(PRESSURE_INTERVAL)
                .unwrap_or_else(Instant::now),
            pause: None,
            announced: false,
        })
    }
}

impl AsyncRead for Throttled {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pause.is_some() {
            let ready = {
                let pause = self.pause.as_mut().expect("a pause is in progress");
                matches!(pause.as_mut().poll(context), Poll::Ready(()))
            };
            if !ready {
                return Poll::Pending;
            }
            self.pause = None;
        }
        if self.last_check.elapsed() >= PRESSURE_INTERVAL {
            self.last_check = Instant::now();
            if self.pressure.lock().unwrap().constrained() {
                if !self.announced {
                    tracing::info!("pausing an upload; the camera is short on CPU or memory");
                    self.announced = true;
                }
                let mut pause = Box::pin(tokio::time::sleep(PRESSURE_INTERVAL));
                let _ = pause.as_mut().poll(context);
                self.pause = Some(pause);
                return Poll::Pending;
            }
            self.announced = false;
        }
        Pin::new(&mut self.file).poll_read(context, buffer)
    }
}
