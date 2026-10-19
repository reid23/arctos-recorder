//! Client for the cloud stitching server (see [`stitching::cloud`]).

use std::path::Path;

use anyhow::Result;
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Body, Client, RequestBuilder, StatusCode};
use stitching::cloud::{self, JobRequest, JobState};
use stitching::recording::RecordingKey;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::io::ReaderStream;

use crate::settings::CloudSettings;

#[derive(Clone)]
pub struct Cloud {
    http: Client,
    url: String,
    authorization: String,
}

impl Cloud {
    pub fn new(settings: &CloudSettings) -> Self {
        Cloud {
            http: Client::new(),
            url: settings.url.trim_end_matches('/').to_owned(),
            authorization: format!("Bearer {}", settings.token),
        }
    }

    pub async fn put_job(&self, key: &RecordingKey, request: &JobRequest) -> Result<JobState> {
        Ok(self
            .request(self.http.put(self.endpoint(&cloud::job(key))))
            .json(request)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// The job's state; `None` if the server doesn't know about it.
    pub async fn job(&self, key: &RecordingKey) -> Result<Option<JobState>> {
        let response = self
            .request(self.http.get(self.endpoint(&cloud::job(key))))
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(response.error_for_status()?.json().await?))
    }

    pub async fn put_token(&self, key: &RecordingKey, access_token: &str) -> Result<()> {
        self.request(self.http.put(self.endpoint(&cloud::token(key))))
            .body(access_token.to_owned())
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Uploads a file of a recording, calling `sent` with the number of bytes
    /// as they go out.
    pub async fn upload_file(
        &self,
        path: &str,
        file: &Path,
        mut sent: impl FnMut(u64) + Send + Sync + 'static,
    ) -> Result<()> {
        let contents = ReaderStream::with_capacity(tokio::fs::File::open(file).await?, 1 << 20)
            .inspect(move |chunk| {
                if let Ok(chunk) = chunk {
                    sent(chunk.len() as u64);
                }
            });
        self.request(self.http.put(self.endpoint(path)))
            .body(Body::wrap_stream(contents))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// The WebSocket request for forwarding a camera's live previews.
    pub fn live_request(
        &self,
        camera: &str,
    ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        let url = format!(
            "{}{}",
            self.url.replacen("http", "ws", 1),
            cloud::live(camera)
        );
        let mut request = url.into_client_request()?;
        request
            .headers_mut()
            .insert(AUTHORIZATION, HeaderValue::from_str(&self.authorization)?);
        Ok(request)
    }

    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        request.header(AUTHORIZATION, &self.authorization)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }
}
