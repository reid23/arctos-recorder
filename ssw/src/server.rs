//! The HTTP API described in [`stitching::cloud`].

use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Request, State, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, put};
use axum::{Json, Router};
use futures_util::StreamExt;
use protocol::api::valid_id;
use stitching::cloud::{self, JobRequest, JobState};
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
    let mut out = tokio::fs::File::create(&partial).await.map_err(internal)?;
    let mut body = body.into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        out.write_all(&chunk).await.map_err(internal)?;
    }
    out.sync_all().await.map_err(internal)?;
    tokio::fs::rename(&partial, dir.join(&file))
        .await
        .map_err(internal)?;
    jobs.received(&key).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn live(Path(camera): Path<String>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| async move {
        tracing::info!("live preview of {camera} connected");
        if let Err(e) = live::run(socket).await {
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
