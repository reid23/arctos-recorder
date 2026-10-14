//! Maps this Pi's `CLOCK_BOOTTIME` (the clock libcamera stamps frames with)
//! onto the base station's clock.

use std::sync::atomic::{AtomicI64, Ordering};

use protocol::Timestamp;

#[derive(Default)]
pub struct Clock {
    /// Base station time minus boot time, in microseconds.
    offset: AtomicI64,
}

impl Clock {
    pub fn boottime_micros() -> i64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid out-pointer and CLOCK_BOOTTIME always exists on Linux.
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
        ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000
    }

    pub fn set_offset(&self, micros: i64) {
        self.offset.store(micros, Ordering::Relaxed);
    }

    pub fn now(&self) -> Timestamp {
        self.boottime_to_timestamp(Self::boottime_micros())
    }

    pub fn boottime_to_timestamp(&self, micros: i64) -> Timestamp {
        Timestamp(micros + self.offset.load(Ordering::Relaxed))
    }
}
