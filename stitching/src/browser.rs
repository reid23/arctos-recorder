//! Screenshots of a live web page with transparency, taken by headless
//! Chromium (what OBS browser sources use) over the Chrome DevTools
//! Protocol.
//!
//! Chromium is `$CHROMIUM` or the first of [`CHROMIUM_NAMES`] on the `PATH`.
//! Extra command line flags can be given in `$CHROMIUM_FLAGS`, e.g.
//! `--no-sandbox` when running as root in a container.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const CHROMIUM_NAMES: [&str; 4] = [
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
];
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// One page open in its own headless Chromium.
pub struct Page {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    session: String,
    next_id: u64,
    chromium: Child,
    profile: PathBuf,
}

impl Page {
    /// Opens `url`, laid out `size` CSS pixels and rendered at `scale`
    /// device pixels per CSS pixel, sending `headers` with every request.
    pub async fn open(
        url: &str,
        headers: &[(String, String)],
        size: (u32, u32),
        scale: f64,
    ) -> Result<Self> {
        static PAGES_OPENED: AtomicU64 = AtomicU64::new(0);
        let profile = std::env::temp_dir().join(format!(
            "arctos-recorder-chromium-{}-{}",
            std::process::id(),
            PAGES_OPENED.fetch_add(1, Ordering::Relaxed)
        ));
        let mut command = Command::new(chromium()?);
        command
            .args([
                "--headless=new",
                "--hide-scrollbars",
                "--mute-audio",
                "--no-first-run",
                "--no-default-browser-check",
                "--remote-debugging-port=0",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .args(
                std::env::var("CHROMIUM_FLAGS")
                    .unwrap_or_default()
                    .split_whitespace(),
            )
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut chromium = command.spawn().context("starting Chromium")?;
        let mut stderr = BufReader::new(chromium.stderr.take().expect("piped stderr")).lines();
        let endpoint = tokio::time::timeout(STARTUP_TIMEOUT, async {
            while let Some(line) = stderr.next_line().await? {
                if let Some(endpoint) = line.strip_prefix("DevTools listening on ") {
                    return Ok(endpoint.to_owned());
                }
            }
            bail!("Chromium exited while starting")
        })
        .await
        .context("Chromium didn't start")??;
        // Chromium blocks once the pipe is full.
        tokio::spawn(async move { while let Ok(Some(_)) = stderr.next_line().await {} });

        let (socket, _) = tokio_tungstenite::connect_async(endpoint).await?;
        let mut page = Page {
            socket,
            session: String::new(),
            next_id: 0,
            chromium,
            profile,
        };
        let target = page
            .call("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let attached = page
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target["targetId"], "flatten": true }),
            )
            .await?;
        page.session = attached["sessionId"]
            .as_str()
            .context("no session id")?
            .to_owned();
        page.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": size.0, "height": size.1, "deviceScaleFactor": scale, "mobile": false }),
        )
        .await?;
        page.call(
            "Emulation.setDefaultBackgroundColorOverride",
            json!({ "color": { "r": 0, "g": 0, "b": 0, "a": 0 } }),
        )
        .await?;
        let headers: serde_json::Map<String, Value> = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone().into()))
            .collect();
        page.call("Network.enable", json!({})).await?;
        page.call("Network.setExtraHTTPHeaders", json!({ "headers": headers }))
            .await?;
        page.call("Page.navigate", json!({ "url": url })).await?;
        Ok(page)
    }

    /// The page as it is now, as a PNG.
    pub async fn screenshot(&mut self) -> Result<Vec<u8>> {
        let result = self
            .call("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;
        Ok(STANDARD.decode(result["data"].as_str().context("no screenshot")?)?)
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if !self.session.is_empty() {
            message["sessionId"] = self.session.clone().into();
        }
        self.socket.send(Message::text(message.to_string())).await?;
        while let Some(message) = self.socket.next().await {
            let Message::Text(text) = message? else {
                continue;
            };
            let mut response: Value = serde_json::from_str(&text)?;
            if response["id"] != id {
                continue;
            }
            if let Some(error) = response.get("error") {
                bail!("{method} failed: {error}");
            }
            return Ok(response["result"].take());
        }
        bail!("Chromium closed the DevTools connection")
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        // Chromium writes to its profile until it exits.
        let _ = self.chromium.start_kill();
        for _ in 0..50 {
            if !matches!(self.chromium.try_wait(), Ok(None)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn chromium() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CHROMIUM") {
        return Ok(path.into());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .flat_map(|dir| CHROMIUM_NAMES.map(|name| dir.join(name)))
        .find(|candidate| candidate.is_file())
        .context("Chromium isn't installed (or set $CHROMIUM to its path)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::RgbaImage;

    #[tokio::test]
    #[ignore = "needs Chromium"]
    async fn screenshots_keep_transparency() {
        let page = "data:text/html,<body style='margin:0'>\
            <div style='width:50px;height:20px;background:rgba(255,0,0,0.5)'></div>";
        let mut page = Page::open(page, &[], (100, 40), 2.0).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let image = RgbaImage::decode_png(&page.screenshot().await.unwrap()).unwrap();
        assert_eq!((image.width, image.height), (200, 80));
        let pixel = |x: usize, y: usize| &image.pixels[(y * 200 + x) * 4..][..4];
        assert_eq!(pixel(10, 10)[0], 255);
        assert!((pixel(10, 10)[3] as i32 - 128).abs() <= 1);
        assert_eq!(pixel(150, 60)[3], 0);
    }
}
