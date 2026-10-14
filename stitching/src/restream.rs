//! Restreaming a stitched live preview over RTMP (e.g. to YouTube Live)
//! using `ffmpeg`.

use std::process::Stdio;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::watch;

use crate::live;

/// Streams the stitched preview JPEGs to `url` until `frames` closes or
/// ffmpeg fails.
pub async fn stream(url: &str, mut frames: watch::Receiver<Option<Bytes>>) -> Result<()> {
    let fps = live::FPS.to_string();
    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-use_wallclock_as_timestamps",
            "1",
            "-f",
            "mjpeg",
            "-i",
            "-",
        ])
        // YouTube expects an audio track.
        .args([
            "-f",
            "lavfi",
            "-i",
            "anullsrc=channel_layout=mono:sample_rate=44100",
        ])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
        ])
        .args([
            "-r",
            &fps,
            "-g",
            &fps,
            "-c:a",
            "aac",
            "-shortest",
            "-f",
            "flv",
            url,
        ])
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg")?;
    let mut stdin = ffmpeg.stdin.take().expect("piped stdin");
    tracing::info!("restreaming to {url}");
    loop {
        frames.changed().await?;
        let jpeg = frames.borrow_and_update().clone();
        if let Some(jpeg) = jpeg {
            stdin.write_all(&jpeg).await.context("ffmpeg exited")?;
        }
    }
}
