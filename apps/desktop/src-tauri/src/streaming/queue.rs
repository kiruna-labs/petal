//! The bounded queue between the encoders and the RTMP connection, and the
//! reconnect schedule.
//!
//! * Once more than 2 s of media is queued, incoming non-key video frames are
//!   dropped and a keyframe is requested; video resumes on that keyframe.
//!   Audio is never dropped before video.
//! * Past a hard limit even audio can't keep up: the connection is treated as
//!   stalled and the publisher reconnects.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use super::flv::MediaPacket;
use crate::sync_ext::MutexExt;

pub const BACKLOG_DROP_VIDEO_MS: u32 = 2_000;
pub const BACKLOG_STALLED_MS: u32 = 10_000;

#[derive(Debug, Default)]
struct QueueState {
    packets: VecDeque<MediaPacket>,
    /// Inter frames are being dropped until the next keyframe.
    dropping_video: bool,
    stalled: bool,
    closed: bool,
}

impl QueueState {
    fn backlog_ms_with(&self, ts_ms: u32) -> u32 {
        self.packets
            .front()
            .map_or(0, |front| ts_ms.saturating_sub(front.ts_ms()))
    }
}

#[derive(Debug, Default)]
pub struct MediaQueue {
    state: Mutex<QueueState>,
    notify: tokio::sync::Notify,
    keyframe_requested: AtomicBool,
    dropped_video_frames: AtomicU64,
}

/// What [`MediaQueue::push`] did with a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Queued,
    DroppedVideo,
    Closed,
}

impl MediaQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, packet: MediaPacket) -> PushOutcome {
        let mut state = self.state.lock_unpoisoned();
        if state.closed {
            return PushOutcome::Closed;
        }
        let droppable_video = packet.is_video() && !packet.is_sequence_header();
        if droppable_video && state.dropping_video {
            if packet.is_video_keyframe() {
                state.dropping_video = false;
            } else {
                drop(state);
                self.dropped_video_frames.fetch_add(1, Ordering::Relaxed);
                return PushOutcome::DroppedVideo;
            }
        }
        let backlog = state.backlog_ms_with(packet.ts_ms());
        if droppable_video && !packet.is_video_keyframe() && backlog > BACKLOG_DROP_VIDEO_MS {
            state.dropping_video = true;
            drop(state);
            self.keyframe_requested.store(true, Ordering::Relaxed);
            self.dropped_video_frames.fetch_add(1, Ordering::Relaxed);
            return PushOutcome::DroppedVideo;
        }
        if backlog > BACKLOG_STALLED_MS {
            state.stalled = true;
        }
        state.packets.push_back(packet);
        drop(state);
        self.notify.notify_one();
        PushOutcome::Queued
    }

    /// The next packet, waiting for one. `None` once closed.
    pub async fn pop(&self) -> Option<MediaPacket> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock_unpoisoned();
                if state.closed {
                    return None;
                }
                if let Some(packet) = state.packets.pop_front() {
                    return Some(packet);
                }
            }
            notified.await;
        }
    }

    pub fn try_pop(&self) -> Option<MediaPacket> {
        self.state.lock_unpoisoned().packets.pop_front()
    }

    /// The connection can't drain even audio: reconnect.
    pub fn is_stalled(&self) -> bool {
        self.state.lock_unpoisoned().stalled
    }

    pub fn backlog_ms(&self) -> u32 {
        let state = self.state.lock_unpoisoned();
        match (state.packets.front(), state.packets.back()) {
            (Some(front), Some(back)) => back.ts_ms().saturating_sub(front.ts_ms()),
            _ => 0,
        }
    }

    /// After a reconnect: discard the backlog and resume video on a keyframe.
    pub fn reset_for_reconnect(&self) {
        let mut state = self.state.lock_unpoisoned();
        state.packets.clear();
        state.dropping_video = true;
        state.stalled = false;
        drop(state);
        self.keyframe_requested.store(true, Ordering::Relaxed);
    }

    /// Encoders poll this; true at most once per request.
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_requested.swap(false, Ordering::Relaxed)
    }

    pub fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Relaxed);
    }

    pub fn dropped_video_frames(&self) -> u64 {
        self.dropped_video_frames.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        self.state.lock_unpoisoned().closed = true;
        self.notify.notify_waiters();
        self.notify.notify_one();
    }
}

/// Reconnect delays: 1 s, 3 s, 9 s, then give up. A connection that stays up
/// for [`RECONNECT_RESET_AFTER`] earns a fresh set of attempts.
pub const RECONNECT_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(9),
];
pub const RECONNECT_RESET_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub struct ReconnectPolicy {
    delays: &'static [Duration],
    attempts_used: usize,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self::with_delays(&RECONNECT_DELAYS)
    }
}

