//! Stitches a base station's forwarded live previews and restreams them.

use std::net::Ipv4Addr;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arctos::Arctos;
use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use stitching::MatchCalibration;
use stitching::cloud::LiveConfig;
use stitching::live::{self, LiveStitcher, StereoYuv};
use stitching::overlay::{self, PlacedImage};
use stitching::restream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{ChildStdout, Command};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};

/// Chunks of the forwarded stream held for the restream before it connects
/// or when it falls behind.
const STREAM_COPY_BACKLOG: usize = 64;

/// Serves one connection of the live WebSocket until either side stops.
pub async fn run(mut socket: WebSocket, arctos: &Arctos) -> Result<()> {
    let Some(Ok(Message::Text(config))) = socket.recv().await else {
        bail!("expected the live config");
    };
    let LiveConfig {
        calibration,
        stream_url,
        overlay,
    } = serde_json::from_str(&config)?;

    let mut decoder = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
        ])
        .args(["-f", "mpegts", "-i", "-"])
        // Frames keep their (irregular) timestamps; a fine time base keeps
        // close ones from colliding.
        .args(["-fps_mode", "passthrough", "-enc_time_base", "1/90000"])
        .args(["-map", "0:v:0", "-f", "rawvideo", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg")?;
    let mut stream = decoder.stdin.take().expect("piped stdin");
    let frames = decoder.stdout.take().expect("piped stdout");

    // The restream takes its audio from a copy of the forwarded stream.
    let audio_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let audio_url = format!("tcp://{}", audio_listener.local_addr()?);
    let (stream_copy, mut stream_copy_rx) = mpsc::channel::<Bytes>(STREAM_COPY_BACKLOG);
    let feed_audio = async {
        let (mut connection, _) = audio_listener.accept().await?;
        while let Some(chunk) = stream_copy_rx.recv().await {
            connection.write_all(&chunk).await?;
        }
        anyhow::Ok(())
    };

    let (panoramas, panoramas_rx) = watch::channel(None);
    // Never changes during the connection.
    let (_overlay_config, overlay_config) = watch::channel(overlay);
    let (overlay_images, overlay_images_rx) = watch::channel(None);
    let runtime = Handle::current();
    let stitcher = tokio::task::spawn_blocking(move || {
        stitch(
            &runtime,
            frames,
            &calibration,
            &overlay_images_rx,
            &panoramas,
        )
    });
    let forward = async {
        while let Some(message) = socket.recv().await {
            match message? {
                Message::Binary(chunk) => {
                    stream.write_all(&chunk).await?;
                    let _ = stream_copy.try_send(chunk);
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        anyhow::Ok(())
    };
    tokio::select! {
        result = forward => result,
        result = stitcher => result?,
        result = restream::stream(&stream_url, panoramas_rx, restream::Audio::MpegTs(&audio_url)) => result,
        result = feed_audio => result,
        () = overlay::follow_live(arctos, overlay_config, &overlay_images, live::OUTPUT_SIZE) => {
            unreachable!("the overlay config is still open")
        }
    }
}

fn stitch(
    runtime: &Handle,
    mut frames: ChildStdout,
    calibration: &MatchCalibration,
    overlay: &watch::Receiver<Option<Arc<PlacedImage>>>,
    panoramas: &watch::Sender<Option<bytes::Bytes>>,
) -> Result<()> {
    let mut stitcher = LiveStitcher::new(calibration)?;
    let mut frame = StereoYuv::new();
    loop {
        runtime
            .block_on(frames.read_exact(frame.bytes_mut()))
            .context("decoder stopped")?;
        let overlay = overlay.borrow().clone();
        if let Some(jpeg) = stitcher.stitch(&frame, overlay.as_deref())? {
            panoramas.send_replace(Some(jpeg));
        }
    }
}
