//! Timing for client mode:
//!
//! * [`FrameMailbox`] holds the latest captured frame. Reading never consumes
//!   it, so an idle window source (window capture only delivers on change)
//!   repeats its last frame.
//! * [`CfrSchedule`] is the constant-frame-rate tick clock: tick `n` is due at
//!   `n / fps` seconds and carries that presentation time. A late encoder skips
//!   ticks rather than bursting to catch up.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::sync_ext::MutexExt;

#[derive(Debug)]
pub struct FrameMailbox<T> {
    slot: Mutex<Option<(Arc<T>, u64)>>,
}

impl<T> Default for FrameMailbox<T> {
    fn default() -> Self {
        Self { slot: Mutex::new(None) }
    }
}

impl<T> FrameMailbox<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&self, frame: T) {
        let mut slot = self.slot.lock_unpoisoned();
        let generation = slot.as_ref().map_or(0, |(_, generation)| generation + 1);
        *slot = Some((Arc::new(frame), generation));
    }

    /// The latest frame and its generation; repeated reads return the same one.
    pub fn latest(&self) -> Option<(Arc<T>, u64)> {
        self.slot.lock_unpoisoned().clone()
    }

    pub fn clear(&self) {
        *self.slot.lock_unpoisoned() = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CfrTick {
    pub index: u64,
    /// Presentation time from stream start.
    pub pts: Duration,
    /// Ticks skipped because the encoder fell behind.
    pub skipped: u64,
}

#[derive(Debug, Clone)]
pub struct CfrSchedule {
    fps: u32,
    next_index: u64,
}

impl CfrSchedule {
    pub fn new(fps: u32) -> Self {
        Self { fps: fps.max(1), next_index: 0 }
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub fn pts_of(&self, index: u64) -> Duration {
        Duration::from_nanos(index.saturating_mul(1_000_000_000) / u64::from(self.fps))
    }

    /// When the next tick is due, from stream start.
    pub fn next_due(&self) -> Duration {
        self.pts_of(self.next_index)
    }

    /// The tick to encode at `elapsed` since stream start, or `None` if the
    /// next tick isn't due yet. Missed ticks are skipped, not replayed.
    pub fn poll(&mut self, elapsed: Duration) -> Option<CfrTick> {
        if elapsed < self.next_due() {
            return None;
        }
        let latest_due =
            u64::try_from(elapsed.as_nanos() * u128::from(self.fps) / 1_000_000_000).unwrap_or(u64::MAX);
        let skipped = latest_due.saturating_sub(self.next_index);
        let index = latest_due.max(self.next_index);
        self.next_index = index + 1;
        Some(CfrTick { index, pts: self.pts_of(index), skipped })
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_source_repeats_its_last_frame() {
        let mailbox = FrameMailbox::new();
        let mut schedule = CfrSchedule::new(30);
        mailbox.put("frame-a");
        let mut encoded = Vec::new();
        for step in 0..5u64 {
            let tick = schedule.poll(Duration::from_millis(step * 34)).expect("tick due");
            let (frame, generation) = mailbox.latest().expect("frame");
            encoded.push((tick.index, *frame, generation));
        }
        // Five ticks, five encodes of the one frame the source delivered.
        assert_eq!(encoded.len(), 5);
        assert!(encoded.iter().all(|(_, frame, generation)| *frame == "frame-a" && *generation == 0));
        mailbox.put("frame-b");
        let (frame, generation) = mailbox.latest().unwrap();
        assert_eq!((*frame, generation), ("frame-b", 1));
    }

    #[test]
    fn ticks_are_evenly_spaced_and_not_early() {
        let mut schedule = CfrSchedule::new(60);
        assert_eq!(schedule.poll(Duration::ZERO).unwrap().index, 0);
        assert!(schedule.poll(Duration::from_millis(10)).is_none());
        let tick = schedule.poll(Duration::from_micros(16_667)).unwrap();
        assert_eq!(tick.index, 1);
        assert_eq!(tick.pts, Duration::from_nanos(16_666_666));
        assert_eq!(tick.skipped, 0);
    }

    #[test]
    fn a_late_encoder_skips_ticks_instead_of_bursting() {
        let mut schedule = CfrSchedule::new(30);
        schedule.poll(Duration::ZERO);
        // 200 ms stall: ticks 1..=5 are due, only 6 is encoded.
        let tick = schedule.poll(Duration::from_millis(200)).unwrap();
        assert_eq!(tick.index, 6);
        assert_eq!(tick.skipped, 5);
        assert!(schedule.poll(Duration::from_millis(201)).is_none());
    }
}
