//! How much of the network the recorder is using, for the Network Usage pane.
//! Bytes are counted whenever they move. Rates are computed only while the
//! pane is open.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::Link;
use stitching::cloud::{CloudTraffic, Transfer};
use sysinfo::Networks;
use tokio::time::MissedTickBehavior;

use crate::hub::Hub;
use crate::settings::{Settings, Stitching};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Bytes sent and received, counted as they move.
#[derive(Default)]
pub struct ByteCounter {
    sent: AtomicU64,
    received: AtomicU64,
}

impl ByteCounter {
    pub fn add_sent(&self, bytes: u64) {
        self.sent.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_received(&self, bytes: u64) {
        self.received.fetch_add(bytes, Ordering::Relaxed);
    }

    fn get(&self) -> (u64, u64) {
        (
            self.sent.load(Ordering::Relaxed),
            self.received.load(Ordering::Relaxed),
        )
    }
}

/// Running byte totals the base station counts, plus the latest reports from
/// the cameras and the cloud server.
pub struct Network {
    cameras: Mutex<BTreeMap<String, Arc<ByteCounter>>>,
    cloud: Arc<ByteCounter>,
    /// Bytes ffmpeg has written to a local restream, by camera.
    streams: Mutex<BTreeMap<String, Arc<AtomicU64>>>,
    /// Last restream total the cloud server reported, by camera.
    cloud_streams: Mutex<BTreeMap<String, u64>>,
    cloud_arctos: Mutex<Transfer>,
    links: Mutex<BTreeMap<String, Link>>,
}

impl Default for Network {
    fn default() -> Self {
        Network {
            cameras: Mutex::default(),
            cloud: Arc::default(),
            streams: Mutex::default(),
            cloud_streams: Mutex::default(),
            cloud_arctos: Mutex::default(),
            links: Mutex::default(),
        }
    }
}

impl Network {
    pub fn cloud(&self) -> Arc<ByteCounter> {
        self.cloud.clone()
    }

    fn camera(&self, name: &str) -> Arc<ByteCounter> {
        self.cameras
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_default()
            .clone()
    }

    /// Bytes the base station sent to a camera.
    pub fn camera_sent(&self, name: &str, bytes: u64) {
        self.camera(name).add_sent(bytes);
    }

    /// Bytes the base station received from a camera.
    pub fn camera_received(&self, name: &str, bytes: u64) {
        self.camera(name).add_received(bytes);
    }

    /// Where a local restream records the bytes ffmpeg has written.
    pub fn stream_sent(&self, camera: &str) -> Arc<AtomicU64> {
        self.streams
            .lock()
            .unwrap()
            .entry(camera.to_owned())
            .or_default()
            .clone()
    }

    pub fn note_link(&self, camera: &str, link: Link) {
        self.links.lock().unwrap().insert(camera.to_owned(), link);
    }

    pub fn note_cloud(&self, camera: &str, traffic: CloudTraffic) {
        self.cloud_streams
            .lock()
            .unwrap()
            .insert(camera.to_owned(), traffic.restream_sent);
        self.note_cloud_arctos(traffic.arctos);
    }

    pub fn note_cloud_arctos(&self, transfer: Transfer) {
        let mut current = self.cloud_arctos.lock().unwrap();
        current.sent = current.sent.max(transfer.sent);
        current.received = current.received.max(transfer.received);
    }

    fn totals(&self, arctos: (u64, u64), adapters: BTreeMap<String, (u64, u64)>) -> Totals {
        Totals {
            cameras: self
                .cameras
                .lock()
                .unwrap()
                .iter()
                .map(|(name, counter)| (name.clone(), counter.get()))
                .collect(),
            cloud: self.cloud.get(),
            arctos,
            streams: self
                .streams
                .lock()
                .unwrap()
                .iter()
                .map(|(name, sent)| (name.clone(), sent.load(Ordering::Relaxed)))
                .collect(),
            cloud_streams: self.cloud_streams.lock().unwrap().clone(),
            cloud_arctos: {
                let transfer = *self.cloud_arctos.lock().unwrap();
                (transfer.sent, transfer.received)
            },
            links: self
                .links
                .lock()
                .unwrap()
                .iter()
                .map(|(name, link)| (name.clone(), (link.sent, link.received, link.megabits)))
                .collect(),
            adapters,
        }
    }
}

/// What the pane draws. Rates are megabits a second over the last sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub ready: bool,
    pub cameras: Vec<CameraFlow>,
    /// Between the base station and the cloud server.
    pub cloud: Option<Flow>,
    /// Between the base station and Arctos.
    pub arctos: Flow,
    /// Between the cloud server and Arctos.
    pub cloud_arctos: Option<Flow>,
    pub streams: Vec<StreamFlow>,
    pub camera_port: Option<Limit>,
    pub internet: Option<Internet>,
    /// Megabits a second the cloud server is sending on to Arctos and the
    /// stream servers.
    pub cloud_upload: Option<f64>,
}

