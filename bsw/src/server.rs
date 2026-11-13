//! HTTP server for the cameras' API (see [`protocol::api`]) and the live
//! feeds shown in the UI and restreamed.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use arctos::timestamp;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use bytes::{Buf, Bytes, BytesMut};
use chrono::Utc;
use futures_util::{Stream, StreamExt};
use protocol::{
    AUDIO_FILE, CameraStats, Instructions, LEFT_FILE, MatchManifest, PreviewFrame, PreviewMessage,
    RIGHT_FILE, Registration, Side, Timestamp, UploadStart, api, rig,
};
use stitching::geometry::Calibration;
use stitching::recording::RecordingKey;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio_stream::wrappers::WatchStream;

use crate::hub::{Hub, StereoPreview};
use crate::settings::RigSetup;

type ApiResult<T> = Result<T, (StatusCode, String)>;

pub async fn serve(hub: Arc<Hub>) -> Result<()> {
    let app = Router::new()
        .route(api::TIME, get(time))
        .route("/api/cameras/{camera}", put(register))
        .route("/api/cameras/{camera}/instructions", post(instructions))
        .route("/api/cameras/{camera}/preview", get(preview))
        .route(
            "/api/cameras/{camera}/recordings/{match_id}",
            put(submit_recording),
        )
        .route(
            "/api/cameras/{camera}/recordings/{match_id}/upload",
            put(start_upload),
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
    fn usable(setup: serde_json::Value) -> Result<RigSetup> {
        let setup: RigSetup = serde_json::from_value(setup)?;
        if let Some(calibration) = &setup.calibration {
            calibration.validate()?;
        }
        Ok(setup)
    }
    check_id(&camera)?;
    let calibration: Calibration =
        serde_json::from_value(registration.calibration).map_err(bad_request)?;
    calibration.validate().map_err(bad_request)?;
    let setup = registration.setup.and_then(|setup| {
        usable(setup)
            .inspect_err(|e| tracing::warn!("ignoring the setup {camera} keeps: {e:#}"))
            .ok()
    });
    hub.register(
        &camera,
        address.ip(),
        registration.software_id,
        calibration,
        setup,
        registration.audio_devices,
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn instructions(
    State(hub): State<Arc<Hub>>,
    Path(camera): Path<String>,
    Json(stats): Json<CameraStats>,
) -> ApiResult<Json<Instructions>> {
    if let Ok(bytes) = serde_json::to_vec(&stats) {
        hub.network.camera_received(&camera, bytes.len() as u64);
    }
    let instructions = hub.instructions(&camera, &stats).map_err(internal)?;
    let Some(instructions) = instructions else {
        return Err((StatusCode::NOT_FOUND, "camera is not registered".to_owned()));
    };
    if let Ok(bytes) = serde_json::to_vec(&instructions) {
        hub.network.camera_sent(&camera, bytes.len() as u64);
    }
    Ok(Json(instructions))
}

async fn preview(
    State(hub): State<Arc<Hub>>,
    Path(camera): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let link = hub.link(&camera).ok_or(StatusCode::NOT_FOUND)?;
    Ok(upgrade.on_upgrade(move |mut socket| async move {
        let mut pairer = PreviewPairer::default();
        while let Some(Ok(message)) = socket.recv().await {
            let Message::Binary(message) = message else {
                continue;
            };
            hub.network.camera_received(&camera, message.len() as u64);
            match PreviewMessage::decode(message) {
                Some(PreviewMessage::Frame(PreviewFrame {
                    side,
                    timestamp: frame_timestamp,
                    jpeg,
                })) => {
                    let side = if hub.settings.borrow().swaps_sides(&camera) {
                        side.opposite()
                    } else {
                        side
                    };
                    link.previews[side.index()].send_replace(Some(jpeg.clone()));
                    for preview in pairer.push(side, frame_timestamp, jpeg) {
                        let _ = link.live.send(preview);
                    }
                }
                Some(PreviewMessage::Audio(pcm)) => {
                    *link.last_audio.lock().unwrap() = Some(Instant::now());
                    let _ = link.audio.send(pcm);
                }
                None => {}
            }
        }
    }))
}

#[derive(Default)]
struct PreviewPairer {
    pending: [BTreeMap<Timestamp, Bytes>; 2],
    newest: Option<Timestamp>,
    legacy: [Option<Bytes>; 2],
}

impl PreviewPairer {
    fn push(
        &mut self,
        side: Side,
        timestamp: Timestamp,
        jpeg: Bytes,
    ) -> Vec<StereoPreview> {
        if timestamp == Timestamp(0) {
            self.legacy[side.index()] = Some(jpeg);
            if self.legacy.iter().any(Option::is_none) {
                return Vec::new();
            }
            let [left, right] = &mut self.legacy;
            return vec![StereoPreview {
                jpegs: [
                    left.take().expect("checked"),
                    right.take().expect("checked"),
                ],
            }];
        }
        if self
            .newest
            .is_some_and(|newest| newest.micros_since(timestamp) > 500_000)
        {
            self.pending = Default::default();
            self.newest = None;
        }
        self.pending[side.index()].insert(timestamp, jpeg);
        self.newest = Some(self.newest.map_or(timestamp, |newest| newest.max(timestamp)));
        let mut pairs = Vec::new();
        loop {
            let heads = Side::BOTH.map(|side| {
                self.pending[side.index()]
                    .first_key_value()
                    .map(|(&timestamp, _)| timestamp)
            });
            if let [Some(left), Some(right)] = heads
                && left.micros_since(right).abs() <= rig::frame_micros() / 2
            {
                let [left_frames, right_frames] = &mut self.pending;
                let (_, left) = left_frames.pop_first().expect("head exists");
                let (_, right) = right_frames.pop_first().expect("head exists");
                pairs.push(StereoPreview { jpegs: [left, right] });
                continue;
            }
            let (oldest_side, oldest) = match heads {
                [Some(left), Some(right)] if left < right => (Side::Left, left),
                [Some(_), Some(right)] => (Side::Right, right),
                [Some(left), None] => (Side::Left, left),
                [None, Some(right)] => (Side::Right, right),
                [None, None] => return pairs,
            };
            if self
                .newest
                .is_none_or(|newest| newest.micros_since(oldest) < 500_000)
            {
                return pairs;
            }
            self.pending[oldest_side.index()].pop_first();
        }
    }
}

async fn start_upload(
    State(hub): State<Arc<Hub>>,
    Path((camera, match_id)): Path<(String, String)>,
    Json(start): Json<UploadStart>,
) -> ApiResult<StatusCode> {
    check_id(&camera)?;
    check_id(&match_id)?;
    hub.start_receiving(RecordingKey { match_id, camera }, start.bytes);
    Ok(StatusCode::NO_CONTENT)
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
    let side = Side::BOTH.into_iter().find(|side| side.file_name() == file);
    let file = match side {
        Some(side) if hub.settings.borrow().swaps_sides(&camera) => {
            side.opposite().file_name().to_owned()
        }
        _ => file,
    };
    let key = RecordingKey { match_id, camera };
    let dir = hub.paths.recording(&key).join(segment.to_string());
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    let mut out = tokio::fs::File::create(dir.join(file))
        .await
        .map_err(internal)?;
    let mut body = body.into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(bad_request)?;
        out.write_all(&chunk).await.map_err(internal)?;
        hub.received(&key, chunk.len() as u64);
        hub.network.camera_received(&key.camera, chunk.len() as u64);
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
    /// The microphone, as in [`protocol::AUDIO_FILE`], in real time.
    Audio,
}

impl Feed {
    pub fn url(self, camera: &str) -> String {
        let feed = match self {
            Feed::Left => "left",
            Feed::Right => "right",
            Feed::Panorama => "panorama",
            Feed::Audio => "audio",
        };
        format!("http://127.0.0.1:{}/live/{camera}/{feed}", api::PORT)
    }
}

/// Audio sent per tick of [`Feed::Audio`].
const AUDIO_TICK: Duration = Duration::from_millis(20);
/// Audio held back to smooth out the network.
const AUDIO_JITTER_BUFFER: Duration = Duration::from_millis(100);
/// Most audio held back; beyond it, the oldest is dropped to catch up.
const AUDIO_MAX_BUFFER: Duration = Duration::from_millis(500);

fn pcm_bytes(duration: Duration) -> usize {
    let samples = duration.as_micros() as usize * rig::AUDIO_SAMPLE_RATE as usize / 1_000_000;
    samples * size_of::<i16>()
}

/// Serves a feed: video as `multipart/x-mixed-replace` JPEGs, which `<img>`
/// plays, and audio as a steady stream of PCM with silence where the
/// microphone's audio is missing.
async fn live(
    State(hub): State<Arc<Hub>>,
    Path((camera, feed)): Path<(String, Feed)>,
) -> Result<Response, StatusCode> {
    let link = hub.link(&camera).ok_or(StatusCode::NOT_FOUND)?;
    let jpegs: std::pin::Pin<Box<dyn Stream<Item = Bytes> + Send>> = match feed {
        Feed::Audio => return Ok(paced_audio(link.audio.subscribe()).into_response()),
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

fn paced_audio(mut audio: broadcast::Receiver<Bytes>) -> Body {
    let mut ticker = tokio::time::interval(AUDIO_TICK);
    let mut buffered = BytesMut::new();
    let mut playing = false;
    let ticks =
        futures_util::stream::poll_fn(move |context| ticker.poll_tick(context).map(|_| Some(())));
    Body::from_stream(ticks.map(move |()| {
        loop {
            match audio.try_recv() {
                Ok(pcm) => buffered.extend_from_slice(&pcm),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        let excess = buffered.len().saturating_sub(pcm_bytes(AUDIO_MAX_BUFFER));
        buffered.advance(excess);
        playing = if playing {
            !buffered.is_empty()
        } else {
            buffered.len() >= pcm_bytes(AUDIO_JITTER_BUFFER)
        };
        let tick = pcm_bytes(AUDIO_TICK);
        let mut chunk = if playing {
            buffered.split_to(tick.min(buffered.len()))
        } else {
            BytesMut::new()
        };
        chunk.resize(tick, 0);
        Ok::<_, Infallible>(chunk.freeze())
    }))
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
