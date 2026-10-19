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
            .then(|| settings.stream_url(&camera).map(str::to_owned))
            .flatten()
    };
    let audio_url = Feed::Audio.url(&camera);
    let mut settings = hub.settings.subscribe();
    loop {
        let url = target(&settings.borrow_and_update());
        let changed = async {
            while settings.changed().await.is_ok() {
                if target(&settings.borrow_and_update()) != url {
                    return;
                }
            }
        };
        let Some(url) = &url else {
            changed.await;
            continue;
        };
        tokio::select! {
            result = restream::stream(
                url,
                link.panorama.subscribe(),
                restream::Audio::Pcm(&audio_url),
            ) => {
                if let Err(e) = result {
                    tracing::warn!("restreaming {camera}: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            _ = changed => {}
        }
    }
}
