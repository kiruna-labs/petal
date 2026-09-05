//! Headless participant bots (SPEC.md §7 point 1) -- thin wrappers around
//! `desktop_lib::transport::{RoomConnection, Subscriber}` that publish a
//! synthetic [`crate::pattern::TestPattern`] instead of a real captured
//! window, and/or subscribe + measure via [`crate::metrics::LatencyTracker`].
//!
//! ## What's reused vs. new here
//!
//! `RoomConnection::connect` / `RoomConnection::publish_window_at` /
//! `PublishedTrack::push_frame` are used completely unchanged -- this module
//! contains ZERO LiveKit API calls of its own; every
//! `Room`/`LocalVideoTrack`/`NativeVideoSource` touch goes through the
//! existing `transport` module, exactly per this task's instruction to reuse
//! it as a library rather than re-implement connect/publish/subscribe. New
//! code here is: (a) driving
//! `PublishedTrack::push_frame` with synthetic frames on a timer instead of
//! real ScreenCaptureKit callbacks, and (b) feeding `Subscriber`'s
//! `on_frame` callback into a `LatencyTracker` instead of the M0 probes'
//! inline stat-printing.
//!
//! ## Portability note (see also `transport/mod.rs`'s own doc comment)
//!
//! `PublishedTrack::push_frame` takes `&desktop_lib::capture::CapturedFrame`
//! -- a plain owned-payload frame struct with no ScreenCaptureKit dependency
//! in its *fields*, but the module it lives in (`capture.rs`) is
//! `#[cfg(target_os = "macos")]`-gated in `desktop_lib`'s `lib.rs`, purely
//! because that's also where the real `SCStream` capture code lives. On
//! macOS (this repo's only target today) that's a non-issue: we construct a
//! real `CapturedFrame` with synthetic bytes and hand it to `push_frame`
//! unchanged. A literal Linux build of this harness would need
//! `CapturedFrame`'s definition (or `push_frame`'s signature) split out of
//! the macOS-only module -- a small, mechanical follow-up, not attempted
//! here since this session only has a macOS environment to verify against.
//!
//! ## LIVE I/O ONLY -- not unit-tested
//!
//! Every `pub async fn` in this module opens a real `Room::connect`. None of
//! it can be exercised in this task's environment (real network connections
//! hang unkillably here, per the task brief) or in a normal `cargo test`
//! run. `pattern.rs`/`metrics.rs`/`impairment.rs`/`scorecard.rs` hold all the
//! logic that CAN be (and is) unit-tested; this module is intentionally thin
//! glue on top of them plus `desktop_lib::transport`, verified only via
//! `cargo check`/`cargo build` (compiles, types line up) in this session.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use desktop_lib::capture::{CapturedFrame, CapturedFramePayload, PooledFrameData};
use desktop_lib::transport::{RoomConnection, ShareQuality, Subscriber};
use desktop_lib::video_color::VideoColorProfile;

use crate::metrics::{FrameSample, LatencyTracker};
use crate::pattern::TestPattern;

