//! Restreaming a stitched live preview over RTMP (e.g. to YouTube Live)
//! using `ffmpeg`.

use std::process::Stdio;

use anyhow::{Context, Result};
use bytes::Bytes;
use protocol::rig;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::watch;

use crate::live;

/// Where ffmpeg reads the stream's audio from.
pub enum Audio<'a> {
    /// Raw PCM (see [`rig::AUDIO_SAMPLE_RATE`]) served in real time at a URL.
    Pcm(&'a str),
    /// The audio track, if any, of an MPEG-TS stream at a URL.
    MpegTs(&'a str),
}

impl Audio<'_> {
    /// ffmpeg arguments for this input. Timestamps come from the clock, as
    /// for the video, so the two line up.
    pub fn input_args(&self) -> Vec<String> {
        let mut args = vec!["-use_wallclock_as_timestamps".to_owned(), "1".to_owned()];
        let (format, url) = match self {
            Audio::Pcm(url) => {
                args.extend(
                    ["-ar", &rig::AUDIO_SAMPLE_RATE.to_string(), "-ac", "1"].map(String::from),
                );
                ("s16le", url)
            }
            Audio::MpegTs(url) => ("mpegts", url),
        };
        args.extend(["-f", format, "-i", url].map(String::from));
        args
    }
}

/// ffmpeg arguments encoding the second input's audio, resampled to keep
/// up with the clock.
pub const AUDIO_OUTPUT_ARGS: [&str; 8] = [
    "-map",
    "1:a?",
    "-c:a",
    "aac",
    "-af",
    "aresample=async=1",
    "-b:a",
    "128k",
];

/// Streams the stitched preview JPEGs, with `audio`, to `url` until
/// `frames` closes or ffmpeg fails.
pub async fn stream(
    url: &str,
    mut frames: watch::Receiver<Option<Bytes>>,
    audio: Audio<'_>,
) -> Result<()> {
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
        .args(audio.input_args())
        .args([
            "-map",
            "0:v",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-r",
            &fps,
            "-g",
            &fps,
        ])
        .args(AUDIO_OUTPUT_ARGS)
        .args(["-f", "flv", url])
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
