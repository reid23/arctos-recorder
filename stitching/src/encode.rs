//! Encoding stitched frames, or a camera's Motion JPEG, into an MP4, and
//! decoding those MP4s back, with ffmpeg. On the GPU when this machine's
//! ffmpeg can.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use protocol::rig;
use serde::{Deserialize, Serialize};

use crate::geometry::AspectRatio;
use crate::{ffmpeg, priority};

const VAAPI_DEVICE: &str = "/dev/dri/renderD128";
/// What each size the encoder won't take is shrunk by to try the next.
const SHRINK: f64 = 0.9;
/// The narrowest video tried.
const NARROWEST: u32 = 640;

/// How finely videos are encoded: finer looks better and makes bigger files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoQuality {
    Low,
    #[default]
    Standard,
    High,
    VeryHigh,
}

impl VideoQuality {
    pub const ALL: [VideoQuality; 4] = [
        VideoQuality::Low,
        VideoQuality::Standard,
        VideoQuality::High,
        VideoQuality::VeryHigh,
    ];

    /// Steps of the encoder's quantizer finer than standard; six about
    /// double the file's size.
    fn finer_steps(self) -> i32 {
        match self {
            VideoQuality::Low => -5,
            VideoQuality::Standard => 0,
            VideoQuality::High => 3,
            VideoQuality::VeryHigh => 6,
        }
    }
}

/// The H.264 encoders tried, best first.
#[derive(Clone, Copy, Debug)]
enum Codec {
    Nvenc,
    Vaapi,
    VideoToolbox,
    Software,
}

impl Codec {
    /// The best codec that works here, found the first time it's asked for.
    fn best() -> Codec {
        static BEST: OnceLock<Codec> = OnceLock::new();
        *BEST.get_or_init(|| {
            let codec = [Codec::Nvenc, Codec::Vaapi, Codec::VideoToolbox]
                .into_iter()
                .find(|codec| codec.encodes((1280, 720)))
                .unwrap_or(Codec::Software);
            tracing::info!(?codec, "encoding video");
            codec
        })
    }

    /// Whether this codec works here for videos of `size`.
    fn encodes(self, (width, height): (u32, u32)) -> bool {
        let source = format!("color=size={width}x{height}:rate={}:duration=0.1", rig::FPS);
        let mut command = ffmpeg();
        command
            .args(self.input_args())
            .args(["-f", "lavfi", "-i", &source])
            .args(self.output_args(VideoQuality::Standard))
            .args(["-f", "null", "-"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.status().is_ok_and(|status| status.success())
    }

    fn input_args(self) -> &'static [&'static str] {
        match self {
            Codec::Vaapi => &["-vaapi_device", VAAPI_DEVICE],
            Codec::Nvenc | Codec::VideoToolbox | Codec::Software => &[],
        }
    }

    /// The value of the encoder's quality option for `quality`: a quantizer
    /// (constant quality, or rate factor), lower for finer video, except
    /// for VideoToolbox, whose quality is higher for finer video.
    fn quality_value(self, quality: VideoQuality) -> i32 {
        let finer_steps = quality.finer_steps();
        match self {
            Codec::Nvenc => 23 - finer_steps,
            Codec::Vaapi => 22 - finer_steps,
            Codec::VideoToolbox => 65 + 3 * finer_steps,
            Codec::Software => 20 - finer_steps,
        }
    }

    fn output_args(self, quality: VideoQuality) -> Vec<String> {
        let value = self.quality_value(quality).to_string();
        let args: &[&str] = match self {
            Codec::Nvenc => &[
                "-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq", "-rc", "vbr", "-cq", &value,
                "-b:v", "0",
            ],
            Codec::Vaapi => &["-vf", "format=nv12,hwupload", "-c:v", "h264_vaapi", "-qp", &value],
            Codec::VideoToolbox => &["-c:v", "h264_videotoolbox", "-allow_sw", "0", "-q:v", &value],
            Codec::Software => &[
                "-c:v", "libx264", "-preset", "veryfast", "-crf", &value, "-pix_fmt", "yuv420p",
            ],
        };
        args.iter().map(|arg| arg.to_string()).collect()
    }

    /// [`Self::output_args`] plus a scale that keeps full-range samples. The
    /// encoder's pixel format would otherwise be reached by a conversion
    /// that crushes them to limited range.
    fn full_range_output_args(self, quality: VideoQuality) -> Vec<String> {
        let mut args = self.output_args(quality);
        let range = "scale=in_range=pc:out_range=pc";
        if let Some(filter) = args.iter().position(|arg| arg == "-vf") {
            let chained = format!("{range},{}", args[filter + 1]);
            args[filter + 1] = chained;
        } else {
            args.splice(0..0, ["-vf".to_owned(), range.to_owned()]);
        }
        args
    }

    /// Output arguments encoding live video quickly, with no frames that
    /// wait for later ones; the bit rate is up to the caller.
    fn live_output_args(self) -> &'static [&'static str] {
        match self {
            Codec::Nvenc => &[
                "-c:v", "h264_nvenc", "-preset", "p4", "-tune", "ll", "-bf", "0", "-pix_fmt", "yuv420p",
            ],
            Codec::Vaapi => &["-vf", "format=nv12,hwupload", "-c:v", "h264_vaapi", "-bf", "0"],
            Codec::VideoToolbox => &[
                "-c:v", "h264_videotoolbox", "-allow_sw", "0", "-realtime", "1", "-bf", "0", "-pix_fmt",
                "yuv420p",
            ],
            Codec::Software => &[
                "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency", "-pix_fmt", "yuv420p",
            ],
        }
    }
}

