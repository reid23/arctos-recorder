//! Captures the rig's microphone with ALSA.

use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::Result;
use protocol::rig;

use crate::clock::Clock;
use crate::recorder::{AudioChunk, Input};

const CHUNK_SAMPLES: usize = 1024;

/// Starts capturing from the ALSA `device` in the background. Audio failures
/// terminate the process (systemd restarts it).
pub fn spawn(device: String, clock: Arc<Clock>, output: Sender<Input>) {
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let error = capture(&device, &clock, &output).unwrap_err();
            tracing::error!("audio capture from {device} failed: {error:#}");
            std::process::exit(1);
        })
        .expect("spawn audio thread");
}

fn capture(device: &str, clock: &Clock, output: &Sender<Input>) -> Result<()> {
    let pcm = PCM::new(device, Direction::Capture, false)?;
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
    tracing::info!("capturing audio from {device}");

    loop {
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
        output.send(Input::Audio(AudioChunk { start, samples }))?;
    }
}
