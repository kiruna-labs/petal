//! Connection resilience (SPEC.md §4.8) -- macOS-only, following the same
//! "own module, follow the house `emit_to` pattern" shape. Its room events
//! come from the connect-time fanout rather than a late `room.subscribe()`.
//!
//! ## What LiveKit's own `livekit` 0.7.49 Rust SDK already does for free
//! (verified by reading the crate source directly -- `rtc_engine/mod.rs`,
//! `rtc_engine/reconnect_strategy.rs`, `room/mod.rs` -- not assumed):
//!
//! - **Reconnect + exponential backoff with full jitter.** The room engine
//!   (`rtc_engine/mod.rs`'s `EngineInner::reconnect_task`) already retries a
//!   dropped connection up to `RECONNECT_ATTEMPTS` (10) times, spaced by
//!   `reconnect_strategy.rs`'s jittered exponential backoff (300ms base,
//!   x2/attempt, capped at 7s, sampled uniformly from `[0, cap]`). This is
//!   NOT something this module reimplements -- SPEC.md's "exponential
//!   backoff with jitter" requirement is already met by the SDK.
//! - **Resume vs. full reconnect, decided by the engine itself.** A "resume"
//!   (lightweight: reopen signalling, ICE-restart the existing
//!   PeerConnection, no new tracks) is tried first; if that fails, or the
//!   server explicitly asks for it, the engine escalates to a "full
//!   reconnect" (brand-new `RtcSession`). Either way, `RoomEvent::Reconnecting`
//!   fires when the attempt starts and `RoomEvent::Reconnected` when it
//!   succeeds -- both handled below.
//! - **Local track republish IS automatic, but only on a full reconnect.**
//!   `room/mod.rs`'s `handle_restarted` snapshots every currently-published
//!   local track, unpublishes + republishes each one against the new
//!   session (same `Track` Arc, same `TrackPublishOptions` -- including this
//!   app's `ShareQuality`-derived `video_encoding`, so a share's focus tier
//!   survives a full reconnect without this module doing anything), and
//!   only dispatches `RoomEvent::Reconnected` once every republish attempt
//!   has been tried. On a "resume" reconnect, no republish happens at all --
//!   because a resume never tears down the existing PeerConnection/tracks in
//!   the first place, so there is nothing to republish.
//!   **This means SPEC.md's "re-publish active shares automatically on
//!   reconnect" requirement is, for the actual media tracks, already
//!   satisfied by the SDK itself** -- `session.rs`'s `ActiveShare.published`
//!   wraps the exact same `LocalVideoTrack` the SDK republishes in place
//!   (confirmed: `PublishedTrack::sid()`/`quality()` read live state off
//!   that track, not a cached copy taken at publish time, so they can't go
//!   stale across an SDK-driven republish). What this module adds on top is
//!   *not* an unbounded republish loop -- it's (a) surfacing the Reconnecting/
//!   Reconnected/Disconnected transitions to the user as toasts (nothing did
//!   this before), and (b) a bounded, generation-gated post-Reconnected
//!   reconciliation pass that gives each still-active share one replacement
//!   publication attempt if the SDK's own full reconnect left the app with
//!   stale or invisible publication state.
//! - **TURN/petal candidates**: gathered entirely server-side (LiveKit
//!   Cloud/self-hosted SFU tells the client which STUN/TURN servers to use
//!   in its join/reconnect response; `RoomOptions.rtc_config.ice_servers`
//!   only overrides this if the app explicitly sets it, which this app does
//!   not). Matches SPEC.md §10's own framing ("TURN... included with the
//!   managed platform... non-negotiable") -- nothing to configure here.
//! - **Congestion control (transport-cc/GCC)**: entirely inside the vendored
//!   `webrtc-sys`/libwebrtc binary, no Rust-level surface at all (checked:
//!   zero `transport-cc`/`GoogCC`/`REMB`/bandwidth-estimate symbols anywhere
//!   in the `livekit`/`webrtc-sys` Rust source) -- there is nothing for this
//!   module, or any Rust code in this app, to verify or tune directly; this
//!   is a "confirm it's on by default" item, satisfied by the fact that
//!   real-world share-cap/focus-quality testing (M1 phase, see CLAUDE.md)
//!   already showed sane adaptive behavior under the SDK's default encoding
//!   presets.
//!
//! ## What genuinely does NOT exist in the SDK, and this module builds
//!
//! - **Network-change detection.** The SDK only reacts to WebRTC's own
//!   `PeerConnectionState::Failed` -- i.e. it waits for ICE to notice the
//!   path is dead (which can take many seconds under ICE's own consent-
//!   freshness timeouts) rather than reacting the instant the OS reports an
//!   interface change. There is no `NWPathMonitor`-equivalent anywhere in
//!   `livekit`/`webrtc-sys` (checked directly, zero hits). This module adds
//!   `start_network_monitor`, watching macOS's `SCDynamicStore`
//!   `State:/Network/Global/IPv4` key (the standard "primary network service
//!   changed" signal -- fires on a Wi-Fi<->Ethernet switch, VPN connect/
//!   disconnect, etc., which is more precise than a generic reachability
//!   flag for this purpose) and proactively calling
//!   `Room::simulate_scenario(SimulateScenario::SignalReconnect)` -- a real,
//!   non-test-gated public SDK method (checked: only a *different*,
//!   explicitly-named test helper on `Room` is behind
//!   `#[cfg(feature = "__lk-e2e-test")]`; `simulate_scenario` itself is not)
//!   that closes the signalling channel locally and lets the engine's own
//!   real resume logic (ICE restart included) take over immediately, instead
//!   of waiting out WebRTC's own failure-detection timeout.
//! - **Audio device hot-swap.** `PlatformAudio` (checked directly in
//!   `platform_audio/mod.rs`) has explicit recording/playout switch
//!   mechanisms, but no push notification for default-device changes --
//!   device enumeration is pull-only. This module adds one periodic poll that
//!   refreshes both the current mic and speaker handles (see
//!   `transport/audio.rs`), detecting a hot-plug/default-device change by diffing
//!   against the previous poll's device snapshot, the simplest correct
//!   option given there's no push API to hook instead (matches the existing
//!   house style of polling on a timer, e.g. `window_source.rs`/
//!   `hover_tab.rs`'s SPEC.md §4.2 window-list refresh, rather than adding a
//!   second, heavier CoreAudio `AudioObjectAddPropertyListener` FFI surface
//!   for a device-list diff that's already cheap to poll).
//! - **Display hot-swap (a shared window's display disconnects).** This
//!   module registers a single `CGDisplayRegisterReconfigurationCallback`.
//!   On a completed display reconfiguration it re-resolves every active
//!   shared CGWindowID: surviving windows keep their fresh frame (and the
//!   share pump's resize-republish path handles changed dimensions), while
//!   vanished windows are stopped and the stale hover-tab/border state is
//!   cleared instead of leaving receivers frozen forever.
//!
//! ## Toast delivery
//!
//! `Toast.svelte` already exists (built in an earlier phase) but was only
//! ever driven by a static example / the mock `/meeting/[room]` route's
//! fake "Leave" trigger -- neither is wired to any real session state. This
//! module broadcasts a global `resilience-event` Tauri event. This is
//! intentionally global, not `emit_to("main", ...)`: plain frontend
//! `listen()` registers an Any-target listener in Tauri 2, and label-targeted
//! emits do not reach it. `ToastHost.svelte` lives in `+layout.svelte`, so
//! it is present on every normal route in the main webview and renders the
//! existing `Toast` component; `Toast.svelte` itself is not modified.
//!
//! Issue #18 toast policy: emission is gated HERE (Rust), not in the
//! frontend -- a proactive resume we initiated ourselves stays silent
//! unless it exceeds `PROACTIVE_SILENT_GRACE` or fails; "Disconnected" is
//! reserved for non-client-initiated `RoomEvent::Disconnected` (attempts
//! exhausted / server closed us), never for our own `leave_room`.

use crate::sync_ext::MutexExt;
use std::ffi::c_void;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::resilience_event::ResilienceEvent;
use crate::session::RoomGeneration;
use crate::transport::audio::{PlayoutDeviceRefresh, RecordingDeviceRefresh};
use tauri::{AppHandle, Manager};

/// How often to poll for default audio-device changes (SPEC.md §4.8).
/// 2s is frequent enough that a user unplugging/plugging a mic or speaker
/// mid-meeting notices the switch within a couple of seconds, without
/// spinning a tight loop -- same order of magnitude as `hover_tab.rs`'s own
/// hover-poll cadence, just far cheaper per tick (an in-process device
/// enumeration call, not a window-list/CoreGraphics round trip).
const AUDIO_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Issue #18 toast policy: a reconnect WE initiated proactively (after a
/// detected network change) stays SILENT unless it's still unresolved after
/// this grace period -- the measured norm for a proactive resume is ~1s (see
/// CLAUDE.md's resilience section), and a 1s silent resume should never
/// produce "Disconnected"/"Reconnecting" scare copy. Only a genuinely slow
/// or failed reconnect surfaces UI.
const PROACTIVE_SILENT_GRACE: Duration = Duration::from_secs(3);

/// How long after we force a proactive reconnect a subsequent
/// `RoomEvent::Reconnecting` is still attributed to that trigger (vs. an
/// independent, SDK-detected failure, which surfaces UI immediately). The
/// engine picks our `SignalReconnect` up near-instantly, so 10s is generous.
const PROACTIVE_ATTRIBUTION_WINDOW: Duration = Duration::from_secs(10);

/// Issue #18 trigger debounce: one physical network event (interface flip,
/// VPN toggle) produces a burst of `SCDynamicStore` notifications; coalesce
/// anything within this window into the single reconnect already triggered.
const NETWORK_CHANGE_DEBOUNCE: Duration = Duration::from_secs(1);

/// Sleep/wake notifications can arrive in a short burst during lid-open or
/// display-unlock. One proactive reconnect/share refresh is enough.
const SYSTEM_WAKE_DEBOUNCE: Duration = Duration::from_secs(2);

/// Let LiveKit's own resume/full-reconnect handling settle before the app
/// publishes a replacement for still-active shares. The SDK emits
/// `Reconnected` after its full-reconnect republish attempts, but this small
/// grace avoids racing immediately-following room events and coalesces bursts.
const POST_RECONNECT_SHARE_REPAIR_DELAY: Duration = Duration::from_millis(250);

