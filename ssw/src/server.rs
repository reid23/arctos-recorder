//! The HTTP API described in [`stitching::cloud`].

use std::sync::Arc;

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, put};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use protocol::api::valid_id;
use stitching::cloud::{self, FileChunk, JobRequest, JobState};
use stitching::recording::RecordingKey;
use tokio::io::AsyncWriteExt;

use crate::jobs::Jobs;
use crate::live;

type ApiResult<T> = Result<T, (StatusCode, String)>;

pub async fn serve(jobs: Arc<Jobs>, token: String) -> Result<()> {
    let authorization = Arc::new(format!("Bearer {token}"));
    let app = Router::new()
        .route("/api/jobs/{match_id}/{camera}", put(put_job).get(get_job))
        .route(
            "/api/jobs/{match_id}/{camera}/files/{segment}/{file}",
            put(upload_file),
        )
        .route("/api/jobs/{match_id}/{camera}/token", put(put_token))
        .route("/api/live/{camera}", get(live))
        .layer(middleware::from_fn(move |request, next| {
            authorize(authorization.clone(), request, next)
        }))
        .layer(DefaultBodyLimit::disable())
        .with_state(jobs);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cloud::PORT)).await?;
    tracing::info!("listening on port {}", cloud::PORT);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn authorize(
    expected: Arc<String>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let authorization = request.headers().get(header::AUTHORIZATION);
    if authorization.is_some_and(|value| value.as_bytes() == expected.as_bytes()) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn put_job(
    State(jobs): State<Arc<Jobs>>,
    Path(key): Path<(String, String)>,
    Json(request): Json<JobRequest>,
) -> ApiResult<Json<JobState>> {
    let key = recording_key(key)?;
    if request.recording.manifest.match_id != key.match_id {
        return Err((StatusCode::BAD_REQUEST, "match id mismatch".into()));
    }
    Ok(Json(jobs.put(&key, &request).map_err(internal)?))
}

async fn get_job(
    State(jobs): State<Arc<Jobs>>,
    Path(key): Path<(String, String)>,
) -> ApiResult<Json<JobState>> {
    let key = recording_key(key)?;
    match jobs.state(&key).map_err(internal)? {
        Some(state) => Ok(Json(state)),
        None => Err((StatusCode::NOT_FOUND, "no such job".into())),
    }
}

async fn upload_file(
    State(jobs): State<Arc<Jobs>>,
    Path((match_id, camera, segment, file)): Path<(String, String, String, String)>,
    Query(chunk): Query<FileChunk>,
    body: Body,
) -> ApiResult<StatusCode> {
    let key = recording_key((match_id, camera))?;
    if !jobs.expects(&key, &segment, &file).map_err(internal)? {
        return Err((
            StatusCode::CONFLICT,
            format!("not expecting {segment}/{file}"),
        ));
    }
    let dir = jobs.dir(&key).join(&segment);
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    // Written under a temporary name so a partial upload never counts as
    // received.
    let partial = dir.join(format!("{file}.part"));
    if write_chunk(&partial, chunk, body.into_data_stream()).await? {
        tokio::fs::rename(&partial, dir.join(&file))
            .await
            .map_err(internal)?;
        jobs.received(&key).map_err(internal)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Writes `body` into `partial` where `chunk` says, which must be right
/// after what it holds; whether that finished the file.
async fn write_chunk<E: std::fmt::Display>(
    partial: &std::path::Path,
    chunk: FileChunk,
    mut body: impl Stream<Item = Result<Bytes, E>> + Unpin,
) -> ApiResult<bool> {
    let mut out = match chunk.offset {
        0 => tokio::fs::File::create(partial).await.map_err(internal)?,
        offset => match tokio::fs::OpenOptions::new()
            .append(true)
            .open(partial)
            .await
        {
            Ok(out) if out.metadata().await.map_err(internal)?.len() == offset => out,
            _ => {
                return Err((
                    StatusCode::CONFLICT,
                    format!("not expecting bytes from {offset} on"),
                ));
            }
        },
    };
    let mut end = chunk.offset;
    while let Some(data) = body.next().await {
        let data = data.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        end += data.len() as u64;
        if end > chunk.size {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("more than the file's {} bytes", chunk.size),
            ));
        }
        out.write_all(&data).await.map_err(internal)?;
    }
    if end < chunk.size {
        // So the next chunk finds all of this one in the file.
        out.flush().await.map_err(internal)?;
        return Ok(false);
    }
    out.sync_all().await.map_err(internal)?;
    Ok(true)
}

async fn put_token(
    State(jobs): State<Arc<Jobs>>,
    Path(key): Path<(String, String)>,
    token: String,
) -> ApiResult<StatusCode> {
    let key = recording_key(key)?;
    if jobs.state(&key).map_err(internal)?.is_none() {
        return Err((StatusCode::NOT_FOUND, "no such job".into()));
    }
    jobs.set_token(&key, token);
    Ok(StatusCode::NO_CONTENT)
}

async fn live(
    State(jobs): State<Arc<Jobs>>,
    Path(camera): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let arctos = jobs.arctos().clone();
    upgrade.on_upgrade(move |socket| async move {
        tracing::info!("live preview of {camera} connected");
        if let Err(e) = live::run(socket, &arctos).await {
            tracing::warn!("live preview of {camera}: {e:#}");
        }
        tracing::info!("live preview of {camera} disconnected");
    })
}

fn recording_key((match_id, camera): (String, String)) -> ApiResult<RecordingKey> {
    if valid_id(&match_id) && valid_id(&camera) {
        Ok(RecordingKey { match_id, camera })
    } else {
        Err((StatusCode::BAD_REQUEST, "invalid id".into()))
    }
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    tracing::error!("{e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write(
        partial: &std::path::Path,
        offset: u64,
        size: u64,
        data: &'static [u8],
    ) -> ApiResult<bool> {
        let body = futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(data))]);
        write_chunk(partial, FileChunk { offset, size }, body).await
    }

    fn status(result: ApiResult<bool>) -> StatusCode {
        result.expect_err("chunk refused").0
    }

    #[tokio::test]
    async fn chunks_are_taken_in_order_until_the_file_is_whole() {
        let directory =
            std::env::temp_dir().join(format!("ssw-chunks-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let partial = directory.join("left.mjpeg.part");

        assert_eq!(
            status(write(&partial, 3, 9, b"def").await),
            StatusCode::CONFLICT
        );
        assert_eq!(write(&partial, 0, 9, b"abc").await, Ok(false));
        assert_eq!(
            status(write(&partial, 6, 9, b"ghi").await),
            StatusCode::CONFLICT
        );
        assert_eq!(write(&partial, 3, 9, b"def").await, Ok(false));
        assert_eq!(
            status(write(&partial, 6, 9, b"ghij").await),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(write(&partial, 0, 9, b"abcdef").await, Ok(false));
        assert_eq!(write(&partial, 6, 9, b"ghi").await, Ok(true));
        assert_eq!(std::fs::read(&partial).unwrap(), b"abcdefghi");

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn an_empty_file_is_whole_with_its_first_chunk() {
        let directory =
            std::env::temp_dir().join(format!("ssw-empty-chunk-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let partial = directory.join("audio.pcm.part");

        assert_eq!(write(&partial, 0, 0, b"").await, Ok(true));
        assert_eq!(std::fs::metadata(&partial).unwrap().len(), 0);

        std::fs::remove_dir_all(&directory).unwrap();
    }
}
