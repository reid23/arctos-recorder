//! Streams preview frames to the base station over a WebSocket.

use std::time::Duration;

use anyhow::Result;
use futures_util::SinkExt;
use protocol::PreviewFrame;
use tokio::sync::mpsc::Receiver;
use tokio_tungstenite::tungstenite::Message;

use crate::base::BaseStation;

pub async fn run(base: BaseStation, mut frames: Receiver<PreviewFrame<Vec<u8>>>) {
    loop {
        if let Err(e) = stream(&base, &mut frames).await {
            tracing::debug!("preview stream: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        // Don't send stale frames after reconnecting.
        while frames.try_recv().is_ok() {}
    }
}

async fn stream(base: &BaseStation, frames: &mut Receiver<PreviewFrame<Vec<u8>>>) -> Result<()> {
    let (mut socket, _) = tokio_tungstenite::connect_async(base.preview_url()).await?;
    tracing::info!("streaming previews");
    while let Some(frame) = frames.recv().await {
        socket.send(Message::binary(frame.encode())).await?;
    }
    Ok(())
}