/// Megabits a second the base station sends and receives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Flow {
    pub sent: f64,
    pub received: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CameraFlow {
    pub name: String,
    /// From the camera to the base station.
    pub from_camera: f64,
    pub to_camera: f64,
    /// The camera's own link, traffic of every program on it.
    pub link: Option<Limit>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamFlow {
    pub name: String,
    /// Sent toward the stream server.
    pub megabits: f64,
    pub from_cloud: bool,
}

/// How much of a link is in use, and how much it can carry each way when
/// that is known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limit {
    pub sent: f64,
    pub received: f64,
    pub megabits: Option<f64>,
}

/// The base station's internet adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct Internet {
    pub adapter: String,
    /// Traffic to the cloud server, Arctos, and stream servers.
    pub ours_sent: f64,
    pub ours_received: f64,
    /// Everything on the adapter, including other programs.
    pub adapter_sent: f64,
    pub adapter_received: f64,
}

impl Usage {
    pub fn measuring() -> Self {
        Usage {
            ready: false,
            cameras: Vec::new(),
            cloud: None,
            arctos: Flow { sent: 0.0, received: 0.0 },
            cloud_arctos: None,
            streams: Vec::new(),
            camera_port: None,
            internet: None,
            cloud_upload: None,
        }
    }
}

/// Which parts of the diagram are in use.
pub struct Shown {
    pub online: Vec<String>,
    pub cloud: bool,
    /// Each restream's camera and the name to give its server.
    pub streams: Vec<(String, String)>,
    pub from_cloud: bool,
    pub camera_port: Option<String>,
    pub port_megabits: Option<u32>,
    /// The adapter the internet uses. Absent to pick the busiest one that
    /// isn't the camera port.
    pub internet: Option<String>,
}

#[derive(Clone, Default, PartialEq)]
struct Totals {
    /// Sent and received by the base station, by camera.
    cameras: BTreeMap<String, (u64, u64)>,
    cloud: (u64, u64),
    arctos: (u64, u64),
    streams: BTreeMap<String, u64>,
    cloud_streams: BTreeMap<String, u64>,
    cloud_arctos: (u64, u64),
    /// Sent and received by the camera, and the link speed.
    links: BTreeMap<String, (u64, u64, Option<u32>)>,
    /// Received and transmitted by this computer, by adapter.
    adapters: BTreeMap<String, (u64, u64)>,
}

fn megabits(before: u64, after: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return 0.0;
    }
    after.saturating_sub(before) as f64 * 8.0 / seconds / 1_000_000.0
}

