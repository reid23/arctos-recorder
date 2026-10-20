//! Screenshots of a live web page with transparency, taken by headless
//! Chromium (what OBS browser sources use) over the Chrome DevTools
//! Protocol.
//!
//! Chromium is `$CHROMIUM`, an installed Chromium-based browser, or Chrome's
//! headless shell downloaded when neither is there (see [`Chromium`]). Extra
//! command line flags can be given in `$CHROMIUM_FLAGS`, e.g.
//! `--no-sandbox` when running as root in a container.

use std::io::{BufRead, BufReader, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{OnceCell, mpsc};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// The version of Chrome's headless shell downloaded when no browser is
/// installed.
const HEADLESS_SHELL_VERSION: &str = "155.0.8059.84";

/// Where to find Chromium: `$CHROMIUM`, else an installed Chromium-based
/// browser, else, if there's a directory to keep it in, Chrome's headless
/// shell, downloaded the first time it's needed.
pub struct Chromium {
    downloads: Option<PathBuf>,
    executable: OnceCell<PathBuf>,
}

impl Chromium {
    pub fn installed() -> Self {
        Chromium {
            downloads: None,
            executable: OnceCell::new(),
        }
    }

    pub fn installed_or_downloaded_into(downloads: PathBuf) -> Self {
        Chromium {
            downloads: Some(downloads),
            executable: OnceCell::new(),
        }
    }

    async fn executable(&self) -> Result<&Path> {
        let executable = self
            .executable
            .get_or_try_init(async || {
                if let Some(path) = std::env::var_os("CHROMIUM") {
                    return Ok(path.into());
                }
                if let Some(installed) = installed_browser() {
                    return Ok(installed);
                }
                match &self.downloads {
                    Some(downloads) => download_headless_shell(downloads)
                        .await
                        .context("downloading Chrome's headless shell"),
                    None => bail!("Chromium isn't installed (or set $CHROMIUM to its path)"),
                }
            })
            .await?;
        Ok(executable)
    }
}

/// The first Chromium-based browser installed in a usual place.
fn installed_browser() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(target_os = "windows") {
        let folders = ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        [
            r"Microsoft\Edge\Application\msedge.exe",
            r"Google\Chrome\Application\chrome.exe",
            r"Chromium\Application\chrome.exe",
        ]
        .into_iter()
        .flat_map(|browser| folders.iter().map(move |folder| folder.join(browser)))
        .collect()
    } else if cfg!(target_os = "macos") {
        let folders = [Some(PathBuf::from("/Applications")), std::env::home_dir().map(|home| home.join("Applications"))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Chromium.app/Contents/MacOS/Chromium",
        ]
        .into_iter()
        .flat_map(|browser| folders.iter().map(move |folder| folder.join(browser)))
        .collect()
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .flat_map(|folder| {
                [
                    "chromium",
                    "chromium-browser",
                    "google-chrome",
                    "google-chrome-stable",
                    "microsoft-edge",
                    "microsoft-edge-stable",
                ]
                .map(|name| folder.join(name))
            })
            .collect()
    };
    /// Snaps (and Ubuntu's scripts that launch them) are left to the
    /// downloaded headless shell: their confinement makes them unreliable to
    /// drive this way.
    fn is_snap(browser: &Path) -> bool {
        let real = std::fs::canonicalize(browser).unwrap_or_default();
        if real.starts_with("/snap") || real == Path::new("/usr/bin/snap") {
            return true;
        }
        let mut start = Vec::new();
        let read = std::fs::File::open(browser).and_then(|file| file.take(4096).read_to_end(&mut start));
        read.is_ok() && start.starts_with(b"#!") && start.windows(6).any(|window| window == b"/snap/")
    }
    candidates.into_iter().find(|candidate| candidate.is_file() && !is_snap(candidate))
}

