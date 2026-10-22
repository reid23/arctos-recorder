//! Uploads finished match recordings to the base station, deleting them once
//! it has acknowledged them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use protocol::{MatchManifest, UploadStart};
use tokio::sync::Notify;

use crate::base::BaseStation;
use crate::recorder::MANIFEST_FILE;

const RETRY: Duration = Duration::from_secs(10);

pub async fn run(base: BaseStation, root: PathBuf, ready: Arc<Notify>) {
    let mut registrations = base.registrations();
    loop {
        registrations.mark_unchanged();
        tokio::select! {
            result = upload_all(&base, &root) => {
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

async fn upload_all(base: &BaseStation, root: &Path) -> Result<()> {
    let mut entries = tokio::fs::read_dir(root).await?;
    while let Some(entry) = entries.next_entry().await? {
        let dir = entry.path();
        // Matches without a manifest are still being recorded.
        let Ok(manifest) = tokio::fs::read(dir.join(MANIFEST_FILE)).await else {
            continue;
        };
        let manifest: MatchManifest = serde_json::from_slice(&manifest)?;
        tracing::info!("uploading match {}", manifest.match_id);
        let files: Vec<_> = manifest
            .files()
            .map(|(segment, file)| (segment, file, dir.join(segment.dir_name()).join(file)))
            .collect();
        let mut bytes = 0;
        for (_, _, path) in &files {
            bytes += tokio::fs::metadata(path).await?.len();
        }
        base.start_upload(&manifest.match_id, &UploadStart { bytes })
            .await?;
        for (segment, file, path) in &files {
            base.upload_file(&manifest.match_id, segment, file, path)
                .await?;
        }
        base.upload_manifest(&manifest).await?;
        tokio::fs::remove_dir_all(&dir).await?;
        tracing::info!("uploaded match {}", manifest.match_id);
    }
    Ok(())
}