/// Start the SPEC.md §4.8 resilience watchers. Called once per room
/// connection from `session::join_room`, the same seam `telepointer::
/// start_receiver_for_room`/`transport::audio::start_audio_track_logger`
/// already use for "start once per room, not once per share."
///
/// Lifecycle (issue #18): only the `RoomEvent` toast bridge is genuinely
/// per-room -- it self-terminates when its room's event channel closes (and
/// explicitly breaks on `Disconnected`). The network-change monitor and the
/// audio device-hot-swap poll are SINGLE app-level watchers, started lazily on
/// the first join and never duplicated: both look up the CURRENT session
/// state at fire time (`SessionState` managed-state lookup / the
/// watch-handle closures) instead of capturing a per-join `Arc<Room>`.
/// Before this, every leave→rejoin cycle leaked one more network-watcher
/// thread, each still holding a room that had already been left -- observed
/// live as duplicate "interface changed" log lines plus a stale watcher's
/// "engine is closed" error.
pub fn start_for_room(
    app: &AppHandle,
    events: tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>,
    generation: RoomGeneration,
    mic: Option<MicWatchHandle>,
    speaker: Option<SpeakerWatchHandle>,
) {
    let watcher_started_at = Instant::now();
    log::info!("resilience: join tail stage=start_for_room_enter elapsed_ms=0 thread=join_tail");
    // The receiver was registered by `Room::connect` and fanned out before
    // join-tail work began (#357/#584). Defer only event processing; a late
    // `room.subscribe()` here would create a second, lossy authority.
    let _ = start_room_event_watcher(
        events,
        generation,
        Some(app.clone()),
        DisconnectEffects::production(app.clone()),
    );
    log::info!(
        "resilience: join tail stage=watcher_processing_started elapsed_ms={} thread=join_tail",
        watcher_started_at.elapsed().as_millis()
    );

    // The remaining monitors are app-lifetime conveniences, not prerequisites
    // for a usable LiveKit room. In particular, macOS display/NSWorkspace
    // registration can cross OS framework boundaries. Never let that delay
    // `join_room`'s terminal result or the active-meeting route (#559).
    let app = app.clone();
    schedule_monitor_bootstrap(move || start_app_global_monitors(app, mic, speaker));
    log::info!(
        "resilience: join tail stage=app_monitor_scheduled elapsed_ms={} thread=join_tail",
        watcher_started_at.elapsed().as_millis()
    );
}

/// Queue optional app-global resilience monitors outside both the room-join
/// critical path and the async executor's worker pool. macOS framework
/// registration is synchronous and can wedge, so it belongs on Tokio's
/// dedicated blocking pool rather than an `async_runtime::spawn` future.
/// Kept as a small helper so the non-blocking boundary has a focused
/// regression test.
fn schedule_monitor_bootstrap(task: impl FnOnce() + Send + 'static) {
    tauri::async_runtime::spawn_blocking(task);
}

fn start_app_global_monitors(
    app: AppHandle,
    mic: Option<MicWatchHandle>,
    speaker: Option<SpeakerWatchHandle>,
) {
    let started_at = Instant::now();
    log::debug!("resilience: app-global monitor bootstrap begin");
    ensure_network_monitor(&app);
    ensure_display_monitor(&app);
    ensure_sleep_wake_monitor(&app);
    if mic.is_some() || speaker.is_some() {
        ensure_audio_device_watch(app, mic, speaker);
    }
    log::info!(
        "resilience: app-global monitor bootstrap completed in {:?}",
        started_at.elapsed()
    );
}

/// A cheap, cloneable handle to the room's live `MicTrack` (if audio
/// published successfully) that the device-watch poll can call into without
/// reaching back through `SessionState`'s full mutex -- kept as its own tiny
/// type so `resilience.rs` doesn't need to know `session.rs`'s internal
/// locking shape, just "give me the current mic track, if any, each tick."
#[derive(Clone)]
pub struct MicWatchHandle {
    inner: Arc<dyn Fn() -> Option<Arc<crate::transport::audio::MicTrack>> + Send + Sync>,
}

impl MicWatchHandle {
    pub fn new(
        f: impl Fn() -> Option<Arc<crate::transport::audio::MicTrack>> + Send + Sync + 'static,
    ) -> Self {
        Self { inner: Arc::new(f) }
    }

    fn current(&self) -> Option<Arc<crate::transport::audio::MicTrack>> {
        (self.inner)()
    }
}

/// A cheap, cloneable handle to the room's live `SpeakerPlayout` (if audio
/// started successfully), re-read on every device-watch tick so the singleton
/// app-level poll always observes the current room.
#[derive(Clone)]
pub struct SpeakerWatchHandle {
    inner: Arc<dyn Fn() -> Option<Arc<crate::transport::audio::SpeakerPlayout>> + Send + Sync>,
}

impl SpeakerWatchHandle {
    pub fn new(
        f: impl Fn() -> Option<Arc<crate::transport::audio::SpeakerPlayout>> + Send + Sync + 'static,
    ) -> Self {
        Self { inner: Arc::new(f) }
    }

    fn current(&self) -> Option<Arc<crate::transport::audio::SpeakerPlayout>> {
        (self.inner)()
    }
}

/// Bridges `RoomEvent`s to user-facing toasts (SPEC.md §4.8's "surface
/// resilience events to the frontend as toasts" -- `Toast.svelte` already
/// exists with exactly this "reconnected"/"Switched to Ethernet" variant,
/// built for this, per the task brief; this is the first real wiring of it).
///
/// The resilience branch of `RoomConnection`'s connect-time fanout. It shares
/// the SDK receiver registered before the join tail begins, so initial
/// reconnect/disconnect events cannot be lost to a late subscription (#584).
/// Toast policy (issue #18): a proactive resume WE initiated is silent by
/// default -- UI only appears if it's still unresolved after
/// `PROACTIVE_SILENT_GRACE` (then "Reconnecting…", degraded -- NOT
/// "Disconnected") or if it fails outright. An SDK-detected reconnect
/// (WebRTC noticed real trouble on its own) still surfaces "Reconnecting…"
/// immediately, and the "Reconnected" confirmation toast is only shown when
/// a "Reconnecting…" toast was actually displayed -- a silent 1s resume
/// needs no confirmation either. "Disconnected" is reserved for
/// `RoomEvent::Disconnected` with a non-client-initiated reason (attempts
/// exhausted / server closed us) -- our own deliberate `leave_room` ->
/// `Room::close()` fires `Disconnected { ClientInitiated }`, which
/// previously produced a false "Disconnected — attempting to reconnect"
/// toast lingering into the NEXT meeting (the user's exact screenshot).
///
/// Self-terminating: breaks on `Disconnected` (the room is gone either
/// way), and `events.recv()` returns `None` once the room's dispatcher is
/// dropped -- verified, so this task does not outlive its room (audited per
/// the issue's lifecycle note; it was never the leak, the network monitor
/// thread was).
type DisconnectCleanup = Pin<Box<dyn Future<Output = ()> + Send>>;

struct DisconnectEffects {
    surface: Arc<dyn Fn(ResilienceEvent) + Send + Sync>,
    cleanup: Arc<dyn Fn() -> DisconnectCleanup + Send + Sync>,
}

impl DisconnectEffects {
    fn production(app: AppHandle) -> Self {
        let surface_app = app.clone();
        Self {
            surface: Arc::new(move |event| emit(&surface_app, event)),
            cleanup: Arc::new(move || {
                let app = app.clone();
                Box::pin(async move {
                    if let Some(state) = app.try_state::<crate::session::SessionState>() {
                        crate::session::cleanup_for_forced_disconnect(&app, &state).await;
                    } else {
                        log::warn!(
                            "resilience: forced disconnect cleanup skipped -- SessionState unavailable"
                        );
                    }
                })
            }),
        }
    }

    fn surface(&self, event: ResilienceEvent) {
        (self.surface)(event);
    }

    async fn cleanup(&self) {
        (self.cleanup)().await;
    }
}

fn start_room_event_watcher(
    mut events: tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>,
    generation: RoomGeneration,
    app: Option<AppHandle>,
    disconnect_effects: DisconnectEffects,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        // Bumped on every reconnect-resolving event (Reconnected/
        // Disconnected) so the delayed grace-period check can tell whether
        // "its" reconnect attempt is still the current, unresolved one.
        let reconnect_epoch = Arc::new(AtomicU64::new(0));
        // Whether a "Reconnecting…" toast was actually shown for the
        // in-flight attempt -- decides if a "Reconnected" confirmation is
        // due (silent resume => silent confirmation).
        let reconnecting_shown = Arc::new(AtomicBool::new(false));
        let share_repair_epoch = Arc::new(AtomicU64::new(0));
        let mut reconnect_started_at: Option<Instant> = None;
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("resilience: room event watcher exiting for stale room generation");
                break;
            }
            match event {
                livekit::RoomEvent::Reconnecting => {
                    let Some(app) = app.as_ref() else {
                        continue;
                    };
                    crate::remote_control::release_held_inputs_for_reconnect();
                    reconnect_started_at = Some(Instant::now());
                    let proactive = {
                        let pending = PROACTIVE_RECONNECT_AT.lock_unpoisoned();
                        matches!(*pending, Some(t) if t.elapsed() <= PROACTIVE_ATTRIBUTION_WINDOW)
                    };
                    if proactive {
                        log::info!(
                            "resilience: room reconnecting (proactive resume after network \
                             change) -- silent unless it exceeds {PROACTIVE_SILENT_GRACE:?}"
                        );
                        let epoch = reconnect_epoch.clone();
                        let this_epoch = epoch.load(Ordering::SeqCst);
                        let shown = reconnecting_shown.clone();
                        let app = app.clone();
                        let generation = generation.clone();
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(PROACTIVE_SILENT_GRACE).await;
                            if generation.is_current() && epoch.load(Ordering::SeqCst) == this_epoch
                            {
                                log::warn!(
                                    "resilience: proactive resume still unresolved after \
                                     {PROACTIVE_SILENT_GRACE:?} -- surfacing Reconnecting toast"
                                );
                                shown.store(true, Ordering::SeqCst);
                                emit(&app, ResilienceEvent::Reconnecting);
                            }
                        });
                    } else {
                        log::info!("resilience: room reconnecting (SDK-detected)");
                        reconnecting_shown.store(true, Ordering::SeqCst);
                        emit(&app, ResilienceEvent::Reconnecting);
                    }
                }
                livekit::RoomEvent::Reconnected => {
                    let Some(app) = app.as_ref() else {
                        continue;
                    };
                    reconnect_epoch.fetch_add(1, Ordering::SeqCst);
                    let repair_epoch = share_repair_epoch
                        .fetch_add(1, Ordering::SeqCst)
                        .saturating_add(1);
                    let took = reconnect_started_at.take().map(|t| t.elapsed());
                    // Consume both proactive-trigger markers whether or not a
                    // toast is due, so a later unrelated reconnect can't be
                    // mis-attributed to a stale network change.
                    let network_changed = NETWORK_CHANGE_PENDING.swap(false, Ordering::SeqCst);
                    *PROACTIVE_RECONNECT_AT.lock_unpoisoned() = None;
                    if reconnecting_shown.swap(false, Ordering::SeqCst) {
                        log::info!("resilience: room reconnected (took {took:?})");
                        let message = if network_changed {
                            // Exact DESIGN.md §9 example text.
                            "Switched network — reconnected".to_string()
                        } else {
                            "Reconnected".to_string()
                        };
                        emit(&app, ResilienceEvent::Reconnected { message });
                    } else {
                        log::info!(
                            "resilience: room reconnected silently in {took:?} (fast proactive \
                             resume -- no toast, per issue #18 policy)"
                        );
                    }
                    if let Some(state) = app.try_state::<crate::session::SessionState>() {
                        crate::remote_control::reemit_active_statuses(&app, state.inner());
                    } else {
                        log::warn!(
                            "resilience: active remote-control status resync skipped -- SessionState unavailable"
                        );
                    }
                    verify_shares_after_reconnect(
                        &app,
                        generation.clone(),
                        share_repair_epoch.clone(),
                        repair_epoch,
                    );
                }
                livekit::RoomEvent::Disconnected { reason } => {
                    reconnect_epoch.fetch_add(1, Ordering::SeqCst);
                    share_repair_epoch.fetch_add(1, Ordering::SeqCst);
                    reconnecting_shown.store(false, Ordering::SeqCst);
                    if should_surface_disconnect(reason) {
                        log::warn!("resilience: room disconnected: {reason:?}");
                        disconnect_effects.surface(ResilienceEvent::Disconnected {
                            reason: format!("{reason:?}"),
                        });
                        disconnect_effects.cleanup().await;
                    } else {
                        log::info!(
                            "resilience: room disconnected (client-initiated leave) -- no toast"
                        );
                    }
                    // The room is gone either way -- this watcher's job is done.
                    break;
                }
                _ => {}
            }
        }
        log::debug!("resilience: room event watcher exiting (room closed)");
    })
}

