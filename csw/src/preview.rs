//! Streams preview frames and microphone audio to the base station over a
//! WebSocket.

use std::time::Duration;

use anyhow::Result;
use futures_util::SinkExt;
use protocol::{PreviewFrame, PreviewMessage};
use tokio::sync::mpsc::Receiver;
use tokio_tungstenite::tungstenite::Message;

use crate::base::BaseStation;

pub struct Inputs {
    pub frames: Receiver<PreviewFrame<Vec<u8>>>,
    /// Little-endian PCM.
    pub audio: Receiver<Vec<u8>>,
}

pub async fn run(base: BaseStation, mut inputs: Inputs) {
    let mut registrations = base.registrations();
    loop {
        registrations.mark_unchanged();
        tokio::select! {
            result = stream(&base, &mut inputs) => {
                if let Err(e) = result {
                    tracing::debug!("preview stream: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            _ = registrations.changed() => {}
        }
        // Don't send stale frames or audio after reconnecting.
        while inputs.frames.try_recv().is_ok() {}
        while inputs.audio.try_recv().is_ok() {}
    }
}

async fn stream(base: &BaseStation, inputs: &mut Inputs) -> Result<()> {
    let (mut socket, _) = tokio_tungstenite::connect_async(base.preview_url()).await?;
    tracing::info!("streaming previews");
    loop {
        let message = tokio::select! {
            Some(frame) = inputs.frames.recv() => PreviewMessage::Frame(frame),
            Some(pcm) = inputs.audio.recv() => PreviewMessage::Audio(pcm),
            else => return Ok(()),
        };
        socket.send(Message::binary(message.encode())).await?;
    }
}
