//! Client for the cloud stitching server (see [`stitching::cloud`]).

use std::io::SeekFrom;
use std::path::Path;

use anyhow::Result;
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, HeaderValue};
use reqwest::{Body, Client, RequestBuilder, StatusCode};
use stitching::cloud::{self, FileChunk, JobRequest, JobState};
use stitching::recording::RecordingKey;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
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

    /// Uploads a file of a recording in chunks, calling `sent` with the
    /// number of bytes as they go out.
    pub async fn upload_file(
        &self,
        path: &str,
        file: &Path,
        sent: impl Fn(u64) + Clone + Send + Sync + 'static,
    ) -> Result<()> {
        let size = tokio::fs::metadata(file).await?.len();
        let mut offset = 0;
        // An empty file takes a chunk too.
        loop {
            let length = cloud::CHUNK_BYTES.min(size - offset);
            let mut contents = tokio::fs::File::open(file).await?;
            contents.seek(SeekFrom::Start(offset)).await?;
            let sent = sent.clone();
            let contents =
                ReaderStream::with_capacity(contents.take(length), 1 << 20).inspect(move |data| {
                    if let Ok(data) = data {
                        sent(data.len() as u64);
                    }
                });
            self.request(self.http.put(self.endpoint(path)))
                .query(&FileChunk { offset, size })
                .header(CONTENT_LENGTH, length)
                .body(Body::wrap_stream(contents))
                .send()
                .await?
                .error_for_status()?;
            offset += length;
            if offset == size {
                return Ok(());
            }
        }
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::{Query, State};
    use axum::routing::put;

    use super::*;

    type Received = Arc<Mutex<Vec<(FileChunk, u64)>>>;

    /// The chunks, with their bodies' lengths, and the bytes reported sent
    /// for uploading a `size`-byte file.
    async fn upload(size: u64) -> (Vec<(FileChunk, u64)>, u64) {
        async fn receive(
            State(received): State<Received>,
            Query(chunk): Query<FileChunk>,
            body: axum::body::Body,
        ) {
            let mut length = 0;
            let mut body = body.into_data_stream();
            while let Some(data) = body.next().await {
                length += data.unwrap().len() as u64;
            }
            received.lock().unwrap().push((chunk, length));
        }
        let received = Received::default();
        let app = Router::new()
            .route("/file", put(receive))
            .with_state(received.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let file =
            std::env::temp_dir().join(format!("bsw-upload-test-{size}-{}", std::process::id()));
        std::fs::File::create(&file).unwrap().set_len(size).unwrap();
        let cloud = Cloud::new(&CloudSettings {
            url,
            token: "token".into(),
        });
        let sent = Arc::new(AtomicU64::new(0));
        let count = {
            let sent = sent.clone();
            move |bytes| {
                sent.fetch_add(bytes, Ordering::Relaxed);
            }
        };
        cloud.upload_file("/file", &file, count).await.unwrap();
        std::fs::remove_file(&file).unwrap();
        let received = received.lock().unwrap().clone();
        (received, sent.load(Ordering::Relaxed))
    }

    #[tokio::test]
    async fn files_go_up_in_chunks_no_bigger_than_the_limit() {
        let chunk = |offset, size| FileChunk { offset, size };
        let limit = cloud::CHUNK_BYTES;
        assert_eq!(upload(0).await, (vec![(chunk(0, 0), 0)], 0));
        assert_eq!(upload(limit).await, (vec![(chunk(0, limit), limit)], limit));
        assert_eq!(
            upload(limit + 1).await,
            (
                vec![(chunk(0, limit + 1), limit), (chunk(limit, limit + 1), 1)],
                limit + 1
            )
        );
    }
}