/// Issue #18: "Disconnected" messaging is reserved for disconnects the user
/// did NOT ask for. Our own `leave_room` -> `Room::close()` dispatches
/// `RoomEvent::Disconnected { reason: ClientInitiated }` (verified in
/// `livekit 0.7.49`'s `room/mod.rs::handle_disconnected`) -- surfacing that
/// as "Disconnected — attempting to reconnect" was one of the false-toast
/// sources this issue fixes. Every other reason (attempts exhausted, server
/// shutdown, removed, duplicate identity, ...) is a real, surprising loss
/// and is surfaced.
fn should_surface_disconnect(reason: livekit::DisconnectReason) -> bool {
    reason != livekit::DisconnectReason::ClientInitiated
}

/// Set immediately before `start_network_monitor` forces a reconnect, so
/// `start_room_event_watcher`'s `Reconnected` handler can use the more
/// specific "switched network" toast copy instead of a generic one. A plain
/// process-wide flag (not per-room state) is enough: this app connects to at
/// most one room at a time (see `session.rs`'s own single-`Option<RoomConnection>`
/// stand-in), so there's no multi-room ambiguity to resolve.
static NETWORK_CHANGE_PENDING: AtomicBool = AtomicBool::new(false);

static SYSTEM_SLEEPING: AtomicBool = AtomicBool::new(false);
/// Distinct from `SYSTEM_SLEEPING`: fires on display idle-sleep / lid-close
/// display off without a full system sleep. Sender-side capture recovery
/// (#734) must treat either as "do not permanently tear down a share".
static SCREENS_SLEEPING: AtomicBool = AtomicBool::new(false);
static LAST_SYSTEM_WAKE_AT: Mutex<Option<Instant>> = Mutex::new(None);
/// Wall-clock of the most recent screens-did-sleep / will-sleep transition.
/// Capture errors can race the flag by a few ms; a short post-sleep grace
/// window still classifies them as sleep-correlated (#734).
static LAST_SLEEP_CORRELATED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// True while the display or the whole system is asleep, or briefly after a
/// sleep transition (capture stall often arrives within the same millisecond
/// as `screensDidSleep` — see #734's petal.log incident).
pub(crate) fn capture_restart_should_wait_for_wake() -> bool {
    if SYSTEM_SLEEPING.load(Ordering::SeqCst) || SCREENS_SLEEPING.load(Ordering::SeqCst) {
        return true;
    }
    LAST_SLEEP_CORRELATED_AT
        .lock_unpoisoned()
        .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
}

/// True when a capture failure should be treated as recoverable sleep
/// interruption rather than genuine source loss (#734).
pub(crate) fn is_sleep_correlated_capture_window() -> bool {
    capture_restart_should_wait_for_wake()
}

fn mark_sleep_correlated() {
    *LAST_SLEEP_CORRELATED_AT.lock_unpoisoned() = Some(Instant::now());
}

/// When the network monitor last forced a proactive reconnect. Read by the
/// room-event watcher to decide whether an incoming `Reconnecting` is OUR
/// proactive resume (silent unless slow, per issue #18) or an independent
/// SDK-detected failure (surfaced immediately). Same single-room-at-a-time
/// reasoning as `NETWORK_CHANGE_PENDING` for why a process-wide slot is
/// enough. Cleared on `Reconnected` or on a failed trigger.
static PROACTIVE_RECONNECT_AT: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProactiveReconnectSource {
    NetworkChange,
    SystemWake,
}

impl ProactiveReconnectSource {
    fn label(self) -> &'static str {
        match self {
            Self::NetworkChange => "network change",
            Self::SystemWake => "system wake",
        }
    }
}

fn trigger_proactive_reconnect(app: &AppHandle, source: ProactiveReconnectSource) {
    let Some(session) = app.try_state::<crate::session::SessionState>() else {
        return;
    };
    let (publisher, _identity, shares) = session.shared_windows_snapshot();
    let Some(publisher) = publisher else {
        log::info!(
            "resilience: {} while not in a room -- nothing to reconnect",
            source.label()
        );
        return;
    };

    if source == ProactiveReconnectSource::NetworkChange {
        emit(app, ResilienceEvent::NetworkChanged);
        NETWORK_CHANGE_PENDING.store(true, Ordering::SeqCst);
    } else {
        NETWORK_CHANGE_PENDING.store(false, Ordering::SeqCst);
    }
    *PROACTIVE_RECONNECT_AT.lock_unpoisoned() = Some(Instant::now());

    log::info!(
        "resilience: triggering proactive reconnect after {} ({} active share(s))",
        source.label(),
        shares.len()
    );

    let room = publisher.room();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = room
            .simulate_scenario(livekit::SimulateScenario::SignalReconnect)
            .await
        {
            log::warn!(
                "resilience: failed to trigger proactive reconnect after {}: {e}",
                source.label()
            );
            if source == ProactiveReconnectSource::NetworkChange {
                NETWORK_CHANGE_PENDING.store(false, Ordering::SeqCst);
            }
            *PROACTIVE_RECONNECT_AT.lock_unpoisoned() = None;
        }
    });
}

/// Defensive post-reconnect sanity check and repair (SPEC.md §4.8:
/// "re-publish active shares automatically on reconnect"). The SDK already
/// republishes local tracks on a full reconnect, but it does not surface a
/// per-share recovery result. After `Reconnected`, each still-tracked share
/// gets one generation-gated replacement publish using the same
/// publish-new/swap/unpublish-old path as quality and resize changes.
fn verify_shares_after_reconnect(
    app: &AppHandle,
    generation: RoomGeneration,
    share_repair_epoch: Arc<AtomicU64>,
    repair_epoch: u64,
) {
    #[cfg(target_os = "macos")]
    {
        if reconnect_share_repair_disabled() {
            log::warn!(
                "resilience: post-reconnect share publication repair disabled by PETAL_DISABLE_RECONNECT_SHARE_REPAIR"
            );
            return;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(POST_RECONNECT_SHARE_REPAIR_DELAY).await;
            let repair_epoch_current = post_reconnect_share_repair_epoch_is_current(
                share_repair_epoch.load(Ordering::SeqCst),
                repair_epoch,
            );
            if !post_reconnect_share_repair_gate(
                generation.is_current(),
                repair_epoch_current,
                true,
            ) {
                log::info!(
                    "resilience: skipping stale post-reconnect share publication repair epoch {repair_epoch}"
                );
                return;
            }
            let Some(state) = app.try_state::<crate::session::SessionState>() else {
                return;
            };
            let (room, _identity, shares) = state.shared_windows_snapshot();
            if !post_reconnect_share_repair_gate(true, true, room.is_some()) {
                return;
            }
            log::info!(
                "resilience: post-reconnect check -- {} share(s) still tracked in session state \
                 (their published tracks are the SDK's own `LocalVideoTrack` objects, republished \
                 in place by `handle_restarted` if this was a full reconnect -- see module doc \
                 comment)",
                shares.len()
            );
            let reconnect_guard = crate::session::ReconnectRepairGuard::new(
                generation,
                share_repair_epoch,
                repair_epoch,
            );
            // #713: the SDK's own `handle_restarted` republish is per-track,
            // not per-share -- the local mic/camera tracks can time out and
            // get dropped exactly the same way a window share's publication
            // can, but were never covered by this repair pass before. Same
            // `reconnect_guard` (generation-gated), run before the window
            // shares below since neither depends on the other.
            crate::session::repair_mic_publication_after_reconnect(
                &app,
                state.inner(),
                &reconnect_guard,
            )
            .await;
            crate::session::repair_camera_publication_after_reconnect(
                &app,
                state.inner(),
                &reconnect_guard,
            )
            .await;
            crate::session::repair_active_share_publications_after_reconnect(
                &app,
                state.inner(),
                reconnect_guard,
            )
            .await;
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, generation, share_repair_epoch, repair_epoch);
    }
}

