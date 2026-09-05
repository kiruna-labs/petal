//! Subscribe side: join a LiveKit room, subscribe to a remote video track,
//! and hand decoded frames (with their embedded SPEC.md §7 metadata) to a
//! caller callback for latency measurement / preview.
//!
//! ## Real compositor feed (SPEC.md §4.4)
//!
//! [`start_compositor_feed`] is the real receiver path used by the app
//! itself (wired from `session::join_room`): for every subscribed remote
//! video track, it recovers the source `window_id` from the track name
//! (`transport::publisher::window_id_from_track_name` -- see that module's
//! doc comment for why the window_id rides in the track name rather than a
//! separate metadata field), opens a real compositor window
//! (`compositor::ensure_window`) the first time that window_id is seen, and
//! pushes every decoded frame's real `CVPixelBufferRef` straight into it
//! (`compositor::push_frame`) with NO CPU copy -- see `native_display.rs`
//! for the zero-copy display path those pixel buffers feed.
//!
//! This is genuinely a different frame path than [`Subscriber::connect`]
//! below (kept for the M0 latency-measurement harness/examples): that path
//! only ever reads `frame.width`/`frame.height` and the embedded timing
//! metadata -- it never inspects `frame.buffer`'s actual buffer type, so it
//! was never verified against a real `CVPixelBuffer`. `start_compositor_feed`
//! is the first code in this codebase that calls `frame.buffer.buffer_type()`
//! and asserts/logs what it actually gets back on real subscribed H.264
//! frames (see its own doc comment below for exactly what was verified).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures::StreamExt;
use livekit::prelude::*;
use livekit::track::VideoQuality;
use livekit::webrtc::stats::RtcStats;
use livekit::webrtc::video_frame::{VideoBuffer, VideoBufferType};
use livekit::webrtc::video_stream::native::NativeVideoStream;
use tokio_util::sync::CancellationToken;

use crate::session::RoomGeneration;
use crate::sync_ext::MutexExt;
use crate::video_color::VideoColorProfile;

const NO_FRAME_WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
const NO_FRAME_RETIRE_AFTER: Duration = Duration::from_secs(30);
const FRAME_HEALTH_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// Receiver-side starvation policy (Windows decode loop): once a window has
/// received its first frame, this long without another decoded frame while
/// the window is on-screen means the SFU stopped serving the requested
/// (HIGH) layer — the observed "black remote window" failure mode, where
/// the publisher's high layer got bandwidth-killed (`target=0kbps,
/// limitation=Bandwidth`) but its low layer kept encoding live content.
/// Downgrading the subscription to LOW asks the SFU to serve the live low
/// layer instead of holding the window frozen on a dead high layer.
/// Sized against the sender's static idle-refresh re-push (~2s cadence): a
/// healthy share delivers a frame at least every ~2s, so 5s is a 2.5x
/// margin that only a genuinely stalled layer trips.
const STARVATION_DOWNGRADE_AFTER: Duration = Duration::from_secs(5);
/// Cadence of the decode loop's stall watchdog tick.
const STARVATION_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// While starved on LOW, re-request HIGH this often to probe whether the
/// publisher's high layer recovered (congestion cleared). A probe whose
/// frames stall again re-downgrades within `STARVATION_DOWNGRADE_AFTER`.
const STARVATION_PROBE_BASE: Duration = Duration::from_secs(30);
/// Exponential backoff cap for repeated probe failures.
const STARVATION_PROBE_MAX: Duration = Duration::from_secs(120);
/// Consecutive failed probes before giving up on HIGH for this share
/// (recovery then comes from a republish/reconnect, which spawns a fresh
/// decode loop with clean state).
const STARVATION_PROBE_FAILURE_CAP: u32 = 3;
/// #907: the liveness trigger above (no frame for `STARVATION_DOWNGRADE_AFTER`)
/// catches a dead layer, but the field incident this issue diagnoses never
/// stopped delivering frames -- the top rung kept encoding 1920x1080 the
/// whole time, just at an avg QP of 31.1 (unreadable) instead of a healthy
/// ~17. A frame arriving is not the same thing as a frame worth watching, so
/// macOS also polls this receiver's own inbound-RTP stats
/// (`InboundRtpStreamStats::qp_sum`/`frames_decoded`, the decoder's own
/// account of how hard the bitstream it decoded was quantized -- the same
/// axis the sender-side field measurement used) and downgrades on sustained
/// high QP even while frames keep flowing. Cadence coarser than the 1s
/// liveness tick: QP stats are noisy frame-to-frame and `get_stats()` is not
/// free to call every tick.
const QUALITY_STATS_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Matches the field-observed "unreadable" threshold (#907: avg QP 31.1 on
/// the starved rung vs 17.2 on the healthy one), with margin below it so the
/// guard trips well before text is unreadable.
///
/// HONEST CAVEAT (#907 adversarial review, finding 8): this number is
/// calibrated from exactly ONE field incident, on text-heavy content. It has
/// not been validated against other content types this product carries
/// (scrolling, video playback inside a shared window, an active
/// remote-control resize burst) where a legitimately complex frame can
/// transiently push QP up even on a healthy link. `QUALITY_DOWNGRADE_SUSTAINED_SAMPLES`
/// below is the only guard against a false-positive downgrade on such
/// content, and it is not corroborated by any other signal (packet loss,
/// freeze count, received bitrate trend) that could distinguish "genuinely
/// starved" from "briefly complex." Treat this threshold as a documented
/// starting point that needs live, content-varied calibration, not a
/// finished tuning.
const QUALITY_DOWNGRADE_AVG_QP: f64 = 30.0;
/// Consecutive high-QP samples (at `QUALITY_STATS_POLL_INTERVAL` cadence, so
/// ~15s) required before downgrading -- long enough that one noisy sample
/// (e.g. a keyframe request burst) can't trip it. Raised from 2 to 3 samples
/// (#907 adversarial review, finding 8) for extra margin against transiently
/// complex healthy content, and to match the sender-side starvation guard's
/// own `RUNG_STARVATION_GUARD_TRIGGER_SAMPLES` (`publisher.rs`) by design.
const QUALITY_DOWNGRADE_SUSTAINED_SAMPLES: u32 = 3;
const PLAYOUT_DELAY_ENV: &str = "PETAL_PLAYOUT_DELAY_MS";
/// Upper bound on a decoded remote frame's width/height before this module
/// will convert or push it. Generous (16K per axis) -- real shares top out
/// far below; anything larger is a corrupt dimension field, not content.
const MAX_DECODED_FRAME_DIMENSION: u32 = 16_384;
/// Throttle for the invalid-dimension warn log: at 30fps a malformed-frame
/// storm logs once per ~10s instead of per frame.
const INVALID_DIMENSION_LOG_EVERY: u64 = 300;

/// libwebrtc's `I420Buffer::Create` RTC_CHECKs its dimensions
/// (`CheckValidDimensions`) and abort()s the WHOLE process on failure -- a
/// remote CVPixelBuffer with bad dims reaching `to_i420()` SIGABRTed a live
/// meeting (desktop-2026-08-06-130423.ips). Gate every decoded frame on
/// this before any conversion/push; dropping is safe (compositor holds the
/// last good frame) and a persistent storm hits the no-frame watchdog's
/// normal retire/resubscribe repair.
fn decoded_frame_dimensions_valid(width: u32, height: u32) -> bool {
    (1..=MAX_DECODED_FRAME_DIMENSION).contains(&width)
        && (1..=MAX_DECODED_FRAME_DIMENSION).contains(&height)
}
// #682: per-window, not a single process-global aggregate -- a global
// counter can report only an aggregate leak rate and can never name which
// window is actually leaking. Cleared whenever the key's `window_states`
// entry is created or removed (`insert_window_state` / `remove_window_state`
// below) so a window's miss count never accumulates across the life of the
// process once it has been legitimately retired or resubscribed. State is
// deliberately retired when a subscription ends, and a late decoded frame
// must remain visible without turning the per-frame callback into a log
// flood -- see `mark_frame_received`.
static RETIRED_RECEIVE_STATE_FRAME_MISSES: OnceLock<Mutex<HashMap<ReceiveWindowKey, u64>>> =
    OnceLock::new();

fn retired_receive_state_frame_misses() -> &'static Mutex<HashMap<ReceiveWindowKey, u64>> {
    RETIRED_RECEIVE_STATE_FRAME_MISSES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a decoded frame that arrived with no matching `window_states` entry
/// for `key`, returning the new per-window count. See `mark_frame_received`.
fn record_window_frame_miss(key: &ReceiveWindowKey) -> u64 {
    let mut misses = retired_receive_state_frame_misses().lock_unpoisoned();
    let count = misses.entry(key.clone()).or_insert(0);
    *count += 1;
    *count
}

/// Drop `key`'s miss count. Called from `insert_window_state` and
/// `remove_window_state` so a window's count never accumulates past its
/// current subscription's lifetime.
fn clear_window_frame_misses(key: &ReceiveWindowKey) {
    retired_receive_state_frame_misses()
        .lock_unpoisoned()
        .remove(key);
}

fn parse_playout_delay_ms(value: Option<&str>) -> Result<Option<u64>, String> {
    let Some(value) = value else {
        return Ok(None);
    };

    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "{PLAYOUT_DELAY_ENV} must be unset or a non-negative integer number of milliseconds (for example, 0 or 75); got {value:?}"
        ));
    }

    let milliseconds = value.parse::<u64>().map_err(|_| {
        format!(
            "{PLAYOUT_DELAY_ENV} must be unset or a non-negative integer number of milliseconds (for example, 0 or 75); got {value:?}"
        )
    })?;
    Ok(Some(milliseconds))
}

fn playout_delay_ms_from_env() -> Result<Option<u64>, String> {
    match std::env::var(PLAYOUT_DELAY_ENV) {
        Ok(value) => parse_playout_delay_ms(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_playout_delay_ms(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "{PLAYOUT_DELAY_ENV} must be unset or a non-negative integer number of milliseconds (for example, 0 or 75); value is not valid Unicode"
        )),
    }
}

fn configure_playout_delay(video_track: &RemoteVideoTrack, milliseconds: u64) {
    let delay_seconds = milliseconds as f64 / 1000.0;
    let setter_result = video_track
        .transceiver()
        .map(|transceiver| {
            transceiver
                .receiver()
                .set_jitter_buffer_minimum_delay(Some(delay_seconds))
        })
        .ok_or("remote video track has no receiver transceiver");

    log::info!(
        "subscriber: {PLAYOUT_DELAY_ENV} resolved to {milliseconds} ms ({delay_seconds} s); receiver setter returned {setter_result:?}"
    );

    // Loud, but NOT a panic. This runs on the subscription path, and a panic in a
    // media callback aborts the process (CLAUDE.md crash class 3). `transceiver()`
    // returning None is a plausible early-subscription race, not a bug worth
    // crashing a meeting over. A measurement run MUST grep for the marker below --
    // a run that did not apply the delay it reports is void. See #214.
    match setter_result {
        Ok(true) => {}
        Ok(false) => log::error!(
            "PLAYOUT_DELAY_NOT_APPLIED: {PLAYOUT_DELAY_ENV}={milliseconds} was configured, \
             but the receiver playout-delay setter returned false -- any measurement \
             from this run is void"
        ),
        Err(error) => log::error!(
            "PLAYOUT_DELAY_NOT_APPLIED: {PLAYOUT_DELAY_ENV}={milliseconds} was configured, \
             but the receiver playout-delay setter was unavailable ({error}) -- any \
             measurement from this run is void"
        ),
    }
}

type WindowPublicationKey = (String, u32);
static WINDOW_PUBLICATIONS: OnceLock<Mutex<HashMap<WindowPublicationKey, RemoteTrackPublication>>> =
    OnceLock::new();

fn window_publications() -> &'static Mutex<HashMap<WindowPublicationKey, RemoteTrackPublication>> {
    WINDOW_PUBLICATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn update_window_subscription_dimensions(
    owner_identity: &str,
    window_id: u32,
    pixel_width: u32,
    pixel_height: u32,
) {
    if pixel_width == 0 || pixel_height == 0 {
        return;
    }
    let publication = window_publications()
        .lock_unpoisoned()
        .get(&(owner_identity.to_string(), window_id))
        .cloned();
    let Some(publication) = publication else {
        return;
    };
    // This is reached synchronously from `compositor::ensure_window` after
    // the AppKit panel and chrome are created. LiveKit's seemingly synchronous
    // update method schedules internal work with `tokio::spawn`, so calling it
    // on that main-thread callback panics with "no reactor running" (#262).
    // Move only the SDK call onto Tauri's Tokio runtime; keep compositor
    // geometry/state lookup synchronous and unchanged.
    let owner_identity = owner_identity.to_string();
    tauri::async_runtime::spawn(async move {
        publication.update_video_dimensions(TrackDimension(pixel_width, pixel_height));
        log::debug!(
            "compositor feed: requested subscription dimensions {pixel_width}x{pixel_height} for window {window_id} from '{owner_identity}'"
        );
    });
}

/// Shared subscription-request policy for a recognized shared-window track.
/// Captures the publisher's canonical encode resolution, registers the
/// publication, requests the HIGH simulcast layer, and hints the canonical
/// dimensions. Called by BOTH the macOS and the Windows compositor feed on
/// `TrackSubscribed`, so the two platforms cannot drift (Windows was missing
/// the dimension hint that macOS sends via viewer_demand, which left it on
/// the low layer with window shares).
///
/// Returns the canonical source size (used by the caller to size the remote
/// window), or `None` when the track has no usable dimensions yet.
///
/// Ordering notes:
/// - #416: the canonical read MUST run before the publication is registered
///   (made visible to viewer-demand's own dimension hints), otherwise a
///   concurrent hint write could poison `dimension()` with the receiver's
///   requested panel size.
/// - #551: quality and dimensions are separate SDK requests; both are sent
///   here from async contexts so a race can't reintroduce a low-layer
///   request. The SDK dedups an identical dimension hint.
/// Canonical source dimensions for a shared window, `None` when unusable
/// (zero width/height). Pure so the guard is unit-testable.
fn canonical_subscription_dimensions(width: u32, height: u32) -> Option<(u32, u32)> {
    (width > 0 && height > 0).then_some((width, height))
}

fn register_and_request_shared_window_subscription(
    publication: &RemoteTrackPublication,
    owner_identity: &str,
    window_id: u32,
) -> Option<(u32, u32)> {
    let canonical_source_size = {
        let TrackDimension(width, height) = publication.dimension();
        canonical_subscription_dimensions(width, height)
    };
    window_publications()
        .lock_unpoisoned()
        .insert((owner_identity.to_string(), window_id), publication.clone());

    // HIGH layer request (#551): without an explicit request, a simulcast
    // publish can sit on the lowest layer (or negotiate slowly), which
    // showed up live as a ~15s first-frame delay and ~2fps delivery.
    let owner_for_quality = owner_identity.to_string();
    let pub_for_quality = publication.clone();
    tauri::async_runtime::spawn(async move {
        pub_for_quality.set_video_quality(VideoQuality::High);
        log::info!(
            "compositor feed: requested HIGH subscription for window {window_id} from '{owner_for_quality}'"
        );
    });

    // Canonical dimension hint: steers the SFU to the layer closest to the
    // true source resolution (the SDK turns this into
    // UpdateTrackSettings{quality: High, dimensions}).
    if let Some((width, height)) = canonical_source_size {
        update_window_subscription_dimensions(owner_identity, window_id, width, height);
    }

    canonical_source_size
}

/// Re-assert the HIGH layer after the SFU dropped this window to a lower
/// simulcast tier (a transient bandwidth/adaptive dip — observed as a brief
/// low-layer stint that upscales q into the canonical window; on a slow
/// path the stint can last ~20s until the layer switches). Harmless when
/// already high (`set_video_quality` is idempotent and cheap); throttled
/// per window so a sustained low-layer stint doesn't spam. Windows-only (its
/// caller is the Windows decode loop and it keys on
/// `windows_compositor::WindowKey`).
#[cfg(target_os = "windows")]
fn publication_dimension_for_window(
    key: &crate::windows_compositor::WindowKey,
) -> Option<(u32, u32)> {
    let publication = window_publications()
        .lock_unpoisoned()
        .get(&(key.0.clone(), key.1))
        .cloned()?;
    let TrackDimension(width, height) = publication.dimension();
    (width > 0 && height > 0).then_some((width, height))
}

#[cfg(target_os = "windows")]
fn reassert_high_after_low_layer(
    key: &crate::windows_compositor::WindowKey,
    frame_width: u32,
    frame_height: u32,
) {
    let Some(publication) = window_publications()
        .lock_unpoisoned()
        .get(&(key.0.clone(), key.1))
        .cloned()
    else {
        return;
    };
    let TrackDimension(canonical_w, canonical_h) = publication.dimension();
    if canonical_w == 0 || canonical_h == 0 {
        return;
    }
    // Only a REAL layer drop (decoded frame materially smaller than the
    // canonical source, e.g. q=908 vs h=1215) triggers a re-assert; a
    // canonical-sized frame is already high.
    if frame_width >= canonical_w.saturating_mul(9) / 10
        && frame_height >= canonical_h.saturating_mul(9) / 10
    {
        return;
    }
    // 10s cooldown: `set_video_quality` is idempotent and cheap, but the
    // stint's switch is decided by the SFU's layer/bandwidth allocation —
    // re-requesting more often than this was measured to change nothing
    // (014: a dead high layer stayed dead despite 5s-interval re-asserts;
    // the layer only delivers once the allocator gives it bitrate AND a
    // keyframe is produced). Keep the tuned pre-existing cadence.
    const REASSERT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10);
    static LAST_REASSERT: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u32, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    let last =
        LAST_REASSERT.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    {
        let mut guard = last.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        if let Some(previous) = guard.get(&key.1) {
            if now.duration_since(*previous) < REASSERT_COOLDOWN {
                return;
            }
        }
        guard.insert(key.1, now);
    }
    let owner_for_log = key.0.clone();
    let window_id = key.1;
    tauri::async_runtime::spawn(async move {
        publication.set_video_quality(VideoQuality::High);
        log::info!(
            "compositor feed: re-asserted HIGH subscription for window {window_id} from '{owner_for_log}' after low-layer drop ({frame_width}x{frame_height} < canonical {canonical_w}x{canonical_h})"
        );
    });
}

/// #355 teardown guard: a republish (quality change, focus switch, repair)
/// publishes the NEW track before unpublishing the OLD one, and both carry
/// the same `petal-window-<id>` name. Removing the compositor window on the
/// old sid's unpublish would kill the just-created live window. Only remove
/// when the unpublished sid is still the one we track.
///
/// Public so `examples/share_lifecycle_probe` can drive this decision with
/// real sids in the real event order observed from a live SFU -- the pure-fn
/// unit test below cannot prove that ordering holds in practice.
pub fn should_remove_window(current_sid: Option<&str>, unpublished_sid: &str) -> bool {
    current_sid.is_some_and(|current_sid| current_sid == unpublished_sid)
}

/// What a teardown signal for a window means once the SFU's own publication
/// set is consulted (#627). The sid guard alone cannot answer this: it is
/// evaluated against whichever `TrackSubscribed` has landed *so far*, and on a
/// republish that is a race the receiver does not control.
///
/// Measured against a real SFU (`examples/share_lifecycle_probe`, 10/10 runs):
/// `TrackSubscribed(new)` beat `TrackUnpublished(old)` by 84-135ms, so the sid
/// guard usually does hold. `HoldForReplacement` exists because "usually" is
/// not a guarantee and the losing side of that race hid a live share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownDecision {
    /// The unpublished sid is already superseded by a newer one we track.
    /// Pre-existing #355 behaviour: nothing to do at all.
    IgnoreSuperseded,
    /// This sid is the one we track AND the SFU still holds a different
    /// publication for the same window -- a republish whose replacement has
    /// not reached us yet. Keep the window on screen with its last frame; the
    /// replacement's `TrackSubscribed` takes over when it lands.
    HoldForReplacement,
    /// A subscription can disappear before a full reconnect announces itself
    /// or before its replacement publication is visible in the room snapshot.
    /// It is therefore not terminal on its own: retain the tracked window
    /// until TrackUnpublished or reconciliation establishes a true departure.
    HoldForTransientUnsubscribe,
    /// This sid is the one we track and the SFU has no publication for the
    /// window. The share is genuinely over.
    RemoveWindow,
}

/// The one rule shared by every native teardown path (#627, "never show a
/// black frame"): a window is hidden only when the SFU has no publication for
/// it. While a publication exists, the window stays on screen holding its last
/// frame -- hiding it makes the share visibly vanish and reveal the desktop,
/// which is the same disruption a black flash is, arrived at by a different
/// route.
pub fn teardown_decision(
    current_sid: Option<&str>,
    unpublished_sid: &str,
    replacement_exists: bool,
) -> TeardownDecision {
    if !should_remove_window(current_sid, unpublished_sid) {
        return TeardownDecision::IgnoreSuperseded;
    }
    if replacement_exists {
        TeardownDecision::HoldForReplacement
    } else {
        TeardownDecision::RemoveWindow
    }
}

/// Resolve the actual `TrackUnsubscribed` event arm.
///
/// Unlike `TrackUnpublished`, unsubscribe says only that this receiver lost
/// its current subscription. During a full reconnect it arrives before both
/// `RoomEvent::Reconnecting` and the replacement publication, so an empty
/// room snapshot is not authority to hide an already-rendered share (#631).
/// Keep its registry entry: a subsequent `TrackSubscribed` resumes normally,
/// while `TrackUnpublished` or reconciliation still performs terminal retire.
pub fn track_unsubscribe_decision(
    current_sid: Option<&str>,
    unsubscribed_sid: &str,
) -> TeardownDecision {
    if should_remove_window(current_sid, unsubscribed_sid) {
        TeardownDecision::HoldForTransientUnsubscribe
    } else {
        TeardownDecision::IgnoreSuperseded
    }
}

/// Does the SFU hold any publication for this window whose sid is not in
/// `excluding_sids`? Reads `reconcile::discover_window_publications`, the
/// established authoritative seam, rather than replaying events -- the whole
/// point is to consult a source the event race cannot skew.
///
/// Pass no exclusions to ask the plain question "does this share still exist".
fn window_publication_exists(
    room: &Room,
    owner_identity: &str,
    window_id: u32,
    excluding_sids: &[&str],
) -> bool {
    crate::transport::reconcile::discover_window_publications(room)
        .into_iter()
        .any(|publication| {
            publication.owner_identity == owner_identity
                && publication.window_id == window_id
                && !excluding_sids.contains(&publication.sid.as_str())
        })
}

/// What must happen to the publication registry once a teardown is decided.
///
/// This is separated out because getting it wrong is how a hold becomes a
/// PERMANENT phantom window, which is a worse regression than the vanishing it
/// replaced. Every path that can remove a compositor window is keyed off state
/// a naive hold would delete:
///
///   * the teardown arms need a registry entry (`should_remove_window(None, _)`
///     is false, so a missing entry decides `IgnoreSuperseded` forever);
///   * `reconcile`'s `Divergence::Orphaned` -- the authority on a genuinely
///     gone publication -- is only produced for keys present in `tracked`
///     (`reconcile::reconcile`'s second loop), so a forgotten window is
///     invisible to it;
///   * the no-frame watchdog needs a receive-state entry.
///
/// So a held window KEEPS its registry entry. Reconciliation then sees
/// `tracked=old_sid` against `discovered=new_sid`, reports `Replaced`, and
/// `RecoveryStep::Adopt` repoints the registry at the live publication -- the
/// #298 mechanism built for exactly this. If instead the publication really
/// disappears, `Orphaned` fires and the window is removed for real.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryUpdate {
    /// Keep the entry so reconciliation retains authority over this window.
    Keep,
    /// Drop it, but only if it is still the sid we decided against.
    RemoveIfUnchanged,
}