/// How live video is encoded: quickly, on the GPU when this machine's
/// ffmpeg can.
pub struct LiveEncoding {
    /// ffmpeg arguments before the inputs.
    pub input_args: &'static [&'static str],
    /// ffmpeg arguments for the output's video, but for its bit rate.
    pub output_args: &'static [&'static str],
}

impl LiveEncoding {
    /// Tries the encoders the first time any encoding is asked for.
    pub async fn find() -> Result<LiveEncoding> {
        let codec = tokio::task::spawn_blocking(Codec::best).await?;
        Ok(LiveEncoding {
            input_args: codec.input_args(),
            output_args: codec.live_output_args(),
        })
    }
}

/// The largest size of `aspect_ratio`, about `width` pixels wide at most,
/// that this machine's encoder takes. Blocks while trying the encoder.
pub fn largest_size(aspect_ratio: AspectRatio, width: f64) -> Result<(u32, u32)> {
    let codec = Codec::best();
    let mut size = aspect_ratio.size_with_width(width);
    while !codec.encodes(size) {
        ensure!(size.0 > NARROWEST, "the encoder takes no video of this shape");
        size = aspect_ratio.size_with_width(size.0 as f64 * SHRINK);
    }
    Ok(size)
}

/// HD video's colors, which [`crate::gpu::Output::Video`] frames are in.
const COLOR_ARGS: [&str; 8] = [
    "-color_range",
    "tv",
    "-colorspace",
    "bt709",
    "-color_primaries",
    "bt709",
    "-color_trc",
    "bt709",
];

/// An ffmpeg process encoding frames into an MP4, giving way to others for
/// the processor.
pub struct Encoder {
    child: Child,
    input: Option<ChildStdin>,
    errors: Option<JoinHandle<String>>,
}