/// Emergency rollback for #298's app-level publication replacement. LiveKit's
/// own reconnect/resume behavior remains active; only Petal's delayed repair is
/// skipped. Default is enabled, and only the exact value `1` disables it.
/// Debug-only. A shipped build must not let the environment switch off
/// publication repair: a user who does so silently breaks their own reconnect
/// and reports it as a product bug against a configuration we never test.
fn reconnect_share_repair_disabled() -> bool {
    #[cfg(debug_assertions)]
    {
        reconnect_share_repair_disabled_value(
            std::env::var("PETAL_DISABLE_RECONNECT_SHARE_REPAIR")
                .ok()
                .as_deref(),
        )
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

fn reconnect_share_repair_disabled_value(value: Option<&str>) -> bool {
    value == Some("1")
}

fn post_reconnect_share_repair_epoch_is_current(current_epoch: u64, repair_epoch: u64) -> bool {
    current_epoch == repair_epoch
}

/// The delayed reconnect task may only touch active shares while it still
/// belongs to the current room, reconnect epoch, and joined session. Keeping
/// this gate explicit gives the event watcher a small, testable boundary
/// between the 250 ms settle delay and media publication repair (#298).
fn post_reconnect_share_repair_gate(
    room_generation_current: bool,
    repair_epoch_current: bool,
    room_joined: bool,
) -> bool {
    room_generation_current && repair_epoch_current && room_joined
}

fn emit(app: &AppHandle, event: ResilienceEvent) {
    // Global `emit`, NOT `emit_to(MAIN_WINDOW_LABEL, ...)`: in Tauri 2 an
    // `emit_to("<label>")` target only matches listeners registered with a
    // label-specific EventTarget — the frontend's plain `listen()` registers
    // `EventTarget::Any`, which `emit_to` never delivers to. ToastHost's
    // reconnection toasts were silently dead because of this (found while
    // root-causing issue #22 — see hover_tab.rs's emit-site comment for
    // the full Tauri-source walkthrough). Same fix as `presence-update`/
    // `room-left`: broadcast globally.
    let _ = tauri::Emitter::emit(app, "resilience-event", event);
}

pub(crate) fn emit_share_publication_repair_recovering(app: &AppHandle, window_id: u32) {
    emit(
        app,
        ResilienceEvent::SharePublicationRepairRecovering { window_id },
    );
}

pub(crate) fn emit_share_publication_repair_cancelled(app: &AppHandle, window_id: u32) {
    emit(
        app,
        ResilienceEvent::SharePublicationRepairCancelled { window_id },
    );
}

pub(crate) fn emit_share_publication_repair_restored(app: &AppHandle, window_id: u32) {
    emit(
        app,
        ResilienceEvent::SharePublicationRepairRestored { window_id },
    );
}

pub(crate) fn emit_share_publication_repair_failed(
    app: &AppHandle,
    window_id: u32,
    message: String,
) {
    emit(
        app,
        ResilienceEvent::SharePublicationRepairFailed { window_id, message },
    );
}

/// #713: still-failing mic/camera republish after the one bounded repair
/// attempt (`session::repair_mic_publication_after_reconnect` /
/// `session::repair_camera_publication_after_reconnect`) must surface to the
/// user rather than silently leaving the track dropped -- same
/// `{ message }`-only shape as `MicDeviceFailed`/`SpeakerDeviceFailed` above
/// (not the window-share repair's windowed variants, since there is at most
/// one mic and one camera track per session, nothing to key by id).
pub(crate) fn emit_mic_publication_repair_failed(app: &AppHandle, message: String) {
    emit(app, ResilienceEvent::MicPublicationRepairFailed { message });
}

pub(crate) fn emit_camera_publication_repair_failed(app: &AppHandle, message: String) {
    emit(
        app,
        ResilienceEvent::CameraPublicationRepairFailed { message },
    );
}

/// SPEC.md §4.8: "`NWPathMonitor` -> on interface change, trigger ICE
/// restart." See module doc comment for why this uses `SCDynamicStore`
/// (pure C CoreFoundation/SystemConfiguration FFI, no Objective-C/Swift
/// surface -- deliberately avoids reintroducing the `-ObjC` duplicate-Swift-
/// symbol linker fight this codebase already had once, see
/// `transport/mod.rs`) watching `State:/Network/Global/IPv4` rather than a
/// `Network.framework`/`NWPathMonitor` FFI binding -- same signal (primary
/// network service changed), lower linking risk.
///
/// Runs its own dedicated background OS thread with its own `CFRunLoop`
/// (SCDynamicStore's callback needs an actual running run loop to pump
/// notifications on -- confirmed via the crate's own doc comment on
/// `schedule_with_runloop`), matching this codebase's existing house style
/// of a dedicated background thread/task per independent watcher
/// (`telepointer.rs`'s sender loop, `hover_tab.rs`'s poll loop) rather than
/// piggybacking on the main AppKit run loop menubar.rs already uses for
/// `NSNotificationCenter` -- `SCDynamicStore` doesn't need to be on the main
/// thread the way AppKit APIs do.
/// Issue #18 lifecycle fix: a SINGLE app-level watcher for the whole
/// process (started lazily on the first room join, guarded by
/// `NETWORK_MONITOR_STARTED`), which looks the CURRENT room up via
/// `SessionState` at fire time instead of capturing an `Arc<Room>` per
/// join. The old shape spawned one new watcher thread per `join_room` with
/// zero teardown -- after leave→rejoin cycles, N watchers were alive,
/// several holding rooms that had already been left (log-proven: duplicate
/// "interface changed" lines in the same millisecond + a stale watcher's
/// "engine is closed" reconnect failure).
fn ensure_network_monitor(app: &AppHandle) {
    static NETWORK_MONITOR_STARTED: AtomicBool = AtomicBool::new(false);
    if NETWORK_MONITOR_STARTED.swap(true, Ordering::SeqCst) {
        log::debug!(
            "resilience: network-change monitor already running (single app-level watcher)"
        );
        return;
    }
    start_network_monitor(app.clone());
}

fn ensure_display_monitor(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        static DISPLAY_MONITOR_STARTED: AtomicBool = AtomicBool::new(false);
        if DISPLAY_MONITOR_STARTED.swap(true, Ordering::SeqCst) {
            log::debug!(
                "resilience: display reconfiguration monitor already running (single app-level watcher)"
            );
            return;
        }
        start_display_monitor(app.clone());
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

fn ensure_sleep_wake_monitor(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        static SLEEP_WAKE_MONITOR_STARTED: AtomicBool = AtomicBool::new(false);
        if SLEEP_WAKE_MONITOR_STARTED.swap(true, Ordering::SeqCst) {
            log::debug!(
                "resilience: sleep/wake monitor already running (single app-level watcher)"
            );
            return;
        }
        let app = app.clone();
        crate::platform::on_main(
            &app.clone(),
            "resilience: register sleep/wake monitor",
            move || {
                register_sleep_wake_observers(app);
            },
        );
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

#[cfg(target_os = "macos")]
fn register_sleep_wake_observers(app: AppHandle) {
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidSleepNotification,
        NSWorkspaceScreensDidWakeNotification, NSWorkspaceWillSleepNotification,
    };
    use objc2_foundation::NSNotification;

    let center = NSWorkspace::sharedWorkspace().notificationCenter();

    let will_sleep = block2::RcBlock::new(|_note: std::ptr::NonNull<NSNotification>| {
        SYSTEM_SLEEPING.store(true, Ordering::SeqCst);
        mark_sleep_correlated();
        log::info!("resilience: system will sleep -- suppressing wake-sensitive recovery noise");
    });
    let will_sleep_observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceWillSleepNotification),
            None,
            None,
            &will_sleep,
        )
    };

    let wake_app = app.clone();
    let did_wake = block2::RcBlock::new(move |_note: std::ptr::NonNull<NSNotification>| {
        let app = wake_app.clone();
        handle_system_wake(app);
    });
    let did_wake_observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidWakeNotification),
            None,
            None,
            &did_wake,
        )
    };

    // #259/#264 display-sleep defensive fix. `NSWorkspaceScreensDidSleep/
    // WakeNotification` are DISTINCT from the whole-system `WillSleep`/
    // `DidWake` pair above -- they fire whenever the DISPLAY specifically
    // goes to/comes back from sleep, which includes the idle-timeout case
    // (no full system sleep at all) that is the actual crash scenario this
    // fixes: a receiver's display went to sleep while Petal kept driving
    // ~30fps `AVSampleBufferDisplayLayer` enqueues to it, and the OS watchdog
    // killed WindowServer. `compositor::set_display_enqueue_paused` is a
    // pure, idempotent, non-AppKit call (see its doc comment), so it's safe
    // to invoke directly from this block, no `platform::on_main` hop needed
    // for the pause/resume side effect itself -- only registering these
    // observers needs the main thread, which `ensure_sleep_wake_monitor`'s
    // caller already provides via `platform::on_main`.
    let screens_did_sleep = block2::RcBlock::new(|_note: std::ptr::NonNull<NSNotification>| {
        SCREENS_SLEEPING.store(true, Ordering::SeqCst);
        mark_sleep_correlated();
        log::info!("resilience: screens did sleep -- pausing compositor display enqueue");
        crate::compositor::set_display_enqueue_paused(true);
        crate::analytics::device_changed(
            crate::analytics::DeviceKind::Display,
            crate::analytics::DeviceChange::Sleep,
        );
    });
    let screens_did_sleep_observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceScreensDidSleepNotification),
            None,
            None,
            &screens_did_sleep,
        )
    };

    let screens_did_wake = block2::RcBlock::new(|_note: std::ptr::NonNull<NSNotification>| {
        SCREENS_SLEEPING.store(false, Ordering::SeqCst);
        log::info!("resilience: screens did wake -- resuming compositor display enqueue");
        crate::compositor::set_display_enqueue_paused(false);
        crate::analytics::device_changed(
            crate::analytics::DeviceKind::Display,
            crate::analytics::DeviceChange::Wake,
        );
    });
    let screens_did_wake_observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceScreensDidWakeNotification),
            None,
            None,
            &screens_did_wake,
        )
    };

    // App-lifetime observer tokens, matching the permanent network/display
    // monitors in this module.
    std::mem::forget(will_sleep_observer);
    std::mem::forget(did_wake_observer);
    std::mem::forget(screens_did_sleep_observer);
    std::mem::forget(screens_did_wake_observer);
    log::info!(
        "resilience: sleep/wake monitor started (NSWorkspace system + screen sleep/wake notifications)"
    );
}

fn accept_system_wake(now: Instant) -> bool {
    let mut last = LAST_SYSTEM_WAKE_AT.lock_unpoisoned();
    if last.is_some_and(|t| now.duration_since(t) < SYSTEM_WAKE_DEBOUNCE) {
        return false;
    }
    *last = Some(now);
    true
}

fn handle_system_wake(app: AppHandle) {
    SYSTEM_SLEEPING.store(false, Ordering::SeqCst);
    if !accept_system_wake(Instant::now()) {
        log::debug!("resilience: duplicate system wake within debounce window -- ignoring");
        return;
    }

    log::info!(
        "resilience: system did wake -- triggering proactive reconnect and active share refresh"
    );
    trigger_proactive_reconnect(&app, ProactiveReconnectSource::SystemWake);
    tauri::async_runtime::spawn(async move {
        let Some(state) = app.try_state::<crate::session::SessionState>() else {
            return;
        };
        crate::session::restart_active_shares_after_wake(&app, state.inner()).await;
    });
}