/// Downloads Chrome's headless shell into `downloads` unless it is there
/// already, removing other versions; returns its executable.
async fn download_headless_shell(downloads: &Path) -> Result<PathBuf> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("windows", "x86_64") => "win64",
        (os, arch) => bail!("it isn't available for {os} on {arch}"),
    };
    let unpacked = downloads.join(HEADLESS_SHELL_VERSION);
    let executable = unpacked
        .join(format!("chrome-headless-shell-{platform}"))
        .join(format!("chrome-headless-shell{}", std::env::consts::EXE_SUFFIX));
    if executable.is_file() {
        return Ok(executable);
    }

    tracing::info!(version = HEADLESS_SHELL_VERSION, "downloading Chrome's headless shell");
    tokio::fs::create_dir_all(downloads).await?;
    let url = format!(
        "https://storage.googleapis.com/chrome-for-testing-public/{HEADLESS_SHELL_VERSION}/{platform}/chrome-headless-shell-{platform}.zip"
    );
    let archive_path = downloads.join(format!("{HEADLESS_SHELL_VERSION}.zip"));
    let mut archive = tokio::fs::File::create(&archive_path).await?;
    let mut body = reqwest::get(url).await?.error_for_status()?.bytes_stream();
    while let Some(chunk) = body.next().await {
        archive.write_all(&chunk?).await?;
    }
    archive.flush().await?;
    drop(archive);

    let unpacking = downloads.join(format!("{HEADLESS_SHELL_VERSION}.partial"));
    let _ = tokio::fs::remove_dir_all(&unpacking).await;
    let archive_to_unpack = archive_path.clone();
    let unpacked_into = unpacking.clone();
    tokio::task::spawn_blocking(move || {
        zip::ZipArchive::new(std::fs::File::open(&archive_to_unpack)?)?.extract(&unpacked_into)?;
        anyhow::Ok(())
    })
    .await??;
    tokio::fs::rename(&unpacking, &unpacked).await?;
    tokio::fs::remove_file(&archive_path).await?;

    let mut entries = tokio::fs::read_dir(downloads).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.path() != unpacked {
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
    tracing::info!(version = HEADLESS_SHELL_VERSION, "downloaded Chrome's headless shell");
    Ok(executable)
}

/// One page open in its own headless Chromium, which exits by itself once
/// the page's end of the DevTools pipes closes, however this program ends.
pub struct Page {
    commands: std::sync::mpsc::Sender<Vec<u8>>,
    messages: mpsc::UnboundedReceiver<serde_json::Result<Value>>,
    session: String,
    next_id: u64,
    chromium: Child,
    profile: PathBuf,
}

impl Page {
    /// Opens `url`, laid out `size` CSS pixels and rendered at `scale`
    /// device pixels per CSS pixel, sending `headers` with every request.
    pub async fn open(
        chromium: &Chromium,
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
        let (chromium_reads, commands_pipe) = std::io::pipe()?;
        let (messages_pipe, chromium_writes) = std::io::pipe()?;
        let mut command = Command::new(chromium.executable().await?);
        command
            .args([
                "--headless=new",
                "--hide-scrollbars",
                "--mute-audio",
                "--no-first-run",
                "--no-default-browser-check",
                "--remote-debugging-pipe",
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
            .stderr(Stdio::null())
            .kill_on_drop(true);
        pass_pipes(&mut command, &chromium_reads, &chromium_writes)?;
        let chromium = command.spawn().context("starting Chromium")?;
        // Only Chromium may hold its ends, so each side sees the other close.
        drop((command, chromium_reads, chromium_writes));

        let (commands, queued_commands) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut pipe = commands_pipe;
            for command in queued_commands {
                if pipe.write_all(&command).is_err() {
                    return;
                }
            }
        });
        let (message_sender, messages) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut pipe = BufReader::new(messages_pipe);
            let mut message = Vec::new();
            while matches!(pipe.read_until(0, &mut message), Ok(read) if read > 0) {
                let json = message.strip_suffix(&[0]).unwrap_or(&message);
                if message_sender.send(serde_json::from_slice(json)).is_err() {
                    return;
                }
                message.clear();
            }
        });

        let mut page = Page {
            commands,
            messages,
            session: String::new(),
            next_id: 0,
            chromium,
            profile,
        };
        let target = tokio::time::timeout(
            STARTUP_TIMEOUT,
            page.call("Target.createTarget", json!({ "url": "about:blank" })),
        )
        .await
        .context("Chromium didn't start")??;
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
        tokio::time::timeout(LOAD_TIMEOUT, page.navigate(url))
            .await
            .context("the page didn't load")??;
        Ok(page)
    }

    /// Navigates to `url`, returning once it has loaded: until then there's
    /// no page to take screenshots of.
    async fn navigate(&mut self, url: &str) -> Result<()> {
        self.call("Page.enable", json!({})).await?;
        self.call("Page.setLifecycleEventsEnabled", json!({ "enabled": true }))
            .await?;
        let id = self.send("Page.navigate", json!({ "url": url }))?;
        // The documents that have loaded and the one navigated to, by loader
        // id, as the load may be reported before the navigation.
        let mut loaded = Vec::new();
        let mut navigated = None;
        loop {
            let mut message = self.receive().await?;
            if message["id"] == id {
                let mut result = result("Page.navigate", message)?;
                if let Some(error) = result["errorText"].as_str() {
                    bail!("loading the page failed: {error}");
                }
                navigated = Some(result["loaderId"].take());
            } else if message["method"] == "Page.lifecycleEvent" && message["params"]["name"] == "load" {
                loaded.push(message["params"]["loaderId"].take());
            }
            if navigated.as_ref().is_some_and(|loader| loaded.contains(loader)) {
                return Ok(());
            }
        }
    }

    /// The page as it is now, as a PNG.
    pub async fn screenshot(&mut self) -> Result<Vec<u8>> {
        let result = self
            .call("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;
        Ok(STANDARD.decode(result["data"].as_str().context("no screenshot")?)?)
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send(method, params)?;
        loop {
            let message = self.receive().await?;
            if message["id"] == id {
                return result(method, message);
            }
        }
    }

    /// Sends a command, returning its id.
    fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        self.next_id += 1;
        let mut message = json!({ "id": self.next_id, "method": method, "params": params });
        if !self.session.is_empty() {
            message["sessionId"] = self.session.clone().into();
        }
        let mut bytes = message.to_string().into_bytes();
        bytes.push(0);
        self.commands
            .send(bytes)
            .map_err(|_| anyhow!("Chromium closed the DevTools pipe"))?;
        Ok(self.next_id)
    }

    /// The next response or event.
    async fn receive(&mut self) -> Result<Value> {
        match self.messages.recv().await {
            Some(message) => Ok(message?),
            None => bail!("Chromium closed the DevTools pipe"),
        }
    }
}