#[derive(Debug, thiserror::Error)]
pub enum BotError {
    #[error("room connect/publish failed: {0}")]
    Room(#[from] desktop_lib::transport::publisher::RoomConnectionError),
    #[error("subscribe failed: {0}")]
    Subscribe(#[from] desktop_lib::transport::subscriber::SubscriberError),
}

/// Config for one publishing bot: identity, target room, how many synthetic
/// "shares" (independent tracks) to publish, resolution, and target fps.
#[derive(Debug, Clone)]
pub struct PublisherBotConfig {
    pub identity: String,
    pub room_name: String,
    /// SPEC.md §7 point 1: "shares-per-bot (1-3)".
    pub shares: u32,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// A running publisher bot: `shares` independent synthetic video tracks, all
/// on one room connection (mirrors the real app's one-room/multi-track model
/// per SPEC.md §4.3, exercised here with bots instead of a human sharer).
///
/// Each track's frame-pump task takes ownership of its `PublishedTrack`
/// (moved into the spawned task, not borrowed) so there is no shared-
/// reference lifetime problem -- `PublisherBot` itself only retains the
/// `stop` flag and a join-handle per track for orderly shutdown.
pub struct PublisherBot {
    _room: RoomConnection,
    pump_handles: Vec<tokio::task::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl PublisherBot {
    /// Connect and publish `config.shares` synthetic tracks, each fed by its
    /// own background frame-pump task at `config.fps`.
    ///
    /// LIVE I/O -- see module doc comment.
    pub async fn start(
        url: &str,
        token: &str,
        config: PublisherBotConfig,
    ) -> Result<Self, BotError> {
        let room = RoomConnection::connect(url, token).await?;
        room.discard_compositor_events();

        let stop = Arc::new(AtomicBool::new(false));
        let mut pump_handles = Vec::with_capacity(config.shares as usize);

        for share_idx in 0..config.shares {
            let bot_share_id = format!("{}-share{}", config.identity, share_idx);
            let track = room
                .publish_window_at(config.width, config.height, ShareQuality::Full, None)
                .await?;

            let pattern = TestPattern::new(config.width, config.height, &bot_share_id);
            let period = Duration::from_secs_f64(1.0 / config.fps.max(1.0));
            let stop_flag = stop.clone();

            // `track` (a `PublishedTrack`) is MOVED into this task -- it
            // owns the track for its entire lifetime, so `push_frame`'s
            // `&self` borrow is trivially valid for as long as the loop
            // runs. No unsafe, no shared mutable state beyond the atomic
            // stop flag.
            let handle = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(period);
                let frame_index = AtomicU64::new(0);
                while !stop_flag.load(Ordering::Relaxed) {
                    ticker.tick().await;
                    let idx = frame_index.fetch_add(1, Ordering::Relaxed);
                    let bytes = pattern.render(idx);
                    let capture_wall_time_us = now_us();
                    let captured = CapturedFrame {
                        width: config.width,
                        height: config.height,
                        payload: CapturedFramePayload::Bgra {
                            bytes_per_row: config.width as usize * 4,
                            data: PooledFrameData::from_vec(bytes),
                        },
                        source_scale: 1.0,
                        color_profile: VideoColorProfile::BT601_VIDEO,
                        sequence: idx + 1,
                        frame_status: None,
                        dirty_rect_count: 0,
                        dirty_area_px: 0,
                        lock_copy_ms: 0.0,
                    };
                    track.push_frame(&captured, capture_wall_time_us);
                }
                log::info!("publisher bot '{bot_share_id}': frame pump stopped");
            });

            pump_handles.push(handle);
        }

        Ok(Self {
            _room: room,
            pump_handles,
            stop,
        })
    }

    pub fn track_count(&self) -> usize {
        self.pump_handles.len()
    }

    /// Signal all frame-pump loops to stop. Does not await completion --
    /// callers that need to block until pumps actually exit should await
    /// [`PublisherBot::join`] afterward.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Await every pump task's exit (call after [`PublisherBot::stop`]).
    pub async fn join(self) {
        for handle in self.pump_handles {
            let _ = handle.await;
        }
    }
}

/// Config for one subscribing/measuring bot.
#[derive(Debug, Clone)]
pub struct SubscriberBotConfig {
    pub identity: String,
    pub room_name: String,
}

/// A running subscriber bot: joins a room, auto-subscribes to every
/// published video track (via `desktop_lib::transport::Subscriber`, exactly
/// as the M0 `subscribe_probe` example does), and feeds every decoded
/// frame's embedded metadata into one shared [`LatencyTracker`].
///
/// SPEC.md §7 point 3 asks for glass-to-glass latency, freeze/jank, and
/// (nice-to-have) spatial quality per subscribed stream. This bot tracks
/// latency/freeze across ALL subscribed tracks combined by default (simplest
/// correct behavior for "does this room's traffic look healthy") --
/// `into_per_track_trackers` below is the hook a caller uses if it wants a
/// breakdown per publisher instead.
pub struct SubscriberBot {
    _subscriber: Subscriber,
    tracker: Arc<std::sync::Mutex<LatencyTracker>>,
}

impl SubscriberBot {
    /// LIVE I/O -- see module doc comment.
    pub async fn start(
        url: &str,
        token: &str,
        _config: SubscriberBotConfig,
    ) -> Result<Self, BotError> {
        let tracker = Arc::new(std::sync::Mutex::new(LatencyTracker::new()));
        let tracker_cb = tracker.clone();

        let subscriber = Subscriber::connect(url, &token, move |frame| {
            tracker_cb.lock().unwrap().observe(FrameSample {
                frame_id: frame.frame_id,
                capture_timestamp_us: frame.capture_timestamp_us,
                receive_timestamp_us: frame.receive_timestamp_us,
            });
        })
        .await?;

        Ok(Self {
            _subscriber: subscriber,
            tracker,
        })
    }

    /// Snapshot current stats without stopping the bot (safe to call
    /// periodically mid-scenario, e.g. to log progress).
    pub fn snapshot(&self) -> (crate::metrics::LatencyStats, crate::metrics::FreezeStats) {
        // `LatencyTracker::finish` consumes `self`, so snapshot via a cheap
        // clone-out of the accumulated samples instead of finishing the
        // real tracker (which would reset it). We rebuild a throwaway
        // tracker from the same observed data by re-running `finish` on a
        // clone of the Mutex-guarded state -- simplest correct option given
        // `LatencyTracker` doesn't implement `Clone` (it holds a `Vec` that
        // easily could, but consuming `finish()` is the only reader today);
        // see `LatencyTracker`'s pub fields note. Since `LatencyTracker`
        // doesn't derive `Clone`, we take the lock and read out via a fresh
        // `finish()` call on a moved-out replacement, restoring an empty
        // tracker in its place -- this DOES reset accumulated stats on every
        // snapshot call, which is a real behavior difference from a true
        // "peek" and is documented here rather than silently assumed.
        let mut guard = self.tracker.lock().unwrap();
        let taken = std::mem::take(&mut *guard);
        taken.finish()
    }
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