#[cfg(target_os = "macos")]
fn start_display_monitor(app: AppHandle) {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGDisplayRegisterReconfigurationCallback(
            callback: extern "C" fn(u32, u32, *mut c_void),
            user_info: *mut c_void,
        ) -> i32;
    }

    const K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG: u32 = 1 << 0;

    extern "C" fn callback(_display: u32, flags: u32, user_info: *mut c_void) {
        if flags & K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG != 0 || user_info.is_null() {
            return;
        }
        // SAFETY: `start_display_monitor` intentionally leaks one boxed
        // `AppHandle` for the process lifetime after successful registration.
        let app = unsafe { &*(user_info as *const AppHandle) }.clone();
        tauri::async_runtime::spawn(async move {
            let context = AppDisplayReconfigurationContext { app };
            handle_display_reconfiguration(&context, &CgDisplayFrameLookup).await;
        });
    }

    let user_info = Box::into_raw(Box::new(app.clone())) as *mut c_void;
    let status = unsafe { CGDisplayRegisterReconfigurationCallback(callback, user_info) };
    if status == 0 {
        log::info!("resilience: display reconfiguration monitor started");
    } else {
        log::warn!(
            "resilience: CGDisplayRegisterReconfigurationCallback failed with status {status}"
        );
        // SAFETY: registration failed, so CoreGraphics will never receive
        // this pointer; reclaim it.
        unsafe {
            drop(Box::from_raw(user_info as *mut AppHandle));
        }
    }
}

#[cfg(target_os = "macos")]
trait DisplayReconfigurationContext {
    fn active_share_sources(&self) -> Vec<(u32, crate::transport::publisher::SharedSourceKind)>;
    fn update_share_frames(&self, fresh: &[(u32, crate::hover_tab::WindowFrame)]);
    fn stop_missing_share(&self, source_id: u32) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

#[cfg(target_os = "macos")]
struct AppDisplayReconfigurationContext {
    app: AppHandle,
}

#[cfg(target_os = "macos")]
impl DisplayReconfigurationContext for AppDisplayReconfigurationContext {
    fn active_share_sources(&self) -> Vec<(u32, crate::transport::publisher::SharedSourceKind)> {
        let Some(state) = self.app.try_state::<crate::session::SessionState>() else {
            return Vec::new();
        };
        state.active_share_sources()
    }

    fn update_share_frames(&self, fresh: &[(u32, crate::hover_tab::WindowFrame)]) {
        if let Some(state) = self.app.try_state::<crate::session::SessionState>() {
            state.update_share_frames(fresh);
        }
    }

    fn stop_missing_share(&self, source_id: u32) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            crate::hover_tab::clear_share_state_for_window(&self.app, source_id);
            crate::remote_control::revoke_window(&self.app, source_id, "shared source disappeared");
            let Some(state) = self.app.try_state::<crate::session::SessionState>() else {
                return;
            };
            if let Err(e) = crate::session::stop_share_explained(
                &self.app,
                &state,
                source_id,
                crate::session::StopShareAnalytics::WindowGone,
            )
            .await
            {
                log::warn!(
                    "resilience: failed to stop vanished shared source {source_id} after display reconfiguration: {e}"
                );
            }
        })
    }
}

#[cfg(target_os = "macos")]
trait DisplayFrameLookup {
    fn frame_for_display_id(&self, display_id: u32) -> Option<crate::hover_tab::WindowFrame>;
}

#[cfg(target_os = "macos")]
struct CgDisplayFrameLookup;

#[cfg(target_os = "macos")]
impl DisplayFrameLookup for CgDisplayFrameLookup {
    fn frame_for_display_id(&self, display_id: u32) -> Option<crate::hover_tab::WindowFrame> {
        let display = core_graphics::display::CGDisplay::new(display_id);
        if !display.is_online() {
            return None;
        }
        let bounds = display.bounds();
        Some(crate::hover_tab::WindowFrame {
            x: bounds.origin.x.round() as i32,
            y: bounds.origin.y.round() as i32,
            width: bounds.size.width.round().max(1.0) as i32,
            height: bounds.size.height.round().max(1.0) as i32,
        })
    }
}

#[cfg(target_os = "macos")]
async fn handle_display_reconfiguration(
    context: &impl DisplayReconfigurationContext,
    displays: &impl DisplayFrameLookup,
) {
    crate::analytics::device_changed(
        crate::analytics::DeviceKind::Display,
        crate::analytics::DeviceChange::Reconfigured,
    );
    let share_sources = context.active_share_sources();
    if share_sources.is_empty() {
        return;
    }
    log::info!(
        "resilience: display reconfiguration detected -- checking {} active share(s)",
        share_sources.len()
    );

    let mut fresh = Vec::new();
    let mut missing = Vec::new();
    for (source_id, source_kind) in share_sources {
        let frame = match source_kind {
            crate::transport::publisher::SharedSourceKind::Window => {
                // #744: route the fresh single-id read through the registry.
                match crate::window_registry::global() {
                    Some(reg) => reg.frame_fresh(source_id),
                    None => crate::platform::cg::frame_for_window_id(source_id),
                }
            }
            crate::transport::publisher::SharedSourceKind::DisplayRegion => {
                crate::region_window::resolve(source_id).map(|source| {
                    crate::hover_tab::WindowFrame {
                        x: source.frame.x.round() as i32,
                        y: source.frame.y.round() as i32,
                        width: source.frame.width.round().max(1.0) as i32,
                        height: source.frame.height.round().max(1.0) as i32,
                    }
                })
            }
            crate::transport::publisher::SharedSourceKind::Display => {
                // #750: display source ids are tagged session ids, never
                // CGWindowIDs. Decode before crossing into CoreGraphics.
                let display_id = crate::window_source::display_id_from_source_id(source_id);
                displays.frame_for_display_id(display_id)
            }
        };
        match frame {
            Some(frame) => fresh.push((source_id, frame)),
            None => missing.push(source_id),
        }
    }
    context.update_share_frames(&fresh);

    for source_id in missing {
        log::warn!(
            "resilience: shared source {source_id} disappeared after display reconfiguration; stopping share"
        );
        context.stop_missing_share(source_id).await;
    }
}

/// The exact well-known SystemConfiguration key that changes when the
/// OS's primary network service/interface changes (Wi-Fi <-> Ethernet,
/// VPN connect/disconnect, etc.) -- the standard signal apps use for
/// this (same key `SCDynamicStoreCopyValue` examples in Apple's own
/// Networking docs reference), not something invented for this task.
/// NOTE (issue #18): notifications on this key also fire for DHCP lease
/// renewals / route-table churn that do NOT change the primary interface --
/// which is why the callback compares the key's actual
/// `PrimaryInterface`/`Router` values against the previous snapshot and
/// ignores no-op notifications, rather than treating every notification as
/// a network change.
const PRIMARY_INTERFACE_KEY: &str = "State:/Network/Global/IPv4";

/// The two fields of `State:/Network/Global/IPv4` that identify the actual
/// primary network path. If neither changed, the notification was routine
/// network-stack noise (DHCP renew, route metadata) and must not trigger a
/// reconnect or any UI (issue #18 root cause 2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PrimaryNetworkSnapshot {
    interface: Option<String>,
    router: Option<String>,
}

impl PrimaryNetworkSnapshot {
    fn describe(&self) -> String {
        format!(
            "interface={} router={}",
            self.interface.as_deref().unwrap_or("<none>"),
            self.router.as_deref().unwrap_or("<none>")
        )
    }
}

/// Read the current `PrimaryInterface`/`Router` values out of the dynamic
/// store (via `SCDynamicStoreCopyValue`, per the issue's implementation
/// sketch). A missing key (no network at all) reads as the all-`None`
/// snapshot -- losing connectivity entirely IS a real change vs. a live
/// interface, and two consecutive no-network reads compare equal.
fn read_primary_network_snapshot(
    store: &system_configuration::dynamic_store::SCDynamicStore,
) -> PrimaryNetworkSnapshot {
    use system_configuration::core_foundation::base::{CFType, TCFType};
    use system_configuration::core_foundation::dictionary::CFDictionary;
    use system_configuration::core_foundation::string::CFString;

    let mut snapshot = PrimaryNetworkSnapshot::default();
    let Some(plist) = store.get(PRIMARY_INTERFACE_KEY) else {
        return snapshot;
    };
    let Some(dict) = plist.downcast_into::<CFDictionary>() else {
        return snapshot;
    };
    let read_string = |name: &str| -> Option<String> {
        let key = CFString::new(name);
        let value = dict.find(key.as_CFTypeRef() as *const std::ffi::c_void)?;
        // The untyped dictionary hands back a raw `*const c_void`; wrap it
        // as a CFType (get-rule: the dictionary still owns it) and downcast,
        // so a non-string value (malformed store entry) reads as None
        // instead of being blindly reinterpreted.
        let cf = unsafe { CFType::wrap_under_get_rule(*value as _) };
        cf.downcast::<CFString>().map(|s| s.to_string())
    };
    snapshot.interface = read_string("PrimaryInterface");
    snapshot.router = read_string("Router");
    snapshot
}

fn start_network_monitor(app: AppHandle) {
    std::thread::spawn(move || {
        use system_configuration::core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop};
        use system_configuration::core_foundation::string::CFString;
        use system_configuration::dynamic_store::{
            SCDynamicStoreBuilder, SCDynamicStoreCallBackContext,
        };

        // Seed the previous-value snapshot from the CURRENT store state (via
        // a short-lived read-only store handle, since the callback context
        // has to be built before the watching store exists) so the very
        // first notification is compared against reality rather than being
        // unconditionally treated as a change.
        let initial = SCDynamicStoreBuilder::new("petal-network-monitor-seed")
            .build()
            .map(|s| read_primary_network_snapshot(&s))
            .unwrap_or_default();
        log::info!(
            "resilience: network monitor initial state: {}",
            initial.describe()
        );

        let context = SCDynamicStoreCallBackContext {
            callout: on_network_store_change,
            info: NetworkWatchState {
                app: app.clone(),
                prev: initial,
                last_change_at: None,
            },
        };

        let Some(store) = SCDynamicStoreBuilder::new("petal-network-monitor")
            .callback_context(context)
            .build()
        else {
            log::warn!(
                "resilience: failed to create SCDynamicStore -- network-change detection disabled \
                 for this session (reconnect still works via the SDK's own WebRTC-failure-driven \
                 resume, just without the faster proactive trigger)"
            );
            return;
        };

        let keys =
            system_configuration::core_foundation::array::CFArray::from_CFTypes(&[CFString::new(
                PRIMARY_INTERFACE_KEY,
            )]);
        if !store.set_notification_keys(
            &keys,
            &system_configuration::core_foundation::array::CFArray::<CFString>::from_CFTypes(&[]),
        ) {
            log::warn!("resilience: SCDynamicStore::set_notification_keys failed");
            return;
        }

        let Some(run_loop_source) = store.create_run_loop_source() else {
            log::warn!("resilience: SCDynamicStore::create_run_loop_source failed");
            return;
        };

        let run_loop = CFRunLoop::get_current();
        run_loop.add_source(&run_loop_source, unsafe { kCFRunLoopDefaultMode });
        log::info!(
            "resilience: network-change monitor watching '{PRIMARY_INTERFACE_KEY}' \
             (SCDynamicStore, dedicated thread)"
        );
        CFRunLoop::run_current();
    });
}