/// Rates between two snapshots of [`Totals`].
fn usage(before: &Totals, after: &Totals, elapsed: Duration, shown: &Shown) -> Usage {
    let mut names = BTreeSet::new();
    names.extend(shown.online.iter().cloned());
    names.extend(after.cameras.keys().cloned());
    let cameras: Vec<CameraFlow> = names
        .into_iter()
        .map(|name| {
            let (sent_before, received_before) = before.cameras.get(&name).copied().unwrap_or((0, 0));
            let (sent_after, received_after) = after.cameras.get(&name).copied().unwrap_or((0, 0));
            CameraFlow {
                from_camera: megabits(received_before, received_after, elapsed),
                to_camera: megabits(sent_before, sent_after, elapsed),
                link: link_limit(before, after, &name, elapsed),
                name,
            }
        })
        .collect();

    let cloud = shown.cloud.then(|| Flow {
        sent: megabits(before.cloud.0, after.cloud.0, elapsed),
        received: megabits(before.cloud.1, after.cloud.1, elapsed),
    });
    let arctos = Flow {
        sent: megabits(before.arctos.0, after.arctos.0, elapsed),
        received: megabits(before.arctos.1, after.arctos.1, elapsed),
    };
    let cloud_arctos_flow = Flow {
        sent: megabits(before.cloud_arctos.0, after.cloud_arctos.0, elapsed),
        received: megabits(before.cloud_arctos.1, after.cloud_arctos.1, elapsed),
    };
    let cloud_arctos = (shown.cloud || cloud_arctos_flow.sent > 0.0 || cloud_arctos_flow.received > 0.0)
        .then_some(cloud_arctos_flow);

    let mut grouped: BTreeMap<&str, f64> = BTreeMap::new();
    for (camera, name) in &shown.streams {
        let (earlier, later) = if shown.from_cloud {
            (
                before.cloud_streams.get(camera).copied().unwrap_or(0),
                after.cloud_streams.get(camera).copied().unwrap_or(0),
            )
        } else {
            (
                before.streams.get(camera).copied().unwrap_or(0),
                after.streams.get(camera).copied().unwrap_or(0),
            )
        };
        *grouped.entry(name).or_default() += megabits(earlier, later, elapsed);
    }
    let streams: Vec<StreamFlow> = grouped
        .into_iter()
        .map(|(name, megabits)| StreamFlow {
            name: name.to_owned(),
            megabits,
            from_cloud: shown.from_cloud,
        })
        .collect();

    let camera_port = shown.camera_port.as_ref().map(|_| Limit {
        sent: cameras.iter().map(|camera| camera.to_camera).sum(),
        received: cameras.iter().map(|camera| camera.from_camera).sum(),
        megabits: shown.port_megabits.map(f64::from),
    });

    let local_streams: f64 = if shown.from_cloud {
        0.0
    } else {
        streams.iter().map(|stream| stream.megabits).sum()
    };
    let ours_sent = cloud.map(|flow| flow.sent).unwrap_or(0.0) + arctos.sent + local_streams;
    let ours_received = cloud.map(|flow| flow.received).unwrap_or(0.0) + arctos.received;
    let adapter = shown
        .internet
        .clone()
        .or_else(|| busiest_adapter(before, after, shown.camera_port.as_deref()));
    let internet = adapter.map(|adapter| {
        let (sent_before, received_before) = before
            .adapters
            .get(&adapter)
            .copied()
            .map(|(received, sent)| (sent, received))
            .unwrap_or((0, 0));
        let (sent_after, received_after) = after
            .adapters
            .get(&adapter)
            .copied()
            .map(|(received, sent)| (sent, received))
            .unwrap_or((0, 0));
        Internet {
            adapter,
            ours_sent,
            ours_received,
            adapter_sent: megabits(sent_before, sent_after, elapsed),
            adapter_received: megabits(received_before, received_after, elapsed),
        }
    });

    let cloud_streams: f64 = streams
        .iter()
        .filter(|stream| stream.from_cloud)
        .map(|stream| stream.megabits)
        .sum();
    let cloud_upload = (shown.cloud || cloud_arctos_flow.sent > 0.0)
        .then(|| cloud_streams + cloud_arctos_flow.sent);

    Usage {
        ready: true,
        cameras,
        cloud,
        arctos,
        cloud_arctos,
        streams,
        camera_port,
        internet,
        cloud_upload,
    }
}

fn link_limit(before: &Totals, after: &Totals, name: &str, elapsed: Duration) -> Option<Limit> {
    let (sent_after, received_after, megabits_per_second) = *after.links.get(name)?;
    let (sent_before, received_before, _) = before.links.get(name).copied().unwrap_or((0, 0, None));
    Some(Limit {
        sent: megabits(sent_before, sent_after, elapsed),
        received: megabits(received_before, received_after, elapsed),
        megabits: megabits_per_second.map(f64::from),
    })
}

fn busiest_adapter(before: &Totals, after: &Totals, skip: Option<&str>) -> Option<String> {
    after
        .adapters
        .iter()
        .filter(|(name, _)| !is_loopback(name) && Some(name.as_str()) != skip)
        .max_by_key(|&(name, &(received, sent))| {
            let (earlier_received, earlier_sent) = before.adapters.get(name).copied().unwrap_or((0, 0));
            received.saturating_sub(earlier_received) + sent.saturating_sub(earlier_sent)
        })
        .map(|(name, _)| name.clone())
}

fn is_loopback(name: &str) -> bool {
    name == "lo" || name.strip_prefix("lo").is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
}

/// What to call a restream server, from its URL.
pub fn stream_name(server: &str) -> String {
    let host = server.split("://").nth(1).unwrap_or(server);
    let host = host.split(['/', ':']).next().unwrap_or(host);
    if host.to_ascii_lowercase().contains("youtube") {
        "YouTube".to_owned()
    } else {
        host.to_owned()
    }
}

fn link_megabits(interface: &str) -> Option<u32> {
    if interface.is_empty() || interface.contains('/') {
        return None;
    }
    let text = std::fs::read_to_string(format!("/sys/class/net/{interface}/speed")).ok()?;
    let speed: i64 = text.trim().parse().ok()?;
    (speed > 0).then_some(speed as u32)
}

fn default_adapter() -> Option<String> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    protocol::route_interface(&table, None).map(str::to_owned)
}

fn shown_now(hub: &Hub) -> Shown {
    let settings = hub.settings.borrow();
    let cameras = hub.cameras.borrow();
    let port = hub.camera_port.borrow();
    shown_from(
        &settings,
        cameras.iter().filter(|(_, status)| status.online).map(|(name, _)| name.as_str()),
        &port.chosen,
    )
}

