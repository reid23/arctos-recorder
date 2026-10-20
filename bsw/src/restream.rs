//! Restreams a camera's locally stitched live preview over RTMP. (When
//! stitching in the cloud, the cloud server restreams instead.)

use std::sync::Arc;
use std::time::Duration;

use stitching::restream;

use crate::hub::{CameraLink, Hub};
use crate::server::Feed;
use crate::settings::{Settings, Stitching};

pub async fn run(hub: Arc<Hub>, camera: String, link: Arc<CameraLink>) {
    let target = |settings: &Settings| {
        (settings.stitching == Stitching::Local)
            .then(|| settings.stream_destination(&camera).map(|destination| (destination, settings.live.bitrate)))
            .flatten()
    };
    let audio_url = Feed::Audio.url(&camera);
    let mut settings = hub.settings.subscribe();
    loop {
        let target_now = target(&settings.borrow_and_update());
        let changed = async {
            while settings.changed().await.is_ok() {
                if target(&settings.borrow_and_update()) != target_now {
                    return;
                }
            }
        };
        let Some((destination, bitrate)) = &target_now else {
            changed.await;
            continue;
        };
        tokio::select! {
            result = restream::stream(
                destination,
                *bitrate,
                link.panorama.subscribe(),
                restream::Audio::Pcm(&audio_url),
            ) => {
                if let Err(e) = result {
                    tracing::warn!("restreaming {camera}: {e:#}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
            _ = changed => {}
        }
    }
}
