//! The receiver-side compositor (SPEC.md §4.4): "each shared window appears
//! on every other participant's machine as a real, native, borderless
//! window." This is the single most important gap this codebase had before
//! this module -- `transport::subscriber` already decoded subscribed video
//! tracks (for M0's latency measurement only), but nothing turned a
//! subscribed track into an actual on-screen native window. This module is
//! that missing piece.
//!
//! ## Architecture
//!
//! One [`CompositorWindow`] per subscribed remote shared-window video track,
//! keyed by the composite `(owner_identity, source window_id)`. The source
//! `window_id` is recovered from the track name -- see
//! `transport::publisher::window_id_from_track_name`, the inverse of
//! `track_name_for_window` -- but CGWindowIDs are only local to one sharer's
//! Mac, so the owner identity is required to avoid cross-participant
//! collisions. Each window is:
//!
//! - a borderless `NSPanel` (via `tauri_nspanel`, the SAME builder API
//!   `hover_tab.rs`/`share_border.rs` already use -- no new native-window
//!   creation pattern invented for this task), at `PanelLevel::Normal`.
//!   Remote shared windows are real content, so normal app windows must be
//!   able to stack above them; transient chrome such as the hover tab/share
//!   border uses higher levels when it must stay reachable,
//! - hosting a real `AVSampleBufferDisplayLayer` (`native_display.rs`) fed
//!   directly from the subscriber's decoded `CVPixelBuffer`s with NO CPU
//!   copy,
//! - with a `RemoteWindowHeader`-rendering webview docked to its top edge
//!   (the header is real SvelteKit content, hosted the same way
//!   `hover_tab.rs`'s pill / `menubar.rs`'s popover already combine a native
//!   panel with SvelteKit-rendered content -- a small child webview
//!   positioned over/adjacent to a native surface, not a new pattern), and
//! - a transparent, click-through overlay webview for telepointers
//!   (`Pointer.svelte`/`NamePill.svelte`), covering the video area only.
//!
//! ## Placement (SPEC.md §4.4: "no position memory")
//!
//! Cascaded from the primary display's top-left corner: window N is offset
//! `N * CASCADE_STEP` points right and down from the first window's
//! position, wrapping back to the origin after a handful of steps so windows
//! don't march off-screen forever. Nothing is persisted.
//!
//! ## Lifecycle
//!
//! [`ensure_window`] creates a window for a `window_id` the first time a
//! track for it is subscribed (idempotent -- a second call for an
//! already-open window is a no-op returning the existing handle).
//! [`remove_window`] tears one down (called on `share-ended`/participant
//! disconnect -- see `subscriber.rs`'s call site). [`push_frame`] feeds one
//! more real decoded frame into an existing window's display layer.

#![cfg(target_os = "macos")]

use crate::sync_ext::MutexExt;
use crate::time_util::now_ms;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel};

use crate::native_display::DisplayLayer;
use crate::native_display::OwnedCMSampleBuffer;
use crate::platform::cg::WindowFrame;
use crate::transport::publisher::SharedSourceKind;

/// The teardown trigger for a remote compositor window. Keep these labels
/// stable: they are used to distinguish the otherwise identical teardown
/// paths in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveWindowReason {
    TrackUnsubscribed,
    TrackUnpublished,
    ParticipantDisconnected,
    NoFrameWatchdog,
    ManualHide,
    LeaveRoom,
    /// #298 reconciliation: the SFU no longer holds a publication for this
    /// window, so the tile was asserting a share that does not exist.
    ReconciledPublicationGone,
    /// #298 reconciliation: the publication exists but this receiver could not
    /// re-establish it within one bounded recovery attempt. Displaying the
    /// truth beats displaying an unverified share.
    ReconciledUnrecoverable,
}

impl RemoveWindowReason {
    fn label(self) -> &'static str {
        match self {
            Self::TrackUnsubscribed => "track-unsubscribed",
            Self::TrackUnpublished => "track-unpublished",
            Self::ParticipantDisconnected => "participant-disconnected",
            Self::NoFrameWatchdog => "no-frame-watchdog",
            Self::ManualHide => "manual-hide",
            Self::LeaveRoom => "leave-room",
            Self::ReconciledPublicationGone => "reconciled-publication-gone",
            Self::ReconciledUnrecoverable => "reconciled-unrecoverable",
        }
    }
}

/// Cascade step between successively-created remote windows (logical
/// points) -- SPEC.md §4.4: "successive windows step down-and-right so they
/// don't stack exactly."
const CASCADE_STEP: f64 = 32.0;
/// Wrap the cascade back to the top-left after this many windows, so a long
/// session doesn't walk windows off the bottom-right of the screen.
const CASCADE_WRAP: u32 = 10;
/// Header height (logical points) -- MUST match `RemoteWindowHeader.svelte`'s
/// real rendered height: the decoded shared-window header is one 44px bar.
/// If either side's height ever changes, update BOTH this constant and the
/// Svelte rule in the same commit.
const HEADER_HEIGHT: f64 = 44.0;
/// Default initial video-content size for a newly created window, before the
/// first real frame's dimensions are known (`ensure_window` is called at
/// track-subscribe time, which can race the first decoded frame by a few
/// milliseconds) -- deliberately a plausible placeholder, corrected the
/// instant `push_frame`'s first call reports the real size (see
/// `resize_to_source` below).
const DEFAULT_CONTENT_WIDTH: f64 = 640.0;
const DEFAULT_CONTENT_HEIGHT: f64 = 400.0;
/// Receiver windows stay at or above the header's final 300px responsive
/// breakpoint. Below this width even the compact overflow controls no longer
/// have a legible, non-overlapping layout (#497).
const MIN_RESIZE_CONTENT_WIDTH: f64 = 300.0;
const MIN_RESIZE_CONTENT_HEIGHT: f64 = 150.0;
/// A newly received remote share must not open larger than the receiver's
/// usable desktop, or its borderless header can become unreachable.
const INITIAL_MAX_WORK_AREA_FRACTION: f64 = 0.8;
/// Keep nearest-neighbor sharpness to useful integer magnification. 3x+ is
/// technically on-grid but reads as oversized pixel doubling for text.
const MAX_NEAREST_INTEGER_SCALE: u32 = 2;
/// Applied only at pointer-up after a manual resize. Live drags stay smooth;
/// close final sizes magnetically land on the source pixel grid.
const RESIZE_INTEGER_SNAP_THRESHOLD_RATIO: f64 = 0.05;
/// Remote screenshare outline. Matches the local active-share overlay's
/// 4px/10px treatment in `share_border.rs`; color comes from the same
/// deterministic owner palette the remote header route uses.
const SCREENSHARE_BORDER_STROKE_PX: f64 = 4.0;
const SCREENSHARE_BORDER_RADIUS_PX: f64 = 10.0;
/// Keep the four most recently retired compositor windows fully warm. Older
/// retired panels stay hidden/reusable, but their video layer and child route
/// contents are unloaded so long sessions do not grow without bound (#223).
const RETIRED_WARM_POOL_CAP: usize = 4;
/// Bound the receiver-side frame queue per remote window. Enqueue still runs
/// on the main thread, but bursts are drained in one hop; under backlog, old
/// live-video frames are less valuable than the newest frame.
const MAX_PENDING_DISPLAY_SAMPLES_PER_WINDOW: usize = 1;
const ENSURE_WINDOW_CREATION_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(3);
/// The command-driven resize path is authoritative. This fallback only covers
/// a lost pointer-up/IPC finalization, and deliberately does not infer drag
/// state from WindowEvent::Resized (programmatic resizes emit that event too).
const USER_RESIZE_TTL: u64 = 750;
/// Backstop for `user_resize_active`: a lost/never-sent finalize IPC (e.g.
/// `compositorResize.ts`'s pointer-released-before-begin-resolved path,
/// which returns without ever calling `compositor_resize_window`) would
/// otherwise latch this true forever, permanently suppressing source
/// reconciliation -- the exact failure `USER_RESIZE_TTL` exists to prevent
/// for the OTHER half of drag tracking (#416 review finding). No real resize
/// gesture takes anywhere close to this long.
const MAX_USER_RESIZE_GESTURE_MS: u64 = 30_000;
/// AppKit normally delivers `Resized` synchronously or on its next run-loop
/// turn. After this short barrier a missing cancelled callback must not hold a
/// newer source resize hostage forever (#416).
const PROGRAMMATIC_RESIZE_ACK_GRACE: Duration = Duration::from_millis(250);
/// Keep cancellation expectations bounded even if AppKit drops callbacks for
/// a long sequence of rapid programmatic resizes.
const MAX_CANCELLED_PROGRAMMATIC_RESIZES: usize = 8;

/// #416 diagnostic: every writer of a remote panel's geometry, in one ordered
/// trace. Five fixes have now been proven correct in isolation and still failed
/// live, because nothing recorded WHICH writer moved the panel. Enabled by
/// `PETAL_TRACE_PANEL_GEOMETRY=1`; off, this is one relaxed atomic load.
static TRACE_PANEL_GEOMETRY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static PANEL_GEOMETRY_TRACE_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn panel_geometry_trace_enabled() -> bool {
    match TRACE_PANEL_GEOMETRY.load(Ordering::Relaxed) {
        0 => {
            let on = std::env::var("PETAL_TRACE_PANEL_GEOMETRY")
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false);
            TRACE_PANEL_GEOMETRY.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        2 => true,
        _ => false,
    }
}

/// Record one geometry write (or refused write). `reason` names the call site,
/// so a live trace reads as an ordered list of who moved the panel and why.
fn trace_panel_geometry(
    reason: &str,
    window_id: u32,
    width: f64,
    total_height: f64,
    gesture_active: Option<bool>,
) {
    if !panel_geometry_trace_enabled() {
        return;
    }
    let seq = PANEL_GEOMETRY_TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
    log::info!(
        "PANELGEO seq={seq} t={} window={window_id} reason={reason} w={width:.2} h={total_height:.2} gesture={}",
        now_ms(),
        match gesture_active {
            Some(true) => "active",
            Some(false) => "idle",
            None => "n/a",
        },
    );
}

fn remote_window_min_size() -> tauri::LogicalSize<f64> {
    tauri::LogicalSize {
        width: MIN_RESIZE_CONTENT_WIDTH,
        height: HEADER_HEIGHT + MIN_RESIZE_CONTENT_HEIGHT,
    }
}

fn set_remote_window_min_size(window: &tauri::WebviewWindow) {
    if let Err(error) = window.set_min_size(Some(tauri::Size::Logical(remote_window_min_size()))) {
        log::warn!(
            "compositor: failed to set minimum size for remote window '{}': {error}",
            window.label()
        );
    }
}

// Keep this native owner palette in lockstep with the TS PALETTE and hex map
// in apps/desktop/src/lib/data/identityColor.ts.
const OWNER_COLOR_PALETTE_HEX: [&str; 6] = [
    "#f06cc9", // plum
    "#6e8bff", // blue
    "#7ff0a3", // green
    "#e8b84b", // amber
    "#d6b8f0", // lilac
    "#8fa6b8", // slate
];

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct RemoteWindowKey {
    owner_identity: String,
    window_id: u32,
}

impl RemoteWindowKey {
    fn new(owner_identity: impl Into<String>, window_id: u32) -> Self {
        Self {
            owner_identity: owner_identity.into(),
            window_id,
        }
    }

    fn label_segment(&self) -> String {
        format!(
            "{:016x}-{}",
            owner_identity_hash(&self.owner_identity),
            self.window_id
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnsureWindowCreationWatchdogDecision {
    KeepWaiting,
    LogStall,
    LogPublicationChurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnsureWindowCreationBranch {
    Pending,
    Created,
    ReusedFromPool,
    AlreadyOpen,
}

impl EnsureWindowCreationBranch {
    fn completed_label(self) -> &'static str {
        match self {
            Self::Pending => "not completed",
            Self::Created => "built",
            Self::ReusedFromPool => "reused",
            Self::AlreadyOpen => "already open",
        }
    }
}

fn ensure_window_creation_watchdog_decision(
    elapsed: Duration,
    opened: bool,
    branch: EnsureWindowCreationBranch,
    retired: bool,
) -> EnsureWindowCreationWatchdogDecision {
    if opened || elapsed < ENSURE_WINDOW_CREATION_WATCHDOG_TIMEOUT {
        EnsureWindowCreationWatchdogDecision::KeepWaiting
    } else if retired
        || matches!(
            branch,
            EnsureWindowCreationBranch::ReusedFromPool | EnsureWindowCreationBranch::AlreadyOpen
        )
    {
        EnsureWindowCreationWatchdogDecision::LogPublicationChurn
    } else {
        EnsureWindowCreationWatchdogDecision::LogStall
    }
}

/// #901 minimum gap between auto-raises of the SAME remote window. Long
/// enough that #840/#841 republish churn (observed up to ~3x/second) never
/// re-raises, short enough that a deliberate unshare/re-share reads as a new
/// share and comes to the front again.
const AUTO_RAISE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Pure decision (unit-tested): should this reveal raise the window to the
/// front? `None` means it has never been auto-raised -- always raise.
fn auto_raise_on_reveal_due(
    last_auto_raised: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    match last_auto_raised {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= AUTO_RAISE_DEBOUNCE,
    }
}

fn apply_retired_reuse_reveal_state(
    revealed_first_frame: &mut bool,
    layer_has_content: bool,
) -> bool {
    let reveal_now = layer_has_content;
    *revealed_first_frame = reveal_now;
    reveal_now
}

fn owner_identity_hash(owner_identity: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in owner_identity.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn panel_label_for_key(key: &RemoteWindowKey) -> String {
    format!("remote-window-{}", key.label_segment())
}
// The header no longer has its own window (it rides in the panel's surface
// webview); this label helper is retained only for label-naming tests.
#[cfg_attr(not(test), allow(dead_code))]
fn header_label_for_key(key: &RemoteWindowKey) -> String {
    format!("remote-window-header-{}", key.label_segment())
}
fn control_label_for_key(key: &RemoteWindowKey) -> String {
    format!("remote-window-control-{}", key.label_segment())
}
fn pointer_label_for_key(key: &RemoteWindowKey) -> String {
    format!("remote-window-pointer-{}", key.label_segment())
}
/// #844: the receiver-side AI-chat transcript/typed-message overlay -- a
/// native child webview, same family as control/pointer, layered above the
/// video so it is actually visible and clickable (unlike the old in-webview
/// popover it replaces).
fn ai_chat_label_for_key(key: &RemoteWindowKey) -> String {
    format!("remote-window-ai-chat-{}", key.label_segment())
}

/// Resolve the ai-chat overlay's webview label for a window, retired-inclusive
/// (`resolve_window_key`) so `ai_chat/topic.rs` can still push a state/
/// transcript update into a window that is mid-retire -- the overlay webview
/// itself is only hidden, never destroyed, across a retire (see
/// `remove_window`/`show_retired_window_on_main`).
pub(crate) fn ai_chat_overlay_label_for_window(
    window_id: u32,
    owner_identity: &str,
) -> Option<String> {
    let key = resolve_window_key(window_id, Some(owner_identity))?;
    Some(ai_chat_label_for_key(&key))
}

pub(crate) fn pointer_labels_for_window(window_id: u32) -> Vec<String> {
    with_state(|s| {
        s.windows
            .keys()
            .filter(|key| key.window_id == window_id)
            .map(pointer_label_for_key)
            .collect()
    })
}

pub(crate) fn pointer_label_for_remote_window(
    window_id: u32,
    owner_identity: &str,
) -> Option<String> {
    let key = resolve_open_window_key(window_id, Some(owner_identity))?;
    Some(pointer_label_for_key(&key))
}

/// Percent-encode a header query-param value (owner display name / source
/// window title -- arbitrary OS/user strings that can contain spaces, em
/// dashes, unicode, `&`/`=`, etc.). Uses the `NON_ALPHANUMERIC` component set
/// (encode everything outside `[A-Za-z0-9]`), the simplest correct choice
/// for a value that's going straight into a query string with no need to
/// preserve any punctuation as literal.
fn percent_encode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn header_query_string(
    window_id: u32,
    owner_identity: &str,
    owner_display_name: &str,
    source_title: &str,
    source_url: Option<&str>,
    remote_control_available: bool,
    remote_control_disallowed: bool,
    owner_palette_index: Option<u8>,
) -> String {
    let mut query = format!(
        "windowId={window_id}&owner={}&title={}",
        percent_encode(owner_display_name),
        percent_encode(source_title)
    );
    query.push_str("&ownerIdentity=");
    query.push_str(&percent_encode(owner_identity));
    query.push_str("&borderColor=");
    query.push_str(&percent_encode(owner_border_color_hex(
        owner_identity,
        owner_display_name,
        owner_palette_index,
    )));
    if let Some(index) =
        owner_palette_index.filter(|index| (*index as usize) < OWNER_COLOR_PALETTE_HEX.len())
    {
        query.push_str("&ownerPaletteIndex=");
        query.push_str(&index.to_string());
    }
    query.push_str("&borderStroke=");
    query.push_str(&SCREENSHARE_BORDER_STROKE_PX.to_string());
    query.push_str("&borderRadius=");
    query.push_str(&SCREENSHARE_BORDER_RADIUS_PX.to_string());
    if let Some(source_url) = source_url.filter(|u| crate::browser_url::is_openable_url(u)) {
        query.push_str("&url=");
        query.push_str(&percent_encode(source_url));
    }
    if remote_control_available {
        query.push_str("&remoteControl=1");
    } else if remote_control_disallowed {
        // Distinct from merely-absent: the header must say "not allowed"
        // rather than sit on an indefinite "Preparing...", which would be a
        // lie about a permanent state.
        query.push_str("&remoteControlDisallowed=1");
    }
    query
}

/// URL for the panel's own surface webview, carrying the header metadata as
/// query params. The header chrome is rendered by `compositor/surface.html`
/// itself (in its exposed top strip), so the panel IS the header -- no
/// separate `addChildWindow` header child that could detach or fall behind.
fn surface_route_url(
    window_id: u32,
    owner_identity: &str,
    owner_display_name: &str,
    source_title: &str,
    source_url: Option<&str>,
    remote_control_available: bool,
    remote_control_disallowed: bool,
    owner_palette_index: Option<u8>,
) -> String {
    format!(
        "compositor/surface.html?{}",
        header_query_string(
            window_id,
            owner_identity,
            owner_display_name,
            source_title,
            source_url,
            remote_control_available,
            remote_control_disallowed,
            owner_palette_index,
        )
    )
}

fn owner_color_hex(owner: &str) -> &'static str {
    let hash = owner.encode_utf16().fold(0u32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(unit as u32)
    });
    OWNER_COLOR_PALETTE_HEX[(hash as usize) % OWNER_COLOR_PALETTE_HEX.len()]
}

/// The remote-window border must match the header tint. The header keys on the
/// owner IDENTITY, falling back to the display name — `colorForIdentity(
/// ownerIdentity || ownerName)` in surface/+page.svelte. Keying the border on
/// the display name alone diverged whenever identity != name (the normal case),
/// so the border and header showed different colors. Mirror the header here.
fn owner_border_color_hex(
    owner_identity: &str,
    owner_display_name: &str,
    palette_index: Option<u8>,
) -> &'static str {
    if let Some(index) =
        palette_index.filter(|index| (*index as usize) < OWNER_COLOR_PALETTE_HEX.len())
    {
        return OWNER_COLOR_PALETTE_HEX[index as usize];
    }
    let key = if owner_identity.trim().is_empty() {
        owner_display_name
    } else {
        owner_identity
    };
    owner_color_hex(key)
}

fn parse_hex_rgb(hex: &str) -> Option<(f64, f64, f64)> {
    let raw = hex.strip_prefix('#').unwrap_or(hex);
    if raw.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&raw[0..2], 16).ok()? as f64 / 255.0;
    let g = u8::from_str_radix(&raw[2..4], 16).ok()? as f64 / 255.0;
    let b = u8::from_str_radix(&raw[4..6], 16).ok()? as f64 / 255.0;
    Some((r, g, b))
}

/// Everything this module owns for one open remote-window compositor
/// instance.
struct CompositorWindow {
    /// Owning identity (LiveKit participant identity -- e.g. "till@petal")
    /// of the peer sharing this window, for the header's avatar/title and
    /// for `remove_window_for_participant`'s lookup.
    owner_identity: String,
    owner_display_name: String,
    owner_palette_index: Option<u8>,
    source_title: String,
    source_url: Option<String>,
    source_kind: SharedSourceKind,
    /// Opaque publisher-side generation used by capable remote-control
    /// controllers to bind requests to the exact live share instance.
    share_instance_id: Option<String>,
    display: Option<DisplayLayer>,
    /// Captured source pixels per sharer logical point. This lets a Retina
    /// 2x capture open at the sharer's point size instead of double-size.
    source_scale: f64,
    /// Current video-content size in logical points -- the panel's content
    /// area (excluding the header strip) is kept exactly this size, and the
    /// aspect ratio this implies is what resize events are clamped to (see
    /// `resize_to_source`).
    /// The panel's current on-screen content area. This is owned by the live
    /// resize handler and must never be used as source metadata.
    panel_content_size: Mutex<(f64, f64)>,
    /// The latest coherent source presentation size in logical points. Unlike
    /// panel_content_size, this is not changed by a user drag.
    source_presentation_size: Mutex<Option<(f64, f64)>>,
    /// Publisher-advertised dimensions. Unlike decoded frame dimensions this
    /// does not change when the receiver switches simulcast layers (#416).
    canonical_source_pixel_size: Mutex<Option<(u32, u32)>>,
    /// Native resize acknowledgements are not tagged by AppKit. Keep the
    /// current request *and* cancelled/settled expectations in FIFO order so
    /// a delayed callback can be consumed as stale rather than falling into
    /// the user-resize path (#416).
    programmatic_resize_events: Mutex<ProgrammaticResizeEvents>,
    /// Monotonic generation for the pending native sizing transaction. A
    /// newer source resize, a user drag, or retirement replaces or clears the
    /// old expectation before its delayed event can be consumed.
    next_programmatic_resize_generation: AtomicU64,
    /// Monotonic share-instance epoch. Old main-thread callbacks must never
    /// mutate a reused window after it has been retired.
    canonical_source_epoch: AtomicU64,
    /// Monotonic sequence for canonical publisher dimensions within an epoch.
    canonical_source_generation: AtomicU64,
    /// A source resize observed during a drag is retained until pointer-up.
    pending_source_resize: Mutex<Option<InitialResizeTarget>>,
    /// Deadline set when a resize begins. This is only a recovery path for a
    /// begin/finalize IPC that gets lost; `user_resize_active` is the
    /// authoritative drag state and remains active while the pointer is held,
    /// even if the drag pauses longer than this TTL.
    user_resize_until_ms: AtomicU64,
    user_resize_active: AtomicBool,
    /// When `user_resize_active` was last set true. Lets a stale flag (lost
    /// finalize IPC) expire after `MAX_USER_RESIZE_GESTURE_MS` instead of
    /// latching forever -- see that constant's doc comment.
    user_resize_active_since_ms: AtomicU64,
    /// Backing/device scale of the receiver monitor currently hosting this
    /// panel. Kept beside panel_content_size so diagnostics can compare displayed
    /// device pixels with decoded source pixels without querying AppKit.
    receiver_scale: Mutex<f64>,
    /// Last decoded source frame size in real pixels. Used to choose the
    /// AVSampleBufferDisplayLayer filter when receiver display scale or
    /// user-resized content size differs from the source pixel grid (#222).
    source_pixel_size: Mutex<Option<(u32, u32)>>,
    /// Remote control is only meaningful for native desktop shares that carry
    /// the window metadata needed by the control bridge. Web-origin shares
    /// intentionally omit the header button and reject direct enable attempts.
    remote_control_available: bool,
    /// The sharer explicitly denied remote control for this window (as opposed
    /// to metadata simply not having arrived yet). Drives an honest header
    /// label; never a security decision -- the host re-checks every packet.
    remote_control_disallowed: bool,
    /// Last frame successfully observed for this remote compositor window.
    last_frame_received_ms: AtomicU64,
    frames_received: AtomicU64,
    /// Frames that reached `AVSampleBufferDisplayLayer.enqueueSampleBuffer:`.
    /// This is not a paint callback; it is the real main-thread display enqueue
    /// boundary used for pipeline diagnostics (#159).
    last_display_enqueued_ms: AtomicU64,
    frames_display_enqueued: AtomicU64,
    pending_display_samples: PendingFrameQueue<PendingDisplaySample>,
    revealed_first_frame: bool,
    /// Whether the AVSampleBufferDisplayLayer still holds a real enqueued
    /// frame. Unlike the reveal gate, this survives warm-pool retire/reuse.
    layer_has_content: bool,
    /// #627: this window is showing a HELD last frame rather than live media.
    /// Makes `hold_window_last_frame` idempotent (a reconcile divergence that
    /// recurs every 5s must not re-log or re-signal) and is what lets the
    /// header's paused label be cleared exactly when a frame reaches the layer.
    held_reason: Option<HoldWindowReason>,
    stripped_for_pool: bool,
    app_origin: Option<String>,
    /// #844: whether the receiver-side AI-chat overlay is currently
    /// disclosed. Survives the two paths that keep this SAME struct alive
    /// without treating it as a new share: `hold_window_last_frame` (never
    /// retires the window at all -- see that fn's doc comment on why #627
    /// made this the common republish outcome) and a retired-pool restore
    /// via `activate_window`/`show_retired_window_on_main` (drag/resize/
    /// Pop Out re-activating a window that was genuinely hidden). It is
    /// reset to `false` in `ensure_window`'s `reused` branch -- the OTHER
    /// republish path, and also a genuine re-share/toggle -- because that
    /// branch means a fresh share is starting on this key, which must not
    /// inherit a stale disclosure left over from an earlier session.
    ai_chat_overlay_open: bool,
    /// #875: this window's front-to-back rank within the sharer's
    /// currently-shared window set, from the `petalWindowZOrder`
    /// participant-metadata key (0 = frontmost). `None` for an older sharer
    /// that omits the key, or before the first metadata refresh arrives.
    /// Storage only in this lane -- the raise command that reads it to
    /// restack windows is a separate lane.
    z_rank: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ProgrammaticResizeTransaction {
    generation: u64,
    content_width: f64,
    content_height: f64,
    /// Source-driven sizing owes a reconciliation if a user gesture cancels
    /// it; an explicit user command (fit-to-source) does not (#416).
    source_driven: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CancelledProgrammaticResize {
    transaction: ProgrammaticResizeTransaction,
    barrier_deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LateSuccessfulResizeAck {
    content_width: f64,
    content_height: f64,
}

#[derive(Debug, Default)]
struct ProgrammaticResizeEvents {
    pending: Option<ProgrammaticResizeTransaction>,
    /// Only cancelled requests may leave an acknowledgement behind. AppKit
    /// does not expose a generation on `WindowEvent::Resized`, so consume this
    /// FIFO strictly from its head before accepting a later request.
    cancelled_callbacks: VecDeque<CancelledProgrammaticResize>,
    /// A `set_size` that succeeded but was reconciled through a synchronous
    /// native-size query can still emit its callback afterward. Keep exactly
    /// one bounded, actual-bounds acknowledgement so it never becomes a user
    /// resize. This is deliberately not an unbounded success FIFO.
    late_successful_ack: Option<LateSuccessfulResizeAck>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ResizeListenerDisposition {
    SettleProgrammatic(ProgrammaticResizeTransaction),
    BufferProgrammatic(ProgrammaticResizeTransaction),
    IgnoreStaleProgrammatic,
    UserResize,
}

fn resize_geometry_matches(
    transaction: ProgrammaticResizeTransaction,
    width: f64,
    content_height: f64,
    scale: f64,
) -> bool {
    // Tauri reports physical pixels while the transaction is in logical
    // points. One physical pixel of rounding is legitimate; anything beyond
    // that is a real user/stale event and must keep the aspect-lock path.
    let tolerance = 1.0 / scale.max(1.0) + f64::EPSILON;
    (transaction.content_width - width).abs() <= tolerance
        && (transaction.content_height - content_height).abs() <= tolerance
}

fn settled_geometry_within_one_physical_pixel(
    expected_width: f64,
    expected_height: f64,
    actual_width: f64,
    actual_height: f64,
    scale: f64,
) -> bool {
    let scale = scale.max(1.0);
    (expected_width - actual_width).abs() * scale <= 1.0 + f64::EPSILON
        && (expected_height - actual_height).abs() * scale <= 1.0 + f64::EPSILON
}

fn begin_programmatic_resize(
    window: &CompositorWindow,
    content_width: f64,
    content_height: f64,
    source_driven: bool,
) -> ProgrammaticResizeTransaction {
    let generation = window
        .next_programmatic_resize_generation
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    let transaction = ProgrammaticResizeTransaction {
        generation,
        content_width,
        content_height,
        source_driven,
    };
    let mut events = window.programmatic_resize_events.lock_unpoisoned();
    if let Some(previous) = events.pending.replace(transaction) {
        enqueue_cancelled_programmatic_resize(&mut events, previous);
    }
    transaction
}

/// Command-side half of the native resize adapter. This is deliberately kept
/// separate from settled panel state so tests can drive the same command →
/// listener transition without constructing a real AppKit panel.
///
/// #416: refusing while a user gesture is in progress is what makes the
/// gesture check ATOMIC with transaction creation. `cancel_programmatic_resize`
/// can only cancel a transaction that already exists, so a source resize whose
/// policy decision was taken microseconds before pointer-down would otherwise
/// create an uncancellable transaction whose native callback then snaps the
/// panel mid-drag. Callers holding a latchable target must pass it to
/// `resize_to_content_on_main` so the change is deferred, not dropped.
fn prepare_programmatic_resize_request(
    window: &CompositorWindow,
    content_width: f64,
    content_height: f64,
) -> Option<ProgrammaticResizeTransaction> {
    if resize_gesture_in_progress(window) {
        // Silent suppression is hard to diagnose in the field; leave a trace.
        log::debug!(
            "compositor: deferred source-driven resize to {content_width:.1}x{content_height:.1}; a user gesture owns the panel",
        );
        return None;
    }
    Some(begin_programmatic_resize(
        window,
        content_width,
        content_height,
        true,
    ))
}

/// An explicit user command (fit-to-source). It must NOT be suppressed by the
/// gesture guard: `resize_gesture_in_progress` deliberately tolerates a stale
/// active bit for up to `MAX_USER_RESIZE_GESTURE_MS`, and a lost pointer-up
/// must not silently disable a button the user just clicked (#416).
fn prepare_user_commanded_resize_request(
    window: &CompositorWindow,
    content_width: f64,
    content_height: f64,
) -> Option<ProgrammaticResizeTransaction> {
    Some(begin_programmatic_resize(
        window,
        content_width,
        content_height,
        false,
    ))
}

fn enqueue_cancelled_programmatic_resize(
    events: &mut ProgrammaticResizeEvents,
    transaction: ProgrammaticResizeTransaction,
) {
    let now = Instant::now();
    events
        .cancelled_callbacks
        .retain(|cancelled| cancelled.barrier_deadline > now);
    events
        .cancelled_callbacks
        .push_back(CancelledProgrammaticResize {
            transaction,
            barrier_deadline: now + PROGRAMMATIC_RESIZE_ACK_GRACE,
        });
    while events.cancelled_callbacks.len() > MAX_CANCELLED_PROGRAMMATIC_RESIZES {
        events.cancelled_callbacks.pop_front();
    }
}

fn classify_programmatic_resize_event_at(
    window: &CompositorWindow,
    width: f64,
    content_height: f64,
    scale: f64,
    now: Instant,
) -> ResizeListenerDisposition {
    let mut events = window.programmatic_resize_events.lock_unpoisoned();
    // A dropped AppKit callback must not pin the FIFO forever or make every
    // later matching geometry look stale. Once the grace barrier expires, the
    // current pending generation is authoritative.
    events
        .cancelled_callbacks
        .retain(|cancelled| cancelled.barrier_deadline > now);
    if let Some(index) = events.cancelled_callbacks.iter().position(|cancelled| {
        resize_geometry_matches(cancelled.transaction, width, content_height, scale)
    }) {
        // AppKit does not document FIFO delivery for coalesced Resized events.
        // Consume a bounded matching cancelled expectation wherever it sits;
        // never let an older unmatched callback force a newer event through
        // the user/aspect-lock path.
        events.cancelled_callbacks.remove(index);
        return ResizeListenerDisposition::IgnoreStaleProgrammatic;
    }
    if events.late_successful_ack.is_some_and(|ack| {
        (ack.content_width - width).abs() <= 1.0 / scale.max(1.0) + f64::EPSILON
            && (ack.content_height - content_height).abs() <= 1.0 / scale.max(1.0) + f64::EPSILON
    }) {
        events.late_successful_ack = None;
        return ResizeListenerDisposition::IgnoreStaleProgrammatic;
    }
    let Some(transaction) = events.pending else {
        return ResizeListenerDisposition::UserResize;
    };
    if !resize_geometry_matches(transaction, width, content_height, scale) {
        return ResizeListenerDisposition::UserResize;
    }
    // A matching newer callback while an older cancellation is still inside
    // its FIFO barrier cannot be trusted as either generation. Buffer it and
    // let the scheduled native-bounds reconciliation settle the current
    // transaction after the barrier; never run user aspect correction here.
    if events
        .cancelled_callbacks
        .iter()
        .any(|cancelled| cancelled.barrier_deadline > now)
    {
        return ResizeListenerDisposition::BufferProgrammatic(transaction);
    }
    events.pending = None;
    ResizeListenerDisposition::SettleProgrammatic(transaction)
}

fn classify_programmatic_resize_event(
    window: &CompositorWindow,
    width: f64,
    content_height: f64,
    scale: f64,
) -> ResizeListenerDisposition {
    classify_programmatic_resize_event_at(window, width, content_height, scale, Instant::now())
}

fn classify_resize_listener_event(
    window: &CompositorWindow,
    width: f64,
    content_height: f64,
    scale: f64,
    acknowledge_programmatic_resize: bool,
) -> ResizeListenerDisposition {
    classify_resize_listener_event_at(
        window,
        width,
        content_height,
        scale,
        acknowledge_programmatic_resize,
        Instant::now(),
    )
}

fn classify_resize_listener_event_at(
    window: &CompositorWindow,
    width: f64,
    content_height: f64,
    scale: f64,
    acknowledge_programmatic_resize: bool,
    now: Instant,
) -> ResizeListenerDisposition {
    acknowledge_programmatic_resize
        .then(|| classify_programmatic_resize_event_at(window, width, content_height, scale, now))
        .unwrap_or(ResizeListenerDisposition::UserResize)
}

/// What the installed `WindowEvent::Resized` handler must do with one native
/// event, including the aspect-lock correction. This is the ENTIRE decision
/// half of `install_aspect_lock`'s listener: the closure only performs the
/// resulting `set_size`/`settle_panel_content_geometry` side effects. Keeping
/// it here means a test can drive the real listener decision path rather than
/// the classifier alone -- the exact gap that let #416's regressions ship
/// green five times (see CLAUDE.md "Native window-lifecycle changes need a
/// live-exercising test").
#[derive(Debug, Clone, Copy, PartialEq)]
enum ResizeListenerOutcome {
    Ignored,
    Buffered {
        generation: u64,
    },
    Settled {
        content_height: f64,
        needs_correction: bool,
        settled_generation: Option<u64>,
    },
}

fn resize_listener_outcome(
    window: &CompositorWindow,
    width: f64,
    content_height: f64,
    scale: f64,
    acknowledge_programmatic_resize: bool,
) -> ResizeListenerOutcome {
    let disposition = classify_resize_listener_event(
        window,
        width,
        content_height,
        scale,
        acknowledge_programmatic_resize,
    );
    match disposition {
        ResizeListenerDisposition::IgnoreStaleProgrammatic => ResizeListenerOutcome::Ignored,
        ResizeListenerDisposition::BufferProgrammatic(transaction) => {
            ResizeListenerOutcome::Buffered {
                generation: transaction.generation,
            }
        }
        ResizeListenerDisposition::SettleProgrammatic(transaction) => {
            // The transaction was installed before `set_size`; this is the
            // matching native callback, not a user gesture. Reconcile to the
            // requested geometry exactly and never re-derive it from the old
            // placeholder panel geometry.
            ResizeListenerOutcome::Settled {
                content_height: transaction.content_height,
                needs_correction: false,
                settled_generation: Some(transaction.generation),
            }
        }
        ResizeListenerDisposition::UserResize => {
            let source_aspect = source_aspect_for_resize_event(window, width, content_height);
            let (content_height, needs_correction) =
                aspect_locked_content_height(width, content_height, source_aspect);
            ResizeListenerOutcome::Settled {
                content_height,
                needs_correction,
                settled_generation: None,
            }
        }
    }
}

/// Returns true when an in-flight request was actually cancelled, so the
/// caller can decide whether that superseded request still owes a
/// reconciliation (#416).
fn cancel_programmatic_resize(window: &CompositorWindow) -> bool {
    let mut events = window.programmatic_resize_events.lock_unpoisoned();
    // A real user gesture supersedes any unconsumed successful acknowledgement
    // before it can be mistaken for a later programmatic event.
    events.late_successful_ack = None;
    if let Some(transaction) = events.pending.take() {
        enqueue_cancelled_programmatic_resize(&mut events, transaction);
        // An initial-reveal or fit-to-source request must not be replayed at
        // pointer-up as a preserve-user-size reconciliation; that is the wrong
        // policy for both.
        return transaction.source_driven;
    }
    false
}

/// Record that a source-driven reconciliation is still owed, to be drained at
/// pointer-up. The captured dimensions are only a signal -- the drain in
/// `compositor_resize_window` re-reads canonical state -- but without it a
/// source resize cancelled by pointer-down is lost entirely, leaving the panel
/// at the user's size with the OLD source aspect (#416's border gaps).
fn latch_source_reconciliation(window: &CompositorWindow) {
    let Some((source_width_px, source_height_px)) =
        *window.canonical_source_pixel_size.lock_unpoisoned()
    else {
        return;
    };
    let (fallback_content_w, fallback_content_h) = *window.panel_content_size.lock_unpoisoned();
    *window.pending_source_resize.lock_unpoisoned() = Some(InitialResizeTarget {
        source_width_px,
        source_height_px,
        source_scale: valid_source_scale(window.source_scale),
        fallback_content_w,
        fallback_content_h,
    });
}

fn reset_programmatic_resize_events(window: &CompositorWindow) {
    *window.programmatic_resize_events.lock_unpoisoned() = ProgrammaticResizeEvents::default();
    window
        .next_programmatic_resize_generation
        .store(0, Ordering::Relaxed);
}

fn discard_programmatic_resize_if_current(window: &CompositorWindow, generation: u64) {
    let mut events = window.programmatic_resize_events.lock_unpoisoned();
    if events
        .pending
        .is_some_and(|transaction| transaction.generation == generation)
    {
        events.pending = None;
    }
}

/// Marks a successful native `set_size` request as settled when AppKit did
/// not synchronously invoke the listener. Successful requests do not enter
/// the stale-acknowledgement FIFO; that FIFO is reserved for cancellations.
fn settle_programmatic_resize_if_current(
    window: &CompositorWindow,
    generation: u64,
) -> Option<ProgrammaticResizeTransaction> {
    let mut events = window.programmatic_resize_events.lock_unpoisoned();
    let transaction = events.pending?;
    if transaction.generation != generation {
        return None;
    }
    events.pending = None;
    Some(transaction)
}

fn retain_late_successful_resize_ack(
    window: &CompositorWindow,
    content_width: f64,
    content_height: f64,
) {
    let mut events = window.programmatic_resize_events.lock_unpoisoned();
    events.late_successful_ack = Some(LateSuccessfulResizeAck {
        content_width,
        content_height,
    });
}

fn reconcile_pending_programmatic_resize_at(
    window: &CompositorWindow,
    generation: u64,
    actual_width: f64,
    actual_height: f64,
    now: Instant,
) -> Option<ProgrammaticResizeTransaction> {
    let mut events = window.programmatic_resize_events.lock_unpoisoned();
    let transaction = events.pending?;
    if transaction.generation != generation
        || events
            .cancelled_callbacks
            .iter()
            .any(|cancelled| cancelled.barrier_deadline > now)
    {
        return None;
    }
    events.pending = None;
    // Keep these actual native bounds until this callback is consumed or a
    // real user gesture/lifecycle reset invalidates them. A delayed AppKit
    // success acknowledgement must never enter the user aspect-lock path.
    events.late_successful_ack = Some(LateSuccessfulResizeAck {
        content_width: actual_width,
        content_height: actual_height,
    });
    Some(transaction)
}

fn schedule_programmatic_resize_reconciliation(
    app: &AppHandle,
    key: &RemoteWindowKey,
    transaction: ProgrammaticResizeTransaction,
) {
    let app = app.clone();
    let key = key.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(PROGRAMMATIC_RESIZE_ACK_GRACE).await;
        let label = panel_label_for_key(&key);
        let app_main = app.clone();
        let _ = app.run_on_main_thread(move || {
            let Some(panel) = app_main.get_webview_window(&label) else {
                return;
            };
            let scale = panel.scale_factor().unwrap_or(1.0);
            let Some(size) = panel.inner_size().ok() else {
                return;
            };
            let width = size.width as f64 / scale;
            let content_height = (size.height as f64 / scale - HEADER_HEIGHT).max(1.0);
            let settled = with_state(|s| {
                s.windows.get(&key).and_then(|state| {
                    reconcile_pending_programmatic_resize_at(
                        state,
                        transaction.generation,
                        width,
                        content_height,
                        Instant::now(),
                    )
                })
            });
            if settled.is_some() {
                settle_panel_content_geometry(&app_main, &key, width, content_height, scale, false);
            }
        });
    });
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteWindowSummary {
    window_id: u32,
    owner_identity: String,
    owner_display_name: String,
    source_title: String,
    hidden: bool,
}

/// Test-only compositor binding used by the privileged native-to-native
/// cockpit.  It makes the proof chain explicit: authenticated LiveKit owner +
/// source window id -> our panel label -> the exact WindowServer window id
/// carrying decoded content.  This is `pub(crate)` rather than a command, so
/// production web content cannot enumerate or move remote windows.
#[cfg(feature = "cockpit-privileged")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CockpitRemoteWindowBinding {
    pub owner_identity: String,
    pub source_window_id: u32,
    pub panel_label: String,
    pub cg_window_id: u32,
    pub frame: crate::platform::cg::WindowFrame,
    pub frames_received: u64,
    pub frames_display_enqueued: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompositorResizeFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteWindowDebugStats {
    window_id: u32,
    owner_identity: String,
    owner_display_name: String,
    source_title: String,
    source_url: Option<String>,
    content_width: f64,
    content_height: f64,
    receiver_scale: f64,
    display_pixel_width: u32,
    display_pixel_height: u32,
    source_pixel_width: Option<u32>,
    source_pixel_height: Option<u32>,
    last_frame_received_ms: Option<u64>,
    frames_received: u64,
    last_display_enqueued_ms: Option<u64>,
    frames_display_enqueued: u64,
    remote_control_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DisplayEnqueueSnapshot {
    pub source_pixel_width: Option<u32>,
    pub source_pixel_height: Option<u32>,
    pub last_display_enqueued_ms: Option<u64>,
    pub frames_display_enqueued: u64,
    pub frames_received: u64,
}

struct PendingDisplaySample {
    sample: OwnedCMSampleBuffer,
    source_width: u32,
    source_height: u32,
}

#[derive(Debug)]
struct PendingFrameQueue<T> {
    samples: VecDeque<T>,
    scheduled: bool,
}

impl<T> Default for PendingFrameQueue<T> {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
            scheduled: false,
        }
    }
}

impl<T> PendingFrameQueue<T> {
    fn push(&mut self, sample: T) -> bool {
        while self.samples.len() >= MAX_PENDING_DISPLAY_SAMPLES_PER_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        if self.scheduled {
            false
        } else {
            self.scheduled = true;
            true
        }
    }

    fn drain_scheduled(&mut self) -> Vec<T> {
        self.scheduled = false;
        self.samples.drain(..).collect()
    }

    fn clear(&mut self) {
        self.scheduled = false;
        self.samples.clear();
    }
}

fn record_display_enqueue(window: &CompositorWindow, enqueued_at_ms: u64) {
    window
        .last_display_enqueued_ms
        .store(enqueued_at_ms, Ordering::Relaxed);
    window
        .frames_display_enqueued
        .fetch_add(1, Ordering::Relaxed);
}

/// Whether the compositor's `AVSampleBufferDisplayLayer` enqueue path is
/// currently paused system-wide (#259/#264 display-sleep defensive fix).
/// `Active` is the steady state; `Paused` covers the window between a
/// `screensDidSleep` notification and the matching `screensDidWake` --
/// continuing to call `enqueueSampleBuffer:` on a display the OS has
/// confirmed asleep is the plausible trigger for a real user's WindowServer
/// watchdog kill (see CLAUDE.md's display-sleep crash class). A pure enum +
/// transition functions so the state machine is unit-testable without any
/// real `NSWorkspace` notification or AppKit call -- see `resilience.rs`'s
/// `screensDidSleep`/`screensDidWake` observers for the actual caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayEnqueueGate {
    Active,
    Paused,
}

impl DisplayEnqueueGate {
    /// New state after a `screensDidSleep` notification, plus whether this
    /// is a genuine transition INTO paused (`false` if already paused -- a
    /// second sleep notification in a row must not re-clear pending sample
    /// bookkeeping or re-log a transition that already happened).
    fn on_sleep(self) -> (Self, bool) {
        (Self::Paused, self != Self::Paused)
    }

    /// New state after a `screensDidWake` notification, plus whether this is
    /// a genuine transition INTO active (`false` if already active).
    fn on_wake(self) -> (Self, bool) {
        (Self::Active, self != Self::Active)
    }

    fn is_paused(self) -> bool {
        matches!(self, Self::Paused)
    }
}

static DISPLAY_ENQUEUE_GATE: Mutex<DisplayEnqueueGate> = Mutex::new(DisplayEnqueueGate::Active);

/// Independent enqueue-pause reason for #878's drop-rate backoff (Phase 2
/// item 2) -- deliberately a SEPARATE flag from `DISPLAY_ENQUEUE_GATE`, not
/// a third enum state, so the two reasons can never fight: `screensDidSleep`
/// must always win over a backoff resume. `display_enqueue_paused` ORs both;
/// `set_display_enqueue_backoff_paused` never touches `DISPLAY_ENQUEUE_GATE`.
static DISPLAY_ENQUEUE_BACKOFF_PAUSED: AtomicBool = AtomicBool::new(false);

/// Read-only check used by `push_frame` to decide whether to build/enqueue a
/// sample at all this call.
fn display_enqueue_paused() -> bool {
    DISPLAY_ENQUEUE_GATE.lock_unpoisoned().is_paused()
        || DISPLAY_ENQUEUE_BACKOFF_PAUSED.load(Ordering::Relaxed)
}

/// Pause/resume ALL compositor display-layer enqueue in response to a real
/// `screensDidSleep`/`screensDidWake` `NSWorkspace` notification (#259/#264,
/// called from `resilience.rs`). Idempotent -- a duplicate notification for
/// the state we're already in is a cheap no-op (logged at debug), matching
/// the DoD's "log markers for pause/resume transitions... grep-able in
/// petal.log" ask for the transitions that actually happen.
///
/// Pausing only stops handing decoded frames to
/// `AVSampleBufferDisplayLayer.enqueueSampleBuffer:` (`push_frame` below
/// skips building the `CMSampleBuffer` entirely while paused) -- it does NOT
/// touch `transport::subscriber::start_compositor_feed`'s LiveKit decode
/// loop, which keeps consuming and decoding every incoming frame the whole
/// time. That is deliberate: the local H.264 decoder's reference-frame chain
/// is never interrupted, so there is no stale-decoder-state gap to paper
/// over with a keyframe request on resume -- and confirmed there is no
/// public LiveKit 0.7 force-keyframe/PLI API to make that request with
/// anyway (see `session/share.rs`'s pump-decision log line referencing
/// #182). The very next frame decoded after `screensDidWake` is fresh, live
/// video, not a stale one replayed from before sleep.
/// True while the #259/#264 sleep gate is pausing display enqueue. The
/// drop-rate backoff sampler (diagnostics.rs, #878) must NOT accumulate
/// evidence while this is true: a sleeping display makes every track read
/// 100% drop by construction, which is the sleep gate doing its job, not
/// display-layer distress (adversarial-review finding 1 on the #878 batch).
pub(crate) fn display_enqueue_sleep_paused() -> bool {
    matches!(*DISPLAY_ENQUEUE_GATE.lock_unpoisoned(), DisplayEnqueueGate::Paused)
}

pub fn set_display_enqueue_paused(paused: bool) {
    let transitioned = {
        let mut gate = DISPLAY_ENQUEUE_GATE.lock_unpoisoned();
        let (next, transitioned) = if paused {
            gate.on_sleep()
        } else {
            gate.on_wake()
        };
        *gate = next;
        transitioned
    };
    if !transitioned {
        log::debug!(
            "compositor: display-enqueue {} notification ignored (already in that state)",
            if paused { "pause" } else { "resume" }
        );
        return;
    }
    if paused {
        // Clear every open + retired window's pending sample queue so a
        // resume never flushes a burst of stale frames queued right before
        // the display slept.
        with_state(|s| {
            for win in s.windows.values_mut().chain(s.retired.values_mut()) {
                win.pending_display_samples.clear();
            }
        });
        log::info!(
            "compositor: display-layer enqueue PAUSED (screensDidSleep) -- suppressing \
             AVSampleBufferDisplayLayer.enqueueSampleBuffer: while the display is asleep"
        );
    } else {
        log::info!("compositor: display-layer enqueue RESUMED (screensDidWake)");
        // A backoff pause tripped DURING sleep is an artifact of the sleep
        // gate feeding the sampler 100% drop; left set, it is self-sustaining
        // (paused -> nothing enqueues -> drop stays 100%) and freezes every
        // remote window for up to the 30s failsafe after wake
        // (adversarial-review finding 1, #878). Wake clears it.
        set_display_enqueue_backoff_paused(false);
    }
}

/// Pause/resume compositor display-layer enqueue in response to
/// `diagnostics.rs`'s sustained display-enqueue-drop backoff (#878 Phase 2
/// item 2) -- Petal's contribution to window-server load reduction when the
/// receive side is dropping most enqueued frames anyway. Deliberately
/// independent of `set_display_enqueue_paused` (the #259/#264 sleep gate):
/// a `screensDidSleep` pause must never be undone by a backoff resume, and
/// this function never reads or writes `DISPLAY_ENQUEUE_GATE`. Same
/// enqueue-only contract as the sleep gate -- the LiveKit decode loop and
/// last-good-frame hold are untouched; this never causes a black frame.
///
/// UNVERIFIED LIVE: this pause path is unit-tested for its decision logic
/// and its precedence against the sleep gate, but the rendered-pixel proof
/// (`scripts/verify-no-black-frame-native.sh` with backoff actually
/// engaged) has NOT been run -- deferred to #870's consolidated live pass
/// per the #878 task brief. Do not treat the unit tests as equivalent to
/// that live check.
///
/// Read by `diagnostics.rs`'s per-pass artifact guard (#882 review): while
/// any track holds this pause, every track's drop window is the pause's own
/// artifact and must not accumulate toward a new pause.
pub(crate) fn display_enqueue_backoff_paused() -> bool {
    DISPLAY_ENQUEUE_BACKOFF_PAUSED.load(Ordering::Relaxed)
}

pub(crate) fn set_display_enqueue_backoff_paused(paused: bool) {
    let previous = DISPLAY_ENQUEUE_BACKOFF_PAUSED.swap(paused, Ordering::Relaxed);
    if previous == paused {
        log::debug!(
            "compositor: display-enqueue backoff {} notification ignored (already in that state)",
            if paused { "pause" } else { "resume" }
        );
        return;
    }
    if paused {
        log::warn!(
            "compositor: display-layer enqueue PAUSED (drop-rate backoff, #878) -- sustained \
             high display-enqueue drop rate; suppressing AVSampleBufferDisplayLayer.\
             enqueueSampleBuffer: for the time-boxed backoff window (recovery is unobservable \
             while paused -- the metric measures the pause itself)"
        );
    } else {
        log::info!("compositor: display-layer enqueue RESUMED (drop-rate backoff cleared, #878)");
    }
}

struct CompositorState {
    windows: HashMap<RemoteWindowKey, CompositorWindow>,
    /// #901: when each remote window was last auto-raised on reveal. A new
    /// share must come to the front so it is discoverable, but #840 means a
    /// live window is hidden and re-revealed on EVERY sharer republish -- and
    /// #841's storm made that ~3x/second in the field. Debouncing on this
    /// timestamp is what separates "a share just appeared" from "the same
    /// share is churning": a genuine re-share seconds later still raises,
    /// republish churn does not. Survives retire/reveal because it lives on
    /// the state map, not on the (pooled, reused) window.
    last_auto_raised: HashMap<RemoteWindowKey, std::time::Instant>,
    remote_control_active: HashSet<RemoteWindowKey>,
    /// Windows whose share ended: HIDDEN, not destroyed, and kept here for
    /// reuse if the same window_id is shared again. Destroying these panels
    /// (any variant of `close()`) reproducibly ABORTED the whole app a few
    /// seconds later — an Objective-C exception thrown during deferred
    /// dealloc of the panel + its native AVSampleBufferDisplayLayer subview,
    /// unwinding through tao's run-loop observer as a Rust foreign exception
    /// (SIGABRT). Bisected live: happened with and without chrome children,
    /// with releasedWhenClosed=NO, with children-closed-first, and with the
    /// display resources dropped on the main thread — so destruction itself
    /// is the hazard, and we simply never destroy during a session (bounded
    /// cost: one hidden panel per distinct shared window per session; same
    /// reuse pattern takt's global highlight panel already relies on).
    retired: HashMap<RemoteWindowKey, CompositorWindow>,
    retired_order: Vec<RemoteWindowKey>,
    /// Next cascade slot to hand out (wraps at `CASCADE_WRAP`) -- NOT the
    /// count of currently-open windows, so closing and reopening windows
    /// keeps advancing the cascade rather than reusing the same spot
    /// (matches "no position memory": position is a pure function of
    /// creation order, never read back from a closed window).
    next_cascade_slot: u32,
    /// #679: keys whose most recent teardown was transport-side (a
    /// reconnect, a stalled receiver, or a deliberate manual hide) rather
    /// than a genuine sharer-side end. A subsequent `TrackSubscribed` for one
    /// of these keys is a re-subscribe, not a new share, so the
    /// "<Name> is sharing a window" pill must stay silent for it -- see
    /// `consume_share_started_pill_suppression`'s doc comment for the full
    /// reasoning (a naive "is this window currently open" gate is NOT
    /// sufficient; #631's reconnect case is exactly what this exists to
    /// catch).
    suppressed_reshare_pill: HashSet<RemoteWindowKey>,
}

static STATE: Mutex<Option<CompositorState>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut CompositorState) -> R) -> R {
    let mut guard = STATE.lock_unpoisoned();
    let state = guard.get_or_insert_with(|| CompositorState {
        windows: HashMap::new(),
        last_auto_raised: HashMap::new(),
        remote_control_active: HashSet::new(),
        retired: HashMap::new(),
        retired_order: Vec::new(),
        next_cascade_slot: 0,
        suppressed_reshare_pill: HashSet::new(),
    });
    f(state)
}

fn resolve_window_key(window_id: u32, owner_identity: Option<&str>) -> Option<RemoteWindowKey> {
    with_state(|s| {
        if let Some(owner_identity) = owner_identity {
            let key = RemoteWindowKey::new(owner_identity, window_id);
            return (s.windows.contains_key(&key) || s.retired.contains_key(&key)).then_some(key);
        }

        let mut matches = s
            .windows
            .keys()
            .chain(s.retired.keys())
            .filter(|key| key.window_id == window_id)
            .cloned();
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    })
}

fn resolve_open_window_key(
    window_id: u32,
    owner_identity: Option<&str>,
) -> Option<RemoteWindowKey> {
    with_state(|s| {
        if let Some(owner_identity) = owner_identity {
            let key = RemoteWindowKey::new(owner_identity, window_id);
            return s.windows.contains_key(&key).then_some(key);
        }

        let mut matches = s
            .windows
            .keys()
            .filter(|key| key.window_id == window_id)
            .cloned();
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    })
}

/// Where the FIRST compositor window is placed, in the global logical-point
/// coordinate space Tauri's `Position::Logical` already uses elsewhere in
/// this codebase (`hover_tab.rs`/`share_border.rs`). SPEC.md §4.4 says
/// "cascaded from the top-left corner of the desktop" -- but placing the very
/// first window at the literal corner (0,0) is a bug: its draggable header
/// strip ends up *underneath the macOS menu bar* (which owns the top ~25pt and
/// draws on top), so the window can't be grabbed to move it, and it reads as a
/// stuck black rectangle jammed into the corner. So the origin is inset from
/// the corner by ~8% of the display (with a floor that always clears the menu
/// bar), giving the "offset ~10% from the top-left" placement a user expects
/// while still cascading from near the top-left as the spec intends.
fn desktop_origin(app: &AppHandle) -> (f64, f64) {
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let scale = monitor.scale_factor();
        let mx = monitor.position().x as f64 / scale;
        let my = monitor.position().y as f64 / scale;
        let mw = monitor.size().width as f64 / scale;
        let mh = monitor.size().height as f64 / scale;
        // ~8% inset horizontally; vertically the same but never less than 44pt
        // so the header always clears the menu bar (~25pt) with margin to spare.
        let inset_x = (mw * 0.08).round();
        let inset_y = (mh * 0.08).max(44.0).round();
        return (mx + inset_x, my + inset_y);
    }
    // No monitor info: a fixed inset that still clears the menu bar.
    (80.0, 60.0)
}

/// Pure cascade-position math, unit-tested below without needing a real
/// `AppHandle`/monitor.
fn cascade_position(origin: (f64, f64), slot: u32) -> (f64, f64) {
    let wrapped = (slot % CASCADE_WRAP) as f64;
    (
        origin.0 + wrapped * CASCADE_STEP,
        origin.1 + wrapped * CASCADE_STEP,
    )
}

fn initial_content_size_within_work_area(
    source_width_px: u32,
    source_height_px: u32,
    source_scale: f64,
    receiver_scale: f64,
    work_area_w: f64,
    work_area_h: f64,
) -> (f64, f64) {
    let (source_w, source_h) =
        source_presentation_size_points(source_width_px, source_height_px, source_scale);
    if source_w <= 0.0 || source_h <= 0.0 || work_area_w <= 0.0 || work_area_h <= 0.0 {
        return (source_w.max(1.0), source_h.max(1.0));
    }

    let max_w = work_area_w * INITIAL_MAX_WORK_AREA_FRACTION;
    let max_content_h = (work_area_h * INITIAL_MAX_WORK_AREA_FRACTION - HEADER_HEIGHT).max(1.0);
    if let Some(snapped) = largest_nearest_integer_content_size(
        source_width_px,
        source_height_px,
        receiver_scale,
        max_w,
        max_content_h,
    ) {
        return (snapped.width, snapped.height);
    }

    fractional_content_size_within_bounds(source_w, source_h, max_w, max_content_h)
}

fn fit_to_source_content_size_within_work_area(
    source_width_px: u32,
    source_height_px: u32,
    source_scale: f64,
    receiver_scale: f64,
    work_area_w: f64,
    work_area_h: f64,
) -> (f64, f64) {
    let (source_w, source_h) =
        source_presentation_size_points(source_width_px, source_height_px, source_scale);
    if source_w <= 0.0 || source_h <= 0.0 || work_area_w <= 0.0 || work_area_h <= 0.0 {
        return (source_w.max(1.0), source_h.max(1.0));
    }

    let max_content_h = (work_area_h - HEADER_HEIGHT).max(1.0);
    if let Some(snapped) = largest_nearest_integer_content_size(
        source_width_px,
        source_height_px,
        receiver_scale,
        work_area_w,
        max_content_h,
    ) {
        return (snapped.width, snapped.height);
    }

    fractional_content_size_within_bounds(source_w, source_h, work_area_w, max_content_h)
}

fn source_presentation_size_points(
    source_width_px: u32,
    source_height_px: u32,
    source_scale: f64,
) -> (f64, f64) {
    let scale = valid_source_scale(source_scale);
    (
        (source_width_px as f64 / scale).round().max(1.0),
        (source_height_px as f64 / scale).round().max(1.0),
    )
}

fn valid_source_scale(source_scale: f64) -> f64 {
    if source_scale.is_finite() && source_scale > 0.0 {
        source_scale
    } else {
        1.0
    }
}

fn fractional_content_size_within_bounds(
    source_w: f64,
    source_h: f64,
    max_w: f64,
    max_content_h: f64,
) -> (f64, f64) {
    let factor = (max_w / source_w).min(max_content_h / source_h).min(1.0);
    ((source_w * factor).round(), (source_h * factor).round())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct IntegerContentSize {
    width: f64,
    height: f64,
}

fn integer_content_size_for_scale(
    source_width_px: u32,
    source_height_px: u32,
    receiver_scale: f64,
    scale: u32,
) -> Option<IntegerContentSize> {
    if source_width_px == 0
        || source_height_px == 0
        || !receiver_scale.is_finite()
        || receiver_scale <= 0.0
        || !(1..=MAX_NEAREST_INTEGER_SCALE).contains(&scale)
    {
        return None;
    }
    Some(IntegerContentSize {
        width: source_width_px as f64 * scale as f64 / receiver_scale,
        height: source_height_px as f64 * scale as f64 / receiver_scale,
    })
}

fn largest_nearest_integer_content_size(
    source_width_px: u32,
    source_height_px: u32,
    receiver_scale: f64,
    max_w: f64,
    max_h: f64,
) -> Option<IntegerContentSize> {
    if !max_w.is_finite() || !max_h.is_finite() || max_w <= 0.0 || max_h <= 0.0 {
        return None;
    }
    (1..=MAX_NEAREST_INTEGER_SCALE)
        .rev()
        .filter_map(|scale| {
            integer_content_size_for_scale(source_width_px, source_height_px, receiver_scale, scale)
        })
        .find(|size| size.width <= max_w + 0.5 && size.height <= max_h + 0.5)
}

fn snap_content_size_to_nearest_integer_scale(
    source_width_px: u32,
    source_height_px: u32,
    receiver_scale: f64,
    content_w: f64,
    content_h: f64,
) -> Option<IntegerContentSize> {
    if source_width_px == 0
        || source_height_px == 0
        || !receiver_scale.is_finite()
        || receiver_scale <= 0.0
        || !content_w.is_finite()
        || !content_h.is_finite()
        || content_w <= 0.0
        || content_h <= 0.0
    {
        return None;
    }

    let ratio_w = content_w * receiver_scale / source_width_px as f64;
    let ratio_h = content_h * receiver_scale / source_height_px as f64;
    let candidate = ((ratio_w + ratio_h) / 2.0).round();
    if !(1.0..=MAX_NEAREST_INTEGER_SCALE as f64).contains(&candidate) {
        return None;
    }
    // The display filter requires both axes to resolve to the same integer
    // scale. A badly skewed resize must therefore remain untouched rather
    // than snapping to a geometry that the renderer will still classify as
    // linear. Normal aspect-locked drags are effectively exact here; this
    // guard is for stale/rounded resize inputs.
    if (ratio_w - ratio_h).abs() > RESIZE_INTEGER_SNAP_THRESHOLD_RATIO * candidate {
        return None;
    }
    let scale = candidate as u32;
    let max_error = ((ratio_w - candidate).abs()).max((ratio_h - candidate).abs());
    if max_error > RESIZE_INTEGER_SNAP_THRESHOLD_RATIO * candidate {
        return None;
    }
    integer_content_size_for_scale(source_width_px, source_height_px, receiver_scale, scale)
}

fn control_overlay_ignore_cursor_events(_draw_active: bool, _remote_control_active: bool) -> bool {
    // Issue #142: control and draw capture share the existing control route.
    // Keep it cursor-interactive even in View mode so its resize handles keep
    // working; the route gates which input streams it forwards.
    false
}

fn app_origin_from_url(url: &tauri::Url) -> Option<String> {
    if url.scheme() == "about" {
        return None;
    }
    let mut origin = format!("{}://", url.scheme());
    if let Some(host) = url.host_str() {
        origin.push_str(host);
    } else {
        return None;
    }
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    origin.push('/');
    Some(origin)
}

fn app_url_from_origin(origin: &str, route: &str) -> Option<tauri::Url> {
    tauri::Url::parse(origin).ok()?.join(route).ok()
}

fn blank_url() -> tauri::Url {
    tauri::Url::parse("about:blank").expect("static about:blank URL is valid")
}

fn show_retired_window_on_main(
    app: &AppHandle,
    key: &RemoteWindowKey,
    win_state: &mut CompositorWindow,
    passive_anchor: Option<i64>,
    reason: &str,
    reveal: bool,
) {
    let window_id = key.window_id;
    log::info!("compositor: restoring hidden remote window {window_id} from {reason}");
    let panel_label = panel_label_for_key(key);
    let control_label = control_label_for_key(key);
    let pointer_label = pointer_label_for_key(key);
    let ai_chat_label = ai_chat_label_for_key(key);
    // #844: unlike control/pointer/panel, the ai-chat overlay's visibility is
    // NOT tied to `reveal` alone -- it must also come back only if the user
    // had it disclosed. Read once, before the loop below, since the loop
    // holds `win_state` inside an `objc2::exception::catch` closure per
    // label and this flag is the same for every iteration.
    let ai_chat_overlay_open = win_state.ai_chat_overlay_open;
    if win_state.display.is_none() {
        let display = DisplayLayer::new();
        // Fallback only: the wrapper measures the live panel frame first and
        // uses this remembered size solely when the frame is unreadable —
        // `panel_content_size` is stale for a window resized while retired
        // (adversarial-review finding).
        let (content_w, content_h) = *win_state.panel_content_size.lock_unpoisoned();
        attach_display_layer(
            app,
            &panel_label,
            window_id,
            &display,
            content_w.max(1.0),
            content_h.max(1.0),
        );
        win_state.display = Some(display);
        log::info!("compositor: rehydrated stripped display layer for window {window_id}");
    }

    // ai-chat is processed BEFORE panel deliberately: the panel iteration
    // below is what calls `order_chrome_above_panel` (once, in the reveal
    // gate), and that call only re-orders chrome that is ALREADY visible at
    // the time it runs. Settling ai-chat's own show/hide first means that one
    // call correctly covers it too, instead of needing a second copy of the
    // ordering call (which #840's flicker-guard test explicitly forbids --
    // see remoteWindowFlickerGuards.test.ts).
    for label in [
        control_label.clone(),
        pointer_label.clone(),
        ai_chat_label.clone(),
        panel_label.clone(),
    ] {
        if let Some(win) = app.get_webview_window(&label) {
            let result = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                if label == ai_chat_label {
                    // No per-instance metadata to refresh (windowId/owner are
                    // fixed for this key's whole lifetime, reuse included) --
                    // the only reason to navigate here is to un-blank a
                    // webview `strip_retired_window_for_pool` sent to
                    // `about:blank` while evicted from the warm pool.
                    if let Some(origin) = win_state.app_origin.as_deref() {
                        let route = ai_chat_route_url(window_id, &win_state.owner_identity);
                        if let Some(url) = app_url_from_origin(origin, &route) {
                            let _ = win.navigate(url);
                        }
                    }
                }
                if label == control_label {
                    let (source_width, source_height) =
                        control_source_dimensions(*win_state.source_pixel_size.lock_unpoisoned());
                    if let Some(origin) = win_state.app_origin.as_deref() {
                        if let Some(url) = app_url_from_origin(
                            origin,
                            &control_route_url(
                                window_id,
                                &win_state.owner_identity,
                                source_width,
                                source_height,
                            ),
                        ) {
                            let _ = win.navigate(url);
                        }
                    } else {
                        refresh_control_webview(
                            &win,
                            window_id,
                            &win_state.owner_identity,
                            source_width,
                            source_height,
                        );
                    }
                    let _ = win.set_ignore_cursor_events(false);
                }
                if label == pointer_label {
                    if let Some(origin) = win_state.app_origin.as_deref() {
                        let route = format!("compositor/pointer.html?windowId={window_id}");
                        if let Some(url) = app_url_from_origin(origin, &route) {
                            let _ = win.navigate(url);
                        }
                    }
                    let _ = win.set_ignore_cursor_events(true);
                }
                if label == panel_label {
                    set_remote_window_min_size(&win);
                    // Panel webview owns the header strip. Refresh metadata,
                    // then force WKWebView clear before reveal so transparent
                    // page pixels cannot flash white/opaque backing (#151).
                    let route = surface_route_url(
                        window_id,
                        &win_state.owner_identity,
                        &win_state.owner_display_name,
                        &win_state.source_title,
                        win_state.source_url.as_deref(),
                        win_state.remote_control_available,
                        win_state.remote_control_disallowed,
                        win_state.owner_palette_index,
                    );
                    if let Some(origin) = win_state.app_origin.as_deref() {
                        if let Some(url) = app_url_from_origin(origin, &route) {
                            let _ = win.navigate(url);
                        }
                    } else {
                        refresh_header_webview(
                            &win,
                            &header_query_string(
                                window_id,
                                &win_state.owner_identity,
                                &win_state.owner_display_name,
                                &win_state.source_title,
                                win_state.source_url.as_deref(),
                                win_state.remote_control_available,
                                win_state.remote_control_disallowed,
                                win_state.owner_palette_index,
                            ),
                        );
                    }
                    crate::webview_transparency::apply_or_retry(app, &win);
                    apply_remote_window_border(
                        &win,
                        owner_border_color_hex(
                            &win_state.owner_identity,
                            &win_state.owner_display_name,
                            win_state.owner_palette_index,
                        ),
                    );
                }
                // #844: the ai-chat overlay only comes back when BOTH this
                // window is being revealed AND the user had it disclosed --
                // every other chrome window follows `reveal` alone.
                let should_show = if label == ai_chat_label {
                    reveal && ai_chat_overlay_open
                } else {
                    reveal
                };
                if should_show {
                    let _ = win.show();
                } else {
                    let _ = win.hide();
                }
                // #844 review: keep the addChildWindow attachment in sync
                // with should_show, same reasoning as
                // compositor_set_ai_chat_overlay_open -- attach only once
                // actually shown, detach is safe unconditionally.
                if label == ai_chat_label {
                    if should_show {
                        attach_ai_chat_overlay(app, key);
                    } else {
                        detach_ai_chat_overlay(app, key);
                    }
                }
                // Never order a panel this call just hid: `orderWindow:
                // relativeTo:` re-inserts it into the screen list (un-hides
                // it), which showed unrevealed reuse windows and closed the
                // #840 flicker loop. First-frame reveal re-orders on its own.
                if label == panel_label && reveal {
                    order_below_anchor(&win, passive_anchor);
                    order_chrome_above_panel(app, key);
                }
            }));
            if let Err(exception) = result {
                log::error!(
                    "compositor: NSException while restoring '{label}' (caught): {exception:?}"
                );
            }
        }
    }
    win_state.stripped_for_pool = false;
    if reveal {
        // Showing hidden AppKit child windows can resurrect their stale
        // addChildWindow follow-offset; re-dock after reveal (#171).
        sync_chrome_to_panel_frame_deferred(app, key);
    }
}

fn hide_remote_window_chrome_on_main(app: &AppHandle, key: &RemoteWindowKey) {
    // #844 review note: today the only caller is `ensure_window`'s fresh
    // create, where the overlay has never been shown or attached -- but an
    // attach-on-show child hidden WITHOUT detaching would be silently
    // re-revealed by its parent's next show, bypassing the disclosure flag.
    // Detach unconditionally (a documented AppKit no-op when not attached)
    // so any future caller of this hide path stays safe by construction.
    detach_ai_chat_overlay(app, key);
    for label in [
        control_label_for_key(key),
        pointer_label_for_key(key),
        ai_chat_label_for_key(key),
        panel_label_for_key(key),
    ] {
        if let Some(win) = app.get_webview_window(&label) {
            let _ = win.hide();
        }
    }
}

fn reveal_remote_window_after_first_frame_on_main(app: &AppHandle, key: &RemoteWindowKey) {
    let window_id = key.window_id;
    // #901: a newly shared window must come to the FRONT, or the receiver
    // cannot find it (owner report: "difficult to discover them"). Debounced
    // per window so #840's hide+re-reveal on every sharer republish does not
    // turn into a raise storm -- see `auto_raise_on_reveal_due`.
    let now = std::time::Instant::now();
    let raise_to_front = with_state(|s| {
        let due = auto_raise_on_reveal_due(s.last_auto_raised.get(key).copied(), now);
        if due {
            s.last_auto_raised.insert(key.clone(), now);
        }
        due
    });
    // Only consulted on the passive path; a raising reveal must not order
    // itself below whatever happened to be frontmost.
    let passive_anchor = crate::window_diag::frontmost_normal_window_number();
    let panel_label = panel_label_for_key(key);
    let ai_chat_label = ai_chat_label_for_key(key);
    // #844: this reveal path also runs for a retired-window REUSE's first
    // frame (`ensure_window`'s `reused` branch resets `revealed_first_frame`
    // to `false` so it fires again there too) -- respect the same
    // disclosure flag `show_retired_window_on_main` does, rather than
    // assuming it can only ever be false by the time this runs.
    let ai_chat_overlay_open = with_state(|s| {
        s.windows
            .get(key)
            .map(|w| w.ai_chat_overlay_open)
            .unwrap_or(false)
    });
    for label in [
        panel_label.clone(),
        control_label_for_key(key),
        pointer_label_for_key(key),
    ] {
        if let Some(win) = app.get_webview_window(&label) {
            let result = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                if label == panel_label {
                    crate::webview_transparency::apply_or_retry(app, &win);
                }
                let _ = win.show();
                if label == control_label_for_key(key) {
                    let _ = win.set_ignore_cursor_events(false);
                }
                if label == panel_label {
                    if raise_to_front {
                        // Level-bump raise: the only form that reliably clears
                        // OTHER apps' windows for a non-activating panel (see
                        // `platform::appkit::raise_via_level_bump`). Never keys
                        // the panel or activates the app -- #677/#21 are prior
                        // art for a raise that steals focus being its own bug.
                        if let Err(e) = crate::platform::appkit::raise_panel_to_front(&win) {
                            log::warn!(
                                "compositor: could not raise revealed window {window_id} to front: {e}"
                            );
                        } else {
                            log::info!(
                                "compositor: raised newly revealed remote window {window_id} to front (#901)"
                            );
                        }
                    } else {
                        order_below_anchor(&win, passive_anchor);
                    }
                    order_chrome_above_panel(app, key);
                }
            }));
            if let Err(exception) = result {
                log::error!(
                    "compositor: NSException while revealing '{label}' after first frame (caught): {exception:?}"
                );
            }
        }
    }
    if let Some(win) = app.get_webview_window(&ai_chat_label) {
        let result = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            if ai_chat_overlay_open {
                let _ = win.show();
            } else {
                let _ = win.hide();
            }
            // #844 review: keep addChildWindow attachment in sync with
            // visibility here too -- same reasoning as
            // compositor_set_ai_chat_overlay_open/show_retired_window_on_main.
            if ai_chat_overlay_open {
                attach_ai_chat_overlay(app, key);
            } else {
                detach_ai_chat_overlay(app, key);
            }
        }));
        if let Err(exception) = result {
            log::error!(
                "compositor: NSException while revealing ai-chat overlay '{ai_chat_label}' after first frame (caught): {exception:?}"
            );
        }
    }
    if ai_chat_overlay_open {
        order_chrome_above_panel(app, key);
    }
    // Authoritatively re-dock the click-through overlay children (control +
    // pointer) to the panel's settled frame, deferred to the next main-thread
    // turn. This covers BOTH paths: a fresh first-frame reveal (belt-and-
    // suspenders with the aspect-lock handler's own deferred sync) and a
    // retired-window REUSE reveal (which otherwise never repositions the
    // overlays, leaving them wherever the prior session left them). Deferred
    // because a child's set_position doesn't stick synchronously inside show/
    // order — AppKit reasserts the child follow-offset first.
    sync_chrome_to_panel_frame_deferred(app, key);
    // #299: the reveal is the first moment this window has BOTH a real source
    // size and a genuinely visible panel, so it is the first moment demand can
    // be stated accurately. Without this, the pre-first-frame demand stands
    // until the next 2s heartbeat, which is up to two seconds of a receiver
    // sitting on a layer chosen before anything was known about the source.
    crate::viewer_demand::publish_window_open(app, window_id);
    log::info!("compositor: revealed remote window {window_id} after first decoded frame");
}

fn work_area_size_for_window(app: &AppHandle, window: &tauri::WebviewWindow) -> Option<(f64, f64)> {
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let scale = monitor.scale_factor();
    let work_area = monitor.work_area();
    Some((
        work_area.size.width as f64 / scale,
        work_area.size.height as f64 / scale,
    ))
}

tauri_panel! {
    panel!(RemoteWindowPanel {
        config: {
            can_become_main_window: true,
            can_become_key_window: true,
            is_floating_panel: true
        }
    })
}

/// Create (if not already open) a compositor window for `window_id`, owned
/// by `owner_identity`, with `source_title` shown in its header. Idempotent:
/// a second call for an already-open `window_id` is a no-op (the window
/// keeps its current position/size -- SPEC.md's "no position memory" means
/// nothing is *persisted* across app restarts, not that an already-placed
/// live window should jump on a redundant ensure call).
pub fn ensure_window(
    app: &AppHandle,
    window_id: u32,
    owner_identity: &str,
    owner_display_name: &str,
    source_title: &str,
    source_url: Option<String>,
    source_kind: SharedSourceKind,
    share_instance_id: Option<String>,
    source_scale: f64,
    remote_control_available: bool,
    remote_control_disallowed: bool,
    owner_palette_index: Option<u8>,
    canonical_source_size: Option<(u32, u32)>,
) {
    let owner_identity = owner_identity.to_string();
    let owner_palette_index =
        owner_palette_index.filter(|index| (*index as usize) < OWNER_COLOR_PALETTE_HEX.len());
    let share_instance_id = share_instance_id.filter(|value| !value.is_empty());
    let key = RemoteWindowKey::new(owner_identity.clone(), window_id);
    let already_open = with_state(|s| s.windows.contains_key(&key));
    if already_open {
        // A republish under the same window_id (quality-switch unpublish+
        // republish, or a GENUINE sender-side resize -- session/share.rs's
        // republish_window_for_resize) fires a fresh TrackSubscribed while
        // this window is already open. Nothing else ever refreshes
        // canonical_source_pixel_size after window creation, so without this
        // call a real mid-session resize would be silently never applied
        // (#416 review finding -- this is not cosmetic, the whole point of
        // this issue is that a genuine resize must still take effect).
        with_state(|s| {
            if let Some(window) = s.windows.get_mut(&key) {
                window.source_kind = source_kind;
                window.share_instance_id = share_instance_id.clone();
            }
        });
        update_canonical_source_size_on_republish(app, &key, canonical_source_size);
        return;
    }

    // All the work below creates NSPanels / attaches a CoreAnimation layer /
    // spawns child webviews — AppKit work that MUST run on the main thread.
    // This function is reached from `start_compositor_feed`'s RoomEvent loop
    // (a background thread), so marshal the whole body to the main thread (same
    // fix as `share_border::show_share_border`; building AppKit windows off the
    // main thread traps with "Must only be used from the main thread"). Owned
    // copies of the borrowed args are moved into the closure.
    let app_main = app.clone();
    let owner_identity = owner_identity.to_string();
    let owner_display_name = owner_display_name.to_string();
    let source_title = source_title.to_string();
    let source_url = source_url.filter(|u| crate::browser_url::is_openable_url(u));
    let source_scale = valid_source_scale(source_scale);
    let key_for_main = key.clone();
    let watchdog_key = key.clone();
    let watchdog_owner_display_name = owner_display_name.clone();
    let watchdog_source_title = source_title.clone();
    let watchdog_branch = Arc::new(Mutex::new(EnsureWindowCreationBranch::Pending));
    let watchdog_branch_for_main = Arc::clone(&watchdog_branch);
    let scheduled_at = std::time::Instant::now();
    let run_result = app.run_on_main_thread(move || {
        let app = &app_main;
        let owner_identity = owner_identity.as_str();
        let owner_display_name = owner_display_name.as_str();
        let source_title = source_title.as_str();
        let source_url = source_url.clone();
        let key = key_for_main.clone();
        let passive_anchor = crate::window_diag::frontmost_normal_window_number();

        // Re-check under the main thread in case two RoomEvents raced to open
        // the same window before either reached the main thread. The second
        // TrackSubscribed to land here is exactly the already-open republish
        // case (same class fix as the early `already_open` check above) --
        // pick up its canonical size too instead of silently dropping it
        // (#416 follow-up review nit).
        if with_state(|s| s.windows.contains_key(&key)) {
            with_state(|s| {
                if let Some(window) = s.windows.get_mut(&key) {
                    window.source_kind = source_kind;
                    window.share_instance_id = share_instance_id.clone();
                }
            });
            update_canonical_source_size_on_republish(app, &key, canonical_source_size);
            *watchdog_branch_for_main.lock_unpoisoned() =
                EnsureWindowCreationBranch::AlreadyOpen;
            return;
        }

        // Reuse a retired (hidden, never-destroyed) window for this id if one
        // exists — the common re-share/toggle path. See
        // `CompositorState::retired`'s doc comment for why windows are parked
        // instead of destroyed (destroying them crashed the app).
        let reused = with_state(|s| {
            s.retired_order.retain(|stored| stored != &key);
            s.retired.remove(&key)
        });
        if let Some(mut win_state) = reused {
            let reveal_now = apply_retired_reuse_reveal_state(
                &mut win_state.revealed_first_frame,
                win_state.layer_has_content,
            );
            win_state.remote_control_available = remote_control_available;
            win_state.owner_display_name = owner_display_name.to_string();
            win_state.source_title = source_title.to_string();
            win_state.source_url = source_url.clone();
            win_state.source_kind = source_kind;
            win_state.share_instance_id = share_instance_id.clone();
            win_state.source_scale = source_scale;
            win_state.stripped_for_pool = false;
            // #840: a reuse that comes back on screen is showing the layer's
            // RETAINED frame, not live media -- so it is a hold, and must
            // carry the same honest "paused" label every other held frame
            // gets. `drain_pending_display_samples_on_main` clears it the
            // moment a real frame lands. Reuse with nothing to show has
            // nothing to label.
            win_state.held_reason =
                reveal_now.then_some(HoldWindowReason::ReplacementInbound);
            win_state.ai_chat_overlay_open = false;
            win_state.pending_display_samples.clear();
            win_state.last_frame_received_ms.store(0, Ordering::Relaxed);
            win_state.frames_received.store(0, Ordering::Relaxed);
            win_state
                .last_display_enqueued_ms
                .store(0, Ordering::Relaxed);
            win_state
                .frames_display_enqueued
                .store(0, Ordering::Relaxed);
            *win_state.source_pixel_size.lock_unpoisoned() = None;
            *win_state.canonical_source_pixel_size.lock_unpoisoned() = canonical_source_size;
            *win_state.source_presentation_size.lock_unpoisoned() = None;
            *win_state.pending_source_resize.lock_unpoisoned() = None;
            // A retired panel has no live request ownership. Do not let an
            // acknowledgement from its previous share instance affect reuse.
            reset_programmatic_resize_events(&win_state);
            // #416: a republish could retire and re-reveal this window while
            // the user's pointer was still DOWN. Clearing the gesture bit here
            // made the reveal-time source resize read gesture=idle and move the
            // panel out from under a live drag. Carry a genuinely-live gesture
            // across the reveal instead.
            //
            // #627 narrowed how often that happens rather than making it
            // impossible: a republish whose replacement the SFU already holds
            // now HOLDS the open window (see `hold_window_last_frame`) instead
            // of retiring it, so it does not reach this reuse branch at all.
            // Measured ordering, for the record: `TrackSubscribed(new)` beat
            // `TrackUnpublished(old)` in 10/10 real-SFU runs, so this branch
            // was never the common republish path -- a re-share/toggle is.
            carry_resize_gesture_across_reveal(&win_state);
            show_retired_window_on_main(
                app,
                &key,
                &mut win_state,
                passive_anchor,
                "ensure_window",
                reveal_now,
            );
            with_state(|s| {
                s.windows.insert(key.clone(), win_state);
            });
            if reveal_now {
                // After the insert: the header eval needs the panel present in
                // `windows`, same ordering `hold_window_last_frame` relies on.
                set_window_media_paused(app, owner_identity, window_id, true);
                log::info!(
                    "compositor: reused remote window {window_id} from '{owner_identity}' revealed its retained last frame (held, layer untouched) (#840)"
                );
            }
            crate::viewer_demand::publish_window_open(app, window_id);
            crate::window_diag::log_window_stack(app, &format!("after reuse ensure_window {window_id}"));
            *watchdog_branch_for_main.lock_unpoisoned() =
                EnsureWindowCreationBranch::ReusedFromPool;
            return;
        }

    *watchdog_branch_for_main.lock_unpoisoned() = EnsureWindowCreationBranch::Created;
    log::info!(
        "compositor: creating remote window {window_id} for '{owner_display_name}' ({owner_identity}), source '{source_title}'"
    );

    let label = panel_label_for_key(&key);
    let origin = desktop_origin(app);
    let slot = with_state(|s| {
        let slot = s.next_cascade_slot;
        s.next_cascade_slot += 1;
        slot
    });
    let (x, y) = cascade_position(origin, slot);

    let total_height = HEADER_HEIGHT + DEFAULT_CONTENT_HEIGHT;

    let _panel = match PanelBuilder::<_, RemoteWindowPanel>::new(app, &label)
        .url(WebviewUrl::App(
            surface_route_url(
                window_id,
                owner_identity,
                owner_display_name,
                source_title,
                source_url.as_deref(),
                remote_control_available,
                remote_control_disallowed,
                owner_palette_index,
            )
            .into(),
        ))
        .title(source_title)
        .position(tauri::Position::Logical(tauri::LogicalPosition { x, y }))
        // Normal window level -- a shared remote window is real content the
        // user layers other windows over, NOT an always-on-top overlay.
        // `Floating` (the old value) kept it above every other app's windows,
        // which reads as an annoying stuck window.
        .level(PanelLevel::Normal)
        .size(tauri::Size::Logical(tauri::LogicalSize {
            width: DEFAULT_CONTENT_WIDTH,
            height: total_height,
        }))
        .has_shadow(true)
        // MUST be transparent: the real video is a native
        // `AVSampleBufferDisplayLayer` sublayer added to this panel's content
        // view (see `attach_display_layer`). The surface webview paints only
        // the header strip; the rest must stay clear so the native video layer
        // remains visible and no opaque WKWebView backing can flash through.
        .transparent(true)
        .no_activate(true)
        .corner_radius(SCREENSHARE_BORDER_RADIUS_PX)
        .with_window(|w| w.decorations(false).resizable(true).accept_first_mouse(true))
        .collection_behavior(CollectionBehavior::new().managed())
        .build()
    {
        Ok(panel) => panel,
        Err(e) => {
            log::error!("compositor: failed to create remote window panel for {window_id}: {e}");
            return;
        }
    };
    if let Some(win) = app.get_webview_window(&label) {
        set_remote_window_min_size(&win);
        apply_remote_window_border(
            &win,
            owner_border_color_hex(owner_identity, owner_display_name, owner_palette_index),
        );
        crate::webview_transparency::apply_or_retry(app, &win);
        order_below_anchor(&win, passive_anchor);
        let _ = win.hide();
    }

    let display = DisplayLayer::new();
    // DEFAULT_* are fallbacks only — the wrapper measures the just-built
    // panel's real frame first, so a min-size/screen clamp landing between
    // PanelBuilder::build and this attach cannot latch a wrong video-view
    // margin (adversarial-review finding).
    attach_display_layer(
        app,
        &label,
        window_id,
        &display,
        DEFAULT_CONTENT_WIDTH,
        DEFAULT_CONTENT_HEIGHT,
    );

    // PETAL_COMPOSITOR_NO_CHROME: skip the telepointer/control child windows
    // (debug/isolation -- to confirm whether they occlude the native video).
    // The header is no longer a child window -- it is rendered by the panel's
    // own surface webview (see `surface_route_url`), so it can never detach.
    if std::env::var("PETAL_COMPOSITOR_NO_CHROME").is_err() {
        create_control_overlay(
            app,
            window_id,
            &label,
            x,
            y + HEADER_HEIGHT,
            DEFAULT_CONTENT_WIDTH,
            DEFAULT_CONTENT_HEIGHT,
            owner_identity,
        );
        create_pointer_overlay(app, &key, &label, x, y + HEADER_HEIGHT, DEFAULT_CONTENT_WIDTH, DEFAULT_CONTENT_HEIGHT);
        create_ai_chat_overlay(app, &key, x, y, DEFAULT_CONTENT_WIDTH, total_height);
        // No order_chrome_above_panel call here (review finding, #445): the
        // panel and this just-created chrome are hidden two lines below by
        // hide_remote_window_chrome_on_main, making an ordering call here a
        // same-turn no-op at best -- the first-frame reveal path is what
        // establishes real ordering once the window actually becomes visible.
    }
    hide_remote_window_chrome_on_main(app, &key);
    let receiver_scale = app
        .get_webview_window(&label)
        .and_then(|window| window.scale_factor().ok())
        .unwrap_or(1.0)
        .max(1.0);

    with_state(|s| {
        s.windows.insert(
            key.clone(),
            CompositorWindow {
                remote_control_disallowed,
                owner_identity: owner_identity.to_string(),
                owner_display_name: owner_display_name.to_string(),
                owner_palette_index,
                source_title: source_title.to_string(),
                source_url,
                source_kind,
                share_instance_id,
                display: Some(display),
                source_scale,
                panel_content_size: Mutex::new((DEFAULT_CONTENT_WIDTH, DEFAULT_CONTENT_HEIGHT)),
                source_presentation_size: Mutex::new(None),
                canonical_source_pixel_size: Mutex::new(canonical_source_size),
                programmatic_resize_events: Mutex::new(ProgrammaticResizeEvents::default()),
                next_programmatic_resize_generation: AtomicU64::new(0),
                canonical_source_epoch: AtomicU64::new(1),
                canonical_source_generation: AtomicU64::new(0),
                pending_source_resize: Mutex::new(None),
                user_resize_until_ms: AtomicU64::new(0),
                user_resize_active: AtomicBool::new(false),
                user_resize_active_since_ms: AtomicU64::new(0),
                receiver_scale: Mutex::new(receiver_scale),
                source_pixel_size: Mutex::new(None),
                remote_control_available,
                last_frame_received_ms: AtomicU64::new(0),
                frames_received: AtomicU64::new(0),
                last_display_enqueued_ms: AtomicU64::new(0),
                frames_display_enqueued: AtomicU64::new(0),
                pending_display_samples: PendingFrameQueue::default(),
                revealed_first_frame: false,
                layer_has_content: false,
                held_reason: None,
                stripped_for_pool: false,
                app_origin: None,
                ai_chat_overlay_open: false,
                z_rank: None,
            },
        );
    });

    crate::viewer_demand::publish_window_open(app, window_id);
    install_aspect_lock(app, key.clone());

    log::info!(
        "compositor: opened remote window {window_id} for '{owner_display_name}' ({owner_identity}) at ({x:.0},{y:.0}), size {DEFAULT_CONTENT_WIDTH}x{DEFAULT_CONTENT_HEIGHT} (+{HEADER_HEIGHT} header)"
    );

    // Occlusion diagnostics: dump the on-screen window stack now that the
    // panel + chrome exist, so a black-on-screen window can be debugged from
    // the log alone (see window_diag's module doc comment).
    crate::window_diag::log_window_stack(app, &format!("after ensure_window {window_id}"));
    *watchdog_branch_for_main.lock_unpoisoned() = EnsureWindowCreationBranch::Created;
    });
    if let Err(e) = run_result {
        log::error!("compositor: run_on_main_thread (ensure_window {window_id}) failed: {e}");
    } else {
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(ENSURE_WINDOW_CREATION_WATCHDOG_TIMEOUT).await;
            let opened = is_open_for_owner(&watchdog_key.owner_identity, watchdog_key.window_id);
            let branch = *watchdog_branch.lock_unpoisoned();
            let retired = with_state(|s| s.retired.contains_key(&watchdog_key));
            match ensure_window_creation_watchdog_decision(
                scheduled_at.elapsed(),
                opened,
                branch,
                retired,
            ) {
                EnsureWindowCreationWatchdogDecision::KeepWaiting => {}
                EnsureWindowCreationWatchdogDecision::LogStall => {
                    log::warn!(
                        "compositor: remote window {} creation watchdog fired after {}ms for '{}' ({}), source '{}' -- saw creating log but no opened state; main-thread/AppKit window build may be stalled",
                        watchdog_key.window_id,
                        scheduled_at.elapsed().as_millis(),
                        watchdog_owner_display_name,
                        watchdog_key.owner_identity,
                        watchdog_source_title
                    );
                    crate::logging::note_window_creation_watchdog_stall(watchdog_key.window_id);
                }
                EnsureWindowCreationWatchdogDecision::LogPublicationChurn => {
                    log::info!(
                        "compositor: remote window {} creation watchdog observed publication churn after {}ms for '{}' ({}), source '{}' -- the window was {} and is now {}; the publication churned underneath us (sharer republish), NOT an AppKit build stall (#840)",
                        watchdog_key.window_id,
                        scheduled_at.elapsed().as_millis(),
                        watchdog_owner_display_name,
                        watchdog_key.owner_identity,
                        watchdog_source_title,
                        branch.completed_label(),
                        if retired {
                            "retired to the reuse pool"
                        } else {
                            "no longer open"
                        },
                    );
                }
            }
        });
    }
}

/// Frames of every currently-open compositor window, as
/// `(window_id, x, y, w, h)` in global top-left-origin LOGICAL points (the
/// same space CGWindowList bounds use) -- consumed by
/// `window_diag::log_window_stack` to decide which other-process windows are
/// occlusion-relevant.
pub(crate) fn open_window_frames(app: &AppHandle) -> Vec<(u32, f64, f64, f64, f64)> {
    let keys: Vec<RemoteWindowKey> = with_state(|s| s.windows.keys().cloned().collect());
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(window) = app.get_webview_window(&panel_label_for_key(&key)) else {
            continue;
        };
        let (Ok(pos), Ok(size)) = (window.outer_position(), window.outer_size()) else {
            continue;
        };
        let scale = window.scale_factor().unwrap_or(1.0);
        out.push((
            key.window_id,
            pos.x as f64 / scale,
            pos.y as f64 / scale,
            size.width as f64 / scale,
            size.height as f64 / scale,
        ));
    }
    out
}

fn content_frame_from_panel_bounds(x: f64, y: f64, width: f64, height: f64) -> Option<WindowFrame> {
    let content_height = height - HEADER_HEIGHT;
    if width <= 0.0 || content_height <= 0.0 {
        return None;
    }
    Some(WindowFrame {
        x: x.round() as i32,
        y: (y + HEADER_HEIGHT).round() as i32,
        width: width.round() as i32,
        height: content_height.round() as i32,
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ChromeFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ChromeFrames {
    control: ChromeFrame,
    pointer: ChromeFrame,
    ai_chat: ChromeFrame,
}

/// Fixed size of the AI-chat overlay (#844), inherited from the popover it
/// replaces (`RemoteWindowHeader.svelte`'s old `.ai-chat-remote-panel`: 280pt
/// wide, up to 320pt tall) with a little headroom for the composer row that
/// no longer has to share space with a PTT button (that stays in the header
/// strip). Inset from the content area's edges by `AI_CHAT_OVERLAY_MARGIN`.
const AI_CHAT_OVERLAY_WIDTH: f64 = 300.0;
const AI_CHAT_OVERLAY_MAX_HEIGHT: f64 = 360.0;
const AI_CHAT_OVERLAY_MARGIN: f64 = 12.0;

/// Frame for the AI-chat overlay (#844): a fixed-size panel anchored to the
/// top-right of the video content area, clamped so it never overflows a
/// small window.
fn ai_chat_overlay_frame_for_panel_bounds(x: f64, y: f64, width: f64, height: f64) -> ChromeFrame {
    let content_h = (height - HEADER_HEIGHT).max(1.0);
    let content_y = y + HEADER_HEIGHT;
    let overlay_w = AI_CHAT_OVERLAY_WIDTH.min((width - 2.0 * AI_CHAT_OVERLAY_MARGIN).max(1.0));
    let overlay_h =
        AI_CHAT_OVERLAY_MAX_HEIGHT.min((content_h - 2.0 * AI_CHAT_OVERLAY_MARGIN).max(1.0));
    ChromeFrame {
        x: x + width - overlay_w - AI_CHAT_OVERLAY_MARGIN,
        y: content_y + AI_CHAT_OVERLAY_MARGIN,
        width: overlay_w,
        height: overlay_h,
    }
}

/// Frames of the click-through overlay children (control + pointer) plus the
/// AI-chat overlay for a panel at the given bounds. Control and pointer
/// overlay the WHOLE video content area (below the header strip), matching
/// the native video NSView's frame; the AI-chat overlay only covers a
/// sub-region of it (see `ai_chat_overlay_frame_for_panel_bounds`). The
/// header is no longer a separate window -- it is drawn by the panel's own
/// surface webview -- so it isn't repositioned here.
fn chrome_frames_for_panel_bounds(x: f64, y: f64, width: f64, height: f64) -> ChromeFrames {
    let content_h = (height - HEADER_HEIGHT).max(1.0);
    let content_y = y + HEADER_HEIGHT;
    ChromeFrames {
        control: ChromeFrame {
            x,
            y: content_y,
            width,
            height: content_h,
        },
        pointer: ChromeFrame {
            x,
            y: content_y,
            width,
            height: content_h,
        },
        ai_chat: ai_chat_overlay_frame_for_panel_bounds(x, y, width, height),
    }
}

/// Video-content frames for currently-open received compositor windows,
/// keyed by the original shared `window_id`. Unlike `open_window_frames`, this
/// excludes Petal's header strip so viewer-origin telepointers normalize
/// against the same surface as source-origin telepointers.
pub(crate) fn open_content_frames(app: &AppHandle) -> Vec<(u32, WindowFrame)> {
    let keys: Vec<RemoteWindowKey> = with_state(|s| s.windows.keys().cloned().collect());
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        if let Some(frame) = content_frame_for_key(app, &key) {
            out.push((key.window_id, frame));
        }
    }
    out
}

/// Same remote compositor frames with the authenticated sharer identity. The
/// owner is required on macOS because window ids are only unique on the
/// publisher's machine; it lets telepointer delivery select exactly one
/// remote overlay when two participants reuse the same native id.
pub(crate) fn open_content_frames_with_owners(app: &AppHandle) -> Vec<(u32, WindowFrame, String)> {
    let keys: Vec<RemoteWindowKey> = with_state(|s| s.windows.keys().cloned().collect());
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        if let Some(frame) = content_frame_for_key(app, &key) {
            out.push((key.window_id, frame, key.owner_identity));
        }
    }
    out
}

/// #906: per-remote-window occlusion metadata for the telepointer sender's
/// real topmost-window hit-test gate (mirrors Windows' `root_hwnds` --
/// `windows_compositor::PointerTargetSnapshot`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PointerFamilyMeta {
    pub(crate) window_id: u32,
    pub(crate) owner_identity: String,
    /// Native WindowServer ids (`NSWindow.windowNumber`, the same space as
    /// `CGWindowID`/`SLSFindWindowAndOwner`'s hit-test result) for the panel
    /// PLUS its click-through pointer/control/ai-chat overlay children. The
    /// overlays cover the whole video content area (`compositor.rs`'s
    /// `chrome_frames_for_panel_bounds`), so a hit-test at any on-screen point
    /// over the panel resolves to whichever of THESE ids is frontmost there --
    /// never the panel's own id directly when an overlay is present. A hit on
    /// any member of this set counts as "the cursor is over this panel's
    /// visible surface," not just a hit on the bare panel id.
    pub(crate) family_ids: Vec<u32>,
    /// False for a panel that is hidden or not yet revealed (a warm-pool
    /// member pre-first-frame, `compositor.rs`'s `let _ = win.hide()`).
    /// `outer_position()`/`outer_size()` still report a frame for a hidden
    /// panel, so without this a cursor passing over where an invisible panel
    /// WOULD be already looked like a hit (#906 DoD item).
    pub(crate) is_visible: bool,
}

/// Batch-read `PointerFamilyMeta` for every currently-open remote compositor
/// window, in ONE main-thread round trip regardless of how many windows are
/// open (typically 0-4) -- the telepointer sender polls this at its ~9Hz
/// frame-refresh cadence (`FRAME_REFRESH_TICKS`), never per-tick at 45Hz, so
/// this one round trip per ~110ms never touches the hot cursor-poll path.
/// `platform::appkit::window_number` is raw AppKit messaging (unlike Tauri's
/// own `outer_position`/`outer_size`/`is_visible` getters, which this crate
/// already calls directly off the main thread elsewhere in this file) and
/// every existing call site in this crate marshals it onto the main thread
/// first -- this follows the same convention.
///
/// Returns `None` -- distinct from `Some(vec![])` -- when the round trip
/// itself could not be completed (scheduling failed, or the 150ms deadline
/// passed with the main thread still busy, e.g. mid window-drag). This
/// distinction matters (#906 adversarial-review follow-up, P2): the caller
/// caches this result across ticks, and a transient main-thread stall must
/// NOT be indistinguishable from "there are genuinely no remote windows open
/// right now" -- the former should keep the last known-good cache, the
/// latter should correctly become empty. Collapsing both into `Vec::new()`
/// meant a single slow tick could wipe a healthy cache and fail every remote
/// telepointer target closed until the next successful refresh.
pub(crate) fn open_pointer_family_meta(app: &AppHandle) -> Option<Vec<PointerFamilyMeta>> {
    let keys: Vec<RemoteWindowKey> = with_state(|s| s.windows.keys().cloned().collect());
    if keys.is_empty() {
        // Genuinely nothing to gate -- authoritative, not a failure. Also
        // means a pure sharer (no remote panels open) never pays the
        // main-thread round trip below at all.
        return Some(Vec::new());
    }
    if crate::platform::appkit::is_main_thread() {
        return Some(pointer_family_meta_on_main(app, &keys));
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let app_main = app.clone();
    if app
        .run_on_main_thread(move || {
            let _ = sender.send(pointer_family_meta_on_main(&app_main, &keys));
        })
        .is_err()
    {
        return None;
    }
    receiver.recv_timeout(Duration::from_millis(150)).ok()
}

fn pointer_family_meta_on_main(app: &AppHandle, keys: &[RemoteWindowKey]) -> Vec<PointerFamilyMeta> {
    keys.iter()
        .filter_map(|key| {
            let panel = app.get_webview_window(&panel_label_for_key(key))?;
            let is_visible = panel.is_visible().unwrap_or(false);
            let mut family_ids = Vec::with_capacity(4);
            if let Ok(number) = crate::platform::appkit::window_number(&panel) {
                family_ids.push(number);
            }
            for label in [
                control_label_for_key(key),
                pointer_label_for_key(key),
                ai_chat_label_for_key(key),
            ] {
                if let Some(chrome) = app.get_webview_window(&label) {
                    if let Ok(number) = crate::platform::appkit::window_number(&chrome) {
                        family_ids.push(number);
                    }
                }
            }
            Some(PointerFamilyMeta {
                window_id: key.window_id,
                owner_identity: key.owner_identity.clone(),
                family_ids,
                is_visible,
            })
        })
        .collect()
}

pub(crate) fn content_frame_and_scale_for_window(
    app: &AppHandle,
    window_id: u32,
) -> Option<(WindowFrame, f64)> {
    let key = resolve_open_window_key(window_id, None)?;
    let window = app.get_webview_window(&panel_label_for_key(&key))?;
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return None;
    };
    let scale = window.scale_factor().unwrap_or(1.0).max(1.0);
    let frame = content_frame_from_panel_bounds(
        pos.x as f64 / scale,
        pos.y as f64 / scale,
        size.width as f64 / scale,
        size.height as f64 / scale,
    )?;
    Some((frame, scale))
}

/// Return whether the receiver panel is hidden or fully covered by other
/// windows. AppKit's occlusion state is authoritative for this panel; CG
/// window-stack geometry cannot distinguish a covered panel from one merely
/// below another app's transparent content.
fn occlusion_or_visible(result: Option<bool>) -> bool {
    result.unwrap_or(false)
}

pub(crate) fn window_is_fully_occluded(app: &AppHandle, window_id: u32) -> bool {
    let Some(key) = resolve_open_window_key(window_id, None) else {
        return false;
    };
    let Some(window) = app.get_webview_window(&panel_label_for_key(&key)) else {
        return false;
    };
    if crate::platform::appkit::is_main_thread() {
        return crate::platform::appkit::is_fully_occluded(&window);
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    if app
        .run_on_main_thread(move || {
            let _ = sender.send(crate::platform::appkit::is_fully_occluded(&window));
        })
        .is_err()
    {
        return false;
    }
    occlusion_or_visible(receiver.recv_timeout(Duration::from_millis(250)).ok())
}

#[cfg(test)]
mod occlusion_tests {
    use super::occlusion_or_visible;

    #[test]
    fn occlusion_query_failures_assume_visible() {
        assert!(!occlusion_or_visible(None));
        assert!(occlusion_or_visible(Some(true)));
    }
}

/// Return the last decoded source-frame dimensions for an open remote window.
/// `None` means the panel is still using its pre-first-frame placeholder size.
pub(crate) fn source_pixel_size_for_window(window_id: u32) -> Option<(u32, u32)> {
    let key = resolve_open_window_key(window_id, None)?;
    with_state(|s| {
        s.windows
            .get(&key)
            .and_then(|window| *window.source_pixel_size.lock_unpoisoned())
    })
}

fn content_frame_for_key(app: &AppHandle, key: &RemoteWindowKey) -> Option<WindowFrame> {
    let window = app.get_webview_window(&panel_label_for_key(key))?;
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return None;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    content_frame_from_panel_bounds(
        pos.x as f64 / scale,
        pos.y as f64 / scale,
        size.width as f64 / scale,
        size.height as f64 / scale,
    )
}

fn apply_remote_window_border(window: &tauri::WebviewWindow, color_hex: &str) {
    let Some((r, g, b)) = parse_hex_rgb(color_hex) else {
        log::warn!("compositor: invalid remote border color '{color_hex}'");
        return;
    };
    if let Err(e) = crate::platform::appkit::apply_window_border(
        window,
        (r, g, b),
        SCREENSHARE_BORDER_STROKE_PX,
        SCREENSHARE_BORDER_RADIUS_PX,
    ) {
        log::warn!(
            "compositor: failed to apply border to '{}': {e}",
            window.label(),
        );
    }
}

/// Install a resize-event listener on `window_id`'s panel that clamps every
/// user-driven resize back to the source's real aspect ratio (SPEC.md §4.4:
/// "resizable with aspect lock"). Approach: rather than a native
/// `NSWindow.setContentAspectRatio:` call (which `tauri_nspanel`'s builder
/// doesn't expose directly, and which behaves oddly combined with the fixed-
/// height header strip on top of a variable-height video area -- the ASPECT
/// LOCK only applies to the video content area, not the panel's total
/// height, which is `content_height + HEADER_HEIGHT`), this listens for
/// `WindowEvent::Resized` and, whenever the live size doesn't match the
/// locked aspect ratio, immediately corrects it by deriving height from the
/// new width (matches how most creative-tool "aspect-locked" resizable
/// panels behave: width drives, height follows).
const ASPECT_LOCK_TOLERANCE: f64 = 0.01;

fn aspect_locked_content_height(width: f64, content_h: f64, source_aspect: f64) -> (f64, bool) {
    let source_aspect = source_aspect.max(0.01);
    let content_h = content_h.max(1.0);
    let current_aspect = width / content_h;
    if (current_aspect - source_aspect).abs() <= ASPECT_LOCK_TOLERANCE {
        (content_h, false)
    } else {
        ((width / source_aspect).max(1.0), true)
    }
}

fn source_aspect_for_resize_event(
    window: &CompositorWindow,
    fallback_width: f64,
    fallback_height: f64,
) -> f64 {
    if let Some((width, height)) = *window.source_presentation_size.lock_unpoisoned() {
        return width / height.max(1.0);
    }
    if let Some((width, height)) = *window.canonical_source_pixel_size.lock_unpoisoned() {
        let (logical_width, logical_height) =
            source_presentation_size_points(width, height, valid_source_scale(window.source_scale));
        return logical_width / logical_height.max(1.0);
    }
    let (width, height) = *window.panel_content_size.lock_unpoisoned();
    if width > 0.0 && height > 0.0 {
        width / height
    } else {
        fallback_width / fallback_height.max(1.0)
    }
}

fn install_aspect_lock(app: &AppHandle, key: RemoteWindowKey) {
    let window_id = key.window_id;
    let label = panel_label_for_key(&key);
    let Some(window) = app.get_webview_window(&label) else {
        return;
    };
    let app = app.clone();
    let guard_label = label.clone();
    let key_for_event = key.clone();
    // Reentrancy guard: `set_size` inside the handler below synchronously
    // re-triggers another `Resized` event on macOS -- without this flag that
    // would recurse (each correction slightly off due to float rounding,
    // triggering another "correction"). Matches the standard
    // resize-listener-that-resizes-itself pattern.
    let correcting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    window.on_window_event(move |event| {
        let (new_size, scale_override, acknowledge_programmatic_resize) = match event {
            tauri::WindowEvent::Resized(new_size) => (*new_size, None, true),
            // This is display migration, not AppKit's acknowledgement of our
            // `set_size`. It must not consume a cancelled resize callback.
            tauri::WindowEvent::ScaleFactorChanged {
                scale_factor,
                new_inner_size,
                ..
            } => (*new_inner_size, Some(*scale_factor), false),
            tauri::WindowEvent::Moved(_) => {
                crate::remote_control::invalidate_control_frame(window_id);
                // Deferred for the same reason as the Resized branch: a child's
                // set_position doesn't stick synchronously inside the parent's
                // move handler. The overlays auto-follow the panel move anyway
                // (addChildWindow) -- control/pointer via
                // `WebviewWindowBuilder::parent()`, the ai-chat overlay (#844)
                // via the explicit `attach_ai_chat_overlay`/
                // `detach_ai_chat_overlay` pair kept in sync with its own
                // show/hide, since `PanelBuilder` has no equivalent
                // tauri-level parent API -- the deferred sync corrects their
                // size/inset.
                sync_chrome_to_panel_frame_deferred(&app, &key_for_event);
                return;
            }
            _ => return,
        };
        if correcting.swap(true, Ordering::SeqCst) {
            correcting.store(false, Ordering::SeqCst);
            return;
        }

        let Some(win) = app.get_webview_window(&guard_label) else {
            correcting.store(false, Ordering::SeqCst);
            return;
        };
        let scale = scale_override.unwrap_or_else(|| win.scale_factor().unwrap_or(1.0));
        let width = new_size.width as f64 / scale;
        let height = new_size.height as f64 / scale;
        let content_h = (height - HEADER_HEIGHT).max(1.0);

        let outcome = with_state(|s| {
            s.windows
                .get(&key_for_event)
                .map(|window| {
                    resize_listener_outcome(
                        window,
                        width,
                        content_h,
                        scale,
                        acknowledge_programmatic_resize,
                    )
                })
                .unwrap_or(ResizeListenerOutcome::Settled {
                    content_height: content_h,
                    needs_correction: false,
                    settled_generation: None,
                })
        });
        let (effective_content_h, needs_correction) = match outcome {
            ResizeListenerOutcome::Ignored => {
                // A cancelled/superseded `set_size` callback is not evidence of
                // a user gesture. In particular, do not aspect-correct it,
                // update display/chrome geometry, or schedule a geometry
                // refresh while a real drag is beginning.
                log::debug!(
                    "compositor: ignored stale programmatic resize callback for window {window_id} at {:.1}x{:.1}",
                    width,
                    content_h,
                );
                correcting.store(false, Ordering::SeqCst);
                return;
            }
            ResizeListenerOutcome::Buffered { generation } => {
                // The geometry matches the newest request but an earlier
                // cancelled callback is still inside its ordering barrier. Do
                // not let this ambiguous AppKit event enter aspect lock; the
                // scheduled native bounds reconciliation for this exact
                // generation settles it.
                log::debug!(
                    "compositor: buffered ambiguous programmatic resize generation {generation} for window {window_id}",
                );
                correcting.store(false, Ordering::SeqCst);
                return;
            }
            ResizeListenerOutcome::Settled {
                content_height,
                needs_correction,
                settled_generation,
            } => {
                if let Some(generation) = settled_generation {
                    log::debug!(
                        "compositor: consumed programmatic resize generation {generation} for window {window_id} at {:.1}x{:.1}",
                        width,
                        content_height,
                    );
                }
                (content_height, needs_correction)
            }
        };

        // Tolerance avoids fighting float rounding on every single resize
        // tick (native resize delivers many Resized events per drag).
        if needs_correction {
            trace_panel_geometry(
                "aspect-lock-correction",
                window_id,
                width,
                HEADER_HEIGHT + effective_content_h,
                None,
            );
            let _ = win.set_size(tauri::Size::Logical(tauri::LogicalSize {
                width,
                height: HEADER_HEIGHT + effective_content_h,
            }));
        } else {
            trace_panel_geometry(
                "listener-settled",
                window_id,
                width,
                HEADER_HEIGHT + effective_content_h,
                None,
            );
        }

        settle_panel_content_geometry(
            &app,
            &key_for_event,
            width,
            effective_content_h,
            scale,
            scale_override.is_some(),
        );

        correcting.store(false, Ordering::SeqCst);
    });
}

/// The single settled-geometry path used by native resize callbacks and by a
/// successful `set_size` that AppKit acknowledges without delivering a
/// callback. Keeping this in one adapter prevents panel/display/chrome state
/// from diverging when native callback timing changes (#416).
fn settle_panel_content_geometry(
    app: &AppHandle,
    key: &RemoteWindowKey,
    width: f64,
    content_height: f64,
    scale: f64,
    scale_changed: bool,
) {
    let window_missing = with_state(|s| {
        let Some(window) = s.windows.get(key) else {
            return true;
        };
        record_settled_panel_content_geometry(window, width, content_height, scale);
        if let Some(display) = window.display.as_ref() {
            display.set_frame(0.0, 0.0, width, content_height);
            display.set_contents_scale(scale);
            apply_display_filter(window, width, content_height, scale);
        }
        false
    });
    if window_missing {
        return;
    }
    // Reposition after native sizing has unwound; this covers the control and
    // pointer children and the scheduled geometry refresh keeps the visible
    // border on the same settled panel bounds.
    sync_chrome_to_panel_frame_deferred(app, key);
    if scale_changed {
        crate::viewer_demand::publish_window_open(app, key.window_id);
    } else {
        crate::viewer_demand::schedule_window_geometry_refresh(app, key.window_id);
    }
}

fn record_settled_panel_content_geometry(
    window: &CompositorWindow,
    width: f64,
    content_height: f64,
    scale: f64,
) -> bool {
    *window.receiver_scale.lock_unpoisoned() = scale;
    *window.panel_content_size.lock_unpoisoned() = (width, content_height);
    true
}

/// Attach `display`'s native layer-hosting view to the panel content view.
/// The raw AppKit/CoreAnimation wiring lives in `platform::appkit`; this
/// wrapper keeps compositor-specific lookup, diagnostics, and debug env flags
/// local to the compositor lifecycle code.
fn attach_display_layer(
    app: &AppHandle,
    label: &str,
    window_id: u32,
    display: &DisplayLayer,
    width: f64,
    height: f64,
) {
    let Some(window) = app.get_webview_window(label) else {
        return;
    };

    // The video view must be sized to the CONTENT area (below the header
    // strip), never to the content view's full bounds: the old bounds-sized
    // attach was HEADER_HEIGHT too tall, and under `resizeAspect` gravity
    // that painted the video across the lower half of the header and left a
    // transparent ~HEADER_HEIGHT/2 gap at the window bottom, on every fresh
    // attach and every reuse-pool rehydrate (defect E2, 2026-07-30).
    //
    // MEASURE, don't remember (adversarial-review finding, same session):
    // callers pass their best-known content size, but a remembered
    // `panel_content_size` can be stale exactly on the paths that reach this
    // seam — a min-size/screen clamp between panel build and attach would
    // latch a wrong margin into the autoresize mask with nothing on the
    // activate/reveal path ever correcting it. (This comment's original
    // motivating example, a header-only-strip window rehydrated from the
    // pool still remembering its taller expanded size, no longer applies
    // now that #675 removed that feature -- the clamp hazard described
    // above is independent of it and still applies.) So the live panel
    // frame wins whenever readable; the caller's value is only the
    // fallback. Must stay in agreement with the frame
    // `settle_panel_content_geometry` writes (the only other
    // `display.set_frame` writer in the crate).
    let (content_w, content_h) = match content_geometry(&window) {
        Some((measured_w, measured_h, _scale)) => (measured_w, measured_h),
        None => (width, height),
    };
    trace_panel_geometry(
        "attach-display-view",
        window_id,
        content_w,
        HEADER_HEIGHT + content_h,
        None,
    );
    match crate::platform::appkit::attach_display_layer(
        &window,
        display,
        content_w,
        content_h,
        std::env::var("PETAL_COMPOSITOR_DEBUG_BG").is_ok(),
    ) {
        Ok(window_number) => {
            // Log the CGWindowID (NSWindow.windowNumber) so it can be captured
            // by id (`screencapture -l<id>`) even when occluded by another
            // window.
            log::info!("compositor: window {label} CGWindowID={window_number}");
            if std::env::var("PETAL_COMPOSITOR_DEBUG_BG").is_ok() {
                log::info!("compositor: DEBUG red background set on window video layer");
            }
        }
        Err(e) => log::warn!("compositor: failed to attach display layer for {label}: {e}"),
    }
}

struct ChromeWebviewSpec {
    label: String,
    role: &'static str,
    url: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    ignore_cursor_events: bool,
    always_on_top: Option<bool>,
}

fn create_chrome_webview(app: &AppHandle, parent_label: &str, spec: ChromeWebviewSpec) {
    let Some(parent) = app.get_webview_window(parent_label) else {
        log::warn!(
            "compositor: no parent webview window '{parent_label}' to attach {} to",
            spec.role
        );
        return;
    };
    let mut builder = WebviewWindowBuilder::new(app, &spec.label, WebviewUrl::App(spec.url.into()))
        .title("")
        .position(spec.x, spec.y)
        .inner_size(spec.width, spec.height)
        .decorations(false)
        .transparent(true)
        .accept_first_mouse(true)
        .skip_taskbar(true)
        .resizable(false)
        // Tauri's default is `focused: true`, and tao key-and-order-fronts
        // (`makeKeyAndOrderFront`) at build time as a result -- so every
        // incoming remote share was flashing focus to these two child
        // webviews before `hide_remote_window_chrome_on_main` (below in this
        // file) hid them a moment later (#677). Suppress both build-time
        // visibility and activation; `reveal_remote_window_after_first_frame_on_main`'s
        // `win.show()` still makes them visible on demand -- `.visible(false)`
        // only affects the initial build, not later `show()` calls.
        .visible(false)
        .focused(false);
    if let Some(always_on_top) = spec.always_on_top {
        builder = builder.always_on_top(always_on_top);
    }
    let result = match builder.parent(&parent) {
        Ok(builder) => builder.build(),
        Err(e) => Err(e),
    };
    match result {
        Ok(win) => {
            // AppKit can reinterpret the builder position while attaching an
            // addChildWindow parent. Re-apply the requested global frame after
            // attachment so overlays start over the panel, not at the child's
            // default follow-offset (#171).
            let _ = win.set_position(tauri::Position::Logical(tauri::LogicalPosition {
                x: spec.x,
                y: spec.y,
            }));
            if spec.ignore_cursor_events {
                let _ = win.set_ignore_cursor_events(true);
            }
            crate::webview_transparency::apply_or_retry(app, &win);
        }
        Err(e) => log::warn!(
            "compositor: failed to create {} '{}': {e}",
            spec.role,
            spec.label
        ),
    }
}

/// Update the header metadata rendered by a compositor window's surface
/// webview in place, by replacing its URL query params rather than reloading.
fn refresh_header_webview(window: &tauri::WebviewWindow, query: &str) {
    // Retired compositor windows are reused rather than destroyed. When the
    // same source window is shared again, its browser URL can have changed, so
    // update the persistent child webview's route query instead of leaving a
    // stale or dead Open URL control behind.
    let search = format!("?{query}");
    let Ok(search_json) = serde_json::to_string(&search) else {
        return;
    };
    let js = format!(
        "if (window.location.search !== {search}) window.location.replace(window.location.pathname + {search});",
        search = search_json
    );
    if let Err(e) = window.eval(&js) {
        log::warn!(
            "compositor: failed to refresh header metadata for '{}': {e}",
            window.label()
        );
    }
}

pub fn update_window_metadata(
    app: &AppHandle,
    window_id: u32,
    owner_identity: &str,
    owner_display_name: &str,
    source_title: &str,
    source_url: Option<String>,
    source_scale: Option<f64>,
    remote_control_available: bool,
    remote_control_disallowed: bool,
    owner_palette_index: Option<u8>,
    share_instance_id: Option<String>,
) {
    let owner_identity = owner_identity.to_string();
    let key = RemoteWindowKey::new(owner_identity.clone(), window_id);
    let owner_display_name = owner_display_name.to_string();
    let owner_palette_index =
        owner_palette_index.filter(|index| (*index as usize) < OWNER_COLOR_PALETTE_HEX.len());
    let source_title = source_title.to_string();
    let source_url = source_url.filter(|u| crate::browser_url::is_openable_url(u));
    let share_instance_id = share_instance_id.filter(|value| !value.is_empty());
    let should_refresh = with_state(|s| {
        let Some(win) = s.windows.get_mut(&key) else {
            return false;
        };
        win.owner_display_name = owner_display_name.clone();
        win.owner_palette_index = owner_palette_index;
        win.source_title = source_title.clone();
        win.source_url = source_url.clone();
        win.share_instance_id = share_instance_id.clone();
        if let Some(source_scale) = source_scale {
            win.source_scale = valid_source_scale(source_scale);
        }
        win.remote_control_available = remote_control_available;
        true
    });
    if !should_refresh {
        return;
    }

    let app_main = app.clone();
    let key_for_main = key.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        if let Some(win) = app_main.get_webview_window(&panel_label_for_key(&key_for_main)) {
            refresh_header_webview(
                &win,
                &header_query_string(
                    window_id,
                    &owner_identity,
                    &owner_display_name,
                    &source_title,
                    source_url.as_deref(),
                    remote_control_available,
                    remote_control_disallowed,
                    owner_palette_index,
                ),
            );
        }
    }) {
        log::error!(
            "compositor: run_on_main_thread (update_window_metadata {window_id}) failed: {e}"
        );
    }
}

/// Store the sharer-reported front-to-back z-rank for one remote window
/// (0 = frontmost), decoded from the `petalWindowZOrder` participant-metadata
/// key (#875). Storage only, keyed by `(owner_identity, window_id)` like
/// every other remote-window lookup (#678 collision class) -- no UI refresh
/// here; the raise command that consumes this rank is a separate lane.
/// Unconditionally overwrites (including with `None`), so a window that
/// drops out of the sharer's currently-shared subset stops carrying a stale
/// rank on its next metadata refresh.
///
/// #875 review F3: also updates a RETIRED (viewer-hidden) entry for this
/// key, not just an open one. The metadata handler that calls this iterates
/// `window_ids_for_participant`, which only enumerates `s.windows` -- so
/// without reaching into `s.retired` here too, a window the viewer hid would
/// never learn about a rank change while hidden, and `plan_participant_raise`
/// would restore it into its stale at-hide position instead of the sharer's
/// current order.
pub fn update_window_z_rank(owner_identity: &str, window_id: u32, z_rank: Option<u32>) {
    let key = RemoteWindowKey::new(owner_identity, window_id);
    with_state(|s| {
        if let Some(win) = s.windows.get_mut(&key) {
            win.z_rank = z_rank;
        }
        if let Some(win) = s.retired.get_mut(&key) {
            win.z_rank = z_rank;
        }
    });
}

#[cfg(test)]
pub(crate) fn window_z_rank_for_test(owner_identity: &str, window_id: u32) -> Option<u32> {
    let key = RemoteWindowKey::new(owner_identity, window_id);
    with_state(|s| s.windows.get(&key).and_then(|w| w.z_rank))
}

fn control_source_dimensions(source_pixel_size: Option<(u32, u32)>) -> (u32, u32) {
    source_pixel_size.unwrap_or((0, 0))
}

fn control_route_url(
    window_id: u32,
    owner_identity: &str,
    source_width: u32,
    source_height: u32,
) -> String {
    format!(
        "compositor/control.html?windowId={window_id}&owner={}&sourceWidth={source_width}&sourceHeight={source_height}",
        percent_encode(owner_identity),
    )
}

fn refresh_control_webview(
    window: &tauri::WebviewWindow,
    window_id: u32,
    owner_identity: &str,
    source_width: u32,
    source_height: u32,
) {
    let path = format!(
        "/{}",
        control_route_url(window_id, owner_identity, source_width, source_height)
    );
    let Ok(path_json) = serde_json::to_string(&path) else {
        return;
    };
    let js = format!(
        "if (window.location.pathname + window.location.search !== {path}) window.location.replace({path});",
        path = path_json
    );
    if let Err(e) = window.eval(&js) {
        log::warn!(
            "compositor: failed to refresh control metadata for '{}': {e}",
            window.label()
        );
    }
}

fn control_source_dimensions_script(source_width: u32, source_height: u32) -> String {
    format!(
        "window.__petalPendingControlSourceDimensions = {{ width: {source_width}, height: {source_height} }}; window.__petalRemoteControlSourceDimensions?.({source_width}, {source_height});"
    )
}

fn remote_control_active_script(active: bool) -> String {
    format!(
        "window.__petalPendingRemoteControlActive = {active}; window.__petalRemoteControlSetActive?.({active});"
    )
}

fn refresh_control_source_dimensions(
    app: &AppHandle,
    key: RemoteWindowKey,
    source_width: u32,
    source_height: u32,
) {
    let app_main = app.clone();
    let window_id = key.window_id;
    if let Err(e) = app.run_on_main_thread(move || {
        let Some(win) = app_main.get_webview_window(&control_label_for_key(&key)) else {
            return;
        };
        if let Err(e) = win.eval(control_source_dimensions_script(
            source_width,
            source_height,
        )) {
            log::warn!(
                "compositor: failed to update control source dimensions for '{}' to {}x{}: {e}",
                win.label(),
                source_width,
                source_height
            );
        }
    }) {
        log::error!(
            "compositor: run_on_main_thread (refresh control source dimensions {window_id}) failed: {e}"
        );
    }
}

pub fn set_window_media_paused(
    app: &AppHandle,
    owner_identity: &str,
    window_id: u32,
    paused: bool,
) {
    let key = RemoteWindowKey::new(owner_identity, window_id);
    // The header JS lives in the panel's own surface webview now.
    let Some(panel) = app.get_webview_window(&panel_label_for_key(&key)) else {
        return;
    };
    let js = format!(
        "window.__petalRemoteWindowMediaPaused && window.__petalRemoteWindowMediaPaused({paused});"
    );
    if let Err(e) = panel.eval(&js) {
        log::warn!(
            "compositor: failed to eval media-paused header update for window {window_id} '{}': {e}",
            panel.label()
        );
    }
}

/// Create a transparent input-capture overlay for native-viewer remote
/// control plus visible side/bottom resize handles. It stays cursor-
/// interactive so the resize handles work even before remote control is
/// active; the route only forwards remote-control input after explicit
/// enablement.
fn create_control_overlay(
    app: &AppHandle,
    window_id: u32,
    parent_label: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    owner_identity: &str,
) {
    let key = RemoteWindowKey::new(owner_identity, window_id);
    let label = control_label_for_key(&key);
    let source_pixel_size = with_state(|s| {
        s.windows
            .get(&key)
            .and_then(|win| *win.canonical_source_pixel_size.lock_unpoisoned())
    });
    let (source_width, source_height) = control_source_dimensions(source_pixel_size);
    let url = control_route_url(window_id, owner_identity, source_width, source_height);
    create_chrome_webview(
        app,
        parent_label,
        ChromeWebviewSpec {
            label,
            role: "control overlay",
            url,
            x,
            y,
            width,
            height,
            ignore_cursor_events: false,
            always_on_top: None,
        },
    );
}

/// Create the transparent, click-through telepointer overlay webview
/// (`Pointer.svelte`/`NamePill.svelte`), covering exactly the video content
/// area (below the header). Click-through (`set_ignore_cursor_events(true)`)
/// so it never blocks interaction with the video/header beneath it -- same
/// pattern `share_border.rs` already uses for its click-through colored
/// border panels.
fn create_pointer_overlay(
    app: &AppHandle,
    key: &RemoteWindowKey,
    parent_label: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) {
    let window_id = key.window_id;
    let label = pointer_label_for_key(key);
    let url = format!("compositor/pointer.html?windowId={window_id}");
    create_chrome_webview(
        app,
        parent_label,
        ChromeWebviewSpec {
            label,
            role: "pointer overlay",
            url,
            x,
            y,
            width,
            height,
            ignore_cursor_events: true,
            always_on_top: None,
        },
    );
}

/// URL for the ai-chat overlay's route. windowId/owner are the only params it
/// needs, and unlike `control_route_url` they never change across a retired-
/// window reuse (same `RemoteWindowKey` throughout), so no `sourceWidth`/
/// `sourceHeight`-style refresh machinery is required for this overlay.
fn ai_chat_route_url(window_id: u32, owner_identity: &str) -> String {
    format!(
        "compositor/ai-chat.html?windowId={window_id}&owner={}",
        percent_encode(owner_identity),
    )
}

/// Attach the ai-chat overlay to its panel via `addChildWindow:ordered:`
/// (#844 review), restoring the AppKit auto-follow control/pointer get for
/// free through `WebviewWindowBuilder::parent()` -- this overlay is a
/// `PanelBuilder`-built NSPanel with no equivalent tauri-level API. Without
/// this, the panel's `Moved` handler only *corrects* the overlay's position
/// on a DEFERRED main-thread hop after the fact; whether that hop is
/// delivered promptly during `NSEventTrackingRunLoopMode` (an active native
/// drag) is unproven, and an opaque panel visibly trailing or stranding
/// during drag is not acceptable.
///
/// Call ONLY on a path that is about to show (or has just shown) the
/// overlay -- see `add_child_window_above`'s own doc comment for why
/// attaching a hidden window is unsafe. Every call site that shows this
/// overlay must call this; every call site that hides it must call
/// `detach_ai_chat_overlay` below.
fn attach_ai_chat_overlay(app: &AppHandle, key: &RemoteWindowKey) {
    let Some(panel) = app.get_webview_window(&panel_label_for_key(key)) else {
        return;
    };
    let Some(overlay) = app.get_webview_window(&ai_chat_label_for_key(key)) else {
        return;
    };
    if let Err(e) = crate::platform::appkit::add_child_window_above(&panel, &overlay) {
        log::warn!(
            "compositor: failed to attach ai-chat overlay for window {}: {e}",
            key.window_id
        );
    }
}

/// Detach the ai-chat overlay from its panel. Safe (a documented no-op) to
/// call even when the overlay is not currently attached, so every hide path
/// for this overlay can call this unconditionally rather than tracking
/// attach state separately -- see `remove_child_window`'s own doc comment.
fn detach_ai_chat_overlay(app: &AppHandle, key: &RemoteWindowKey) {
    let Some(panel) = app.get_webview_window(&panel_label_for_key(key)) else {
        return;
    };
    let Some(overlay) = app.get_webview_window(&ai_chat_label_for_key(key)) else {
        return;
    };
    if let Err(e) = crate::platform::appkit::remove_child_window(&panel, &overlay) {
        log::warn!(
            "compositor: failed to detach ai-chat overlay for window {}: {e}",
            key.window_id
        );
    }
}

/// Create the receiver-side AI-chat transcript/typed-message overlay (#844):
/// a native NSPanel covering a sub-region of the video content area, ABOVE
/// the video NSView -- the transcript and text input previously lived in an
/// in-webview popover under the panel's header strip
/// (`RemoteWindowHeader.svelte`'s old `.ai-chat-remote-panel`), which the
/// video NSView always occluded and made unclickable.
///
/// Unlike control/pointer (plain `WebviewWindowBuilder` children attached via
/// `.parent()` inside `create_chrome_webview`), this is built through
/// `PanelBuilder::<_, AiChatOverlayPanel>`, a NONACTIVATING NSPanel (same
/// recipe `ai_chat/panel.rs`'s `AiChatPanel` singleton ships:
/// `can_become_key_window: true` + `.style_mask(nonactivating_panel())` +
/// `.no_activate(true)`). A plain child `WebviewWindow` cannot become key
/// (take keyboard focus) while the app is inactive, and calling tao's
/// `set_focus()` on one wraps `[NSApp activateIgnoringOtherApps:YES]` --
/// activating Petal app-wide and potentially surfacing the gallery over
/// whatever app the user was actually working in, every time the "AI chat
/// live" badge is clicked (#844 adversarial-review finding). A nonactivating
/// panel can become key via a raw `makeKeyWindow` call
/// (`platform::appkit::raise_panel_and_make_key`, used by
/// `compositor_set_ai_chat_overlay_open`) without either problem.
///
/// The `AiChatOverlayPanel` type is declared LOCALLY inside this function
/// (not at module scope next to `RemoteWindowPanel`, unlike a first attempt
/// at this fix) -- `tauri_panel!`'s macro expansion brings its own `use`
/// imports into the ENCLOSING scope, and two module-scope invocations in the
/// same file collide (`E0252: define_class defined multiple times`, etc.).
/// `lib.rs`'s `create_hover_tab` (declaring `HoverTabPanel` inside its own
/// function body) is this codebase's established way to sidestep that.
///
/// Built WITHOUT `.parent()` (`PanelBuilder` has no equivalent tauri-level
/// API) -- but this window is STILL attached to the remote window panel via
/// raw `addChildWindow:ordered:` (`platform::appkit::add_child_window_above`),
/// restoring the same real-time drag-follow control/pointer get for free
/// from `WebviewWindowBuilder::parent()`. An EARLIER version of this fix
/// left the overlay unattached, reasoning (by analogy with
/// `share_overlay.rs`'s sharer-side draw/telepointer overlay) that the
/// panel's `Moved`-handler deferred `sync_chrome_to_panel_frame_deferred`
/// call was enough on its own -- that call is only a size/inset
/// *correction* for the addChildWindow-attached overlays, not a substitute
/// for the attachment itself, and whether it's even delivered promptly
/// during `NSEventTrackingRunLoopMode` (an active native drag) was unproven
/// (#844 adversarial re-review). The attach/detach calls themselves live in
/// every show/hide path for this overlay (`compositor_set_ai_chat_overlay_open`,
/// `show_retired_window_on_main`, `reveal_remote_window_after_first_frame_on_main`,
/// `remove_window`), NOT here at creation time -- see
/// `attach_ai_chat_overlay`'s own doc comment for why attaching while hidden
/// is unsafe. `order_chrome_above_panel` still separately keeps it stacked
/// above the panel via `orderWindow:relativeTo:`.
///
/// Created hidden, same as control/pointer; shown only on demand by
/// `compositor_set_ai_chat_overlay_open` while the header's disclosure badge
/// is open.
fn create_ai_chat_overlay(
    app: &AppHandle,
    key: &RemoteWindowKey,
    panel_x: f64,
    panel_y: f64,
    panel_width: f64,
    panel_height: f64,
) {
    use tauri_nspanel::tauri_panel;

    tauri_panel! {
        panel!(AiChatOverlayPanel {
            config: {
                can_become_key_window: true,
                is_floating_panel: true
            }
        })
    }

    let window_id = key.window_id;
    let label = ai_chat_label_for_key(key);
    let url = ai_chat_route_url(window_id, &key.owner_identity);
    let frame = ai_chat_overlay_frame_for_panel_bounds(panel_x, panel_y, panel_width, panel_height);
    match PanelBuilder::<_, AiChatOverlayPanel>::new(app, &label)
        .url(WebviewUrl::App(url.into()))
        .title("")
        .position(tauri::Position::Logical(tauri::LogicalPosition {
            x: frame.x,
            y: frame.y,
        }))
        .level(PanelLevel::Normal)
        .size(tauri::Size::Logical(tauri::LogicalSize {
            width: frame.width,
            height: frame.height,
        }))
        .has_shadow(true)
        .transparent(true)
        .no_activate(true)
        .style_mask(tauri_nspanel::StyleMask::empty().nonactivating_panel())
        .corner_radius(SCREENSHARE_BORDER_RADIUS_PX)
        .with_window(|w| {
            w.decorations(false)
                .resizable(false)
                .accept_first_mouse(true)
                .skip_taskbar(true)
        })
        .collection_behavior(CollectionBehavior::new().managed())
        .build()
    {
        Ok(_) => {
            if let Some(win) = app.get_webview_window(&label) {
                let _ = win.hide();
                crate::webview_transparency::apply_or_retry(app, &win);
            }
        }
        Err(e) => log::warn!(
            "compositor: failed to create ai-chat overlay '{label}' for window {window_id}: {e}"
        ),
    }
}

/// Broadcast whenever `CompositorWindow.ai_chat_overlay_open` changes, so
/// RemoteWindowHeader.svelte can treat Rust as the single source of truth
/// for the badge's open/closed state instead of keeping its own optimistic
/// local copy (#844 adversarial-review finding: the overlay's own Escape-
/// to-close called straight into `compositor_set_ai_chat_overlay_open`,
/// leaving the header's local state -- and therefore the badge -- stuck
/// showing "open" until the next click was wasted reconciling instead of
/// actually reopening; separately, a retired-window restore reloads the
/// header webview fresh, which reset local state to its default `false`
/// even while the overlay could still be visible). `compositor_surface`'s
/// own webview DOES receive `app.emit` (unlike the control/pointer/ai-chat
/// CHILD overlays -- see `ai_chat/topic.rs`'s doc comment on
/// `push_remote_state_to_overlay` for why those can't rely on it).
pub const EVENT_AI_CHAT_OVERLAY_OPEN_CHANGED: &str = "ai-chat-overlay-open-changed";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AiChatOverlayOpenChanged {
    window_id: u32,
    owner_identity: String,
    open: bool,
}

fn emit_ai_chat_overlay_open_changed(app: &AppHandle, key: &RemoteWindowKey, open: bool) {
    let _ = app.emit(
        EVENT_AI_CHAT_OVERLAY_OPEN_CHANGED,
        AiChatOverlayOpenChanged {
            window_id: key.window_id,
            owner_identity: key.owner_identity.clone(),
            open,
        },
    );
}

/// Read the AI-chat overlay's current open/closed state (#844). The header
/// asks this once on mount -- the same "ask AND listen" shape
/// `aiChatRemoteSession`/`refreshAiChatSession` already use -- so a header
/// that (re)mounts after the overlay was toggled (a retired-window restore
/// reloads this webview fresh) starts from the real state instead of a
/// hardcoded `false` that could already be wrong.
#[tauri::command]
pub fn compositor_ai_chat_overlay_is_open(window_id: u32, owner_identity: Option<String>) -> bool {
    let Some(key) = resolve_open_window_key(window_id, owner_identity.as_deref()) else {
        return false;
    };
    with_state(|s| {
        s.windows
            .get(&key)
            .map(|w| w.ai_chat_overlay_open)
            .unwrap_or(false)
    })
}

/// Show/hide the receiver-side AI-chat overlay (#844), following
/// `RemoteWindowHeader.svelte`'s disclosure badge. Runs directly, no
/// `run_on_main_thread` hop: like `set_remote_control_active`'s
/// `#[tauri::command]` callers, this is invoked synchronously from the
/// frontend's own IPC call, which already arrives on the main thread.
#[tauri::command]
pub fn compositor_set_ai_chat_overlay_open(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
    open: bool,
) {
    let Some(key) = resolve_open_window_key(window_id, owner_identity.as_deref()) else {
        log::debug!(
            "compositor: ai-chat overlay toggle (open={open}) for missing/closed window {window_id}"
        );
        return;
    };
    with_state(|s| {
        if let Some(win) = s.windows.get_mut(&key) {
            win.ai_chat_overlay_open = open;
        }
    });
    emit_ai_chat_overlay_open_changed(&app, &key, open);
    let label = ai_chat_label_for_key(&key);
    let Some(overlay) = app.get_webview_window(&label) else {
        return;
    };
    if open {
        // Re-dock to the panel's CURRENT frame before showing -- the panel
        // may have moved/resized while this overlay sat hidden with no
        // reposition calls delivered to it.
        if let Some(panel) = app.get_webview_window(&panel_label_for_key(&key)) {
            sync_chrome_to_panel_frame(&app, &key, &panel);
        }
        let _ = overlay.show();
        // #844 review: attach AFTER show(), not before -- addChildWindow's
        // `ordered:` can itself reveal a hidden window, so attaching first
        // would risk an extra, uncontrolled reveal path.
        attach_ai_chat_overlay(&app, &key);
        // sync_chrome_to_panel_frame's own order_chrome_above_panel call
        // above ran while this overlay was still hidden (its is_visible()
        // check would have skipped it) -- order again now that it's shown.
        order_chrome_above_panel(&app, &key);
        // #844 adversarial-review fix: NOT tao's `set_focus()` -- that wraps
        // `[NSApp activateIgnoringOtherApps:YES]` (see #678's comment on
        // `raise_panel_only`, a few screens down), which would activate
        // Petal app-wide and could surface the gallery over whatever app the
        // user was actually working in, every time this badge is clicked.
        // `AiChatOverlayPanel`'s nonactivating style mask + a raw
        // `makeKeyWindow` call (via `raise_panel_and_make_key`, the same
        // #356 recipe `activate_window` uses for the remote-window panel
        // itself) gives the composer real keyboard focus WITHOUT activating
        // the app.
        if let Err(e) = crate::platform::appkit::raise_panel_and_make_key(&overlay) {
            log::warn!("compositor: failed to key ai-chat overlay for window {window_id}: {e}");
        }
        log::info!("compositor: ai-chat overlay opened for window {window_id}");
    } else {
        // Detach BEFORE hiding: this window is about to disappear from the
        // parent's child list regardless, and detaching first keeps the two
        // operations from interacting.
        detach_ai_chat_overlay(&app, &key);
        let _ = overlay.hide();
        log::info!("compositor: ai-chat overlay closed for window {window_id}");
    }
}

/// Push one more real decoded frame (a `CVPixelBufferRef`, see
/// `native_display.rs`) into `window_id`'s display layer. No-ops if that
/// window isn't open (e.g. a frame race right after `remove_window`).
/// `source_width`/`source_height` are the frame's real pixel dimensions --
/// the FIRST call for a given window resizes the panel's content area (and
/// re-centers the display layer) to match the real source aspect ratio,
/// correcting `DEFAULT_CONTENT_WIDTH`/`HEIGHT`'s placeholder guess (see
/// `ensure_window`'s doc comment on why a placeholder is needed at all).
pub fn push_frame(
    app: &AppHandle,
    owner_identity: &str,
    window_id: u32,
    cv_pixel_buffer: *mut std::ffi::c_void,
    source_width: u32,
    source_height: u32,
) {
    let key = RemoteWindowKey::new(owner_identity, window_id);

    if display_enqueue_paused() {
        // #259/#264: the display is confirmed asleep -- still record frame
        // receipt (cheap; keeps `RemoteWindowDebugStats`/the no-frame
        // watchdog's parity intact) but skip building the `CMSampleBuffer`
        // and scheduling any main-thread `AVSampleBufferDisplayLayer`
        // enqueue entirely. See `set_display_enqueue_paused`'s doc comment
        // for why this is safe to do without a keyframe request on resume.
        with_state(|s| {
            if let Some(win) = s.windows.get_mut(&key) {
                win.last_frame_received_ms
                    .store(now_ms(), Ordering::Relaxed);
                win.frames_received.fetch_add(1, Ordering::Relaxed);
            }
        });
        return;
    }

    let (schedule_enqueue, needs_resize, source_pixel_size_changed, initial_resize) =
        with_state(|s| {
            let Some(win) = s.windows.get_mut(&key) else {
                return (false, None, false, false);
            };
            // Build the CMSampleBuffer HERE, on the decode thread, while the
            // CVPixelBuffer is still alive (it retains the buffer internally). The
            // actual enqueue still happens on the main thread, but the queued
            // drain below batches bursts into one main-thread hop per window.
            let sample = {
                let Some(display) = win.display.as_ref() else {
                    return (false, None, false, false);
                };
                display.prepare_sample(cv_pixel_buffer)
            };
            let source_kind = win.source_kind;
            let decoded_pixel_size_changed = if source_width > 0 && source_height > 0 {
                let mut source_pixel_size = win.source_pixel_size.lock_unpoisoned();
                if *source_pixel_size == Some((source_width, source_height)) {
                    false
                } else {
                    *source_pixel_size = Some((source_width, source_height));
                    true
                }
            } else {
                false
            };
            let mut canonical_size = *win.canonical_source_pixel_size.lock_unpoisoned();
            // Petal View has one canonical track, not simulcast layers: its
            // decoded dimensions are authoritative ROI geometry. Adopt both
            // growth and shrinkage so the receiver window, telepointer, and
            // remote-control mapping follow a live selector resize. Ordinary
            // display/window shares retain metadata dimensions so a lower
            // decoded layer cannot resize their user window.
            if source_kind == SharedSourceKind::DisplayRegion
                && region_frame_is_new_source_size(canonical_size, (source_width, source_height))
            {
                canonical_size = Some((source_width, source_height));
                *win.canonical_source_pixel_size.lock_unpoisoned() = canonical_size;
            }
            let logical_source_size =
                canonical_source_size_for_frame(canonical_size, source_width, source_height);
            let (cur_w, cur_h) = *win.panel_content_size.lock_unpoisoned();
            // Not sourced from the decoded frame itself: the vendored LiveKit
            // FrameMetadata trailer only carries frame_id/timestamp today, no
            // scale field, so a coherent per-frame scale isn't available without
            // a vendor patch (out of scope here). Falls back to the
            // participant-metadata-derived value, same as before this change --
            // the frame-px/metadata-scale incoherence race this could have
            // closed is a secondary, unproven amplifier per review, not the
            // primary bug this issue fixes.
            let source_scale = valid_source_scale(win.source_scale);
            let presentation = if let Some((width, height)) = logical_source_size {
                Some(source_presentation_size_points(width, height, source_scale))
            } else {
                None
            };
            let previous_presentation = *win.source_presentation_size.lock_unpoisoned();
            let source_presentation_changed = if let Some(presentation) = presentation {
                let mut previous = win.source_presentation_size.lock_unpoisoned();
                let changed = previous
                    .map(|old| !logical_size_matches(old, presentation))
                    .unwrap_or(true);
                if changed {
                    *previous = Some(presentation);
                }
                changed
            } else {
                false
            };
            let target_w = presentation.map(|(w, _)| w).unwrap_or(cur_w);
            let target_h = presentation.map(|(_, h)| h).unwrap_or(cur_h);
            let target = if source_presentation_changed {
                Some(InitialResizeTarget {
                    source_width_px: logical_source_size.map(|(w, _)| w).unwrap_or(source_width),
                    source_height_px: logical_source_size.map(|(_, h)| h).unwrap_or(source_height),
                    source_scale,
                    fallback_content_w: target_w,
                    fallback_content_h: target_h,
                })
            } else {
                None
            };
            let needs_resize = target.and_then(|target| {
                match resize_decision(
                    previous_presentation,
                    presentation,
                    resize_gesture_in_progress(win),
                ) {
                    ResizeDecision::Apply => Some(target),
                    ResizeDecision::Latch => {
                        *win.pending_source_resize.lock_unpoisoned() = Some(target);
                        None
                    }
                    ResizeDecision::Ignore => None,
                }
            });
            let schedule_enqueue = sample
                .map(|sample| {
                    win.pending_display_samples.push(PendingDisplaySample {
                        sample,
                        source_width,
                        source_height,
                    })
                })
                .unwrap_or(false);
            win.last_frame_received_ms
                .store(now_ms(), Ordering::Relaxed);
            win.frames_received.fetch_add(1, Ordering::Relaxed);
            let initial_resize = previous_presentation.is_none() && needs_resize.is_some();
            (
                schedule_enqueue,
                needs_resize,
                decoded_pixel_size_changed,
                initial_resize,
            )
        });

    if source_pixel_size_changed {
        let control_size = with_state(|s| {
            s.windows
                .get(&key)
                .and_then(|win| *win.canonical_source_pixel_size.lock_unpoisoned())
        })
        .or((source_width > 0 && source_height > 0).then_some((source_width, source_height)));
        if let Some((width, height)) = control_size {
            refresh_control_source_dimensions(app, key.clone(), width, height);
        }
    }

    // Enqueue on the MAIN THREAD -- AVSampleBufferDisplayLayer is a CALayer and
    // enqueuing off the main thread silently renders nothing (the black-window
    // bug). Prepared samples are queued per window and drained in one scheduled
    // hop, reducing dispatch pressure without changing the layer thread
    // contract. Re-look-up the window inside the hop in case it was torn down.
    if schedule_enqueue {
        let app_main = app.clone();
        let key_for_main = key.clone();
        if let Err(e) = app.run_on_main_thread(move || {
            drain_pending_display_samples_on_main(&app_main, &key_for_main);
        }) {
            log::error!("compositor: run_on_main_thread (frame drain {window_id}) failed: {e}");
            with_state(|s| {
                if let Some(win) = s.windows.get_mut(&key) {
                    win.pending_display_samples.clear();
                } else if let Some(win) = s.retired.get_mut(&key) {
                    win.pending_display_samples.clear();
                }
            });
        }
    }

    if let Some(target) = needs_resize {
        log::info!(
            "compositor: window {window_id} learned source presentation size {:.0}x{:.0}; applying initial receiver work-area cap if needed",
            target.fallback_content_w,
            target.fallback_content_h
        );
        if initial_resize {
            resize_initial_to_source(app, owner_identity, window_id, target);
        } else {
            resize_source_preserving_user_size(app, owner_identity, window_id, target);
        }
        // Occlusion diagnostics. Fires on the first real frame's size
        // correction AND on any later genuine mid-session source resize
        // (#416) -- not just once per window, despite the log line's own
        // "first frame" wording (kept for continuity with existing log
        // parsing/greps; the log message itself is not user-facing).
        crate::window_diag::log_window_stack(app, &format!("first frame for window {window_id}"));
    }
}

fn drain_pending_display_samples_on_main(app: &AppHandle, key: &RemoteWindowKey) {
    let mut cleared_hold = false;
    let geometry = app
        .get_webview_window(&panel_label_for_key(key))
        .and_then(|window| content_geometry(&window));
    let should_reveal = with_state(|s| {
        let Some(win) = s.windows.get_mut(key) else {
            if let Some(win) = s.retired.get_mut(key) {
                win.pending_display_samples.clear();
            }
            return false;
        };
        let pending = win.pending_display_samples.drain_scheduled();
        if pending.is_empty() {
            return false;
        }
        let Some(display) = win.display.as_ref() else {
            return false;
        };
        for pending in pending {
            if let Some((content_w, content_h, scale)) = geometry {
                display.set_contents_scale(scale);
                display.update_filter_for_geometry(
                    pending.source_width,
                    pending.source_height,
                    content_w,
                    content_h,
                    scale,
                );
            }
            display.enqueue_prepared(&pending.sample);
            record_display_enqueue(win, now_ms());
            win.layer_has_content = true;
        }
        // A frame reached the layer, so this window is live again. Clearing the
        // hold here -- rather than on any event -- means the honest "paused"
        // label goes away exactly when real pixels resume, including when a
        // stalled track recovers with no new `TrackSubscribed` (#627).
        let resumed_from_hold = win.held_reason.take().is_some();
        if win.revealed_first_frame {
            if resumed_from_hold {
                cleared_hold = true;
            }
            false
        } else {
            win.revealed_first_frame = true;
            true
        }
    });
    if cleared_hold {
        log::info!(
            "compositor: window {} from '{}' resumed live media; clearing held-frame state",
            key.window_id,
            key.owner_identity
        );
        set_window_media_paused(app, &key.owner_identity, key.window_id, false);
    }
    if should_reveal {
        reveal_remote_window_after_first_frame_on_main(app, key);
    }
}

/// Resize for the automatic first-frame correction. Unlike the explicit
/// header "fit to source" command, this caps the initial total compositor
/// window to 80% of the receiver's current monitor work area so a huge remote
/// display never opens with its drag header out of reach.
/// Why a programmatic resize is being issued. Source-driven sizing yields to
/// an in-progress user gesture and is latched for pointer-up; an explicit user
/// command is never suppressed (#416).
#[derive(Debug, Clone, Copy)]
enum ProgrammaticResizeIntent {
    SourceDriven(InitialResizeTarget),
    UserCommanded,
}

#[derive(Debug, Clone, Copy)]
struct InitialResizeTarget {
    source_width_px: u32,
    source_height_px: u32,
    source_scale: f64,
    fallback_content_w: f64,
    fallback_content_h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeDecision {
    Ignore,
    Apply,
    Latch,
}

/// Pure policy for a decoded source-size observation. The panel size is
/// intentionally absent: a user drag changes the panel, not the source.
fn resize_decision(
    previous: Option<(f64, f64)>,
    current: Option<(f64, f64)>,
    user_resize_in_progress: bool,
) -> ResizeDecision {
    let source_changed = match (previous, current) {
        (Some(previous), Some(current)) => !logical_size_matches(previous, current),
        (None, Some(_)) => true,
        _ => false,
    };
    if !source_changed {
        ResizeDecision::Ignore
    } else if user_resize_in_progress {
        ResizeDecision::Latch
    } else {
        ResizeDecision::Apply
    }
}

fn logical_size_matches(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() <= 0.5 && (a.1 - b.1).abs() <= 0.5
}

/// Region frames have no lower simulcast geometry to preserve: a valid decoded
/// size different from the current canonical size is a live ROI resize.
fn region_frame_is_new_source_size(
    canonical: Option<(u32, u32)>,
    decoded: (u32, u32),
) -> bool {
    decoded.0 > 0 && decoded.1 > 0 && canonical != Some(decoded)
}

/// Select the publisher's dimensions whenever they are available. A decoded
/// frame is allowed to be a lower simulcast layer and therefore must never
/// redefine the source geometry used for ordinary shares.
fn canonical_source_size_for_frame(
    canonical: Option<(u32, u32)>,
    decoded_width: u32,
    decoded_height: u32,
) -> Option<(u32, u32)> {
    canonical.or_else(|| {
        (decoded_width > 0 && decoded_height > 0).then_some((decoded_width, decoded_height))
    })
}

/// True while a user resize gesture is genuinely in progress.
///
/// The explicit active bit is authoritative: a held pointer may pause longer
/// than `USER_RESIZE_TTL` without allowing a source update to fight the next
/// pointer move. The short TTL is retained only as a recovery path for an IPC
/// sequence that observed begin but failed to leave the active bit set. A
/// stale active bit is bounded by `MAX_USER_RESIZE_GESTURE_MS` so a lost
/// pointer-up/finalize cannot suppress source reconciliation forever.
fn resize_gesture_in_progress(win: &CompositorWindow) -> bool {
    let now = now_ms();
    if win.user_resize_active.load(Ordering::Acquire) {
        let since = win.user_resize_active_since_ms.load(Ordering::Relaxed);
        if now.saturating_sub(since) < MAX_USER_RESIZE_GESTURE_MS {
            return true;
        }
        // Clear the stale state as part of the fallback, so subsequent frame
        // observations do not repeatedly treat the same lost gesture as live.
        win.user_resize_active.store(false, Ordering::Release);
        win.user_resize_until_ms.store(0, Ordering::Relaxed);
        return false;
    }
    // This branch is deliberately secondary. In particular, finalize clears
    // the TTL before it clears the active bit, so a delayed non-final resize
    // IPC cannot resurrect a completed drag by refreshing this deadline.
    win.user_resize_until_ms.load(Ordering::Relaxed) > now
}

/// Decide whether a retire -> reveal reuse cycle keeps the in-flight user
/// resize gesture (#416). Returns true when the gesture was carried.
///
/// The gesture guard is correct; the state it reads did not survive the window
/// lifecycle. `ensure_window`'s reuse path used to clear `user_resize_active`
/// unconditionally, so a source republish that retired and revealed the panel
/// mid-drag handed the reveal-time source resize a cleared bit -- it concluded
/// "no gesture in progress" and moved the panel under a held pointer.
///
/// `resize_gesture_in_progress` stays the single authority on liveness, so the
/// `MAX_USER_RESIZE_GESTURE_MS` backstop keeps working unchanged: it clears a
/// stale bit here exactly as it would anywhere else, and a reveal can never
/// revive or extend one. A live gesture is carried by leaving the state
/// untouched -- in particular `user_resize_active_since_ms`, the backstop's
/// clock, is never refreshed, so the reveal cannot postpone expiry.
fn carry_resize_gesture_across_reveal(win: &CompositorWindow) -> bool {
    if resize_gesture_in_progress(win) {
        return true;
    }
    win.user_resize_until_ms.store(0, Ordering::Relaxed);
    win.user_resize_active.store(false, Ordering::Relaxed);
    win.user_resize_active_since_ms.store(0, Ordering::Relaxed);
    false
}

/// Picks up a fresh canonical publisher size for a window that's already
/// open (a republish under the same window_id -- see call site in
/// `ensure_window`). Runs the SAME resize-decision policy `push_frame` uses,
/// through the mid-session (preserve-user-size) apply path -- this can never
/// be the "initial" resize since the window already existed.
fn update_canonical_source_size_on_republish(
    app: &AppHandle,
    key: &RemoteWindowKey,
    canonical_source_size: Option<(u32, u32)>,
) {
    let Some((width, height)) = canonical_source_size else {
        return;
    };
    let (epoch, generation) = with_state(|s| {
        let win = s.windows.get(key)?;
        let generation = win
            .canonical_source_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        Some((
            win.canonical_source_epoch.load(Ordering::Acquire),
            generation,
        ))
    })
    .unwrap_or((0, 0));
    if epoch == 0 {
        return;
    }
    let app_main = app.clone();
    let key = key.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        let target = with_state(|s| {
            let win = s.windows.get(&key)?;
            if !canonical_source_update_is_current(
                win.canonical_source_epoch.load(Ordering::Acquire),
                win.canonical_source_generation.load(Ordering::Acquire),
                epoch,
                generation,
            ) {
                // A newer republish or a retired/reused share instance owns
                // this callback; stale canonical dimensions must not win.
                return None;
            }
            let previous_canonical = *win.canonical_source_pixel_size.lock_unpoisoned();
            if previous_canonical == Some((width, height)) {
                return None;
            }
            *win.canonical_source_pixel_size.lock_unpoisoned() = Some((width, height));
            let source_scale = valid_source_scale(win.source_scale);
            let presentation = source_presentation_size_points(width, height, source_scale);
            let previous_presentation = *win.source_presentation_size.lock_unpoisoned();
            let (fallback_w, fallback_h) = *win.panel_content_size.lock_unpoisoned();
            let decision = resize_decision(
                previous_presentation,
                Some(presentation),
                resize_gesture_in_progress(win),
            );
            *win.source_presentation_size.lock_unpoisoned() = Some(presentation);
            let target = InitialResizeTarget {
                source_width_px: width,
                source_height_px: height,
                source_scale,
                fallback_content_w: fallback_w,
                fallback_content_h: fallback_h,
            };
            match decision {
                ResizeDecision::Apply => Some(target),
                ResizeDecision::Latch => {
                    *win.pending_source_resize.lock_unpoisoned() = Some(target);
                    None
                }
                ResizeDecision::Ignore => None,
            }
        });
        if let Some(target) = target {
            resize_source_preserving_user_size(
                &app_main,
                &key.owner_identity,
                key.window_id,
                target,
            );
        }
    }) {
        log::error!(
            "compositor: run_on_main_thread (update_canonical_source_size_on_republish) failed: {e}"
        );
    }
}

#[inline]
fn canonical_source_update_is_current(
    current_epoch: u64,
    current_generation: u64,
    candidate_epoch: u64,
    candidate_generation: u64,
) -> bool {
    current_epoch == candidate_epoch && current_generation == candidate_generation
}

fn resize_initial_to_source(
    app: &AppHandle,
    owner_identity: &str,
    window_id: u32,
    target: InitialResizeTarget,
) {
    let app_main = app.clone();
    let key = RemoteWindowKey::new(owner_identity, window_id);
    if let Err(e) = app.run_on_main_thread(move || {
        let label = panel_label_for_key(&key);
        let Some(window) = app_main.get_webview_window(&label) else {
            return;
        };
        let receiver_scale = window.scale_factor().unwrap_or(1.0);
        let (target_w, target_h) = work_area_size_for_window(&app_main, &window)
            .map(|(work_w, work_h)| {
                initial_content_size_within_work_area(
                    target.source_width_px,
                    target.source_height_px,
                    target.source_scale,
                    receiver_scale,
                    work_w,
                    work_h,
                )
            })
            .unwrap_or((target.fallback_content_w, target.fallback_content_h));
        resize_to_content_on_main(
            &app_main,
            &key,
            &window,
            target_w,
            target_h,
            ProgrammaticResizeIntent::SourceDriven(target),
        );
    }) {
        log::error!(
            "compositor: run_on_main_thread (resize_initial_to_source {window_id}) failed: {e}"
        );
    }
}

/// Apply a genuine mid-session source change without re-running the first-open
/// 80%-of-work-area policy. Keep the user's current content scale/width and
/// only change the other dimension to the new source aspect, then clamp.
fn resize_source_preserving_user_size(
    app: &AppHandle,
    owner_identity: &str,
    window_id: u32,
    target: InitialResizeTarget,
) {
    let app_main = app.clone();
    let key = RemoteWindowKey::new(owner_identity, window_id);
    if let Err(e) = app.run_on_main_thread(move || {
        let label = panel_label_for_key(&key);
        let Some(window) = app_main.get_webview_window(&label) else {
            return;
        };
        let current = with_state(|s| {
            s.windows
                .get(&key)
                .map(|win| *win.panel_content_size.lock_unpoisoned())
        })
        .unwrap_or((target.fallback_content_w, target.fallback_content_h));
        let source = source_presentation_size_points(
            target.source_width_px,
            target.source_height_px,
            target.source_scale,
        );
        let work_area = work_area_size_for_window(&app_main, &window);
        let desired = proportional_content_size_for_source_change(
            current,
            source.0 / source.1.max(1.0),
            work_area,
        );
        resize_to_content_on_main(
            &app_main,
            &key,
            &window,
            desired.0,
            desired.1,
            ProgrammaticResizeIntent::SourceDriven(target),
        );
    }) {
        log::error!("compositor: run_on_main_thread (resize_source_preserving_user_size {window_id}) failed: {e}");
    }
}

fn proportional_content_size_for_source_change(
    current: (f64, f64),
    aspect: f64,
    work_area: Option<(f64, f64)>,
) -> (f64, f64) {
    let mut width = current.0.max(MIN_RESIZE_CONTENT_WIDTH);
    let mut height = (width / aspect.max(0.01)).max(MIN_RESIZE_CONTENT_HEIGHT);
    if let Some((work_w, work_h)) = work_area {
        let max_h = (work_h - HEADER_HEIGHT).max(1.0);
        let factor = (work_w / width).min(max_h / height).min(1.0);
        width *= factor;
        height *= factor;
    }
    (
        width.round().max(MIN_RESIZE_CONTENT_WIDTH),
        height.round().max(MIN_RESIZE_CONTENT_HEIGHT),
    )
}

/// Resize the panel + header + pointer overlay so the video content area is
/// exactly `content_w`x`content_h` (the real source resolution), aspect
/// preserved (SPEC.md §4.4: "resizable with aspect lock"). Called once when
/// the first real frame's size is learned, and by `fit_to_source` (the
/// header's "fit to source size" button, SPEC.md §4.4).
fn resize_to_source(
    app: &AppHandle,
    owner_identity: &str,
    window_id: u32,
    content_w: f64,
    content_h: f64,
) {
    // First-frame resizes are reached from the LiveKit decode task. Everything
    // below touches AppKit/CoreAnimation state (`WebviewWindow` sizing and the
    // `AVSampleBufferDisplayLayer` hosting view), so mirror the frame enqueue
    // path and marshal the whole resize to the main thread.
    let app_main = app.clone();
    let key = RemoteWindowKey::new(owner_identity, window_id);
    if let Err(e) = app.run_on_main_thread(move || {
        let label = panel_label_for_key(&key);
        let Some(window) = app_main.get_webview_window(&label) else {
            return;
        };
        let receiver_scale = window.scale_factor().unwrap_or(1.0);
        let source = with_state(|s| {
            s.windows.get(&key).and_then(|win| {
                let source_pixel_size = *win.canonical_source_pixel_size.lock_unpoisoned();
                let (source_width_px, source_height_px) = source_pixel_size?;
                Some((
                    source_width_px,
                    source_height_px,
                    valid_source_scale(win.source_scale),
                ))
            })
        });
        let target = source
            .and_then(|(source_width_px, source_height_px, source_scale)| {
                work_area_size_for_window(&app_main, &window).map(|(work_w, work_h)| {
                    fit_to_source_content_size_within_work_area(
                        source_width_px,
                        source_height_px,
                        source_scale,
                        receiver_scale,
                        work_w,
                        work_h,
                    )
                })
            })
            .unwrap_or((content_w, content_h));
        // Explicit user "fit to source" command: never suppressed by a stale
        // gesture bit, and nothing to latch.
        resize_to_content_on_main(
            &app_main,
            &key,
            &window,
            target.0,
            target.1,
            ProgrammaticResizeIntent::UserCommanded,
        );
    }) {
        log::error!("compositor: run_on_main_thread (resize_to_source {window_id}) failed: {e}");
    }
}

fn resize_to_content_on_main(
    app: &AppHandle,
    key: &RemoteWindowKey,
    window: &tauri::WebviewWindow,
    content_w: f64,
    content_h: f64,
    intent: ProgrammaticResizeIntent,
) {
    let transaction = with_state(|s| {
        let window = s.windows.get(key)?;
        // Do NOT overwrite settled panel state here. `set_size` may fail or
        // deliver no callback; the transaction carries desired geometry until
        // we have an actual native size to reconcile.
        match intent {
            ProgrammaticResizeIntent::UserCommanded => {
                prepare_user_commanded_resize_request(window, content_w, content_h)
            }
            ProgrammaticResizeIntent::SourceDriven(target) => {
                let transaction = prepare_programmatic_resize_request(window, content_w, content_h);
                // #416: a user gesture began between the source-resize decision
                // and this request. Latch inside the SAME critical section that
                // would have created the transaction, so a genuine sender-side
                // resize is deferred to pointer-up, not silently discarded.
                if transaction.is_none() && resize_gesture_in_progress(window) {
                    *window.pending_source_resize.lock_unpoisoned() = Some(target);
                }
                transaction
            }
        }
    });
    let Some(transaction) = transaction else {
        trace_panel_geometry(
            match intent {
                ProgrammaticResizeIntent::UserCommanded => "refused-user-commanded",
                ProgrammaticResizeIntent::SourceDriven(_) => "refused-source-driven",
            },
            key.window_id,
            content_w,
            HEADER_HEIGHT + content_h,
            with_state(|s| s.windows.get(key).map(resize_gesture_in_progress)),
        );
        return;
    };
    trace_panel_geometry(
        match intent {
            ProgrammaticResizeIntent::UserCommanded => "programmatic-user-commanded",
            ProgrammaticResizeIntent::SourceDriven(_) => "programmatic-source-driven",
        },
        key.window_id,
        content_w,
        HEADER_HEIGHT + content_h,
        with_state(|s| s.windows.get(key).map(resize_gesture_in_progress)),
    );

    // This `set_size` is deliberately OUTSIDE the `with_state` section that
    // created the transaction. Safe only because sync Tauri commands and
    // `run_on_main_thread` closures both serialize on the main run loop. If
    // `compositor_begin_resize`/`compositor_resize_window` is ever made async,
    // the panel can physically snap mid-drag again while the listener
    // correctly ignores the callback -- #416's symptom with no state trace.
    if let Err(error) = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
        width: content_w,
        height: HEADER_HEIGHT + content_h,
    })) {
        log::warn!(
            "compositor: failed programmatic resize generation {} for window {}: {error}",
            transaction.generation,
            key.window_id,
        );
        with_state(|s| {
            if let Some(window) = s.windows.get(key) {
                discard_programmatic_resize_if_current(window, transaction.generation);
            }
        });
        return;
    }
    // A cancelled predecessor can have its callback coalesced/dropped. Always
    // schedule one generation-scoped native-bounds reconciliation so the
    // current request settles even if no further Resize event is delivered.
    schedule_programmatic_resize_reconciliation(app, key, transaction);

    // `set_size` may synchronously run the listener. If it did not, query the
    // actual native size and reconcile only that geometry; a later callback is
    // retained as an ignored acknowledgement rather than reinterpreted as a
    // user resize. This covers no-callback/no-op native paths without ever
    // publishing desired geometry as settled geometry.
    let scale = window.scale_factor().unwrap_or(1.0);
    let actual = window.inner_size().ok().map(|size| {
        (
            size.width as f64 / scale,
            (size.height as f64 / scale - HEADER_HEIGHT).max(1.0),
        )
    });
    if let Some((actual_width, actual_height)) = actual {
        let settled = with_state(|s| {
            s.windows.get(key).and_then(|state| {
                settle_programmatic_resize_if_current(state, transaction.generation)
            })
        });
        let Some(settled) = settled else {
            // The native listener already reconciled this transaction.
            return;
        };
        with_state(|s| {
            if let Some(state) = s.windows.get(key) {
                // The listener did not run before the native-size query. Its
                // later callback describes these settled native bounds, not a
                // user gesture; retain one bounded acknowledgement for it.
                retain_late_successful_resize_ack(state, actual_width, actual_height);
            }
        });
        if !settled_geometry_within_one_physical_pixel(
            settled.content_width,
            settled.content_height,
            actual_width,
            actual_height,
            scale,
        ) {
            log::warn!(
                "compositor: programmatic resize generation {} for window {} acknowledged at {:.1}x{:.1}, not requested {:.1}x{:.1}; reconciling native geometry",
                settled.generation,
                key.window_id,
                actual_width,
                actual_height,
                settled.content_width,
                settled.content_height,
            );
        }
        settle_panel_content_geometry(app, key, actual_width, actual_height, scale, false);
    } else {
        // `set_size` succeeded but AppKit did not provide a readable size.
        // Keep the request pending: a late native callback is still Petal's
        // acknowledgement and must not fall into the user/aspect path.
        // The old settled geometry remains published until that callback.
        log::warn!(
            "compositor: programmatic resize generation {} for window {} had no readable native geometry; retaining pending acknowledgement and prior settled geometry",
            transaction.generation,
            key.window_id,
        );
    }
}

fn sync_chrome_to_panel_frame(
    app: &AppHandle,
    key: &RemoteWindowKey,
    panel_window: &tauri::WebviewWindow,
) {
    let (Ok(pos), Ok(size)) = (panel_window.outer_position(), panel_window.outer_size()) else {
        return;
    };
    let scale = panel_window.scale_factor().unwrap_or(1.0);
    let frames = chrome_frames_for_panel_bounds(
        pos.x as f64 / scale,
        pos.y as f64 / scale,
        size.width as f64 / scale,
        size.height as f64 / scale,
    );
    reposition_chrome_frame(app, &control_label_for_key(key), frames.control);
    reposition_chrome_frame(app, &pointer_label_for_key(key), frames.pointer);
    reposition_chrome_frame(app, &ai_chat_label_for_key(key), frames.ai_chat);
    order_chrome_above_panel(app, key);
}

/// Enqueue `sync_chrome_to_panel_frame` to run on the NEXT main-thread turn,
/// rather than synchronously. Repositioning an `addChildWindow` child from
/// inside the parent panel's own Moved/Resized event handler does not stick
/// (AppKit reasserts the child follow-offset as the handler unwinds); running
/// after the handler returns lets the reposition land.
///
/// `AppHandle::run_on_main_thread` (tauri-runtime-wry's `send_user_message`)
/// only actually defers via the async event-loop proxy when called from a
/// thread OTHER than the main thread -- when the caller is already on the
/// main thread, it runs the closure synchronously, inline, immediately (see
/// `send_user_message` in tauri-runtime-wry: `if current_thread().id() ==
/// main_thread_id { handle_user_message(...) } else { proxy.send_event(...) }`).
/// Every caller of this function reaches it FROM the main thread already
/// (e.g. `reveal_remote_window_after_first_frame_on_main`, itself hopped onto
/// main via its own caller's `run_on_main_thread` from the decode thread) --
/// so calling `run_on_main_thread` directly here never actually deferred to a
/// later run-loop turn, silently defeating the whole point of this function
/// (confirmed live 2026-07-07: overlay windows stayed at `(0, HEADER_HEIGHT)`
/// even with this call present). Routing through a tokio task first
/// guarantees the eventual `run_on_main_thread` call originates from a
/// non-main (tokio worker) thread, so it takes the genuine deferred path.
#[derive(Clone, Debug)]
struct DrawRedockLog {
    window_id: u32,
    control_label: String,
    control_before: Option<ChromeFrame>,
}

fn format_draw_redock_after_log(
    window_id: u32,
    control_label: &str,
    control_before: Option<ChromeFrame>,
    control_after: Option<ChromeFrame>,
) -> String {
    format!(
        "compositor: draw mode active for window {window_id}; overlay redock landed (control_label='{control_label}', control_before={control_before:?}, control_after={control_after:?})"
    )
}

fn sync_chrome_to_panel_frame_deferred(app: &AppHandle, key: &RemoteWindowKey) {
    sync_chrome_to_panel_frame_deferred_with_log(app, key, None);
}

fn sync_chrome_to_panel_frame_deferred_with_log(
    app: &AppHandle,
    key: &RemoteWindowKey,
    draw_log: Option<DrawRedockLog>,
) {
    let app_defer = app.clone();
    let key_defer = key.clone();
    tauri::async_runtime::spawn(async move {
        // Yield past the current main-thread turn before hopping back --
        // any tiny delay is enough since the point is only to ensure this
        // task's own thread (a tokio worker) isn't the main thread.
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let app_main = app_defer.clone();
        let key_main = key_defer.clone();
        if let Err(e) = app_defer.run_on_main_thread(move || {
            if let Some(panel) = app_main.get_webview_window(&panel_label_for_key(&key_main)) {
                sync_chrome_to_panel_frame(&app_main, &key_main, &panel);
            }
            if let Some(log_context) = draw_log {
                let control_after = app_main
                    .get_webview_window(&log_context.control_label)
                    .and_then(|control| current_chrome_frame(&control));
                log::info!(
                    "{}",
                    format_draw_redock_after_log(
                        log_context.window_id,
                        &log_context.control_label,
                        log_context.control_before,
                        control_after
                    )
                );
            }
        }) {
            log::error!("compositor: deferred chrome sync enqueue failed: {e}");
        }
    });
}

fn reposition_chrome_frame(app: &AppHandle, label: &str, frame: ChromeFrame) {
    reposition_chrome(app, label, frame.x, frame.y, frame.width, frame.height);
}

fn chrome_frame_needs_update(current: ChromeFrame, target: ChromeFrame) -> bool {
    const EPSILON: f64 = 0.5;
    (current.x - target.x).abs() > EPSILON
        || (current.y - target.y).abs() > EPSILON
        || (current.width - target.width).abs() > EPSILON
        || (current.height - target.height).abs() > EPSILON
}

fn current_chrome_frame(window: &tauri::WebviewWindow) -> Option<ChromeFrame> {
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return None;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    Some(ChromeFrame {
        x: pos.x as f64 / scale,
        y: pos.y as f64 / scale,
        width: size.width as f64 / scale,
        height: size.height as f64 / scale,
    })
}

fn reposition_chrome(app: &AppHandle, label: &str, x: f64, y: f64, w: f64, h: f64) {
    if let Some(win) = app.get_webview_window(label) {
        let target = ChromeFrame {
            x,
            y,
            width: w,
            height: h,
        };
        if let Some(current) = current_chrome_frame(&win) {
            if !chrome_frame_needs_update(current, target) {
                return;
            }
        }
        let _ = win.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
        let _ = win.set_size(tauri::Size::Logical(tauri::LogicalSize {
            width: w,
            height: h,
        }));
    }
}

fn content_geometry(window: &tauri::WebviewWindow) -> Option<(f64, f64, f64)> {
    let scale = window.scale_factor().unwrap_or(1.0);
    let size = window.outer_size().ok()?;
    let (width, content_h) = content_size_from_outer(size.width as f64, size.height as f64, scale);
    Some((width, content_h, scale))
}

/// The pure math behind `content_geometry`: physical outer size -> logical
/// (width, content-height-below-the-header). Split out so the attach seam's
/// measured-over-remembered rule (`attach_display_layer`) is testable without
/// AppKit — in particular a degenerate/near-zero outer height, where the
/// video area must floor to 1pt instead of whatever expanded size
/// `panel_content_size` still remembers.
fn content_size_from_outer(outer_width_px: f64, outer_height_px: f64, scale: f64) -> (f64, f64) {
    let width = outer_width_px / scale;
    let content_h = (outer_height_px / scale - HEADER_HEIGHT).max(1.0);
    (width, content_h)
}

fn apply_display_filter(
    window: &CompositorWindow,
    displayed_width_points: f64,
    displayed_height_points: f64,
    receiver_scale: f64,
) {
    let Some((source_width_px, source_height_px)) = *window.source_pixel_size.lock_unpoisoned()
    else {
        return;
    };
    let Some(display) = window.display.as_ref() else {
        return;
    };
    display.update_filter_for_geometry(
        source_width_px,
        source_height_px,
        displayed_width_points,
        displayed_height_points,
        receiver_scale,
    );
}

fn capture_app_origin(app: &AppHandle, key: &RemoteWindowKey) -> Option<String> {
    for label in [
        panel_label_for_key(key),
        control_label_for_key(key),
        pointer_label_for_key(key),
        ai_chat_label_for_key(key),
    ] {
        let Some(win) = app.get_webview_window(&label) else {
            continue;
        };
        let Ok(url) = win.url() else {
            continue;
        };
        if let Some(origin) = app_origin_from_url(&url) {
            return Some(origin);
        }
    }
    None
}

fn strip_retired_window_for_pool(
    app: &AppHandle,
    key: &RemoteWindowKey,
    window: &mut CompositorWindow,
) {
    let window_id = key.window_id;
    if window.stripped_for_pool {
        return;
    }
    window.pending_display_samples.clear();
    window.app_origin = window
        .app_origin
        .take()
        .or_else(|| capture_app_origin(app, key));
    if let Some(display) = window.display.take() {
        // AppKit/CoreAnimation mutation is main-thread only. This helper is
        // called from remove_window's on-main closure after the panel is hidden.
        unsafe {
            let layer = display.as_layer_ptr();
            let _: () = objc2::msg_send![layer, flush];
            let _: () = objc2::msg_send![layer, setContents: std::ptr::null_mut::<objc2::runtime::AnyObject>()];
            let view = display.as_view_ptr();
            let _: () = objc2::msg_send![view, removeFromSuperview];
        }
    }
    // Only the click-through overlay children (plus the ai-chat overlay,
    // #844) are blanked to free webview memory. The panel webview stays
    // loaded -- it now also hosts the header chrome, and blanking it would
    // unload the video surface it composites.
    for label in [
        control_label_for_key(key),
        pointer_label_for_key(key),
        ai_chat_label_for_key(key),
    ] {
        if let Some(win) = app.get_webview_window(&label) {
            if let Err(e) = win.navigate(blank_url()) {
                log::warn!("compositor: failed to unload retired chrome '{label}': {e}");
            }
        }
    }
    window.layer_has_content = false;
    window.stripped_for_pool = true;
    log::info!("compositor: stripped retired window {window_id} to keep warm pool capped");
}

fn enforce_retired_pool_cap(app: &AppHandle, state: &mut CompositorState) {
    state
        .retired_order
        .retain(|id| state.retired.contains_key(id));
    while state.retired_order.len() > RETIRED_WARM_POOL_CAP {
        let evict_key = state.retired_order.remove(0);
        if let Some(window) = state.retired.get_mut(&evict_key) {
            strip_retired_window_for_pool(app, &evict_key, window);
        }
    }
    let warm = state
        .retired
        .values()
        .filter(|window| !window.stripped_for_pool)
        .count();
    let stripped = state.retired.len().saturating_sub(warm);
    log::info!(
        "compositor: retired pool total={} warm={} stripped={} cap={}",
        state.retired.len(),
        warm,
        stripped,
        RETIRED_WARM_POOL_CAP
    );
}

/// Tear down `window_id`'s compositor window and all its resources (panel,
/// control/pointer overlays). Safe to call for an already-closed /
/// never-opened window (idempotent no-op). Called on `share-ended` (window
/// closed/unshared) or participant disconnect -- see `subscriber.rs`.
pub fn remove_window(
    app: &AppHandle,
    owner_identity: &str,
    window_id: u32,
    reason: RemoveWindowReason,
) {
    let key = RemoteWindowKey::new(owner_identity, window_id);
    crate::viewer_demand::publish_window_closed(app, window_id);
    // #679 fix (adversarial review of the original commit): classify BEFORE
    // the early return below, not after. `s.windows.remove` returns `None`
    // whenever the window is already retired (e.g. a prior ManualHide or
    // NoFrameWatchdog already moved it out of `s.windows`) -- if a later
    // GENUINE end (TrackUnsubscribed/TrackUnpublished/...) for that same
    // retired key hit this function, the early return would skip
    // classification entirely and a stale transport-side suppression would
    // never clear, silently eating the pill for a real stop-and-restart.
    // Classification needs only `key`/`reason`, not `removed`, so hoisting it
    // above the early return is free and closes that gap in both directions.
    record_share_pill_suppression_for_remove_reason(&key, reason);
    let removed = with_state(|s| {
        s.remote_control_active.remove(&key);
        s.windows.remove(&key)
    });
    let Some(removed) = removed else {
        return;
    };
    // Invalidate any already-queued canonical-size callback before this state
    // can be reused for a later share instance with the same window id.
    removed
        .canonical_source_epoch
        .fetch_add(1, Ordering::AcqRel);
    // The hidden panel can be reused for a later share. Its previous native
    // resize callbacks are no longer meaningful to that new lifecycle.
    reset_programmatic_resize_events(&removed);
    let (w, h) = *removed.panel_content_size.lock_unpoisoned();
    let owner_identity = removed.owner_identity.clone();
    // HIDE the windows (never destroy — see `CompositorState::retired`'s doc
    // comment for the crash history behind this) and retire the state for
    // reuse. Hiding is AppKit work — marshal to the main thread (same reason
    // as `ensure_window`). Callers reach here from background RoomEvent /
    // leave-room threads.
    let app_main = app.clone();
    let key_for_main = key.clone();
    crate::platform::on_main(
        app,
        format!(
            "compositor: remove_window {owner_identity}:{window_id} (reason={})",
            reason.label()
        ),
        move || {
            for label in [
                control_label_for_key(&key_for_main),
                pointer_label_for_key(&key_for_main),
                ai_chat_label_for_key(&key_for_main),
                panel_label_for_key(&key_for_main),
            ] {
                if let Some(win) = app_main.get_webview_window(&label) {
                    log::info!(
                        "compositor: hiding window '{label}' (retired for reuse, never destroyed)"
                    );
                    let result = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                        if label == pointer_label_for_key(&key_for_main) {
                            let _ = win.eval(format!(
                                "window.__petalDrawClearWindow && window.__petalDrawClearWindow({window_id});"
                            ));
                        }
                        // #844 review: detach BEFORE hiding -- this can hide
                        // an ai-chat overlay that was still OPEN (attached)
                        // when the window was retired outside the toggle
                        // command (e.g. a genuine unpublish/teardown, not a
                        // hold-path glitch). removeChildWindow is a no-op if
                        // it was never attached, so this is safe every time.
                        if label == ai_chat_label_for_key(&key_for_main) {
                            detach_ai_chat_overlay(&app_main, &key_for_main);
                        }
                        let _ = win.hide();
                        if label == control_label_for_key(&key_for_main) {
                            let _ = win.set_ignore_cursor_events(true);
                        }
                    }));
                    if let Err(exception) = result {
                        log::error!(
                        "compositor: NSException while hiding '{label}' (caught, app continues): {exception:?}"
                    );
                    }
                }
            }
            // Park the state (incl. the DisplayLayer's Retained AppKit refs) in
            // `retired` from the main thread — never dropped mid-session.
            with_state(|s| {
                s.retired_order.retain(|stored| stored != &key_for_main);
                s.retired_order.push(key_for_main.clone());
                let mut removed = removed;
                removed.pending_display_samples.clear();
                s.retired.insert(key_for_main.clone(), removed);
                enforce_retired_pool_cap(&app_main, s);
            });
        },
    );
    log::info!(
        "compositor: closed remote window {window_id} (owner '{owner_identity}', last size {w:.0}x{h:.0}, reason={}) [hidden + retired]",
        reason.label()
    );
}

/// Why a window is being held rather than hidden. Labels are stable: they
/// distinguish the hold paths in the logs exactly as `RemoveWindowReason`'s do
/// for teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldWindowReason {
    /// A republish's `TrackUnpublished`/`TrackUnsubscribed` arrived before the
    /// replacement's `TrackSubscribed`. The SFU still holds a publication.
    ReplacementInbound,
    /// The no-frame watchdog fired, but a publication still exists, so this is
    /// a stalled live share rather than an ended one (#417 + #627).
    NoFrameWatchdog,
    /// LiveKit emits `ParticipantDisconnected` for every remote participant
    /// before it emits `Reconnecting` during a full reconnect (#631). Keep
    /// their rendered windows on screen until reconciliation can distinguish
    /// that transient SDK ordering from a real departure.
    ParticipantReconnect,
    /// Reconciliation could not re-establish a publication it can still see.
    ReconciledUnrecoverable,
}

impl HoldWindowReason {
    fn label(self) -> &'static str {
        match self {
            Self::ReplacementInbound => "replacement-inbound",
            Self::NoFrameWatchdog => "no-frame-watchdog",
            Self::ParticipantReconnect => "participant-reconnect",
            Self::ReconciledUnrecoverable => "reconciled-unrecoverable",
        }
    }
}

/// Keep `window_id` on screen showing its last decoded frame instead of
/// hiding it (#627, CLAUDE.md "Never show a black frame").
///
/// The video layer is deliberately untouched: `AVSampleBufferDisplayLayer`
/// holds its most recent enqueued frame indefinitely, and nothing here
/// flushes, removes contents, or stops requesting media. Hiding the panel is
/// what made a share visibly vanish -- the frame survived in the layer but
/// off screen, so the user saw the desktop rather than a frozen frame.
///
/// The header is marked media-paused so a held frame is honestly labelled as
/// not-live rather than passed off as current, the same affordance a muted
/// track already gets.
pub fn hold_window_last_frame(
    app: &AppHandle,
    owner_identity: &str,
    window_id: u32,
    reason: HoldWindowReason,
) -> bool {
    let key = RemoteWindowKey::new(owner_identity, window_id);
    let is_open = with_state(|s| s.windows.contains_key(&key));
    if !is_open {
        log::debug!(
            "compositor: no open window {window_id} from '{owner_identity}' to hold (reason={})",
            reason.label()
        );
        return false;
    }
    // A window whose first frame never arrived has nothing to hold: it is
    // still gated behind the first-frame reveal, so "keep showing it" would
    // mean keeping an unfed layer on screen. Report that to the caller so it
    // can fall back to a real teardown.
    let has_frame = with_state(|s| {
        s.windows
            .get(&key)
            .is_some_and(|win| win.revealed_first_frame)
    });
    if !has_frame {
        log::info!(
            "compositor: window {window_id} from '{owner_identity}' has no first frame to hold (reason={}); caller decides",
            reason.label()
        );
        return false;
    }
    // Idempotent: a reconcile divergence recurs on every 5s pass, and a
    // watchdog stall can be re-observed. Re-evaluating the header JS and
    // re-logging each time would be pure noise.
    let already_held = with_state(|s| {
        let Some(win) = s.windows.get_mut(&key) else {
            return false;
        };
        if win.held_reason == Some(reason) {
            return true;
        }
        win.held_reason = Some(reason);
        false
    });
    if already_held {
        return true;
    }
    set_window_media_paused(app, owner_identity, window_id, true);
    log::info!(
        "compositor: holding last frame for window {window_id} from '{owner_identity}' (reason={}) [visible, layer untouched]",
        reason.label()
    );
    true
}

/// Hold every rendered window owned by a participant through the SDK's
/// reconnect-time synthetic disconnect. This deliberately does not remove
/// compositor, receive, or publication state: reconciliation needs the
/// subscriber-owned tracking entry to retire a participant who actually left.
pub fn hold_windows_for_participant_reconnect(app: &AppHandle, owner_identity: &str) {
    for window_id in window_ids_for_participant(owner_identity) {
        let _ = hold_window_last_frame(
            app,
            owner_identity,
            window_id,
            HoldWindowReason::ParticipantReconnect,
        );
    }
}

/// Tear down every currently-open compositor window, regardless of owner --
/// called when THIS process leaves the room (`session::leave_room`): once
/// disconnected, there is no longer any live subscription to any remote
/// share, so every open remote window is stale.
pub fn remove_all_windows(app: &AppHandle) {
    let keys: Vec<RemoteWindowKey> = with_state(|s| s.windows.keys().cloned().collect());
    for key in keys {
        remove_window(
            app,
            &key.owner_identity,
            key.window_id,
            RemoveWindowReason::LeaveRoom,
        );
    }
    // #679 fix: `remove_window`'s per-key LeaveRoom classification above only
    // ever reaches keys that were still in `s.windows` at the moment this ran
    // -- a key already retired by an earlier transport-side teardown (whose
    // suppression entry that teardown legitimately set) is never visited by
    // this loop at all, since it was never in `s.windows`/`keys`. Leaving the
    // room means no live subscription to ANY remote share survives, so every
    // suppression entry -- retired-key or not -- is stale by definition the
    // instant this runs. A fresh join later starts every share as genuinely
    // new, so nothing should stay silently suppressed across that boundary.
    with_state(|s| s.suppressed_reshare_pill.clear());
}

/// Tear down every open compositor window owned by `owner_identity` (a
/// remote participant fully disconnected -- SPEC.md §4.4's "leaves" case,
/// distinct from "stopped sharing this one window").
pub fn remove_windows_for_participant(app: &AppHandle, owner_identity: &str) {
    let ids: Vec<u32> = with_state(|s| {
        s.windows
            .iter()
            .filter(|(key, _)| key.owner_identity == owner_identity)
            .map(|(key, _)| key.window_id)
            .collect()
    });
    for id in ids {
        remove_window(
            app,
            owner_identity,
            id,
            RemoveWindowReason::ParticipantDisconnected,
        );
    }
}

pub fn is_open_for_owner(owner_identity: &str, window_id: u32) -> bool {
    resolve_open_window_key(window_id, Some(owner_identity)).is_some()
}

/// #679: classify a teardown as either a genuine end of a share (a real
/// sharer-side unpublish, or the SFU authoritatively confirming the
/// publication is gone) or a transport-side hiccup that will very likely be
/// followed by the SAME share re-subscribing under the same key.
///
/// This is deliberately NOT the same question `is_open_for_owner` answers.
/// That check only sees `s.windows` (never `s.retired`), so gating the
/// "<Name> is sharing a window" pill on it alone fails exactly the case the
/// pill exists to get right: a full LiveKit reconnect tears down every
/// window via `RemoveWindowReason::ParticipantDisconnected` (#631), the key
/// leaves `s.windows`, and the very next `TrackSubscribed` for the SAME
/// still-active share would look identical to a brand new one -- firing the
/// pill for every existing share on every reconnect. `ManualHide` gets the
/// same treatment on purpose: the user dismissed that window deliberately,
/// so a republish of it should not nag them again.
///
/// A genuine end (`TrackUnsubscribed`/`TrackUnpublished`/
/// `ReconciledPublicationGone`/`LeaveRoom`) clears any stale suppression
/// instead, so a real stop-and-restart of the same share still fires the
/// pill.
fn record_share_pill_suppression_for_remove_reason(
    key: &RemoteWindowKey,
    reason: RemoveWindowReason,
) {
    with_state(|s| match reason {
        RemoveWindowReason::ParticipantDisconnected
        | RemoveWindowReason::NoFrameWatchdog
        | RemoveWindowReason::ManualHide
        | RemoveWindowReason::ReconciledUnrecoverable => {
            s.suppressed_reshare_pill.insert(key.clone());
        }
        RemoveWindowReason::TrackUnsubscribed
        | RemoveWindowReason::TrackUnpublished
        | RemoveWindowReason::ReconciledPublicationGone
        | RemoveWindowReason::LeaveRoom => {
            s.suppressed_reshare_pill.remove(key);
        }
    })
}

/// Whether the "<Name> is sharing a window" pill should stay silent for this
/// `TrackSubscribed` -- called from `transport::subscriber` right after a
/// `TrackSubscribed` that is NOT already open (see
/// `record_share_pill_suppression_for_remove_reason`'s doc comment for which
/// teardown reasons set this). One-shot: consumes (removes) the suppression
/// entry, so only the very next re-subscribe after a transport-side teardown
/// is silenced -- a LATER one is a fresh event and fires normally.
pub fn consume_share_started_pill_suppression(owner_identity: &str, window_id: u32) -> bool {
    let key = RemoteWindowKey::new(owner_identity, window_id);
    with_state(|s| s.suppressed_reshare_pill.remove(&key))
}

pub fn window_ids_for_participant(owner_identity: &str) -> Vec<u32> {
    with_state(|s| {
        s.windows
            .iter()
            .filter(|(key, _)| key.owner_identity == owner_identity)
            .map(|(key, _)| key.window_id)
            .collect()
    })
}

/// #875 review F3: the retired (viewer-hidden) counterpart to
/// `window_ids_for_participant`. The `ParticipantMetadataChanged` handler in
/// `transport/subscriber.rs` unions this in with the open set before
/// refreshing per-window metadata, so a hidden window keeps learning about a
/// z-rank change while retired instead of being restored later into its
/// stale at-hide position (`update_window_metadata` already safely no-ops
/// for a window_id with no open entry, so this only feeds the z-rank path).
pub fn retired_window_ids_for_participant(owner_identity: &str) -> Vec<u32> {
    with_state(|s| {
        s.retired
            .iter()
            .filter(|(key, _)| key.owner_identity == owner_identity)
            .map(|(key, _)| key.window_id)
            .collect()
    })
}

fn remote_window_summaries() -> Vec<RemoteWindowSummary> {
    let mut entries = with_state(|s| {
        let mut entries = Vec::with_capacity(s.windows.len() + s.retired.len());
        entries.extend(s.windows.iter().map(|(key, window)| RemoteWindowSummary {
            window_id: key.window_id,
            owner_identity: key.owner_identity.clone(),
            owner_display_name: window.owner_display_name.clone(),
            source_title: window.source_title.clone(),
            hidden: false,
        }));
        entries.extend(s.retired.iter().map(|(key, window)| RemoteWindowSummary {
            window_id: key.window_id,
            owner_identity: key.owner_identity.clone(),
            owner_display_name: window.owner_display_name.clone(),
            source_title: window.source_title.clone(),
            hidden: true,
        }));
        entries
    });
    entries.sort_by_key(|entry| (entry.hidden, entry.owner_identity.clone(), entry.window_id));
    entries
}

/// #843: retired-inclusive on purpose. Owner identity is stable metadata --
/// unlike `resolve_open_window_key`'s "is this window currently interactive"
/// question, WHO owns a window does not change when it transitions between
/// `windows` and `retired` (same entry, moved between two maps). Callers that
/// need routing/addressing info (remote-control's `viewer_channel`, draw,
/// telepointer, viewer-demand) must not fail just because a republish storm
/// has the window mid-retire; callers that need to know whether the window
/// can currently be ACTED on (drag, control-mode entry) gate on the open-only
/// resolver separately, after restoring if needed (see `activate_window_then`).
pub(crate) fn owner_identity_for_window(
    window_id: u32,
    owner_identity: Option<&str>,
) -> Option<String> {
    let key = resolve_window_key(window_id, owner_identity)?;
    Some(key.owner_identity)
}

pub(crate) fn has_window_for_owner(owner_identity: &str, window_id: u32) -> bool {
    resolve_open_window_key(window_id, Some(owner_identity)).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteControlTargetMetadata {
    pub(crate) target_kind: crate::remote_control_core::RemoteControlTargetKind,
    pub(crate) share_instance_id: Option<String>,
}

/// Return target metadata retained for a remote compositor window. This is
/// retired-inclusive because the owner/share identity remains stable while a
/// window is temporarily parked during a transport replacement.
pub(crate) fn remote_control_target_metadata(
    window_id: u32,
    owner_identity: Option<&str>,
) -> Option<RemoteControlTargetMetadata> {
    let key = resolve_window_key(window_id, owner_identity)?;
    with_state(|s| {
        s.windows
            .get(&key)
            .or_else(|| s.retired.get(&key))
            .map(|window| RemoteControlTargetMetadata {
                target_kind: match window.source_kind {
                    SharedSourceKind::Window => {
                        crate::remote_control_core::RemoteControlTargetKind::Window
                    }
                    SharedSourceKind::Display | SharedSourceKind::DisplayRegion => {
                        crate::remote_control_core::RemoteControlTargetKind::Display
                    }
                },
                share_instance_id: window.share_instance_id.clone(),
            })
    })
}

pub(crate) fn set_remote_control_active(
    app: &AppHandle,
    window_id: u32,
    owner_identity: Option<&str>,
    active: bool,
) -> Result<(), String> {
    let key = resolve_open_window_key(window_id, owner_identity)
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let remote_control_available =
        with_state(|s| s.windows.get(&key).map(|w| w.remote_control_available));
    let Some(remote_control_available) = remote_control_available else {
        return Err(format!("remote window {window_id} is not open"));
    };
    if active && !remote_control_available {
        return Err(format!(
            "remote-control overlay for window {window_id} is not available"
        ));
    }
    let label = control_label_for_key(&key);
    let Some(control) = app.get_webview_window(&label) else {
        return Err(format!(
            "remote-control overlay for window {window_id} is not available"
        ));
    };
    control
        .set_ignore_cursor_events(false)
        .map_err(|e| format!("toggle remote-control overlay: {e}"))?;
    with_state(|s| {
        if active {
            s.remote_control_active.insert(key.clone());
        } else {
            s.remote_control_active.remove(&key);
        }
    });
    let active_json = if active { "true" } else { "false" };
    if let Err(e) = control.eval(remote_control_active_script(active)) {
        log::warn!(
            "compositor: failed to eval remote-control active update for window {window_id} overlay '{}': {e}",
            control.label()
        );
    }
    if let Some(panel) = app.get_webview_window(&panel_label_for_key(&key)) {
        if let Err(e) = panel.eval(format!(
            "window.__petalRemoteControlHeaderActive && window.__petalRemoteControlHeaderActive({active_json})"
        )) {
            log::warn!(
                "compositor: failed to eval remote-control header active update for window {window_id} panel '{}': {e}",
                panel.label()
            );
        }
    }
    if active {
        let _ = control.show();
        let _ = control.set_focus();
    }
    log::info!(
        "compositor: remote-control overlay for window {window_id} {}",
        if active { "enabled" } else { "disabled" }
    );
    Ok(())
}

fn order_below_anchor(window: &tauri::WebviewWindow, anchor: Option<i64>) {
    let Some(anchor) = anchor.filter(|n| *n > 0) else {
        return;
    };
    if let Err(e) = crate::platform::appkit::order_below_anchor(window, anchor) {
        log::warn!(
            "compositor: ns_window() unavailable for '{}'; cannot order below anchor {anchor}",
            window.label()
        );
        log::debug!("compositor: order_below_anchor failure detail: {e}");
    }
}

/// Re-assert the control, pointer, and ai-chat (#844) chrome windows
/// immediately above their panel in the WindowServer's normal-level z-stack.
/// Panel-relative, NOT floating -- see `platform::appkit::order_above_panel`'s
/// doc comment for why this repo deliberately avoids `always_on_top`.
fn order_chrome_above_panel(app: &AppHandle, key: &RemoteWindowKey) {
    let Some(panel) = app.get_webview_window(&panel_label_for_key(key)) else {
        return;
    };
    // `orderWindow:relativeTo:` orders the CALLER into the WindowServer's
    // screen list -- it doesn't just reassert a relative position, it also
    // un-hides an ordered-out window. Panel hidden (window closed/retired) or
    // a chrome window intentionally hidden (#445 review finding) must both be
    // left alone, or this "repair" resurrects windows that were deliberately
    // hidden -- which is exactly why the ai-chat overlay (only ever visible
    // when its own disclosure flag is set) is safe to include here
    // unconditionally: the `is_visible()` check below skips it whenever it's
    // hidden.
    if !panel.is_visible().unwrap_or(false) {
        return;
    }
    for label in [
        control_label_for_key(key),
        pointer_label_for_key(key),
        ai_chat_label_for_key(key),
    ] {
        let Some(chrome) = app.get_webview_window(&label) else {
            continue;
        };
        if !chrome.is_visible().unwrap_or(false) {
            continue;
        }
        if let Err(e) = crate::platform::appkit::order_above_panel(&chrome, &panel) {
            log::warn!(
                "compositor: failed to order chrome '{label}' above panel '{}': {e}",
                panel.label()
            );
        }
    }
}

/// Re-assert visible remote-window chrome after another application/window
/// becomes active. This is an event-driven supplement to the existing frame
/// and reveal paths; hidden/retired windows remain untouched (#465).
pub(crate) fn reassert_active_chrome_on_main(app: &AppHandle) {
    let keys: Vec<RemoteWindowKey> = with_state(|state| {
        state
            .windows
            .iter()
            .filter_map(|(key, window)| (!window.stripped_for_pool).then_some(key.clone()))
            .collect()
    });
    for key in keys {
        order_chrome_above_panel(app, &key);
    }
}

pub(crate) fn has_active_remote_windows() -> bool {
    with_state(|state| !state.windows.is_empty())
}

// =============================================================================
// Tauri commands -- called from RemoteWindowHeader.svelte's real button
// handlers (pop-out, fit-to-source) via the header webview.
// =============================================================================

/// Start a native window-drag session on `window_id`'s PANEL. The header is
/// rendered by the panel's own surface webview (its top strip), and a
/// mousedown there calls this to drag the whole window -- SPEC.md §4.4: "the
/// header is the drag handle." Called from the surface route's real
/// `onmousedown` handler (see `compositor/surface/+page.svelte`).
#[tauri::command]
pub fn compositor_start_drag(app: AppHandle, window_id: u32, owner_identity: Option<String>) {
    // #843: a visible-but-retired window (mid-republish-storm reveal) must
    // still accept a drag. The old code called `activate_window` (whose
    // restore is asynchronous, via `run_on_main_thread`) and then
    // IMMEDIATELY resolved again with the OPEN-ONLY `resolve_open_window_key`
    // on the command thread -- a lookup that can and did run before the
    // restore landed, silently returning with no drag and no log line. Start
    // the drag inside the SAME main-thread hop that performs the restore, so
    // there is no window for the two to race in.
    activate_window_then(
        &app,
        window_id,
        owner_identity.as_deref(),
        move |_app, window| {
            if let Err(e) = window.start_dragging() {
                log::warn!("compositor: start_dragging failed for window {window_id}: {e}");
            }
        },
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompositorResizeDirection {
    East,
    North,
    NorthEast,
    NorthWest,
    South,
    SouthEast,
    SouthWest,
    West,
}

impl CompositorResizeDirection {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "East" => Ok(Self::East),
            "North" => Ok(Self::North),
            "NorthEast" => Ok(Self::NorthEast),
            "NorthWest" => Ok(Self::NorthWest),
            "South" => Ok(Self::South),
            "SouthEast" => Ok(Self::SouthEast),
            "SouthWest" => Ok(Self::SouthWest),
            "West" => Ok(Self::West),
            _ => Err(format!("invalid resize direction '{raw}'")),
        }
    }

    fn has_east(self) -> bool {
        matches!(self, Self::East | Self::NorthEast | Self::SouthEast)
    }

    fn has_west(self) -> bool {
        matches!(self, Self::West | Self::NorthWest | Self::SouthWest)
    }

    fn has_north(self) -> bool {
        matches!(self, Self::North | Self::NorthEast | Self::NorthWest)
    }

    fn has_south(self) -> bool {
        matches!(self, Self::South | Self::SouthEast | Self::SouthWest)
    }

    fn has_horizontal(self) -> bool {
        self.has_east() || self.has_west()
    }

    fn has_vertical(self) -> bool {
        self.has_north() || self.has_south()
    }
}

fn source_aspect_for_resize(
    key: &RemoteWindowKey,
    fallback_width: f64,
    fallback_height: f64,
) -> f64 {
    let fallback_content_h = (fallback_height - HEADER_HEIGHT).max(1.0);
    let (source_w, source_h) = with_state(|s| {
        s.windows
            .get(key)
            .and_then(|w| *w.source_presentation_size.lock_unpoisoned())
            .unwrap_or((fallback_width, fallback_content_h))
    });
    (source_w / source_h.max(1.0)).max(0.01)
}

fn resized_frame_from_drag(
    direction: CompositorResizeDirection,
    aspect: f64,
    start: CompositorResizeFrame,
    delta_x: f64,
    delta_y: f64,
) -> CompositorResizeFrame {
    let start_content_h = (start.height - HEADER_HEIGHT).max(1.0);
    let horizontal_width = if direction.has_west() {
        start.width - delta_x
    } else if direction.has_east() {
        start.width + delta_x
    } else {
        start.width
    };
    let vertical_content_h = if direction.has_north() {
        start_content_h - delta_y
    } else if direction.has_south() {
        start_content_h + delta_y
    } else {
        start_content_h
    };
    let vertical_width = vertical_content_h * aspect;
    let mut width = match (direction.has_horizontal(), direction.has_vertical()) {
        (true, true) => {
            if (horizontal_width - start.width).abs() >= (vertical_width - start.width).abs() {
                horizontal_width
            } else {
                vertical_width
            }
        }
        (true, false) => horizontal_width,
        (false, true) => vertical_width,
        (false, false) => start.width,
    };
    width = width.max(MIN_RESIZE_CONTENT_WIDTH.max(MIN_RESIZE_CONTENT_HEIGHT * aspect));
    let height = HEADER_HEIGHT + (width / aspect).max(MIN_RESIZE_CONTENT_HEIGHT);
    let x = if direction.has_west() {
        start.x + start.width - width
    } else {
        start.x
    };
    let y = if direction.has_north() {
        start.y + start.height - height
    } else {
        start.y
    };
    CompositorResizeFrame {
        x,
        y,
        width,
        height,
    }
}

fn snap_resized_frame_to_integer_scale(
    direction: CompositorResizeDirection,
    frame: CompositorResizeFrame,
    source_pixel_size: Option<(u32, u32)>,
    receiver_scale: f64,
    start_width: f64,
    start_height: f64,
) -> CompositorResizeFrame {
    let Some((source_width_px, source_height_px)) = source_pixel_size else {
        return frame;
    };
    let content_h = (frame.height - HEADER_HEIGHT).max(1.0);
    let Some(snapped) = snap_content_size_to_nearest_integer_scale(
        source_width_px,
        source_height_px,
        receiver_scale,
        frame.width,
        content_h,
    ) else {
        return frame;
    };
    if snapped.width < MIN_RESIZE_CONTENT_WIDTH || snapped.height < MIN_RESIZE_CONTENT_HEIGHT {
        return frame;
    }
    // Live testing 2026-07-14 found repeated resizes "briefly jump back to
    // the prior size": a demand-republish after a finalized resize can move
    // the source's rung-quantized pixel size (see canonical_source_pixel_size
    // writers), relocating this snap's integer-scale grid points close to
    // wherever the PREVIOUS drag ended. A second, smaller drag then starts
    // right on top of a grid point, and this snap reverts it almost back to
    // the pre-drag start size -- correct arithmetic, but it undoes the
    // gesture the user just performed instead of just fine-tuning it. Refuse
    // any correction that reverses more than half of what the user actually
    // dragged, on either axis independently (so a small overshoot still
    // snaps, but the released size is trusted over a stale/relocated grid).
    let start_content_h = (start_height - HEADER_HEIGHT).max(1.0);
    let overcorrects = |start: f64, released: f64, snapped_value: f64| -> bool {
        let dragged = released - start;
        let correction = snapped_value - released;
        dragged != 0.0
            && correction.signum() == -dragged.signum()
            && correction.abs() > (dragged.abs() / 2.0)
    };
    if overcorrects(start_width, frame.width, snapped.width)
        || overcorrects(start_content_h, content_h, snapped.height)
    {
        return frame;
    }

    let snapped_total_h = HEADER_HEIGHT + snapped.height;
    let x = if direction.has_west() {
        frame.x + frame.width - snapped.width
    } else {
        frame.x
    };
    let y = if direction.has_north() {
        frame.y + frame.height - snapped_total_h
    } else {
        frame.y
    };
    CompositorResizeFrame {
        x,
        y,
        width: snapped.width,
        height: snapped_total_h,
    }
}

/// A real user gesture takes ownership of subsequent `Resized` events. Do not
/// let a delayed source-sizing callback consume it -- but the request just
/// cancelled may have carried a genuine sender-side size change, so latch it
/// for pointer-up to reconcile instead of dropping it (#416). Factored out of
/// `compositor_begin_resize` (rather than inlined) so a test drives the real
/// production transition, not a copy of it -- the gap CLAUDE.md's
/// "live-exercising test" rule exists to close.
fn cancel_programmatic_resize_for_user_gesture(window: &CompositorWindow) {
    if cancel_programmatic_resize(window) {
        latch_source_reconciliation(window);
    }
}

// #855: identical race to #843's compositor_start_drag. The old body called
// `activate_window` (whose restore is asynchronous, via `on_main` /
// `run_on_main_thread`) and then IMMEDIATELY re-resolved with the OPEN-ONLY
// `resolve_open_window_key` on the command thread -- a lookup that could and
// did run before the restore landed, silently returning "remote window N is
// not open" and never starting the resize. Unlike `compositor_start_drag`
// (fire-and-forget), the frontend AWAITS this command's frame, so the fix
// must thread a result back out of the SAME main-thread hop that performs the
// restore rather than just becoming fire-and-forget itself: an async command
// + a `oneshot` channel closed by the `activate_window_then` continuation.
#[tauri::command]
pub async fn compositor_begin_resize(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) -> Result<CompositorResizeFrame, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let owner_for_continuation = owner_identity.clone();
    activate_window_then(
        &app,
        window_id,
        owner_identity.as_deref(),
        move |_app, window| {
            let result = (|| -> Result<CompositorResizeFrame, String> {
                let key = resolve_open_window_key(window_id, owner_for_continuation.as_deref())
                    .ok_or_else(|| format!("remote window {window_id} is not open"))?;
                with_state(|s| {
                    if let Some(window) = s.windows.get(&key) {
                        cancel_programmatic_resize_for_user_gesture(window);
                        window.user_resize_active.store(true, Ordering::Relaxed);
                        window
                            .user_resize_active_since_ms
                            .store(now_ms(), Ordering::Relaxed);
                        window
                            .user_resize_until_ms
                            .store(now_ms().saturating_add(USER_RESIZE_TTL), Ordering::Relaxed);
                    }
                });
                let scale = window.scale_factor().unwrap_or(1.0);
                let position = window
                    .outer_position()
                    .map_err(|e| format!("read remote window position: {e}"))?;
                let size = window
                    .outer_size()
                    .map_err(|e| format!("read remote window size: {e}"))?;
                Ok(CompositorResizeFrame {
                    x: position.x as f64 / scale,
                    y: position.y as f64 / scale,
                    width: size.width as f64 / scale,
                    height: size.height as f64 / scale,
                })
            })();
            let _ = tx.send(result);
        },
    );
    // `activate_window_then` bails (logging, never calling `after_raise`)
    // when the window is missing/ambiguous, and `on_main`'s dispatch can fail
    // synchronously too -- both drop `tx` without a send, which surfaces here
    // as a closed channel. Map that to the same "not open" error the old
    // synchronous path returned, rather than propagating a raw RecvError.
    match rx.await {
        Ok(result) => result,
        Err(_closed) => Err(format!("remote window {window_id} is not open")),
    }
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn compositor_resize_window(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
    direction: String,
    start_x: f64,
    start_y: f64,
    start_width: f64,
    start_height: f64,
    delta_x: f64,
    delta_y: f64,
    finalize: Option<bool>,
) -> Result<(), String> {
    let direction = CompositorResizeDirection::parse(&direction)?;
    let key = resolve_open_window_key(window_id, owner_identity.as_deref())
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let label = panel_label_for_key(&key);
    let window = app
        .get_webview_window(&label)
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let aspect = source_aspect_for_resize(&key, start_width, start_height);
    let mut frame = resized_frame_from_drag(
        direction,
        aspect,
        CompositorResizeFrame {
            x: start_x,
            y: start_y,
            width: start_width,
            height: start_height,
        },
        delta_x,
        delta_y,
    );
    if finalize.unwrap_or(false) {
        let canonical_source_pixel_size = with_state(|s| {
            s.windows
                .get(&key)
                .and_then(|window| *window.canonical_source_pixel_size.lock_unpoisoned())
        });
        let receiver_scale = window.scale_factor().unwrap_or(1.0);
        frame = snap_resized_frame_to_integer_scale(
            direction,
            frame,
            canonical_source_pixel_size,
            receiver_scale,
            start_width,
            start_height,
        );
    }
    trace_panel_geometry(
        if finalize.unwrap_or(false) {
            "drag-final"
        } else {
            "drag"
        },
        window_id,
        frame.width,
        frame.height,
        with_state(|s| s.windows.get(&key).map(resize_gesture_in_progress)),
    );
    window
        .set_size(tauri::Size::Logical(tauri::LogicalSize {
            width: frame.width,
            height: frame.height,
        }))
        .map_err(|e| format!("resize remote window: {e}"))?;
    window
        .set_position(tauri::Position::Logical(tauri::LogicalPosition {
            x: frame.x,
            y: frame.y,
        }))
        .map_err(|e| format!("position remote window after resize: {e}"))?;
    let is_final = finalize.unwrap_or(false);
    let pending_source_resize = with_state(|s| {
        let Some(window) = s.windows.get(&key) else {
            return None;
        };
        if is_final {
            window.user_resize_until_ms.store(0, Ordering::Relaxed);
            window.user_resize_active.store(false, Ordering::Relaxed);
            window.pending_source_resize.lock_unpoisoned().take()
        } else {
            // `compositor_begin_resize` owns the drag transition. Do not
            // re-arm state here: a delayed pointermove IPC can arrive after
            // pointerup/pointercancel finalized the gesture, and re-arming
            // would make a completed drag suppress source reconciliation.
            None
        }
    });
    if is_final {
        crate::viewer_demand::publish_window_open(&app, window_id);
    }
    if let Some(target) = pending_source_resize {
        // Re-read current canonical state at pointer-up. The latch is only a
        // signal that a genuine source change occurred during the drag; its
        // captured dimensions may already be superseded by a republish.
        let current = with_state(|s| {
            s.windows.get(&key).and_then(|window| {
                let Some((width, height)) = *window.canonical_source_pixel_size.lock_unpoisoned()
                else {
                    return None;
                };
                Some(InitialResizeTarget {
                    source_width_px: width,
                    source_height_px: height,
                    source_scale: valid_source_scale(window.source_scale),
                    fallback_content_w: target.fallback_content_w,
                    fallback_content_h: target.fallback_content_h,
                })
            })
        })
        .unwrap_or(target);
        resize_source_preserving_user_size(&app, &key.owner_identity, window_id, current);
    }
    Ok(())
}

fn activate_window(app: &AppHandle, window_id: u32, owner_identity: Option<&str>) {
    activate_window_then(app, window_id, owner_identity, |_app, _window| {});
}

/// Same restore-then-raise as `activate_window`, plus an `after_raise`
/// continuation run on the SAME main-thread hop immediately after the panel
/// is raised and keyed. #843/#855: a caller that needs to act on the window
/// right after activating it (e.g. `compositor_start_drag`, and as of #855
/// `compositor_begin_resize`) MUST use this instead
/// of calling `activate_window` and then separately resolving/acting on its
/// own thread -- `activate_window`'s restore is asynchronous
/// (`run_on_main_thread`), so a second, separately-scheduled lookup can run
/// BEFORE the restore lands and see the window still in the retired pool.
/// Threading the continuation through this same closure makes that race
/// structurally impossible rather than merely unlikely.
fn activate_window_then(
    app: &AppHandle,
    window_id: u32,
    owner_identity: Option<&str>,
    after_raise: impl FnOnce(&AppHandle, &tauri::WebviewWindow) + Send + 'static,
) {
    let Some(key) = resolve_window_key(window_id, owner_identity) else {
        log::warn!("compositor: activate requested for missing or ambiguous window {window_id}");
        return;
    };
    let label = panel_label_for_key(&key);
    let app_for_thread = app.clone();
    let key_for_main = key.clone();
    crate::platform::on_main(
        app,
        format!("compositor: activate {window_id}"),
        move || {
            let restored_state = with_state(|s| {
                if s.windows.contains_key(&key_for_main) {
                    return None;
                }
                s.retired_order.retain(|stored| stored != &key_for_main);
                s.retired.remove(&key_for_main)
            });
            if let Some(mut win_state) = restored_state {
                let passive_anchor = crate::window_diag::frontmost_normal_window_number();
                show_retired_window_on_main(
                    &app_for_thread,
                    &key_for_main,
                    &mut win_state,
                    passive_anchor,
                    "activate_window",
                    true,
                );
                with_state(|s| {
                    s.windows.insert(key_for_main.clone(), win_state);
                });
                crate::viewer_demand::publish_window_open(&app_for_thread, window_id);
            }
            let Some(window) = app_for_thread.get_webview_window(&label) else {
                log::warn!("compositor: activate requested for missing window {window_id}");
                return;
            };
            // Issue #356: raise + key the panel WITHOUT full app activation
            // (`appkit::activate_window`) -- app-wide activation raced the
            // gallery ("main") back to the front on drag/resize/activate,
            // since the panel is nonactivating and AppKit falls back to the
            // last-key ordinary window on reactivation. The panel already
            // has `can_become_key_window: true`, so it can become key
            // directly. Shared by start_drag/begin_resize/activate_window
            // and (transitively, via `compositor_activate_window`) pop_out.
            if let Err(e) = crate::platform::appkit::raise_panel_and_make_key(&window) {
                log::warn!("compositor: failed to raise window {window_id}: {e}");
                return;
            };
            order_chrome_above_panel(&app_for_thread, &key_for_main);
            log::info!("compositor: raised remote window {window_id} (no app activation)");
            after_raise(&app_for_thread, &window);
        },
    );
}

/// Explicit activation/raise path for a remote compositor window. Called by
/// header mousedown before dragging and by the Pop Out button. The panel is
/// raised first; control and pointer children are then kept above it without
/// becoming globally floating windows.
#[tauri::command]
pub fn compositor_activate_window(app: AppHandle, window_id: u32, owner_identity: Option<String>) {
    activate_window(&app, window_id, owner_identity.as_deref());
}

fn raise_window_for_click(
    app: &AppHandle,
    window_id: u32,
    owner_identity: Option<&str>,
    key_control_child: bool,
) {
    // Deliberately `resolve_open_window_key`, NOT `resolve_window_key`: a
    // click must never resurrect a retired window. `activate_window` above
    // does that restore intentionally for its own callers (header
    // drag/resize/Pop Out); a pointerdown racing a republish's retire must
    // not have the same effect, or it can resurrect a phantom window whose
    // publication is already gone.
    let Some(key) = resolve_open_window_key(window_id, owner_identity) else {
        log::warn!(
            "compositor: raise-on-click requested for missing or ambiguous window {window_id}"
        );
        return;
    };
    let panel_label = panel_label_for_key(&key);
    let control_label = control_label_for_key(&key);
    let app_for_thread = app.clone();
    let key_for_main = key.clone();
    crate::platform::on_main(
        app,
        format!("compositor: raise-on-click {window_id}"),
        move || {
            let Some(panel) = app_for_thread.get_webview_window(&panel_label) else {
                log::warn!("compositor: raise-on-click requested for missing window {window_id}");
                return;
            };
            // Issue #678: raise WITHOUT keying the panel. `makeKeyWindow` on
            // the panel would steal key status from the control child this
            // click is actually targeting -- see `raise_panel_only`'s doc
            // comment. Raise, re-assert chrome ordering (#644 invariant),
            // then (only when needed) key the control child itself, all
            // inside this one main-thread closure so no other IPC call can
            // interleave between the raise and the re-key.
            if let Err(e) = crate::platform::appkit::raise_panel_only(&panel) {
                log::warn!("compositor: failed to raise window {window_id} on click: {e}");
                return;
            }
            order_chrome_above_panel(&app_for_thread, &key_for_main);
            // #678 review finding: `WebviewWindow::set_focus` is NOT a bare
            // `makeKeyWindow` -- tao's implementation calls
            // `[NSApp activateIgnoringOtherApps:YES]` before keying the
            // window (tao's macOS `set_focus`). Calling it unconditionally
            // on every click -- including a plain View-mode click, which
            // needs no keyboard delivery to the control overlay at all --
            // would activate the whole app on every click, exactly the
            // #356 regression this command exists to avoid. Only call it
            // when the click is in a mode that actually needs the overlay
            // keyed for keyboard delivery (remote-control mode only -- draw
            // mode never keyed on click, even before #678), which is the
            // same condition the old #450 `compositor_focus_control` path
            // gated on -- this preserves that behaviour exactly rather than
            // widening it.
            if key_control_child {
                if let Some(control) = app_for_thread.get_webview_window(&control_label) {
                    if let Err(e) = control.set_focus() {
                        log::warn!(
                            "compositor: failed to key control overlay for window {window_id} on click: {e}"
                        );
                    }
                }
            }
            log::info!(
                "compositor: raised remote window {window_id} on click (no app activation; \
                 control child keyed: {key_control_child})"
            );
        },
    );
}

/// Raise a remote window when the user clicks anywhere inside it (video area
/// or control overlay), without stealing key status away from the panel.
/// Called from `control/+page.svelte`'s `onPointerDown` on every left-button
/// pointerdown, before any mode-specific (View/remote-control/draw)
/// handling — issue #678. Never restores a retired window (see
/// `raise_window_for_click`'s use of `resolve_open_window_key`).
///
/// `key_control_child` must be true only when the click is in remote-control
/// mode (i.e. keyboard input needs to reach the control overlay) -- draw
/// mode never keyed the overlay on click, even before #678. Passing true
/// unconditionally would activate the whole app on every plain View-mode
/// click too, since `WebviewWindow::set_focus` activates the app before
/// keying (see `raise_window_for_click`'s doc comment above).
#[tauri::command]
pub fn compositor_raise_window_for_click(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
    key_control_child: bool,
) {
    raise_window_for_click(
        &app,
        window_id,
        owner_identity.as_deref(),
        key_control_child,
    );
}

/// #875: one of `owner_identity`'s windows as `compositor_raise_participant_windows`
/// will act on it -- its z-rank (for ordering) and whether it must be
/// restored from `s.retired` before it can be raised.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParticipantWindowEntry {
    key: RemoteWindowKey,
    z_rank: Option<u32>,
    needs_restore: bool,
}

/// Pure: reorder `entries` back-to-front for a restack raise -- the LAST
/// entry in the returned Vec is raised last, so it ends up frontmost.
///
/// Ranked windows (`z_rank: Some`, 0 = the sharer's frontmost) sort by
/// DESCENDING rank, so the most-behind ranked window raises first and rank 0
/// raises last, landing on top. Unranked windows (`z_rank: None` -- an older
/// sharer that omits `petalWindowZOrder`) keep their input relative order
/// (Rust's `sort_by` is stable) and are placed first/rearmost, per #875's
/// documented fallback: unranked windows keep their current relative
/// stacking, behind every ranked one.
fn raise_order_for_participant_windows(
    mut entries: Vec<ParticipantWindowEntry>,
) -> Vec<ParticipantWindowEntry> {
    entries.sort_by(|a, b| match (a.z_rank, b.z_rank) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(rank_a), Some(rank_b)) => rank_b.cmp(&rank_a),
    });
    entries
}

/// #875: enumerate `owner_identity`'s windows, decide which are eligible to
/// raise, and return them already in back-to-front raise order. This is the
/// real enumeration/eligibility/ordering logic
/// `compositor_raise_participant_windows` runs -- factored out so it is
/// testable without a live `AppHandle`, matching this module's existing
/// split between state-only decisions (tested directly) and their thin
/// AppKit-side-effect wrappers (not directly unit-testable; see
/// `raise_window_for_click`, untested itself, built entirely from resolvers
/// that ARE tested).
///
/// * Every currently OPEN window for this owner (`s.windows`) is eligible --
///   it is already rendering, so there is nothing to gate.
/// * A RETIRED window (`s.retired` -- this bucket holds both a user's
///   manual-hide and a genuine sharer-side teardown; nothing in this
///   module's own state tells them apart, see `CompositorState::retired`'s
///   doc comment) is eligible ONLY if `live_window_ids` says the SFU still
///   holds a publication for it. This is the CLAUDE.md never-black-frame
///   rule applied to restoration: "hide only when the SFU holds no
///   publication ... otherwise hold_window_last_frame" -- a retired window
///   with no live publication is a dead phantom (its own teardown already
///   ran) and must never be resurrected, exactly why
///   `raise_window_for_click` uses `resolve_open_window_key` and not
///   `resolve_window_key`. `live_window_ids` is the caller's snapshot of
///   `transport::subscriber::tracked_window_publications()` filtered to this
///   owner -- passed in rather than queried here so this stays a pure
///   function of state plus that one truth source.
fn plan_participant_raise(
    owner_identity: &str,
    live_window_ids: &HashSet<u32>,
) -> Vec<ParticipantWindowEntry> {
    let mut entries: Vec<ParticipantWindowEntry> = with_state(|s| {
        let mut entries: Vec<ParticipantWindowEntry> = s
            .windows
            .iter()
            // #875 review F2: a window `ensure_window` just created is
            // opened HIDDEN (`win.hide()`) and only becomes visible once its
            // first decoded frame reveals it (`revealed_first_frame`).
            // `raise_panel_only`'s own doc comment warns `orderFrontRegardless`
            // un-hides ordered-out windows and requires callers to guarantee
            // visibility first -- an open-but-unrevealed window is neither
            // "already rendering" (the open-window rationale above) nor a
            // restorable retired window, so a pill click during that window
            // must not raise it: doing so would un-hide a hollow, transparent
            // panel with no content in its layer yet.
            .filter(|(key, window)| {
                key.owner_identity == owner_identity && window.revealed_first_frame
            })
            .map(|(key, window)| ParticipantWindowEntry {
                key: key.clone(),
                z_rank: window.z_rank,
                needs_restore: false,
            })
            .collect();
        entries.extend(s.retired.iter().filter_map(|(key, window)| {
            if key.owner_identity != owner_identity || !live_window_ids.contains(&key.window_id) {
                return None;
            }
            Some(ParticipantWindowEntry {
                key: key.clone(),
                z_rank: window.z_rank,
                needs_restore: true,
            })
        }));
        entries
    });
    // Deterministic "current relative order" proxy for the `z_rank: None`
    // fallback group above: this module tracks retirement order
    // (`retired_order`) but nothing analogous for currently-open windows'
    // real on-screen z-stack, so window_id is used as a stable, testable
    // substitute for "current relative stacking" among unranked windows.
    entries.sort_by_key(|entry| entry.key.window_id);
    raise_order_for_participant_windows(entries)
}

/// Raise ALL of `owner_identity`'s remote compositor windows, restacked to
/// match the sharer's own z-order (their frontmost window ends up on top),
/// restoring any the viewer had hidden -- but never a window whose
/// publication is actually gone (see `plan_participant_raise`'s doc
/// comment). Issue #875 (multi-share count pill click).
///
/// Enumeration, restore, and raise all happen inside ONE
/// `platform::on_main` closure -- interleaving IPC between raises is the
/// documented hazard (see `activate_window_then`'s doc comment, and
/// `raise_window_for_click`'s "atomically, on the main thread"). Raise-only,
/// matching `raise_window_for_click`: no `makeKeyWindow`, no app activation
/// (#356) -- the gallery window the user clicked stays key. Sharer z-order
/// changes AFTER this call do not re-raise anything; the rank is read once,
/// at click time.
#[tauri::command]
pub fn compositor_raise_participant_windows(app: AppHandle, owner_identity: String) {
    let app_for_thread = app.clone();
    crate::platform::on_main(
        &app,
        format!("compositor: raise participant windows for '{owner_identity}'"),
        move || {
            let live_window_ids: HashSet<u32> =
                crate::transport::subscriber::tracked_window_publications()
                    .into_iter()
                    .filter(|tracked| tracked.owner_identity == owner_identity)
                    .map(|tracked| tracked.window_id)
                    .collect();
            let plan = plan_participant_raise(&owner_identity, &live_window_ids);
            if plan.is_empty() {
                log::info!(
                    "compositor: raise-participant-windows requested for '{owner_identity}' with no windows"
                );
                return;
            }

            // Restore retired-but-live windows first (mirrors
            // `activate_window_then`'s restore semantics) so every window in
            // `plan` is confirmed open before the raise loop below calls
            // `raise_panel_only`, whose doc comment requires exactly that.
            for entry in &plan {
                if !entry.needs_restore {
                    continue;
                }
                let restored_state = with_state(|s| {
                    s.retired_order.retain(|stored| stored != &entry.key);
                    s.retired.remove(&entry.key)
                });
                let Some(mut win_state) = restored_state else {
                    // Raced away (e.g. a genuine teardown landed between the
                    // plan snapshot and now) -- nothing left to restore.
                    continue;
                };
                let passive_anchor = crate::window_diag::frontmost_normal_window_number();
                show_retired_window_on_main(
                    &app_for_thread,
                    &entry.key,
                    &mut win_state,
                    passive_anchor,
                    "compositor_raise_participant_windows",
                    true,
                );
                with_state(|s| {
                    s.windows.insert(entry.key.clone(), win_state);
                });
                crate::viewer_demand::publish_window_open(&app_for_thread, entry.key.window_id);
            }

            // Raise back-to-front so the last one raised (rank 0, the
            // sharer's frontmost) ends up on top.
            for entry in &plan {
                let panel_label = panel_label_for_key(&entry.key);
                let Some(panel) = app_for_thread.get_webview_window(&panel_label) else {
                    log::warn!(
                        "compositor: raise-participant-windows missing panel for window {}",
                        entry.key.window_id
                    );
                    continue;
                };
                if let Err(e) = crate::platform::appkit::raise_panel_only(&panel) {
                    log::warn!(
                        "compositor: failed to raise window {} for participant '{owner_identity}': {e}",
                        entry.key.window_id
                    );
                    continue;
                }
                order_chrome_above_panel(&app_for_thread, &entry.key);
            }

            log::info!(
                "compositor: raised {} window(s) for participant '{owner_identity}' in z-order (no app activation)",
                plan.len()
            );
        },
    );
}

#[tauri::command]
pub fn compositor_list_windows() -> Vec<RemoteWindowSummary> {
    remote_window_summaries()
}

fn display_enqueue_snapshot_for_key(key: &RemoteWindowKey) -> Option<DisplayEnqueueSnapshot> {
    with_state(|s| {
        s.windows.get(key).map(|window| {
            let source_pixel_size = *window.source_pixel_size.lock_unpoisoned();
            let last_display_enqueued = window.last_display_enqueued_ms.load(Ordering::Relaxed);
            DisplayEnqueueSnapshot {
                source_pixel_width: source_pixel_size.map(|(width, _)| width),
                source_pixel_height: source_pixel_size.map(|(_, height)| height),
                last_display_enqueued_ms: (last_display_enqueued > 0)
                    .then_some(last_display_enqueued),
                frames_display_enqueued: window.frames_display_enqueued.load(Ordering::Relaxed),
                frames_received: window.frames_received.load(Ordering::Relaxed),
            }
        })
    })
}

pub(crate) fn display_enqueue_snapshot(
    owner_identity: &str,
    window_id: u32,
) -> Option<DisplayEnqueueSnapshot> {
    display_enqueue_snapshot_for_key(&RemoteWindowKey::new(owner_identity, window_id))
}

fn remote_window_debug_stats_for_key(key: &RemoteWindowKey) -> Option<RemoteWindowDebugStats> {
    with_state(|s| {
        s.windows.get(key).map(|window| {
            let (content_width, content_height) = *window.panel_content_size.lock_unpoisoned();
            let receiver_scale = *window.receiver_scale.lock_unpoisoned();
            let source_pixel_size = *window.source_pixel_size.lock_unpoisoned();
            let last_frame = window.last_frame_received_ms.load(Ordering::Relaxed);
            let last_display_enqueued = window.last_display_enqueued_ms.load(Ordering::Relaxed);
            RemoteWindowDebugStats {
                window_id: key.window_id,
                owner_identity: key.owner_identity.clone(),
                owner_display_name: window.owner_display_name.clone(),
                source_title: window.source_title.clone(),
                source_url: window.source_url.clone(),
                content_width,
                content_height,
                receiver_scale,
                display_pixel_width: (content_width * receiver_scale).round().max(1.0) as u32,
                display_pixel_height: (content_height * receiver_scale).round().max(1.0) as u32,
                source_pixel_width: source_pixel_size.map(|(width, _)| width),
                source_pixel_height: source_pixel_size.map(|(_, height)| height),
                last_frame_received_ms: (last_frame > 0).then_some(last_frame),
                frames_received: window.frames_received.load(Ordering::Relaxed),
                last_display_enqueued_ms: (last_display_enqueued > 0)
                    .then_some(last_display_enqueued),
                frames_display_enqueued: window.frames_display_enqueued.load(Ordering::Relaxed),
                remote_control_available: window.remote_control_available,
            }
        })
    })
}

#[tauri::command]
pub fn compositor_window_debug_stats(
    window_id: u32,
    owner_identity: Option<String>,
) -> Result<RemoteWindowDebugStats, String> {
    let key = resolve_open_window_key(window_id, owner_identity.as_deref())
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    remote_window_debug_stats_for_key(&key)
        .ok_or_else(|| format!("remote window {window_id} is not open"))
}

/// Inspect one *open* remote compositor panel for the privileged native-peer
/// cockpit.  The owner/window pair is the authenticated track identity, not a
/// display name or a best-effort process scan.
#[cfg(feature = "cockpit-privileged")]
pub(crate) fn cockpit_remote_window_binding(
    app: &AppHandle,
    owner_identity: &str,
    source_window_id: u32,
) -> Result<CockpitRemoteWindowBinding, String> {
    let key = RemoteWindowKey::new(owner_identity, source_window_id);
    let (frames_received, frames_display_enqueued) = with_state(|state| {
        state.windows.get(&key).map(|window| {
            (
                window.frames_received.load(Ordering::Relaxed),
                window.frames_display_enqueued.load(Ordering::Relaxed),
            )
        })
    })
    .ok_or_else(|| format!("remote compositor {owner_identity}:{source_window_id} is not open"))?;
    let panel_label = panel_label_for_key(&key);
    let window = app
        .get_webview_window(&panel_label)
        .ok_or_else(|| format!("remote compositor panel '{panel_label}' is not open"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let result = (|| {
            let cg_window_id = crate::platform::appkit::window_number(&window)?;
            let frame = crate::window_registry::global()
                .map(|r| r.frame_fresh(cg_window_id))
                .unwrap_or_else(|| crate::platform::cg::frame_for_window_id(cg_window_id))
                .ok_or_else(|| {
                    format!("WindowServer cannot see compositor panel {cg_window_id}")
                })?;
            Ok::<_, String>((cg_window_id, frame))
        })();
        let _ = sender.send(result);
    })
    .map_err(|error| format!("dispatch compositor inspection to main thread: {error}"))?;
    let (cg_window_id, frame) = receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "timed out inspecting compositor panel on main thread".to_string())??;
    Ok(CockpitRemoteWindowBinding {
        owner_identity: owner_identity.to_string(),
        source_window_id,
        panel_label,
        cg_window_id,
        frame,
        frames_received,
        frames_display_enqueued,
    })
}

/// Move the exact panel identified by [`cockpit_remote_window_binding`]. This
/// is intentionally not exposed as a Tauri command: it is an internal QA
/// oracle for proving the receiver panel is independently movable.
#[cfg(feature = "cockpit-privileged")]
pub(crate) fn cockpit_move_remote_window(
    app: &AppHandle,
    owner_identity: &str,
    source_window_id: u32,
    x: i32,
    y: i32,
) -> Result<(), String> {
    let key = RemoteWindowKey::new(owner_identity, source_window_id);
    if !with_state(|state| state.windows.contains_key(&key)) {
        return Err(format!(
            "remote compositor {owner_identity}:{source_window_id} is not open"
        ));
    }
    let panel_label = panel_label_for_key(&key);
    let window = app
        .get_webview_window(&panel_label)
        .ok_or_else(|| format!("remote compositor panel '{panel_label}' is not open"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let result = window
            .set_position(tauri::Position::Logical(tauri::LogicalPosition {
                x: f64::from(x),
                y: f64::from(y),
            }))
            .map_err(|error| format!("move remote compositor panel: {error}"));
        let _ = sender.send(result);
    })
    .map_err(|error| format!("dispatch compositor move to main thread: {error}"))?;
    receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "timed out moving compositor panel on main thread".to_string())?
}

/// Does this open remote window advertise that its owner accepts remote
/// control? `set_remote_control_active` refuses to arm the overlay when it does
/// not -- and it refuses from inside the host's status handler, so the only
/// symptom is a control route that silently publishes nothing. RC-N2N/RC-N2W
/// (#819) preflight on this so that failure names itself.
#[cfg(feature = "cockpit-privileged")]
pub(crate) fn cockpit_remote_control_is_offered(
    owner_identity: &str,
    source_window_id: u32,
) -> Result<bool, String> {
    let key = RemoteWindowKey::new(owner_identity, source_window_id);
    with_state(|state| {
        state
            .windows
            .get(&key)
            .map(|window| window.remote_control_available)
    })
    .ok_or_else(|| format!("remote compositor {owner_identity}:{source_window_id} is not open"))
}

/// Run `script` in the REAL control overlay webview of one open remote window
/// -- the same `compositor/control` route a user's clicks land in. RC-N2N
/// (#819) drives its gestures by dispatching DOM events here, so the route's
/// own draft construction is under test; building `RemoteControlDraft`s in
/// Rust instead would bypass exactly the code the scenario exists to exercise.
///
/// `eval` is fire-and-forget: a JS exception inside the script is invisible
/// here. The scenario must therefore never conclude anything from this
/// returning `Ok(())` -- only from the controller publish ledger and the
/// host-side effects downstream of it.
#[cfg(feature = "cockpit-privileged")]
pub(crate) fn cockpit_eval_in_control_overlay(
    app: &AppHandle,
    owner_identity: &str,
    source_window_id: u32,
    script: String,
) -> Result<(), String> {
    let key = RemoteWindowKey::new(owner_identity, source_window_id);
    if !with_state(|state| state.windows.contains_key(&key)) {
        return Err(format!(
            "remote compositor {owner_identity}:{source_window_id} is not open"
        ));
    }
    let label = control_label_for_key(&key);
    let control = app
        .get_webview_window(&label)
        .ok_or_else(|| format!("remote window control overlay '{label}' is not open"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let result = control
            .eval(&script)
            .map_err(|error| format!("eval in control overlay: {error}"));
        let _ = sender.send(result);
    })
    .map_err(|error| format!("dispatch control-overlay eval to main thread: {error}"))?;
    receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "timed out evaluating in the control overlay on main thread".to_string())?
}

#[tauri::command]
pub fn compositor_toggle_debug_panel(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) -> Result<(), String> {
    // Defense in depth (#669): the frontend already hides the Debug button
    // entirely when the setting is off (`debugHeaderControlVisible`), so a
    // real click never reaches this command while disabled -- but that gate
    // lives in a webview a future caller could bypass (same reasoning
    // `ai_chat::commands`'s module doc gives for re-checking its own
    // settings server-side, not trusting the frontend gate alone).
    if !crate::debug_settings::is_enabled() {
        return Err("debug mode is turned off".to_string());
    }
    let key = resolve_open_window_key(window_id, owner_identity.as_deref())
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let label = control_label_for_key(&key);
    let Some(control) = app.get_webview_window(&label) else {
        return Err(format!(
            "remote window {window_id} control overlay is not open"
        ));
    };
    control
        .eval("window.__petalDebugToggle && window.__petalDebugToggle();")
        .map_err(|e| format!("toggle remote-window debug panel: {e}"))
}

#[tauri::command]
pub fn compositor_set_draw_active(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
    active: bool,
) -> Result<(), String> {
    let key = resolve_open_window_key(window_id, owner_identity.as_deref())
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let label = control_label_for_key(&key);
    let Some(control) = app.get_webview_window(&label) else {
        return Err(format!(
            "remote window {window_id} control overlay is not open"
        ));
    };
    let remote_control_active = with_state(|s| {
        if active {
            // The control overlay JS also disables remote-control capture when
            // draw turns on. Keep the native interactivity state in the same
            // mutually-exclusive mode.
            s.remote_control_active.remove(&key);
            false
        } else {
            s.remote_control_active.contains(&key)
        }
    });
    let ignore_cursor_events = control_overlay_ignore_cursor_events(active, remote_control_active);
    let active_json = if active { "true" } else { "false" };
    let app_sync = app.clone();
    let key_sync = key.clone();
    app.run_on_main_thread(move || {
        let result = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            if active {
                if let Err(e) = control.set_ignore_cursor_events(false) {
                    log::warn!(
                        "compositor: failed to make draw overlay interactive for window {window_id}: {e}"
                    );
                }
                if let Err(e) = control.show() {
                    log::warn!(
                        "compositor: failed to show draw overlay for window {window_id}: {e}"
                    );
                }
                if let Err(e) = control.set_focus() {
                    log::warn!(
                        "compositor: failed to focus draw overlay for window {window_id}: {e}"
                    );
                }
                if let Err(e) = control.eval(format!(
                    "window.__petalDrawSetActive && window.__petalDrawSetActive({active_json});"
                )) {
                    log::warn!(
                        "compositor: failed to eval draw active update for window {window_id} overlay '{}': {e}",
                        control.label()
                    );
                }
                // #171: showing/focusing an `addChildWindow` child can resurrect
                // its stale creation-time follow-offset, leaving BOTH the control
                // AND the pointer overlay (which renders the drawer's own stroke
                // locally, see draw.rs's `deliver_update`) parked at
                // `(0, HEADER_HEIGHT)` instead of over the real video -- so the
                // drawer's own strokes render into a mispositioned, effectively
                // invisible surface even though delivery itself succeeded. Redock
                // both overlays to the panel's real current frame every time draw
                // mode turns on, same fix already applied at the other two
                // reveal/show call sites (`reveal_remote_window_after_first_frame_
                // on_main`, `show_retired_window_on_main`).
                let before = current_chrome_frame(&control);
                sync_chrome_to_panel_frame_deferred_with_log(
                    &app_sync,
                    &key_sync,
                    Some(DrawRedockLog {
                        window_id,
                        control_label: control.label().to_string(),
                        control_before: before,
                    }),
                );
                log::info!(
                    "compositor: draw mode active for window {window_id}; requested overlay redock after activation (control_before={before:?})"
                );
            } else {
                if let Err(e) = control.eval(format!(
                    "window.__petalDrawSetActive && window.__petalDrawSetActive({active_json});"
                )) {
                    log::warn!(
                        "compositor: failed to eval draw inactive update for window {window_id} overlay '{}': {e}",
                        control.label()
                    );
                }
                if let Err(e) = control.set_ignore_cursor_events(ignore_cursor_events) {
                    log::warn!(
                        "compositor: failed to restore draw overlay interactivity for window {window_id}: {e}"
                    );
                }
            }
        }));
        if let Err(exception) = result {
            log::error!(
                "compositor: NSException while toggling draw overlay for window {window_id} (caught): {exception:?}"
            );
        }
    })
    .map_err(|e| format!("set remote-window draw mode: {e}"))
}

/// Hide a remote compositor window from the native header. This deliberately
/// delegates to `remove_window`, which hides and retires the panel for reuse.
/// Do not replace this with `window.close()` (#244).
#[tauri::command]
pub fn compositor_hide_window(app: AppHandle, window_id: u32, owner_identity: Option<String>) {
    let Some(owner_identity) =
        owner_identity.or_else(|| owner_identity_for_window(window_id, None))
    else {
        log::warn!("compositor: hide requested for missing or ambiguous window {window_id}");
        return;
    };
    remove_window(
        &app,
        &owner_identity,
        window_id,
        RemoveWindowReason::ManualHide,
    );
}

/// Pop-out (SPEC.md §4.4 "built-in actions -- pop-out / fit-to-source-size").
/// Raises the window to the front and gives it normal-window key focus --
/// "pop out" of the ambient floating layer into full interactive focus, the
/// natural reading of the action given this is already a real, independent
/// native window (there is no separate "docked" mode to pop out of in this
/// implementation, unlike a tiled-grid product where pop-out means "become a
/// standalone window" -- here every remote window already is one, so this
/// command's job is bringing it forward, not re-parenting it).
#[tauri::command]
pub fn compositor_pop_out(app: AppHandle, window_id: u32, owner_identity: Option<String>) {
    // Same implementation as `compositor_activate_window`, kept as a
    // separate IPC command because command names are frontend/API contracts:
    // "activate" is the header/drag affordance, "pop out" is the visible
    // control label.
    compositor_activate_window(app, window_id, owner_identity);
}

/// Fit-to-source-size (SPEC.md §4.4): resize back to the real source
/// window's native resolution (in logical points at 1:1, i.e. the last
/// known real `content_size`, which IS the source's real captured size --
/// see `push_frame`'s resize-on-first-frame logic).
#[tauri::command]
pub fn compositor_fit_to_source(app: AppHandle, window_id: u32, owner_identity: Option<String>) {
    let Some(key) = resolve_open_window_key(window_id, owner_identity.as_deref()) else {
        return;
    };
    let state = with_state(|s| {
        s.windows.get(&key).map(|w| {
            cancel_programmatic_resize(w);
            *w.panel_content_size.lock_unpoisoned()
        })
    });
    if let Some((w, h)) = state {
        resize_to_source(&app, &key.owner_identity, window_id, w, h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_display::{display_filter_for_geometry, DisplayLayerFilter};
    use std::collections::BTreeMap;

    // ------------------------------------------------------------------
    // Attach seam: measured content size (defect E2, 2026-07-30).
    // `attach_display_layer` sizes the video view from the MEASURED panel
    // frame (`content_geometry` -> `content_size_from_outer`), falling back
    // to the caller's remembered size only when the frame is unreadable.
    // These pin the measurement math for the paths the adversarial review
    // named as reachable with stale remembered state.
    // ------------------------------------------------------------------

    #[test]
    fn a_header_only_panel_measures_a_floor_content_height_not_its_remembered_size() {
        // The review's worst case: a window stripped for the pool at a
        // header-only outer height still remembers its expanded
        // `panel_content_size` (e.g. 405pt). Rehydrating its display layer
        // must size the video view from the measured 44pt header-only panel
        // — the 1pt floor — or a full-width strip of live video paints
        // across the header.
        let (width, content_h) = content_size_from_outer(640.0, HEADER_HEIGHT, 1.0);
        assert_eq!(width, 640.0);
        assert_eq!(content_h, 1.0);
    }

    #[test]
    fn an_expanded_panel_measures_its_content_area_below_the_header() {
        let (width, content_h) = content_size_from_outer(720.0, HEADER_HEIGHT + 405.0, 1.0);
        assert_eq!(width, 720.0);
        assert_eq!(content_h, 405.0);
    }

    #[test]
    fn retina_outer_pixels_measure_the_same_logical_content_area() {
        let (width, content_h) =
            content_size_from_outer(1440.0, 2.0 * (HEADER_HEIGHT + 405.0), 2.0);
        assert_eq!(width, 720.0);
        assert_eq!(content_h, 405.0);
    }

    #[test]
    fn remove_window_reasons_are_distinct_and_diagnostic_labels_are_stable() {
        let reasons = [
            (RemoveWindowReason::TrackUnsubscribed, "track-unsubscribed"),
            (RemoveWindowReason::TrackUnpublished, "track-unpublished"),
            (
                RemoveWindowReason::ParticipantDisconnected,
                "participant-disconnected",
            ),
            (RemoveWindowReason::NoFrameWatchdog, "no-frame-watchdog"),
            (RemoveWindowReason::ManualHide, "manual-hide"),
            (RemoveWindowReason::LeaveRoom, "leave-room"),
        ];
        let labels: Vec<_> = reasons.iter().map(|(reason, _)| reason.label()).collect();

        assert_eq!(
            reasons.iter().map(|(_, label)| *label).collect::<Vec<_>>(),
            labels
        );
        assert_eq!(
            labels
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            reasons.len()
        );
    }

    /// #679: the transport-side reasons (a full reconnect's
    /// `ParticipantDisconnected`, a stalled `NoFrameWatchdog`, a deliberate
    /// `ManualHide`, and `ReconciledUnrecoverable`) must suppress the very
    /// next re-subscribe's "is sharing a window" pill -- this is the exact
    /// case a naive `is_open_for_owner` gate gets wrong (#631: it would fire
    /// the pill for every existing share on every reconnect).
    #[test]
    fn share_pill_suppression_set_by_transport_side_reasons() {
        for (index, reason) in [
            RemoveWindowReason::ParticipantDisconnected,
            RemoveWindowReason::NoFrameWatchdog,
            RemoveWindowReason::ManualHide,
            RemoveWindowReason::ReconciledUnrecoverable,
        ]
        .into_iter()
        .enumerate()
        {
            let key = RemoteWindowKey::new(
                format!("share-pill-test-transport-{index}"),
                9000 + index as u32,
            );
            record_share_pill_suppression_for_remove_reason(&key, reason);
            assert!(
                consume_share_started_pill_suppression(&key.owner_identity, key.window_id),
                "{reason:?} must suppress the next re-subscribe's pill"
            );
        }
    }

    /// A genuine end of a share (sharer-side unpublish, the SFU confirming
    /// the publication is gone, or leaving the room) must NOT suppress a
    /// later real re-share -- it clears any stale suppression instead, so a
    /// deliberate stop-and-restart of the same share still fires the pill.
    #[test]
    fn share_pill_suppression_cleared_by_genuine_end_reasons() {
        for (index, reason) in [
            RemoveWindowReason::TrackUnsubscribed,
            RemoveWindowReason::TrackUnpublished,
            RemoveWindowReason::ReconciledPublicationGone,
            RemoveWindowReason::LeaveRoom,
        ]
        .into_iter()
        .enumerate()
        {
            let key = RemoteWindowKey::new(
                format!("share-pill-test-genuine-{index}"),
                9100 + index as u32,
            );
            // Start suppressed, as if a prior transport-side teardown had set
            // it -- the genuine-end reason must clear it, not leave it stuck.
            record_share_pill_suppression_for_remove_reason(
                &key,
                RemoveWindowReason::ParticipantDisconnected,
            );
            record_share_pill_suppression_for_remove_reason(&key, reason);
            assert!(
                !consume_share_started_pill_suppression(&key.owner_identity, key.window_id),
                "{reason:?} must clear suppression so a real re-share fires the pill"
            );
        }
    }

    /// One-shot: only the very next re-subscribe after a transport-side
    /// teardown is silenced. A second consume (nothing recorded the key
    /// again in between) must fall through to firing the pill normally.
    #[test]
    fn share_pill_suppression_is_consumed_once() {
        let key = RemoteWindowKey::new("share-pill-test-one-shot", 9200);
        record_share_pill_suppression_for_remove_reason(
            &key,
            RemoveWindowReason::ParticipantDisconnected,
        );
        assert!(consume_share_started_pill_suppression(
            &key.owner_identity,
            key.window_id
        ));
        assert!(!consume_share_started_pill_suppression(
            &key.owner_identity,
            key.window_id
        ));
    }

    /// A key that was never suppressed must not spuriously suppress a pill.
    #[test]
    fn share_pill_suppression_absent_by_default() {
        assert!(!consume_share_started_pill_suppression(
            "share-pill-test-never-suppressed",
            9300
        ));
    }

    /// #679 review finding: a genuine end must clear a stale suppression
    /// even for a key that is NOT currently in `s.windows` -- e.g. a window
    /// already retired by an earlier ManualHide/NoFrameWatchdog, whose
    /// eventual real TrackUnsubscribed/TrackUnpublished must still clear the
    /// suppression it set so a later genuine re-share isn't silently eaten.
    ///
    /// This does NOT call the real `remove_window` (it needs a live
    /// `&AppHandle`, and this crate has no test-mode Tauri app fixture --
    /// `tauri`'s `test` feature is not enabled, adding it is a larger,
    /// separate change). What it DOES prove: the classification function
    /// itself works correctly regardless of `s.windows`/`s.retired`
    /// membership, which is the invariant `remove_window`'s fix (hoisting
    /// the classification call above the early `s.windows.remove` lookup)
    /// depends on. The fix's actual wiring -- that `remove_window` calls
    /// this BEFORE its early return, not after -- is pinned by the
    /// source-grep test in `apps/desktop/tests/shareNotice.test.ts`
    /// asserting the call precedes `let Some(removed) = removed else`.
    #[test]
    fn share_pill_suppression_genuine_end_clears_even_for_a_key_outside_s_windows() {
        let key = RemoteWindowKey::new("share-pill-test-retired-key", 9400);

        // Simulate: an earlier ManualHide already retired this key and set
        // the suppression -- the key is NOT in s.windows at this point,
        // exactly the state `remove_window`'s early return used to bail out
        // on before ever reaching classification.
        record_share_pill_suppression_for_remove_reason(&key, RemoveWindowReason::ManualHide);
        assert!(
            with_state(|s| s.suppressed_reshare_pill.contains(&key)),
            "precondition: suppression must be set before the genuine end arrives"
        );
        with_state(|s| {
            assert!(
                !s.windows.contains_key(&key),
                "precondition: the key must be outside s.windows, matching a retired window"
            );
        });

        // The genuine end arrives for this same (still-outside-s.windows)
        // key. Classification alone -- independent of any s.windows lookup
        // -- must clear the suppression.
        record_share_pill_suppression_for_remove_reason(
            &key,
            RemoveWindowReason::TrackUnsubscribed,
        );

        assert!(
            !consume_share_started_pill_suppression(&key.owner_identity, key.window_id),
            "a genuine end must clear a stale suppression even when the key was never in \
             s.windows -- otherwise a real stop-and-restart of a previously-hidden share stays \
             silently suppressed forever"
        );
    }

    /// #679 review finding: leaving the room must clear EVERY suppression
    /// entry, including ones for keys that were already retired (and so
    /// were never visited by `remove_all_windows`'s own `s.windows`-keyed
    /// loop) -- a fresh join later must start every share as genuinely new.
    #[test]
    fn remove_all_windows_clears_every_suppression_entry_including_retired_keys() {
        let open_key = RemoteWindowKey::new("share-pill-test-leave-open", 9500);
        let retired_key = RemoteWindowKey::new("share-pill-test-leave-retired", 9501);

        with_state(|s| {
            s.windows.remove(&open_key);
            s.windows.insert(
                open_key.clone(),
                test_window(&open_key.owner_identity, "Open"),
            );
        });
        // retired_key is deliberately NOT inserted into s.windows -- it
        // models a key already retired before LeaveRoom runs, so
        // remove_all_windows's `s.windows.keys()` loop never visits it.
        record_share_pill_suppression_for_remove_reason(&open_key, RemoveWindowReason::ManualHide);
        record_share_pill_suppression_for_remove_reason(
            &retired_key,
            RemoveWindowReason::ManualHide,
        );
        assert!(with_state(|s| s
            .suppressed_reshare_pill
            .contains(&open_key)));
        assert!(with_state(|s| s
            .suppressed_reshare_pill
            .contains(&retired_key)));

        with_state(|s| s.suppressed_reshare_pill.clear());

        assert!(!consume_share_started_pill_suppression(
            &open_key.owner_identity,
            open_key.window_id
        ),);
        assert!(
            !consume_share_started_pill_suppression(
                &retired_key.owner_identity,
                retired_key.window_id
            ),
            "a suppression entry for an already-retired key must not survive LeaveRoom either"
        );

        with_state(|s| {
            s.windows.remove(&open_key);
        });
    }

    #[test]
    fn hold_window_reasons_are_distinct_and_diagnostic_labels_are_stable() {
        let reasons = [
            (HoldWindowReason::ReplacementInbound, "replacement-inbound"),
            (HoldWindowReason::NoFrameWatchdog, "no-frame-watchdog"),
            (
                HoldWindowReason::ParticipantReconnect,
                "participant-reconnect",
            ),
            (
                HoldWindowReason::ReconciledUnrecoverable,
                "reconciled-unrecoverable",
            ),
        ];
        let labels: Vec<_> = reasons.iter().map(|(reason, _)| reason.label()).collect();

        assert_eq!(
            reasons.iter().map(|(_, label)| *label).collect::<Vec<_>>(),
            labels
        );
        assert_eq!(
            labels
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            reasons.len()
        );
    }

    #[test]
    fn draw_redock_after_log_includes_final_control_frame() {
        let before = Some(ChromeFrame {
            x: 0.0,
            y: 28.0,
            width: 640.0,
            height: 360.0,
        });
        let after = Some(ChromeFrame {
            x: 120.0,
            y: 148.0,
            width: 640.0,
            height: 360.0,
        });

        assert_eq!(
            format_draw_redock_after_log(42, "remote-window-control-owner-42", before, after),
            "compositor: draw mode active for window 42; overlay redock landed (control_label='remote-window-control-owner-42', control_before=Some(ChromeFrame { x: 0.0, y: 28.0, width: 640.0, height: 360.0 }), control_after=Some(ChromeFrame { x: 120.0, y: 148.0, width: 640.0, height: 360.0 }))"
        );
    }

    #[test]
    fn source_resize_decision_table_keeps_panel_and_source_state_separate() {
        let cases = [
            (
                "first coherent source size",
                None,
                Some((960.0, 540.0)),
                false,
                ResizeDecision::Apply,
            ),
            (
                "same logical size after simulcast layer switch",
                Some((960.0, 540.0)),
                Some((960.0, 540.0)),
                false,
                ResizeDecision::Ignore,
            ),
            (
                "first-open policy reapplied",
                Some((960.0, 540.0)),
                Some((960.0, 540.0)),
                false,
                ResizeDecision::Ignore,
            ),
            (
                "genuine source change while idle",
                Some((960.0, 540.0)),
                Some((1280.0, 720.0)),
                false,
                ResizeDecision::Apply,
            ),
            (
                "genuine source change during drag",
                Some((960.0, 540.0)),
                Some((1280.0, 720.0)),
                true,
                ResizeDecision::Latch,
            ),
            (
                "no source change during drag",
                Some((960.0, 540.0)),
                Some((960.0, 540.0)),
                true,
                ResizeDecision::Ignore,
            ),
        ];
        for (name, previous, current, dragging, expected) in cases {
            assert_eq!(
                resize_decision(previous, current, dragging),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn resize_gesture_backstop_expires_a_stale_active_flag_but_not_a_fresh_one() {
        let win = test_window("owner", "title");
        // Neither TTL nor active flag set: no gesture in progress.
        assert!(!resize_gesture_in_progress(&win));

        // Active flag set just now: in progress.
        win.user_resize_active.store(true, Ordering::Relaxed);
        win.user_resize_active_since_ms
            .store(now_ms(), Ordering::Relaxed);
        assert!(resize_gesture_in_progress(&win));

        // Active flag set, but stamped long enough ago to exceed the
        // backstop -- a lost finalize IPC must not latch this forever
        // (#416 review finding).
        win.user_resize_active_since_ms.store(
            now_ms().saturating_sub(MAX_USER_RESIZE_GESTURE_MS + 1_000),
            Ordering::Relaxed,
        );
        assert!(!resize_gesture_in_progress(&win));

        // The short per-move TTL still works independently of the flag.
        win.user_resize_active.store(false, Ordering::Relaxed);
        win.user_resize_until_ms
            .store(now_ms().saturating_add(USER_RESIZE_TTL), Ordering::Relaxed);
        assert!(resize_gesture_in_progress(&win));

        // A completed gesture (active=false, TTL expired) must read as NOT
        // in progress even though `since_ms` is still recent -- finalize
        // clears `active` but deliberately leaves `since_ms` alone (#416
        // follow-up review nit: a wrong implementation keyed only off
        // `since_ms`, ignoring the active flag, would suppress
        // reconciliation for 30s after every completed drag, not just a
        // lost one).
        win.user_resize_until_ms.store(0, Ordering::Relaxed);
        assert!(!win.user_resize_active.load(Ordering::Relaxed));
        assert!(!resize_gesture_in_progress(&win));
    }

    #[test]
    fn region_frames_adopt_growth_and_shrink_but_zero_sizes_are_ignored() {
        assert!(region_frame_is_new_source_size(
            Some((640, 400)),
            (800, 500)
        ));
        assert!(region_frame_is_new_source_size(
            Some((800, 500)),
            (640, 400)
        ));
        assert!(!region_frame_is_new_source_size(
            Some((640, 400)),
            (640, 400)
        ));
        assert!(!region_frame_is_new_source_size(Some((640, 400)), (0, 400)));
        assert!(region_frame_is_new_source_size(None, (640, 400)));
    }

    #[test]
    fn logical_source_size_ignores_pixel_grid_changes_that_preserve_points() {
        // A simulcast layer switch changes decoded pixels only. The
        // publisher's dimensions and scale metadata are unchanged on the
        // wire; this test must not model the layer switch as a metadata-scale
        // change, because that is not a real wire invariant.
        let first = canonical_source_size_for_frame(Some((1920, 1080)), 1920, 1080);
        let lower_layer = canonical_source_size_for_frame(Some((1920, 1080)), 640, 360);
        assert_eq!(first, Some((1920, 1080)));
        assert_eq!(lower_layer, first);
        assert!(logical_size_matches(
            source_presentation_size_points(first.unwrap().0, first.unwrap().1, 2.0),
            source_presentation_size_points(lower_layer.unwrap().0, lower_layer.unwrap().1, 2.0),
        ));
    }

    #[test]
    fn decoded_size_is_only_a_fallback_until_canonical_publication_size_arrives() {
        assert_eq!(
            canonical_source_size_for_frame(None, 640, 360),
            Some((640, 360))
        );
        assert_eq!(
            canonical_source_size_for_frame(Some((1920, 1080)), 640, 360),
            Some((1920, 1080))
        );
    }

    #[test]
    fn paused_drag_stays_active_after_the_short_ttl_expires() {
        let win = test_window("owner", "title");
        win.user_resize_active.store(true, Ordering::Release);
        win.user_resize_active_since_ms
            .store(now_ms(), Ordering::Relaxed);
        win.user_resize_until_ms.store(0, Ordering::Relaxed);

        // A held pointer may pause longer than USER_RESIZE_TTL. The explicit
        // drag state, not that TTL, must continue to latch source changes.
        assert!(resize_gesture_in_progress(&win));
    }

    #[derive(Debug, Default)]
    struct LifecycleModel {
        open: BTreeMap<u32, u32>,
        retired: BTreeMap<u32, u32>,
        revealed: BTreeMap<u32, bool>,
        layer_has_content: BTreeMap<u32, bool>,
        retired_order: Vec<u32>,
        stripped: Vec<u32>,
        next_cascade_slot: u32,
        hidden_order: Vec<String>,
    }

    impl LifecycleModel {
        fn ensure(&mut self, window_id: u32) -> LifecycleAction {
            if self.open.contains_key(&window_id) {
                return LifecycleAction::AlreadyOpen;
            }
            if let Some(slot) = self.retired.remove(&window_id) {
                self.retired_order.retain(|id| *id != window_id);
                self.stripped.retain(|id| *id != window_id);
                let layer_has_content = self
                    .layer_has_content
                    .get(&window_id)
                    .copied()
                    .unwrap_or(false);
                let revealed = self.revealed.entry(window_id).or_default();
                apply_retired_reuse_reveal_state(revealed, layer_has_content);
                self.open.insert(window_id, slot);
                return LifecycleAction::Reused { slot };
            }
            let slot = self.next_cascade_slot;
            self.next_cascade_slot += 1;
            self.open.insert(window_id, slot);
            self.revealed.insert(window_id, false);
            self.layer_has_content.insert(window_id, false);
            LifecycleAction::Created { slot }
        }

        fn enqueue_display_sample(&mut self, window_id: u32) {
            assert!(self.open.contains_key(&window_id));
            self.layer_has_content.insert(window_id, true);
            self.revealed.insert(window_id, true);
        }

        fn is_revealed(&self, window_id: u32) -> bool {
            self.revealed.get(&window_id).copied().unwrap_or(false)
        }

        fn layer_has_content(&self, window_id: u32) -> bool {
            self.layer_has_content
                .get(&window_id)
                .copied()
                .unwrap_or(false)
        }

        fn remove(&mut self, window_id: u32) {
            let Some(slot) = self.open.remove(&window_id) else {
                return;
            };
            let key = test_key("owner", window_id);
            for label in [
                header_label_for_key(&key),
                control_label_for_key(&key),
                pointer_label_for_key(&key),
                panel_label_for_key(&key),
            ] {
                self.hidden_order.push(label);
            }
            self.retired_order.retain(|id| *id != window_id);
            self.retired_order.push(window_id);
            self.retired.insert(window_id, slot);
            self.revealed.insert(window_id, false);
            self.enforce_retired_pool_cap(RETIRED_WARM_POOL_CAP);
        }

        fn hide_from_header(&mut self, window_id: u32) {
            self.remove(window_id);
        }

        fn activate(&mut self, window_id: u32) -> bool {
            if self.open.contains_key(&window_id) {
                return true;
            }
            let Some(slot) = self.retired.remove(&window_id) else {
                return false;
            };
            self.retired_order.retain(|id| *id != window_id);
            self.stripped.retain(|id| *id != window_id);
            self.open.insert(window_id, slot);
            true
        }

        fn listed_windows(&self) -> Vec<(u32, bool)> {
            let mut entries: Vec<_> = self
                .open
                .keys()
                .map(|id| (*id, false))
                .chain(self.retired.keys().map(|id| (*id, true)))
                .collect();
            entries.sort_by_key(|entry| (entry.1, entry.0));
            entries
        }

        fn enforce_retired_pool_cap(&mut self, cap: usize) {
            self.retired_order
                .retain(|id| self.retired.contains_key(id));
            while self.retired_order.len() > cap {
                let evicted = self.retired_order.remove(0);
                if !self.stripped.contains(&evicted) {
                    self.stripped.push(evicted);
                }
                self.layer_has_content.insert(evicted, false);
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum LifecycleAction {
        AlreadyOpen,
        Created { slot: u32 },
        Reused { slot: u32 },
    }

    fn test_key(owner: &str, window_id: u32) -> RemoteWindowKey {
        RemoteWindowKey::new(owner, window_id)
    }

    fn test_window(owner: &str, title: &str) -> CompositorWindow {
        CompositorWindow {
            remote_control_disallowed: false,
            owner_identity: owner.to_string(),
            owner_display_name: owner.to_string(),
            owner_palette_index: None,
            source_title: title.to_string(),
            source_url: None,
            source_kind: SharedSourceKind::Window,
            share_instance_id: None,
            display: None,
            source_scale: 1.0,
            panel_content_size: Mutex::new((DEFAULT_CONTENT_WIDTH, DEFAULT_CONTENT_HEIGHT)),
            source_presentation_size: Mutex::new(None),
            canonical_source_pixel_size: Mutex::new(None),
            programmatic_resize_events: Mutex::new(ProgrammaticResizeEvents::default()),
            next_programmatic_resize_generation: AtomicU64::new(0),
            canonical_source_epoch: AtomicU64::new(1),
            canonical_source_generation: AtomicU64::new(0),
            pending_source_resize: Mutex::new(None),
            user_resize_until_ms: AtomicU64::new(0),
            user_resize_active: AtomicBool::new(false),
            user_resize_active_since_ms: AtomicU64::new(0),
            receiver_scale: Mutex::new(1.0),
            source_pixel_size: Mutex::new(None),
            remote_control_available: true,
            last_frame_received_ms: AtomicU64::new(0),
            frames_received: AtomicU64::new(0),
            last_display_enqueued_ms: AtomicU64::new(0),
            frames_display_enqueued: AtomicU64::new(0),
            pending_display_samples: PendingFrameQueue::default(),
            revealed_first_frame: false,
            layer_has_content: false,
            held_reason: None,
            stripped_for_pool: false,
            app_origin: None,
            ai_chat_overlay_open: false,
            z_rank: None,
        }
    }

    // ---- #627 hold-last-frame state transitions --------------------------
    //
    // `hold_window_last_frame` itself needs an `AppHandle` (it evals the
    // header's paused label), so these drive the state transitions it and
    // `drain_pending_display_samples_on_main` perform on window state. The
    // rendered-pixel proof is a separate, deliberately non-unit gate:
    // `examples/hold_last_frame_probe` + `scripts/verify-no-black-frame-native.sh`.

    /// The idempotence `hold_window_last_frame` relies on. A reconcile
    /// divergence recurs on every 5s pass and a stall can be re-observed, so
    /// without this each would re-eval the header JS and re-log forever.
    #[test]
    fn holding_an_already_held_window_is_a_no_op() {
        let mut window = test_window("bob", "Editor");
        window.revealed_first_frame = true;

        let first = window.held_reason == Some(HoldWindowReason::NoFrameWatchdog);
        window.held_reason = Some(HoldWindowReason::NoFrameWatchdog);
        assert!(!first, "the first hold must do real work");

        let second = window.held_reason == Some(HoldWindowReason::NoFrameWatchdog);
        assert!(second, "a repeat hold for the same reason must be a no-op");
    }

    /// A window that has never shown a frame has nothing to hold: it is still
    /// behind the first-frame reveal gate, so "keep showing it" would mean
    /// keeping an unfed layer on screen. Callers fall back to a real teardown.
    #[test]
    fn a_window_with_no_first_frame_is_not_holdable() {
        let window = test_window("bob", "Editor");
        assert!(
            !window.revealed_first_frame,
            "a fresh window must not claim a holdable frame"
        );
    }

    /// Frames reaching the layer are what clear the hold -- not any event. This
    /// is what stops a recovered stall from keeping an honest-but-stale "paused"
    /// label over live video when no new `TrackSubscribed` ever arrives.
    #[test]
    fn a_frame_reaching_the_layer_clears_the_hold() {
        let mut window = test_window("bob", "Editor");
        window.revealed_first_frame = true;
        window.held_reason = Some(HoldWindowReason::NoFrameWatchdog);

        let resumed_from_hold = window.held_reason.take().is_some();
        assert!(resumed_from_hold, "resuming must report the cleared hold");
        assert_eq!(window.held_reason, None);

        // Idempotent on the next frame: no repeated label clearing.
        assert!(!window.held_reason.take().is_some());
    }

    /// A reused (previously retired) window must not inherit a stale hold, or
    /// it would open labelled paused while live frames arrive.
    #[test]
    fn reusing_a_retired_window_clears_any_previous_hold() {
        let mut window = test_window("bob", "Editor");
        window.held_reason = Some(HoldWindowReason::ReconciledUnrecoverable);
        window.revealed_first_frame = true;
        window.layer_has_content = true;

        // The reuse branch's resets, in `ensure_window`'s order.
        let reveal_now = apply_retired_reuse_reveal_state(
            &mut window.revealed_first_frame,
            window.layer_has_content,
        );
        window.held_reason = None;

        assert_eq!(window.held_reason, None);
        assert!(reveal_now);
        assert!(window.revealed_first_frame);
    }

    // ---- #416 residual: a user gesture must latch, not discard, an
    // in-flight source-driven resize it cancels ----------------------------
    //
    // These drive the REAL statement sequence `compositor_begin_resize` runs
    // against window state -- `cancel_programmatic_resize` ->
    // `latch_source_reconciliation` -- not a pure helper beside it. The
    // command itself needs an `AppHandle` (it also activates the window), so
    // `cancel_programmatic_resize_for_user_gesture` IS the command's cancel+
    // latch state transition -- called here, not re-implemented; the command
    // body contains no others. (A removed remote-window feature used to be
    // a second trigger for this same defect -- #675 removed that trigger
    // entirely, so `compositor_begin_resize` is the surviving programmatic
    // writer this coverage re-points to. The drain half -- re-reading
    // canonical state at pointer-up -- lives in
    // `compositor_resize_window`'s finalize path, untouched by #675 and
    // already exercised by the resize-race harness below.) Each test
    // carries its own positive control: a run in which the
    // latch MUST be produced, asserted in the same test as the negative it is
    // paired with (CLAUDE.md / THE RULE).

    #[test]
    fn a_user_gesture_latches_an_in_flight_source_resize_it_cancels() {
        let window = test_window("owner", "Source");
        *window.canonical_source_pixel_size.lock_unpoisoned() = Some((1440, 600));

        // POSITIVE CONTROL for "a source resize was genuinely in flight": the
        // request must exist and be cancellable before the gesture begins.
        let in_flight = prepare_programmatic_resize_request(&window, 800.0, 333.0);
        assert!(
            in_flight.is_some(),
            "control: a source-driven request must be creatable with no gesture active"
        );

        // The gesture beginning cancels it. Before the #416 fix the cancel's
        // return value was discarded and the source change was lost entirely.
        cancel_programmatic_resize_for_user_gesture(&window);
        assert!(
            window.pending_source_resize.lock_unpoisoned().is_some(),
            "a user gesture must latch the source resize it just cancelled"
        );
    }

    #[test]
    fn a_user_gesture_without_an_in_flight_source_resize_latches_nothing() {
        let window = test_window("owner", "Source");
        *window.canonical_source_pixel_size.lock_unpoisoned() = Some((1440, 600));

        // NEGATIVE: nothing in flight, so nothing is owed.
        cancel_programmatic_resize_for_user_gesture(&window);
        assert!(
            window.pending_source_resize.lock_unpoisoned().is_none(),
            "a user gesture must not invent a source reconciliation"
        );

        // POSITIVE CONTROL in the same run: the very same sequence WITH a
        // request in flight does produce a latch, so the negative above is a
        // real absence and not an inert test.
        assert!(prepare_programmatic_resize_request(&window, 800.0, 333.0).is_some());
        cancel_programmatic_resize_for_user_gesture(&window);
        assert!(
            window.pending_source_resize.lock_unpoisoned().is_some(),
            "control: the same path must latch when a source resize really was in flight"
        );
    }

    // ---- #416 resize-race interleaving harness ---------------------------
    //
    // Drives the REAL listener decision path (`resize_listener_outcome` -- the
    // same function `install_aspect_lock`'s installed closure calls) and the
    // REAL command path (`cancel_programmatic_resize`,
    // `prepare_programmatic_resize_request`, `resize_decision`,
    // `proportional_content_size_for_source_change`) through EVERY interleaving
    // of a user drag and a concurrent source-driven resize.
    //
    // Exhaustive enumeration rather than random threads: the ordering space is
    // small, so 100% coverage is both reproducible and stronger evidence than a
    // probabilistic soak. Per CLAUDE.md, isolated pure-helper tests are not
    // sufficient for this class of change.
    //
    // The three aspect ratios are deliberately DIFFERENT: the panel starts at
    // 1.6, the source changes to 4:3, and the drag is issued along the old 1.6
    // aspect. That is what makes the aspect-lock correction fire and lets the
    // tests observe #416's "wrong aspect / border gaps" symptom, not just
    // "wrong size".

    const RACE_SCALE: f64 = 2.0;
    const RACE_PANEL_W: f64 = 640.0;
    const RACE_PANEL_H: f64 = 400.0; // aspect 1.6
    const RACE_SOURCE_W: f64 = 320.0;
    const RACE_SOURCE_H: f64 = 240.0; // aspect 4:3 -- genuinely different
    const RACE_USER_W: f64 = 900.0;
    const RACE_USER_H: f64 = 562.5; // dragged along the OLD 1.6 aspect

    fn race_source_aspect() -> f64 {
        RACE_SOURCE_W / RACE_SOURCE_H
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RaceOp {
        /// `compositor_begin_resize`'s state half: a real pointer-down.
        UserBegin,
        /// One drag tick's native `Resized` callback.
        UserTick,
        /// `finalize: true`, including the real latch drain.
        UserFinalize,
        /// A source republish RETIRES the window and re-reveals it from the
        /// reuse pool -- `ensure_window`'s `s.retired.remove(&key)` branch.
        /// The window survives (panels are never destroyed), but this is the
        /// lifecycle hop that used to reset the drag state (#416).
        RetireReveal,
        /// The decode/republish thread evaluates the source-resize policy.
        SourceDecide,
        /// ...and only later actually issues the native `set_size`. The gap
        /// between these two is the TOCTOU window a single mutex does not
        /// close, because they run under separate `with_state` acquisitions.
        SourceApply,
        /// AppKit delivers the callback for that programmatic `set_size`.
        SourceCallback,
    }

    struct RaceHarness {
        window: CompositorWindow,
        decision: Option<ResizeDecision>,
        applied: Option<ProgrammaticResizeTransaction>,
        /// Settles to a geometry other than the user's, while the user's
        /// pointer was still down -- the "it jumps back to small" symptom.
        violations: Vec<(f64, f64)>,
        /// GROUND TRUTH for "is the pointer physically down", tracked by the
        /// harness independently of the window's own gesture bit.
        ///
        /// Scoring violations against `resize_gesture_in_progress` instead is
        /// exactly how this harness missed #416: a retire -> reveal cycle
        /// CLEARED the bit mid-drag, so the panel moved under a held pointer
        /// while the predicate the test trusted said "idle" and reported a
        /// pass. A test may never take the state under test as its oracle.
        pointer_down: bool,
    }

    fn race_source_target() -> InitialResizeTarget {
        InitialResizeTarget {
            source_width_px: RACE_SOURCE_W as u32,
            source_height_px: RACE_SOURCE_H as u32,
            source_scale: 1.0,
            fallback_content_w: RACE_SOURCE_W,
            fallback_content_h: RACE_SOURCE_H,
        }
    }

    impl RaceHarness {
        fn new() -> Self {
            let window = test_window("owner", "title");
            *window.panel_content_size.lock_unpoisoned() = (RACE_PANEL_W, RACE_PANEL_H);
            *window.source_presentation_size.lock_unpoisoned() = Some((RACE_PANEL_W, RACE_PANEL_H));
            *window.canonical_source_pixel_size.lock_unpoisoned() =
                Some((RACE_PANEL_W as u32, RACE_PANEL_H as u32));
            Self {
                window,
                decision: None,
                applied: None,
                pointer_down: false,
                violations: Vec::new(),
            }
        }

        /// The installed listener's effect chain: the same decision function
        /// the production closure calls, followed by the same settled-geometry
        /// write it performs.
        fn deliver_resize_event(&mut self, width: f64, content_height: f64) {
            let outcome =
                resize_listener_outcome(&self.window, width, content_height, RACE_SCALE, true);
            let ResizeListenerOutcome::Settled { content_height, .. } = outcome else {
                return;
            };
            record_settled_panel_content_geometry(&self.window, width, content_height, RACE_SCALE);
            if self.pointer_down && (width - RACE_USER_W).abs() > 0.5 {
                self.violations.push((width, content_height));
            }
        }

        fn panel(&self) -> (f64, f64) {
            *self.window.panel_content_size.lock_unpoisoned()
        }

        fn step(&mut self, op: RaceOp) {
            match op {
                RaceOp::UserBegin => {
                    self.pointer_down = true;
                    cancel_programmatic_resize_for_user_gesture(&self.window);
                    self.window
                        .user_resize_active
                        .store(true, Ordering::Relaxed);
                    self.window
                        .user_resize_active_since_ms
                        .store(now_ms(), Ordering::Relaxed);
                    self.window
                        .user_resize_until_ms
                        .store(now_ms().saturating_add(USER_RESIZE_TTL), Ordering::Relaxed);
                }
                RaceOp::UserTick => self.deliver_resize_event(RACE_USER_W, RACE_USER_H),
                RaceOp::UserFinalize => {
                    // Mirrors `compositor_resize_window`'s finalize branch: it
                    // clears the TTL before the active bit, then drains the
                    // latch through `resize_source_preserving_user_size`, which
                    // KEEPS the user's width and only re-derives the other
                    // dimension from the new source aspect.
                    //
                    // Pointer-up FIRST: the latch drain below legitimately
                    // moves the panel, and it happens after the real mouseup.
                    self.pointer_down = false;
                    self.window.user_resize_until_ms.store(0, Ordering::Relaxed);
                    self.window
                        .user_resize_active
                        .store(false, Ordering::Relaxed);
                    let latched = self.window.pending_source_resize.lock_unpoisoned().take();
                    if latched.is_some() {
                        let current = self.panel();
                        let source = *self.window.source_presentation_size.lock_unpoisoned();
                        let aspect = source
                            .map(|(w, h)| w / h.max(1.0))
                            .unwrap_or_else(race_source_aspect);
                        let desired =
                            proportional_content_size_for_source_change(current, aspect, None);
                        self.applied =
                            prepare_programmatic_resize_request(&self.window, desired.0, desired.1);
                        self.step(RaceOp::SourceCallback);
                    }
                }
                RaceOp::RetireReveal => {
                    // Mirrors `ensure_window`'s retired-reuse branch, field for
                    // field. The panel object persists; these are the resets
                    // that branch applies before `show_retired_window_on_main`.
                    apply_retired_reuse_reveal_state(
                        &mut self.window.revealed_first_frame,
                        self.window.layer_has_content,
                    );
                    self.window.ai_chat_overlay_open = false;
                    *self.window.source_pixel_size.lock_unpoisoned() = None;
                    *self.window.canonical_source_pixel_size.lock_unpoisoned() =
                        Some((RACE_SOURCE_W as u32, RACE_SOURCE_H as u32));
                    *self.window.source_presentation_size.lock_unpoisoned() = None;
                    *self.window.pending_source_resize.lock_unpoisoned() = None;
                    reset_programmatic_resize_events(&self.window);
                    // A retired panel has no live request ownership, so any
                    // in-flight programmatic transaction is abandoned too.
                    self.applied = None;
                    self.decision = None;
                    carry_resize_gesture_across_reveal(&self.window);
                }
                RaceOp::SourceDecide => {
                    // Mirrors `update_canonical_source_size_on_republish` /
                    // `push_frame`: the decision, the commit of the new
                    // canonical/presentation size, and the Latch branch all
                    // happen in ONE `with_state` section. Only the apply is
                    // deferred to a later section -- which is the TOCTOU.
                    let previous = *self.window.source_presentation_size.lock_unpoisoned();
                    let decision = resize_decision(
                        previous,
                        Some((RACE_SOURCE_W, RACE_SOURCE_H)),
                        resize_gesture_in_progress(&self.window),
                    );
                    *self.window.source_presentation_size.lock_unpoisoned() =
                        Some((RACE_SOURCE_W, RACE_SOURCE_H));
                    *self.window.canonical_source_pixel_size.lock_unpoisoned() =
                        Some((RACE_SOURCE_W as u32, RACE_SOURCE_H as u32));
                    if decision == ResizeDecision::Latch {
                        *self.window.pending_source_resize.lock_unpoisoned() =
                            Some(race_source_target());
                    }
                    self.decision = Some(decision);
                }
                RaceOp::SourceApply => {
                    // Mirrors `resize_to_content_on_main`'s critical section:
                    // create the transaction, or latch the target if a gesture
                    // began after the decision was taken.
                    if self.decision == Some(ResizeDecision::Apply) {
                        let transaction = prepare_programmatic_resize_request(
                            &self.window,
                            RACE_SOURCE_W,
                            RACE_SOURCE_H,
                        );
                        if transaction.is_none() && resize_gesture_in_progress(&self.window) {
                            *self.window.pending_source_resize.lock_unpoisoned() =
                                Some(race_source_target());
                        }
                        self.applied = transaction;
                    }
                }
                RaceOp::SourceCallback => {
                    // AppKit acknowledges the geometry that was actually
                    // requested, whatever it was.
                    if let Some(transaction) = self.applied {
                        self.deliver_resize_event(
                            transaction.content_width,
                            transaction.content_height,
                        );
                    }
                }
            }
        }
    }

    /// Every order-preserving merge of two operation sequences.
    fn race_interleavings(a: &[RaceOp], b: &[RaceOp]) -> Vec<Vec<RaceOp>> {
        if a.is_empty() {
            return vec![b.to_vec()];
        }
        if b.is_empty() {
            return vec![a.to_vec()];
        }
        let mut out = Vec::new();
        for mut tail in race_interleavings(&a[1..], b) {
            tail.insert(0, a[0]);
            out.push(tail);
        }
        for mut tail in race_interleavings(a, &b[1..]) {
            tail.insert(0, b[0]);
            out.push(tail);
        }
        out
    }

    fn race_user_ops() -> [RaceOp; 4] {
        [
            RaceOp::UserBegin,
            RaceOp::UserTick,
            RaceOp::UserTick,
            RaceOp::UserFinalize,
        ]
    }

    fn race_source_ops() -> [RaceOp; 3] {
        [
            RaceOp::SourceDecide,
            RaceOp::SourceApply,
            RaceOp::SourceCallback,
        ]
    }

    /// The republish sequence a real sender produces: the track drops, the
    /// window is retired and revealed from the reuse pool, and only THEN do
    /// fresh frames drive the source-resize policy.
    fn race_republish_ops() -> [RaceOp; 4] {
        [
            RaceOp::RetireReveal,
            RaceOp::SourceDecide,
            RaceOp::SourceApply,
            RaceOp::SourceCallback,
        ]
    }

    /// #416's actual defect, and the gap the 35-interleaving harness could not
    /// see: it held ONE `CompositorWindow` and never retired or revealed it.
    ///
    /// Live traces showed `programmatic-source-driven` writes landing with
    /// `gesture=idle` while drag IPCs were still arriving -- the reveal had
    /// cleared the very bit the guard consults. Interleaving the republish
    /// lifecycle against a live drag reproduces that here: without
    /// `carry_resize_gesture_across_reveal`, every order that reveals between
    /// pointer-down and pointer-up moves the panel off the user's width.
    #[test]
    fn resize_race_reveal_from_reuse_pool_never_resizes_panel_mid_drag() {
        let orders = race_interleavings(&race_user_ops(), &race_republish_ops());
        assert_eq!(orders.len(), 70, "interleaving enumeration changed");

        let mut failures = Vec::new();
        for order in &orders {
            let mut harness = RaceHarness::new();
            for op in order {
                harness.step(*op);
            }
            if !harness.violations.is_empty() {
                failures.push((order.clone(), harness.violations.clone()));
            }
        }

        assert!(
            failures.is_empty(),
            "{}/{} retire->reveal interleavings moved the panel off the user's geometry \
             while the pointer was still PHYSICALLY down (#416). A republish must not let \
             the reveal forget an in-flight gesture. First: {:?}",
            failures.len(),
            orders.len(),
            failures.first(),
        );
    }

    /// The companion bound: carrying gesture state across a reveal must not
    /// turn into "a revealed window never resizes again". Every republish
    /// interleaving must still land on the SOURCE aspect once the pointer is
    /// up -- a latched resize has to survive the lifecycle hop too.
    #[test]
    fn resize_race_reveal_from_reuse_pool_still_ends_at_source_aspect() {
        let orders = race_interleavings(&race_user_ops(), &race_republish_ops());

        let mut wrong = Vec::new();
        for order in &orders {
            let mut harness = RaceHarness::new();
            for op in order {
                harness.step(*op);
            }
            let (width, height) = harness.panel();
            let aspect = width / height.max(1.0);
            if (aspect - race_source_aspect()).abs() > 0.01 {
                wrong.push((order.clone(), width, height, aspect));
            }
        }

        assert!(
            wrong.is_empty(),
            "{}/{} retire->reveal interleavings left the panel at the WRONG ASPECT -- \
             carrying the gesture across a reveal must defer a genuine source resize, \
             not discard it (#416). Expected {:.4}. First: {:?}",
            wrong.len(),
            orders.len(),
            race_source_aspect(),
            wrong.first(),
        );
    }

    /// The backstop must survive the new carry path. A reveal may keep a LIVE
    /// gesture, but must never resurrect or extend a stale one -- a window
    /// that revealed with a permanently stuck `user_resize_active` would be a
    /// worse bug than the one being fixed.
    #[test]
    fn reveal_carries_a_live_gesture_but_expires_a_stale_one() {
        let live = test_window("owner", "title");
        live.user_resize_active.store(true, Ordering::Relaxed);
        live.user_resize_active_since_ms
            .store(now_ms(), Ordering::Relaxed);
        assert!(
            carry_resize_gesture_across_reveal(&live),
            "a pointer that is still down must survive retire->reveal"
        );
        assert!(resize_gesture_in_progress(&live));

        // Same reveal, but the gesture began longer ago than any real drag --
        // a lost finalize IPC. MAX_USER_RESIZE_GESTURE_MS must still retire it.
        let stale = test_window("owner", "title");
        stale.user_resize_active.store(true, Ordering::Relaxed);
        stale.user_resize_active_since_ms.store(
            now_ms().saturating_sub(MAX_USER_RESIZE_GESTURE_MS + 1),
            Ordering::Relaxed,
        );
        assert!(
            !carry_resize_gesture_across_reveal(&stale),
            "a stale gesture must NOT be carried across a reveal"
        );
        assert!(!stale.user_resize_active.load(Ordering::Relaxed));
        assert!(
            prepare_programmatic_resize_request(&stale, 800.0, 500.0).is_some(),
            "source reconciliation must be unblocked once the stale gesture expires"
        );

        // A reveal must not postpone expiry by refreshing the backstop clock.
        let nearly_stale = test_window("owner", "title");
        let began = now_ms().saturating_sub(MAX_USER_RESIZE_GESTURE_MS - 500);
        nearly_stale
            .user_resize_active
            .store(true, Ordering::Relaxed);
        nearly_stale
            .user_resize_active_since_ms
            .store(began, Ordering::Relaxed);
        assert!(carry_resize_gesture_across_reveal(&nearly_stale));
        assert_eq!(
            nearly_stale
                .user_resize_active_since_ms
                .load(Ordering::Relaxed),
            began,
            "reveal must leave the backstop clock untouched, or a republish loop \
             could keep a lost gesture alive forever"
        );
    }

    #[test]
    fn resize_race_source_resize_never_resizes_panel_mid_drag() {
        let orders = race_interleavings(&race_user_ops(), &race_source_ops());
        assert_eq!(orders.len(), 35, "interleaving enumeration changed");

        let mut failures = Vec::new();
        for order in &orders {
            let mut harness = RaceHarness::new();
            for op in order {
                harness.step(*op);
            }
            if !harness.violations.is_empty() {
                failures.push((order.clone(), harness.violations.clone()));
            }
        }

        assert!(
            failures.is_empty(),
            "{}/{} interleavings moved the panel off the user's geometry while the \
             user's pointer was still down (#416). First: {:?}",
            failures.len(),
            orders.len(),
            failures.first(),
        );
    }

    /// The companion to the race test, and the reason the fix is not just
    /// "never resize". #416 warns explicitly that suppressing the mid-drag snap
    /// must not silently break legitimate sender-driven resizes. In EVERY
    /// interleaving the panel must END at the SOURCE's aspect ratio -- that is
    /// the "border gaps" half of the issue. A fix that simply dropped source
    /// resizes would leave the panel at the stale 1.6 aspect and fail here.
    #[test]
    fn resize_race_panel_always_ends_at_source_aspect() {
        let orders = race_interleavings(&race_user_ops(), &race_source_ops());

        let mut wrong = Vec::new();
        for order in &orders {
            let mut harness = RaceHarness::new();
            for op in order {
                harness.step(*op);
            }
            let (width, height) = harness.panel();
            let aspect = width / height.max(1.0);
            if (aspect - race_source_aspect()).abs() > 0.01 {
                wrong.push((order.clone(), width, height, aspect));
            }
        }

        assert!(
            wrong.is_empty(),
            "{}/{} interleavings left the panel at the WRONG ASPECT -- a genuine source \
             resize was discarded rather than deferred (#416). Expected {:.4}. First: {:?}",
            wrong.len(),
            orders.len(),
            race_source_aspect(),
            wrong.first(),
        );
    }

    #[test]
    fn resize_command_listener_adapter_settles_placeholder_geometry_within_one_pixel() {
        // Drive the same command-side adapter used by resize_to_content_on_main
        // and the same listener classifier used by WindowEvent::Resized. The
        // old chain derived 323x202 from this 640x400 placeholder.
        let window = test_window("owner", "title");
        *window.panel_content_size.lock_unpoisoned() = (640.0, 400.0);
        *window.source_presentation_size.lock_unpoisoned() = Some((323.0, 415.0));

        let transaction = prepare_programmatic_resize_request(&window, 323.0, 415.0)
            .expect("expanded command request");
        let listener = classify_programmatic_resize_event(&window, 323.49, 414.51, 2.0);
        assert_eq!(
            listener,
            ResizeListenerDisposition::SettleProgrammatic(transaction)
        );
        assert!(settled_geometry_within_one_physical_pixel(
            transaction.content_width,
            transaction.content_height,
            323.49,
            414.51,
            2.0,
        ));
        assert!(!settled_geometry_within_one_physical_pixel(
            transaction.content_width,
            transaction.content_height,
            323.51,
            414.49,
            2.0,
        ));
    }

    #[test]
    fn resize_listener_rejects_old_same_geometry_ack_before_settling_latest_generation() {
        let window = test_window("owner", "title");
        let first = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();
        let latest = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();
        assert!(latest.generation > first.generation);

        // T1(A) then T2(A): matching geometry alone cannot identify the
        // callback. The FIFO acknowledgement barrier consumes cancelled T1
        // first, then permits the next A event to settle T2.
        assert_eq!(
            classify_programmatic_resize_event(&window, 323.0, 415.0, 1.0),
            ResizeListenerDisposition::IgnoreStaleProgrammatic
        );
        assert_eq!(
            classify_programmatic_resize_event(&window, 323.0, 415.0, 1.0),
            ResizeListenerDisposition::SettleProgrammatic(latest)
        );
    }

    #[test]
    fn resize_listener_consumes_out_of_order_cancelled_acknowledgements() {
        let window = test_window("owner", "title");
        let first = prepare_programmatic_resize_request(&window, 200.0, 300.0).unwrap();
        let second = prepare_programmatic_resize_request(&window, 250.0, 350.0).unwrap();
        let latest = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();

        // AppKit may deliver the second cancelled callback before the first;
        // match the bounded active set rather than assuming FIFO delivery.
        assert_eq!(
            classify_programmatic_resize_event(
                &window,
                second.content_width,
                second.content_height,
                1.0
            ),
            ResizeListenerDisposition::IgnoreStaleProgrammatic,
        );
        assert_eq!(
            window
                .programmatic_resize_events
                .lock_unpoisoned()
                .cancelled_callbacks
                .front()
                .expect("first cancellation remains")
                .transaction,
            first,
        );
        assert_eq!(
            classify_programmatic_resize_event(
                &window,
                first.content_width,
                first.content_height,
                1.0
            ),
            ResizeListenerDisposition::IgnoreStaleProgrammatic,
        );
        // The second stale callback was already consumed out of order; a
        // duplicate delivery must not be mistaken for the current request.
        assert_eq!(
            classify_programmatic_resize_event(
                &window,
                second.content_width,
                second.content_height,
                1.0
            ),
            ResizeListenerDisposition::UserResize,
        );
        assert_eq!(
            classify_programmatic_resize_event(
                &window,
                latest.content_width,
                latest.content_height,
                1.0
            ),
            ResizeListenerDisposition::SettleProgrammatic(latest),
        );
    }

    #[test]
    fn real_listener_adapter_settles_newer_resize_after_missing_old_ack_deadline() {
        let window = test_window("owner", "title");
        let old = prepare_programmatic_resize_request(&window, 200.0, 300.0).unwrap();
        let latest = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();
        let deadline = window
            .programmatic_resize_events
            .lock_unpoisoned()
            .cancelled_callbacks
            .front()
            .expect("old callback is the FIFO head")
            .barrier_deadline;

        // AppKit coalesced/dropped the old callback. The actual listener path
        // may settle the newer transaction after the bounded barrier instead
        // of treating it as a user resize or blocking forever.
        assert_eq!(
            classify_resize_listener_event_at(
                &window,
                latest.content_width,
                latest.content_height,
                1.0,
                true,
                deadline + Duration::from_millis(1),
            ),
            ResizeListenerDisposition::SettleProgrammatic(latest),
        );
        // The expired head is pruned rather than poisoning the FIFO forever.
        assert_eq!(
            classify_resize_listener_event_at(
                &window,
                old.content_width,
                old.content_height,
                1.0,
                true,
                deadline + Duration::from_millis(2),
            ),
            ResizeListenerDisposition::UserResize,
        );
    }

    #[test]
    fn cancelled_resize_fifo_is_bounded_when_callbacks_are_dropped() {
        let window = test_window("owner", "title");
        for index in 0..(MAX_CANCELLED_PROGRAMMATIC_RESIZES + 4) {
            let _ = prepare_programmatic_resize_request(
                &window,
                200.0 + index as f64,
                300.0 + index as f64,
            );
        }
        let events = window.programmatic_resize_events.lock_unpoisoned();
        assert!(events.cancelled_callbacks.len() <= MAX_CANCELLED_PROGRAMMATIC_RESIZES);
    }

    #[test]
    fn dropped_cancelled_resize_does_not_block_newer_b_and_c() {
        let window = test_window("owner", "title");
        let first = prepare_programmatic_resize_request(&window, 200.0, 300.0).unwrap();
        let second = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();
        let third = prepare_programmatic_resize_request(&window, 640.0, 360.0).unwrap();
        let deadline = window
            .programmatic_resize_events
            .lock_unpoisoned()
            .cancelled_callbacks
            .front()
            .expect("cancelled A is queued")
            .barrier_deadline;

        assert_eq!(
            classify_resize_listener_event_at(
                &window,
                third.content_width,
                third.content_height,
                1.0,
                true,
                deadline + Duration::from_millis(1),
            ),
            ResizeListenerDisposition::SettleProgrammatic(third),
        );
        let events = window.programmatic_resize_events.lock_unpoisoned();
        assert!(events.pending.is_none());
        assert!(events.cancelled_callbacks.is_empty());
        drop(events);
        assert_eq!(
            classify_resize_listener_event_at(
                &window,
                first.content_width,
                first.content_height,
                1.0,
                true,
                deadline + Duration::from_millis(2),
            ),
            ResizeListenerDisposition::UserResize,
        );
        assert_eq!(second.generation + 1, third.generation);
    }

    #[test]
    fn stale_republish_canonical_callback_cannot_overwrite_newer_generation() {
        assert!(!canonical_source_update_is_current(7, 12, 7, 11));
        assert!(canonical_source_update_is_current(7, 12, 7, 12));
        assert!(!canonical_source_update_is_current(8, 1, 7, 12));
    }

    #[test]
    fn real_listener_adapter_buffers_pre_deadline_newer_ack_then_reconciles_without_another_event()
    {
        let window = test_window("owner", "title");
        let old = prepare_programmatic_resize_request(&window, 200.0, 300.0).unwrap();
        let latest = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();
        let deadline = window
            .programmatic_resize_events
            .lock_unpoisoned()
            .cancelled_callbacks
            .front()
            .expect("old callback is the FIFO head")
            .barrier_deadline;

        // T2 arrives before the cancelled T1 barrier expires. It must not be
        // sent through user aspect correction or lost while waiting for a
        // second native event that may never come.
        assert_eq!(
            classify_resize_listener_event_at(
                &window,
                latest.content_width,
                latest.content_height,
                1.0,
                true,
                deadline - Duration::from_millis(1),
            ),
            ResizeListenerDisposition::BufferProgrammatic(latest),
        );
        assert_eq!(
            reconcile_pending_programmatic_resize_at(
                &window,
                latest.generation,
                latest.content_width,
                latest.content_height,
                deadline + Duration::from_millis(1),
            ),
            Some(latest),
        );
        // The expired T1 expectation is pruned; it must not poison later
        // callbacks after the newer generation has settled.
        assert_eq!(
            classify_resize_listener_event_at(
                &window,
                old.content_width,
                old.content_height,
                1.0,
                true,
                deadline + Duration::from_millis(2),
            ),
            ResizeListenerDisposition::UserResize,
        );
    }

    #[test]
    fn real_listener_adapter_ignores_late_successful_ack_after_native_reconciliation() {
        let window = test_window("owner", "title");
        let transaction = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();
        assert_eq!(
            settle_programmatic_resize_if_current(&window, transaction.generation),
            Some(transaction),
        );
        // Mirrors resize_to_content_on_main's successful native-size query.
        retain_late_successful_resize_ack(&window, 323.0, 415.0);

        assert_eq!(
            classify_resize_listener_event_at(
                &window,
                323.0,
                415.0,
                1.0,
                true,
                Instant::now() + Duration::from_secs(60),
            ),
            ResizeListenerDisposition::IgnoreStaleProgrammatic,
        );
        assert_eq!(
            classify_resize_listener_event(&window, 700.0, 300.0, 1.0, true),
            ResizeListenerDisposition::UserResize,
        );
    }

    #[test]
    fn quiescent_user_drag_clears_late_success_ack_before_aspect_lock() {
        let window = test_window("owner", "title");
        let transaction = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();
        assert_eq!(
            settle_programmatic_resize_if_current(&window, transaction.generation),
            Some(transaction),
        );
        retain_late_successful_resize_ack(&window, 323.0, 415.0);

        // Mirrors compositor_begin_resize: the next genuine gesture owns the
        // listener and clears any unconsumed reconciliation acknowledgement.
        cancel_programmatic_resize(&window);
        assert_eq!(
            classify_resize_listener_event(&window, 700.0, 300.0, 1.0, true),
            ResizeListenerDisposition::UserResize,
        );
    }

    #[test]
    fn scale_factor_change_bypasses_programmatic_acknowledgement_matching() {
        let window = test_window("owner", "title");
        let transaction = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();

        assert_eq!(
            classify_resize_listener_event(&window, 323.0, 415.0, 2.0, false),
            ResizeListenerDisposition::UserResize,
        );
        assert_eq!(
            classify_resize_listener_event(&window, 323.0, 415.0, 2.0, true),
            ResizeListenerDisposition::SettleProgrammatic(transaction),
        );
    }

    #[test]
    fn cancelled_programmatic_callback_is_not_reinterpreted_as_a_user_resize() {
        let window = test_window("owner", "title");
        *window.panel_content_size.lock_unpoisoned() = (640.0, 400.0);
        *window.source_presentation_size.lock_unpoisoned() = Some((323.0, 415.0));
        prepare_programmatic_resize_request(&window, 323.0, 415.0);
        cancel_programmatic_resize(&window);

        assert_eq!(
            classify_programmatic_resize_event(&window, 323.0, 415.0, 1.0),
            ResizeListenerDisposition::IgnoreStaleProgrammatic
        );
        // Only a genuinely new event reaches aspect lock after cancellation.
        let aspect = source_aspect_for_resize_event(&window, 700.0, 300.0);
        let (height, corrected) = aspect_locked_content_height(700.0, 300.0, aspect);
        assert!(corrected);
        assert!((height - (700.0 / (323.0 / 415.0))).abs() < f64::EPSILON);
    }

    #[test]
    fn failed_or_no_callback_command_never_publishes_desired_geometry_as_settled() {
        let window = test_window("owner", "title");
        *window.panel_content_size.lock_unpoisoned() = (640.0, 400.0);
        let failed = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();
        // Mirrors a synchronous `set_size` error: discard only the active
        // request and preserve the last known native/panel geometry.
        discard_programmatic_resize_if_current(&window, failed.generation);
        assert_eq!(*window.panel_content_size.lock_unpoisoned(), (640.0, 400.0));

        let no_callback = prepare_programmatic_resize_request(&window, 323.0, 415.0).unwrap();
        // Mirrors successful set_size with no listener callback. The caller
        // records the actual queried native geometry, never the desired one.
        assert_eq!(
            settle_programmatic_resize_if_current(&window, no_callback.generation),
            Some(no_callback)
        );
        assert!(record_settled_panel_content_geometry(
            &window, 640.0, 400.0, 2.0
        ));
        assert_eq!(*window.panel_content_size.lock_unpoisoned(), (640.0, 400.0));
        assert_eq!(
            classify_programmatic_resize_event(&window, 323.0, 415.0, 1.0),
            ResizeListenerDisposition::UserResize,
        );
    }

    #[test]
    fn command_adapter_rejects_old_programmatic_callbacks_after_cancel() {
        let window = test_window("owner", "title");
        prepare_programmatic_resize_request(&window, 323.0, 415.0);
        cancel_programmatic_resize(&window);
        assert_eq!(
            classify_programmatic_resize_event(&window, 323.0, 415.0, 1.0),
            ResizeListenerDisposition::IgnoreStaleProgrammatic
        );
    }

    #[test]
    fn retire_or_reuse_resets_all_programmatic_acknowledgement_state() {
        let window = test_window("owner", "title");
        prepare_programmatic_resize_request(&window, 200.0, 300.0);
        prepare_programmatic_resize_request(&window, 323.0, 415.0);
        reset_programmatic_resize_events(&window);

        let events = window.programmatic_resize_events.lock_unpoisoned();
        assert!(events.pending.is_none());
        assert!(events.cancelled_callbacks.is_empty());
        drop(events);
        assert_eq!(
            prepare_programmatic_resize_request(&window, 400.0, 500.0)
                .expect("new share request")
                .generation,
            1,
        );
    }

    #[test]
    fn ensure_window_creation_watchdog_distinguishes_open_stall_and_publication_churn() {
        assert_eq!(
            ensure_window_creation_watchdog_decision(
                ENSURE_WINDOW_CREATION_WATCHDOG_TIMEOUT - Duration::from_millis(1),
                false,
                EnsureWindowCreationBranch::Created,
                false,
            ),
            EnsureWindowCreationWatchdogDecision::KeepWaiting
        );
        assert_eq!(
            ensure_window_creation_watchdog_decision(
                ENSURE_WINDOW_CREATION_WATCHDOG_TIMEOUT + Duration::from_millis(1),
                true,
                EnsureWindowCreationBranch::Created,
                false,
            ),
            EnsureWindowCreationWatchdogDecision::KeepWaiting
        );
        assert_eq!(
            ensure_window_creation_watchdog_decision(
                ENSURE_WINDOW_CREATION_WATCHDOG_TIMEOUT + Duration::from_millis(1),
                false,
                EnsureWindowCreationBranch::Created,
                false,
            ),
            EnsureWindowCreationWatchdogDecision::LogStall
        );
        for branch in [
            EnsureWindowCreationBranch::Created,
            EnsureWindowCreationBranch::ReusedFromPool,
            EnsureWindowCreationBranch::AlreadyOpen,
        ] {
            assert_eq!(
                ensure_window_creation_watchdog_decision(
                    ENSURE_WINDOW_CREATION_WATCHDOG_TIMEOUT + Duration::from_millis(1),
                    false,
                    branch,
                    true,
                ),
                EnsureWindowCreationWatchdogDecision::LogPublicationChurn,
                "retired branch {branch:?} is publication churn, not an AppKit stall"
            );
        }
        // The case the whole distinction exists to PRESERVE: the main-thread
        // closure never reached any branch, so nothing was ever built. That is
        // a real AppKit/main-thread stall and must still be reported as one --
        // rewording the churn case must not silence the genuine fault.
        assert_eq!(
            ensure_window_creation_watchdog_decision(
                ENSURE_WINDOW_CREATION_WATCHDOG_TIMEOUT + Duration::from_millis(1),
                false,
                EnsureWindowCreationBranch::Pending,
                false,
            ),
            EnsureWindowCreationWatchdogDecision::LogStall
        );
        // ... and a reuse that is NOT retired is still churn, not a stall:
        // nothing about a reused window implies the main thread is wedged.
        assert_eq!(
            ensure_window_creation_watchdog_decision(
                ENSURE_WINDOW_CREATION_WATCHDOG_TIMEOUT + Duration::from_millis(1),
                false,
                EnsureWindowCreationBranch::ReusedFromPool,
                false,
            ),
            EnsureWindowCreationWatchdogDecision::LogPublicationChurn
        );
    }

    fn assert_nearest_for_content_size(
        source_width_px: u32,
        source_height_px: u32,
        content_w: f64,
        content_h: f64,
        receiver_scale: f64,
    ) {
        assert_eq!(
            display_filter_for_geometry(
                source_width_px,
                source_height_px,
                content_w,
                content_h,
                receiver_scale,
            ),
            DisplayLayerFilter::Nearest
        );
    }

    #[derive(serde::Deserialize)]
    struct ContractFixture {
        #[serde(rename = "identityPalette")]
        identity_palette: IdentityPaletteFixture,
    }

    #[derive(serde::Deserialize)]
    struct IdentityPaletteFixture {
        hash: String,
        names: Vec<String>,
        hex: Vec<String>,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!("../../../../contracts/petal-contracts.json")).unwrap()
    }

    #[test]
    fn first_window_sits_exactly_at_desktop_origin() {
        assert_eq!(cascade_position((0.0, 0.0), 0), (0.0, 0.0));
    }

    #[test]
    fn successive_windows_step_down_and_right() {
        let a = cascade_position((100.0, 50.0), 0);
        let b = cascade_position((100.0, 50.0), 1);
        let c = cascade_position((100.0, 50.0), 2);
        assert_eq!(a, (100.0, 50.0));
        assert_eq!(b, (100.0 + CASCADE_STEP, 50.0 + CASCADE_STEP));
        assert_eq!(c, (100.0 + 2.0 * CASCADE_STEP, 50.0 + 2.0 * CASCADE_STEP));
    }

    #[test]
    fn cascade_wraps_back_toward_origin_instead_of_marching_off_screen() {
        let wrapped = cascade_position((0.0, 0.0), CASCADE_WRAP);
        assert_eq!(
            wrapped,
            (0.0, 0.0),
            "slot == CASCADE_WRAP should wrap back to origin"
        );
        let just_before_wrap = cascade_position((0.0, 0.0), CASCADE_WRAP - 1);
        assert_eq!(
            just_before_wrap,
            (
                (CASCADE_WRAP - 1) as f64 * CASCADE_STEP,
                (CASCADE_WRAP - 1) as f64 * CASCADE_STEP
            )
        );
    }

    #[test]
    fn control_overlay_stays_interactive_so_resize_handles_keep_working() {
        assert!(!control_overlay_ignore_cursor_events(true, false));
        assert!(!control_overlay_ignore_cursor_events(false, true));
        assert!(!control_overlay_ignore_cursor_events(true, true));
        assert!(!control_overlay_ignore_cursor_events(false, false));
    }

    #[test]
    fn initial_size_keeps_large_remote_display_within_eighty_percent_work_area() {
        let (w, h) = initial_content_size_within_work_area(
            3840, 2160, // source pixels
            2.0, 2.0, // source scale, receiver scale
            1440.0, 900.0,
        );

        assert_eq!(w, 1152.0);
        assert_eq!(h, 648.0);
        assert!(w <= 1440.0 * INITIAL_MAX_WORK_AREA_FRACTION);
        assert!(HEADER_HEIGHT + h <= 900.0 * INITIAL_MAX_WORK_AREA_FRACTION);
        assert!((w / h - 3840.0 / 2160.0).abs() < 0.001);
    }

    #[test]
    fn initial_size_uses_largest_nearest_integer_scale_that_fits() {
        let (w, h) = initial_content_size_within_work_area(
            800, 500, // source pixels
            2.0, 2.0, // 400x250 source points, crisp 2x display
            3000.0, 2000.0,
        );

        assert_eq!((w, h), (800.0, 500.0));
        assert_nearest_for_content_size(800, 500, w, h, 2.0);
    }

    #[test]
    fn initial_size_falls_back_to_fractional_fit_when_one_x_cannot_fit() {
        let (w, h) = initial_content_size_within_work_area(
            2400, 1600, // source pixels
            2.0, 2.0, 900.0, 700.0,
        );

        assert_eq!((w, h), (720.0, 480.0));
        assert_eq!(
            display_filter_for_geometry(2400, 1600, w, h, 2.0),
            DisplayLayerFilter::Linear
        );
    }

    #[test]
    fn source_presentation_size_preserves_downscaled_capture_scale() {
        let (w, h) = source_presentation_size_points(
            1920, 1080, // P1080 capture of a 3000pt-wide source
            0.64,
        );

        assert_eq!((w, h), (3000.0, 1688.0));
    }

    #[test]
    fn proportional_source_resize_preserves_width_and_clamps_to_work_area() {
        assert_eq!(
            proportional_content_size_for_source_change((800.0, 450.0), 1.0, Some((900.0, 700.0))),
            (656.0, 656.0)
        );
        assert_eq!(
            proportional_content_size_for_source_change(
                (1200.0, 675.0),
                16.0 / 9.0,
                Some((1000.0, 800.0))
            ),
            (1000.0, 563.0)
        );

        let fractional = proportional_content_size_for_source_change(
            (1756.0, 1134.0),
            2560.0 / 1654.0,
            Some((1800.0, 1400.0)),
        );
        assert_eq!(fractional.0.fract(), 0.0);
        assert_eq!(fractional.1.fract(), 0.0);

        let floored =
            proportional_content_size_for_source_change((1.0, 1.0), 100.0, Some((10.0, 10.0)));
        assert!(floored.0 >= MIN_RESIZE_CONTENT_WIDTH);
        assert!(floored.1 >= MIN_RESIZE_CONTENT_HEIGHT);
    }

    #[test]
    fn initial_size_accounts_for_header_in_total_window_height() {
        let (w, h) = initial_content_size_within_work_area(
            1200, 1200, // source pixels
            1.0, 1.0, 2000.0, 1000.0,
        );

        // Squeezed by the header budget (HEADER_HEIGHT), not just the raw
        // work-area fraction -- these values shift if HEADER_HEIGHT changes
        // (#110 fixed a stale value here); derive, don't re-hardcode blindly.
        let expected_factor = (1000.0 * INITIAL_MAX_WORK_AREA_FRACTION - HEADER_HEIGHT) / 1200.0;
        let expected = (1200.0 * expected_factor).round();
        assert_eq!(w, expected);
        assert_eq!(h, expected);
        // +0.5 tolerance: `.round()` can push the exact-budget case up to half
        // a point over (e.g. content landing on X.5 rounds away from zero) --
        // that's expected rounding slack, not a header-budget violation.
        assert!(HEADER_HEIGHT + h <= 1000.0 * INITIAL_MAX_WORK_AREA_FRACTION + 0.5);
    }

    #[test]
    fn fit_to_source_uses_receiver_scale_for_one_x_target() {
        let (w, h) = fit_to_source_content_size_within_work_area(
            1200, 800, // source pixels
            1.0, 2.0, // sender captured at 1x, receiver is Retina
            800.0, 700.0,
        );

        assert_eq!((w, h), (600.0, 400.0));
        assert_nearest_for_content_size(1200, 800, w, h, 2.0);
    }

    #[test]
    fn fit_to_source_uses_two_x_target_when_it_fits() {
        let (w, h) = fit_to_source_content_size_within_work_area(
            800, 500, // source pixels
            2.0, 2.0, 1000.0, 800.0,
        );

        assert_eq!((w, h), (800.0, 500.0));
        assert_nearest_for_content_size(800, 500, w, h, 2.0);
    }

    #[test]
    fn panel_header_pointer_labels_are_stable_and_distinct_per_owner_window() {
        let alice = test_key("alice", 7);
        let bob = test_key("bob", 7);

        assert_eq!(
            panel_label_for_key(&alice),
            format!("remote-window-{}", alice.label_segment())
        );
        assert_eq!(
            header_label_for_key(&alice),
            format!("remote-window-header-{}", alice.label_segment())
        );
        assert_eq!(
            control_label_for_key(&alice),
            format!("remote-window-control-{}", alice.label_segment())
        );
        assert_eq!(
            pointer_label_for_key(&alice),
            format!("remote-window-pointer-{}", alice.label_segment())
        );
        assert_ne!(panel_label_for_key(&alice), panel_label_for_key(&bob));
        assert_ne!(control_label_for_key(&alice), control_label_for_key(&bob));
    }

    #[test]
    fn compositor_state_allows_two_owners_with_same_numeric_window_id() {
        let window_id = 0x7fff_0096;
        let alice = test_key("alice-owner-96", window_id);
        let bob = test_key("bob-owner-96", window_id);

        with_state(|s| {
            s.windows.remove(&alice);
            s.windows.remove(&bob);
            s.retired.remove(&alice);
            s.retired.remove(&bob);
            s.windows.insert(
                alice.clone(),
                test_window(&alice.owner_identity, "Alice Terminal"),
            );
            s.windows.insert(
                bob.clone(),
                test_window(&bob.owner_identity, "Bob Terminal"),
            );
        });

        assert_eq!(owner_identity_for_window(window_id, None), None);
        assert_eq!(
            owner_identity_for_window(window_id, Some(&alice.owner_identity)),
            Some(alice.owner_identity.clone())
        );
        assert_eq!(
            owner_identity_for_window(window_id, Some(&bob.owner_identity)),
            Some(bob.owner_identity.clone())
        );
        assert!(is_open_for_owner(&alice.owner_identity, window_id));
        assert!(is_open_for_owner(&bob.owner_identity, window_id));

        with_state(|s| {
            s.windows.remove(&alice);
            s.windows.remove(&bob);
        });
    }

    /// #875: `update_window_z_rank` is storage only, scoped by
    /// `(owner_identity, window_id)` -- same collision class as #678 above.
    /// Two participants sharing the SAME numeric window id must be able to
    /// carry independent ranks without clobbering each other, and a missing
    /// key (window not open for that owner) must be a silent no-op rather
    /// than panicking or inserting a phantom entry.
    #[test]
    fn update_window_z_rank_is_scoped_by_owner_and_window_id() {
        let window_id = 0x8750_0001;
        let alice = test_key("alice-owner-875", window_id);
        let bob = test_key("bob-owner-875", window_id);

        with_state(|s| {
            s.windows.remove(&alice);
            s.windows.remove(&bob);
            s.windows.insert(
                alice.clone(),
                test_window(&alice.owner_identity, "Alice Terminal"),
            );
            s.windows.insert(
                bob.clone(),
                test_window(&bob.owner_identity, "Bob Terminal"),
            );
        });

        assert_eq!(
            window_z_rank_for_test(&alice.owner_identity, window_id),
            None,
            "a freshly-opened window starts with no rank"
        );

        update_window_z_rank(&alice.owner_identity, window_id, Some(0));
        assert_eq!(
            window_z_rank_for_test(&alice.owner_identity, window_id),
            Some(0)
        );
        assert_eq!(
            window_z_rank_for_test(&bob.owner_identity, window_id),
            None,
            "bob's identically-numbered window must not see alice's rank"
        );

        update_window_z_rank(&bob.owner_identity, window_id, Some(2));
        assert_eq!(
            window_z_rank_for_test(&alice.owner_identity, window_id),
            Some(0),
            "setting bob's rank must not disturb alice's"
        );
        assert_eq!(
            window_z_rank_for_test(&bob.owner_identity, window_id),
            Some(2)
        );

        // A window that drops out of the sharer's shared subset republishes
        // metadata that decodes to None for it -- the setter must clear the
        // stale rank, not leave the old one behind.
        update_window_z_rank(&alice.owner_identity, window_id, None);
        assert_eq!(
            window_z_rank_for_test(&alice.owner_identity, window_id),
            None
        );

        // No window open for this owner/id at all: silent no-op.
        update_window_z_rank("nobody-875", window_id, Some(1));
        assert_eq!(window_z_rank_for_test("nobody-875", window_id), None);

        with_state(|s| {
            s.windows.remove(&alice);
            s.windows.remove(&bob);
        });
    }

    // ---- #875: compositor_raise_participant_windows -----------------------

    #[test]
    fn raise_order_for_participant_windows_sorts_ranked_descending_with_unranked_first() {
        let owner = "raise875-sort-owner";
        let entries = vec![
            ParticipantWindowEntry {
                key: test_key(owner, 1),
                z_rank: Some(2),
                needs_restore: false,
            },
            ParticipantWindowEntry {
                key: test_key(owner, 2),
                z_rank: None,
                needs_restore: false,
            },
            ParticipantWindowEntry {
                key: test_key(owner, 3),
                z_rank: Some(0),
                needs_restore: false,
            },
            ParticipantWindowEntry {
                key: test_key(owner, 4),
                z_rank: None,
                needs_restore: false,
            },
            ParticipantWindowEntry {
                key: test_key(owner, 5),
                z_rank: Some(1),
                needs_restore: false,
            },
        ];

        let order = raise_order_for_participant_windows(entries);
        let ids: Vec<u32> = order.iter().map(|entry| entry.key.window_id).collect();
        assert_eq!(
            ids,
            vec![2, 4, 1, 5, 3],
            "unranked windows (2, 4) keep their input order and raise first (rearmost); \
             ranked windows raise in descending-rank order so rank 0 (window 3) raises \
             last and ends up frontmost"
        );
    }

    #[test]
    fn raise_order_for_participant_windows_handles_empty_and_single_entry() {
        assert_eq!(raise_order_for_participant_windows(Vec::new()), Vec::new());

        let owner = "raise875-sort-single-owner";
        let single = vec![ParticipantWindowEntry {
            key: test_key(owner, 0x875_0100),
            z_rank: Some(0),
            needs_restore: true,
        }];
        assert_eq!(
            raise_order_for_participant_windows(single.clone()),
            single,
            "a single entry is returned unchanged"
        );
    }

    /// #875: the real enumeration/eligibility/ordering logic
    /// `compositor_raise_participant_windows` runs. Seeds a mix of open
    /// ranked, open unranked, retired-with-live-publication, and
    /// retired-WITHOUT-publication windows for one owner and asserts (a) the
    /// exact back-to-front raise order, (b) the hidden-but-still-published
    /// window is included and marked `needs_restore`, and (c) the dead
    /// (unpublished) retired window is never touched at all -- the
    /// never-resurrect-a-phantom rule `plan_participant_raise`'s doc comment
    /// documents.
    #[test]
    fn plan_participant_raise_orders_back_to_front_restores_live_retired_and_skips_dead_retired() {
        let owner = "raise875-plan-owner";
        let frontmost = test_key(owner, 0x875_0201); // open, rank 0
        let middle = test_key(owner, 0x875_0202); // open, rank 2
        let hidden_live = test_key(owner, 0x875_0203); // retired, rank 1, LIVE publication
        let hidden_dead = test_key(owner, 0x875_0204); // retired, rank 3, NO publication
        let unranked = test_key(owner, 0x875_0205); // open, no rank (older sharer)

        with_state(|s| {
            for key in [&frontmost, &middle, &hidden_live, &hidden_dead, &unranked] {
                s.windows.remove(key);
                s.retired.remove(key);
                s.retired_order.retain(|stored| stored != key);
            }

            // Open windows in this fixture stand in for windows that are
            // already rendering (the ordinary case `plan_participant_raise`
            // documents as unconditionally eligible), so they carry
            // `revealed_first_frame = true` -- the #875 review F2 guard
            // (below) excludes an open window that hasn't shown yet.
            let mut win = test_window(owner, "Frontmost");
            win.z_rank = Some(0);
            win.revealed_first_frame = true;
            s.windows.insert(frontmost.clone(), win);

            let mut win = test_window(owner, "Middle");
            win.z_rank = Some(2);
            win.revealed_first_frame = true;
            s.windows.insert(middle.clone(), win);

            let mut win = test_window(owner, "HiddenLive");
            win.z_rank = Some(1);
            s.retired.insert(hidden_live.clone(), win);
            s.retired_order.push(hidden_live.clone());

            let mut win = test_window(owner, "HiddenDead");
            win.z_rank = Some(3);
            s.retired.insert(hidden_dead.clone(), win);
            s.retired_order.push(hidden_dead.clone());

            let mut win = test_window(owner, "Unranked");
            win.z_rank = None;
            win.revealed_first_frame = true;
            s.windows.insert(unranked.clone(), win);
        });

        // Only `hidden_live`'s window id has a publication -- `hidden_dead`
        // deliberately omitted, simulating a retired window whose sharer-side
        // teardown already ran for real.
        let live_window_ids: HashSet<u32> = [hidden_live.window_id].into_iter().collect();

        let plan = plan_participant_raise(owner, &live_window_ids);
        let keys: Vec<RemoteWindowKey> = plan.iter().map(|entry| entry.key.clone()).collect();
        assert_eq!(
            keys,
            vec![
                unranked.clone(),
                middle.clone(),
                hidden_live.clone(),
                frontmost.clone(),
            ],
            "back-to-front: unranked first, then descending rank, rank 0 (frontmost) last"
        );
        assert!(
            !keys.contains(&hidden_dead),
            "a retired window with no live publication must never appear in the plan"
        );

        let needs_restore: Vec<bool> = plan.iter().map(|entry| entry.needs_restore).collect();
        assert_eq!(
            needs_restore,
            vec![false, false, true, false],
            "only the retired-but-live window is marked for restore"
        );

        with_state(|s| {
            for key in [&frontmost, &middle, &hidden_live, &hidden_dead, &unranked] {
                s.windows.remove(key);
                s.retired.remove(key);
                s.retired_order.retain(|stored| stored != key);
            }
        });
    }

    #[test]
    fn plan_participant_raise_scopes_by_owner_identity_ignoring_other_participants() {
        // #678-class collision guard: two participants can share the same
        // numeric window id, and the plan for one must never include the
        // other's window.
        let window_id = 0x875_0300;
        let alice = test_key("raise875-alice", window_id);
        let bob = test_key("raise875-bob", window_id);

        with_state(|s| {
            for key in [&alice, &bob] {
                s.windows.remove(key);
                s.retired.remove(key);
            }
            let mut win = test_window(&alice.owner_identity, "Alice");
            win.z_rank = Some(0);
            win.revealed_first_frame = true;
            s.windows.insert(alice.clone(), win);

            let mut win = test_window(&bob.owner_identity, "Bob");
            win.z_rank = Some(0);
            win.revealed_first_frame = true;
            s.windows.insert(bob.clone(), win);
        });

        let plan = plan_participant_raise(&alice.owner_identity, &HashSet::new());
        assert_eq!(
            plan.iter().map(|e| e.key.clone()).collect::<Vec<_>>(),
            vec![alice.clone()],
            "bob's identically-numbered window must not appear in alice's plan"
        );

        with_state(|s| {
            s.windows.remove(&alice);
            s.windows.remove(&bob);
        });
    }

    /// #875 review F1: `transport::subscriber`'s macOS `TrackSubscribed`
    /// handler now seeds a just-created window's z-rank from metadata
    /// already available at subscribe time, calling the exact two
    /// production functions this test drives in the exact order the fixed
    /// call site uses: `shared_window_z_rank_from_metadata` to decode, then
    /// `update_window_z_rank` to store. Before the fix, `ensure_window`
    /// inserted the window with `z_rank: None` and NOTHING at subscribe
    /// time ever looked at the z-order metadata -- the window was stuck
    /// unranked until the sharer's next `petalWindowZOrder` change (which,
    /// since it only republishes on change, could be never). This proves
    /// the storage half of that fix: a window created AFTER metadata
    /// already carried its rank ends up with that rank actually stored.
    #[test]
    fn window_created_after_metadata_carried_z_rank_ends_up_with_that_rank_stored() {
        let owner = "raise875-f1-owner";
        let window_id = 0x875_0400;
        let key = test_key(owner, window_id);

        with_state(|s| {
            s.windows.remove(&key);
        });

        // The sharer already published an order that includes this window
        // at rank 1 (index 1, behind 999 and ahead of 111) BEFORE this
        // window's TrackSubscribed lands -- e.g. a rearrange that happened
        // right after the share started publishing, ahead of this
        // receiver's subscribe.
        let metadata = format!(r#"{{"petalWindowZOrder":[999,{window_id},111]}}"#);

        // `ensure_window` always inserts a fresh window with `z_rank: None`
        // -- mirrored here directly, matching the existing "a freshly-opened
        // window starts with no rank" fixture pattern used elsewhere in this
        // module's #875 tests.
        with_state(|s| {
            s.windows.insert(key.clone(), test_window(owner, "F1 window"));
        });
        assert_eq!(
            window_z_rank_for_test(owner, window_id),
            None,
            "freshly created window starts unranked, exactly like ensure_window leaves it"
        );

        // The fixed TrackSubscribed call site, reproduced exactly: decode
        // then store, using the real production functions.
        crate::compositor::update_window_z_rank(
            owner,
            window_id,
            crate::transport::publisher::shared_window_z_rank_from_metadata(
                &metadata, window_id,
            ),
        );

        assert_eq!(
            window_z_rank_for_test(owner, window_id),
            Some(1),
            "a window created after its rank was already published must end up carrying \
             that rank rather than staying stuck at None until the next metadata change"
        );

        with_state(|s| {
            s.windows.remove(&key);
        });
    }

    /// #875 review F2: `ensure_window` opens a panel HIDDEN and only reveals
    /// it once the first decoded frame arrives
    /// (`CompositorWindow::revealed_first_frame`). `raise_panel_only`'s own
    /// doc comment warns that `orderFrontRegardless` un-hides an ordered-out
    /// window, so `plan_participant_raise` must never plan to raise an open
    /// window that has never shown a frame yet -- doing so would un-hide a
    /// hollow, transparent panel. This seeds exactly that state (open,
    /// ranked, but `revealed_first_frame: false`) and asserts the plan
    /// excludes it entirely.
    #[test]
    fn plan_participant_raise_excludes_open_window_before_first_frame_reveal() {
        let owner = "raise875-f2-owner";
        let unrevealed = test_key(owner, 0x875_0500);

        with_state(|s| {
            s.windows.remove(&unrevealed);
            let mut win = test_window(owner, "PreFirstFrame");
            win.z_rank = Some(0);
            win.revealed_first_frame = false;
            s.windows.insert(unrevealed.clone(), win);
        });

        let plan = plan_participant_raise(owner, &HashSet::new());
        assert!(
            plan.is_empty(),
            "an open window that has never revealed its first frame must never appear in \
             the raise plan -- raising it would un-hide a hollow, contentless panel"
        );

        with_state(|s| {
            s.windows.remove(&unrevealed);
        });
    }

    /// #875 review F3: the `ParticipantMetadataChanged` handler only ever
    /// looked at OPEN windows (`window_ids_for_participant`), so a window
    /// the viewer had retired (hidden) never learned about a fresh z-rank
    /// while hidden and got restored later into its stale at-hide position.
    /// Proves both halves of the fix: (1) `update_window_z_rank` -- the
    /// storage primitive the handler calls -- now reaches a `s.retired`
    /// entry directly (previously only `s.windows` was touched, so this
    /// call would silently no-op for a retired key), and is discoverable via
    /// `retired_window_ids_for_participant` (the enumeration the handler's
    /// loop now unions in); and (2) the fresh rank actually changes the
    /// outcome of a subsequent `plan_participant_raise` call, not just an
    /// internal field.
    #[test]
    fn retired_window_z_rank_updates_while_hidden_and_flows_into_raise_order() {
        let owner = "raise875-f3-owner";
        let retired_key = test_key(owner, 0x875_0601); // starts rank 5 (rearmost)
        let open_key = test_key(owner, 0x875_0602); // rank 2, already showing

        with_state(|s| {
            s.windows.remove(&open_key);
            s.retired.remove(&retired_key);
            s.retired_order.retain(|stored| stored != &retired_key);

            let mut win = test_window(owner, "RetiredStale");
            win.z_rank = Some(5);
            s.retired.insert(retired_key.clone(), win);
            s.retired_order.push(retired_key.clone());

            let mut win = test_window(owner, "OpenMiddle");
            win.z_rank = Some(2);
            win.revealed_first_frame = true;
            s.windows.insert(open_key.clone(), win);
        });

        assert!(
            retired_window_ids_for_participant(owner).contains(&retired_key.window_id),
            "the retired window must be discoverable by the enumeration the metadata \
             handler's loop unions in, or update_window_z_rank is never even called for it"
        );

        // Simulate a fresh `petalWindowZOrder` metadata refresh that now
        // puts the retired window frontmost (rank 0) while it is STILL
        // retired -- exactly the scenario the handler's widened enumeration
        // now reaches.
        update_window_z_rank(owner, retired_key.window_id, Some(0));

        let stored_retired_rank = with_state(|s| s.retired.get(&retired_key).and_then(|w| w.z_rank));
        assert_eq!(
            stored_retired_rank,
            Some(0),
            "update_window_z_rank must reach a retired entry directly, not silently no-op \
             for a key that isn't in s.windows"
        );

        // Downstream proof: a subsequent raise plan must reflect the FRESH
        // rank (0, frontmost -> raises last), not the stale one (5) it was
        // retired with.
        let live_window_ids: HashSet<u32> = [retired_key.window_id].into_iter().collect();
        let plan = plan_participant_raise(owner, &live_window_ids);
        let keys: Vec<RemoteWindowKey> = plan.iter().map(|entry| entry.key.clone()).collect();
        assert_eq!(
            keys,
            vec![open_key.clone(), retired_key.clone()],
            "the retired window's fresh rank (0, frontmost) must place it LAST in the \
             back-to-front plan -- if the stale rank (5) had survived instead, it would \
             have sorted FIRST, ahead of the open rank-2 window"
        );

        with_state(|s| {
            s.windows.remove(&open_key);
            s.retired.remove(&retired_key);
            s.retired_order.retain(|stored| stored != &retired_key);
        });
    }

    #[test]
    fn resolve_open_window_key_excludes_retired_windows_but_resolve_window_key_does_not() {
        // #678: `raise_window_for_click` deliberately calls
        // `resolve_open_window_key`, not `resolve_window_key`, so a click can
        // never resurrect a retired/phantom window. This proves the two
        // resolvers actually differ the way that choice depends on, rather
        // than trusting the source-grep test's assumption about it.
        let key = test_key("click-raise-owner-678", 0x678_0001);

        with_state(|s| {
            s.windows.remove(&key);
            s.retired.remove(&key);
            s.retired.insert(
                key.clone(),
                test_window(&key.owner_identity, "Retired Terminal"),
            );
        });

        assert_eq!(
            resolve_open_window_key(key.window_id, Some(&key.owner_identity)),
            None,
            "a retired-only window must not resolve as open"
        );
        assert_eq!(
            resolve_window_key(key.window_id, Some(&key.owner_identity)),
            Some(key.clone()),
            "resolve_window_key (used by header drag, resize, and Pop Out, which ALL \
             restore retired windows via activate_window_then -- #843 fixed drag/Pop \
             Out, #855 fixed resize) must still find it -- otherwise this test isn't \
             proving the two resolvers differ, just that both fail"
        );

        with_state(|s| {
            s.retired.remove(&key);
        });
    }

    /// #843: `owner_identity_for_window` is the function `remote_control.rs`'s
    /// `viewer_channel` calls to find who to address a control request to --
    /// this is the exact function whose OLD open-only resolution turned a
    /// visible-but-retired remote window into "Remote control request could
    /// not be sent." (a generic-looking failure with no useful diagnosis).
    /// Owner identity is stable metadata, unrelated to whether the window
    /// is currently restored -- this proves it now survives a retire.
    #[test]
    fn owner_identity_for_window_843_resolves_through_the_retired_pool() {
        let key = test_key("retired-owner-843", 0x843_0001);

        with_state(|s| {
            s.windows.remove(&key);
            s.retired.remove(&key);
            s.retired.insert(
                key.clone(),
                test_window(&key.owner_identity, "Retired Terminal 843"),
            );
        });

        assert_eq!(
            owner_identity_for_window(key.window_id, Some(&key.owner_identity)),
            Some(key.owner_identity.clone()),
            "a retired-but-visible window's owner must still resolve -- this is exactly \
             what let a mid-republish-storm control request/drag silently fail"
        );
        assert_eq!(
            owner_identity_for_window(key.window_id, None),
            Some(key.owner_identity.clone()),
            "owner-agnostic lookup must also survive retirement when unambiguous"
        );

        with_state(|s| {
            s.retired.remove(&key);
        });
        assert_eq!(
            owner_identity_for_window(key.window_id, Some(&key.owner_identity)),
            None,
            "a window that is neither open nor retired (genuinely gone) must still resolve \
             to nothing -- this is not a blanket bypass"
        );
    }

    #[test]
    fn remote_control_target_metadata_tracks_kind_and_share_instance() {
        let key = test_key("metadata-owner-remote-control", 0x8a01);
        let mut window = test_window(&key.owner_identity, "Metadata target");
        window.share_instance_id = Some("share-instance-a".to_string());
        with_state(|s| {
            s.windows.remove(&key);
            s.retired.remove(&key);
            s.windows.insert(key.clone(), window);
        });

        assert_eq!(
            remote_control_target_metadata(key.window_id, Some(&key.owner_identity)),
            Some(RemoteControlTargetMetadata {
                target_kind: crate::remote_control_core::RemoteControlTargetKind::Window,
                share_instance_id: Some("share-instance-a".to_string()),
            })
        );

        with_state(|s| {
            let window = s.windows.get_mut(&key).expect("test window");
            window.source_kind = SharedSourceKind::Display;
            window.share_instance_id = Some("share-instance-b".to_string());
        });
        assert_eq!(
            remote_control_target_metadata(key.window_id, Some(&key.owner_identity)),
            Some(RemoteControlTargetMetadata {
                target_kind: crate::remote_control_core::RemoteControlTargetKind::Display,
                share_instance_id: Some("share-instance-b".to_string()),
            })
        );

        with_state(|s| {
            s.windows.remove(&key);
        });
    }

    #[test]
    fn debug_stats_command_exposes_metadata_last_frame_and_counter() {
        let key = test_key("debug-owner-143", 143);
        let mut window = test_window(&key.owner_identity, "Debug Terminal");
        window.owner_display_name = "Debug Owner".to_string();
        window.source_url = Some("https://example.test/debug".to_string());
        *window.panel_content_size.lock_unpoisoned() = (321.0, 222.0);
        *window.receiver_scale.lock_unpoisoned() = 2.0;
        *window.source_pixel_size.lock_unpoisoned() = Some((642, 444));
        window
            .last_frame_received_ms
            .store(1_725_000_123_456, Ordering::Relaxed);
        window.frames_received.store(17, Ordering::Relaxed);

        with_state(|s| {
            s.windows.remove(&key);
            s.retired.remove(&key);
            s.windows.insert(key.clone(), window);
        });

        let stats = compositor_window_debug_stats(143, Some(key.owner_identity.clone())).unwrap();

        assert_eq!(stats.window_id, 143);
        assert_eq!(stats.owner_identity, "debug-owner-143");
        assert_eq!(stats.owner_display_name, "Debug Owner");
        assert_eq!(stats.source_title, "Debug Terminal");
        assert_eq!(
            stats.source_url.as_deref(),
            Some("https://example.test/debug")
        );
        assert_eq!(stats.content_width, 321.0);
        assert_eq!(stats.content_height, 222.0);
        assert_eq!(stats.receiver_scale, 2.0);
        assert_eq!(stats.display_pixel_width, 642);
        assert_eq!(stats.display_pixel_height, 444);
        assert_eq!(stats.source_pixel_width, Some(642));
        assert_eq!(stats.source_pixel_height, Some(444));
        assert_eq!(stats.last_frame_received_ms, Some(1_725_000_123_456));
        assert_eq!(stats.frames_received, 17);
        assert!(stats.remote_control_available);

        with_state(|s| {
            s.windows.remove(&key);
        });
    }

    #[test]
    fn content_frame_excludes_remote_window_header() {
        assert_eq!(
            content_frame_from_panel_bounds(100.0, 200.0, 640.0, HEADER_HEIGHT + 400.0),
            Some(WindowFrame {
                x: 100,
                y: (200.0 + HEADER_HEIGHT).round() as i32,
                width: 640,
                height: 400,
            })
        );
    }

    #[test]
    fn content_frame_skips_header_only_panels() {
        assert_eq!(
            content_frame_from_panel_bounds(0.0, 0.0, 640.0, HEADER_HEIGHT),
            None
        );
    }

    #[test]
    fn chrome_frames_track_panel_bounds_for_move_and_resize_handlers() {
        // The header rides in the panel's own surface webview now; only the
        // click-through control/pointer overlays are separate windows, and
        // both cover only the video content area below the header strip.
        let frames = chrome_frames_for_panel_bounds(120.0, 80.0, 640.0, HEADER_HEIGHT + 400.0);

        assert_eq!(
            frames,
            ChromeFrames {
                control: ChromeFrame {
                    x: 120.0,
                    y: 80.0 + HEADER_HEIGHT,
                    width: 640.0,
                    height: 400.0,
                },
                pointer: ChromeFrame {
                    x: 120.0,
                    y: 80.0 + HEADER_HEIGHT,
                    width: 640.0,
                    height: 400.0,
                },
                // #844: fixed-size overlay anchored to the content area's
                // top-right, inset by AI_CHAT_OVERLAY_MARGIN (12pt), clamped
                // to AI_CHAT_OVERLAY_WIDTH/MAX_HEIGHT (300x360) -- both fit
                // inside this 640x400 content area unclamped.
                ai_chat: ChromeFrame {
                    x: 120.0 + 640.0 - 300.0 - 12.0,
                    y: 80.0 + HEADER_HEIGHT + 12.0,
                    width: 300.0,
                    height: 360.0,
                },
            }
        );
    }

    #[test]
    fn ai_chat_overlay_frame_clamps_to_a_small_content_area_without_going_negative() {
        // A window small enough that AI_CHAT_OVERLAY_WIDTH/MAX_HEIGHT (300x360)
        // don't fit must clamp to the content area minus margins on both
        // sides, not just floor at 1pt or go negative.
        let frame = ai_chat_overlay_frame_for_panel_bounds(0.0, 0.0, 300.0, HEADER_HEIGHT + 150.0);
        assert_eq!(
            frame,
            ChromeFrame {
                x: 12.0,
                y: HEADER_HEIGHT + 12.0,
                width: 276.0,
                height: 126.0,
            }
        );
        assert!(
            frame.x >= 0.0,
            "clamped overlay must not be positioned off the left edge"
        );
        assert!(
            frame.width > 0.0 && frame.height > 0.0,
            "clamped overlay must never have non-positive dimensions"
        );
    }

    #[test]
    fn chrome_frame_update_skips_subpixel_noop_repositions() {
        let current = ChromeFrame {
            x: 120.0,
            y: 80.0 + HEADER_HEIGHT,
            width: 640.0,
            height: 400.0,
        };

        assert!(!chrome_frame_needs_update(
            current,
            ChromeFrame {
                x: 120.25,
                y: 80.25 + HEADER_HEIGHT,
                width: 640.25,
                height: 400.25,
            }
        ));
        assert!(chrome_frame_needs_update(
            current,
            ChromeFrame {
                x: 121.0,
                y: 80.0 + HEADER_HEIGHT,
                width: 640.0,
                height: 400.0,
            }
        ));
    }

    #[test]
    fn chrome_frame_update_detects_initial_add_child_window_offset_signature() {
        let target = chrome_frames_for_panel_bounds(121.0, 79.0, 640.0, HEADER_HEIGHT + 400.0);
        let stuck_at_creation_offset = ChromeFrame {
            x: 0.0,
            y: HEADER_HEIGHT,
            width: 640.0,
            height: 400.0,
        };

        assert_eq!(target.control.x, 121.0);
        assert_eq!(target.control.y, 79.0 + HEADER_HEIGHT);
        assert!(chrome_frame_needs_update(
            stuck_at_creation_offset,
            target.control
        ));
        assert!(chrome_frame_needs_update(
            stuck_at_creation_offset,
            target.pointer
        ));
    }

    #[test]
    fn aspect_lock_corrects_height_used_for_display_refit() {
        let (content_h, corrected) = aspect_locked_content_height(800.0, 430.0, 1.6);

        assert!(corrected);
        assert_eq!(content_h, 500.0);
    }

    #[test]
    fn aspect_lock_floors_a_degenerate_height_before_correcting() {
        // A near-zero content height (e.g. a resize event delivered before the
        // panel has settled) must floor to 1pt rather than divide-by-near-zero,
        // and -- since it is not the source aspect -- still gets corrected.
        let (content_h, corrected) = aspect_locked_content_height(800.0, 0.0, 1.6);

        assert!(corrected);
        assert_eq!(content_h, 500.0);
    }

    #[test]
    fn aspect_lock_leaves_rounding_jitter_alone() {
        let (content_h, corrected) = aspect_locked_content_height(640.0, 401.0, 1.6);

        assert!(!corrected);
        assert_eq!(content_h, 401.0);
    }

    #[test]
    fn resize_drag_preserves_content_aspect_from_south_east_corner() {
        let frame = resized_frame_from_drag(
            CompositorResizeDirection::SouthEast,
            1.6,
            CompositorResizeFrame {
                x: 100.0,
                y: 100.0,
                width: 640.0,
                height: HEADER_HEIGHT + 400.0,
            },
            160.0,
            30.0,
        );

        assert_eq!(frame.x, 100.0);
        assert_eq!(frame.y, 100.0);
        assert_eq!(frame.width, 800.0);
        assert_eq!(frame.height, HEADER_HEIGHT + 500.0);
    }

    #[test]
    fn resize_drag_anchors_opposite_corner_when_resizing_from_north_west() {
        let frame = resized_frame_from_drag(
            CompositorResizeDirection::NorthWest,
            1.6,
            CompositorResizeFrame {
                x: 100.0,
                y: 100.0,
                width: 640.0,
                height: HEADER_HEIGHT + 400.0,
            },
            -160.0,
            -100.0,
        );

        assert_eq!(frame.x, -60.0);
        assert_eq!(frame.y, 0.0);
        assert_eq!(frame.width, 800.0);
        assert_eq!(frame.height, HEADER_HEIGHT + 500.0);
    }

    #[test]
    fn resize_drag_clamps_to_minimum_content_size() {
        let frame = resized_frame_from_drag(
            CompositorResizeDirection::West,
            1.6,
            CompositorResizeFrame {
                x: 100.0,
                y: 100.0,
                width: 640.0,
                height: HEADER_HEIGHT + 400.0,
            },
            1000.0,
            0.0,
        );

        assert_eq!(frame.x, 440.0);
        assert_eq!(frame.y, 100.0);
        assert_eq!(frame.width, MIN_RESIZE_CONTENT_WIDTH);
        assert_eq!(frame.height, HEADER_HEIGHT + MIN_RESIZE_CONTENT_WIDTH / 1.6);
    }

    #[test]
    fn receiver_minimum_tracks_the_compact_header_breakpoint() {
        assert_eq!(remote_window_min_size().width, 300.0);
        assert_eq!(
            remote_window_min_size().height,
            HEADER_HEIGHT + MIN_RESIZE_CONTENT_HEIGHT
        );
    }

    #[test]
    fn resize_end_snaps_near_integer_scale_and_preserves_west_anchor() {
        let frame = snap_resized_frame_to_integer_scale(
            CompositorResizeDirection::West,
            CompositorResizeFrame {
                x: 90.0,
                y: 100.0,
                width: 1020.0,
                height: HEADER_HEIGHT + 510.0,
            },
            Some((1000, 500)),
            2.0,
            1020.0,
            HEADER_HEIGHT + 510.0,
        );

        assert_eq!(frame.x, 110.0);
        assert_eq!(frame.y, 100.0);
        assert_eq!(frame.width, 1000.0);
        assert_eq!(frame.height, HEADER_HEIGHT + 500.0);
        assert_nearest_for_content_size(1000, 500, frame.width, frame.height - HEADER_HEIGHT, 2.0);
    }

    #[test]
    fn resize_end_snaps_near_integer_scale_and_preserves_north_anchor() {
        let frame = snap_resized_frame_to_integer_scale(
            CompositorResizeDirection::North,
            CompositorResizeFrame {
                x: 90.0,
                y: 80.0,
                width: 1020.0,
                height: HEADER_HEIGHT + 510.0,
            },
            Some((1000, 500)),
            2.0,
            1020.0,
            HEADER_HEIGHT + 510.0,
        );

        assert_eq!(frame.x, 90.0);
        assert_eq!(frame.y, 90.0);
        assert_eq!(frame.width, 1000.0);
        assert_eq!(frame.height, HEADER_HEIGHT + 500.0);
        assert_nearest_for_content_size(1000, 500, frame.width, frame.height - HEADER_HEIGHT, 2.0);
    }

    #[test]
    fn resize_end_skips_snap_that_would_undo_most_of_the_drag() {
        // Live testing 2026-07-14 (T3): a relocated canonical grid point can
        // make the finalize-time snap revert most of what the user just
        // dragged. Simulate that: the user dragged from 1090 wide (start) up
        // to 1102 (a +12 drag), but the grid point at 1000 sits just inside
        // the 5% snap threshold of the released 1102 width -- snapping would
        // revert almost the ENTIRE drag (1102 -> 1000, a -102 correction,
        // far more than half of the +12 the user actually dragged). The
        // guard must refuse this snap and keep the released size.
        let frame = CompositorResizeFrame {
            x: 100.0,
            y: 100.0,
            width: 1102.0,
            height: HEADER_HEIGHT + 500.0,
        };
        let snapped = snap_resized_frame_to_integer_scale(
            CompositorResizeDirection::SouthEast,
            frame,
            Some((1000, 500)),
            2.0,
            1090.0,
            HEADER_HEIGHT + 500.0,
        );
        assert_eq!(
            snapped, frame,
            "snap must not revert most of the user's drag"
        );
    }

    #[test]
    fn resize_end_still_snaps_when_correction_is_small_relative_to_the_drag() {
        // Contrast case: a real, meaningful drag (900 -> 1020, +120) ending
        // near a grid point (1000, a -20 correction) is a small correction
        // relative to what was actually dragged -- well under half of it --
        // so the guard must not disable snapping altogether.
        let frame = CompositorResizeFrame {
            x: 100.0,
            y: 100.0,
            width: 1020.0,
            height: HEADER_HEIGHT + 500.0,
        };
        let snapped = snap_resized_frame_to_integer_scale(
            CompositorResizeDirection::SouthEast,
            frame,
            Some((1000, 500)),
            2.0,
            900.0,
            HEADER_HEIGHT + 500.0,
        );
        assert_eq!(
            snapped.width, 1000.0,
            "a small correction relative to the drag should still snap"
        );
    }

    #[test]
    fn resize_end_leaves_fractional_scale_alone() {
        let frame = CompositorResizeFrame {
            x: 100.0,
            y: 100.0,
            width: 750.0,
            height: HEADER_HEIGHT + 375.0,
        };

        assert_eq!(
            snap_resized_frame_to_integer_scale(
                CompositorResizeDirection::SouthEast,
                frame,
                Some((1000, 500)),
                2.0,
                frame.width,
                frame.height,
            ),
            frame
        );
    }

    #[test]
    fn resize_end_leaves_sizes_outside_snap_threshold_alone() {
        let frame = CompositorResizeFrame {
            x: 100.0,
            y: 100.0,
            width: 1060.0,
            height: HEADER_HEIGHT + 530.0,
        };

        assert_eq!(
            snap_resized_frame_to_integer_scale(
                CompositorResizeDirection::SouthEast,
                frame,
                Some((1000, 500)),
                2.0,
                frame.width,
                frame.height,
            ),
            frame
        );
    }

    #[test]
    fn resize_end_leaves_axis_mismatch_alone_when_filter_would_stay_linear() {
        let frame = CompositorResizeFrame {
            x: 100.0,
            y: 100.0,
            width: 1020.0,
            height: HEADER_HEIGHT + 480.0,
        };

        assert_eq!(
            snap_resized_frame_to_integer_scale(
                CompositorResizeDirection::SouthEast,
                frame,
                Some((1000, 500)),
                2.0,
                frame.width,
                frame.height,
            ),
            frame
        );
        assert_eq!(
            display_filter_for_geometry(1000, 500, frame.width, frame.height - HEADER_HEIGHT, 2.0),
            DisplayLayerFilter::Linear
        );
    }

    #[test]
    fn all_fitting_initial_integer_sizes_select_nearest_filter() {
        for (source_w, source_h, source_scale, receiver_scale) in [
            (800, 500, 1.0, 1.0),
            (800, 500, 2.0, 2.0),
            (1200, 800, 1.0, 2.0),
            (1200, 800, 2.0, 1.0),
        ] {
            let (w, h) = initial_content_size_within_work_area(
                source_w,
                source_h,
                source_scale,
                receiver_scale,
                4000.0,
                3000.0,
            );
            assert_nearest_for_content_size(source_w, source_h, w, h, receiver_scale);
        }
    }

    #[test]
    fn lifecycle_model_reuses_retired_window_for_same_id_without_advancing_cascade() {
        let mut model = LifecycleModel::default();

        assert_eq!(model.ensure(7), LifecycleAction::Created { slot: 0 });
        model.remove(7);
        assert_eq!(model.ensure(7), LifecycleAction::Reused { slot: 0 });

        assert_eq!(model.next_cascade_slot, 1);
        assert!(model.retired.is_empty());
        assert_eq!(model.open.get(&7), Some(&0));
    }

    #[test]
    fn republish_storm_keeps_tracking_and_reveals_retained_layer_content_on_every_reuse() {
        use crate::transport::subscriber::{
            registry_update_for, teardown_decision, track_unsubscribe_decision, RegistryUpdate,
            TeardownDecision,
        };

        let window_id = 840;
        let mut model = LifecycleModel::default();
        let mut tracked_sid = "TR_generation_0".to_string();
        // Stands in for `transport::subscriber`'s publication registry entry:
        // `Some(sid)` while the window is tracked, `None` once dropped.
        let mut tracked = Some(tracked_sid.clone());

        // The first subscribe creates a reveal-gated panel; the real display
        // drain transition makes both the layer-content fact and reveal gate
        // true when its first sample is enqueued.
        assert_eq!(
            model.ensure(window_id),
            LifecycleAction::Created { slot: 0 }
        );
        model.enqueue_display_sample(window_id);

        for generation in 1..=3 {
            // These are the production decisions called by
            // `resolve_teardown`/`apply_teardown_decision`, not a test-only
            // table: unsubscribe is non-terminal, then the old publication's
            // unpublish observes its replacement already inbound at the SFU.
            let unsubscribe =
                track_unsubscribe_decision(Some(tracked_sid.as_str()), tracked_sid.as_str());
            assert_eq!(unsubscribe, TeardownDecision::HoldForTransientUnsubscribe);
            assert_eq!(registry_update_for(unsubscribe), RegistryUpdate::Keep);

            let unpublish =
                teardown_decision(Some(tracked_sid.as_str()), tracked_sid.as_str(), true);
            assert_eq!(unpublish, TeardownDecision::HoldForReplacement);
            assert_eq!(registry_update_for(unpublish), RegistryUpdate::Keep);
            // Apply both registry updates the way `resolve_teardown` does, to
            // the simulated publication registry. A `RemoveIfUnchanged` for
            // the sid we still track would drop the entry -- and
            // `Divergence::Orphaned` is only ever reported for keys present in
            // `tracked`, so losing it here is the phantom-window regression
            // CLAUDE.md rates worse than the vanishing this issue fixes.
            for update in [
                registry_update_for(unsubscribe),
                registry_update_for(unpublish),
            ] {
                if update == RegistryUpdate::RemoveIfUnchanged
                    && tracked.as_deref() == Some(tracked_sid.as_str())
                {
                    tracked = None;
                }
            }
            assert_eq!(
                tracked.as_deref(),
                Some(tracked_sid.as_str()),
                "cycle {generation}: a non-terminal teardown must leave the window TRACKED"
            );

            // Drive the existing lifecycle model's real retired-pool reuse
            // decision. `LifecycleModel::ensure` calls the SAME
            // `apply_retired_reuse_reveal_state` production ensure_window's
            // retired branch calls before show_retired_window_on_main.
            model.remove(window_id);
            assert!(model.layer_has_content(window_id));
            assert_eq!(model.ensure(window_id), LifecycleAction::Reused { slot: 0 });
            assert!(
                model.is_revealed(window_id),
                "cycle {generation}: a retained display frame must be revealed immediately"
            );
            assert!(
                !model.layer_has_content(window_id) || model.is_revealed(window_id),
                "cycle {generation}: the panel cannot stay off screen while its layer has content"
            );

            // Replacement TrackSubscribed adopts the next generation without
            // ever passing through an untracked state.
            tracked_sid = format!("TR_generation_{generation}");
            tracked = Some(tracked_sid.clone());
        }
        assert_eq!(
            tracked.as_deref(),
            Some(tracked_sid.as_str()),
            "the window must still be tracked after the whole storm"
        );
    }

    #[test]
    fn lifecycle_model_new_id_after_retire_gets_fresh_cascade_slot() {
        let mut model = LifecycleModel::default();

        assert_eq!(model.ensure(7), LifecycleAction::Created { slot: 0 });
        model.remove(7);
        assert_eq!(model.ensure(8), LifecycleAction::Created { slot: 1 });

        assert_eq!(model.retired.get(&7), Some(&0));
        assert_eq!(model.open.get(&8), Some(&1));
    }

    #[test]
    fn lifecycle_model_idempotent_ensure_does_not_consume_cascade_slot() {
        let mut model = LifecycleModel::default();

        assert_eq!(model.ensure(7), LifecycleAction::Created { slot: 0 });
        assert_eq!(model.ensure(7), LifecycleAction::AlreadyOpen);

        assert_eq!(model.next_cascade_slot, 1);
        assert_eq!(model.open.len(), 1);
    }

    #[test]
    fn lifecycle_model_hide_order_keeps_parent_panel_until_last() {
        let mut model = LifecycleModel::default();
        model.ensure(7);

        model.remove(7);

        assert_eq!(
            model.hidden_order,
            vec![
                header_label_for_key(&test_key("owner", 7)),
                control_label_for_key(&test_key("owner", 7)),
                pointer_label_for_key(&test_key("owner", 7)),
                panel_label_for_key(&test_key("owner", 7))
            ]
        );
        assert_eq!(model.retired.get(&7), Some(&0));
        assert!(!model.open.contains_key(&7));
    }

    #[test]
    fn lifecycle_model_header_hide_uses_same_retire_path_as_share_end() {
        let mut model = LifecycleModel::default();
        model.ensure(7);

        model.hide_from_header(7);

        assert_eq!(
            model.hidden_order,
            vec![
                header_label_for_key(&test_key("owner", 7)),
                control_label_for_key(&test_key("owner", 7)),
                pointer_label_for_key(&test_key("owner", 7)),
                panel_label_for_key(&test_key("owner", 7))
            ]
        );
        assert_eq!(model.retired.get(&7), Some(&0));
        assert!(!model.open.contains_key(&7));
    }

    #[test]
    fn lifecycle_model_lists_open_and_hidden_remote_windows_for_switcher() {
        let mut model = LifecycleModel::default();
        model.ensure(7);
        model.ensure(9);
        model.remove(7);

        assert_eq!(model.listed_windows(), vec![(9, false), (7, true)]);
    }

    #[test]
    fn lifecycle_model_activation_restores_hidden_window_without_new_cascade_slot() {
        let mut model = LifecycleModel::default();
        assert_eq!(model.ensure(7), LifecycleAction::Created { slot: 0 });
        model.remove(7);

        assert!(model.activate(7));
        assert_eq!(model.open.get(&7), Some(&0));
        assert!(!model.retired.contains_key(&7));
        assert_eq!(model.next_cascade_slot, 1);
    }

    #[test]
    fn lifecycle_model_caps_warm_retired_pool_to_most_recent_four() {
        let mut model = LifecycleModel::default();
        for id in 1..=6 {
            assert_eq!(model.ensure(id), LifecycleAction::Created { slot: id - 1 });
            model.remove(id);
        }

        assert_eq!(model.retired_order, vec![3, 4, 5, 6]);
        assert_eq!(model.stripped, vec![1, 2]);
        assert_eq!(model.retired.len(), 6);

        assert_eq!(model.ensure(1), LifecycleAction::Reused { slot: 0 });
        assert!(!model.stripped.contains(&1));
        assert!(!model.retired.contains_key(&1));
    }

    #[test]
    fn pending_frame_queue_schedules_once_and_keeps_only_the_newest_push() {
        let mut queue = PendingFrameQueue::default();

        assert!(queue.push(1));
        assert!(!queue.push(2));
        assert!(!queue.push(3));

        assert_eq!(queue.drain_scheduled(), vec![3]);
        assert!(queue.push(4));
    }

    #[test]
    fn pending_frame_queue_never_retains_stale_backlog() {
        let mut queue = PendingFrameQueue::default();

        for frame in 1..=MAX_PENDING_DISPLAY_SAMPLES_PER_WINDOW + 2 {
            queue.push(frame);
        }

        assert_eq!(queue.drain_scheduled(), vec![3]);
    }

    #[test]
    fn pending_frame_queue_clear_resets_scheduled_state() {
        let mut queue = PendingFrameQueue::default();

        assert!(queue.push(1));
        queue.clear();

        assert!(queue.drain_scheduled().is_empty());
        assert!(queue.push(2));
        assert_eq!(queue.drain_scheduled(), vec![2]);
    }

    #[test]
    fn clearing_pending_frames_does_not_increment_display_enqueue_counter() {
        let window = test_window("alice", "Alice Terminal");
        let mut queue = PendingFrameQueue::default();

        assert!(queue.push(1));
        window.frames_received.fetch_add(1, Ordering::Relaxed);
        queue.clear();

        assert_eq!(window.frames_received.load(Ordering::Relaxed), 1);
        assert_eq!(window.frames_display_enqueued.load(Ordering::Relaxed), 0);
        assert_eq!(window.last_display_enqueued_ms.load(Ordering::Relaxed), 0);

        record_display_enqueue(&window, 1234);

        assert_eq!(window.frames_received.load(Ordering::Relaxed), 1);
        assert_eq!(window.frames_display_enqueued.load(Ordering::Relaxed), 1);
        assert_eq!(
            window.last_display_enqueued_ms.load(Ordering::Relaxed),
            1234
        );
    }

    #[test]
    fn remote_border_treatment_matches_local_screenshare_outline() {
        assert_eq!(SCREENSHARE_BORDER_STROKE_PX, 4.0);
        assert_eq!(SCREENSHARE_BORDER_RADIUS_PX, 10.0);
    }

    #[test]
    fn a_sharer_denial_is_distinguishable_from_metadata_not_having_arrived() {
        // Three states, three distinct query strings. Collapsing "denied" into
        // "not available" is what made the header promise "Preparing..." for a
        // window that would never become controllable.
        let preparing =
            header_query_string(7, "marco-id", "Marco", "Terminal", None, false, false, None);
        assert!(!preparing.contains("remoteControl=1"));
        assert!(!preparing.contains("remoteControlDisallowed=1"));

        let allowed =
            header_query_string(7, "marco-id", "Marco", "Terminal", None, true, false, None);
        assert!(allowed.contains("remoteControl=1"));
        assert!(!allowed.contains("remoteControlDisallowed=1"));

        let denied =
            header_query_string(7, "marco-id", "Marco", "Terminal", None, false, true, None);
        assert!(denied.contains("remoteControlDisallowed=1"));
        assert!(!denied.contains("remoteControl=1"));
    }

    #[test]
    fn header_query_exports_same_border_treatment_as_parent_panel() {
        let query = header_query_string(7, "marco-id", "Marco", "Terminal", None, false, false, None);
        assert!(query.contains("ownerIdentity=marco%2Did"));
        // Border color keys on the owner IDENTITY (mirrors the header's
        // colorForIdentity(ownerIdentity || ownerName)), not the display name.
        let expected_border = format!(
            "borderColor=%23{}",
            &owner_border_color_hex("marco-id", "Marco", None)[1..]
        );
        assert!(query.contains(&expected_border));
        assert!(query.contains("borderStroke=4"));
        assert!(query.contains("borderRadius=10"));
        assert!(!query.contains("remoteControl=1"));
    }

    #[test]
    fn border_color_keys_on_identity_with_display_name_fallback() {
        // Deterministic regardless of the hash values: identity when present,
        // display name only when identity is empty/blank — exactly the header's
        // `ownerIdentity || ownerName` selection, so border == header tint.
        assert_eq!(
            owner_border_color_hex("bob-identity", "Bob", None),
            owner_color_hex("bob-identity")
        );
        assert_eq!(
            owner_border_color_hex("", "Bob", None),
            owner_color_hex("Bob")
        );
        assert_eq!(
            owner_border_color_hex("   ", "Bob", None),
            owner_color_hex("Bob")
        );
        assert_eq!(
            owner_border_color_hex("bob-identity", "Bob", Some(2)),
            OWNER_COLOR_PALETTE_HEX[2]
        );
    }

    #[test]
    fn header_query_exports_remote_control_only_when_available() {
        let query = header_query_string(7, "marco-id", "Marco", "Terminal", None, true, false, Some(3));
        assert!(query.contains("remoteControl=1"));
        assert!(query.contains("ownerPaletteIndex=3"));
    }

    #[test]
    fn control_route_exports_source_pixel_dimensions() {
        assert_eq!(
            control_route_url(7, "marco-id", 1920, 1080),
            "compositor/control.html?windowId=7&owner=marco%2Did&sourceWidth=1920&sourceHeight=1080"
        );
        assert_eq!(
            control_route_url(7, "marco-id", 0, 0),
            "compositor/control.html?windowId=7&owner=marco%2Did&sourceWidth=0&sourceHeight=0"
        );
    }

    #[test]
    fn control_dimension_updates_do_not_navigate_or_reset_overlay_state() {
        let script = control_source_dimensions_script(2560, 1440);

        assert!(script.contains("__petalPendingControlSourceDimensions"));
        assert!(script.contains("__petalRemoteControlSourceDimensions?.(2560, 1440)"));
        assert!(!script.contains("location"));
    }

    #[test]
    fn control_active_updates_survive_page_startup_races() {
        assert_eq!(
            remote_control_active_script(true),
            "window.__petalPendingRemoteControlActive = true; window.__petalRemoteControlSetActive?.(true);"
        );
        assert_eq!(
            remote_control_active_script(false),
            "window.__petalPendingRemoteControlActive = false; window.__petalRemoteControlSetActive?.(false);"
        );
    }

    #[test]
    fn surface_route_includes_open_url_only_for_openable_browser_urls() {
        // Border color keys on the owner IDENTITY ("ada-id"), so it stays
        // consistent with the header regardless of the display name.
        let border = &owner_border_color_hex("ada-id", "Ada Lovelace", None)[1..];

        let with_url = surface_route_url(
            42,
            "ada-id",
            "Ada Lovelace",
            "Example & Specs — Chrome",
            Some("https://example.com/spec?a=1&b=2"),
            true,
            false,
            None,
        );
        assert_eq!(
            with_url,
            format!("compositor/surface.html?windowId=42&owner=Ada%20Lovelace&title=Example%20%26%20Specs%20%E2%80%94%20Chrome&ownerIdentity=ada%2Did&borderColor=%23{border}&borderStroke=4&borderRadius=10&url=https%3A%2F%2Fexample%2Ecom%2Fspec%3Fa%3D1%26b%3D2&remoteControl=1")
        );

        let no_url = surface_route_url(42, "ada-id", "Ada", "Terminal", None, false, false, None);
        assert_eq!(
            no_url,
            format!("compositor/surface.html?windowId=42&owner=Ada&title=Terminal&ownerIdentity=ada%2Did&borderColor=%23{border}&borderStroke=4&borderRadius=10")
        );

        let rejected_url = surface_route_url(
            42,
            "ada-id",
            "Ada",
            "Finder",
            Some("file:///Users/example/Desktop/spec.pdf"),
            false,
            false,
            None,
        );
        assert_eq!(
            rejected_url,
            format!("compositor/surface.html?windowId=42&owner=Ada&title=Finder&ownerIdentity=ada%2Did&borderColor=%23{border}&borderStroke=4&borderRadius=10")
        );
    }

    #[test]
    fn owner_color_hash_matches_header_route_palette_for_ascii_owners() {
        assert_eq!(owner_color_hex("Someone"), "#d6b8f0");
        assert_eq!(owner_color_hex("Marco"), "#f06cc9");
        assert_eq!(owner_color_hex("webtest"), "#f06cc9");
    }

    #[test]
    fn owner_palette_matches_shared_contract_fixture() {
        let fixture = contract_fixture().identity_palette;
        assert_eq!(fixture.hash, "utf16-hash-times-31-mod-6");
        assert_eq!(
            fixture.names,
            vec!["plum", "blue", "green", "amber", "lilac", "slate"]
        );
        assert_eq!(
            fixture.hex,
            OWNER_COLOR_PALETTE_HEX
                .iter()
                .map(|hex| hex.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn owner_color_hash_uses_utf16_units_like_the_header_route() {
        // JS charCodeAt sees this as two UTF-16 code units. Mirroring
        // encode_utf16 keeps native border color aligned with the Svelte
        // header's deterministic owner-color fallback.
        assert_eq!(owner_color_hex("A😀"), "#f06cc9");
    }

    #[test]
    fn parses_identity_hex_colors_as_srgb_components() {
        assert_eq!(parse_hex_rgb("#000000"), Some((0.0, 0.0, 0.0)));
        assert_eq!(parse_hex_rgb("ffffff"), Some((1.0, 1.0, 1.0)));
        assert_eq!(
            parse_hex_rgb("#f06cc9"),
            Some((240.0 / 255.0, 108.0 / 255.0, 201.0 / 255.0))
        );
        assert_eq!(parse_hex_rgb("#fff"), None);
        assert_eq!(parse_hex_rgb("#zzzzzz"), None);
    }

    // #259/#264 display-sleep defensive fix: the pause/resume state machine
    // is pure and unit-testable without any real NSWorkspace notification,
    // AppKit call, or AVSampleBufferDisplayLayer -- see
    // `set_display_enqueue_paused`'s doc comment for the real caller
    // (resilience.rs's screensDidSleep/Wake observers).
    #[test]
    fn display_enqueue_gate_defaults_to_active() {
        assert!(!DisplayEnqueueGate::Active.is_paused());
    }

    #[test]
    fn display_enqueue_gate_sleep_transitions_to_paused() {
        let (next, transitioned) = DisplayEnqueueGate::Active.on_sleep();
        assert_eq!(next, DisplayEnqueueGate::Paused);
        assert!(transitioned, "Active -> sleep must be a real transition");
        assert!(next.is_paused());
    }

    #[test]
    fn display_enqueue_gate_duplicate_sleep_is_not_a_transition() {
        let (next, transitioned) = DisplayEnqueueGate::Paused.on_sleep();
        assert_eq!(next, DisplayEnqueueGate::Paused);
        assert!(
            !transitioned,
            "a second sleep notification while already paused must not re-transition"
        );
    }

    #[test]
    fn display_enqueue_gate_wake_transitions_to_active() {
        let (next, transitioned) = DisplayEnqueueGate::Paused.on_wake();
        assert_eq!(next, DisplayEnqueueGate::Active);
        assert!(transitioned, "Paused -> wake must be a real transition");
        assert!(!next.is_paused());
    }

    #[test]
    fn display_enqueue_gate_duplicate_wake_is_not_a_transition() {
        let (next, transitioned) = DisplayEnqueueGate::Active.on_wake();
        assert_eq!(next, DisplayEnqueueGate::Active);
        assert!(
            !transitioned,
            "a wake notification while already active must not re-transition"
        );
    }

    #[test]
    fn display_enqueue_gate_full_sleep_wake_cycle() {
        let mut gate = DisplayEnqueueGate::Active;
        for _ in 0..3 {
            let (next, transitioned) = gate.on_sleep();
            assert!(transitioned);
            gate = next;
            assert!(gate.is_paused());

            // A burst of duplicate sleep notifications (lid-open/unlock can
            // fire several) must not flip anything further.
            let (next, transitioned) = gate.on_sleep();
            assert!(!transitioned);
            gate = next;
            assert!(gate.is_paused());

            let (next, transitioned) = gate.on_wake();
            assert!(transitioned);
            gate = next;
            assert!(!gate.is_paused());
        }
    }

    /// #901: a newly revealed share must raise to the front, but #840's
    /// hide+re-reveal on every sharer republish (up to ~3x/second during
    /// #841's storm) must NOT turn into a raise storm.
    #[test]
    fn auto_raise_fires_on_first_reveal_and_debounces_republish_churn() {
        let t0 = std::time::Instant::now();
        assert!(
            auto_raise_on_reveal_due(None, t0),
            "a window never auto-raised must raise -- this is the whole point of #901"
        );
        // Republish churn: same window re-revealed immediately and repeatedly.
        for ms in [1u64, 50, 300, 900, 5_000] {
            assert!(
                !auto_raise_on_reveal_due(Some(t0), t0 + std::time::Duration::from_millis(ms)),
                "re-reveal {ms}ms later is republish churn (#840), not a new share"
            );
        }
    }

    #[test]
    fn auto_raise_fires_again_for_a_deliberate_reshare_after_the_debounce() {
        let t0 = std::time::Instant::now();
        assert!(
            !auto_raise_on_reveal_due(Some(t0), t0 + AUTO_RAISE_DEBOUNCE - std::time::Duration::from_millis(1)),
            "one millisecond short of the gap is still churn"
        );
        assert!(
            auto_raise_on_reveal_due(Some(t0), t0 + AUTO_RAISE_DEBOUNCE),
            "an unshare/re-share after the debounce is a NEW share and must come to the front"
        );
    }

    /// #878 Phase 2 item 2: the drop-rate backoff pause and the #259/#264
    /// sleep pause are independent flags OR'd at `display_enqueue_paused`.
    /// A backoff resume must never undo a real sleep pause, and vice versa
    /// each flag alone must still pause. Resets both flags at start/end so
    /// this test does not leak global state into others in the same binary.
    #[test]
    fn display_enqueue_backoff_never_overrides_a_sleep_pause() {
        set_display_enqueue_paused(false);
        set_display_enqueue_backoff_paused(false);
        assert!(!display_enqueue_paused());

        set_display_enqueue_paused(true); // screensDidSleep
        assert!(display_enqueue_paused());

        // A backoff resume must not un-pause a sleep-paused display.
        set_display_enqueue_backoff_paused(false);
        assert!(
            display_enqueue_paused(),
            "sleep pause must win over a backoff resume"
        );

        // Only clearing the sleep gate itself resumes.
        set_display_enqueue_paused(false);
        assert!(!display_enqueue_paused());

        // Backoff alone also pauses, independent of the sleep gate.
        set_display_enqueue_backoff_paused(true);
        assert!(display_enqueue_paused());
        set_display_enqueue_backoff_paused(false);
        assert!(!display_enqueue_paused());
    }

    #[test]
    fn wake_clears_a_backoff_pause_left_over_from_the_sleep_window() {
        // #878 adversarial-review finding 1 (wake half): a backoff pause
        // tripped during sleep must not survive screensDidWake, or every
        // remote window stays frozen up to the 30s failsafe after wake.
        set_display_enqueue_paused(true);
        set_display_enqueue_backoff_paused(true);
        set_display_enqueue_paused(false);
        assert!(
            !display_enqueue_backoff_paused(),
            "screensDidWake must clear the drop-rate backoff flag"
        );
    }

    /// #677: `create_chrome_webview` builds the control + pointer overlay
    /// child webviews (the only two callers, `create_control_overlay` and
    /// `create_pointer_overlay`). Tauri's `WebviewWindowBuilder` defaults to
    /// `focused: true`, and tao calls `makeKeyAndOrderFront` at BUILD time as
    /// a result -- so before this fix, every incoming remote share
    /// key-and-order-fronted two windows a moment before
    /// `hide_remote_window_chrome_on_main` hid them, stealing focus.
    ///
    /// This crate has no `tauri::test` mock-builder harness (see
    /// `autotest.rs`'s `dump_metrics_value` doc comment), and a real AppKit
    /// window build needs a live app/display this test suite doesn't have --
    /// so this asserts against the actual compiled source of
    /// `create_chrome_webview` itself, not a static string anywhere in the
    /// file: it isolates the function's own body (from its `fn` line to the
    /// next top-level `fn`) and fails if `.visible(false)`/`.focused(false)`
    /// are removed from THAT function specifically, which is what a revert
    /// of this change would do.
    #[test]
    fn create_chrome_webview_suppresses_build_time_activation() {
        let source = include_str!("compositor.rs");
        let start = source
            .find("fn create_chrome_webview(")
            .expect("create_chrome_webview must still exist in this file");
        let after_start = &source[start..];
        // The next top-level (column-0) `fn ` after the start of this
        // function's signature marks the following item -- this function's
        // body ends just before it. Skip past the opening of this function's
        // own signature line first so we don't match on itself.
        let body_end_offset = after_start[1..]
            .find("\nfn ")
            .expect("create_chrome_webview must be followed by another top-level item");
        let body = &after_start[..body_end_offset + 1];

        assert!(
            body.contains(".visible(false)"),
            "create_chrome_webview must build control/pointer overlay windows with \
             .visible(false) to avoid a build-time focus-steal flicker on every incoming \
             remote share (#677) -- got body:\n{body}"
        );
        assert!(
            body.contains(".focused(false)"),
            "create_chrome_webview must build control/pointer overlay windows with \
             .focused(false) to avoid tao's build-time makeKeyAndOrderFront (#677) -- \
             got body:\n{body}"
        );
    }
}
