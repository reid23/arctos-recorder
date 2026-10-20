//! How this computer and the cameras are doing, for the Performance pane.
//! This computer is sampled while the pane is open; the cameras report
//! with each poll for their instructions.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nvml_wrapper::Nvml;
use nvml_wrapper::error::NvmlError;
use protocol::{CameraStats, FrameCounts, Memory};
use sysinfo::System;
use tokio::time::MissedTickBehavior;

use crate::hub::Hub;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// How far back the shares of frames the cameras lost are measured.
pub const LOSS_WINDOW: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Performance {
    /// `None` until sampled.
    pub computer: Option<ComputerUsage>,
    /// By camera id.
    pub cameras: BTreeMap<String, CameraUsage>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComputerUsage {
    /// Share of all the cores' time, from 0 to 1.
    pub cpu: f64,
    pub memory: Memory,
    /// None unless NVIDIA's driver is installed.
    pub gpus: Vec<GpuUsage>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GpuUsage {
    pub name: String,
    /// Share of the time it was busy, from 0 to 1.
    pub busy: f64,
    pub memory: Memory,
}

/// What a camera last reported (see [`CameraStats`]).
#[derive(Clone, Debug, PartialEq)]
pub struct CameraUsage {
    pub cpu: Option<f64>,
    pub memory: Option<Memory>,
    pub temperature: Option<f64>,
    /// Indexed by [`protocol::Side::index`], as the sides are shown.
    pub losses: [FrameLoss; 2],
}

impl CameraUsage {
    pub fn new(stats: &CameraStats, losses: [FrameLoss; 2]) -> CameraUsage {
        CameraUsage {
            cpu: stats.cpu,
            memory: stats.memory,
            temperature: stats.temperature,
            losses,
        }
    }
}

/// Shares (from 0 to 1) of one camera's frames lost over the last
/// [`LOSS_WINDOW`]; `None` while there were none to lose.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameLoss {
    pub dropped: Option<f64>,
    /// Of the previews, which the camera didn't encode or the base station
    /// didn't take in time.
    pub previews_dropped: Option<f64>,
}

/// The frame counts a camera reported over the last [`LOSS_WINDOW`], and
/// the one before, which the losses are measured from.
#[derive(Default)]
pub struct FrameHistory(VecDeque<(Instant, [FrameCounts; 2])>);

impl FrameHistory {
    /// Adds a report, returning each camera's losses over the window up to
    /// it, indexed by [`protocol::Side::index`] as the camera reports them.
    pub fn add(&mut self, now: Instant, counts: [FrameCounts; 2]) -> [FrameLoss; 2] {
        let restarted = self.0.back().is_some_and(|(_, last)| {
            last.iter()
                .zip(&counts)
                .any(|(last, now)| now.captured < last.captured || now.previews < last.previews)
        });
        if restarted {
            self.0.clear();
        }
        self.0.push_back((now, counts));
        while self.0.len() > 1 && now.duration_since(self.0[1].0) >= LOSS_WINDOW {
            self.0.pop_front();
        }
        let (_, first) = self.0.front().expect("just added");
        std::array::from_fn(|side| {
            let (first, last) = (first[side], counts[side]);
            let since = |count: fn(&FrameCounts) -> u64| count(&last).saturating_sub(count(&first));
            FrameLoss {
                dropped: share(since(|c| c.dropped), since(|c| c.captured + c.dropped)),
                previews_dropped: share(since(|c| c.previews_dropped), since(|c| c.previews)),
            }
        })
    }
}

fn share(part: u64, whole: u64) -> Option<f64> {
    (whole > 0).then(|| part as f64 / whole as f64)
}

/// Samples this computer's use while the Performance pane is open.
pub async fn track_computer(hub: Arc<Hub>) {
    let mut system = System::new();
    // Loaded the first time the pane opens.
    let mut nvml: Option<Option<Nvml>> = None;
    let mut settings = hub.settings.subscribe();
    let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        if !settings.borrow_and_update().panes.performance {
            if settings.changed().await.is_err() {
                return;
            }
            continue;
        }
        ticker.tick().await;
        system.refresh_cpu_usage();
        system.refresh_memory();
        let nvml = nvml.get_or_insert_with(|| {
            Nvml::init()
                .inspect_err(|e| tracing::info!("not watching NVIDIA GPUs: {e}"))
                .ok()
        });
        let usage = ComputerUsage {
            cpu: f64::from(system.global_cpu_usage()) / 100.0,
            memory: Memory {
                used: system.used_memory(),
                total: system.total_memory(),
            },
            gpus: nvml.as_ref().map(gpu_usage).unwrap_or_default(),
        };
        hub.performance.send_modify(|performance| performance.computer = Some(usage));
    }
}

fn gpu_usage(nvml: &Nvml) -> Vec<GpuUsage> {
    let count = nvml.device_count().unwrap_or_else(|e| {
        tracing::debug!("counting NVIDIA GPUs: {e}");
        0
    });
    (0..count)
        .filter_map(|index| {
            let usage = || -> Result<GpuUsage, NvmlError> {
                let device = nvml.device_by_index(index)?;
                let memory = device.memory_info()?;
                Ok(GpuUsage {
                    name: device.name()?,
                    busy: f64::from(device.utilization_rates()?.gpu) / 100.0,
                    memory: Memory {
                        used: memory.used,
                        total: memory.total,
                    },
                })
            };
            usage()
                .inspect_err(|e| tracing::debug!("reading GPU {index}'s use: {e}"))
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(captured: u64, dropped: u64, previews: u64, previews_dropped: u64) -> [FrameCounts; 2] {
        let side = FrameCounts {
            captured,
            dropped,
            previews,
            previews_dropped,
        };
        [side, FrameCounts { dropped: 0, ..side }]
    }

    #[test]
    fn losses_are_measured_over_the_window() {
        let start = Instant::now();
        let second = |seconds: u64| start + Duration::from_secs(seconds);
        let mut history = FrameHistory::default();
        let unmeasured = FrameLoss {
            dropped: None,
            previews_dropped: None,
        };
        assert_eq!(history.add(second(0), counts(0, 0, 0, 0)), [unmeasured; 2]);
        let losses = history.add(second(1), counts(54, 2, 28, 7));
        assert_eq!(losses[0].dropped, Some(2.0 / 56.0));
        assert_eq!(losses[0].previews_dropped, Some(0.25));
        assert_eq!(losses[1].dropped, Some(0.0));
        for seconds in 2..=10 {
            history.add(second(seconds), counts(54 * seconds, 2, 28 * seconds, 7));
        }
        let losses = history.add(second(11), counts(54 * 11 + 2, 2, 28 * 11, 7));
        assert_eq!(losses[0].dropped, Some(0.0), "the drops in the first second are out of the window");
        assert_eq!(losses[0].previews_dropped, Some(0.0));
    }

    #[test]
    fn a_restarted_camera_is_measured_afresh() {
        let start = Instant::now();
        let mut history = FrameHistory::default();
        history.add(start, counts(1000, 0, 500, 0));
        let losses = history.add(start + Duration::from_secs(1), counts(50, 6, 25, 0));
        assert_eq!(losses[0].dropped, None);
        let losses = history.add(start + Duration::from_secs(2), counts(104, 8, 52, 0));
        assert_eq!(losses[0].dropped, Some(2.0 / 56.0));
    }
}
