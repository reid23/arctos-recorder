//! Stitches a base station's forwarded live previews and restreams them.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use axum::extract::ws::{Message, WebSocket};
use stitching::MatchCalibration;
use stitching::cloud::LiveConfig;
use stitching::live::{LiveStitcher, StereoYuv};
use stitching::restream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdout, Command};
use tokio::runtime::Handle;
use tokio::sync::watch;

/// Serves one connection of the live WebSocket until either side stops.
pub async fn run(mut socket: WebSocket) -> Result<()> {
    let Some(Ok(Message::Text(config))) = socket.recv().await else {
        bail!("expected the live config");
    };
    let LiveConfig {
        calibration,
        stream_url,
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
        .args(["-f", "rawvideo", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg")?;
    let mut stream = decoder.stdin.take().expect("piped stdin");
    let frames = decoder.stdout.take().expect("piped stdout");

    let (panoramas, panoramas_rx) = watch::channel(None);
    let runtime = Handle::current();
    let stitcher =
        tokio::task::spawn_blocking(move || stitch(&runtime, frames, &calibration, &panoramas));
    let forward = async {
        while let Some(message) = socket.recv().await {
            match message? {
                Message::Binary(chunk) => stream.write_all(&chunk).await?,
                Message::Close(_) => break,
                _ => {}
            }
        }
        anyhow::Ok(())
    };
    tokio::select! {
        result = forward => result,
        result = stitcher => result?,
        result = restream::stream(&stream_url, panoramas_rx) => result,
    }
}

fn stitch(
    runtime: &Handle,
    mut frames: ChildStdout,
    calibration: &MatchCalibration,
    panoramas: &watch::Sender<Option<bytes::Bytes>>,
) -> Result<()> {
    let mut stitcher = LiveStitcher::new(calibration)?;
    let mut frame = StereoYuv::new();
    loop {
        runtime
            .block_on(frames.read_exact(frame.bytes_mut()))
            .context("decoder stopped")?;
        if let Some(jpeg) = stitcher.stitch(&frame)? {
            panoramas.send_replace(Some(jpeg));
        }
    }
}
