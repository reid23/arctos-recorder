//! Stitches a base station's forwarded live previews and restreams them.

use std::net::Ipv4Addr;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arctos::Arctos;
use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use stitching::browser::Chromium;
use stitching::cloud::LiveConfig;
use stitching::geometry::StitchConfig;
use stitching::gpu::GpuChoice;
use stitching::live::{LiveStitcher, StereoYuv};
use stitching::overlay::{self, PlacedImage};
use stitching::restream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{ChildStdout, Command};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};

/// Chunks of the forwarded stream held for the restream before it connects
/// or when it falls behind.
const STREAM_COPY_BACKLOG: usize = 64;

/// Serves one connection of the live WebSocket until either side stops.
pub async fn run(mut socket: WebSocket, arctos: &Arctos) -> Result<()> {
    let Some(Ok(Message::Text(config))) = socket.recv().await else {
        bail!("expected the live config");
    };
    let LiveConfig {
        stitch,
        preview_size,
        size,
        destination,
        bitrate,
        overlay,
    } = serde_json::from_str(&config)?;
    let (stitch, stitch_rx) = watch::channel(stitch);

    let mut decoder = Command::new(stitching::ffmpeg_program())
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
        .args(["-map", "0:v:0", "-f", "rawvideo", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg")?;
    let mut stream = decoder.stdin.take().expect("piped stdin");
    let frames = decoder.stdout.take().expect("piped stdout");

    // The restream takes its audio from a copy of the forwarded stream.
    let audio_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let audio_url = format!("tcp://{}", audio_listener.local_addr()?);
    let (stream_copy, mut stream_copy_rx) = mpsc::channel::<Bytes>(STREAM_COPY_BACKLOG);
    let feed_audio = async {
        let (mut connection, _) = audio_listener.accept().await?;
        while let Some(chunk) = stream_copy_rx.recv().await {
            connection.write_all(&chunk).await?;
        }
        anyhow::Ok(())
    };

    let (panoramas, panoramas_rx) = watch::channel(None);
    let chromium = Chromium::installed();
    // Never changes during the connection.
    let (_overlay_config, overlay_config) = watch::channel(overlay.map(|overlay| (overlay, size)));
    let (overlay_images, overlay_images_rx) = watch::channel(None);
    let runtime = Handle::current();
    let stitcher = tokio::task::spawn_blocking(move || {
        stitch_previews(
            &runtime,
            frames,
            stitch_rx,
            preview_size,
            size,
            &overlay_images_rx,
            &panoramas,
        )
    });
    let forward = async {
        while let Some(message) = socket.recv().await {
            match message? {
                Message::Binary(chunk) => {
                    stream.write_all(&chunk).await?;
                    let _ = stream_copy.try_send(chunk);
                }
                Message::Text(config) => {
                    stitch.send_replace(serde_json::from_str(&config)?);
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        anyhow::Ok(())
    };
    tokio::select! {
        result = forward => result,
        result = stitcher => result?,
        result = restream::stream(&destination, bitrate, panoramas_rx, restream::Audio::MpegTs(&audio_url)) => result,
        result = feed_audio => result,
        () = overlay::follow_live(arctos, &chromium, overlay_config, &overlay_images) => {
            unreachable!("the overlay config is still open")
        }
    }
}

fn stitch_previews(
    runtime: &Handle,
    mut frames: ChildStdout,
    mut config: watch::Receiver<StitchConfig>,
    preview_size: (u32, u32),
    size: (u32, u32),
    overlay: &watch::Receiver<Option<Arc<PlacedImage>>>,
    panoramas: &watch::Sender<Option<bytes::Bytes>>,
) -> Result<()> {
    let mut stitcher = LiveStitcher::new(
        &config.borrow_and_update(),
        preview_size,
        size,
        GpuChoice::Discrete,
    )?;
    let mut frame = StereoYuv::new(preview_size);
    loop {
        runtime
            .block_on(frames.read_exact(frame.bytes_mut()))
            .context("decoder stopped")?;
        if config.has_changed()?
            && let Err(e) = stitcher.configure(&config.borrow_and_update())
        {
            tracing::warn!("keeping the last stitching config: {e:#}");
        }
        let overlay = overlay.borrow().clone();
        let jpeg = stitcher.stitch(&frame, overlay.as_ref())?;
        panoramas.send_replace(Some(jpeg));
    }
}
