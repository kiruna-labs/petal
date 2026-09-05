//! Latency + freeze/jank measurement (SPEC.md §7 point 3), computed purely
//! from a sequence of received-frame samples -- no live I/O in this module,
//! so it's fully unit-testable by feeding it synthetic timestamp/frame-id
//! sequences with known gaps, exactly as this task asks for.
//!
//! A subscribing bot (`bot.rs`) feeds each decoded frame's embedded LiveKit
//! metadata (`capture_timestamp_us`, `frame_id`, both already proven end-to-
//! end by M0 -- see `desktop_lib::transport::subscriber::ReceivedFrame`) plus
//! its own local receive wall-clock into a [`LatencyTracker`], which
//! incrementally computes glass-to-glass latency and freeze/jank stats.

/// One received sample, mirroring the fields of
/// `desktop_lib::transport::subscriber::ReceivedFrame` that this module
/// actually needs. Kept as a small local struct (rather than depending on
/// the transport type directly) so this module -- and its tests -- have zero
/// dependency on `livekit`/`desktop_lib` at all; `bot.rs` is the only place
/// that translates a real `ReceivedFrame` into this shape.
#[derive(Debug, Clone, Copy)]
pub struct FrameSample {
    pub frame_id: Option<u32>,
    pub capture_timestamp_us: Option<u64>,
    pub receive_timestamp_us: u64,
}

/// A detected freeze: a gap in the monotonic frame-counter sequence.
/// `missing_frames` is how many frame ids were skipped (e.g. ids 10 then 13
/// => 2 missing: 11, 12). `duration_us` is wall-clock time between the last
/// frame before the gap and the first frame after it, at the receiver's
/// clock -- the practically meaningful "how long did the picture actually
/// freeze for" number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freeze {
    pub after_frame_id: u32,
    pub missing_frames: u32,
    pub duration_us: u64,
}

/// Rolling stats produced by [`LatencyTracker::finish`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LatencyStats {
    pub sample_count: usize,
    pub min_ms: f64,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

/// Freeze/jank stats produced by [`LatencyTracker::finish`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FreezeStats {
    pub freeze_count: usize,
    pub total_missing_frames: u32,
    pub longest_freeze_us: u64,
    /// Total frames actually observed (does not include the missing ones).
    pub frames_received: u64,
}

/// Incremental accumulator: feed it samples in receive order via
/// [`LatencyTracker::observe`], then call [`LatencyTracker::finish`] once for
/// a final [`LatencyStats`] + [`FreezeStats`] pair.
#[derive(Debug, Default)]
pub struct LatencyTracker {
    latencies_us: Vec<i64>,
    freezes: Vec<Freeze>,
    last_frame_id: Option<u32>,
    last_receive_us: Option<u64>,
    frames_received: u64,
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one received sample, in the order frames actually arrived.
    pub fn observe(&mut self, sample: FrameSample) {
        self.frames_received += 1;

        if let Some(capture_us) = sample.capture_timestamp_us {
            let latency_us = sample.receive_timestamp_us as i64 - capture_us as i64;
            self.latencies_us.push(latency_us);
        }

        if let Some(frame_id) = sample.frame_id {
            if let Some(prev) = self.last_frame_id {
                if frame_id > prev.wrapping_add(1) {
                    let missing = frame_id - prev - 1;
                    let duration_us = match self.last_receive_us {
                        Some(prev_us) => sample.receive_timestamp_us.saturating_sub(prev_us),
                        None => 0,
                    };
                    self.freezes.push(Freeze {
                        after_frame_id: prev,
                        missing_frames: missing,
                        duration_us,
                    });
                }
                // frame_id <= prev: an out-of-order/duplicate arrival. Not
                // treated as a freeze (nothing was skipped) and not
                // regressing `last_frame_id` -- a late-arriving duplicate
                // shouldn't retroactively "un-gap" a real freeze already
                // recorded.
                if frame_id > prev {
                    self.last_frame_id = Some(frame_id);
                }
            } else {
                self.last_frame_id = Some(frame_id);
            }
        }

        self.last_receive_us = Some(sample.receive_timestamp_us);
    }

    /// Consume the tracker and produce final stats. Safe to call on an
    /// empty tracker (returns zeroed stats, `sample_count: 0`).
    pub fn finish(self) -> (LatencyStats, FreezeStats) {
        let mut sorted = self.latencies_us.clone();
        sorted.sort_unstable();
        let n = sorted.len();

        let latency = if n == 0 {
            LatencyStats::default()
        } else {
            let to_ms = |us: i64| us as f64 / 1000.0;
            LatencyStats {
                sample_count: n,
                min_ms: to_ms(sorted[0]),
                max_ms: to_ms(sorted[n - 1]),
                avg_ms: to_ms(sorted.iter().sum::<i64>()) / n as f64,
                p50_ms: to_ms(percentile(&sorted, 0.50)),
                p95_ms: to_ms(percentile(&sorted, 0.95)),
            }
        };

        let longest_freeze_us = self.freezes.iter().map(|f| f.duration_us).max().unwrap_or(0);
        let total_missing_frames = self.freezes.iter().map(|f| f.missing_frames).sum();

        let freeze = FreezeStats {
            freeze_count: self.freezes.len(),
            total_missing_frames,
            longest_freeze_us,
            frames_received: self.frames_received,
        };

        (latency, freeze)
    }

    /// Freezes observed so far, without consuming the tracker -- exposed for
    /// tests/tools that want the raw list, not just the rolled-up stats.
    pub fn freezes(&self) -> &[Freeze] {
        &self.freezes
    }
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    debug_assert!(!sorted.is_empty());
    let idx = ((sorted.len() as f64) * p) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(frame_id: u32, capture_us: u64, receive_us: u64) -> FrameSample {
        FrameSample {
            frame_id: Some(frame_id),
            capture_timestamp_us: Some(capture_us),
            receive_timestamp_us: receive_us,
        }
    }