impl Encoder {
    /// Starts encoding planar YUV 4:2:0 frames of `size` at `fps` into
    /// `output`, along with `audio` (mono PCM at
    /// [`rig::AUDIO_SAMPLE_RATE`]) if given.
    pub fn start(
        output: &Path,
        size: (u32, u32),
        fps: u32,
        quality: VideoQuality,
        audio: Option<&Path>,
    ) -> Result<Encoder> {
        let codec = Codec::best();
        let mut command = ffmpeg();
        command
            .args(codec.input_args())
            .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
            .args(["-video_size", &format!("{}x{}", size.0, size.1)])
            .args(["-framerate", &fps.to_string()])
            .args(COLOR_ARGS)
            .args(["-i", "-"]);
        if let Some(audio) = audio {
            command
                .args(["-f", "s16le", "-ar", &rig::AUDIO_SAMPLE_RATE.to_string()])
                .args(["-ac", "1", "-i"])
                .arg(audio)
                .args(["-map", "0:v", "-map", "1:a", "-c:a", "aac", "-b:a", "128k"]);
        }
        command
            .args(codec.output_args(quality))
            .args(COLOR_ARGS)
            .args(["-movflags", "+faststart"])
            .arg(output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        priority::lower_process(&mut command);
        let mut child = command.spawn().context("starting ffmpeg")?;
        let input = child.stdin.take();
        let mut stderr = child.stderr.take().context("ffmpeg's errors")?;
        let errors = std::thread::spawn(move || {
            let mut errors = String::new();
            let _ = stderr.read_to_string(&mut errors);
            errors
        });
        Ok(Encoder {
            child,
            input,
            errors: Some(errors),
        })
    }

    pub fn write(&mut self, frame: &[u8]) -> Result<()> {
        let input = self.input.as_mut().context("the encoder is finished")?;
        if let Err(error) = input.write_all(frame) {
            self.wait()?;
            bail!("the encoder stopped taking frames: {error}");
        }
        Ok(())
    }

    /// Waits for the video to be written.
    pub fn finish(mut self) -> Result<()> {
        self.wait()
    }

    fn wait(&mut self) -> Result<()> {
        drop(self.input.take());
        let status = self.child.wait()?;
        let errors = self
            .errors
            .take()
            .and_then(|errors| errors.join().ok())
            .unwrap_or_default();
        if !status.success() {
            bail!("encoding failed ({status}): {}", errors.trim());
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if self.input.is_some() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// How often encoding reports how far along it is.
const ENCODE_PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// Encodes concatenated baseline JPEGs into an H.264 MP4.
///
/// The samples stay full-range 4:2:0, which is how the cameras recorded
/// them and how stitching reads them. `frames` is how many the recording
/// says the file holds; a different count is logged and still encoded.
pub fn encode_mjpeg(
    input: &Path,
    output: &Path,
    fps: u32,
    frames: u32,
    progress: impl Fn(f32),
) -> Result<()> {
    let result = encode_mjpeg_file(input, output, fps, frames, &progress);
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result
}

fn encode_mjpeg_file(
    input: &Path,
    output: &Path,
    fps: u32,
    frames: u32,
    progress: &impl Fn(f32),
) -> Result<()> {
    let codec = Codec::best();
    let mut command = ffmpeg();
    command
        .args(codec.input_args())
        .args(["-f", "mjpeg", "-color_range", "pc"])
        .args(["-framerate", &fps.to_string()])
        .arg("-i")
        .arg(input)
        .args(["-fps_mode", "passthrough", "-an"])
        // The panorama is encoded again at the user's quality, so this copy stays fine.
        .args(codec.full_range_output_args(VideoQuality::VeryHigh))
        .args([
            "-color_range",
            "pc",
            "-movflags",
            "+faststart",
            "-progress",
            "pipe:1",
            "-f",
            "mp4",
        ])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    priority::lower_process(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("encoding {}", input.display()))?;
    let mut stdout = child.stdout.take().context("ffmpeg's progress")?;
    let mut stderr = child.stderr.take().context("ffmpeg's errors")?;
    let logged = Arc::new(Mutex::new(String::new()));
    let logged_for_thread = Arc::clone(&logged);
    let stderr_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        *logged_for_thread.lock().unwrap() = text;
    });
    let encoded_frames = Arc::new(AtomicU64::new(0));
    let encoded_frames_for_thread = Arc::clone(&encoded_frames);
    let stdout_thread = std::thread::spawn(move || {
        let mut buffer = String::new();
        let mut chunk = [0u8; 512];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(size) => {
                    buffer.push_str(&String::from_utf8_lossy(&chunk[..size]));
                    note_encoded_frames(&mut buffer, &encoded_frames_for_thread);
                }
            }
        }
    });
    let mut encoding = EncodeChild {
        child,
        stdout: Some(stdout_thread),
        stderr: Some(stderr_thread),
    };
    let status = loop {
        match encoding.child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let done = encoded_frames.load(Ordering::Relaxed) as f32;
                let portion = done / (frames.max(1) as f32);
                progress(portion.min(1.0));
                std::thread::sleep(ENCODE_PROGRESS_INTERVAL);
            }
            Err(error) => return Err(error).context("watching ffmpeg"),
        }
    };
    encoding.join_pipes();
    if !status.success() {
        let errors = logged.lock().unwrap().clone();
        bail!("encoding failed ({status}): {}", errors.trim());
    }
    let encoded = encoded_frames.load(Ordering::Relaxed);
    if encoded > 0 && encoded != u64::from(frames) {
        tracing::warn!(
            file = %input.display(),
            encoded,
            expected = frames,
            "encoded a different number of frames than the recording says"
        );
    }
    progress(1.0);
    Ok(())
}

fn note_encoded_frames(buffer: &mut String, frames: &AtomicU64) {
    while let Some(newline) = buffer.find('\n') {
        let count = buffer[..newline]
            .trim()
            .strip_prefix("frame=")
            .and_then(|value| value.trim().parse::<u64>().ok());
        buffer.drain(..=newline);
        if let Some(count) = count {
            frames.store(count, Ordering::Relaxed);
        }
    }
}

struct EncodeChild {
    child: Child,
    stdout: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

impl EncodeChild {
    fn join_pipes(&mut self) {
        if let Some(thread) = self.stdout.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for EncodeChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        self.join_pipes();
        let _ = self.child.wait();
    }
}

/// Decodes a pair of full-range H.264 MP4s into planar YUV 4:2:0.
///
/// A side that runs short repeats its last frame so the pair stays in time.
/// Extra frames past `frames` are ignored.
pub struct Mp4Frames {
    sides: [DecodedVideo; 2],
    remaining: u32,
    expected: u32,
    read: u32,
    have_pair: bool,
    short: bool,
    label: String,
}

impl Mp4Frames {
    pub fn open(files: [PathBuf; 2], frames: u32, size: (u32, u32), label: &str) -> Result<Mp4Frames> {
        Ok(Mp4Frames {
            sides: [
                DecodedVideo::open(&files[0], size)?,
                DecodedVideo::open(&files[1], size)?,
            ],
            remaining: frames,
            expected: frames,
            read: 0,
            have_pair: false,
            short: false,
            label: label.to_owned(),
        })
    }