/// State owned by the `SCDynamicStore` callback. No `Arc<Room>` (issue #18):
/// the current room is looked up through `SessionState` at fire time, so a
/// left room can never be reconnect-poked by a stale watcher. `prev` +
/// `last_change_at` back the trigger discrimination (value diff + debounce).
struct NetworkWatchState {
    app: AppHandle,
    prev: PrimaryNetworkSnapshot,
    last_change_at: Option<Instant>,
}

fn on_network_store_change(
    store: system_configuration::dynamic_store::SCDynamicStore,
    changed_keys: system_configuration::core_foundation::array::CFArray<
        system_configuration::core_foundation::string::CFString,
    >,
    state: &mut NetworkWatchState,
) {
    // Trigger discrimination (issue #18 root cause 2): read the key's actual
    // value and only treat a REAL primary-interface/router change as a
    // network change -- notifications also fire for DHCP lease renewals,
    // route-table updates, and VPN metadata churn that change the dict
    // without changing the primary path.
    let next = read_primary_network_snapshot(&store);
    if next == state.prev {
        log::debug!(
            "resilience: SCDynamicStore notification with unchanged primary network \
             ({} key(s) notified, still {}) -- ignoring (routine network-stack noise)",
            changed_keys.len(),
            next.describe()
        );
        return;
    }
    let prev = std::mem::replace(&mut state.prev, next.clone());

    // Debounce: one physical event (interface flip) produces a burst of
    // notifications, sometimes with intermediate values (e.g. interface up
    // before the router is assigned). The first real change already
    // triggered the reconnect; coalesce the rest of the burst.
    if state
        .last_change_at
        .is_some_and(|t| t.elapsed() < NETWORK_CHANGE_DEBOUNCE)
    {
        log::info!(
            "resilience: primary network changed again within the {NETWORK_CHANGE_DEBOUNCE:?} \
             debounce window ({} -> {}) -- snapshot updated, reconnect already in flight",
            prev.describe(),
            next.describe()
        );
        return;
    }
    state.last_change_at = Some(Instant::now());

    log::info!(
        "resilience: primary network interface changed: {} -> {} -- triggering a proactive \
         reconnect rather than waiting for WebRTC's own failure detection",
        prev.describe(),
        next.describe()
    );

    // Look the CURRENT room up at fire time (issue #18 lifecycle fix) --
    // when no room is joined there is nothing to reconnect and nothing to
    // toast; a real change is still logged above for diagnosability.
    let Some(session) = state.app.try_state::<crate::session::SessionState>() else {
        return;
    };
    let (publisher, _identity, _shares) = session.shared_windows_snapshot();
    let Some(publisher) = publisher else {
        log::info!("resilience: network changed while not in a room -- nothing to reconnect");
        return;
    };

    emit(&state.app, ResilienceEvent::NetworkChanged);
    NETWORK_CHANGE_PENDING.store(true, Ordering::SeqCst);
    *PROACTIVE_RECONNECT_AT.lock_unpoisoned() = Some(Instant::now());

    let room = publisher.room();
    tauri::async_runtime::spawn(async move {
        // `SimulateScenario::SignalReconnect` (a real, non-test-gated public
        // `Room` method -- see module doc comment) closes the signalling
        // channel locally, which the engine's own resume/backoff machinery
        // then picks up immediately instead of waiting on WebRTC's own
        // (much slower) ICE-failure detection. This is the SPEC.md §4.8
        // "trigger ICE restart... don't tear down the PeerConnection"
        // requirement: a resume reuses the existing PeerConnection (ICE-
        // restarting it), it does not recreate one.
        if let Err(e) = room
            .simulate_scenario(livekit::SimulateScenario::SignalReconnect)
            .await
        {
            log::warn!(
                "resilience: failed to trigger proactive reconnect after network change: {e}"
            );
            NETWORK_CHANGE_PENDING.store(false, Ordering::SeqCst);
            *PROACTIVE_RECONNECT_AT.lock_unpoisoned() = None;
        }
    });
}

/// The per-tick seam of the audio device watch. The spawned poll implements this
/// against the live session and `AppHandle`; tests drive the SAME tick function
/// with a fake, so the wiring is exercised and not just the pure arithmetic
/// (CLAUDE.md: green unit tests on isolated helpers are not sufficient evidence).
trait AudioDeviceWatchContext {
    fn refresh_recording(&self) -> Option<RecordingDeviceRefresh>;
    fn refresh_playout(&self) -> Option<PlayoutDeviceRefresh>;
    fn emit_event(&self, event: ResilienceEvent);
    fn clear_playout_preference(&self);
    fn capture_playout_diagnostic(&self, transition: crate::logging::PlayoutTransitionTag);
}

struct AppAudioDeviceWatchContext {
    app: AppHandle,
    mic: Option<MicWatchHandle>,
    speaker: Option<SpeakerWatchHandle>,
}

impl AudioDeviceWatchContext for AppAudioDeviceWatchContext {
    fn refresh_recording(&self) -> Option<RecordingDeviceRefresh> {
        self.mic
            .as_ref()
            .and_then(MicWatchHandle::current)
            .map(|track| track.refresh_default_recording_device())
    }

    fn refresh_playout(&self) -> Option<PlayoutDeviceRefresh> {
        self.speaker
            .as_ref()
            .and_then(SpeakerWatchHandle::current)
            .map(|playout| playout.refresh_default_playout_device())
    }

    fn emit_event(&self, event: ResilienceEvent) {
        emit(&self.app, event);
    }

    fn clear_playout_preference(&self) {
        self.app
            .state::<crate::transport::audio::AudioDevicePreferences>()
            .set_playout_device(String::new());
    }

    fn capture_playout_diagnostic(&self, transition: crate::logging::PlayoutTransitionTag) {
        crate::logging::capture_sentry_diagnostic(
            crate::logging::SentryDiagnosticEvent::PlayoutDeviceRepointed(
                crate::logging::PlayoutDeviceDiagnostic {
                    role: crate::logging::DiagnosticRole::Both,
                    transition,
                },
            ),
        );
    }
}

#[derive(Default)]
struct AudioWatchReporting {
    mic_failure_reported: bool,
    speaker_failure_reported: bool,
}

fn audio_device_watch_tick(ctx: &dyn AudioDeviceWatchContext, reporting: &mut AudioWatchReporting) {
    match ctx.refresh_recording() {
        Some(RecordingDeviceRefresh::Unchanged) => {}
        Some(RecordingDeviceRefresh::Switched(device_name)) => {
            reporting.mic_failure_reported = false;
            crate::analytics::device_changed(
                crate::analytics::DeviceKind::Mic,
                crate::analytics::DeviceChange::Switched,
            );
            ctx.emit_event(ResilienceEvent::MicDeviceChanged {
                device_name,
                using_default: None,
            });
        }
        Some(RecordingDeviceRefresh::Failed(message)) => {
            if !reporting.mic_failure_reported {
                reporting.mic_failure_reported = true;
                crate::analytics::device_changed(
                    crate::analytics::DeviceKind::Mic,
                    crate::analytics::DeviceChange::Failed,
                );
                ctx.emit_event(ResilienceEvent::MicDeviceFailed { message });
            }
        }
        None => reporting.mic_failure_reported = false,
    }

    match ctx.refresh_playout() {
        Some(PlayoutDeviceRefresh::Unchanged) => {
            reporting.speaker_failure_reported = false;
        }
        Some(PlayoutDeviceRefresh::Switched(device_name)) => {
            reporting.speaker_failure_reported = false;
            ctx.clear_playout_preference();
            log::warn!(
                "audio: default playout device changed mid-call -> re-pointed playout to '{device_name}' (#867)"
            );
            ctx.emit_event(ResilienceEvent::SpeakerDeviceChanged {
                device_name,
                using_default: Some(true),
            });
            ctx.capture_playout_diagnostic(crate::logging::PlayoutTransitionTag::Repointed);
        }
        Some(PlayoutDeviceRefresh::Failed(message)) => {
            if !reporting.speaker_failure_reported {
                reporting.speaker_failure_reported = true;
                log::warn!("audio: default playout device unavailable mid-call: {message} (#867)");
                ctx.emit_event(ResilienceEvent::SpeakerDeviceFailed { message });
                ctx.capture_playout_diagnostic(crate::logging::PlayoutTransitionTag::Unavailable);
            }
        }
        None => reporting.speaker_failure_reported = false,
    }
}