impl ReconnectPolicy {
    pub fn with_delays(delays: &'static [Duration]) -> Self {
        Self {
            delays,
            attempts_used: 0,
        }
    }

    /// The delay before the next attempt, or `None` when the stream fails.
    /// `connected_for` is how long the connection that just dropped was up.
    pub fn next_delay(&mut self, connected_for: Option<Duration>) -> Option<Duration> {
        if connected_for.is_some_and(|up| up >= RECONNECT_RESET_AFTER) {
            self.attempts_used = 0;
        }
        let delay = self.delays.get(self.attempts_used).copied()?;
        self.attempts_used += 1;
        Some(delay)
    }

    /// Reconnect attempts since the last stable connection (1 = the first).
    pub fn attempts_used(&self) -> u32 {
        self.attempts_used as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(ts: u32, keyframe: bool) -> MediaPacket {
        MediaPacket::Video {
            ts_ms: ts,
            keyframe,
            sequence_header: false,
            data: vec![0; 4],
        }
    }

    fn audio(ts: u32) -> MediaPacket {
        MediaPacket::Audio {
            ts_ms: ts,
            sequence_header: false,
            data: vec![0; 4],
        }
    }

    #[test]
    fn drops_inter_frames_past_two_seconds_and_resumes_on_keyframe() {
        let queue = MediaQueue::new();
        assert_eq!(queue.push(video(0, true)), PushOutcome::Queued);
        for ts in (33..=2000).step_by(33) {
            assert_eq!(queue.push(video(ts, false)), PushOutcome::Queued);
        }
        // 2.033 s behind: inter frame dropped, keyframe requested.
        assert_eq!(queue.push(video(2033, false)), PushOutcome::DroppedVideo);
        assert!(queue.take_keyframe_request());
        assert!(!queue.take_keyframe_request());
        // Audio still flows while video drops.
        assert_eq!(queue.push(audio(2040)), PushOutcome::Queued);
        // Dropping continues until a keyframe, even if the backlog clears.
        while queue.try_pop().is_some() {}
        assert_eq!(queue.push(video(2066, false)), PushOutcome::DroppedVideo);
        assert_eq!(queue.push(video(2100, true)), PushOutcome::Queued);
        assert_eq!(queue.push(video(2133, false)), PushOutcome::Queued);
        assert_eq!(queue.dropped_video_frames(), 2);
    }

    #[test]
    fn sequence_headers_are_never_dropped() {
        let queue = MediaQueue::new();
        queue.push(audio(0));
        let header = MediaPacket::Video {
            ts_ms: 5000,
            keyframe: true,
            sequence_header: true,
            data: vec![1],
        };
        assert_eq!(queue.push(header), PushOutcome::Queued);
    }

    #[test]
    fn audio_past_the_hard_limit_marks_stalled() {
        let queue = MediaQueue::new();
        queue.push(audio(0));
        assert!(!queue.is_stalled());
        queue.push(audio(BACKLOG_STALLED_MS + 1));
        assert!(queue.is_stalled());
        queue.reset_for_reconnect();
        assert!(!queue.is_stalled());
        assert_eq!(queue.backlog_ms(), 0);
        // After a reconnect video waits for a keyframe.
        assert!(queue.take_keyframe_request());
        assert_eq!(queue.push(video(20_000, false)), PushOutcome::DroppedVideo);
        assert_eq!(queue.push(video(20_033, true)), PushOutcome::Queued);
    }

    #[tokio::test]
    async fn pop_waits_and_close_ends() {
        let queue = std::sync::Arc::new(MediaQueue::new());
        let waiter = {
            let queue = queue.clone();
            tokio::spawn(async move { queue.pop().await })
        };
        tokio::task::yield_now().await;
        queue.push(audio(7));
        assert_eq!(waiter.await.unwrap().map(|p| p.ts_ms()), Some(7));
        queue.close();
        assert!(queue.pop().await.is_none());
        assert_eq!(queue.push(audio(8)), PushOutcome::Closed);
    }

    #[test]
    fn reconnect_backoff_is_one_three_nine_then_fail() {
        let mut policy = ReconnectPolicy::default();
        let delays: Vec<_> = std::iter::from_fn(|| policy.next_delay(None)).collect();
        assert_eq!(delays, RECONNECT_DELAYS.to_vec());
        assert_eq!(policy.attempts_used(), 3);
        assert_eq!(policy.next_delay(Some(Duration::from_secs(5))), None);
        // A connection that held for a minute earns fresh attempts.
        assert_eq!(
            policy.next_delay(Some(RECONNECT_RESET_AFTER)),
            Some(RECONNECT_DELAYS[0])
        );
        assert_eq!(policy.attempts_used(), 1);
    }
}
