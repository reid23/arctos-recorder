//! Encoding stitched frames into an MP4, or live video, with ffmpeg, on the
//! GPU when this machine's ffmpeg can.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::OnceLock;
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail, ensure};
use protocol::rig;
use serde::{Deserialize, Serialize};

use crate::ffmpeg;
use crate::geometry::AspectRatio;

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

/// An ffmpeg process encoding frames into an MP4.
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