    #[test]
    fn no_samples_gives_zeroed_stats() {
        let tracker = LatencyTracker::new();
        let (lat, freeze) = tracker.finish();
        assert_eq!(lat.sample_count, 0);
        assert_eq!(freeze.freeze_count, 0);
        assert_eq!(freeze.frames_received, 0);
    }

    #[test]
    fn computes_exact_latency_for_uniform_samples() {
        let mut tracker = LatencyTracker::new();
        // Every frame arrives exactly 50ms (50_000us) after capture.
        for i in 0..10u32 {
            let base = i as u64 * 33_000; // ~30fps cadence
            tracker.observe(sample(i, base, base + 50_000));
        }
        let (lat, freeze) = tracker.finish();
        assert_eq!(lat.sample_count, 10);
        assert!((lat.min_ms - 50.0).abs() < 1e-9);
        assert!((lat.max_ms - 50.0).abs() < 1e-9);
        assert!((lat.avg_ms - 50.0).abs() < 1e-9);
        assert!((lat.p50_ms - 50.0).abs() < 1e-9);
        assert!((lat.p95_ms - 50.0).abs() < 1e-9);
        assert_eq!(freeze.freeze_count, 0);
        assert_eq!(freeze.frames_received, 10);
    }

    #[test]
    fn percentiles_reflect_a_skewed_distribution() {
        let mut tracker = LatencyTracker::new();
        // 19 frames at 20ms latency, 1 frame (the "tail") at 500ms.
        for i in 0..19u32 {
            tracker.observe(sample(i, 0, 20_000));
        }
        tracker.observe(sample(19, 0, 500_000));
        let (lat, _freeze) = tracker.finish();
        assert_eq!(lat.sample_count, 20);
        assert!((lat.p50_ms - 20.0).abs() < 1e-9, "p50 should sit in the dense low cluster, got {}", lat.p50_ms);
        assert!(lat.p95_ms >= 20.0, "p95 should be pulled toward the tail, got {}", lat.p95_ms);
        assert!((lat.max_ms - 500.0).abs() < 1e-9);
    }

    #[test]
    fn detects_a_single_gap_and_reports_missing_count_and_duration() {
        let mut tracker = LatencyTracker::new();
        tracker.observe(sample(0, 0, 0));
        tracker.observe(sample(1, 33_000, 33_000));
        // Frames 2,3,4 dropped -- next arrival is frame_id 5.
        tracker.observe(sample(5, 165_000, 165_000));
        tracker.observe(sample(6, 198_000, 198_000));

        let (_lat, freeze) = tracker.finish();
        assert_eq!(freeze.freeze_count, 1);
        assert_eq!(freeze.total_missing_frames, 3);
        // Gap duration is receive-clock delta between frame 1 and frame 5.
        assert_eq!(freeze.longest_freeze_us, 165_000 - 33_000);
        assert_eq!(freeze.frames_received, 4);
    }

    #[test]
    fn detects_multiple_gaps_and_tracks_the_longest() {
        let mut tracker = LatencyTracker::new();
        tracker.observe(sample(0, 0, 0));
        tracker.observe(sample(1, 33_000, 33_000)); // ok
        tracker.observe(sample(3, 100_000, 100_000)); // gap: 1 missing (id 2), 67ms
        tracker.observe(sample(4, 133_000, 133_000)); // ok
        tracker.observe(sample(10, 500_000, 500_000)); // gap: 5 missing (ids 5-9), 367ms -- longest

        let (_lat, freeze) = tracker.finish();
        assert_eq!(freeze.freeze_count, 2);
        assert_eq!(freeze.total_missing_frames, 1 + 5);
        assert_eq!(freeze.longest_freeze_us, 500_000 - 133_000);
    }

    #[test]
    fn no_gap_when_sequence_is_perfectly_contiguous() {
        let mut tracker = LatencyTracker::new();
        for i in 0..100u32 {
            tracker.observe(sample(i, i as u64 * 33_000, i as u64 * 33_000 + 40_000));
        }
        let (_lat, freeze) = tracker.finish();
        assert_eq!(freeze.freeze_count, 0);
        assert_eq!(freeze.frames_received, 100);
    }

    #[test]
    fn out_of_order_duplicate_does_not_create_a_phantom_freeze_or_unfreeze() {
        let mut tracker = LatencyTracker::new();
        tracker.observe(sample(0, 0, 0));
        tracker.observe(sample(1, 33_000, 33_000));
        tracker.observe(sample(5, 165_000, 165_000)); // real gap: 3 missing
        // A stale, late-arriving duplicate of frame 1 shows up after the gap
        // was already recorded -- must not retroactively change the gap or
        // regress last_frame_id back down to 1.
        tracker.observe(sample(1, 33_000, 170_000));
        tracker.observe(sample(6, 198_000, 198_000));

        let (_lat, freeze) = tracker.finish();
        assert_eq!(freeze.freeze_count, 1, "the late duplicate must not add a second gap");
        assert_eq!(freeze.total_missing_frames, 3);
    }

    #[test]
    fn samples_without_metadata_are_ignored_for_latency_but_still_counted() {
        let mut tracker = LatencyTracker::new();
        tracker.observe(FrameSample {
            frame_id: None,
            capture_timestamp_us: None,
            receive_timestamp_us: 1000,
        });
        let (lat, freeze) = tracker.finish();
        assert_eq!(lat.sample_count, 0);
        assert_eq!(freeze.frames_received, 1);
    }
}