pub(crate) fn registry_update_for(decision: TeardownDecision) -> RegistryUpdate {
    match decision {
        // #627: keeping this entry is what preserves a teardown path for a held
        // window. Dropping it left the window frozen on screen for the rest of
        // the meeting with nothing able to remove it.
        TeardownDecision::HoldForReplacement | TeardownDecision::HoldForTransientUnsubscribe => {
            RegistryUpdate::Keep
        }
        TeardownDecision::RemoveWindow => RegistryUpdate::RemoveIfUnchanged,
        TeardownDecision::IgnoreSuperseded => RegistryUpdate::Keep,
    }
}

/// Resolve a `TrackUnpublished`/`TrackUnsubscribed` against the tracked sid
/// and, for terminal unpublishes, the SFU's live publication set.
///
/// `unpublished` distinguishes the two arms, and the distinction is real: an
/// unpublish removes the publication from the SFU, so the sid must be excluded
/// when asking whether a replacement exists. An UNSUBSCRIBE is not a terminal
/// event at all: a reconnect can clear both the subscription and the snapshot
/// before its replacement arrives, so it always holds the tracked window.
fn resolve_teardown(
    room: &Room,
    owner_identity: &str,
    window_id: u32,
    publication_sid: &str,
    unpublished: bool,
) -> TeardownDecision {
    let key = (owner_identity.to_string(), window_id);
    let current_sid = window_publications()
        .lock_unpoisoned()
        .get(&key)
        .map(|current| current.sid().to_string());
    if !unpublished {
        return track_unsubscribe_decision(current_sid.as_deref(), publication_sid);
    }
    if !should_remove_window(current_sid.as_deref(), publication_sid) {
        return TeardownDecision::IgnoreSuperseded;
    }
    // Ask the SFU while holding no lock: `discover_window_publications`
    // touches the SDK's room object, and the registry lock must not span it.
    let excluding: &[&str] = if unpublished { &[publication_sid] } else { &[] };
    let replacement_exists = window_publication_exists(room, owner_identity, window_id, excluding);
    let decision = teardown_decision(current_sid.as_deref(), publication_sid, replacement_exists);
    if registry_update_for(decision) == RegistryUpdate::RemoveIfUnchanged {
        // Conditional: the lock was released for the SFU query above, so the
        // replacement's `TrackSubscribed` may have inserted its own publication
        // in that window. Removing unconditionally would drop the NEW sid,
        // after which a later genuine unpublish would read `current_sid == None`
        // and leave the window up forever once the share really ended.
        let mut publications = window_publications().lock_unpoisoned();
        if publications
            .get(&key)
            .is_some_and(|current| current.sid().to_string() == publication_sid)
        {
            publications.remove(&key);
        }
    }
    decision
}

/// Drive one `TrackUnpublished`/`TrackUnsubscribed` to its consequence. Shared
/// by both arms so the two cannot drift apart -- they differ only in the log
/// verb and the teardown reason they report.
///
/// `HoldForReplacement` deliberately keeps the receive state in place: the
/// window is still on screen, so the no-frame watchdog must keep watching it,
/// and the replacement's `TrackSubscribed` overwrites the entry when it lands.
#[cfg(target_os = "macos")]
fn apply_teardown_decision(
    app: &tauri::AppHandle,
    room: &Room,
    window_states: &Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>>,
    receive_key: &ReceiveWindowKey,
    publication_sid: &str,
    verb: &str,
    unpublished: bool,
    reason: crate::compositor::RemoveWindowReason,
) {
    let owner_identity = &receive_key.owner_identity;
    let window_id = receive_key.window_id;
    let decision = resolve_teardown(
        room,
        owner_identity,
        window_id,
        publication_sid,
        unpublished,
    );
    match decision {
        TeardownDecision::IgnoreSuperseded => {
            log::debug!(
                "compositor feed: ignoring stale track {verb} for window {window_id} from '{owner_identity}'"
            );
        }
        TeardownDecision::HoldForReplacement | TeardownDecision::HoldForTransientUnsubscribe => {
            let held_for_replacement = decision == TeardownDecision::HoldForReplacement;
            log::debug!(
                "compositor feed: track {verb} for window {window_id} from '{owner_identity}' {}; attempting last-frame hold (#627, #631, #840)",
                if held_for_replacement {
                    "is a republish and the SFU still holds a publication"
                } else {
                    "is non-terminal until an unpublish or reconciliation proves departure"
                }
            );
            if crate::compositor::hold_window_last_frame(
                app,
                owner_identity,
                window_id,
                crate::compositor::HoldWindowReason::ReplacementInbound,
            ) {
                log::info!(
                    "compositor feed: track {verb} for window {window_id} from '{owner_identity}' left the last frame held on screen and the window tracked (#627, #631, #840)"
                );
            } else {
                match undisplayable_hold_fallback(crate::compositor::is_open_for_owner(
                    owner_identity,
                    window_id,
                )) {
                    UndisplayableHoldFallback::KeepTracked => {
                        log::info!(
                            "compositor feed: track {verb} for window {window_id} from '{owner_identity}' could not hold a displayable frame; left the open window tracked and reveal-gated (#627, #631, #840)"
                        );
                    }
                    UndisplayableHoldFallback::Remove => {
                        remove_window_state(window_states, receive_key);
                        crate::compositor::remove_window(app, owner_identity, window_id, reason);
                        log::info!(
                            "compositor feed: track {verb} for window {window_id} from '{owner_identity}' could not hold because no compositor window remained; removed stale receive state (#627, #631, #840)"
                        );
                    }
                }
            }
        }
        TeardownDecision::RemoveWindow => {
            remove_window_state(window_states, receive_key);
            crate::compositor::remove_window(app, owner_identity, window_id, reason);
            log::info!(
                "compositor feed: track {verb} for window {window_id} from '{owner_identity}' found no SFU replacement and removed the window (#627, #631, #840)"
            );
        }
    }
}

/// What a failed `hold_window_last_frame` means on a NON-TERMINAL teardown
/// (#840). Before this, `false` was read as authority to hide, which is how a
/// sharer-side republish storm became visible once-a-second window flapping
/// on the receiver: every republish resets `revealed_first_frame` on pool
/// reuse, so the hold could never succeed and every cycle hid a window the
/// SFU still held a publication for.
///
/// A hold fails for exactly two reasons, and only one of them is a teardown:
/// the window is open but still behind the first-frame reveal gate (NOT on
/// screen -- nothing to hide, and the replacement subscribe will feed it), or
/// there is no compositor window at all (stale receive state to clean up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UndisplayableHoldFallback {
    KeepTracked,
    Remove,
}

fn undisplayable_hold_fallback(window_is_open: bool) -> UndisplayableHoldFallback {
    if window_is_open {
        UndisplayableHoldFallback::KeepTracked
    } else {
        UndisplayableHoldFallback::Remove
    }
}

/// A LiveKit full reconnect delivers `ParticipantDisconnected` before it
/// announces `Reconnecting`. The event itself is therefore not terminal: keep
/// the participant's rendered windows visible and leave the receiver's
/// tracking alone so reconciliation can retire a genuine departure.
///
/// `hold_windows` is injected to keep this lifecycle decision independently
/// testable without manufacturing a Tauri `AppHandle` or SDK participant.
#[cfg(target_os = "macos")]
pub fn handle_participant_disconnected(
    participant_name: &str,
    identity: &str,
    hold_windows: impl FnOnce(&str),
) {
    log::warn!(
        "compositor feed: '{participant_name}' ({identity}) disconnected -- holding any \
         rendered native windows until reconciliation establishes whether this \
         is a reconnect or a real departure (#631)"
    );
    hold_windows(identity);
}

/// The receiver's own picture of what it is currently receiving, in the shape
/// `transport::reconcile` diffs against the SFU's authoritative publication
/// set (#298). An entry exists exactly while a decode loop does, which is what
/// makes reconciliation's resubscribe recovery safe against double decode
/// loops.
pub(crate) fn tracked_window_publications() -> Vec<crate::transport::reconcile::TrackedWindow> {
    window_publications()
        .lock_unpoisoned()
        .iter()
        .map(|((owner_identity, window_id), publication)| {
            crate::transport::reconcile::TrackedWindow {
                owner_identity: owner_identity.clone(),
                window_id: *window_id,
                sid: publication.sid().to_string(),
            }
        })
        .collect()
}

/// Repoint the teardown SID guard at the authoritative publication (#298).
/// Adoption only: it starts no decode loop and sends no subscription request,
/// so it can never become a second discovery path for a track the
/// `TrackSubscribed` arm is already handling.
pub(crate) fn adopt_window_publication(
    owner_identity: &str,
    window_id: u32,
    publication: RemoteTrackPublication,
) {
    window_publications()
        .lock_unpoisoned()
        .insert((owner_identity.to_string(), window_id), publication);
}

/// Drop receiver state for a window reconciliation has proven is not backed by
/// a live publication (#298).
pub(crate) fn forget_window_publication(owner_identity: &str, window_id: u32) {
    window_publications()
        .lock_unpoisoned()
        .remove(&(owner_identity.to_string(), window_id));
}

#[derive(Debug, thiserror::Error)]
pub enum SubscriberError {
    #[error("room connect failed: {0}")]
    Connect(#[from] livekit::RoomError),
    #[error("{0}")]
    PlayoutDelayConfiguration(String),
}

/// One received, decoded frame plus the sender-embedded metadata needed to
/// compute glass-to-glass latency (SPEC.md §7).
pub struct ReceivedFrame {
    pub width: u32,
    pub height: u32,
    /// Wall-clock microseconds (sender's clock) when the frame was captured
    /// from ScreenCaptureKit -- carried via LiveKit's frame metadata
    /// trailer (see `transport::publisher`).
    pub capture_timestamp_us: Option<u64>,
    pub frame_id: Option<u32>,
    /// Wall-clock microseconds (receiver's clock) when this frame was
    /// pulled off the decoded-frame stream. This is only comparable with
    /// `capture_timestamp_us` after diagnostics applies the data-channel
    /// sender/receiver clock offset; raw cross-machine subtraction is invalid.
    pub receive_timestamp_us: u64,
}

pub struct Subscriber {
    pub room: Arc<Room>,
}

impl Subscriber {
    /// Connect to `url` as `identity` in `room_name`, auto-subscribing to
    /// all published tracks. `on_frame` is invoked for every decoded video
    /// frame received from ANY remote video track (the M0 spike only
    /// expects one).
    pub async fn connect(
        url: &str,
        token: &str,
        on_frame: impl Fn(ReceivedFrame) + Send + Sync + 'static,
    ) -> Result<Self, SubscriberError> {
        Self::connect_with_quality_request(url, token, None, on_frame).await
    }

    /// Same as [`Subscriber::connect`], but optionally issues an explicit
    /// `UpdateTrackSettings` quality request on every subscribed video track.
    /// `None` sends nothing, which is what BOTH real receive paths do for
    /// camera tracks today (`start_compositor_feed` requests HIGH only for
    /// `petal-window-*`; the JS gallery bridge sends no track settings at
    /// all). `examples/camera_cadence_probe` drives the explicit arms to
    /// measure whether such a request changes the SFU's initial layer choice
    /// at all, and to force the low layer as a positive control (#592).
    pub async fn connect_with_quality_request(
        url: &str,
        token: &str,
        quality_request: Option<VideoQuality>,
        on_frame: impl Fn(ReceivedFrame) + Send + Sync + 'static,
    ) -> Result<Self, SubscriberError> {
        let playout_delay_ms =
            playout_delay_ms_from_env().map_err(SubscriberError::PlayoutDelayConfiguration)?;
        let mut room_options = RoomOptions::default();
        room_options.auto_subscribe = true;

        let (room, mut events) = Room::connect(url, token, room_options).await?;
        let room = Arc::new(room);

        log::info!(
            "Subscriber connected: room='{}' sid={}",
            room.name(),
            room.sid().await
        );

        let on_frame = Arc::new(on_frame);

        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let RoomEvent::TrackSubscribed {
                    track,
                    publication,
                    participant,
                } = event
                {
                    let RemoteTrack::Video(video_track) = track else {
                        continue;
                    };
                    log::info!(
                        "Subscribed to video track from participant '{}'",
                        participant.identity()
                    );
                    if let Some(milliseconds) = playout_delay_ms {
                        configure_playout_delay(&video_track, milliseconds);
                    }
                    if let Some(quality) = quality_request {
                        publication.set_video_quality(quality);
                    }
                    let on_frame = on_frame.clone();
                    tokio::spawn(async move {
                        let rtc_track = video_track.rtc_track();
                        let mut stream = NativeVideoStream::new(rtc_track);
                        while let Some(frame) = stream.next().await {
                            let receive_timestamp_us = now_us();
                            let (capture_timestamp_us, frame_id) = frame
                                .frame_metadata
                                .as_ref()
                                .map(|m| (m.user_timestamp, m.frame_id))
                                .unwrap_or((None, None));
                            on_frame(ReceivedFrame {
                                width: frame.buffer.width(),
                                height: frame.buffer.height(),
                                capture_timestamp_us,
                                frame_id,
                                receive_timestamp_us,
                            });
                        }
                        log::info!("Video stream ended");
                    });
                }
            }
        });

        Ok(Self { room })
    }
}

