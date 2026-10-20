//! HTTP client for the base station's camera API.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use protocol::{CameraStats, Instructions, MatchManifest, Registration, Segment, Timestamp, api};
use reqwest::{Body, Client};
use tokio_util::io::ReaderStream;

use crate::clock::Clock;

/// For requests answered right away.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct BaseStation {
    http: Client,
    /// e.g. `http://10.42.0.1:7700`; changes when the base station is
    /// rediscovered.
    url: Arc<RwLock<String>>,
    camera: String,
}

impl BaseStation {
    pub fn new(url: String, camera: String) -> Self {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("build HTTP client");
        BaseStation {
            http,
            url: Arc::new(RwLock::new(url)),
            camera,
        }
    }

    pub fn set_url(&self, url: String) {
        *self.url.write().unwrap() = url;
    }

    pub fn url(&self) -> String {
        self.url.read().unwrap().clone()
    }

    pub fn camera(&self) -> &str {
        &self.camera
    }

    pub fn preview_url(&self) -> String {
        let ws = self.url.read().unwrap().replacen("http", "ws", 1);
        format!("{ws}{}", api::preview(&self.camera))
    }

    /// Estimates the offset to the base station's clock from the fastest of
    /// several round trips.
    pub async fn sync_clock(&self, clock: &Clock) -> Result<()> {
        let mut best: Option<(i64, i64)> = None;
        for _ in 0..8 {
            let sent = Clock::boottime_micros();
            let remote: Timestamp = self.get(api::TIME).await?;
            let received = Clock::boottime_micros();
            let round_trip = received - sent;
            if best.is_none_or(|(rtt, _)| round_trip < rtt) {
                best = Some((round_trip, remote.0 - (sent + received) / 2));
            }
        }
        let (round_trip, offset) = best.expect("at least one sample");
        clock.set_offset(offset);
        tracing::debug!("clock offset {offset}µs (round trip {round_trip}µs)");
        Ok(())
    }

    pub async fn register(&self, registration: &Registration) -> Result<()> {
        self.http
            .put(self.endpoint(&api::camera(&self.camera)))
            .json(registration)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn instructions(&self, stats: &CameraStats) -> Result<Instructions> {
        Ok(self
            .http
            .post(self.endpoint(&api::instructions(&self.camera)))
            .json(stats)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn upload_file(
        &self,
        match_id: &str,
        segment: &Segment,
        file: &str,
        path: &Path,
    ) -> Result<()> {
        let contents = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        self.http
            .put(self.endpoint(&api::recording_file(&self.camera, match_id, segment, file)))
            .body(Body::wrap_stream(ReaderStream::with_capacity(
                contents,
                1 << 20,
            )))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn upload_manifest(&self, manifest: &MatchManifest) -> Result<()> {
        self.http
            .put(self.endpoint(&api::recording(&self.camera, &manifest.match_id)))
            .json(manifest)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self
            .http
            .get(self.endpoint(path))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.url.read().unwrap())
    }
}
