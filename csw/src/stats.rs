//! What the camera reports about itself with each poll for instructions:
//! the Pi's CPU, memory and temperature, how many frames each camera
//! dropped, and where autofocusing put the lenses.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use protocol::{AutofocusOutcome, CameraStats, FrameCounts, Memory, Side};
use tokio::sync::watch;

const CPU_TIMES_FILE: &str = "/proc/stat";
const MEMORY_FILE: &str = "/proc/meminfo";
const TEMPERATURE_FILE: &str = "/sys/class/thermal/thermal_zone0/temp";

/// Running totals of each camera's frames, indexed by [`Side::index`], kept
/// by the capture and encoding threads.
#[derive(Default)]
pub struct FrameCounters([SideCounters; 2]);

#[derive(Default)]
struct SideCounters {
    captured: AtomicU64,
    dropped: AtomicU64,
    previews: AtomicU64,
    previews_dropped: AtomicU64,
}

impl FrameCounters {
    /// Counts a frame from the sensor, which missed `dropped` frames since
    /// the last.
    pub fn count_capture(&self, side: Side, dropped: u64) {
        let counters = &self.0[side.index()];
        counters.captured.fetch_add(1, Ordering::Relaxed);
        counters.dropped.fetch_add(dropped, Ordering::Relaxed);
    }

    pub fn count_preview(&self, side: Side, sent: bool) {
        let counters = &self.0[side.index()];
        counters.previews.fetch_add(1, Ordering::Relaxed);
        if !sent {
            counters.previews_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn totals(&self) -> [FrameCounts; 2] {
        self.0.each_ref().map(|counters| FrameCounts {
            captured: counters.captured.load(Ordering::Relaxed),
            dropped: counters.dropped.load(Ordering::Relaxed),
            previews: counters.previews.load(Ordering::Relaxed),
            previews_dropped: counters.previews_dropped.load(Ordering::Relaxed),
        })
    }
}

/// Makes the [`CameraStats`] for each poll.
pub struct Reporter {
    counters: Arc<FrameCounters>,
    autofocus: watch::Receiver<Option<AutofocusOutcome>>,
    /// When last reported, to measure the CPU's use since.
    cpu_times: Option<CpuTimes>,
}

impl Reporter {
    pub fn new(counters: Arc<FrameCounters>, autofocus: watch::Receiver<Option<AutofocusOutcome>>) -> Reporter {
        Reporter {
            counters,
            autofocus,
            cpu_times: read_cpu_times(),
        }
    }

    pub fn stats(&mut self) -> CameraStats {
        let cpu_times = read_cpu_times();
        let cpu = self
            .cpu_times
            .zip(cpu_times)
            .and_then(|(before, now)| now.busy_share_since(&before));
        self.cpu_times = cpu_times;
        CameraStats {
            cpu,
            memory: std::fs::read_to_string(MEMORY_FILE)
                .ok()
                .and_then(|text| parse_memory(&text)),
            temperature: std::fs::read_to_string(TEMPERATURE_FILE)
                .ok()
                .and_then(|text| parse_temperature(&text)),
            frames: self.counters.totals(),
            autofocus: *self.autofocus.borrow(),
        }
    }
}

fn read_cpu_times() -> Option<CpuTimes> {
    parse_cpu_times(&std::fs::read_to_string(CPU_TIMES_FILE).ok()?)
}

/// Time all the CPU's cores spent since boot, in clock ticks.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

impl CpuTimes {
    fn busy_share_since(&self, earlier: &CpuTimes) -> Option<f64> {
        let total = self.total.checked_sub(earlier.total).filter(|&total| total > 0)?;
        let busy = self.busy.checked_sub(earlier.busy)?;
        Some(busy as f64 / total as f64)
    }
}

/// From the first line of `/proc/stat`: `cpu` then the ticks spent in
/// user, nice, system, idle, iowait, irq, softirq and steal time (and guest
/// time, which user time already includes).
fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let ticks: Vec<u64> = stat
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .take(8)
        .map(|field| field.parse().ok())
        .collect::<Option<_>>()?;
    let [_, _, _, idle, iowait, ..] = ticks[..] else {
        return None;
    };
    let total: u64 = ticks.iter().sum();
    Some(CpuTimes {
        busy: total - idle - iowait,
        total,
    })
}

/// From `/proc/meminfo`, whose values are in kibibytes.
fn parse_memory(meminfo: &str) -> Option<Memory> {
    let field = |name: &str| -> Option<u64> {
        let line = meminfo.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))?;
        let kibibytes: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
        Some(kibibytes * 1024)
    };
    let total = field("MemTotal")?;
    Some(Memory {
        used: total.saturating_sub(field("MemAvailable")?),
        total,
    })
}

/// From a thermal zone's `temp`, in thousandths of a degree.
fn parse_temperature(text: &str) -> Option<f64> {
    Some(text.trim().parse::<f64>().ok()? / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_cpus_busy_share() {
        let before = parse_cpu_times(
            "cpu  100 5 50 800 20 3 2 0 0 0\ncpu0 25 1 12 200 5 1 0 0 0 0\nintr 12345\n",
        )
        .unwrap();
        let after = parse_cpu_times("cpu  160 5 70 900 40 3 2 0 0 0\n").unwrap();
        assert_eq!(before, CpuTimes { busy: 160, total: 980 });
        assert_eq!(after.busy_share_since(&before), Some(80.0 / 200.0));
        assert_eq!(before.busy_share_since(&before), None);
        assert_eq!(parse_cpu_times("intr 12345\n"), None);
    }

    #[test]
    fn reads_memory_in_use() {
        let meminfo = "MemTotal:        8245440 kB\nMemFree:         1000000 kB\nMemAvailable:    6197440 kB\n";
        assert_eq!(
            parse_memory(meminfo),
            Some(Memory {
                used: 2_048_000 * 1024,
                total: 8_245_440 * 1024,
            })
        );
        assert_eq!(parse_memory("MemTotal:        8245440 kB\n"), None);
    }

    #[test]
    fn reads_the_temperature() {
        assert_eq!(parse_temperature("76550\n"), Some(76.55));
        assert_eq!(parse_temperature(""), None);
    }

    #[test]
    fn counts_frames_by_side() {
        let counters = FrameCounters::default();
        counters.count_capture(Side::Left, 0);
        counters.count_capture(Side::Left, 2);
        counters.count_capture(Side::Right, 0);
        counters.count_preview(Side::Right, true);
        counters.count_preview(Side::Right, false);
        assert_eq!(
            counters.totals(),
            [
                FrameCounts {
                    captured: 2,
                    dropped: 2,
                    previews: 0,
                    previews_dropped: 0,
                },
                FrameCounts {
                    captured: 1,
                    dropped: 0,
                    previews: 2,
                    previews_dropped: 1,
                },
            ]
        );
    }
}
