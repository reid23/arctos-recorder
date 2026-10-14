//! Pairs left and right frames that were exposed at the same time.
//!
//! Frames arrive slightly out of order from the encoder pool, so a frame is
//! only considered once it is [`SETTLE`] older than the newest frame seen.

use std::collections::BTreeMap;
use std::time::Duration;

use protocol::{Side, Timestamp, rig};

use crate::recorder::CameraFrame;

const SETTLE: Duration = Duration::from_millis(500);
const MAX_SKEW_MICROS: i64 = rig::frame_micros() / 2;

pub struct StereoFrame {
    pub timestamp: Timestamp,
    /// Indexed by [`Side::index`].
    pub jpegs: [Vec<u8>; 2],
}

#[derive(Default)]
pub struct Pairer {
    pending: [BTreeMap<Timestamp, Vec<u8>>; 2],
    newest: Option<Timestamp>,
}

impl Pairer {
    /// Adds a frame and returns any pairs that are now settled, oldest first.
    /// Settled frames without a partner are dropped.
    pub fn push(&mut self, frame: CameraFrame) -> Vec<StereoFrame> {
        self.pending[frame.side.index()].insert(frame.timestamp, frame.jpeg);
        let newest = self
            .newest
            .map_or(frame.timestamp, |n| n.max(frame.timestamp));
        self.newest = Some(newest);
        let settled = newest - SETTLE;

        let mut pairs = Vec::new();
        loop {
            let heads = Side::BOTH.map(|side| {
                self.pending[side.index()]
                    .first_key_value()
                    .map(|(&t, _)| t)
            });
            let oldest = match heads {
                [Some(l), Some(r)] => l.min(r),
                [Some(t), None] | [None, Some(t)] => t,
                [None, None] => break,
            };
            if oldest > settled {
                break;
            }
            match heads {
                [Some(l), Some(r)] if l.micros_since(r).abs() <= MAX_SKEW_MICROS => {
                    let [left, right] = &mut self.pending;
                    let (timestamp, left) = left.pop_first().expect("head exists");
                    let (_, right) = right.pop_first().expect("head exists");
                    pairs.push(StereoFrame {
                        timestamp,
                        jpegs: [left, right],
                    });
                }
                _ => {
                    let side = if heads[0] == Some(oldest) {
                        Side::Left
                    } else {
                        Side::Right
                    };
                    self.pending[side.index()].pop_first();
                    tracing::debug!("dropped unpaired {side:?} frame at {oldest:?}");
                }
            }
        }
        pairs
    }
}
