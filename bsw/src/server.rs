//! HTTP server for the cameras' API (see [`protocol::api`]) and the live
//! MJPEG feeds shown in the UI.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use arctos::timestamp;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use bytes::{Bytes, BytesMut};
use chrono::Utc;
use futures_util::{Stream, StreamExt};
use protocol::{
    AUDIO_FILE, Directive, LEFT_FILE, MatchManifest, PreviewFrame, RIGHT_FILE, Registration, Side,
    Timestamp, api,
};
use stitching::MatchCalibration;
use stitching::recording::RecordingKey;
use tokio::io::AsyncWriteExt;
use tokio_stream::wrappers::WatchStream;

use crate::hub::Hub;

type ApiResult<T> = Result<T, (StatusCode, String)>;

pub async fn serve(hub: Arc<Hub>) -> Result<()> {
    let app = Router::new()
        .route(api::TIME, get(time))
        .route("/api/cameras/{camera}", put(register))
        .route("/api/cameras/{camera}/directive", get(directive))
        .route("/api/cameras/{camera}/preview", get(preview))
        .route(
            "/api/cameras/{camera}/recordings/{match_id}",
            put(submit_recording),
        )
        .route(
            "/api/cameras/{camera}/recordings/{match_id}/{segment}/{file}",
            put(upload_file),
        )
        .route("/live/{camera}/{feed}", get(live))
        .layer(DefaultBodyLimit::disable())
        .with_state(hub);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", api::PORT)).await?;
    tracing::info!("listening on port {}", api::PORT);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn time() -> Json<Timestamp> {
    Json(timestamp(Utc::now()))
}

async fn register(
    State(hub): State<Arc<Hub>>,
    Path(camera): Path<String>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    Json(registration): Json<Registration>,
) -> ApiResult<StatusCode> {
    check_id(&camera)?;
    let calibration: MatchCalibration =
        serde_json::from_value(registration.calibration).map_err(bad_request)?;
    calibration.validate().map_err(bad_request)?;
    hub.register(&camera, address.ip(), calibration);
    Ok(StatusCode::NO_CONTENT)
}

async fn directive(
    State(hub): State<Arc<Hub>>,
    Path(camera): Path<String>,
) -> Result<Json<Directive>, StatusCode> {
    hub.directive(&camera)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn preview(
    State(hub): State<Arc<Hub>>,
    Path(camera): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let link = hub.link(&camera).ok_or(StatusCode::NOT_FOUND)?;
    Ok(upgrade.on_upgrade(move |mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            if let Message::Binary(message) = message
                && let Some(frame) = PreviewFrame::decode(message)
            {
                link.previews[frame.side.index()].send_replace(Some(frame.jpeg));
            }
        }
    }))
}

async fn upload_file(
    State(hub): State<Arc<Hub>>,
    Path((camera, match_id, segment, file)): Path<(String, String, i64, String)>,
    body: Body,
) -> ApiResult<StatusCode> {
    check_id(&camera)?;
    check_id(&match_id)?;
    if ![LEFT_FILE, RIGHT_FILE, AUDIO_FILE].contains(&file.as_str()) {
        return Err((StatusCode::BAD_REQUEST, format!("unexpected file {file}")));
    }
    let dir = hub
        .paths
        .recording(&RecordingKey { match_id, camera })
        .join(segment.to_string());
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    let mut out = tokio::fs::File::create(dir.join(file))
        .await
        .map_err(internal)?;
    let mut body = body.into_data_stream();
    while let Some(chunk) = body.next().await {
        out.write_all(&chunk.map_err(bad_request)?)
            .await
            .map_err(internal)?;
    }
    out.sync_all().await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn submit_recording(
    State(hub): State<Arc<Hub>>,
    Path((camera, match_id)): Path<(String, String)>,
    Json(manifest): Json<MatchManifest>,
) -> ApiResult<StatusCode> {
    check_id(&camera)?;
    if manifest.match_id != match_id {
        return Err((StatusCode::BAD_REQUEST, "match id mismatch".into()));
    }
    let dir = hub.paths.recording(&RecordingKey {
        match_id,
        camera: camera.clone(),
    });
    for (segment, file) in manifest.files() {
        if !dir.join(segment.dir_name()).join(file).exists() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{} is missing", segment.dir_name()),
            ));
        }
    }
    hub.submit_recording(&camera, manifest).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// A live feed of a camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feed {
    Left,
    Right,
    Panorama,
}

impl Feed {
    pub fn url(self, camera: &str) -> String {
        let feed = match self {
            Feed::Left => "left",
            Feed::Right => "right",
            Feed::Panorama => "panorama",
        };
        format!("http://127.0.0.1:{}/live/{camera}/{feed}", api::PORT)
    }
}

/// Serves a feed as `multipart/x-mixed-replace` JPEGs, which `<img>` plays.
async fn live(
    State(hub): State<Arc<Hub>>,
    Path((camera, feed)): Path<(String, Feed)>,
) -> Result<Response, StatusCode> {
    let link = hub.link(&camera).ok_or(StatusCode::NOT_FOUND)?;
    let jpegs: std::pin::Pin<Box<dyn Stream<Item = Bytes> + Send>> = match feed {
        Feed::Left | Feed::Right => {
            let side = if feed == Feed::Left {
                Side::Left
            } else {
                Side::Right
            };
            let previews = WatchStream::new(link.previews[side.index()].subscribe());
            Box::pin(previews.filter_map(|preview| async move { preview }))
        }
        Feed::Panorama => {
            let panoramas = WatchStream::new(link.panorama.subscribe());
            Box::pin(panoramas.filter_map(|jpeg| async move { jpeg }))
        }
    };
    let parts = jpegs.map(|jpeg| {
        let mut part = BytesMut::from(
            format!(
                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                jpeg.len()
            )
            .as_bytes(),
        );
        part.extend_from_slice(&jpeg);
        part.extend_from_slice(b"\r\n");
        Ok::<_, Infallible>(part.freeze())
    });
    Ok((
        [(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=frame",
        )],
        Body::from_stream(parts),
    )
        .into_response())
}

fn check_id(id: &str) -> ApiResult<()> {
    if api::valid_id(id) {
        Ok(())
    } else {
        Err((StatusCode::BAD_REQUEST, format!("invalid id {id:?}")))
    }
}

fn bad_request(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    tracing::error!("{e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
