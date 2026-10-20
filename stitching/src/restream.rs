//! Restreaming a stitched live preview over RTMP (e.g. to YouTube Live)
//! using `ffmpeg`.

use std::collections::VecDeque;
use std::process::{ExitStatus, Stdio};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use protocol::rig;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::encode::LiveEncoding;
use crate::{ffmpeg_program, live};

/// How many of ffmpeg's last error lines say why it stopped.
const ERROR_LINES: usize = 5;

/// An RTMP server and the stream key it gave, kept apart so the key stays
/// out of logs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Destination {
    pub server: String,
    /// Empty if the server's URL needs none.
    pub key: String,
}

impl Destination {
    fn url(&self) -> String {
        if self.key.is_empty() {
            self.server.clone()
        } else {
            format!("{}/{}", self.server.trim_end_matches('/'), self.key)
        }
    }

    fn redact(&self, text: &str) -> String {
        if self.key.is_empty() {
            text.to_owned()
        } else {
            text.replace(&self.key, "<stream key>")
        }
    }
}

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

/// ffmpeg arguments holding the video to `bitrate` megabits a second.
fn bitrate_args(bitrate: f64) -> [String; 6] {
    let kilobits = (bitrate * 1000.0).round() as u64;
    [
        "-b:v".to_owned(),
        format!("{kilobits}k"),
        "-maxrate".to_owned(),
        format!("{kilobits}k"),
        "-bufsize".to_owned(),
        format!("{}k", 2 * kilobits),
    ]
}

/// Streams the stitched preview JPEGs, with `audio`, to `destination` at
/// `bitrate` megabits a second until `frames` closes or ffmpeg fails, or
/// (returning `Ok`) until they change size, which the stream can't, so it
/// must be started again.
pub async fn stream(
    destination: &Destination,
    bitrate: f64,
    mut frames: watch::Receiver<Option<Bytes>>,
    audio: Audio<'_>,
) -> Result<()> {
    let fps = live::FPS.to_string();
    let encoding = LiveEncoding::find().await?;
    let mut ffmpeg = Command::new(ffmpeg_program())
        .args(["-loglevel", "error"])
        .args(encoding.input_args)
        .args(["-use_wallclock_as_timestamps", "1", "-f", "mjpeg", "-i", "-"])
        .args(audio.input_args())
        .args(["-map", "0:v"])
        .args(encoding.output_args)
        .args(["-r", &fps, "-g", &fps])
        .args(bitrate_args(bitrate))
        .args(AUDIO_OUTPUT_ARGS)
        .args(["-f", "flv"])
        .arg(destination.url())
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg")?;
    let mut stdin = ffmpeg.stdin.take().expect("piped stdin");
    let errors = tokio::spawn(last_lines(ffmpeg.stderr.take().expect("piped stderr")));
    tracing::info!("restreaming to {}", destination.server);
    let mut size = None;
    loop {
        tokio::select! {
            status = ffmpeg.wait() => return Err(failure(destination, status, errors).await),
            changed = frames.changed() => changed?,
        }
        let jpeg = frames.borrow_and_update().clone();
        if let Some(jpeg) = jpeg {
            let header = turbojpeg::read_header(&jpeg).context("reading a preview's size")?;
            let this = (header.width, header.height);
            if *size.get_or_insert(this) != this {
                tracing::info!("the preview changed size; restarting the restream");
                return Ok(());
            }
            if stdin.write_all(&jpeg).await.is_err() {
                return Err(failure(destination, ffmpeg.wait().await, errors).await);
            }
        }
    }
}

/// Why ffmpeg stopped: what it last wrote to stderr, or else how it exited.
async fn failure(
    destination: &Destination,
    status: std::io::Result<ExitStatus>,
    errors: JoinHandle<VecDeque<String>>,
) -> anyhow::Error {
    let errors = Vec::from(errors.await.unwrap_or_default());
    if errors.is_empty() {
        match status {
            Ok(status) => anyhow!("ffmpeg stopped ({status})"),
            Err(e) => anyhow!("ffmpeg stopped: {e}"),
        }
    } else {
        anyhow!("ffmpeg failed: {}", destination.redact(&errors.join(" / ")))
    }
}

/// The last [`ERROR_LINES`] lines ffmpeg wrote to `stderr` before closing it.
async fn last_lines(stderr: ChildStderr) -> VecDeque<String> {
    let mut lines = BufReader::new(stderr).lines();
    let mut last = VecDeque::with_capacity(ERROR_LINES);
    while let Ok(Some(line)) = lines.next_line().await {
        if last.len() == ERROR_LINES {
            last.pop_front();
        }
        last.push_back(line);
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_holds_the_stream_to_it() {
        assert_eq!(bitrate_args(4.5), ["-b:v", "4500k", "-maxrate", "4500k", "-bufsize", "9000k"]);
    }

    #[test]
    fn the_key_ends_the_url_but_not_what_is_logged() {
        let destination = Destination {
            server: "rtmp://a.rtmp.youtube.com/live2/".to_owned(),
            key: "abcd-1234".to_owned(),
        };
        assert_eq!(destination.url(), "rtmp://a.rtmp.youtube.com/live2/abcd-1234");
        assert_eq!(
            destination.redact(&format!("Error opening output {}: Input/output error", destination.url())),
            "Error opening output rtmp://a.rtmp.youtube.com/live2/<stream key>: Input/output error"
        );
        let keyless = Destination {
            server: "rtmp://example.org/live/stream".to_owned(),
            key: String::new(),
        };
        assert_eq!(keyless.url(), "rtmp://example.org/live/stream");
        assert_eq!(keyless.redact("Connection refused"), "Connection refused");
    }
}