/// SPEC.md §4.8 mic and speaker hot-swap poll -- see the module doc comment
/// for why this is a poll rather than a push callback. A SINGLE app-level task
/// re-reads both CURRENT session handles on every tick, so it no-ops between
/// rooms and picks up the next room automatically. Either side starts it;
/// microphone publication failure must not disable default-speaker recovery.
fn ensure_audio_device_watch(
    app: AppHandle,
    mic: Option<MicWatchHandle>,
    speaker: Option<SpeakerWatchHandle>,
) {
    static AUDIO_WATCH_STARTED: AtomicBool = AtomicBool::new(false);
    if AUDIO_WATCH_STARTED.swap(true, Ordering::SeqCst) {
        log::debug!("resilience: audio device watch already running (single app-level poll)");
        return;
    }
    tauri::async_runtime::spawn(async move {
        let context = AppAudioDeviceWatchContext { app, mic, speaker };
        let mut ticker = tokio::time::interval(AUDIO_DEVICE_POLL_INTERVAL);
        let mut reporting = AudioWatchReporting::default();
        loop {
            ticker.tick().await;
            audio_device_watch_tick(&context, &mut reporting);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeAudioDeviceWatchContext {
        recording: Mutex<VecDeque<Option<RecordingDeviceRefresh>>>,
        playout: Mutex<VecDeque<Option<PlayoutDeviceRefresh>>>,
        emitted: Mutex<Vec<ResilienceEvent>>,
        preference_clears: Mutex<Vec<()>>,
        diagnostics: Mutex<Vec<crate::logging::PlayoutTransitionTag>>,
    }

    impl FakeAudioDeviceWatchContext {
        fn new(
            recording: impl IntoIterator<Item = Option<RecordingDeviceRefresh>>,
            playout: impl IntoIterator<Item = Option<PlayoutDeviceRefresh>>,
        ) -> Self {
            Self {
                recording: Mutex::new(recording.into_iter().collect()),
                playout: Mutex::new(playout.into_iter().collect()),
                emitted: Mutex::new(Vec::new()),
                preference_clears: Mutex::new(Vec::new()),
                diagnostics: Mutex::new(Vec::new()),
            }
        }
    }

    impl AudioDeviceWatchContext for FakeAudioDeviceWatchContext {
        fn refresh_recording(&self) -> Option<RecordingDeviceRefresh> {
            self.recording.lock_unpoisoned().pop_front().flatten()
        }

        fn refresh_playout(&self) -> Option<PlayoutDeviceRefresh> {
            self.playout.lock_unpoisoned().pop_front().flatten()
        }

        fn emit_event(&self, event: ResilienceEvent) {
            self.emitted.lock_unpoisoned().push(event);
        }

        fn clear_playout_preference(&self) {
            self.preference_clears.lock_unpoisoned().push(());
        }

        fn capture_playout_diagnostic(&self, transition: crate::logging::PlayoutTransitionTag) {
            self.diagnostics.lock_unpoisoned().push(transition);
        }
    }

    #[test]
    fn default_playout_device_change_repoints_playout_mid_call() {
        let context = FakeAudioDeviceWatchContext::new(
            [None],
            [Some(PlayoutDeviceRefresh::Switched(
                "MacBook Pro Speakers".to_string(),
            ))],
        );
        let mut reporting = AudioWatchReporting::default();
        audio_device_watch_tick(&context, &mut reporting);

        assert!(matches!(
            context.emitted.lock_unpoisoned().as_slice(),
            [ResilienceEvent::SpeakerDeviceChanged {
                device_name,
                using_default: Some(true),
            }] if device_name == "MacBook Pro Speakers"
        ));
        assert_eq!(context.preference_clears.lock_unpoisoned().len(), 1);
        assert_eq!(
            context.diagnostics.lock_unpoisoned().as_slice(),
            [crate::logging::PlayoutTransitionTag::Repointed]
        );
    }

    #[test]
    fn steady_state_playout_ticks_emit_no_diagnostic() {
        let context = FakeAudioDeviceWatchContext::new(
            [None, None, None],
            [
                Some(PlayoutDeviceRefresh::Unchanged),
                Some(PlayoutDeviceRefresh::Unchanged),
                Some(PlayoutDeviceRefresh::Unchanged),
            ],
        );
        let mut reporting = AudioWatchReporting::default();
        for _ in 0..3 {
            audio_device_watch_tick(&context, &mut reporting);
        }

        assert!(context.emitted.lock_unpoisoned().is_empty());
        assert!(context.preference_clears.lock_unpoisoned().is_empty());
        assert!(context.diagnostics.lock_unpoisoned().is_empty());
    }

    #[test]
    fn one_diagnostic_per_playout_failure_episode_not_per_poll() {
        let failure = || {
            Some(PlayoutDeviceRefresh::Failed(
                "Speaker disconnected — check output device".to_string(),
            ))
        };
        let context = FakeAudioDeviceWatchContext::new(
            [None, None, None, None, None, None],
            [
                failure(),
                failure(),
                failure(),
                failure(),
                Some(PlayoutDeviceRefresh::Unchanged),
                failure(),
            ],
        );
        let mut reporting = AudioWatchReporting::default();
        for _ in 0..6 {
            audio_device_watch_tick(&context, &mut reporting);
        }

        let emitted = context.emitted.lock_unpoisoned();
        assert_eq!(emitted.len(), 2);
        assert!(emitted.iter().all(|event| matches!(
            event,
            ResilienceEvent::SpeakerDeviceFailed { message }
                if message == "Speaker disconnected — check output device"
        )));
        assert_eq!(
            context.diagnostics.lock_unpoisoned().as_slice(),
            [
                crate::logging::PlayoutTransitionTag::Unavailable,
                crate::logging::PlayoutTransitionTag::Unavailable,
            ]
        );
    }

    /// The tick tests above drive the real `audio_device_watch_tick`, but
    /// through a fake context -- so they cannot see whether the PRODUCTION
    /// context and the join path are still hooked to a live playout handle,
    /// and that disconnection is precisely the #867 defect (the wrapper was
    /// discarded, so nothing could ever call `refresh_default_playout_device`).
    /// This crate has no `tauri::test` mock-runtime harness to build a real
    /// `AppHandle` with (see `compositor.rs`/`camera_session.rs`), so pin the
    /// three wiring hops by source, the way `share_border.rs` already does.
    #[test]
    fn the_production_audio_watch_is_still_wired_to_a_live_playout_handle() {
        // Split the test module off FIRST. `include_str!` reads this file
        // whole, so searching all of it also matches the assertion message
        // literal below -- the guard then passes no matter what production
        // does. Caught by mutation-checking the guard itself (#867).
        let resilience = include_str!("resilience.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("split always yields a first element");
        assert!(
            resilience.contains("playout.refresh_default_playout_device()"),
            "AppAudioDeviceWatchContext::refresh_playout no longer calls \
             refresh_default_playout_device -- the watch would tick forever \
             against a handle it never asks about (#867)"
        );

        let room = include_str!("session/room.rs");
        assert!(
            room.contains("SpeakerWatchHandle::new"),
            "join_room no longer passes a SpeakerWatchHandle to \
             resilience::start_for_room -- the poll can never reach this \
             room's playout (#867)"
        );

        let audio = include_str!("transport/audio.rs");
        assert!(
            !audio.contains("into_audio"),
            "transport::audio unwraps SpeakerPlayout back to a bare \
             PlatformAudio again; that discards the only handle capable of \
             following a default-device change mid-call (#867)"
        );
    }

    #[test]
    fn mic_watch_behaviour_is_unchanged_by_the_speaker_watch() {
        let context = FakeAudioDeviceWatchContext::new(
            [
                Some(RecordingDeviceRefresh::Switched("Studio Mic".to_string())),
                Some(RecordingDeviceRefresh::Failed(
                    "mic failed once".to_string(),
                )),
                Some(RecordingDeviceRefresh::Failed(
                    "duplicate failure".to_string(),
                )),
                None,
                Some(RecordingDeviceRefresh::Failed(
                    "mic failed again".to_string(),
                )),
            ],
            [None, None, None, None, None],
        );
        let mut reporting = AudioWatchReporting::default();
        for _ in 0..5 {
            audio_device_watch_tick(&context, &mut reporting);
        }

        let emitted = context.emitted.lock_unpoisoned();
        assert_eq!(emitted.len(), 3);
        assert!(matches!(
            &emitted[0],
            ResilienceEvent::MicDeviceChanged {
                device_name,
                using_default: None,
            } if device_name == "Studio Mic"
        ));
        assert!(matches!(
            &emitted[1],
            ResilienceEvent::MicDeviceFailed { message } if message == "mic failed once"
        ));
        assert!(matches!(
            &emitted[2],
            ResilienceEvent::MicDeviceFailed { message } if message == "mic failed again"
        ));
        assert!(context.preference_clears.lock_unpoisoned().is_empty());
        assert!(context.diagnostics.lock_unpoisoned().is_empty());
    }

    #[cfg(target_os = "macos")]
    struct FakeDisplayReconfigurationContext {
        shares: Vec<(u32, crate::transport::publisher::SharedSourceKind)>,
        updated_frames: Mutex<Vec<(u32, crate::hover_tab::WindowFrame)>>,
        stopped_sources: Mutex<Vec<u32>>,
    }

    #[cfg(target_os = "macos")]
    impl FakeDisplayReconfigurationContext {
        fn display(source_id: u32) -> Self {
            Self {
                shares: vec![(
                    source_id,
                    crate::transport::publisher::SharedSourceKind::Display,
                )],
                updated_frames: Mutex::new(Vec::new()),
                stopped_sources: Mutex::new(Vec::new()),
            }
        }
    }

    #[cfg(target_os = "macos")]
    impl DisplayReconfigurationContext for FakeDisplayReconfigurationContext {
        fn active_share_sources(
            &self,
        ) -> Vec<(u32, crate::transport::publisher::SharedSourceKind)> {
            self.shares.clone()
        }

        fn update_share_frames(&self, fresh: &[(u32, crate::hover_tab::WindowFrame)]) {
            self.updated_frames
                .lock_unpoisoned()
                .extend_from_slice(fresh);
        }

        fn stop_missing_share(
            &self,
            source_id: u32,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                self.stopped_sources.lock_unpoisoned().push(source_id);
            })
        }
    }

    #[cfg(target_os = "macos")]
    struct FakeDisplayFrameLookup {
        frames: std::collections::HashMap<u32, crate::hover_tab::WindowFrame>,
        requested_ids: Mutex<Vec<u32>>,
    }

    #[cfg(target_os = "macos")]
    impl FakeDisplayFrameLookup {
        fn new(frames: impl IntoIterator<Item = (u32, crate::hover_tab::WindowFrame)>) -> Self {
            Self {
                frames: frames.into_iter().collect(),
                requested_ids: Mutex::new(Vec::new()),
            }
        }
    }

    #[cfg(target_os = "macos")]
    impl DisplayFrameLookup for FakeDisplayFrameLookup {
        fn frame_for_display_id(&self, display_id: u32) -> Option<crate::hover_tab::WindowFrame> {
            self.requested_ids.lock_unpoisoned().push(display_id);
            self.frames.get(&display_id).copied()
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn display_reconfiguration_keeps_present_display_share_and_updates_its_frame() {
        let raw_display_id = 42;
        let tagged_source_id = crate::window_source::display_source_id(raw_display_id);
        let resized_frame = crate::hover_tab::WindowFrame {
            x: 120,
            y: -40,
            width: 2560,
            height: 1440,
        };
        let context = FakeDisplayReconfigurationContext::display(tagged_source_id);
        let displays = FakeDisplayFrameLookup::new([(raw_display_id, resized_frame)]);

        handle_display_reconfiguration(&context, &displays).await;

        assert_eq!(
            displays.requested_ids.lock_unpoisoned().as_slice(),
            [raw_display_id],
            "the handler must decode the tagged session source id before the raw display lookup"
        );
        assert_eq!(
            context.updated_frames.lock_unpoisoned().as_slice(),
            [(tagged_source_id, resized_frame)],
            "a resolution/arrangement change must refresh the live display share's frame"
        );
        assert!(
            context.stopped_sources.lock_unpoisoned().is_empty(),
            "an online display must survive the real reconfiguration handler"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn display_reconfiguration_stops_genuinely_removed_display_share() {
        let raw_display_id = 73;
        let tagged_source_id = crate::window_source::display_source_id(raw_display_id);
        let context = FakeDisplayReconfigurationContext::display(tagged_source_id);
        let displays = FakeDisplayFrameLookup::new([]);

        handle_display_reconfiguration(&context, &displays).await;

        assert_eq!(
            displays.requested_ids.lock_unpoisoned().as_slice(),
            [raw_display_id],
            "removed-display classification must use the decoded raw display id"
        );
        assert!(context.updated_frames.lock_unpoisoned().is_empty());
        assert_eq!(
            context.stopped_sources.lock_unpoisoned().as_slice(),
            [tagged_source_id],
            "a display absent from the online-display lookup must still take the handler's teardown path"
        );
    }

    #[tokio::test]
    async fn connect_time_resilience_receiver_reaches_watcher_and_stops_on_disconnect() {
        use std::sync::atomic::AtomicUsize;

        let (source_tx, source_rx) = tokio::sync::mpsc::unbounded_channel();
        let connection =
            crate::transport::RoomConnection::from_connect_event_source_for_test(source_rx);
        source_tx
            .send(livekit::RoomEvent::Disconnected {
                reason: livekit::DisconnectReason::ServerShutdown,
            })
            .expect("the connect-time fanout accepts the terminal event");

        let events = connection
            .take_resilience_events()
            .expect("the resilience branch is available exactly once");
        let session = Arc::new(crate::session::SessionState::default());
        let generation = session.begin_room_generation();
        let surfaced = Arc::new(Mutex::new(Vec::new()));
        let cleanup_count = Arc::new(AtomicUsize::new(0));
        let effects = DisconnectEffects {
            surface: {
                let surfaced = surfaced.clone();
                Arc::new(move |event| surfaced.lock_unpoisoned().push(event))
            },
            cleanup: {
                let session = session.clone();
                let cleanup_count = cleanup_count.clone();
                Arc::new(move || {
                    cleanup_count.fetch_add(1, Ordering::SeqCst);
                    session.invalidate_room_generation();
                    Box::pin(async {}) as DisconnectCleanup
                })
            },
        };

        let watcher = start_room_event_watcher(events, generation.clone(), None, effects);
        tokio::time::timeout(Duration::from_secs(1), watcher)
            .await
            .expect("the terminal disconnect must end the actual watcher task")
            .expect("the watcher task exits cleanly");
        assert!(
            !generation.is_current(),
            "forced-disconnect cleanup invalidates the room generation"
        );
        assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
        assert!(matches!(
            surfaced.lock_unpoisoned().as_slice(),
            [ResilienceEvent::Disconnected { reason }] if reason == "ServerShutdown"
        ));
    }

    #[test]
    fn app_global_monitor_bootstrap_does_not_starve_async_runtime() {
        use std::sync::mpsc;

        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);

        // Hold the simulated OS registration open, then require an unrelated
        // async sentinel to run. This protects the join handoff from both a
        // direct wait and executor-worker starvation.
        schedule_monitor_bootstrap(move || {
            started_tx.send(()).expect("test receiver is alive");
            release_rx.recv().expect("test releases bootstrap");
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("scheduled monitor bootstrap should run");

        let (sentinel_tx, sentinel_rx) = mpsc::sync_channel(1);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            sentinel_tx.send(()).expect("test receiver is alive");
        });
        sentinel_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked optional monitor must not starve async join work");
        release_tx.send(()).expect("bootstrap is still waiting");
    }

    /// `ResilienceEvent` must serialize with the tagged `kind` shape the
    /// frontend's `ToastHost.svelte` switches on -- a JSON round-trip test
    /// like `telepointer.rs`'s own field-name assertion, since this is a
    /// cross-language contract (Rust `serde` tag <-> a TS discriminated
    /// union), not just an internal implementation detail.
    #[test]
    fn reconnected_event_serializes_with_expected_shape() {
        let event = ResilienceEvent::Reconnected {
            message: "Switched network — reconnected".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "reconnected");
        assert_eq!(json["message"], "Switched network — reconnected");
    }

    #[test]
    fn post_reconnect_share_repair_epoch_rejects_stale_tasks() {
        assert!(post_reconnect_share_repair_epoch_is_current(3, 3));
        assert!(!post_reconnect_share_repair_epoch_is_current(4, 3));
    }

    #[test]
    fn post_reconnect_share_repair_gate_requires_current_joined_lifecycle() {
        assert!(post_reconnect_share_repair_gate(true, true, true));
        assert!(!post_reconnect_share_repair_gate(false, true, true));
        assert!(!post_reconnect_share_repair_gate(true, false, true));
        assert!(!post_reconnect_share_repair_gate(true, true, false));
    }

    #[test]
    fn reconnect_share_repair_rollback_requires_exact_opt_out() {
        assert!(reconnect_share_repair_disabled_value(Some("1")));
        assert!(!reconnect_share_repair_disabled_value(None));
        assert!(!reconnect_share_repair_disabled_value(Some("0")));
        assert!(!reconnect_share_repair_disabled_value(Some("true")));
        assert!(!reconnect_share_repair_disabled_value(Some(" 1 ")));
    }

    #[test]
    fn network_changed_event_serializes_with_expected_shape() {
        let event = ResilienceEvent::NetworkChanged;
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "networkChanged");
    }

    #[test]
    fn mic_device_changed_event_serializes_with_expected_shape() {
        let event = ResilienceEvent::MicDeviceChanged {
            device_name: "USB Mic".into(),
            using_default: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "micDeviceChanged");
        assert_eq!(json["deviceName"], "USB Mic");
    }

    #[test]
    fn mic_device_failed_event_serializes_with_expected_shape() {
        let event = ResilienceEvent::MicDeviceFailed {
            message: "Microphone disconnected — check input device".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "micDeviceFailed");
        assert_eq!(
            json["message"],
            "Microphone disconnected — check input device"
        );
    }

    #[test]
    fn share_publication_repair_events_serialize_with_expected_shape() {
        let recovering = serde_json::to_value(ResilienceEvent::SharePublicationRepairRecovering {
            window_id: 42,
        })
        .unwrap();
        assert_eq!(recovering["kind"], "sharePublicationRepairRecovering");
        assert_eq!(recovering["windowId"], 42);

        let cancelled = serde_json::to_value(ResilienceEvent::SharePublicationRepairCancelled {
            window_id: 42,
        })
        .unwrap();
        assert_eq!(cancelled["kind"], "sharePublicationRepairCancelled");
        assert_eq!(cancelled["windowId"], 42);

        let restored =
            serde_json::to_value(ResilienceEvent::SharePublicationRepairRestored { window_id: 42 })
                .unwrap();
        assert_eq!(restored["kind"], "sharePublicationRepairRestored");
        assert_eq!(restored["windowId"], 42);

        let failed = serde_json::to_value(ResilienceEvent::SharePublicationRepairFailed {
            window_id: 42,
            message: "Share repair failed".into(),
        })
        .unwrap();
        assert_eq!(failed["kind"], "sharePublicationRepairFailed");
        assert_eq!(failed["windowId"], 42);
        assert_eq!(failed["message"], "Share repair failed");
    }

    #[test]
    fn system_wake_debounce_accepts_first_and_drops_burst_duplicates() {
        let now = Instant::now();
        *LAST_SYSTEM_WAKE_AT.lock_unpoisoned() = None;
        assert!(accept_system_wake(now));
        assert!(!accept_system_wake(now + Duration::from_millis(500)));
        assert!(accept_system_wake(
            now + SYSTEM_WAKE_DEBOUNCE + Duration::from_millis(1)
        ));
    }

    #[test]
    fn proactive_reconnect_source_labels_are_stable_for_logs() {
        assert_eq!(
            ProactiveReconnectSource::NetworkChange.label(),
            "network change"
        );
        assert_eq!(ProactiveReconnectSource::SystemWake.label(), "system wake");
    }

    #[test]
    fn disconnected_event_serializes_with_expected_shape() {
        let event = ResilienceEvent::Disconnected {
            reason: "ServerShutdown".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "disconnected");
        assert_eq!(json["reason"], "ServerShutdown");
    }

    /// Issue #18: our own deliberate `leave_room` -> `Room::close()` fires
    /// `Disconnected { ClientInitiated }` -- that must never reach the UI
    /// (it was one of the false "Disconnected — attempting to reconnect"
    /// sources); every non-deliberate reason must still surface.
    #[test]
    fn client_initiated_disconnect_is_not_surfaced() {
        assert!(!should_surface_disconnect(
            livekit::DisconnectReason::ClientInitiated
        ));
    }

    #[test]
    fn non_client_initiated_disconnects_are_surfaced() {
        for reason in [
            livekit::DisconnectReason::UnknownReason,
            livekit::DisconnectReason::ServerShutdown,
            livekit::DisconnectReason::ParticipantRemoved,
            livekit::DisconnectReason::DuplicateIdentity,
            livekit::DisconnectReason::ConnectionTimeout,
        ] {
            assert!(
                should_surface_disconnect(reason),
                "{reason:?} should surface a toast"
            );
        }
    }

    /// Issue #18 trigger discrimination: the snapshot comparison is what
    /// separates a real primary-interface change from routine notification
    /// noise (DHCP renew etc. notifies without changing these two fields).
    #[test]
    fn unchanged_primary_network_snapshot_compares_equal() {
        let a = PrimaryNetworkSnapshot {
            interface: Some("en0".into()),
            router: Some("192.168.1.1".into()),
        };
        assert_eq!(a, a.clone());
        // No network at all, twice in a row, is also "no change".
        assert_eq!(
            PrimaryNetworkSnapshot::default(),
            PrimaryNetworkSnapshot::default()
        );
    }

    #[test]
    fn primary_interface_or_router_change_is_detected() {
        let wifi = PrimaryNetworkSnapshot {
            interface: Some("en0".into()),
            router: Some("192.168.1.1".into()),
        };
        let ethernet = PrimaryNetworkSnapshot {
            interface: Some("en5".into()),
            router: Some("192.168.1.1".into()),
        };
        let new_router = PrimaryNetworkSnapshot {
            interface: Some("en0".into()),
            router: Some("10.0.0.1".into()),
        };
        let offline = PrimaryNetworkSnapshot::default();
        assert_ne!(wifi, ethernet, "interface switch is a real change");
        assert_ne!(
            wifi, new_router,
            "same interface, new router is a real change"
        );
        assert_ne!(
            wifi, offline,
            "losing the primary interface is a real change"
        );
    }

    #[test]
    fn snapshot_describe_is_log_friendly() {
        let wifi = PrimaryNetworkSnapshot {
            interface: Some("en0".into()),
            router: Some("192.168.1.1".into()),
        };
        assert_eq!(wifi.describe(), "interface=en0 router=192.168.1.1");
        assert_eq!(
            PrimaryNetworkSnapshot::default().describe(),
            "interface=<none> router=<none>"
        );
    }
}
