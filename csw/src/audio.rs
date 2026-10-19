//! Captures the rig's microphone with ALSA.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use alsa::device_name::HintIter;
use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::Result;
use protocol::{AudioDevice, rig};
use tokio::sync::watch;

use crate::clock::Clock;
use crate::recorder::{AudioChunk, Input};

const CHUNK_SAMPLES: usize = 1024;
/// How long to wait before reopening a device that failed.
const RETRY: Duration = Duration::from_secs(10);

pub struct Outputs {
    pub recorder: Sender<Input>,
    /// Little-endian PCM for the live preview.
    pub live: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Whether audio is being captured right now.
    pub capturing: Arc<AtomicBool>,
}

/// The capture devices that could be a microphone, with ALSA's conversions
/// so any of them can deliver the rig's audio format.
pub fn devices() -> Vec<AudioDevice> {
    let hints = match HintIter::new_str(None, "pcm") {
        Ok(hints) => hints,
        Err(e) => {
            tracing::warn!("listing audio devices: {e}");
            return Vec::new();
        }
    };
    hints
        .filter(|hint| hint.direction != Some(Direction::Playback))
        .filter_map(|hint| {
            let name = hint.name.filter(|name| name.starts_with("plughw:"))?;
            let description = hint
                .desc
                .as_deref()
                .and_then(|desc| desc.lines().next())
                .unwrap_or(&name)
                .to_owned();
            Some(AudioDevice { name, description })
        })
        .collect()
}

/// Captures from the ALSA device named by `device` in the background,
/// switching whenever it changes.
pub fn spawn(mut device: watch::Receiver<String>, clock: Arc<Clock>, outputs: Outputs) {
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let mut failing = None;
            loop {
                let name = device.borrow_and_update().clone();
                let Err(error) = capture(&name, &mut device, &clock, &outputs) else {
                    failing = None;
                    continue;
                };
                outputs.capturing.store(false, Ordering::Relaxed);
                // Only log the first of repeated failures.
                if failing.as_ref() != Some(&name) {
                    tracing::warn!("audio capture from {name} failed: {error:#}");
                    failing = Some(name);
                }
                let retry = Instant::now() + RETRY;
                while Instant::now() < retry && !device.has_changed().unwrap_or(false) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .expect("spawn audio thread");
}

/// Captures until `device` changes.
fn capture(
    name: &str,
    device: &mut watch::Receiver<String>,
    clock: &Clock,
    outputs: &Outputs,
) -> Result<()> {
    let pcm = PCM::new(name, Direction::Capture, false)?;
    {
        let params = HwParams::any(&pcm)?;
        params.set_access(Access::RWInterleaved)?;
        params.set_format(Format::s16())?;
        params.set_channels(1)?;
        params.set_rate(rig::AUDIO_SAMPLE_RATE, ValueOr::Nearest)?;
        params.set_period_size_near(CHUNK_SAMPLES as _, ValueOr::Nearest)?;
        pcm.hw_params(&params)?;
    }
    let io = pcm.io_i16()?;
    pcm.start()?;
    outputs.capturing.store(true, Ordering::Relaxed);
    tracing::info!("capturing audio from {name}");

    while !device.has_changed().unwrap_or(false) {
        let mut samples = vec![0; CHUNK_SAMPLES];
        let read = match io.readi(&mut samples) {
            Ok(read) => read,
            Err(e) => {
                tracing::warn!("audio overrun: {e}");
                pcm.try_recover(e, true)?;
                continue;
            }
        };
        samples.truncate(read);
        // Samples still queued in the driver were captured after this chunk.
        let queued = pcm.delay()?.max(0) as u64;
        let age = (queued + read as u64) * 1_000_000 / rig::AUDIO_SAMPLE_RATE as u64;
        let start = clock.now() - Duration::from_micros(age);
        let pcm_bytes = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        // The live preview skips audio while it can't keep up.
        let _ = outputs.live.try_send(pcm_bytes);
        outputs
            .recorder
            .send(Input::Audio(AudioChunk { start, samples }))?;
    }
    outputs.capturing.store(false, Ordering::Relaxed);
    Ok(())
}