    /// The next pair of frames, valid until the next call, or `None` once
    /// `frames` pairs have been returned.
    pub fn next_pair(&mut self) -> Result<Option<[&[u8]; 2]>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        if !self.short {
            let left = self.sides[0].read_frame()?;
            let right = self.sides[1].read_frame()?;
            if left && right {
                self.sides[0].commit();
                self.sides[1].commit();
                self.read += 1;
                self.have_pair = true;
            } else if self.have_pair {
                self.short = true;
                tracing::warn!(
                    segment = %self.label,
                    got = self.read,
                    expected = self.expected,
                    "the video runs short; repeating its last frames"
                );
            } else {
                let errors = format!(
                    "{}\n{}",
                    self.sides[0].take_errors(),
                    self.sides[1].take_errors()
                );
                bail!("{} has no frames: {}", self.label, errors.trim());
            }
        }
        let [left, right] = &self.sides;
        Ok(Some([&left.last, &right.last]))
    }

    /// Logs when the files hold more frames than were asked for.
    pub fn finish(mut self) -> Result<()> {
        if !self.short && self.have_pair {
            let extra = self.sides[0].read_frame()? || self.sides[1].read_frame()?;
            if extra {
                tracing::warn!(
                    segment = %self.label,
                    expected = self.expected,
                    "the video has more frames than the recording says; the extras are ignored"
                );
            }
        }
        Ok(())
    }
}

struct DecodedVideo {
    child: Child,
    stdout: ChildStdout,
    errors: Arc<Mutex<String>>,
    errors_thread: Option<JoinHandle<()>>,
    slot: Vec<u8>,
    last: Vec<u8>,
}

impl DecodedVideo {
    fn open(path: &Path, size: (u32, u32)) -> Result<DecodedVideo> {
        ensure!(path.is_file(), "missing {}", path.display());
        let frame_bytes = (size.0 * size.1 * 3 / 2) as usize;
        ensure!(frame_bytes > 0, "a frame has no pixels");
        let mut command = ffmpeg();
        command
            .arg("-i")
            .arg(path)
            .args([
                "-fps_mode",
                "passthrough",
                "-an",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "-color_range",
                "pc",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        priority::lower_process(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("decoding {}", path.display()))?;
        let stdout = child.stdout.take().context("ffmpeg's output")?;
        let mut stderr = child.stderr.take().context("ffmpeg's errors")?;
        let errors = Arc::new(Mutex::new(String::new()));
        let errors_for_thread = Arc::clone(&errors);
        let errors_thread = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            *errors_for_thread.lock().unwrap() = text;
        });
        Ok(DecodedVideo {
            child,
            stdout,
            errors,
            errors_thread: Some(errors_thread),
            slot: vec![0; frame_bytes],
            last: vec![0; frame_bytes],
        })
    }

    fn read_frame(&mut self) -> Result<bool> {
        match self.stdout.read_exact(&mut self.slot) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
            Err(error) => {
                let errors = self.take_errors();
                Err(error).with_context(|| errors)
            }
        }
    }

    fn commit(&mut self) {
        std::mem::swap(&mut self.slot, &mut self.last);
    }

    fn take_errors(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.errors_thread.take() {
            let _ = thread.join();
        }
        std::mem::take(&mut *self.errors.lock().unwrap())
    }
}

impl Drop for DecodedVideo {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if let Some(thread) = self.errors_thread.take() {
            let _ = thread.join();
        }
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_quality_sets_each_encoders_quantizer() {
        let quantizer = |codec: Codec, option: &str, quality| {
            let args = codec.output_args(quality);
            let position = args.iter().position(|arg| arg == option).unwrap();
            args[position + 1].parse::<i32>().unwrap()
        };
        for (codec, option) in [(Codec::Nvenc, "-cq"), (Codec::Vaapi, "-qp"), (Codec::Software, "-crf")] {
            let quantizers = VideoQuality::ALL.map(|quality| quantizer(codec, option, quality));
            assert_eq!(quantizers[1], codec.quality_value(VideoQuality::Standard));
            assert!(quantizers.is_sorted_by(|coarser, finer| coarser > finer), "{codec:?}: {quantizers:?}");
        }
        assert_eq!(quantizer(Codec::Software, "-crf", VideoQuality::VeryHigh), 14);
        let qualities = VideoQuality::ALL.map(|quality| quantizer(Codec::VideoToolbox, "-q:v", quality));
        assert!(qualities.is_sorted_by(|coarser, finer| coarser < finer), "{qualities:?}");
    }
}