fn shown_from<'a>(
    settings: &Settings,
    online: impl Iterator<Item = &'a str>,
    camera_port: &Option<String>,
) -> Shown {
    let cloud = settings.stitching == Stitching::Cloud;
    let streams = if settings.workflow.live_stream {
        settings
            .cameras
            .keys()
            .filter_map(|camera| {
                let destination = settings.stream_destination(camera)?;
                Some((camera.clone(), stream_name(&destination.server)))
            })
            .collect()
    } else {
        Vec::new()
    };
    Shown {
        online: online.map(str::to_owned).collect(),
        cloud,
        streams,
        from_cloud: cloud,
        port_megabits: camera_port.as_deref().and_then(link_megabits),
        camera_port: camera_port.clone(),
        internet: default_adapter(),
    }
}

/// Publishes [`Hub::network_usage`] while the Network Usage pane is open.
pub async fn track(hub: Arc<Hub>) {
    let mut settings = hub.settings.subscribe();
    let mut networks = Networks::new_with_refreshed_list();
    let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut previous: Option<(Instant, Totals)> = None;
    loop {
        if !settings.borrow_and_update().panes.network {
            previous = None;
            hub.network_usage.send_replace(Usage::measuring());
            if settings.changed().await.is_err() {
                return;
            }
            interval.reset();
            continue;
        }
        interval.tick().await;
        networks.refresh(true);
        let adapters = networks
            .iter()
            .map(|(name, data)| (name.clone(), (data.total_received(), data.total_transmitted())))
            .collect();
        let arctos = (hub.arctos.traffic().sent(), hub.arctos.traffic().received());
        let totals = hub.network.totals(arctos, adapters);
        let now = Instant::now();
        if let Some((then, before)) = &previous {
            let usage = usage(before, &totals, now.saturating_duration_since(*then), &shown_now(&hub));
            hub.network_usage.send_replace(usage);
        } else {
            hub.network_usage.send_replace(Usage::measuring());
        }
        previous = Some((now, totals));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn second() -> Duration {
        Duration::from_secs(1)
    }

    #[test]
    fn rates_are_megabits_over_the_interval() {
        assert_eq!(megabits(0, 1_000_000, second()), 8.0);
        assert_eq!(megabits(500, 100, second()), 0.0, "a restarted counter");
    }

    #[test]
    fn the_camera_port_is_every_camera_combined() {
        let mut before = Totals::default();
        let mut after = Totals::default();
        before.cameras.insert("a".into(), (0, 0));
        after.cameras.insert("a".into(), (1_000_000, 2_000_000));
        after.cameras.insert("b".into(), (0, 500_000));
        let shown = Shown {
            online: vec!["a".into(), "b".into()],
            cloud: false,
            streams: Vec::new(),
            from_cloud: false,
            camera_port: Some("enp1s0".into()),
            port_megabits: Some(1000),
            internet: None,
        };
        let usage = usage(&before, &after, second(), &shown);
        let port = usage.camera_port.unwrap();
        assert_eq!(port.received, 20.0);
        assert_eq!(port.sent, 8.0);
        assert_eq!(port.megabits, Some(1000.0));
    }

    #[test]
    fn internet_counts_what_leaves_this_computer() {
        let before = Totals::default();
        let mut after = Totals::default();
        after.cloud = (1_000_000, 250_000);
        after.arctos = (500_000, 0);
        after.streams.insert("cam".into(), 250_000);
        after.cloud_streams.insert("cam".into(), 5_000_000);
        after.adapters.insert("wlan0".into(), (100, 9_000_000));
        let shown = Shown {
            online: Vec::new(),
            cloud: true,
            streams: vec![("cam".into(), "YouTube".into())],
            from_cloud: true,
            camera_port: None,
            port_megabits: None,
            internet: Some("wlan0".into()),
        };
        let usage = usage(&before, &after, second(), &shown);
        let internet = usage.internet.unwrap();
        assert_eq!(internet.ours_sent, 12.0, "the cloud forward and Arctos, not YouTube");
        assert_eq!(internet.ours_received, 2.0);
        assert_eq!(internet.adapter_sent, 72.0);
        assert_eq!(usage.streams[0].megabits, 40.0);
        assert_eq!(usage.cloud_upload, Some(40.0));
    }

    #[test]
    fn a_stream_server_is_named_from_its_url() {
        assert_eq!(stream_name("rtmp://a.rtmp.youtube.com/live2"), "YouTube");
        assert_eq!(stream_name("rtmp://live.example.org:1935/app"), "live.example.org");
    }
}