/// Start the REAL receiver-side compositor feed on the event receiver
/// registered by `Room::connect` (SPEC.md §4.4). Called once per room
/// connection from `session::join_room`, exactly like
/// `telepointer::start_receiver_for_room`/`presence::start_for_room`, but it
/// consumes the connect-time receiver so LiveKit's initial `Connected` and
/// early `TrackSubscribed` events are not lost to a late `room.subscribe()`
/// registration (#357).
///
/// `local_identity` is this process's own LiveKit identity -- used only to
/// skip a track this same process published (shouldn't normally arrive as a
/// `TrackSubscribed` for our own publish, but guarded explicitly rather than
/// assumed, since `auto_subscribe: true`'s exact self-track behavior isn't
/// spelled out in the SDK docs).
///
/// For each subscribed remote video track:
/// 1. Recovers `window_id` from the track name
///    (`publisher::window_id_from_track_name`) -- tracks that don't parse
///    (e.g. a camera track, or a non-Petal publisher) are skipped: this feed
///    is specifically for shared-WINDOW tracks, not every video track in the
///    room.
/// 2. Opens (idempotently) a real compositor window for that `window_id` via
///    `compositor::ensure_window`.
/// 3. Runs a per-track decode loop identical in structure to
///    `Subscriber::connect`'s above, but pulls the REAL `CVPixelBufferRef`
///    out of each frame instead of only reading its dimensions, and pushes
///    it straight into the compositor window with `compositor::push_frame`
///    -- zero CPU copies from decode to the `AVSampleBufferDisplayLayer` (see
///    `native_display.rs`).
/// 4. On `TrackUnsubscribed`/`ParticipantDisconnected`, tears down the
///    corresponding compositor window(s) (SPEC.md §4.4 lifecycle: "when a
///    remote participant stops sharing... tear down... when a new remote
///    share starts, create one").
#[cfg(target_os = "macos")]
pub(crate) fn start_compositor_feed(
    app: &tauri::AppHandle,
    mut events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    room: Arc<Room>,
    local_identity: String,
    generation: RoomGeneration,
) {
    let playout_delay_ms = playout_delay_ms_from_env().unwrap_or_else(|error| panic!("{error}"));
    let app = app.clone();
    let window_states = Arc::new(Mutex::new(
        HashMap::<ReceiveWindowKey, ReceiveWindowState>::new(),
    ));

    tauri::async_runtime::spawn(async move {
        let mut watchdog = tokio::time::interval(NO_FRAME_WATCHDOG_INTERVAL);
        // #298 receiver-side reconciliation rides this existing tick rather
        // than adding a timer or a per-trigger signal: reconnect, publication
        // replacement, and subscription churn all surface as the same thing --
        // local state disagreeing with `discover_window_publications`.
        let feed_started = Instant::now();
        let mut recovery_ledger = crate::transport::reconcile::RecoveryLedger::new();
        let mut reconnect_lifecycle = ReconnectLifecycle::default();
        loop {
            let event = tokio::select! {
                _ = watchdog.tick() => {
                    if !generation.is_current() {
                        log::debug!("compositor feed: exiting for stale room generation");
                        break;
                    }
                    log_receiver_frame_health(&window_states);
                    retire_no_frame_windows(&app, &room, &window_states);
                    // Strictly after-the-fact: join-time TrackSubscribed
                    // delivery is already structural (#364), so reconciling
                    // during it would only race a track arriving normally.
                    if feed_started.elapsed() >= crate::transport::reconcile::FIRST_PASS_GRACE {
                        crate::transport::reconcile::run_reconciliation_pass(
                            &app,
                            &room,
                            &mut recovery_ledger,
                            reconnect_lifecycle.is_reconnecting(),
                        );
                    }
                    continue;
                }
                event = events.recv() => event,
            };
            let Some(event) = event else {
                break;
            };
            if !generation.is_current() {
                log::debug!("compositor feed: exiting for stale room generation");
                break;
            }
            match event {
                RoomEvent::TrackSubscribed {
                    track,
                    publication,
                    participant,
                } => {
                    if participant.identity().to_string() == local_identity {
                        continue;
                    }
                    let RemoteTrack::Video(video_track) = track else {
                        continue;
                    };
                    if let Some(milliseconds) = playout_delay_ms {
                        configure_playout_delay(&video_track, milliseconds);
                    }
                    let track_name = video_track.name();
                    let Some(window_id) =
                        crate::transport::publisher::window_id_from_track_name(&track_name)
                    else {
                        // #51 waterproofing: a camera track landing here is
                        // routine (every participant's webcam is a video
                        // track, just not a window share) -- keep that at
                        // debug. Anything else is unexpected: a genuinely new
                        // TrackSubscribed for a video track this feed doesn't
                        // recognize at all is exactly the shape of bug this
                        // module exists to catch (e.g. a track-naming
                        // mismatch that would otherwise silently swallow a
                        // real window share with zero INFO-level trace), so
                        // surface it instead of debug-only.
                        if track_name.starts_with(crate::transport::publisher::CAMERA_TRACK_PREFIX)
                        {
                            log::debug!(
                                "compositor feed: track '{}' from '{}' is a camera track, not a window share, skipping",
                                track_name,
                                participant.identity()
                            );
                        } else {
                            log::info!(
                                "compositor feed: track '{}' from '{}' is not a recognized Petal window/camera share, skipping",
                                track_name,
                                participant.identity()
                            );
                        }
                        continue;
                    };

                    let owner_identity = participant.identity().to_string();
                    let receive_key = ReceiveWindowKey::new(owner_identity.clone(), window_id);
                    // Shared subscription policy: capture canonical dims
                    // (pre-registration, #416), register the publication,
                    // request HIGH, and hint the canonical dimensions — the
                    // SAME call the Windows feed makes, so the two platforms
                    // can't drift.
                    let mut canonical_source_size = register_and_request_shared_window_subscription(
                        &publication,
                        &owner_identity,
                        window_id,
                    );
                    let owner_display_name = participant.name();
                    let owner_display_name = if owner_display_name.is_empty() {
                        owner_identity.clone()
                    } else {
                        owner_display_name
                    };
                    let source_title =
                        crate::transport::publisher::shared_window_title_from_metadata(
                            &participant.metadata(),
                            window_id,
                        )
                        .unwrap_or_else(|| track_name.clone());
                    let source_kind = crate::transport::publisher::shared_window_kind_from_metadata(
                        &participant.metadata(),
                        window_id,
                    );
                    let share_instance_id =
                        crate::transport::publisher::shared_window_share_instance_from_metadata(
                            &participant.metadata(),
                            window_id,
                        );
                    if canonical_source_size.is_none()
                        && source_kind
                            == crate::transport::publisher::SharedSourceKind::DisplayRegion
                    {
                        canonical_source_size = crate::transport::publisher::
                            shared_window_region_physical_size_from_metadata(
                                &participant.metadata(),
                                window_id,
                            );
                    }
                    let source_title = source_title_for_kind(source_kind, &source_title);
                    let source_scale_metadata =
                        crate::transport::publisher::shared_window_scale_from_metadata(
                            &participant.metadata(),
                            window_id,
                        );
                    // The sharer's own denial (petalWindowRemoteControl) is
                    // separate from "metadata hasn't arrived yet": the first is
                    // permanent and must read as such, the second is transient.
                    let remote_control_disallowed =
                        !crate::transport::publisher::shared_window_remote_control_allowed_from_metadata(
                            &participant.metadata(),
                            window_id,
                        );
                    let remote_control_available =
                        source_scale_metadata.is_some() && !remote_control_disallowed;
                    if !remote_control_available && !remote_control_disallowed {
                        log::debug!(
                            "compositor feed: native metadata not available yet for window {window_id} from '{}'; remote control stays hidden until metadata arrives",
                            participant.identity()
                        );
                    }
                    let source_scale = source_scale_metadata.unwrap_or(1.0);
                    let source_url = crate::transport::publisher::shared_window_url_from_metadata(
                        &participant.metadata(),
                        window_id,
                    );
                    let owner_palette_index =
                        crate::transport::publisher::identity_palette_index_from_metadata(
                            &participant.metadata(),
                        );
                    let color_profile =
                        shared_window_color_profile_or_default(&participant.metadata(), window_id);

                    log::info!(
                        "compositor feed: track subscribed for window {window_id} from '{owner_display_name}' ({owner_identity}), color_profile {color_profile:?}"
                    );
                    // #682: owns this decode loop's lifetime. Cloned into the
                    // spawned loop below via `next_frame_or_cancelled`; the
                    // original is stored on the state itself so
                    // `insert_window_state`/`remove_window_state` can cancel
                    // it later without needing anything else in scope.
                    let cancel_token = CancellationToken::new();
                    insert_window_state(
                        &window_states,
                        receive_key.clone(),
                        ReceiveWindowState::new(
                            owner_identity.clone(),
                            track_name.clone(),
                            color_profile,
                            Instant::now(),
                            cancel_token.clone(),
                        ),
                    );
                    crate::compositor::set_window_media_paused(
                        &app,
                        &owner_identity,
                        window_id,
                        false,
                    );
                    crate::diagnostics::record_native_video_stream_state(
                        &app,
                        &owner_identity,
                        &track_name,
                        "active",
                        "livekit-rust-track-subscribed",
                    );

                    // A republish under the SAME window_id (SPEC.md §4.3's
                    // focus-quality switch, `session.rs`'s `apply_quality`
                    // unpublish+republish) fires a fresh `TrackSubscribed`
                    // for a window whose compositor window is already open --
                    // `ensure_window` itself is idempotent (no-op if already
                    // open), so the window/header/pointer overlay are left
                    // exactly as they are; only a new decode loop is started
                    // below, for the NEW track. #682 correction: the OLD
                    // track's decode loop does NOT end on its own here --
                    // `stream.next()` merely stopping yielding does not make
                    // it return (the underlying `VideoFrameQueue` is only
                    // closed by this same task's own `NativeVideoStream::drop`,
                    // which never runs while the task is parked awaiting the
                    // next frame). What actually prevents the double-feed is
                    // `insert_window_state` above: it cancels the OLD state's
                    // `CancellationToken` before installing this new one, so
                    // the old loop's `next_frame_or_cancelled` race resolves
                    // to `Cancelled` and it exits -- logged here so a
                    // quality-switch republish is visibly distinguishable
                    // from a genuinely new share in the logs.
                    let already_open =
                        crate::compositor::is_open_for_owner(&owner_identity, window_id);
                    if already_open {
                        // #679: info -- this is the third way to get no pill,
                        // and it must be visible in a default-level field log.
                        log::info!(
                            "compositor feed: window {window_id} already open -- treating this TrackSubscribed as a republish (e.g. focus-quality switch); no share-started pill (#679)"
                        );
                    }

                    crate::compositor::ensure_window(
                        &app,
                        window_id,
                        &owner_identity,
                        &owner_display_name,
                        &source_title,
                        source_url,
                        source_kind,
                        share_instance_id,
                        source_scale,
                        remote_control_available,
                        remote_control_disallowed,
                        owner_palette_index,
                        canonical_source_size,
                    );

                    // #875 review F1: seed this window's z-rank from
                    // metadata already available at TrackSubscribed time,
                    // the same way every other per-window field above
                    // (title/kind/scale/url/palette) is seeded here.
                    // `ensure_window` always inserts a fresh window with
                    // `z_rank: None`, and the ONLY other writer is the
                    // `ParticipantMetadataChanged` handler below, which only
                    // fires again on a metadata CHANGE -- so a rank that was
                    // already published before this window existed (a
                    // rearrange right after a share starts, before this
                    // subscribe lands) would otherwise be lost until the
                    // sharer's next rearrangement, possibly never.
                    crate::compositor::update_window_z_rank(
                        &owner_identity,
                        window_id,
                        crate::transport::publisher::shared_window_z_rank_from_metadata(
                            &participant.metadata(),
                            window_id,
                        ),
                    );

                    // #679: the "<Name> is sharing a window" pill fires only
                    // for a GENUINELY new remote share -- never for the
                    // republish `already_open` just logged above (a
                    // quality-switch/resize unpublish+republish), and never
                    // for a re-subscribe that follows a transport-side
                    // teardown (reconnect, stalled watchdog, or a deliberate
                    // manual hide -- see
                    // `compositor::consume_share_started_pill_suppression`'s
                    // doc comment for why `already_open` alone is NOT a
                    // sufficient gate for that second case: a full reconnect
                    // removes the key from the open set entirely, so the very
                    // next TrackSubscribed would look identical to a brand
                    // new share without this separate, reason-keyed check).
                    if !already_open {
                        let suppressed = crate::compositor::consume_share_started_pill_suppression(
                            &owner_identity,
                            window_id,
                        );
                        if suppressed {
                            // #679: info, not debug. A field report of "the
                            // pill never appears" is undiagnosable if every
                            // branch of this decision is invisible at the
                            // default log level -- there are three distinct
                            // ways to get no pill (already_open, suppressed,
                            // emitted-but-never-shown) and the log has to say
                            // which one happened.
                            log::info!(
                                "share_notice: suppressed the remote-share-started pill for window {window_id} from '{owner_identity}' (transport-side re-subscribe, not a new share) (#679)"
                            );
                        } else {
                            log::info!(
                                "share_notice: emitting remote-share-started pill for window {window_id} from '{owner_identity}' (#679)"
                            );
                            crate::share_notice::emit_remote_share_started(
                                &app,
                                crate::share_notice::RemoteShareStartedPayload {
                                    window_id,
                                    owner_identity: owner_identity.clone(),
                                    owner_display_name: owner_display_name.clone(),
                                    source_title: source_title.clone(),
                                },
                            );
                        }
                    }

                    // (The HIGH layer + canonical dimension hint were already
                    // requested by register_and_request_shared_window_subscription
                    // above; resize updates continue through viewer-demand.)

                    // #110: metadata race. `ensure_window` marshals the actual
                    // AppKit window creation + `s.windows` insertion onto the
                    // main thread via `run_on_main_thread` and returns
                    // immediately -- it does NOT wait for that to land. If
                    // `participant.metadata()` (read just above) didn't yet
                    // carry this window's real title/scale
                    // (`remote_control_available` false), and the real
                    // metadata publish lands as a `ParticipantMetadataChanged`
                    // event in the gap before the insert completes,
                    // `window_ids_for_participant` finds nothing to update and
                    // that refresh is silently dropped -- the header is then
                    // stuck on the TrackSubscribed-time fallback title (or the
                    // raw track name) forever, with no later event to retry
                    // it. Since that's the ONLY symptom this closes (a window
                    // that already got good metadata at subscribe time needs
                    // nothing further), only run it when metadata was
                    // genuinely absent at subscribe time.
                    if !remote_control_available {
                        let app_for_retry = app.clone();
                        let owner_identity_for_retry = owner_identity.clone();
                        let participant_for_retry = participant.clone();
                        let generation_for_retry = generation.clone();
                        tokio::spawn(async move {
                            // Bounded, cheap polling: main-thread scheduling
                            // lag is on the order of milliseconds, not
                            // seconds, so ~1.5s total gives ample margin
                            // without holding a task open indefinitely if
                            // metadata genuinely never arrives (a separate,
                            // real problem this isn't meant to mask).
                            for _ in 0..10 {
                                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                                if !generation_for_retry.is_current() {
                                    return;
                                }
                                if !crate::compositor::window_ids_for_participant(
                                    &owner_identity_for_retry,
                                )
                                .contains(&window_id)
                                {
                                    continue;
                                }
                                let metadata = participant_for_retry.metadata();
                                let Some(source_scale) =
                                    crate::transport::publisher::shared_window_scale_from_metadata(
                                        &metadata, window_id,
                                    )
                                else {
                                    continue;
                                };
                                let source_title =
                                    crate::transport::publisher::shared_window_title_from_metadata(
                                        &metadata, window_id,
                                    )
                                    .unwrap_or_else(|| format!("Shared window {window_id}"));
                                let source_kind =
                                    crate::transport::publisher::shared_window_kind_from_metadata(
                                        &metadata, window_id,
                                    );
                                let share_instance_id = crate::transport::publisher::
                                    shared_window_share_instance_from_metadata(&metadata, window_id);
                                let source_title =
                                    source_title_for_kind(source_kind, &source_title);
                                let source_url =
                                    crate::transport::publisher::shared_window_url_from_metadata(
                                        &metadata, window_id,
                                    );
                                let owner_palette_index =
                                    crate::transport::publisher::identity_palette_index_from_metadata(
                                        &metadata,
                                    );
                                let owner_display_name = participant_for_retry.name();
                                let owner_display_name = if owner_display_name.is_empty() {
                                    owner_identity_for_retry.clone()
                                } else {
                                    owner_display_name
                                };
                                log::info!(
                                    "compositor feed: applying metadata that raced window creation for window {window_id} from '{owner_display_name}' ({owner_identity_for_retry})"
                                );
                                let remote_control_disallowed = !crate::transport::publisher::
                                    shared_window_remote_control_allowed_from_metadata(
                                        &metadata, window_id,
                                    );
                                crate::compositor::update_window_metadata(
                                    &app_for_retry,
                                    window_id,
                                    &owner_identity_for_retry,
                                    &owner_display_name,
                                    &source_title,
                                    source_url,
                                    Some(source_scale),
                                    !remote_control_disallowed,
                                    remote_control_disallowed,
                                    owner_palette_index,
                                    share_instance_id,
                                );
                                return;
                            }
                            log::debug!(
                                "compositor feed: window {window_id} metadata race-retry gave up after 10 attempts (no metadata landed, or window never appeared)"
                            );
                        });
                    }

                    let app_for_frames = app.clone();
                    let owner_identity_for_frames = owner_identity.clone();
                    let track_name_for_frames = track_name.clone();
                    let generation_for_frames = generation.clone();
                    let window_states_for_frames = window_states.clone();
                    let receive_key_for_frames = receive_key.clone();
                    tokio::spawn(async move {
                        // #907: cloned before `.rtc_track()` below consumes
                        // `video_track` -- this handle drives the periodic
                        // `get_stats()` poll for the quality-based starvation
                        // trigger (see `starvation_action_for_macos`).
                        // `RemoteVideoTrack` is `Clone` (an `Arc` inside).
                        let video_track_for_stats = video_track.clone();
                        let rtc_track = video_track.rtc_track();
                        // Keep a handle after building the stream (which
                        // takes it by value) so `set_enabled(false)` can be
                        // called on it once this loop exits -- mirrors the
                        // Windows receiver's existing use of the same setter
                        // (`spawn_windows_decode_loop`).
                        let mut stream = NativeVideoStream::new(rtc_track.clone());
                        let mut logged_buffer_type = false;
                        let mut warned_software_fallback = false;
                        let mut software_fallbacks = 0u64;
                        let mut invalid_dimension_frames = 0u64;
                        // #907 starvation watchdog state -- see
                        // `starvation_action_for_macos`. Structurally the
                        // same state machine ported from the Windows decode
                        // loop (`spawn_windows_decode_loop`), plus the
                        // quality-poll fields the liveness-only Windows guard
                        // doesn't need.
                        let mut first_frame_received = false;
                        let mut last_frame_seen = std::time::Instant::now();
                        let mut starved = false;
                        let mut starved_since: Option<std::time::Instant> = None;
                        let mut consecutive_probe_failures: u32 = 0;
                        let mut probe_outstanding = false;
                        let mut last_quality_check = std::time::Instant::now();
                        let mut prev_qp_sum: u64 = 0;
                        let mut prev_frames_decoded: u32 = 0;
                        let mut consecutive_high_qp_samples: u32 = 0;
                        let mut qp_signal_available: Option<bool> = None;
                        let mut last_stats_error_logged: Option<std::time::Instant> = None;
                        const STATS_ERROR_LOG_THROTTLE: Duration = Duration::from_secs(60);
                        // #907 review finding 1 (CRITICAL): this MUST be a
                        // persistent `Interval` created once here, not
                        // `tokio::time::sleep(STARVATION_CHECK_INTERVAL)`
                        // built fresh inside the `select!` below. A `sleep`
                        // constructed inside a `select!` arm is a brand-new
                        // future every loop iteration -- at any frame rate
                        // faster than the interval (i.e. any healthy stream
                        // at all) the frame branch wins the race EVERY time
                        // and the sleep never gets a chance to elapse. The
                        // exact incident this quality check exists to catch
                        // is frames arriving continuously at QP 31.1 -- with
                        // a recreated sleep, this tick would never fire while
                        // that was happening, so `get_stats()` would never
                        // be called and the QP downgrade could never trigger
                        // for the case it was built for. Confirmed by
                        // adversarial review (counselors #907) and fixed here
                        // the same way as the Windows decode loop's
                        // equivalent tick (`spawn_windows_decode_loop`).
                        let mut watchdog_tick = tokio::time::interval(STARVATION_CHECK_INTERVAL);
                        watchdog_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        loop {
                            let frame = tokio::select! {
                                _ = watchdog_tick.tick() => {
                                    if !first_frame_received {
                                        // Subscription negotiation still in
                                        // flight; nothing to evaluate yet.
                                        continue;
                                    }
                                    if last_quality_check.elapsed() >= QUALITY_STATS_POLL_INTERVAL {
                                        last_quality_check = std::time::Instant::now();
                                        match video_track_for_stats.get_stats().await {
                                        Ok(stats) => {
                                            let inbound_video = stats.iter().find_map(|s| match s {
                                                RtcStats::InboundRtp(inbound)
                                                    if inbound.stream.kind == "video" =>
                                                {
                                                    Some(inbound.inbound.clone())
                                                }
                                                _ => None,
                                            });
                                            match inbound_video {
                                                Some(sample) => {
                                                    match inbound_qp_sample(
                                                        prev_qp_sum,
                                                        prev_frames_decoded,
                                                        sample.qp_sum,
                                                        sample.frames_decoded,
                                                    ) {
                                                        QpSample::Average(avg_qp) => {
                                                            if qp_signal_available != Some(true) {
                                                                qp_signal_available = Some(true);
                                                                log::info!(
                                                                    "compositor feed: window {window_id} inbound QP telemetry available (avg {avg_qp:.1}); quality-based starvation guard active (#907)"
                                                                );
                                                            }
                                                            if avg_qp >= QUALITY_DOWNGRADE_AVG_QP {
                                                                consecutive_high_qp_samples =
                                                                    consecutive_high_qp_samples.saturating_add(1);
                                                            } else {
                                                                consecutive_high_qp_samples = 0;
                                                            }
                                                        }
                                                        QpSample::Unsupported => {
                                                            // #907 review finding 8: explicitly
                                                            // reset (not just "leave untouched") --
                                                            // once we know this decoder path never
                                                            // populates qp_sum, a prior streak of
                                                            // genuine `Average` samples must not
                                                            // keep counting toward a downgrade off
                                                            // of stale evidence.
                                                            consecutive_high_qp_samples = 0;
                                                            if qp_signal_available.is_none() {
                                                                qp_signal_available = Some(false);
                                                                log::info!(
                                                                    "compositor feed: window {window_id} decoder does not populate inbound qp_sum; quality-based starvation guard disabled, liveness-only (#907)"
                                                                );
                                                            }
                                                        }
                                                        // Deliberately NOT reset here: a poll that
                                                        // landed between decoded frames is an
                                                        // ABSENCE of evidence, not evidence of
                                                        // health, and resetting on it would let a
                                                        // genuinely sustained high-QP streak get
                                                        // broken up by poll-boundary luck alone.
                                                        QpSample::NoNewFrames => {}
                                                    }
                                                    prev_qp_sum = sample.qp_sum;
                                                    prev_frames_decoded = sample.frames_decoded;
                                                }
                                                None => {
                                                    if qp_signal_available.is_none() {
                                                        qp_signal_available = Some(false);
                                                        log::info!(
                                                            "compositor feed: window {window_id} no inbound-rtp video stats found; quality-based starvation guard disabled, liveness-only (#907)"
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            // #907 review finding 8: `if let Ok(stats) = ...`
                                            // with no `else` silently swallowed every
                                            // `get_stats()` failure -- a persistently broken
                                            // stats poll (and the quality-based guard riding
                                            // on it) would have left no trace at all.
                                            // Throttled, not per-tick.
                                            let should_log = last_stats_error_logged
                                                .is_none_or(|last| last.elapsed() >= STATS_ERROR_LOG_THROTTLE);
                                            if should_log {
                                                last_stats_error_logged = Some(std::time::Instant::now());
                                                log::warn!(
                                                    "compositor feed: window {window_id} get_stats() failed: {error:?} (throttled to once per {}s; quality-based starvation guard cannot observe QP while this persists)",
                                                    STATS_ERROR_LOG_THROTTLE.as_secs()
                                                );
                                            }
                                        }
                                        }
                                    }
                                    let since_last_frame = last_frame_seen.elapsed();
                                    let since_starved = starved_since.map(|t| t.elapsed());
                                    match starvation_action_for_macos(
                                        since_last_frame,
                                        starved,
                                        since_starved,
                                        consecutive_probe_failures,
                                        consecutive_high_qp_samples,
                                    ) {
                                        StarvationAction::Keep => {}
                                        StarvationAction::DowngradeToLow => {
                                            if probe_outstanding {
                                                consecutive_probe_failures =
                                                    consecutive_probe_failures.saturating_add(1);
                                            } else {
                                                consecutive_probe_failures = 0;
                                            }
                                            probe_outstanding = false;
                                            starved = true;
                                            starved_since = Some(std::time::Instant::now());
                                            // Reset the quality counter: the
                                            // downgrade already acted on it,
                                            // and it must not immediately
                                            // re-trigger once back on a
                                            // (lower-resolution) layer.
                                            consecutive_high_qp_samples = 0;
                                            let publication = window_publications()
                                                .lock_unpoisoned()
                                                .get(&(owner_identity_for_frames.clone(), window_id))
                                                .cloned();
                                            if let Some(pub_) = publication {
                                                pub_.set_video_quality(VideoQuality::Low);
                                            }
                                            log::warn!(
                                                "compositor feed: window {window_id} downgrading subscription to LOW (no frame for >= {}s, or sustained high QP >= {consecutive_high_qp_samples} consecutive samples); probe failures so far: {consecutive_probe_failures} (#907)",
                                                STARVATION_DOWNGRADE_AFTER.as_secs()
                                            );
                                        }
                                        StarvationAction::ProbeHigh => {
                                            probe_outstanding = true;
                                            starved = false;
                                            starved_since = None;
                                            let publication = window_publications()
                                                .lock_unpoisoned()
                                                .get(&(owner_identity_for_frames.clone(), window_id))
                                                .cloned();
                                            if let Some(pub_) = publication {
                                                pub_.set_video_quality(VideoQuality::High);
                                            }
                                            log::warn!(
                                                "compositor feed: window {window_id} recovery probe: re-requesting HIGH after {}s on LOW (probe failure {})",
                                                since_starved.unwrap_or_default().as_secs(),
                                                consecutive_probe_failures + 1
                                            );
                                        }
                                    }
                                    continue;
                                }
                                outcome = next_frame_or_cancelled(&mut stream, &cancel_token) => outcome,
                            };
                            match &frame {
                                FrameOrCancelled::Frame(Some(_)) => {
                                    if !first_frame_received {
                                        first_frame_received = true;
                                    }
                                    last_frame_seen = std::time::Instant::now();
                                    // #907 review finding 7: the FIRST frame
                                    // received after issuing a recovery probe
                                    // is conclusive success evidence for THAT
                                    // probe -- clear it and reset the failure
                                    // streak here. Without this, an
                                    // outstanding probe that actually
                                    // succeeded stays "outstanding" forever,
                                    // and an unrelated stall/QP event much
                                    // later gets miscounted as a continuation
                                    // of that already-succeeded probe's
                                    // failure history.
                                    if probe_outstanding {
                                        probe_outstanding = false;
                                        consecutive_probe_failures = 0;
                                    }
                                }
                                _ => {}
                            }
                            let frame = match frame {
                                FrameOrCancelled::Cancelled => {
                                    // #682: the normal exit path now. This
                                    // window's `ReceiveWindowState` entry was
                                    // removed or replaced
                                    // (`remove_window_state`/
                                    // `insert_window_state`), so there is
                                    // nothing left for this loop to feed.
                                    log::info!(
                                        "compositor feed: decode loop cancelled for window {window_id} (receive state removed or replaced, #682)"
                                    );
                                    break;
                                }
                                FrameOrCancelled::Frame(None) => {
                                    // Not observed in practice (#682's
                                    // mechanics writeup: the underlying
                                    // `VideoFrameQueue` is only closed by this
                                    // task's own `NativeVideoStream::drop`,
                                    // which cannot run before this `await`
                                    // returns) -- kept as a safety net, not
                                    // relied on.
                                    log::info!(
                                        "compositor feed: video stream ended for window {window_id}"
                                    );
                                    break;
                                }
                                FrameOrCancelled::Frame(Some(frame)) => frame,
                            };
                            // #682 counselors-review follow-up: `select!` is
                            // biased toward `cancelled()` (see
                            // `next_frame_or_cancelled`), but if a frame was
                            // ALREADY dequeued from the stream in the same
                            // poll that raced a concurrent cancellation, this
                            // loop can still observe one stale frame after
                            // its entry was replaced -- `mark_frame_received`
                            // would then attribute it to the NEW state. Drop
                            // it here instead: cheaper than trying to make
                            // the race itself atomic, and turns "bounded to
                            // one misattributed frame" into "bounded to
                            // zero."
                            if cancel_token.is_cancelled() {
                                log::debug!(
                                    "compositor feed: dropping one already-dequeued frame for window {window_id} after cancellation"
                                );
                                continue;
                            }
                            if !generation_for_frames.is_current() {
                                log::debug!(
                                    "compositor feed: video stream exiting for stale room generation"
                                );
                                break;
                            }
                            // Before `mark_frame_received` on purpose: a
                            // stream yielding ONLY invalid frames must look
                            // frameless to the no-frame watchdog so its
                            // retire/resubscribe repair still fires.
                            let frame_width = frame.buffer.width();
                            let frame_height = frame.buffer.height();
                            if !decoded_frame_dimensions_valid(frame_width, frame_height) {
                                invalid_dimension_frames += 1;
                                if invalid_dimension_frames == 1
                                    || invalid_dimension_frames % INVALID_DIMENSION_LOG_EVERY == 0
                                {
                                    log::warn!(
                                        "compositor feed: window {window_id} dropping decoded frame with invalid dimensions {frame_width}x{frame_height} (count {invalid_dimension_frames}) -- to_i420 on such a frame aborts the process"
                                    );
                                }
                                continue;
                            }
                            let Some(color_profile) = mark_frame_received(
                                &window_states_for_frames,
                                &receive_key_for_frames,
                            ) else {
                                // #682 item 3: a stateless frame (this
                                // window's state was removed/replaced but the
                                // cancellation hasn't taken effect on this
                                // exact frame yet) does none of the
                                // diagnostics/CVBuffer/push_frame work below.
                                continue;
                            };
                            let receive_timestamp_us = now_us();
                            let frame_id = frame.frame_metadata.as_ref().and_then(|m| m.frame_id);
                            crate::diagnostics::record_native_receiver_frame(
                                &app_for_frames,
                                &owner_identity_for_frames,
                                &track_name_for_frames,
                                frame_id,
                            );
                            if let Some((capture_timestamp_us, frame_id)) = frame
                                .frame_metadata
                                .as_ref()
                                .map(|m| (m.user_timestamp, m.frame_id))
                                .and_then(|(ts, id)| ts.map(|ts| (ts, id)))
                            {
                                // Crisp mode (#384 Phase 1 spike, not wired up
                                // yet): `capture_timestamp_us` here is the SAME
                                // axis `crisp_still::StillValidity` uses to
                                // invalidate a stale still (see that type's
                                // doc comment) -- a follow-up should call
                                // `crisp_still::StillValidity::observe_video_frame(capture_timestamp_us)`
                                // (per-window_id) right here, on every real
                                // decoded video frame, so a still can never
                                // outlive the video frame that supersedes it.
                                crate::diagnostics::record_glass_to_glass_frame_timing(
                                    &app_for_frames,
                                    &owner_identity_for_frames,
                                    &track_name_for_frames,
                                    capture_timestamp_us,
                                    receive_timestamp_us,
                                    frame_id,
                                );
                            }
                            let buffer_type = frame.buffer.buffer_type();
                            if !logged_buffer_type {
                                // Verification hook (see task's honesty
                                // requirement): log the REAL buffer type of
                                // the first decoded frame for this track, so
                                // whether the zero-copy `Native`/CVPixelBuffer
                                // path is actually in use is a directly
                                // observed fact, not an assumption. Every
                                // other build in this module only ever read
                                // `frame.width`/`height`, never this.
                                log::info!(
                                    "compositor feed: window {window_id} first frame buffer_type={buffer_type:?} {}x{}",
                                    frame.buffer.width(),
                                    frame.buffer.height()
                                );
                                logged_buffer_type = true;
                            }

                            if buffer_type == VideoBufferType::Native {
                                if let Some(native) = frame.buffer.as_native() {
                                    let cv_pixel_buffer = native.get_cv_pixel_buffer();
                                    if !cv_pixel_buffer.is_null() {
                                        crate::native_display::attach_video_color_profile_to_cv_pixel_buffer(
                                            cv_pixel_buffer,
                                            color_profile,
                                        );
                                        crate::compositor::push_frame(
                                            &app_for_frames,
                                            &owner_identity_for_frames,
                                            window_id,
                                            cv_pixel_buffer,
                                            frame.buffer.width(),
                                            frame.buffer.height(),
                                        );
                                        continue;
                                    }
                                }
                            }
                            let i420 = frame.buffer.to_i420();
                            software_fallbacks += 1;
                            if !warned_software_fallback {
                                warned_software_fallback = true;
                                log::warn!(
                                    "compositor feed: software decode fallback activated for window {window_id} track '{track_name_for_frames}'"
                                );
                            }
                            crate::diagnostics::record_native_receiver_software_fallback(
                                &app_for_frames,
                                &owner_identity_for_frames,
                                &track_name_for_frames,
                                software_fallbacks,
                            );
                            let (y, u, v) = i420.data();
                            let (y_stride, u_stride, v_stride) = i420.strides();
                            match crate::native_display::i420_to_cv_pixel_buffer_with_color_profile(
                                crate::native_display::I420Planes {
                                    y,
                                    y_stride,
                                    u,
                                    u_stride,
                                    v,
                                    v_stride,
                                    width: i420.width(),
                                    height: i420.height(),
                                },
                                color_profile,
                            ) {
                                Ok(pixel_buffer) => {
                                    crate::compositor::push_frame(
                                        &app_for_frames,
                                        &owner_identity_for_frames,
                                        window_id,
                                        pixel_buffer.as_ptr(),
                                        i420.width(),
                                        i420.height(),
                                    );
                                }
                                Err(e) => {
                                    log::warn!(
                                        "compositor feed: window {window_id} failed software I420->CVPixelBuffer fallback for buffer_type={buffer_type:?}: {e:?}"
                                    );
                                }
                            }
                        }
                        // #682 item 2: on every exit path (cancelled, stale
                        // generation, or the unreached natural-end fallback
                        // above), stop libwebrtc from decoding this track on
                        // our behalf. This is the one lever in this loop that
                        // plausibly reclaims actual decode CPU, not just
                        // Rust-side bookkeeping -- decode itself happens
                        // upstream of `NativeVideoStream`'s sink, inside
                        // libwebrtc's `VideoReceiveStream2`, before any frame
                        // reaches this task at all. Cancelling the task (via
                        // `insert_window_state`/`remove_window_state`) and
                        // dropping `stream` here reclaim the Rust-side
                        // per-frame work and this task's own memory/refs --
                        // a DIFFERENT saving from this call, which is why
                        // both are done rather than relying on cancellation
                        // alone.
                        //
                        // Counselors-review guard: `set_enabled` is a plain
                        // sync FFI call on the shared `RtcVideoTrack` handle,
                        // not scoped to this subscription -- if a SUCCESSOR
                        // loop for this same key is already live (a
                        // replacement-insert race), disabling here would
                        // silently kill the successor's live feed with
                        // nothing to ever re-enable it (no #682 exit path
                        // calls `set_enabled(true)`), a delayed black/frozen
                        // share only the 30s watchdog would eventually
                        // surface. Every `add_subscribed_media_track` call
                        // observed in the pinned livekit SDK builds a fresh
                        // `RemoteVideoTrack`, so two `TrackSubscribed` events
                        // are not currently expected to share a handle -- but
                        // that is an SDK-internal invariant this code doesn't
                        // control, so check the cheap, structural signal
                        // instead of assuming it holds.
                        let successor_is_live = window_states_for_frames
                            .lock_unpoisoned()
                            .contains_key(&receive_key_for_frames);
                        if successor_is_live {
                            log::debug!(
                                "compositor feed: window {window_id} skipping set_enabled(false) -- a successor receive state is already live for this key"
                            );
                        } else {
                            rtc_track.set_enabled(false);
                        }
                    });
                }
                RoomEvent::TrackUnsubscribed {
                    track,
                    publication,
                    participant,
                } => {
                    let RemoteTrack::Video(video_track) = track else {
                        continue;
                    };
                    if let Some(window_id) =
                        crate::transport::publisher::window_id_from_track_name(&video_track.name())
                    {
                        let owner_identity = participant.identity().to_string();
                        let receive_key = ReceiveWindowKey::new(owner_identity.clone(), window_id);
                        apply_teardown_decision(
                            &app,
                            &room,
                            &window_states,
                            &receive_key,
                            &publication.sid().to_string(),
                            "unsubscribe",
                            false,
                            crate::compositor::RemoveWindowReason::TrackUnsubscribed,
                        );
                    }
                }
                // A remote peer *unpublishing* a shared window (e.g. clicking
                // "stop sharing") fires `TrackUnpublished`, and in this SDK
                // version that does NOT reliably also fire `TrackUnsubscribed`
                // on the subscriber -- so without this arm the compositor window
                // would linger showing a frozen last frame after the sharer
                // stopped. `publication.name()` carries the same
                // `petal-window-<id>` track name we key windows by.
                RoomEvent::TrackUnpublished {
                    publication,
                    participant,
                } => {
                    if let Some(window_id) =
                        crate::transport::publisher::window_id_from_track_name(&publication.name())
                    {
                        let owner_identity = participant.identity().to_string();
                        let receive_key = ReceiveWindowKey::new(owner_identity.clone(), window_id);
                        apply_teardown_decision(
                            &app,
                            &room,
                            &window_states,
                            &receive_key,
                            &publication.sid().to_string(),
                            "unpublish",
                            true,
                            crate::compositor::RemoveWindowReason::TrackUnpublished,
                        );
                    }
                }
                // #51 waterproofing: `TrackPublished` fires as soon as the SFU
                // registers a remote participant's new track -- BEFORE
                // `auto_subscribe` completes the actual `TrackSubscribed`
                // handshake above. Logging it gives a diagnostic anchor point:
                // if a share never becomes visible, "was TrackPublished seen
                // at all for this identity/track" answers whether the publish
                // reached this client's room-event stream in the first place
                // (ruling out "Bob never actually published") versus the
                // subscribe step silently never completing after it.
                RoomEvent::TrackPublished {
                    publication,
                    participant,
                } => {
                    let owner_identity = participant.identity().to_string();
                    if owner_identity == local_identity {
                        continue;
                    }
                    let track_name = publication.name();
                    let is_window_share =
                        crate::transport::publisher::window_id_from_track_name(&track_name)
                            .is_some();
                    log::info!(
                        "compositor feed: track published sid={} name='{track_name}' kind={:?} from '{owner_identity}' (window_share={is_window_share}); awaiting auto-subscribe",
                        publication.sid(),
                        publication.kind()
                    );
                }
                // #51 waterproofing: previously unhandled (fell into the `_ =>
                // {}` catch-all with zero log trace). This is the room event
                // for "auto_subscribe tried to subscribe to a track and
                // failed" -- exactly the shape of bug this file exists to
                // catch (a remote share that *was* published but never
                // becomes a visible compositor window for this viewer, with
                // no prior log line explaining why). Surfacing it turns a
                // silent no-op into a diagnosable WARN.
                RoomEvent::TrackSubscriptionFailed {
                    participant,
                    error,
                    track_sid,
                } => {
                    log::warn!(
                        "compositor feed: track subscription FAILED sid={track_sid} from '{}': {error} -- that participant's share may silently never appear for this viewer",
                        participant.identity()
                    );
                }
                RoomEvent::TrackMuted {
                    participant,
                    publication,
                } => {
                    if participant.identity().to_string() == local_identity {
                        continue;
                    }
                    let track_name = publication.name();
                    let Some(window_id) =
                        crate::transport::publisher::window_id_from_track_name(&track_name)
                    else {
                        continue;
                    };
                    window_states
                        .lock_unpoisoned()
                        .entry(ReceiveWindowKey::new(
                            participant.identity().to_string(),
                            window_id,
                        ))
                        .or_insert_with(|| {
                            ReceiveWindowState::new(
                                participant.identity().to_string(),
                                track_name.clone(),
                                shared_window_color_profile_or_default(
                                    &participant.metadata(),
                                    window_id,
                                ),
                                Instant::now(),
                                // No decode loop owns this placeholder entry
                                // (TrackMuted arriving before TrackSubscribed
                                // has one to reuse) -- an inert token nothing
                                // ever races against or cancels twice.
                                CancellationToken::new(),
                            )
                        })
                        .track_muted = true;
                    log::info!("compositor feed: track muted for window {window_id}");
                    crate::compositor::set_window_media_paused(
                        &app,
                        &participant.identity().to_string(),
                        window_id,
                        true,
                    );
                    crate::diagnostics::record_native_video_stream_state(
                        &app,
                        &participant.identity().to_string(),
                        &track_name,
                        "paused",
                        "livekit-rust-track-muted",
                    );
                }
                RoomEvent::TrackUnmuted {
                    participant,
                    publication,
                } => {
                    if participant.identity().to_string() == local_identity {
                        continue;
                    }
                    let track_name = publication.name();
                    let Some(window_id) =
                        crate::transport::publisher::window_id_from_track_name(&track_name)
                    else {
                        continue;
                    };
                    {
                        let mut states = window_states.lock_unpoisoned();
                        let state = states
                            .entry(ReceiveWindowKey::new(
                                participant.identity().to_string(),
                                window_id,
                            ))
                            .or_insert_with(|| {
                                ReceiveWindowState::new(
                                    participant.identity().to_string(),
                                    track_name.clone(),
                                    shared_window_color_profile_or_default(
                                        &participant.metadata(),
                                        window_id,
                                    ),
                                    Instant::now(),
                                    // See the identical TrackMuted placeholder
                                    // above: no decode loop owns this entry.
                                    CancellationToken::new(),
                                )
                            });
                        state.track_muted = false;
                        state.last_frame_at.get_or_insert_with(Instant::now);
                    }
                    log::info!("compositor feed: track unmuted for window {window_id}");
                    crate::compositor::set_window_media_paused(
                        &app,
                        &participant.identity().to_string(),
                        window_id,
                        false,
                    );
                    crate::diagnostics::record_native_video_stream_state(
                        &app,
                        &participant.identity().to_string(),
                        &track_name,
                        "active",
                        "livekit-rust-track-unmuted",
                    );
                }
                RoomEvent::Reconnecting => {
                    reconnect_lifecycle.set_reconnecting(true);
                    set_reconnecting(&window_states, true);
                }
                RoomEvent::Reconnected => {
                    reconnect_lifecycle.set_reconnecting(false);
                    set_reconnecting(&window_states, false);
                }
                RoomEvent::ParticipantMetadataChanged {
                    participant,
                    metadata,
                    ..
                } => {
                    let owner_identity = participant.identity().to_string();
                    if owner_identity == local_identity {
                        continue;
                    }
                    let owner_display_name = participant.name();
                    let owner_display_name = if owner_display_name.is_empty() {
                        owner_identity.clone()
                    } else {
                        owner_display_name
                    };
                    let mut window_ids =
                        crate::compositor::window_ids_for_participant(&owner_identity);
                    // #875 review F3: also refresh a RETIRED (viewer-hidden)
                    // window's z-rank while it's hidden, not just open ones
                    // -- otherwise `plan_participant_raise` restores it into
                    // its stale at-hide position instead of the sharer's
                    // current order. `update_window_metadata` below already
                    // safely no-ops for a window_id with no open entry, so
                    // widening this enumeration only feeds the z-rank path.
                    window_ids.extend(crate::compositor::retired_window_ids_for_participant(
                        &owner_identity,
                    ));
                    #[cfg(target_os = "windows")]
                    window_ids.extend(
                        crate::windows_compositor::compositor_list_windows()
                            .into_iter()
                            .filter(|window| window.owner_identity == owner_identity)
                            .map(|window| window.window_id),
                    );
                    window_ids.sort_unstable();
                    window_ids.dedup();
                    for window_id in window_ids {
                        let source_scale_metadata =
                            crate::transport::publisher::shared_window_scale_from_metadata(
                                &metadata, window_id,
                            );
                        let Some(source_scale) = source_scale_metadata else {
                            continue;
                        };
                        let source_title =
                            crate::transport::publisher::shared_window_title_from_metadata(
                                &metadata, window_id,
                            )
                            .unwrap_or_else(|| format!("Shared window {window_id}"));
                        let source_kind =
                            crate::transport::publisher::shared_window_kind_from_metadata(
                                &metadata, window_id,
                            );
                        let share_instance_id = crate::transport::publisher::
                            shared_window_share_instance_from_metadata(&metadata, window_id);
                        let source_title = source_title_for_kind(source_kind, &source_title);
                        let source_url =
                            crate::transport::publisher::shared_window_url_from_metadata(
                                &metadata, window_id,
                            );
                        let owner_palette_index =
                            crate::transport::publisher::identity_palette_index_from_metadata(
                                &metadata,
                            );
                        let remote_control_disallowed = !crate::transport::publisher::
                            shared_window_remote_control_allowed_from_metadata(
                                &metadata, window_id,
                            );
                        log::info!(
                            "compositor feed: metadata refreshed for native window {window_id} from '{owner_display_name}' ({owner_identity}); remote control {}",
                            if remote_control_disallowed { "DENIED by the sharer" } else { "enabled" }
                        );
                        crate::compositor::update_window_metadata(
                            &app,
                            window_id,
                            &owner_identity,
                            &owner_display_name,
                            &source_title,
                            source_url,
                            Some(source_scale),
                            !remote_control_disallowed,
                            remote_control_disallowed,
                            owner_palette_index,
                            share_instance_id,
                        );
                        // #875: `petalWindowZOrder` -- store this window's
                        // front-to-back rank within the sharer's
                        // currently-shared subset, if the sharer publishes
                        // it. Absent/malformed metadata (older sharer, or no
                        // order published yet) decodes to `None`, which
                        // clears any stale rank rather than leaving one from
                        // before this window dropped out of the shared set.
                        crate::compositor::update_window_z_rank(
                            &owner_identity,
                            window_id,
                            crate::transport::publisher::shared_window_z_rank_from_metadata(
                                &metadata, window_id,
                            ),
                        );
                        #[cfg(target_os = "windows")]
                        {
                            let share_instance_id =
                                crate::transport::publisher::shared_window_share_instance_from_metadata(
                                    &metadata,
                                    window_id,
                                );
                            let control_mode = crate::transport::publisher::
                                shared_window_control_mode_from_metadata(&metadata, window_id);
                            crate::windows_compositor::update_window_metadata(
                                &app,
                                (owner_identity.clone(), window_id),
                                owner_display_name.clone(),
                                source_title.clone(),
                                source_kind,
                                true,
                                share_instance_id,
                                control_mode,
                            )
                            .await;
                        }

                        // #251: color_profile was previously read only at
                        // TrackSubscribed. Under #249's >3s signaling-stall
                        // path, the track can publish before metadata
                        // arrives, so a receiver can subscribe while
                        // color_profile is still the fallback default and --
                        // unlike title/kind/url/scale above -- never see it
                        // corrected. Refresh it here too, fill-only-when-
                        // present so a metadata update that doesn't carry
                        // color info can't clobber an already-correct value.
                        if let Some(new_profile) =
                            refreshed_color_profile_from_metadata(&metadata, window_id)
                        {
                            let receive_key =
                                ReceiveWindowKey::new(owner_identity.clone(), window_id);
                            if update_receive_window_color_profile(
                                &window_states,
                                &receive_key,
                                new_profile,
                            ) {
                                log::info!(
                                    "compositor feed: color_profile refreshed for window {window_id} from '{owner_display_name}' ({owner_identity}) -> {new_profile:?}"
                                );
                            }
                        }
                    }
                }
                RoomEvent::ParticipantDisconnected(participant) => {
                    let identity = participant.identity().to_string();
                    // A full LiveKit reconnect dispatches this event before
                    // `RoomEvent::Reconnecting`, so neither event order nor
                    // the room's current publication set can classify it. Do
                    // not drop receive/publication tracking here: an orphaned
                    // tracked entry is what lets reconciliation retire a real
                    // departure instead of creating a permanent held phantom.
                    handle_participant_disconnected(&participant.name(), &identity, |identity| {
                        crate::compositor::hold_windows_for_participant_reconnect(&app, identity);
                    });
                }
                _ => {}
            }
        }
        cancel_all_window_states(&window_states);
    });
}

/// Cancel every remaining `window_states` entry's decode loop and drop the
/// entries (#682 follow-up, counselors review). Every `break` out of
/// `start_compositor_feed`'s event loop above (stale generation, closed
/// events channel -- i.e. ordinary room leave/rejoin, not just a republish)
/// used to drop `window_states` with no cleanup. Dropping a
/// `CancellationToken` does NOT fire `cancelled()` -- so without this call,
/// every decode loop still parked in `stream.next()` at leave/rejoin time
/// leaked forever, reproducing #682's exact defect (a task outliving its
/// state) on the most common exit path, not only the republish path the
/// rest of this fix targets. A named function rather than an inline block so
/// the test below drives this exact code, not a duplicated restatement of
/// it.
fn cancel_all_window_states(states: &Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>>) {
    for (_, state) in states.lock_unpoisoned().drain() {
        state.cancel.cancel();
    }
}

fn source_title_for_kind(
    kind: crate::transport::publisher::SharedSourceKind,
    source_title: &str,
) -> String {
    match kind {
        crate::transport::publisher::SharedSourceKind::Window => source_title.to_string(),
        crate::transport::publisher::SharedSourceKind::DisplayRegion => "Petal View".to_string(),
        crate::transport::publisher::SharedSourceKind::Display => {
            let trimmed = source_title.trim();
            if trimmed.is_empty() {
                "Screen".to_string()
            } else if trimmed.to_ascii_lowercase().starts_with("screen") {
                trimmed.to_string()
            } else {
                format!("Screen - {trimmed}")
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn start_compositor_feed(
    app: &tauri::AppHandle,
    mut events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    // `room` queries the SFU's live publication set for the receiver-side
    // resize-republish decision (does a replacement publication for the
    // window already exist?).
    room: Arc<Room>,
    local_identity: String,
    generation: RoomGeneration,
    on_forced_disconnect: tokio::sync::mpsc::UnboundedSender<()>,
) {
    // The Windows feed creates/closes Tauri WebviewWindows for remote shares
    // (the surface route renders the header), so it needs the app handle.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Receiver-side cadence log (verification item 10's "measured rather
        // than inferred"): per 5s interval, how many decoded frames were
        // dispatched to the native compositor.
        let frames_this_interval = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut health_interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            let event = tokio::select! {
                _ = health_interval.tick() => {
                    if !generation.is_current() {
                        log::debug!("windows compositor feed: exiting for stale room generation");
                        break;
                    }
                    let frames = frames_this_interval.swap(0, std::sync::atomic::Ordering::Relaxed);
                    let paused = crate::windows_compositor::off_screen_window_count();
                    if paused > 0 {
                        // A 0-fps health line with every window minimized/
                        // hidden would read as a broken receiver; say why.
                        log::info!(
                            "windows compositor feed: {frames} frame(s) dispatched to compositor in last 5s ({:.1} fps, {paused} window(s) paused off-screen)",
                            frames as f64 / 5.0
                        );
                    } else {
                        log::info!(
                            "windows compositor feed: {frames} frame(s) dispatched to compositor in last 5s ({:.1} fps)",
                            frames as f64 / 5.0
                        );
                    }
                    // Windows no-frame watchdog: safety net ONLY (macOS
                    // parity, 30s). The pinned SDK should deliver
                    // `TrackUnpublished` for an explicit stop-sharing but
                    // does not on Windows (under investigation); this retires
                    // only after 30s of silence AND the SFU holding no
                    // publication (#627), so a poor-network stall never
                    // closes a live window.
                    let now = std::time::Instant::now();
                    for key in crate::windows_compositor::active_frame_keys() {
                        if !generation.is_current() {
                            break;
                        }
                        let Some(last) = crate::windows_compositor::last_frame_at(&key) else {
                            continue;
                        };
                        if now.duration_since(last) < NO_FRAME_RETIRE_AFTER {
                            continue;
                        }
                        let (owner_identity, window_id) = &key;
                        if window_publication_exists(&room, owner_identity, *window_id, &[]) {
                            log::debug!(
                                "windows compositor feed: window {window_id} from '{owner_identity}' has no frames for >= {}s but the SFU still holds the publication; keeping the frozen window",
                                NO_FRAME_RETIRE_AFTER.as_secs()
                            );
                            continue;
                        }
                        log::warn!(
                            "windows compositor feed: no frames for window {window_id} from '{owner_identity}' for >= {}s and the SFU holds no publication; retiring frozen window",
                            NO_FRAME_RETIRE_AFTER.as_secs()
                        );
                        crate::windows_compositor::remove_window(&app, key.clone()).await;
                        crate::windows_compositor::drop_frame_timing(&key);
                    }
                    continue;
                }
                event = events.recv() => event,
            };
            let Some(event) = event else {
                break;
            };
            if !generation.is_current() {
                log::debug!("windows compositor feed: exiting for stale room generation");
                break;
            }
            match event {
                RoomEvent::TrackSubscribed {
                    track,
                    publication,
                    participant,
                } => {
                    if participant.identity().to_string() == local_identity {
                        continue;
                    }
                    let RemoteTrack::Video(video_track) = track else {
                        continue;
                    };
                    let track_name = video_track.name();
                    // EXACT `petal-window-<id>` prefix only — camera slugs
                    // must never parse as window ids (see the publisher
                    // contract test), so remote cameras stay on the gallery
                    // bridge and are never fed to the compositor.
                    let Some(window_id) =
                        crate::transport::publisher::window_id_from_track_name(&track_name)
                    else {
                        log::debug!(
                            "windows compositor feed: track '{track_name}' is not a window share; keeping it on the gallery bridge"
                        );
                        continue;
                    };
                    let owner_identity = participant.identity().to_string();
                    let key = (owner_identity.clone(), window_id);
                    // Record the window's current publication so the
                    // TrackUnpublished arm's sid guard (`resolve_teardown` ←
                    // `window_publications()`) can recognize a genuine
                    // unpublish as terminal. The macOS feed does this insert
                    // in its own TrackSubscribed arm; without it on Windows
                    // every unpublish reads `current_sid == None` and is
                    // skipped as "stale" — the frozen window lingers until
                    // the sharer disconnects.
                    window_publications()
                        .lock_unpoisoned()
                        .insert((owner_identity.clone(), window_id), publication.clone());
                    let source_title =
                        crate::transport::publisher::shared_window_title_from_metadata(
                            &participant.metadata(),
                            window_id,
                        )
                        .unwrap_or_else(|| track_name.clone());
                    let source_kind = crate::transport::publisher::shared_window_kind_from_metadata(
                        &participant.metadata(),
                        window_id,
                    );
                    let source_url = crate::transport::publisher::shared_window_url_from_metadata(
                        &participant.metadata(),
                        window_id,
                    );
                    let share_instance_id =
                        crate::transport::publisher::shared_window_share_instance_from_metadata(
                            &participant.metadata(),
                            window_id,
                        );
                    let remote_control_disallowed =
                        !crate::transport::publisher::shared_window_remote_control_allowed_from_metadata(
                            &participant.metadata(),
                            window_id,
                        );
                    let remote_control_available = (share_instance_id.is_some()
                        || crate::transport::publisher::shared_window_scale_from_metadata(
                            &participant.metadata(),
                            window_id,
                        )
                        .is_some())
                        && !remote_control_disallowed;
                    let control_mode =
                        crate::transport::publisher::shared_window_control_mode_from_metadata(
                            &participant.metadata(),
                            window_id,
                        );
                    // Human-readable handle for the header title — mirrors the
                    // macOS feed: participant name, falling back to the UUID
                    // identity when the peer set no display name.
                    let participant_name = participant.name();
                    let owner_display_name = if participant_name.is_empty() {
                        owner_identity.clone()
                    } else {
                        participant_name
                    };
                    log::info!(
                        "windows compositor feed: track subscribed for window {window_id} from '{owner_display_name}' ({owner_identity}) title='{source_title}'"
                    );
                    // Reattach (sender republish under the same window id):
                    // `create_window` is idempotent, so a fresh
                    // TrackSubscribed only starts a new decode loop; the
                    // window keeps its position and last presented frame.
                    // Shared subscription policy (same call as the macOS
                    // feed, so the two can't drift): captures the canonical
                    // encode resolution (used to size the window to the FULL
                    // source resolution, macOS parity — previously the
                    // window followed the first decoded low layer and stayed
                    // perpetually downscaled), registers the publication,
                    // requests the HIGH layer, and hints the canonical
                    // dimensions so the SFU serves the tier closest to the
                    // true resolution (without it, Windows parked on the low
                    // layer and the canonical-sized window upscaled q ~2.1x —
                    // the murky-text complaint).
                    let mut canonical_source_size = register_and_request_shared_window_subscription(
                        &publication,
                        &owner_identity,
                        window_id,
                    );
                    if canonical_source_size.is_none()
                        && source_kind
                            == crate::transport::publisher::SharedSourceKind::DisplayRegion
                    {
                        canonical_source_size = crate::transport::publisher::
                            shared_window_region_physical_size_from_metadata(
                                &participant.metadata(),
                                window_id,
                            );
                    }
                    if !crate::windows_compositor::window_open_for(&key) {
                        crate::windows_compositor::create_window(
                            &app,
                            key.clone(),
                            owner_display_name,
                            source_title,
                            source_url,
                            source_kind,
                            remote_control_available,
                            share_instance_id,
                            control_mode,
                            canonical_source_size,
                        )
                        .await;
                    } else {
                        log::debug!(
                            "windows compositor feed: window {window_id} already open -- treating this TrackSubscribed as a republish"
                        );
                    }
                    if let Some(size) = canonical_source_size {
                        // `create_window` is intentionally idempotent, but a
                        // sender resize can reuse the same window key. Refresh
                        // the source geometry before the replacement frames
                        // arrive so control/drawing coordinates do not retain
                        // the old publication dimensions.
                        crate::windows_compositor::update_window_canonical_source_size(
                            key.clone(),
                            size,
                        )
                        .await;
                    }
                    // #694: owns this decode loop's lifetime. Cancels
                    // whatever token a PRIOR loop for this same key was
                    // racing (the republish/replacement-insert case just
                    // above -- `window_open_for` was already true), so a
                    // republish stops the old loop instead of double-feeding
                    // the compositor alongside the new one. Mirrors #682's
                    // `insert_window_state` on macOS, adapted to this file's
                    // key-only registry (`windows_compositor` has no
                    // per-window async state map to hang the token off).
                    let cancel_token = crate::windows_compositor::install_decode_loop_token(&key);
                    spawn_windows_decode_loop(
                        video_track,
                        key,
                        source_kind == crate::transport::publisher::SharedSourceKind::DisplayRegion,
                        generation.clone(),
                        frames_this_interval.clone(),
                        cancel_token,
                    );
                }
                RoomEvent::ParticipantMetadataChanged {
                    participant,
                    metadata,
                    ..
                } => {
                    let owner_identity = participant.identity().to_string();
                    if owner_identity == local_identity {
                        continue;
                    }
                    let owner_display_name = participant.name();
                    let owner_display_name = if owner_display_name.is_empty() {
                        owner_identity.clone()
                    } else {
                        owner_display_name
                    };
                    for window in crate::windows_compositor::compositor_list_windows().await {
                        if window.owner_identity != owner_identity {
                            continue;
                        }
                        let window_id = window.window_id;
                        let source_title =
                            crate::transport::publisher::shared_window_title_from_metadata(
                                &metadata, window_id,
                            )
                            .unwrap_or_else(|| window.source_title.clone());
                        let source_kind =
                            crate::transport::publisher::shared_window_kind_from_metadata(
                                &metadata, window_id,
                            );
                        let source_url =
                            crate::transport::publisher::shared_window_url_from_metadata(
                                &metadata, window_id,
                            );
                        let share_instance_id =
                            crate::transport::publisher::shared_window_share_instance_from_metadata(
                                &metadata, window_id,
                            );
                        let remote_control_disallowed =
                            !crate::transport::publisher::shared_window_remote_control_allowed_from_metadata(
                                &metadata, window_id,
                            );
                        let remote_control_available = (share_instance_id.is_some()
                            || crate::transport::publisher::shared_window_scale_from_metadata(
                                &metadata, window_id,
                            )
                            .is_some())
                            && !remote_control_disallowed;
                        let control_mode =
                            crate::transport::publisher::shared_window_control_mode_from_metadata(
                                &metadata, window_id,
                            );
                        crate::windows_compositor::update_window_metadata(
                            &app,
                            (owner_identity.clone(), window_id),
                            owner_display_name.clone(),
                            source_title,
                            source_url,
                            source_kind,
                            remote_control_available,
                            share_instance_id,
                            control_mode,
                        )
                        .await;
                    }
                }
                RoomEvent::TrackUnsubscribed { track, .. } => {
                    // Do NOT remove the compositor window: it keeps the last
                    // presented frame. #694 correction (mirrors #682's finding
                    // on the macOS side): the decode loop does NOT end on its
                    // own here -- `stream.next()` merely not yielding does not
                    // make it return (the underlying frame queue is only
                    // closed by this same task's own `NativeVideoStream::drop`,
                    // which never runs while the task is parked awaiting the
                    // next frame). What actually stops a superseded loop is
                    // `windows_compositor::install_decode_loop_token`
                    // (TrackSubscribed republish, above) or
                    // `remove_window`/`remove_all_for`/`remove_all` on a real
                    // teardown -- both cancel the loop's token as part of the
                    // same operation. The sender's resize-republish fires
                    // TrackUnsubscribed+TrackSubscribed, and removing the
                    // window here would make it vanish/reappear on every
                    // resize.
                    if let RemoteTrack::Video(video_track) = track {
                        if let Some(window_id) =
                            crate::transport::publisher::window_id_from_track_name(
                                &video_track.name(),
                            )
                        {
                            log::debug!(
                                "windows compositor feed: track unsubscribed for window {window_id} (window kept with frozen frame)"
                            );
                        }
                    }
                }
                // TERMINAL arm: in this SDK an explicit stop-sharing does NOT
                // reliably also fire TrackUnsubscribed, so without this the
                // frozen window would linger forever.
                RoomEvent::TrackUnpublished {
                    publication,
                    participant,
                } => {
                    if let Some(window_id) =
                        crate::transport::publisher::window_id_from_track_name(&publication.name())
                    {
                        let owner_identity = participant.identity().to_string();
                        // The sender's resize-republish unpublishes the old sid
                        // and publishes a new one under the SAME window id.
                        // Ask the SFU's live publication set whether a
                        // replacement is already announced: if so, keep the
                        // window (its last frame freezes until the
                        // replacement's TrackSubscribed reattaches and resizes
                        // it in place) instead of destroying and recreating it
                        // on every resize (#627, #631 — macOS parity).
                        match resolve_teardown(
                            &room,
                            &owner_identity,
                            window_id,
                            &publication.sid().to_string(),
                            true,
                        ) {
                            TeardownDecision::IgnoreSuperseded => {
                                log::debug!(
                                    "windows compositor feed: ignoring stale track unpublish for window {window_id} from '{owner_identity}'"
                                );
                            }
                            TeardownDecision::HoldForReplacement
                            | TeardownDecision::HoldForTransientUnsubscribe => {
                                log::info!(
                                    "windows compositor feed: window {window_id} unpublished by '{owner_identity}' is a republish and the SFU still holds a publication; the window keeps its last frame on screen (#627, #631)"
                                );
                            }
                            TeardownDecision::RemoveWindow => {
                                log::info!(
                                    "windows compositor feed: window {window_id} unpublished by '{owner_identity}' and the SFU holds no replacement, removing"
                                );
                                crate::windows_compositor::remove_window(
                                    &app,
                                    (owner_identity, window_id),
                                )
                                .await;
                            }
                        }
                    }
                }
                RoomEvent::TrackPublished {
                    publication,
                    participant,
                } => {
                    let owner_identity = participant.identity().to_string();
                    if owner_identity == local_identity {
                        continue;
                    }
                    let track_name = publication.name();
                    let is_window_share =
                        crate::transport::publisher::window_id_from_track_name(&track_name)
                            .is_some();
                    log::info!(
                        "windows compositor feed: track published sid={} name='{track_name}' from '{owner_identity}' (window_share={is_window_share}); awaiting auto-subscribe",
                        publication.sid()
                    );
                }
                RoomEvent::ParticipantDisconnected(participant) => {
                    let identity = participant.identity().to_string();
                    if identity == local_identity {
                        continue;
                    }
                    log::info!(
                        "windows compositor feed: participant '{identity}' disconnected; removing their compositor windows"
                    );
                    crate::windows_compositor::remove_all_for(&app, identity).await;
                }
                RoomEvent::Disconnected { reason } => {
                    log::warn!(
                        "windows compositor feed: room disconnected ({reason:?}); removing all compositor windows"
                    );
                    crate::windows_compositor::remove_all(&app).await;
                    // Fan out: the session performs its own teardown on the
                    // receiving end (move of the old disconnect watcher).
                    let _ = on_forced_disconnect.send(());
                    break;
                }
                _ => {}
            }
        }
    });
}

/// Receiver starvation-policy decision (Windows decode loop watchdog).
/// Pure so the timing policy is unit-testable without a LiveKit session:
///
/// - Not starved + frames flowing: keep the current (HIGH) request.
/// - Not starved + no frame for `STARVATION_DOWNGRADE_AFTER`: the requested
///   layer is dead — downgrade to LOW so the SFU serves the live layer.
/// - Starved on LOW long enough (probe delay with backoff): re-probe HIGH
///   in case the publisher's high layer recovered.
/// - Starved but past the failure cap: stay on LOW (give up probing; a
///   republish/reconnect restarts the loop with clean state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StarvationAction {
    Keep,
    DowngradeToLow,
    ProbeHigh,
}

/// Probe delay after `consecutive_probe_failures` (30s, 60s, 120s, capped).
fn starvation_probe_delay(consecutive_probe_failures: u32) -> Duration {
    let multiplier = 1u32 << consecutive_probe_failures.min(2);
    STARVATION_PROBE_BASE
        .saturating_mul(multiplier)
        .min(STARVATION_PROBE_MAX)
}

fn starvation_action(
    since_last_frame: Duration,
    starved: bool,
    since_starved: Option<Duration>,
    consecutive_probe_failures: u32,
) -> StarvationAction {
    if starved {
        let Some(since_starved) = since_starved else {
            return StarvationAction::Keep;
        };
        if consecutive_probe_failures < STARVATION_PROBE_FAILURE_CAP
            && since_starved >= starvation_probe_delay(consecutive_probe_failures)
        {
            StarvationAction::ProbeHigh
        } else {
            StarvationAction::Keep
        }
    } else if since_last_frame >= STARVATION_DOWNGRADE_AFTER {
        StarvationAction::DowngradeToLow
    } else {
        StarvationAction::Keep
    }
}

/// One inbound-RTP QP poll compared against the previous poll (#907's
/// macOS quality-based starvation trigger). `frames_decoded` and `qp_sum`
/// are both monotonically non-decreasing counters read from
/// `RemoteVideoTrack::get_stats()`; this reduces two consecutive readings to
/// what changed between them.
#[derive(Debug, Clone, Copy, PartialEq)]
enum QpSample {
    /// No new frame was decoded between the two polls -- too soon to say
    /// anything about quality this interval (not evidence of a healthy
    /// stream OR a starved one).
    NoNewFrames,
    /// Frames decoded advanced but `qp_sum` never moved. A layer that is
    /// genuinely decoding content can never truly average QP 0, so this
    /// means the decoder path (e.g. a given VideoToolbox/H.264 build) does
    /// not populate `qp_sum` at all -- treat the quality signal as
    /// unsupported rather than reporting a false "perfect quality".
    Unsupported,
    /// Average QP of the frames decoded since the last poll.
    Average(f64),
}

fn inbound_qp_sample(
    prev_qp_sum: u64,
    prev_frames_decoded: u32,
    qp_sum: u64,
    frames_decoded: u32,
) -> QpSample {
    let frame_delta = frames_decoded.saturating_sub(prev_frames_decoded);
    if frame_delta == 0 {
        return QpSample::NoNewFrames;
    }
    let qp_delta = qp_sum.saturating_sub(prev_qp_sum);
    if qp_delta == 0 {
        return QpSample::Unsupported;
    }
    QpSample::Average(qp_delta as f64 / f64::from(frame_delta))
}

/// Whether a streak of sustained-high-QP samples is long enough to downgrade
/// the subscription. Pure so the hysteresis threshold is unit-testable
/// without a real decoder or LiveKit session.
fn quality_downgrade_due(consecutive_high_qp_samples: u32) -> bool {
    consecutive_high_qp_samples >= QUALITY_DOWNGRADE_SUSTAINED_SAMPLES
}

/// macOS starvation decision: the Windows `starvation_action` liveness
/// trigger, layered with the quality-based trigger above. A sustained-high-QP
/// streak downgrades immediately (bypassing the liveness clock, since frames
/// ARE arriving) as long as the layer isn't already starved; once starved,
/// the existing probe/backoff policy in `starvation_action` governs recovery
/// unchanged, so the two triggers share one downgrade/probe/give-up state
/// machine rather than fighting each other.
fn starvation_action_for_macos(
    since_last_frame: Duration,
    starved: bool,
    since_starved: Option<Duration>,
    consecutive_probe_failures: u32,
    consecutive_high_qp_samples: u32,
) -> StarvationAction {
    if !starved && quality_downgrade_due(consecutive_high_qp_samples) {
        return StarvationAction::DowngradeToLow;
    }
    starvation_action(
        since_last_frame,
        starved,
        since_starved,
        consecutive_probe_failures,
    )
}

/// #694 (Windows sibling of #682): `cancel_token` ties this loop's lifetime
/// to its window's teardown -- installed by the `TrackSubscribed` arm above
/// via `windows_compositor::install_decode_loop_token`, which is also the
/// ONLY place that can hand this loop a token already cancelled at spawn
/// time (a republish that lands and gets superseded before this task's first
/// poll). Every path that removes a Windows compositor window for this key
/// (`windows_compositor::remove_window`/`remove_all_for`/`remove_all`) and
/// `install_decode_loop_token` itself (the replacement-insert case) cancel
/// this same token, so `stream.next()` no longer needing to return `None`
/// for this loop to exit (see `next_frame_or_cancelled`'s doc comment for why
/// it doesn't, in production) stops mattering: cancellation is the loop's
/// real exit path now, exactly as it is for the macOS decode loop.
#[cfg(target_os = "windows")]
fn spawn_windows_decode_loop(
    video_track: RemoteVideoTrack,
    key: crate::windows_compositor::WindowKey,
    is_display_region: bool,
    generation: RoomGeneration,
    frames_this_interval: Arc<std::sync::atomic::AtomicU64>,
    cancel_token: CancellationToken,
) {
    use std::sync::atomic::Ordering;
    tauri::async_runtime::spawn(async move {
        let rtc_track = video_track.rtc_track();
        let mut stream = NativeVideoStream::new(rtc_track.clone());
        let mut first_frame_logged = false;
        let mut invalid_dimension_frames = 0u64;
        let mut enabled = true;
        // Starvation watchdog state (see `starvation_action`): the first
        // frame is given the full subscription-negotiation time before the
        // clock starts, so a slow first-frame does NOT read as starvation;
        // the policy engages only once at least one frame has been received
        // and then gone quiet for `STARVATION_DOWNGRADE_AFTER`.
        let mut first_frame_received = false;
        let mut last_frame_seen = std::time::Instant::now();
        let mut starved = false;
        let mut starved_since: Option<std::time::Instant> = None;
        let mut consecutive_probe_failures: u32 = 0;
        // Set when a recovery probe is outstanding; the next stall-downgrade
        // counts as a probe failure, any other downgrade resets the count.
        let mut probe_outstanding = false;
        // #907 review finding 1: this MUST be a persistent `Interval` created
        // once here, not `tokio::time::sleep(STARVATION_CHECK_INTERVAL)`
        // constructed fresh inside the `select!` below. A `sleep` built
        // inside a `select!` arm is a brand-new future every loop iteration,
        // and at any frame rate faster than the interval (here, any healthy
        // stream at all) the frame branch wins the race every single time --
        // the sleep never gets a chance to elapse, so this tick would never
        // fire while frames are flowing. For the liveness-only check this
        // loop originally shipped with, that happened to be harmless (a
        // silent stream lets a freshly-built sleep run uninterrupted), but it
        // is fatal for anything that must run PERIODICALLY regardless of
        // whether frames keep arriving (see the macOS decode loop's
        // quality-based check, which needs exactly that). Fixed here too so
        // the two loops share one correct pattern instead of one working by
        // accident.
        let mut watchdog_tick = tokio::time::interval(STARVATION_CHECK_INTERVAL);
        watchdog_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // #694: do not remove this check as "redundant with cancel_token" --
            // it is what closes the leave-room race the token alone can't. A
            // token installed after `remove_all` has already drained the
            // registry would never be cancelled by that removal; this check,
            // re-run every iteration BEFORE parking on the frame race, is what
            // makes such a loop exit on its first poll instead of leaking. See
            // the #694 adversarial review notes on this exact interleaving.
            if !generation.is_current() {
                log::debug!(
                    "windows compositor feed: video stream exiting for stale room generation"
                );
                break;
            }
            if cancel_token.is_cancelled() {
                // #694: the normal exit path now. This window's decode-loop
                // token was cancelled or replaced (window removed, or a
                // republish installed a fresh token for the same key).
                log::info!(
                    "windows compositor feed: decode loop cancelled for window {key:?} (window removed or replaced, #694)"
                );
                break;
            }
            let on_screen = crate::windows_compositor::window_is_on_screen(&key);
            if on_screen != enabled {
                // Window minimized / hidden / on another virtual desktop:
                // pause the track so libwebrtc stops delivering frames (and
                // stops decoding). The stream queue is a 1-frame latest-wins
                // slot, so even without the pause it would not grow — but the
                // decode itself happens in the C++ pipeline before the queue,
                // and that is what we save. The compositor thread refreshes
                // the flag every pump tick (~16 ms), and we re-check it on
                // this poll loop. #694: independent of and composes with the
                // cancellation check above -- this pauses decode for a
                // visible-but-minimized window, cancellation stops the loop
                // entirely for a torn-down one.
                rtc_track.set_enabled(on_screen);
                enabled = on_screen;
                if enabled {
                    // Fresh stall clock on restore: the resume handshake
                    // (re-enable -> SFU resumes delivery -> keyframe) needs
                    // its own grace period, and the pre-hide starvation
                    // state must not downgrade a freshly-restored window.
                    last_frame_seen = std::time::Instant::now();
                    starved = false;
                    starved_since = None;
                    probe_outstanding = false;
                }
                log::debug!(
                    "windows compositor feed: window {:?} {} — track {}",
                    key,
                    if on_screen {
                        "visible again"
                    } else {
                        "off-screen"
                    },
                    if on_screen { "enabled" } else { "paused" }
                );
            }
            if !enabled {
                // Off-screen: do not pull frames (delivery is disabled, so
                // the queue stays empty — no unbounded growth). Poll the
                // flag until the window comes back, then resume decoding.
                // Re-checked against `generation`/`cancel_token` at the top of
                // the loop every 50ms, so a teardown while paused off-screen
                // is still bounded.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            // Stall watchdog + frame wait in one select: a 1s tick wakes the
            // loop even when the requested layer is silent, so a starved
            // HIGH subscription gets downgraded to LOW (and later re-probed)
            // instead of holding the window frozen on a dead layer. Frames
            // cancel the tick normally; `next_frame_or_cancelled` keeps its
            // own biased cancellation handling.
            let frame = tokio::select! {
                _ = watchdog_tick.tick() => {
                    if !first_frame_received {
                        // Initial subscription negotiation in flight; nothing
                        // to starve yet. Do not downgrade a brand-new share
                        // whose first frame is merely slow to land.
                        continue;
                    }
                    let since_last_frame = last_frame_seen.elapsed();
                    let since_starved = starved_since.map(|t| t.elapsed());
                    match starvation_action(
                        since_last_frame,
                        starved,
                        since_starved,
                        consecutive_probe_failures,
                    ) {
                        StarvationAction::Keep => {}
                        StarvationAction::DowngradeToLow => {
                            // A downgrade right after a recovery probe means
                            // the high layer is still dead; count it so the
                            // probe backoff grows and eventually gives up.
                            if probe_outstanding {
                                consecutive_probe_failures =
                                    consecutive_probe_failures.saturating_add(1);
                            } else {
                                consecutive_probe_failures = 0;
                            }
                            probe_outstanding = false;
                            starved = true;
                            starved_since = Some(std::time::Instant::now());
                            // On-demand fetch: a sender republish replaces
                            // the registered publication, and the downgrade
                            // must ride the CURRENT one.
                            let publication = window_publications()
                                .lock_unpoisoned()
                                .get(&key)
                                .cloned();
                            if let Some(pub_) = publication {
                                pub_.set_video_quality(VideoQuality::Low);
                            }
                            log::warn!(
                                "windows compositor feed: window {key:?} no decoded frame for >= {}s; downgrading subscription to LOW so the SFU serves the live layer (HIGH layer starved, e.g. bandwidth-killed; probe failures so far: {})",
                                STARVATION_DOWNGRADE_AFTER.as_secs(),
                                consecutive_probe_failures
                            );
                        }
                        StarvationAction::ProbeHigh => {
                            probe_outstanding = true;
                            starved = false;
                            starved_since = None;
                            let publication = window_publications()
                                .lock_unpoisoned()
                                .get(&key)
                                .cloned();
                            if let Some(pub_) = publication {
                                pub_.set_video_quality(VideoQuality::High);
                            }
                            log::warn!(
                                "windows compositor feed: window {key:?} recovery probe: re-requesting HIGH after {}s on LOW (probe failure {})",
                                since_starved
                                    .unwrap_or_default()
                                    .as_secs(),
                                consecutive_probe_failures + 1
                            );
                        }
                    }
                    continue;
                }
                outcome = next_frame_or_cancelled(&mut stream, &cancel_token) => outcome,
            };
            let frame = match frame {
                FrameOrCancelled::Cancelled => {
                    log::info!(
                        "windows compositor feed: decode loop cancelled for window {key:?} (window removed or replaced, #694)"
                    );
                    break;
                }
                FrameOrCancelled::Frame(None) => {
                    // Not observed in practice (`next_frame_or_cancelled`'s
                    // doc comment: the underlying frame queue is only closed
                    // by this task's own `NativeVideoStream::drop`, which
                    // cannot run before this `await` returns) -- kept as a
                    // safety net, not relied on.
                    log::info!("windows compositor feed: video stream ended for window {key:?}");
                    break;
                }
                FrameOrCancelled::Frame(Some(frame)) => frame,
            };
            // #694 (mirrors #682's counselors-review follow-up): `select!` is
            // biased toward cancellation in `next_frame_or_cancelled`, but a
            // frame already dequeued from the stream in the same poll that
            // raced a concurrent cancellation can still reach here once. This
            // window shares no per-subscription state the way macOS's
            // `ReceiveWindowState` does (a stray frame here would just be one
            // stale-but-harmless frame pushed to the SAME compositor window
            // key), but dropping it is free and keeps the two decode loops'
            // behavior in lockstep.
            if cancel_token.is_cancelled() {
                log::debug!(
                    "windows compositor feed: dropping one already-dequeued frame for window {key:?} after cancellation"
                );
                continue;
            }
            // Starvation watchdog: the layer is alive if ANY frame arrives
            // (even one later dropped for invalid dimensions), so restart
            // the stall clock here rather than after validation.
            if !first_frame_received {
                first_frame_received = true;
            }
            last_frame_seen = std::time::Instant::now();
            // #907 review finding 7: a probe is outstanding exactly when we
            // re-requested HIGH after a downgrade and are waiting to find out
            // whether it recovered. The FIRST frame received afterward is
            // conclusive success evidence for THAT probe -- clear it and
            // reset the failure streak here. Without this, an outstanding
            // probe that actually succeeded stays "outstanding" forever, and
            // an unrelated stall/downgrade much later gets miscounted as a
            // continuation of that already-succeeded probe's failure history
            // -- eventually exhausting the retry cap on phantom failures.
            if probe_outstanding {
                probe_outstanding = false;
                consecutive_probe_failures = 0;
            }
            if !first_frame_logged {
                first_frame_logged = true;
                log::info!(
                    "windows compositor feed: window {:?} first decoded frame buffer_type={:?} {}x{}",
                    key,
                    frame.buffer.buffer_type(),
                    frame.buffer.width(),
                    frame.buffer.height()
                );
            }
            // Same SIGABRT guard as the macOS loop above: to_i420 on a
            // frame with invalid dimensions aborts the whole process
            // (webrtc CheckValidDimensions RTC_CHECK).
            let frame_width = frame.buffer.width();
            let frame_height = frame.buffer.height();
            if !decoded_frame_dimensions_valid(frame_width, frame_height) {
                invalid_dimension_frames += 1;
                if invalid_dimension_frames == 1
                    || invalid_dimension_frames % INVALID_DIMENSION_LOG_EVERY == 0
                {
                    log::warn!(
                        "windows compositor feed: window {key:?} dropping decoded frame with invalid dimensions {frame_width}x{frame_height} (count {invalid_dimension_frames}) -- to_i420 on such a frame aborts the process"
                    );
                }
                continue;
            }
            // Transient SFU adaptive dips can drop this window to the low
            // simulcast layer; nudge it back to HIGH (idempotent, throttled).
            // Suppressed while deliberately starved on LOW: the first LOW
            // frame would otherwise re-request HIGH immediately and fight the
            // starvation downgrade (black/frozen oscillation every few
            // seconds). Recovery from LOW happens via the watchdog's probe.
            if !starved
                && !is_display_region
                && !crate::windows_compositor::decoded_frame_has_source_aspect_change(
                    publication_dimension_for_window(&key),
                    (frame_width, frame_height),
                )
            {
                // A Petal View ROI, or an ordinary sender resize, can
                // legitimately change aspect without a replacement event.
                // Do not fight that source change with repeated HIGH requests.
                reassert_high_after_low_layer(&key, frame_width, frame_height);
            }
            let i420 = frame.buffer.to_i420();
            let (y, u, v) = i420.data();
            let (y_stride, u_stride, v_stride) = i420.strides();
            crate::windows_compositor::push_frame(
                key.clone(),
                y.to_vec(),
                y_stride as usize,
                u.to_vec(),
                u_stride as usize,
                v.to_vec(),
                v_stride as usize,
                i420.width(),
                i420.height(),
            )
            .await;
            frames_this_interval.fetch_add(1, Ordering::Relaxed);
        }
    });
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn start_compositor_feed(
    _app: &tauri::AppHandle,
    _events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    _local_identity: String,
    _generation: RoomGeneration,
) {
}

use crate::time_util::now_us;

fn shared_window_color_profile_or_default(metadata: &str, window_id: u32) -> VideoColorProfile {
    crate::transport::publisher::shared_window_color_profile_from_metadata(metadata, window_id)
        .unwrap_or_else(VideoColorProfile::legacy_publish_default)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReceiveWindowKey {
    owner_identity: String,
    window_id: u32,
}

impl ReceiveWindowKey {
    fn new(owner_identity: String, window_id: u32) -> Self {
        Self {
            owner_identity,
            window_id,
        }
    }
}

#[derive(Debug, Clone)]
struct ReceiveWindowState {
    owner_identity: String,
    track_name: String,
    subscribed_at: Instant,
    last_frame_at: Option<Instant>,
    track_muted: bool,
    reconnecting: bool,
    color_profile: VideoColorProfile,
    frames_received: u64,
    last_health_log_at: Instant,
    last_health_log_frames: u64,
    /// #627: the no-frame watchdog has already put this window into a held
    /// last-frame state. The receive state is deliberately KEPT while held (a
    /// removed entry makes the watchdog one-shot and leaves nothing able to
    /// retire the window later), so this flag is what stops the watchdog
    /// re-firing its repair request every tick. Cleared when frames resume.
    held_no_frames: bool,
    /// #682: ties this entry's decode loop's lifetime to the entry itself.
    /// The loop spawned for this window (`start_compositor_feed`'s
    /// `TrackSubscribed` arm) races `stream.next()` against
    /// `cancel.cancelled()` (`next_frame_or_cancelled`) and exits the moment
    /// this token is cancelled. `insert_window_state`/`remove_window_state`
    /// are the ONLY places that should ever cancel it -- both cancel
    /// whatever entry they displace, so "this entry no longer exists in
    /// `window_states`" and "its decode loop has been told to stop" become
    /// the same fact by construction, covering every removal site plus the
    /// republish/replacement-insert path in one stroke.
    cancel: CancellationToken,
}

impl ReceiveWindowState {
    fn new(
        owner_identity: String,
        track_name: String,
        color_profile: VideoColorProfile,
        now: Instant,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            owner_identity,
            track_name,
            subscribed_at: now,
            last_frame_at: None,
            track_muted: false,
            reconnecting: false,
            color_profile,
            frames_received: 0,
            last_health_log_at: now,
            last_health_log_frames: 0,
            held_no_frames: false,
            cancel,
        }
    }
}

/// Insert `state` for `key`, cancelling any PRIOR entry's decode loop first
/// (#682). This is what makes "putting a fresh entry in place of an old one"
/// -- the republish/replacement-insert case #682 identifies as the likely
/// actual defect behind its own log evidence -- equivalent to a removal for
/// cancellation purposes: the old loop's token is cancelled before the new
/// one is spawned, and the decode loop's own `if cancel_token.is_cancelled()
/// { continue; }` guard (added per counselors review) drops any frame the
/// old loop had already dequeued in the same poll that raced this
/// cancellation. Two generations of decode loop for the same key are
/// therefore never both ACTIVE (i.e. both able to reach
/// `mark_frame_received`/`push_frame`) at once -- NOT a claim that the old
/// task's OS thread/future stops executing at the exact instant this
/// function returns, which `cancel()` (wake-only) does not guarantee. Every
/// site that can replace a `window_states` entry (the `TrackSubscribed`
/// arm's republish path) MUST go through this helper rather than a bare
/// `.insert(...)`, or a republish silently resurrects the leak this issue
/// fixes.
fn insert_window_state(
    states: &Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>>,
    key: ReceiveWindowKey,
    state: ReceiveWindowState,
) {
    let previous = states.lock_unpoisoned().insert(key.clone(), state);
    if let Some(previous) = previous {
        previous.cancel.cancel();
    }
    clear_window_frame_misses(&key);
}

/// Remove `key`'s entry, cancelling its decode loop as part of the same
/// operation (#682) -- so every removal site gets cancellation "for free" by
/// construction rather than needing to separately remember to call
/// `.cancel()` alongside `.remove()`. Returns what `.remove()` would have, so
/// call sites that branch on the `Option`/`is_none()` keep working unchanged.
fn remove_window_state(
    states: &Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>>,
    key: &ReceiveWindowKey,
) -> Option<ReceiveWindowState> {
    let removed = states.lock_unpoisoned().remove(key);
    if let Some(ref state) = removed {
        state.cancel.cancel();
    }
    clear_window_frame_misses(key);
    removed
}

/// Outcome of racing a decode loop's per-window `CancellationToken` against
/// pulling the next frame off its stream (#682 item 1).
///
/// `pub(crate)` (not module-private): #694 reuses this unchanged for the
/// Windows decode loop (`spawn_windows_decode_loop` below) and its
/// `windows_compositor` tests, rather than hand-rolling the identical race a
/// second time -- the race itself is platform-agnostic (generic over the
/// stream), only the registry that owns the `CancellationToken` differs
/// between macOS's `ReceiveWindowState` and Windows'
/// `windows_compositor::decode_loop_tokens`.
#[derive(Debug)]
pub(crate) enum FrameOrCancelled<T> {
    /// `stream.next()` resolved first. `None` is the stream's own natural
    /// end -- see `next_frame_or_cancelled`'s doc comment for why this is not
    /// expected to happen in production.
    Frame(Option<T>),
    /// `cancel.cancelled()` resolved first: the window this loop feeds was
    /// removed from its owning registry, or replaced by a republish.
    Cancelled,
}

/// The exact race backing #682's (macOS) and #694's (Windows) structural
/// fix: a decode loop for a retired/replaced window must stop pulling
/// frames the moment its `CancellationToken` fires, not only when
/// `stream.next()` itself returns `None` -- which, per #682's mechanics
/// writeup (confirmed to apply identically to the Windows
/// `NativeVideoStream`/`VideoFrameQueue` pairing by #694), does not happen
/// while the task is parked here: the underlying frame queue is only closed
/// by this same stream's own `Drop`, which cannot run until this `await`
/// resolves and the loop that owns `stream` exits and drops it.
///
/// Generic over the stream (`S: Stream + Unpin`, matching what
/// `futures::StreamExt::next()` requires) rather than hard-coded to
/// `NativeVideoStream`/`RtcVideoTrack`, so this exact production logic --
/// not a hand-rolled restatement of it -- can be driven by a test with a
/// fake stream: `NativeVideoStream` needs a live `RtcVideoTrack` (a real SFU
/// connection) to construct at all, so it can never appear in a headless
/// unit test. `pub(crate)`: shared verbatim between the macOS and Windows
/// decode loops (see `FrameOrCancelled`'s doc comment).
pub(crate) async fn next_frame_or_cancelled<S>(
    stream: &mut S,
    cancel: &CancellationToken,
) -> FrameOrCancelled<S::Item>
where
    S: futures::Stream + Unpin,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => FrameOrCancelled::Cancelled,
        item = stream.next() => FrameOrCancelled::Frame(item),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ReceiverFrameHealth {
    window_id: u32,
    owner_identity: String,
    total_frames: u64,
    fps: f64,
    gap_since_last_frame: Duration,
    /// #683: live count of this app's own `native_display::
    /// OwnedCVPixelBuffer` decode-output buffers (`platform::mem::
    /// live_pixel_buffer_count`) -- `None` on any platform where the
    /// counter isn't wired up (only macOS today; see that function's doc
    /// comment). This is the RECEIVE/decode->display path's own counter,
    /// deliberately placed on this receiver-side line rather than the
    /// sharer's `capture-diag` -- a sharer's own CVPixelBuffers live inside
    /// ScreenCaptureKit/libwebrtc and are invisible to this counter.
    live_pixel_buffers: Option<u32>,
}

fn format_receiver_frame_health(health: &ReceiverFrameHealth) -> String {
    let pixbufs_str = match health.live_pixel_buffers {
        Some(n) => n.to_string(),
        None => "n/a".to_string(),
    };
    format!(
        "compositor feed: window {} receiver frame health from '{}' -- frames={} compositor_fps={:.1} gap_since_last_frame_ms={} pixbufs={pixbufs_str}",
        health.window_id,
        health.owner_identity,
        health.total_frames,
        health.fps,
        health.gap_since_last_frame.as_millis()
    )
}

fn receiver_frame_health(
    key: &ReceiveWindowKey,
    state: &ReceiveWindowState,
    now: Instant,
) -> ReceiverFrameHealth {
    let elapsed = now.duration_since(state.last_health_log_at).as_secs_f64();
    let delta_frames = state
        .frames_received
        .saturating_sub(state.last_health_log_frames);
    let fps = if elapsed > 0.0 {
        delta_frames as f64 / elapsed
    } else {
        0.0
    };
    let gap_since_last_frame =
        now.duration_since(state.last_frame_at.unwrap_or(state.subscribed_at));
    ReceiverFrameHealth {
        window_id: key.window_id,
        owner_identity: state.owner_identity.clone(),
        total_frames: state.frames_received,
        fps,
        gap_since_last_frame,
        live_pixel_buffers: crate::platform::mem::live_pixel_buffer_count(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoFrameDecision {
    Keep,
    Retire,
}

fn no_frame_decision(
    now: Instant,
    subscribed_at: Instant,
    last_frame_at: Option<Instant>,
    track_muted: bool,
    reconnecting: bool,
    already_held: bool,
) -> NoFrameDecision {
    if track_muted || reconnecting || already_held {
        return NoFrameDecision::Keep;
    }
    let last_media_at = last_frame_at.unwrap_or(subscribed_at);
    if now.duration_since(last_media_at) >= NO_FRAME_RETIRE_AFTER {
        NoFrameDecision::Retire
    } else {
        NoFrameDecision::Keep
    }
}

/// Record that a frame arrived for `key`, or -- #682's item 3 -- signal that
/// it must be dropped without any further per-frame work. `None` means the
/// caller MUST `continue`/skip the rest of the loop body for this frame: the
/// diagnostics recording, glass-to-glass timing sample, CVBuffer color-profile
/// attach, and `compositor::push_frame` call that follow this call in the
/// decode loop are all real per-frame cost (a String alloc + a shared mutex
/// lock, a full-frame color-space conversion in the software-fallback path,
/// etc.) that a stateless frame -- one whose window has been retired or
/// replaced -- must never pay. Previously this returned a fallback color
/// profile on a miss and let the caller fall through and do that work anyway;
/// this bounds the damage in the window between "state removed" and "the
/// #682 cancellation token this state now carries actually takes effect",
/// independent of that fix.
fn mark_frame_received(
    states: &Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>>,
    key: &ReceiveWindowKey,
) -> Option<VideoColorProfile> {
    // Piggyback the current color_profile read onto the same lock acquisition
    // this call already does every frame, rather than adding a second lock on
    // the hot per-frame decode path. `color_profile` here reflects whatever
    // `ParticipantMetadataChanged` most recently wrote (see that handler and
    // `update_receive_window_color_profile`) -- see #251: a >3s signaling
    // stall (fix #249) can publish the track before metadata arrives, so the
    // subscribe-time value can be a fallback default that only becomes
    // correct once metadata lands after the fact.
    if let Some(state) = states.lock_unpoisoned().get_mut(key) {
        state.last_frame_at = Some(Instant::now());
        state.frames_received = state.frames_received.saturating_add(1);
        // Media is flowing again: re-arm the watchdog for a future stall. The
        // header's paused label is cleared by the compositor when the frame
        // actually reaches the layer, not here.
        state.held_no_frames = false;
        Some(state.color_profile)
    } else {
        let misses = record_window_frame_miss(key);
        if misses == 1 || misses % 300 == 0 {
            log::warn!(
                "compositor feed: decoded frame arrived without receive state for window {} from '{}' (retired/replaced subscription); retired_receive_state_frame_misses={misses}",
                key.window_id,
                key.owner_identity,
            );
        }
        None
    }
}

#[cfg(target_os = "macos")]
fn log_receiver_frame_health(states: &Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>>) {
    let now = Instant::now();
    let mut lines = Vec::new();
    {
        let mut guard = states.lock_unpoisoned();
        for (key, state) in guard.iter_mut() {
            if now.duration_since(state.last_health_log_at) < FRAME_HEALTH_LOG_INTERVAL {
                continue;
            }
            lines.push(format_receiver_frame_health(&receiver_frame_health(
                key, state, now,
            )));
            state.last_health_log_at = now;
            state.last_health_log_frames = state.frames_received;
        }
    }
    for line in lines {
        log::info!("{line}");
    }
}

/// Pure decision for whether a `ParticipantMetadataChanged` update should
/// change a window's tracked color_profile: only when the new metadata
/// genuinely carries a color_profile entry for this window -- a metadata
/// republish that doesn't include color info (e.g. a title-only update)
/// must never clobber an already-correct profile with the legacy default.
fn refreshed_color_profile_from_metadata(
    metadata: &str,
    window_id: u32,
) -> Option<VideoColorProfile> {
    crate::transport::publisher::shared_window_color_profile_from_metadata(metadata, window_id)
}

fn update_receive_window_color_profile(
    states: &Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>>,
    key: &ReceiveWindowKey,
    new_profile: VideoColorProfile,
) -> bool {
    if let Some(state) = states.lock_unpoisoned().get_mut(key) {
        if state.color_profile != new_profile {
            state.color_profile = new_profile;
            return true;
        }
    }
    false
}

fn set_reconnecting(
    states: &Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>>,
    reconnecting: bool,
) {
    for state in states.lock_unpoisoned().values_mut() {
        state.reconnecting = reconnecting;
    }
}

/// The compositor feed owns this state rather than deriving it from receive
/// entries: the latter can be empty while the SDK is reconnecting, and its
/// ordering emits participant disconnects before `RoomEvent::Reconnecting`.
#[derive(Debug, Default)]
struct ReconnectLifecycle {
    reconnecting: bool,
}

impl ReconnectLifecycle {
    fn set_reconnecting(&mut self, reconnecting: bool) {
        self.reconnecting = reconnecting;
    }

    fn is_reconnecting(&self) -> bool {
        self.reconnecting
    }
}

#[cfg(target_os = "macos")]
fn retire_no_frame_windows(
    app: &tauri::AppHandle,
    room: &Room,
    states: &Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>>,
) {
    let now = Instant::now();
    let mut retire = Vec::new();
    {
        let guard = states.lock_unpoisoned();
        for (key, state) in guard.iter() {
            if no_frame_decision(
                now,
                state.subscribed_at,
                state.last_frame_at,
                state.track_muted,
                state.reconnecting,
                state.held_no_frames,
            ) == NoFrameDecision::Retire
            {
                retire.push((key.clone(), state.clone()));
            }
        }
    }

    for (key, state) in retire {
        let window_id = key.window_id;
        // #627: decide BEFORE touching the receive state. A stall is not an
        // ended share: while the SFU still holds a publication the share is
        // real and merely not arriving, so hiding the window would make a live
        // share vanish. Hold its last frame and KEEP the receive state, so this
        // watchdog can re-arm if the stall later becomes a real disappearance.
        // (Removing it made the watchdog one-shot and, combined with the
        // registry drop, left the window with no teardown path at all.)
        let publication_exists =
            window_publication_exists(room, &key.owner_identity, window_id, &[]);
        if publication_exists {
            let held = crate::compositor::hold_window_last_frame(
                app,
                &key.owner_identity,
                window_id,
                crate::compositor::HoldWindowReason::NoFrameWatchdog,
            );
            if held {
                let mut guard = states.lock_unpoisoned();
                if let Some(state) = guard.get_mut(&key) {
                    state.held_no_frames = true;
                }
                drop(guard);
                log::warn!(
                    "compositor feed: no frames for window {window_id} from '{}' for >= {}s; \
                     holding its last frame and asking the owner to repair the publication (#627)",
                    state.owner_identity,
                    NO_FRAME_RETIRE_AFTER.as_secs()
                );
                crate::diagnostics::record_native_video_stream_state(
                    app,
                    &state.owner_identity,
                    &state.track_name,
                    "stalled",
                    "native-no-frame-watchdog",
                );
                crate::viewer_demand::publish_window_repair_request(app, window_id);
                continue;
            }
        }
        if remove_window_state(states, &key).is_none() {
            continue;
        }
        log::warn!(
            "compositor feed: no frames for window {window_id} from '{}' for >= {}s and the SFU \
             holds no publication; retiring frozen window",
            state.owner_identity,
            NO_FRAME_RETIRE_AFTER.as_secs()
        );
        crate::diagnostics::record_native_video_stream_state(
            app,
            &state.owner_identity,
            &state.track_name,
            "stalled",
            "native-no-frame-watchdog",
        );
        // This is a watchdog retirement, not a user close. Keep the retired
        // compositor window reusable, but ask the owner to repair its live
        // publication over the existing viewer-demand channel. #417
        crate::viewer_demand::publish_window_repair_request(app, window_id);
        crate::compositor::remove_window(
            app,
            &key.owner_identity,
            window_id,
            crate::compositor::RemoveWindowReason::NoFrameWatchdog,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Starvation watchdog policy --------------------------------------
    //
    // Receiver-side fix for the observed "black remote window" failure:
    // the publisher's high simulcast layer got bandwidth-killed while its
    // low layer kept encoding live content, and a HIGH-subscribed receiver
    // held its last (black, capture-start) frame forever. The watchdog
    // downgrades to LOW on stall and re-probes HIGH on a backoff.

    #[test]
    fn starvation_keeps_while_frames_flow() {
        // Healthy share (sender idle-refresh delivers a frame every ~2s).
        assert_eq!(
            starvation_action(Duration::from_secs(1), false, None, 0),
            StarvationAction::Keep
        );
    }

    #[test]
    fn starvation_downgrades_after_threshold() {
        assert_eq!(
            starvation_action(STARVATION_DOWNGRADE_AFTER, false, None, 0),
            StarvationAction::DowngradeToLow
        );
        assert_eq!(
            starvation_action(
                STARVATION_DOWNGRADE_AFTER + Duration::from_secs(1),
                false,
                None,
                0
            ),
            StarvationAction::DowngradeToLow
        );
    }

    #[test]
    fn starvation_stays_low_during_probe_backoff() {
        // Starved but not yet at the probe deadline.
        assert_eq!(
            starvation_action(
                Duration::from_secs(60),
                true,
                Some(STARVATION_PROBE_BASE - Duration::from_secs(1)),
                0
            ),
            StarvationAction::Keep
        );
    }

    #[test]
    fn starvation_probes_high_at_deadline() {
        assert_eq!(
            starvation_action(
                Duration::from_secs(60),
                true,
                Some(STARVATION_PROBE_BASE),
                0
            ),
            StarvationAction::ProbeHigh
        );
    }

    #[test]
    fn starvation_probe_delay_backs_off_and_caps() {
        assert_eq!(starvation_probe_delay(0), STARVATION_PROBE_BASE);
        assert_eq!(
            starvation_probe_delay(1),
            STARVATION_PROBE_BASE.saturating_mul(2)
        );
        assert_eq!(
            starvation_probe_delay(2),
            STARVATION_PROBE_BASE.saturating_mul(4)
        );
        // 3+ failures saturate at the cap.
        assert_eq!(starvation_probe_delay(3), STARVATION_PROBE_MAX);
        assert_eq!(starvation_probe_delay(100), STARVATION_PROBE_MAX);
    }

    #[test]
    fn starvation_gives_up_after_failure_cap() {
        // Past the failure cap: stay on LOW, never probe again (recovery is
        // delegated to a republish/reconnect which restarts the loop).
        assert_eq!(
            starvation_action(
                Duration::from_secs(600),
                true,
                Some(STARVATION_PROBE_MAX),
                STARVATION_PROBE_FAILURE_CAP
            ),
            StarvationAction::Keep
        );
    }

    // ---- #907 macOS quality-based trigger: QP sampling + hysteresis -------

    #[test]
    fn inbound_qp_sample_distinguishes_no_new_frames_unsupported_and_average() {
        // No frames decoded since the last poll: no evidence either way.
        assert_eq!(inbound_qp_sample(0, 10, 0, 10), QpSample::NoNewFrames);
        // Frames advanced but qp_sum never moved: the decoder path does not
        // report QP at all -- NOT "avg QP 0" (a starved layer can never
        // truly average QP 0).
        assert_eq!(inbound_qp_sample(0, 10, 0, 20), QpSample::Unsupported);
        // Genuine average: 300 qp over 10 frames = 30.0.
        assert_eq!(inbound_qp_sample(0, 10, 300, 20), QpSample::Average(30.0));
    }

    #[test]
    fn quality_downgrade_due_requires_the_full_sustained_streak() {
        for count in 0..QUALITY_DOWNGRADE_SUSTAINED_SAMPLES {
            assert!(
                !quality_downgrade_due(count),
                "must not fire before {QUALITY_DOWNGRADE_SUSTAINED_SAMPLES} consecutive samples (got count={count})"
            );
        }
        assert!(quality_downgrade_due(QUALITY_DOWNGRADE_SUSTAINED_SAMPLES));
        assert!(quality_downgrade_due(QUALITY_DOWNGRADE_SUSTAINED_SAMPLES + 5));
    }

    #[test]
    fn starvation_action_for_macos_downgrades_on_sustained_high_qp_alone() {
        // The exact #907 mechanism: frames ARE arriving (since_last_frame is
        // small, well under the liveness threshold) but quality has been
        // sustained-bad -- the liveness-only `starvation_action` would say
        // Keep forever here; the macOS wrapper must not.
        assert_eq!(
            starvation_action_for_macos(
                Duration::from_millis(100),
                false,
                None,
                0,
                QUALITY_DOWNGRADE_SUSTAINED_SAMPLES,
            ),
            StarvationAction::DowngradeToLow
        );
        // Below the sustained-sample threshold: no action from quality alone.
        assert_eq!(
            starvation_action_for_macos(
                Duration::from_millis(100),
                false,
                None,
                0,
                QUALITY_DOWNGRADE_SUSTAINED_SAMPLES - 1,
            ),
            StarvationAction::Keep
        );
        // Already starved: quality signal is ignored (the shared
        // probe/backoff machinery governs, not a fresh quality trigger).
        assert_eq!(
            starvation_action_for_macos(
                Duration::from_millis(100),
                true,
                Some(Duration::from_secs(1)),
                0,
                QUALITY_DOWNGRADE_SUSTAINED_SAMPLES,
            ),
            StarvationAction::Keep
        );
    }

    // ---- #907 review finding 1: persistent watchdog tick vs. a recreated
    // sleep --------------------------------------------------------------
    //
    // The original bug (both the macOS and Windows decode loops shared it):
    // `tokio::select! { _ = tokio::time::sleep(INTERVAL) => {...}, frame =
    // ... => {...} }` builds a BRAND NEW `sleep` future every loop
    // iteration. At any frame rate faster than `INTERVAL`, the frame branch
    // wins the race every single time and the sleep never gets a chance to
    // elapse -- the watchdog tick never fires while frames keep flowing,
    // which is exactly the #907 field scenario (frames arriving
    // continuously at QP 31.1). These two tests exercise the actual
    // `select!` wiring under tokio's paused virtual clock (deterministic:
    // with time paused, `select!` between two timers always resolves the
    // shorter one, so this is not a flaky race) -- not just the pure
    // decision functions above, which stayed green even while this bug was
    // live in production code (see CLAUDE.md's own documented lesson: a
    // green unit test on extracted pure logic proves nothing about whether
    // the real event/timer wiring actually drives it).

    #[tokio::test(start_paused = true)]
    async fn a_persistent_interval_still_ticks_against_a_faster_repeating_future() {
        let mut watchdog_tick = tokio::time::interval(Duration::from_secs(1));
        watchdog_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut watchdog_fires = 0u32;
        let mut fast_ticks = 0u32;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(3_500);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                _ = watchdog_tick.tick() => { watchdog_fires += 1; }
                _ = tokio::time::sleep(Duration::from_millis(33)) => { fast_ticks += 1; }
            }
        }
        assert!(fast_ticks > 0, "the fast branch must actually be racing, or this test proves nothing");
        assert!(
            watchdog_fires >= 3,
            "a persistent `Interval` created once outside the loop must keep firing on its own \
             schedule even when raced against a much faster repeating future every iteration; got {watchdog_fires} fires in 3.5s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_sleep_recreated_inside_select_never_fires_against_a_faster_repeating_future() {
        // Documents the bug pattern this fix replaced -- does NOT recommend
        // reintroducing it. If this assertion ever starts failing, it means
        // tokio's `select!`/timer semantics changed underneath this
        // codebase's assumptions, not that the bug got fixed by accident.
        let mut watchdog_fires = 0u32;
        let mut fast_ticks = 0u32;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(3_500);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => { watchdog_fires += 1; }
                _ = tokio::time::sleep(Duration::from_millis(33)) => { fast_ticks += 1; }
            }
        }
        assert!(fast_ticks > 0);
        assert_eq!(
            watchdog_fires, 0,
            "a `sleep` recreated fresh inside a `select!` arm loses to a persistently faster \
             competing branch every time -- this is the exact #907 review finding 1 bug"
        );
    }

    // ---- SIGABRT guard: invalid decoded-frame dimensions ------------------
    //
    // desktop-2026-08-06-130423.ips: a remote frame with dimensions webrtc's
    // CheckValidDimensions rejects reached `to_i420()` and abort()ed the
    // process. Both decode loops now gate every frame on
    // `decoded_frame_dimensions_valid` before any conversion or push.

    #[test]
    fn decoded_frame_dimensions_accept_normal_sizes() {
        assert!(decoded_frame_dimensions_valid(1920, 1080));
        assert!(decoded_frame_dimensions_valid(1, 1));
        assert!(decoded_frame_dimensions_valid(
            MAX_DECODED_FRAME_DIMENSION,
            MAX_DECODED_FRAME_DIMENSION
        ));
    }

    #[test]
    fn decoded_frame_dimensions_reject_zero() {
        assert!(!decoded_frame_dimensions_valid(0, 1080));
        assert!(!decoded_frame_dimensions_valid(1920, 0));
        assert!(!decoded_frame_dimensions_valid(0, 0));
    }

    #[test]
    fn decoded_frame_dimensions_reject_negative_reinterpreted_as_u32() {
        // A negative C int dimension crossing the FFI boundary shows up
        // here as a huge u32 (e.g. -1 -> u32::MAX).
        assert!(!decoded_frame_dimensions_valid(u32::MAX, 1080));
        assert!(!decoded_frame_dimensions_valid(1920, i32::MIN as u32));
    }

    #[test]
    fn decoded_frame_dimensions_reject_oversized() {
        assert!(!decoded_frame_dimensions_valid(
            MAX_DECODED_FRAME_DIMENSION + 1,
            1080
        ));
        assert!(!decoded_frame_dimensions_valid(
            1920,
            MAX_DECODED_FRAME_DIMENSION + 1
        ));
    }

    // ---- #682: decode loop cancellation -----------------------------------
    //
    // These tests are grouped up front, ahead of the pre-existing suite
    // below, because several of them exercise the ACTUAL production
    // functions every removal/replacement site calls (`insert_window_state`,
    // `remove_window_state`, `next_frame_or_cancelled`) rather than a pure
    // helper extracted just for testability -- per CLAUDE.md's native-
    // lifecycle rule, a unit test on an isolated pure function is not
    // sufficient evidence that a real removal actually stops a real running
    // task.

    #[tokio::test]
    async fn decode_loop_task_terminates_when_state_is_removed() {
        // The real teardown path: `remove_window_state` is exactly what
        // `apply_teardown_decision` (TrackUnsubscribed/TrackUnpublished) and
        // `retire_no_frame_windows` (the no-frame watchdog) call. This spawns
        // a task that runs the SAME `next_frame_or_cancelled` race the real
        // decode loop in `start_compositor_feed` awaits every iteration, fed
        // by a stream that never yields on its own (`futures::stream::
        // pending`) -- so the ONLY way this task can ever end is
        // cancellation -- then calls `remove_window_state` and asserts the
        // task is actually gone (joined with a bounded timeout) and reached
        // its post-cancellation completion marker, not merely that a token
        // object was flipped.
        let states: Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let key = ReceiveWindowKey::new("issue682-terminate-owner".to_string(), 682);
        let cancel_token = CancellationToken::new();
        insert_window_state(
            &states,
            key.clone(),
            ReceiveWindowState::new(
                "issue682-terminate-owner".to_string(),
                "petal-window-682".to_string(),
                VideoColorProfile::BT601_VIDEO,
                Instant::now(),
                cancel_token.clone(),
            ),
        );

        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_for_task = completed.clone();
        let handle = tokio::spawn(async move {
            let mut stream = futures::stream::pending::<()>();
            loop {
                match next_frame_or_cancelled(&mut stream, &cancel_token).await {
                    FrameOrCancelled::Cancelled => break,
                    FrameOrCancelled::Frame(_) => continue,
                }
            }
            completed_for_task.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        // Give the spawned task a chance to actually start awaiting the
        // race before we cancel it -- otherwise a pass could be a false
        // positive (the task ending before it ever raced anything).
        tokio::task::yield_now().await;

        assert!(
            remove_window_state(&states, &key).is_some(),
            "remove_window_state must find and remove the entry we just inserted"
        );

        let joined = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            joined.is_ok(),
            "decode loop task did not terminate within 2s of its state being removed -- \
             this is exactly the #682 leak (a task that outlives its window_states entry)"
        );
        assert!(joined.unwrap().is_ok(), "decode loop task panicked");
        assert!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            "task exited without reaching its post-cancellation completion marker, so it did \
             not exit via the Cancelled arm"
        );
    }

    #[tokio::test]
    async fn feed_loop_exit_drains_and_cancels_every_remaining_decode_loop() {
        // Counselors review of #682 (Fable): every `break` out of
        // `start_compositor_feed`'s event loop (stale generation, closed
        // events channel -- i.e. ordinary room leave/rejoin, not just a
        // republish) drops `window_states` with no cleanup, and dropping a
        // `CancellationToken` does NOT fire `cancelled()`. Without the
        // drain-and-cancel this test exercises, every decode loop still
        // parked in `stream.next()` at leave time would leak forever --
        // reproducing #682's exact defect on the MOST common path (leaving a
        // room), not only the republish path the rest of this fix targets.
        // This drives the same drain-and-cancel this file's feed loop now
        // runs on every exit, against TWO windows at once, and asserts both
        // real spawned tasks actually terminate (joined with a bounded
        // timeout) -- not merely that their tokens were flipped.
        let states: Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let mut handles = Vec::new();
        let mut completions = Vec::new();
        for window_id in [682u32, 683u32] {
            let key = ReceiveWindowKey::new("issue682-feed-exit-owner".to_string(), window_id);
            let cancel_token = CancellationToken::new();
            insert_window_state(
                &states,
                key,
                ReceiveWindowState::new(
                    "issue682-feed-exit-owner".to_string(),
                    format!("petal-window-{window_id}"),
                    VideoColorProfile::BT601_VIDEO,
                    Instant::now(),
                    cancel_token.clone(),
                ),
            );
            let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed_for_task = completed.clone();
            let handle = tokio::spawn(async move {
                let mut stream = futures::stream::pending::<()>();
                loop {
                    match next_frame_or_cancelled(&mut stream, &cancel_token).await {
                        FrameOrCancelled::Cancelled => break,
                        FrameOrCancelled::Frame(_) => continue,
                    }
                }
                completed_for_task.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            handles.push(handle);
            completions.push(completed);
        }

        tokio::task::yield_now().await;

        // The REAL function `start_compositor_feed` calls on every exit from
        // its `loop { ... }` -- not a duplicated restatement of its logic.
        cancel_all_window_states(&states);

        for (i, handle) in handles.into_iter().enumerate() {
            let joined = tokio::time::timeout(Duration::from_secs(2), handle).await;
            assert!(
                joined.is_ok(),
                "decode loop task {i} did not terminate within 2s of the feed loop's \
                 drain-and-cancel -- a leave/rejoin would leak it forever (#682)"
            );
            assert!(joined.unwrap().is_ok(), "decode loop task {i} panicked");
        }
        for (i, completed) in completions.into_iter().enumerate() {
            assert!(
                completed.load(std::sync::atomic::Ordering::SeqCst),
                "task {i} exited without reaching its post-cancellation completion marker"
            );
        }
    }

    #[test]
    fn insert_window_state_cancels_prior_entrys_decode_loop() {
        // #682's own log evidence points at the replacement-insert path (a
        // republish's `insert_window_state` call overwriting the map entry
        // directly, with NO removal call in between) as the likely actual
        // defect. `insert_window_state` must cancel whatever it displaces on
        // its own -- this is the regression test for exactly that path,
        // independent of any explicit `remove_window_state` call.
        let states: Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let key = ReceiveWindowKey::new("issue682-republish-owner".to_string(), 683);

        let old_token = CancellationToken::new();
        insert_window_state(
            &states,
            key.clone(),
            ReceiveWindowState::new(
                "issue682-republish-owner".to_string(),
                "petal-window-683".to_string(),
                VideoColorProfile::BT601_VIDEO,
                Instant::now(),
                old_token.clone(),
            ),
        );
        assert!(!old_token.is_cancelled());

        let new_token = CancellationToken::new();
        // Simulates the production `TrackSubscribed` arm's republish path: a
        // second `TrackSubscribed` for the SAME (owner, window_id) calls
        // `insert_window_state` again -- no `remove_window_state` runs
        // in between, exactly like the real event handler.
        insert_window_state(
            &states,
            key.clone(),
            ReceiveWindowState::new(
                "issue682-republish-owner".to_string(),
                "petal-window-683-v2".to_string(),
                VideoColorProfile::BT601_VIDEO,
                Instant::now(),
                new_token.clone(),
            ),
        );

        assert!(
            old_token.is_cancelled(),
            "the OLD decode loop's token must be cancelled by the replacement insert alone"
        );
        assert!(
            !new_token.is_cancelled(),
            "the NEW decode loop's token must still be live"
        );
        assert_eq!(states.lock_unpoisoned().len(), 1);
    }

    #[test]
    fn republish_regression_miss_counter_stays_flat_after_replacement() {
        // The scenario #682 flags as most likely to be the actual defect
        // behind its own log evidence: publish -> unpublish -> republish
        // (the quality-switch shape). Misses recorded while the window was
        // genuinely unpublished must not keep accumulating once a fresh
        // subscription exists for the same key, and frames delivered to the
        // NEW subscription must never be misread as misses.
        let states: Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let key = ReceiveWindowKey::new("issue682-flat-owner".to_string(), 684);

        // Publish #1.
        insert_window_state(
            &states,
            key.clone(),
            ReceiveWindowState::new(
                "issue682-flat-owner".to_string(),
                "petal-window-684".to_string(),
                VideoColorProfile::BT601_VIDEO,
                Instant::now(),
                CancellationToken::new(),
            ),
        );
        assert!(mark_frame_received(&states, &key).is_some());

        // Unpublish (the real removal call).
        remove_window_state(&states, &key);

        // A stray/leaked frame lands after unpublish but before republish --
        // legitimately a miss (the state really is gone at this instant).
        assert!(mark_frame_received(&states, &key).is_none());
        assert_eq!(
            *retired_receive_state_frame_misses()
                .lock_unpoisoned()
                .get(&key)
                .expect("a miss must have been recorded"),
            1
        );

        // Republish under the same (owner, window_id).
        insert_window_state(
            &states,
            key.clone(),
            ReceiveWindowState::new(
                "issue682-flat-owner".to_string(),
                "petal-window-684".to_string(),
                VideoColorProfile::BT601_VIDEO,
                Instant::now(),
                CancellationToken::new(),
            ),
        );
        // The pre-republish miss must be gone, not carried forward.
        assert!(
            retired_receive_state_frame_misses()
                .lock_unpoisoned()
                .get(&key)
                .is_none(),
            "a republish must clear the prior subscription's miss count"
        );

        // Frames from the NEW loop are hits and must never touch the miss
        // counter -- this is the double-feed/corruption hazard #682 flags: a
        // leaked old loop that finds the NEW entry and "successfully" marks
        // frames received under it.
        for _ in 0..5 {
            assert!(mark_frame_received(&states, &key).is_some());
        }
        assert!(
            retired_receive_state_frame_misses()
                .lock_unpoisoned()
                .get(&key)
                .is_none(),
            "misses must stay flat (absent) once the republished subscription is receiving"
        );
    }

    #[test]
    fn mark_frame_received_is_none_for_untracked_window() {
        // #682 item 3: the miss case must be an early exit the caller can
        // detect and skip all further per-frame work on, not a fall-through
        // with a fallback value. See this file's mutation-check note in the
        // commit message for how this was manually verified to actually gate
        // the caller's `continue`.
        let states: Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let key = ReceiveWindowKey::new("issue682-miss-owner".to_string(), 685);
        assert_eq!(mark_frame_received(&states, &key), None);
    }

    #[test]
    fn mark_frame_received_is_some_for_tracked_window() {
        let states: Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let key = ReceiveWindowKey::new("issue682-hit-owner".to_string(), 686);
        insert_window_state(
            &states,
            key.clone(),
            ReceiveWindowState::new(
                "issue682-hit-owner".to_string(),
                "petal-window-686".to_string(),
                VideoColorProfile::BT601_VIDEO,
                Instant::now(),
                CancellationToken::new(),
            ),
        );
        assert_eq!(
            mark_frame_received(&states, &key),
            Some(VideoColorProfile::BT601_VIDEO)
        );
    }

    #[test]
    fn window_frame_miss_counters_are_independent_per_window() {
        // #682 item 4: the counter must be per-window, not a shared/global
        // aggregate, so two different leaking windows don't corrupt each
        // other's counts (and so a log line can name the actual offender).
        let states: Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let key_a = ReceiveWindowKey::new("issue682-owner-a".to_string(), 687);
        let key_b = ReceiveWindowKey::new("issue682-owner-b".to_string(), 688);

        for _ in 0..3 {
            assert!(mark_frame_received(&states, &key_a).is_none());
        }
        assert!(mark_frame_received(&states, &key_b).is_none());

        let misses = retired_receive_state_frame_misses().lock_unpoisoned();
        assert_eq!(*misses.get(&key_a).unwrap(), 3);
        assert_eq!(*misses.get(&key_b).unwrap(), 1);
    }

    // ---- pre-existing suite -------------------------------------------------

    #[test]
    fn playout_delay_unset_is_disabled() {
        assert_eq!(parse_playout_delay_ms(None), Ok(None));
    }

    #[test]
    fn playout_delay_zero_is_valid() {
        assert_eq!(parse_playout_delay_ms(Some("0")), Ok(Some(0)));
    }

    #[test]
    fn playout_delay_positive_value_is_valid() {
        assert_eq!(parse_playout_delay_ms(Some("125")), Ok(Some(125)));
    }

    #[test]
    fn playout_delay_negative_value_is_rejected() {
        let error = parse_playout_delay_ms(Some("-1")).unwrap_err();
        assert!(error.contains(PLAYOUT_DELAY_ENV));
        assert!(error.contains("non-negative integer"));
    }

    #[test]
    fn playout_delay_non_numeric_value_is_rejected() {
        let error = parse_playout_delay_ms(Some("fast")).unwrap_err();
        assert!(error.contains(PLAYOUT_DELAY_ENV));
        assert!(error.contains("non-negative integer"));
    }

    #[test]
    fn playout_delay_empty_value_is_rejected() {
        let error = parse_playout_delay_ms(Some("")).unwrap_err();
        assert!(error.contains(PLAYOUT_DELAY_ENV));
        assert!(error.contains("non-negative integer"));
    }

    #[test]
    fn playout_delay_whitespace_value_is_rejected() {
        let error = parse_playout_delay_ms(Some(" 75 ")).unwrap_err();
        assert!(error.contains(PLAYOUT_DELAY_ENV));
        assert!(error.contains("non-negative integer"));
    }

    #[test]
    fn shared_window_subscription_policy_is_gated_to_window_tracks() {
        // The HIGH layer + canonical-dimension hint policy is only for
        // recognized shared-WINDOW tracks; camera tracks must never receive
        // it. The gate lives in the feeds' track-name classification, which
        // is the only path into
        // `register_and_request_shared_window_subscription`.
        assert!(
            crate::transport::publisher::window_id_from_track_name("petal-window-42").is_some()
        );
        assert!(
            crate::transport::publisher::window_id_from_track_name("petal-camera-alice").is_none()
        );
    }

    #[test]
    fn canonical_dimension_hint_ignores_zero_sizes() {
        // The shared policy's dimension hint must refuse degenerate sizes:
        // a zero-sized canonical hint is meaningless to the SFU and would
        // only churn the subscription settings. `update_window_subscription_dimensions`
        // guards the same invariant the helper relies on.
        // (No publication is constructible in a unit test; the guard itself
        // is the observable contract.)
        assert_eq!(canonical_subscription_dimensions(0, 1215), None);
        assert_eq!(canonical_subscription_dimensions(1215, 0), None);
        assert_eq!(
            canonical_subscription_dimensions(1215, 719),
            Some((1215, 719))
        );
    }

    #[test]
    fn no_frame_watchdog_retires_unmuted_non_reconnecting_stall() {
        let subscribed = Instant::now();
        let now = subscribed + NO_FRAME_RETIRE_AFTER + Duration::from_secs(1);
        assert_eq!(
            no_frame_decision(now, subscribed, Some(subscribed), false, false, false),
            NoFrameDecision::Retire
        );
    }

    #[test]
    fn no_frame_watchdog_keeps_recent_or_intentional_silence() {
        let subscribed = Instant::now();
        let recent = subscribed + Duration::from_secs(10);
        assert_eq!(
            no_frame_decision(recent, subscribed, Some(subscribed), false, false, false),
            NoFrameDecision::Keep
        );
        let stale = subscribed + NO_FRAME_RETIRE_AFTER + Duration::from_secs(1);
        assert_eq!(
            no_frame_decision(stale, subscribed, Some(subscribed), true, false, false),
            NoFrameDecision::Keep
        );
        assert_eq!(
            no_frame_decision(stale, subscribed, Some(subscribed), false, true, false),
            NoFrameDecision::Keep
        );
    }

    #[test]
    fn stale_unpublish_does_not_remove_replacement_window() {
        assert!(!should_remove_window(Some("new-sid"), "old-sid"));
        assert!(should_remove_window(Some("current-sid"), "current-sid"));
        assert!(!should_remove_window(None, "old-sid"));
    }

    /// #627: the case the sid guard cannot answer. Measured against a real SFU
    /// (`examples/share_lifecycle_probe`, 10/10 runs) `TrackSubscribed(new)`
    /// arrives 84-135ms BEFORE `TrackUnpublished(old)`, so the guard usually
    /// holds -- but when it loses, hiding a live share is the #627 failure.
    /// The replacement check does not depend on that ordering: the sender
    /// awaits its new publish before unpublishing the old track, so the SFU
    /// holds the replacement whenever the unpublish exists at all.
    #[test]
    fn republish_holds_the_window_when_the_sfu_still_has_a_publication() {
        assert_eq!(
            teardown_decision(Some("old-sid"), "old-sid", true),
            TeardownDecision::HoldForReplacement
        );
    }

    #[test]
    fn a_genuinely_ended_share_still_removes_its_window() {
        assert_eq!(
            teardown_decision(Some("old-sid"), "old-sid", false),
            TeardownDecision::RemoveWindow
        );
    }

    /// Exercise the decision used by the real `RoomEvent::TrackUnsubscribed`
    /// arm. A full reconnect delivers this event before it has published the
    /// replacement or announced `Reconnecting`, so it must retain the exact
    /// tracked SID that `TrackSubscribed`/reconciliation need to resume or
    /// eventually retire the window.
    #[test]
    fn track_unsubscribe_event_holds_the_tracked_window_until_terminal_evidence() {
        let tracked_sid = Some("TR_old");
        let decision = track_unsubscribe_decision(tracked_sid, "TR_old");

        assert_eq!(decision, TeardownDecision::HoldForTransientUnsubscribe);
        assert_eq!(registry_update_for(decision), RegistryUpdate::Keep);
        assert_eq!(
            teardown_decision(tracked_sid, "TR_old", false),
            TeardownDecision::RemoveWindow,
            "the later TrackUnpublished remains the terminal event"
        );
    }

    /// #840, live 2026-08-20: a sharer republishing every ~300ms made this
    /// receiver hide and re-reveal a live remote window 94 times in 73s. The
    /// hold failed every cycle (pool reuse clears `revealed_first_frame`), and
    /// the failure was wrongly treated as authority to hide -- while the SFU
    /// held a publication throughout, which the same log line asserted.
    #[test]
    fn a_failed_hold_on_a_non_terminal_teardown_never_hides_an_open_window() {
        assert_eq!(
            undisplayable_hold_fallback(true),
            UndisplayableHoldFallback::KeepTracked,
            "an open window behind the reveal gate must stay tracked, never be torn down"
        );
        assert_eq!(
            undisplayable_hold_fallback(false),
            UndisplayableHoldFallback::Remove,
            "with no compositor window there is stale receive state to clean up"
        );
    }

    #[test]
    fn stale_track_unsubscribe_cannot_hold_or_remove_a_replacement_generation() {
        let decision = track_unsubscribe_decision(Some("TR_replacement"), "TR_old");
        assert_eq!(decision, TeardownDecision::IgnoreSuperseded);
        assert_eq!(registry_update_for(decision), RegistryUpdate::Keep);
    }

    #[test]
    fn a_superseded_unpublish_is_ignored_regardless_of_replacement_state() {
        // Pre-existing #355 behaviour is preserved on both branches: if the
        // sid is already superseded there is nothing to decide.
        assert_eq!(
            teardown_decision(Some("new-sid"), "old-sid", true),
            TeardownDecision::IgnoreSuperseded
        );
        assert_eq!(
            teardown_decision(Some("new-sid"), "old-sid", false),
            TeardownDecision::IgnoreSuperseded
        );
        assert_eq!(
            teardown_decision(None, "old-sid", false),
            TeardownDecision::IgnoreSuperseded
        );
    }

    /// #627 regression, and the nastier half of the bug: a HELD window must
    /// stay in the publication registry.
    ///
    /// Every path that can retire a compositor window is keyed off that entry
    /// -- the teardown arms (a missing entry decides `IgnoreSuperseded`
    /// forever) and, critically, `reconcile`'s `Divergence::Orphaned`, which is
    /// only reported for keys present in `tracked`. Dropping it on a hold left
    /// the window frozen on screen with NOTHING able to remove it once the
    /// share really ended: a permanent phantom, worse than the vanishing this
    /// change fixes.
    #[test]
    fn a_held_window_stays_tracked_so_reconciliation_can_still_retire_it() {
        assert_eq!(
            registry_update_for(TeardownDecision::HoldForReplacement),
            RegistryUpdate::Keep
        );
        assert_eq!(
            registry_update_for(TeardownDecision::HoldForTransientUnsubscribe),
            RegistryUpdate::Keep
        );
        assert_eq!(
            registry_update_for(TeardownDecision::IgnoreSuperseded),
            RegistryUpdate::Keep
        );
        // Only a real teardown drops the entry, and even then conditionally --
        // see `resolve_teardown`.
        assert_eq!(
            registry_update_for(TeardownDecision::RemoveWindow),
            RegistryUpdate::RemoveIfUnchanged
        );
    }

    /// #631 is lifecycle wiring, not a pure decision-table change. The same
    /// production helper used by the SDK event arm must hold the participant's
    /// panel while leaving both tracking stores intact for a later
    /// `Divergence::Orphaned` genuine-departure verdict.
    #[test]
    #[cfg(target_os = "macos")]
    fn participant_disconnect_event_holds_without_forgetting_tracking() {
        let receive_key = ReceiveWindowKey::new("bob".into(), 7);
        let publication_key = ("bob".to_string(), 7);
        let receive_states = HashMap::from([(receive_key, "still-decoding")]);
        let publications = HashMap::from([(publication_key, "TR_bob_7")]);
        let receive_before = receive_states.clone();
        let publications_before = publications.clone();
        let mut held_identities = Vec::new();

        handle_participant_disconnected("Bob", "bob", |identity| {
            held_identities.push(identity.to_string());
        });

        assert_eq!(held_identities, ["bob"]);
        assert_eq!(receive_states, receive_before);
        assert_eq!(publications, publications_before);
    }

    /// A full reconnect can exceed `ORPHANED_GRACE`. The feed's actual
    /// lifecycle state must keep the real tracked entry alive throughout that
    /// interval, then restart the grace after `Reconnected` so a participant
    /// who genuinely left still reaches the terminal retirement step.
    #[test]
    fn reconnect_defers_orphan_retirement_then_a_real_departure_retires() {
        use crate::transport::reconcile::{
            plan_recovery_steps_for_connection, reconcile, RecoveryStep, TrackedWindow,
            ORPHANED_GRACE,
        };

        let tracked = vec![TrackedWindow {
            owner_identity: "bob".into(),
            window_id: 7,
            sid: "TR_bob_7".into(),
        }];
        let findings = reconcile(&[], &tracked);
        let start = Instant::now();
        let mut ledger = crate::transport::reconcile::RecoveryLedger::new();
        let mut lifecycle = ReconnectLifecycle::default();

        lifecycle.set_reconnecting(true);
        let during_long_reconnect = plan_recovery_steps_for_connection(
            lifecycle.is_reconnecting(),
            &findings,
            &tracked,
            &mut ledger,
            start + ORPHANED_GRACE * 10,
        );
        assert_eq!(during_long_reconnect, vec![RecoveryStep::Wait]);
        assert_eq!(ledger.orphan_sighting_count(), 0);
        assert_eq!(tracked.len(), 1, "the real registry entry stays retirable");

        lifecycle.set_reconnecting(false);
        let first_absent_after_resume = plan_recovery_steps_for_connection(
            lifecycle.is_reconnecting(),
            &findings,
            &tracked,
            &mut ledger,
            start + ORPHANED_GRACE * 11,
        );
        assert_eq!(first_absent_after_resume, vec![RecoveryStep::Wait]);
        let genuine_departure = plan_recovery_steps_for_connection(
            lifecycle.is_reconnecting(),
            &findings,
            &tracked,
            &mut ledger,
            start + ORPHANED_GRACE * 12,
        );
        assert_eq!(genuine_departure, vec![RecoveryStep::ReportTruth]);
    }

    /// A held window must not re-trigger the watchdog on every 5s tick: the
    /// receive state is kept (so the watchdog can re-arm later), which without
    /// this flag would re-request repair and re-log forever.
    #[test]
    fn a_held_stall_does_not_re_fire_the_watchdog() {
        let subscribed = Instant::now();
        let stale = subscribed + NO_FRAME_RETIRE_AFTER + Duration::from_secs(1);
        assert_eq!(
            no_frame_decision(stale, subscribed, Some(subscribed), false, false, true),
            NoFrameDecision::Keep
        );
        // ...and still fires the first time.
        assert_eq!(
            no_frame_decision(stale, subscribed, Some(subscribed), false, false, false),
            NoFrameDecision::Retire
        );
    }

    #[test]
    fn receiver_frame_health_log_contract_includes_fps_and_gap() {
        let line = format_receiver_frame_health(&ReceiverFrameHealth {
            window_id: 42,
            owner_identity: "alice".to_string(),
            total_frames: 150,
            fps: 29.94,
            gap_since_last_frame: Duration::from_millis(1234),
            live_pixel_buffers: Some(3),
        });
        assert_eq!(
            line,
            "compositor feed: window 42 receiver frame health from 'alice' -- frames=150 compositor_fps=29.9 gap_since_last_frame_ms=1234 pixbufs=3"
        );
    }

    #[test]
    fn receiver_frame_health_log_contract_reports_pixbufs_na_when_not_tracked() {
        let line = format_receiver_frame_health(&ReceiverFrameHealth {
            window_id: 42,
            owner_identity: "alice".to_string(),
            total_frames: 150,
            fps: 29.94,
            gap_since_last_frame: Duration::from_millis(1234),
            live_pixel_buffers: None,
        });
        assert!(
            line.ends_with("pixbufs=n/a"),
            "a platform without the counter wired up must report n/a, not a \
             plausible-looking zero: {line}"
        );
    }

    #[test]
    fn receiver_frame_health_uses_the_actual_elapsed_sample_window() {
        let subscribed_at = Instant::now();
        let sampled_at = subscribed_at + Duration::from_millis(9_900);
        let key = ReceiveWindowKey::new("alice".to_string(), 42);
        let mut state = ReceiveWindowState::new(
            "alice".to_string(),
            "petal-window-42".to_string(),
            VideoColorProfile::BT601_VIDEO,
            subscribed_at,
            CancellationToken::new(),
        );
        // A 4.9s timer tick was too early for the 5s gate, so this sample
        // covers the full 9.9s since the last emitted health line.
        state.frames_received = 300;
        state.last_health_log_frames = 3;

        let health = receiver_frame_health(&key, &state, sampled_at);

        assert!((health.fps - 30.0).abs() < 1e-9, "got {}", health.fps);
        // #683: confirms this is actually wired to `platform::mem`, not
        // just always `None` -- see `platform::mem::live_pixel_buffer_count`
        // for why it's `Some` only on macOS.
        #[cfg(target_os = "macos")]
        assert!(health.live_pixel_buffers.is_some());
        #[cfg(not(target_os = "macos"))]
        assert!(health.live_pixel_buffers.is_none());
    }

    #[test]
    fn receiver_color_profile_uses_metadata_when_available() {
        let metadata = r#"{"petalWindowColorProfiles":{"42":{"primaries":"display-p3","transfer":"srgb","matrix":"bt709","range":"full"}}}"#;
        assert_eq!(
            shared_window_color_profile_or_default(metadata, 42),
            VideoColorProfile::DISPLAY_P3_BT709_FULL
        );
    }

    #[test]
    fn receiver_color_profile_falls_back_for_missing_or_bad_metadata() {
        assert_eq!(
            shared_window_color_profile_or_default("{}", 42),
            VideoColorProfile::BT601_VIDEO
        );
        assert_eq!(
            shared_window_color_profile_or_default(
                r#"{"petalWindowColorProfiles":{"42":{"primaries":"display-p3","transfer":"srgb","matrix":"bogus","range":"full"}}}"#,
                42,
            ),
            VideoColorProfile::BT601_VIDEO
        );
    }

    #[test]
    fn refreshed_color_profile_picked_up_when_metadata_carries_it() {
        // #251: a metadata update that now carries a color_profile entry
        // (e.g. arriving after #249's >3s stall published the track first)
        // must be picked up as a change.
        let metadata = r#"{"petalWindowColorProfiles":{"42":{"primaries":"display-p3","transfer":"srgb","matrix":"bt709","range":"full"}}}"#;
        assert_eq!(
            refreshed_color_profile_from_metadata(metadata, 42),
            Some(VideoColorProfile::DISPLAY_P3_BT709_FULL)
        );
    }

    #[test]
    fn refreshed_color_profile_is_none_when_metadata_omits_it() {
        // A metadata republish that doesn't carry color info (e.g. a
        // title-only update, or a different window_id's entry) must return
        // None so the caller never clobbers an already-correct profile with
        // the legacy default.
        assert_eq!(refreshed_color_profile_from_metadata("{}", 42), None);
        assert_eq!(
            refreshed_color_profile_from_metadata(r#"{"petalWindowTitles":{"42":"Terminal"}}"#, 42,),
            None
        );
    }

    #[test]
    fn update_receive_window_color_profile_only_writes_on_change() {
        let states: Arc<Mutex<HashMap<ReceiveWindowKey, ReceiveWindowState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let key = ReceiveWindowKey::new("owner-1".to_string(), 42);
        states.lock_unpoisoned().insert(
            key.clone(),
            ReceiveWindowState::new(
                "owner-1".to_string(),
                "petal-window-42".to_string(),
                VideoColorProfile::BT601_VIDEO,
                Instant::now(),
                CancellationToken::new(),
            ),
        );

        // Same profile: no-op, reports unchanged.
        assert!(!update_receive_window_color_profile(
            &states,
            &key,
            VideoColorProfile::BT601_VIDEO
        ));

        // Different profile: applied, reports changed.
        assert!(update_receive_window_color_profile(
            &states,
            &key,
            VideoColorProfile::DISPLAY_P3_BT709_FULL
        ));
        assert_eq!(
            states.lock_unpoisoned().get(&key).unwrap().color_profile,
            VideoColorProfile::DISPLAY_P3_BT709_FULL
        );

        // Unknown key: no-op, doesn't panic.
        let missing_key = ReceiveWindowKey::new("owner-2".to_string(), 7);
        assert!(!update_receive_window_color_profile(
            &states,
            &missing_key,
            VideoColorProfile::DISPLAY_P3_BT709_FULL
        ));
    }

    #[test]
    fn display_source_titles_are_prefixed_for_headers() {
        assert_eq!(
            source_title_for_kind(
                crate::transport::publisher::SharedSourceKind::Display,
                "Built-in Retina Display"
            ),
            "Screen - Built-in Retina Display"
        );
        assert_eq!(
            source_title_for_kind(
                crate::transport::publisher::SharedSourceKind::Display,
                "Screen 1"
            ),
            "Screen 1"
        );
        assert_eq!(
            source_title_for_kind(
                crate::transport::publisher::SharedSourceKind::Window,
                "Terminal"
            ),
            "Terminal"
        );
        assert_eq!(
            source_title_for_kind(
                crate::transport::publisher::SharedSourceKind::DisplayRegion,
                "ignored native title"
            ),
            "Petal View"
        );
    }

    #[test]
    fn receive_window_state_key_includes_owner_identity() {
        let window_id = 96;
        let alice = ReceiveWindowKey::new("alice".to_string(), window_id);
        let bob = ReceiveWindowKey::new("bob".to_string(), window_id);
        let mut states = HashMap::new();

        states.insert(
            alice.clone(),
            ReceiveWindowState::new(
                "alice".to_string(),
                "petal-window-96".to_string(),
                VideoColorProfile::BT601_VIDEO,
                Instant::now(),
                CancellationToken::new(),
            ),
        );
        states.insert(
            bob.clone(),
            ReceiveWindowState::new(
                "bob".to_string(),
                "petal-window-96".to_string(),
                VideoColorProfile::BT601_VIDEO,
                Instant::now(),
                CancellationToken::new(),
            ),
        );

        assert_eq!(states.len(), 2);
        states.remove(&alice);
        assert!(!states.contains_key(&alice));
        assert!(states.contains_key(&bob));
    }

    /// #357 regression, driven against a real SFU rather than a pure helper.
    ///
    /// The bug: a participant who joins *after* someone already started
    /// sharing never saw that window. The cause was ordering, not logic --
    /// `RoomConnection::connect` dropped the event receiver `Room::connect`
    /// had already registered, and `start_compositor_feed` registered a
    /// fresh one a signaling round trip later. LiveKit's dispatcher does no
    /// replay (`Dispatcher::dispatch` is pure fan-out to currently-
    /// registered senders), so every join-time event -- the `Connected`
    /// snapshot and the `TrackSubscribed` the SDK's room task emits for each
    /// already-published track -- landed in the dropped receiver.
    ///
    /// No unit test on an extracted helper can catch that class of bug: none
    /// of the helpers were ever wrong, they were simply never called,
    /// because the events carrying their inputs had already been discarded.
    /// So this test asserts on the ordering itself, through the real
    /// functions the app calls: `RoomConnection::connect` and
    /// `RoomConnection::take_compositor_events`, the exact pair
    /// `session::join_room` hands to `start_compositor_feed`.
    ///
    /// Scope boundary, stated honestly: this covers join -> subscribe ->
    /// decode. It stops short of the AppKit render, which needs a real
    /// `tauri::AppHandle` and a main-thread native window and therefore
    /// cannot run under the test harness. `examples/share_lifecycle_probe
    /// --late-joiner` covers the same seam with two positive controls and a
    /// trial count; this test is the CI-runnable core of it.
    ///
    /// Gated on a LiveKit server because it needs one:
    ///
    /// ```sh
    /// LIVEKIT_URL=ws://localhost:7880 LIVEKIT_API_KEY=devkey \
    /// LIVEKIT_API_SECRET=secretsecretsecretsecretsecretsecret \
    ///   cargo test --lib late_joiner_receives -- --ignored --nocapture
    /// ```
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a local LiveKit server (LIVEKIT_URL) -- see doc comment"]
    async fn late_joiner_receives_an_already_active_window_share() {
        use crate::transport::RoomConnection;
        use futures::StreamExt;
        use livekit::options::{TrackPublishOptions, VideoCodec};
        use livekit::prelude::*;
        use livekit::track::{LocalTrack, LocalVideoTrack, RemoteTrack};
        use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
        use livekit::webrtc::video_source::native::NativeVideoSource;
        use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
        use livekit::webrtc::video_stream::native::NativeVideoStream;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::time::Duration;

        const W: u32 = 640;
        const H: u32 = 400;
        const WINDOW_ID: u32 = 357;
        /// The share is already live and present in the SFU's join offer, so
        /// this budget is generous by orders of magnitude. It exists to fail
        /// the test rather than hang it.
        const DISCOVERY_BUDGET: Duration = Duration::from_secs(10);

        let Ok(url) = crate::transport::token::livekit_url() else {
            panic!("LIVEKIT_URL must be set for this #[ignore]d test");
        };
        let room_name = format!(
            "petal-357-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );
        let token = |identity: &str, publish: bool| {
            crate::transport::mint_access_token(identity, &room_name, publish, !publish)
                .expect("mint token")
        };

        let stop = Arc::new(AtomicBool::new(false));

        /// Watch one joiner's event receiver for the already-active window
        /// share, counting decoded frames. Frames rather than the event
        /// alone: an event with no media behind it still leaves the user
        /// looking at nothing, which is the symptom #357 reports.
        fn watch(
            mut events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
            stop: Arc<AtomicBool>,
        ) -> Arc<AtomicU64> {
            let frames = Arc::new(AtomicU64::new(0));
            let out = frames.clone();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let RoomEvent::TrackSubscribed { track, .. } = event else {
                        continue;
                    };
                    let RemoteTrack::Video(video) = track else {
                        continue;
                    };
                    // Same classification `start_compositor_feed` applies.
                    if crate::transport::publisher::window_id_from_track_name(&video.name())
                        != Some(WINDOW_ID)
                    {
                        continue;
                    }
                    let f = frames.clone();
                    let s = stop.clone();
                    tokio::spawn(async move {
                        let mut stream = NativeVideoStream::new(video.rtc_track());
                        while stream.next().await.is_some() {
                            if s.load(Ordering::Relaxed) {
                                break;
                            }
                            f.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
            });
            out
        }

        // --- in-run positive control ------------------------------------
        // This peer joins BEFORE anything is published, so #357 cannot
        // affect it. If it fails to see the share, the environment or the
        // harness is broken and the late joiner's result below means
        // nothing -- so it is asserted first, separately.
        let early = RoomConnection::connect(&url, &token("petal-357-early", false))
            .await
            .expect("early observer connects");
        let early_frames = watch(
            early
                .take_compositor_events()
                .expect("connect-time receiver present"),
            stop.clone(),
        );

        // --- publisher ---------------------------------------------------
        let publisher = RoomConnection::connect(&url, &token("petal-357-pub", true))
            .await
            .expect("publisher connects");
        publisher.discard_compositor_events();
        let source = NativeVideoSource::new(
            VideoResolution {
                width: W,
                height: H,
            },
            true,
        );
        let track = LocalVideoTrack::create_video_track(
            &format!("petal-window-{WINDOW_ID}"),
            RtcVideoSource::Native(source.clone()),
        );
        publisher
            .room()
            .local_participant()
            .publish_track(
                LocalTrack::Video(track),
                TrackPublishOptions {
                    source: TrackSource::Screenshare,
                    video_codec: VideoCodec::H264,
                    ..Default::default()
                },
            )
            .await
            .expect("publish window share");

        let feed_stop = stop.clone();
        tokio::spawn(async move {
            // Changing content, so the encoder cannot coast and stop
            // producing packets while the late joiner is still connecting.
            let mut tick = tokio::time::interval(Duration::from_millis(33));
            let mut n = 0u32;
            while !feed_stop.load(Ordering::Relaxed) {
                tick.tick().await;
                let mut buf = I420Buffer::new(W, H);
                {
                    let (y, u, v) = buf.data_mut();
                    let band = (n * 7) % H;
                    for row in 0..H as usize {
                        let luma = if row.abs_diff(band as usize) < 40 {
                            235
                        } else {
                            16
                        };
                        let start = row * W as usize;
                        y[start..start + W as usize].fill(luma);
                    }
                    u.fill(128);
                    v.fill(128);
                }
                source.capture_frame(&VideoFrame {
                    rotation: VideoRotation::VideoRotation0,
                    timestamp_us: 0,
                    frame_metadata: None,
                    buffer: &buf,
                });
                n = n.wrapping_add(1);
            }
        });

        let reached = |frames: &Arc<AtomicU64>| frames.load(Ordering::Relaxed) > 0;
        let await_frames = |frames: Arc<AtomicU64>| async move {
            let deadline = tokio::time::Instant::now() + DISCOVERY_BUDGET;
            while tokio::time::Instant::now() < deadline {
                if frames.load(Ordering::Relaxed) > 0 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        };

        assert!(
            await_frames(early_frames.clone()).await,
            "positive control failed: a peer that joined BEFORE the share -- which #357 \
             cannot affect -- decoded no frames either. The SFU or this harness is broken, \
             so the late-joiner result below would be uninterpretable."
        );

        // The share is now unambiguously established, not racing the join.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // --- the late joiner: the actual regression ----------------------
        let late = RoomConnection::connect(&url, &token("petal-357-late", false))
            .await
            .expect("late joiner connects");
        let late_frames = watch(
            late.take_compositor_events()
                .expect("connect-time receiver present"),
            stop.clone(),
        );

        let late_ok = await_frames(late_frames.clone()).await;

        stop.store(true, Ordering::Relaxed);
        late.room().close().await.ok();
        early.room().close().await.ok();
        publisher.room().close().await.ok();

        assert!(
            late_ok,
            "#357 reproduces: a peer joining after the share started decoded no frames \
             from the already-active window within {DISCOVERY_BUDGET:?}, while the \
             positive control (early joiner, same room, same run) decoded {}. The \
             connect-time event receiver is not reaching the compositor feed.",
            early_frames.load(Ordering::Relaxed)
        );
        assert!(reached(&early_frames));
    }
}