/// Hands Chromium the ends of the DevTools pipes it reads commands from and
/// writes messages to.
#[cfg(unix)]
fn pass_pipes(command: &mut Command, reads: &PipeReader, writes: &PipeWriter) -> Result<()> {
    use std::os::fd::AsRawFd;
    let (reads, writes) = (reads.as_raw_fd(), writes.as_raw_fd());
    let check = |result: libc::c_int| {
        if result == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(result)
        }
    };
    // Chromium takes them as file descriptors 3 and 4, which they may already
    // be, so they are moved out of the way first.
    // SAFETY: fcntl and dup2 are async-signal-safe, so they may run between
    // fork and exec.
    unsafe {
        command.pre_exec(move || {
            let reads = check(libc::fcntl(reads, libc::F_DUPFD_CLOEXEC, 10))?;
            let writes = check(libc::fcntl(writes, libc::F_DUPFD_CLOEXEC, 10))?;
            check(libc::dup2(reads, 3))?;
            check(libc::dup2(writes, 4))?;
            Ok(())
        });
    }
    Ok(())
}

/// Hands Chromium the ends of the DevTools pipes it reads commands from and
/// writes messages to.
#[cfg(windows)]
fn pass_pipes(command: &mut Command, reads: &PipeReader, writes: &PipeWriter) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    let (reads, writes) = (reads.as_raw_handle(), writes.as_raw_handle());
    for handle in [reads, writes] {
        // SAFETY: the handle stays open while borrowed.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    command.arg(format!(
        "--remote-debugging-io-pipes={},{}",
        reads as usize, writes as usize
    ));
    Ok(())
}

/// The result in a `response` to `method`.
fn result(method: &str, mut response: Value) -> Result<Value> {
    if let Some(error) = response.get("error") {
        bail!("{method} failed: {error}");
    }
    Ok(response["result"].take())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::RgbaImage;

    #[tokio::test]
    #[ignore = "needs Chromium"]
    async fn screenshots_keep_transparency() {
        let page = "data:text/html,<body style='margin:0'>\
            <div style='width:50px;height:20px;background:rgba(255,0,0,0.5)'></div>";
        let mut page = Page::open(&Chromium::installed(), page, &[], (100, 40), 2.0).await.unwrap();
        let image = RgbaImage::decode_png(&page.screenshot().await.unwrap()).unwrap();
        assert_eq!((image.width, image.height), (200, 80));
        let pixel = |x: usize, y: usize| &image.pixels[(y * 200 + x) * 4..][..4];
        assert_eq!(pixel(10, 10)[0], 255);
        assert!((pixel(10, 10)[3] as i32 - 128).abs() <= 1);
        assert_eq!(pixel(150, 60)[3], 0);
    }

    #[tokio::test]
    #[ignore = "downloads Chrome's headless shell"]
    async fn the_downloaded_headless_shell_takes_screenshots() {
        let downloads = std::env::temp_dir().join(format!("arctos-recorder-downloads-{}", std::process::id()));
        let executable = download_headless_shell(&downloads).await.unwrap();
        assert_eq!(download_headless_shell(&downloads).await.unwrap(), executable);
        let chromium = Chromium {
            downloads: None,
            executable: OnceCell::new_with(Some(executable)),
        };
        let page = "data:text/html,<body style='background:red'>";
        let mut page = Page::open(&chromium, page, &[], (100, 40), 1.0).await.unwrap();
        let image = RgbaImage::decode_png(&page.screenshot().await.unwrap()).unwrap();
        assert_eq!(&image.pixels[..4], &[255, 0, 0, 255]);
        drop(page);
        std::fs::remove_dir_all(downloads).unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Chromium"]
    async fn pages_that_cant_load_fail_to_open() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let error = Page::open(&Chromium::installed(), &format!("http://{closed}/"), &[], (100, 40), 1.0)
            .await
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("ERR_CONNECTION_REFUSED"), "{error:#}");
    }
}
