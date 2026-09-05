//! Telepointers (SPEC.md §4.5) -- "seeing where someone is pointing is half
//! of what makes screen-sharing collaborative," explicitly called out as a
//! v1/P0 feature, not polish.
//!
//! ## What this module does
//!
//! - **Sender side:** while at least one window is being shared (per
//!   `session::SessionState`), polls the local cursor position at ~45Hz. For
//!   each currently-shared window, if the cursor is over that window's
//!   bounds, computes normalized (0-1) coordinates relative to the window's
//!   logical frame and publishes `{windowId, userId, x, y, visible: true}`
//!   over the room's LiveKit **data channel** (`Room::local_participant()
//!   .publish_data`, NOT the video track -- SPEC.md §4.5 is explicit that
//!   pointer position is metadata, never baked into pixels). If the cursor
//!   isn't over any shared window, sends `visible: false` for the
//!   most-recently-visible window instead of just stopping (see
//!   `PointerSender::run` doc comment for why).
//! - **Receiver side:** subscribes to the same room's `RoomEvent::DataReceived`
//!   and, for every telepointer-topic message, emits a `telepointer-update`
//!   Tauri event -- see `emit_pointer_update`'s doc comment for exactly which
//!   webview it targets and why.
//!
//! ## `userId` -- now the real per-user identity (retired stand-in)
//!
//! Previously a hardcoded `DEV_USER_ID` constant (same literal as
//! `session.rs`'s old `DEV_IDENTITY`). Both are retired as of the real
//! `join_room` flow (SPEC.md §4.6): the `userId` field of every pointer
//! message now comes from `SessionState::shared_windows_snapshot()`'s
//! identity value, which is the real identity `session::join_room` connected
//! under (itself sourced from the frontend's onboarding identity store) --
//! exactly the collapse this module's prior doc comment said should happen
//! once a real per-user identity existed, not a second, still-separate
//! stand-in. Receivers still do not trust this payload field for attribution:
//! they overwrite it with the authenticated LiveKit sender identity before
//! emitting/rendering any cursor or activity state (issue #95).
//!
//! ## Coordinate model (SPEC.md §4.5)
//!
//! Normalized to the source window's *current* logical bounds
//! (`ActiveShare::frame`, see `session.rs`) -- 0,0 = window's top-left
//! logical point, 1,1 = bottom-right. Those bounds are seeded at share-start
//! and kept fresh by this module's own loop (issue #30): every
//! [`FRAME_REFRESH_TICKS`]th tick (~9Hz, matching `share_border.rs`'s 10Hz
//! move tracker and the issue's ~200ms acceptance budget) takes ONE
//! `CGWindowList` snapshot (`platform::cg::onscreen_stack`, reused
//! read-only -- one enumeration covers every shared window, unlike a
//! per-window `platform::cg::frame_for_window_id` scan), diffs it against the
//! known frames ([`frames_to_apply`], pure + unit-tested), and writes real
//! changes back via `SessionState::update_share_frames` -- so
//! `ActiveShare.frame` stays the single source of truth for hit-testing and
//! telepointers keep landing on the right spot after a move/resize.
//! Refreshing here (the consumer) rather than hooking `share_border`'s
//! tracker was deliberate: that tracker only enumerates while border panels
//! exist (`panel_number != 0`), so a share whose border failed to build
//! would never refresh, and it keeps session-state writes out of
//! `share_border.rs` entirely. Cursor positions outside those bounds
//! are clamped to the nearest edge before being marked invisible, so a
//! receiver that gets one stale "visible: true, x: 1.03" frame during a fast
//! mouse exit doesn't draw a pointer floating off the video layer.
//!
//! ## Why a separate, lighter poll loop instead of reusing `hover_tab`'s
//!
//! `hover_tab.rs` already runs a ~60Hz loop, but it does a full
//! `CGWindowListCopyWindowInfo` enumeration + hit-test against EVERY on-screen
//! window every tick, for a different purpose (finding whatever window is
//! currently hovered, to position the share/unshare pill). Telepointers only
//! need the cursor position tested against the small, already-known set of
//! *currently-shared* windows (typically 0-4), which is a plain arithmetic
//! bounds check against `SessionState::shared_windows_snapshot()` -- no
//! window-list enumeration at all. Piggybacking on `hover_tab`'s loop would
//! mean paying its enumeration cost even when nothing is shared, and would
//! couple two independent features' cadence/lifecycle together for no
//! reason. The two loops DO share the same underlying cursor-position
//! primitive (`platform::cg::cursor_position`)
//! so there's exactly one raw `CGEventCreate` FFI call site, not two.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{AppHandle, Manager};

use crate::platform::cg::WindowFrame;
use crate::session::{RoomGeneration, SessionState};

/// Poll rate for the sender-side cursor tracker. SPEC.md §4.5: "~30-60Hz."
/// 45Hz splits the difference -- smooth enough for the receiver's 60ms CSS
/// interpolation (`Pointer.svelte`) to glide rather than step, without
/// polling meaningfully harder than `hover_tab.rs`'s existing 16ms/~60Hz loop
/// for a per-share (not per-window-enumeration) workload.
const POLL_MS: u64 = 22; // ~45Hz

/// Refresh shared-window frames every Nth sender-loop tick (issue #30).
/// 5 ticks x 22ms = ~110ms (~9Hz) -- inside the issue's ~200ms acceptance
/// budget and the same order as `share_border.rs`'s 10Hz tracker, without
/// paying a full `CGWindowList` enumeration at the cursor poll's 45Hz.
const FRAME_REFRESH_TICKS: u64 = 5;

/// Sender-side deadband for shared-window frame updates (#762). Windows
/// `GetWindowRect` can report a 1-2px rect wiggle on a ~9Hz cadence even for
/// a perfectly stationary window; because the frame is both the telepointer
/// normalization basis and the WGC capture anchor, that jitter makes the tag
/// AND the captured content bob. Frame changes within this radius (per
/// dimension) are treated as no-op so micro-jitter is invisible while genuine
/// moves/resizes still track. Slightly larger than the observed ~2px jitter.
const FRAME_JITTER_TOLERANCE_PX: i32 = 3;

/// True when `fresh` differs from `stored` by no more than `tolerance` px in
/// any dimension — i.e. a sub-tolerance GetWindowRect wiggle, not a real
/// move/resize. Changes of exactly `tolerance` px are also suppressed (`<=`).
fn within_frame_tolerance(stored: WindowFrame, fresh: WindowFrame, tolerance: i32) -> bool {
    (fresh.x - stored.x).abs() <= tolerance
        && (fresh.y - stored.y).abs() <= tolerance
        && (fresh.width as i32 - stored.width as i32).abs() <= tolerance
        && (fresh.height as i32 - stored.height as i32).abs() <= tolerance
}

/// LiveKit data-channel topic for telepointer messages, so a receiver can
/// filter `RoomEvent::DataReceived` to just these (vs. some future chat/RPC
/// use of the same data channel) without a second connection or channel.
const TOPIC: &str = "petal.telepointer";

/// Optional high-level activity riding on a telepointer update. Kept tiny and
/// stringly on the wire so older clients simply ignore the absent field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PointerActivity {
    Click,
    Type,
}

/// Wire shape for one telepointer update -- SPEC.md §4.5's
/// `{windowId, userId, x, y, visible}`, serialized as JSON for the data
/// packet payload. JSON (not a hand-rolled binary format) because payloads
/// are tiny (well under LiveKit's data-channel size limits) and this keeps
/// the wire format trivially inspectable while iterating -- worth
/// revisiting for a compact binary encoding only if the 30-60Hz rate is
/// ever shown to matter for bandwidth (SPEC.md's own priority ladder, §4.3,
/// puts this metadata channel far below the video/audio budget anyway).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PointerMessage {
    pub window_id: u32,
    pub user_id: String,
    pub x: f64,
    pub y: f64,
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<PointerActivity>,
    /// Optional owner (sharer) identity of the shared surface the cursor is
    /// over. Lets a receiver route the cursor to exactly one shared surface
    /// when per-machine window tokens collide across sharers (Windows). Old
    /// clients and macOS omit it, so receivers fall back to treating the
    /// sender as the owner (legacy single-sharer behavior).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "surfaceOwnerId"
    )]
    pub surface_owner_id: Option<String>,
}

/// Payload for the `telepointer-update` Tauri event -- the receiver-side
/// hop from "data channel message arrived" to "frontend can render it."
/// Kept separate from `PointerMessage` because the sender-side wire type and
/// the frontend-facing event type may differ. `display_name` is receiver-only
/// metadata resolved from LiveKit's participant roster; it is not part of the
/// data-channel protocol.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelepointerUpdate {
    pub window_id: u32,
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palette_index: Option<u8>,
    pub x: f64,
    pub y: f64,
    pub visible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<PointerActivity>,
}

impl From<PointerMessage> for TelepointerUpdate {
    fn from(m: PointerMessage) -> Self {
        Self::from_message(m, None, None)
    }
}

impl TelepointerUpdate {
    fn from_message(
        m: PointerMessage,
        display_name: Option<String>,
        palette_index: Option<u8>,
    ) -> Self {
        Self {
            window_id: m.window_id,
            user_id: m.user_id,
            surface_owner_id: m.surface_owner_id,
            display_name,
            palette_index,
            x: m.x,
            y: m.y,
            visible: m.visible,
            activity: m.activity,
        }
    }
}

fn update_for_authenticated_sender(
    mut message: PointerMessage,
    sender_identity: Option<String>,
    display_name: Option<String>,
    palette_index: Option<u8>,
) -> Option<TelepointerUpdate> {
    let sender_identity = sender_identity?;
    let sender_identity = sender_identity.trim();
    if sender_identity.is_empty() {
        return None;
    }
    // Security #95: payload userId is display metadata at most for the
    // continuous raw position stream -- receiver-side identity, keying,
    // color, and activity attribution must come from the authenticated
    // LiveKit sender identity, since any participant could otherwise claim
    // to BE someone else's cursor.
    //
    // Regression follow-up (found via live testing 2026-07-14): that blanket
    // overwrite also broke `publish_control_activity`'s relay path -- the
    // window OWNER publishes a click/type activity marker on behalf of a
    // remote controller it has already authorized (the RC grant check
    // already happened before this packet was ever sent), so the payload's
    // `user_id` there is the controller, not the owner. Overwriting it
    // unconditionally attributed every remote click/type flash to the
    // sharer instead of the person actually clicking. Trust the payload's
    // user_id ONLY for activity-bearing messages, and ONLY when the
    // authenticated sender is verifiably the window's own owner (so an
    // unrelated participant still can't forge someone else's activity
    // marker for a window they don't share).
    let trust_payload_identity = message.activity.is_some() && {
        // macOS: the compositor knows which windows it owns.
        // Windows: the compositor also tracks owner identity, but this path
        // does not consult it yet; it fails closed and keeps the authenticated
        // sender identity rather than trusting the payload's activity identity.
        #[cfg(target_os = "macos")]
        {
            crate::compositor::owner_identity_for_window(message.window_id, None)
                .is_some_and(|owner| owner == sender_identity)
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    };
    if !trust_payload_identity {
        message.user_id = sender_identity.to_string();
    }
    Some(TelepointerUpdate::from_message(
        message,
        display_name,
        palette_index,
    ))
}

/// Compute normalized (0-1) coordinates of `cursor` (global logical points)
/// within `frame` (a shared window's global logical frame), plus whether the
/// cursor is actually inside those bounds. Pure function, unit-tested below
/// without needing a real window/cursor.
fn normalize(cursor: (f64, f64), frame: &WindowFrame) -> (f64, f64, bool) {
    let (cx, cy) = cursor;
    let (fx, fy, fw, fh) = (
        frame.x as f64,
        frame.y as f64,
        frame.width.max(1) as f64,
        frame.height.max(1) as f64,
    );
    let inside = cx >= fx && cx < fx + fw && cy >= fy && cy < fy + fh;
    // Clamp even when outside, so a caller that (for whatever reason) still
    // wants to send a last-known position doesn't emit wildly out-of-[0,1]
    // values -- see module doc comment on why hidden/edge-clamped is safer
    // than "just don't send" for the receiver's last-frame handling.
    let nx = ((cx - fx) / fw).clamp(0.0, 1.0);
    let ny = ((cy - fy) / fh).clamp(0.0, 1.0);
    (nx, ny, inside)
}

/// Decide which shared windows' frames actually changed, given the current
/// on-screen stack (issue #30). `shared` is `(window_id, last_known_frame)`
/// (from `shared_windows_snapshot`); `stack` is `(window_number, frame)` in
/// front-to-back order (from `platform::cg::onscreen_stack` -- first
/// match by id wins, though CGWindowIDs are unique so order is moot).
/// Returns only real changes, so the caller can skip the session-state lock
/// entirely on the (overwhelmingly common) no-movement tick. A shared id
/// absent from the stack (window minimized / on another Space / just closed)
/// produces no entry -- its last-known frame is retained (see
/// `ActiveShare::frame`'s doc comment). Pure function, unit-tested below.
/// Which shared windows count as "visible on screen" this tick.
///
/// Presence in the `CGWindowList` on-screen snapshot IS the visibility signal:
/// a shared id absent from the stack is off-screen (minimized, other Space, or
/// closed -- `session::share` disambiguates the last case). Extracted as a seam
/// (#742) because this derivation decides whether a share is reported
/// off-screen, and it previously lived inline in the sender loop with no test
/// of any kind. PINNED by `visible_window_ids_*` tests; a replacement window
/// source must reproduce it or change it knowingly.
fn visible_window_ids(shared: &[(u32, WindowFrame)], stack: &[(i64, WindowFrame)]) -> Vec<u32> {
    shared
        .iter()
        .filter_map(|(id, _)| {
            stack
                .iter()
                .any(|(window_number, _)| *window_number == *id as i64)
                .then_some(*id)
        })
        .collect()
}

/// #875: the currently-SHARED subset of `stack`'s front-to-back order,
/// preserving the STACK's ordering (not `shared`'s) -- this is exactly what
/// gets published as `petalWindowZOrder`. A shared window absent from the
/// stack (e.g. momentarily off-screen) is simply omitted, matching
/// `visible_window_ids`'s presence-only semantics above.
fn shared_window_z_order(shared: &[(u32, WindowFrame)], stack: &[(i64, WindowFrame)]) -> Vec<u32> {
    stack
        .iter()
        .filter_map(|&(id, _)| {
            shared
                .iter()
                .any(|(window_id, _)| i64::from(*window_id) == id)
                .then_some(id as u32)
        })
        .collect()
}

fn frames_to_apply(
    shared: &[(u32, WindowFrame)],
    stack: &[(i64, WindowFrame)],
) -> Vec<(u32, WindowFrame)> {
    shared
        .iter()
        .filter_map(|&(id, known)| {
            let current = stack.iter().find(|&&(n, _)| n == id as i64)?.1;
            (current != known).then_some((id, current))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PointerTargetKind {
    LocalShare,
    RemoteCompositor,
}

impl PointerTargetKind {
    fn label(self) -> &'static str {
        match self {
            Self::LocalShare => "local-share",
            Self::RemoteCompositor => "remote-compositor",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PointerTarget {
    kind: PointerTargetKind,
    window_id: u32,
    frame: WindowFrame,
    surface_owner_id: Option<String>,
    display_like: bool,
    /// #906: native WindowServer ids for this target's panel + overlay-child
    /// family (see `compositor::PointerFamilyMeta`). Empty for `LocalShare`
    /// targets, which aren't gated on occlusion by this issue's fix -- only
    /// `RemoteCompositor` targets are receiver-rendered native windows that
    /// another window on the VIEWER's own screen can cover.
    panel_family_ids: Vec<u32>,
    /// #906: whether the remote panel is currently visible (not hidden / not
    /// yet revealed). Always `true` for `LocalShare` targets, which this
    /// field does not gate.
    is_visible: bool,
}

fn pointer_targets(
    local_frames: &[(u32, WindowFrame)],
    remote_content_frames: &[(u32, WindowFrame)],
) -> Vec<PointerTarget> {
    local_frames
        .iter()
        .map(|(window_id, frame)| PointerTarget {
            kind: PointerTargetKind::LocalShare,
            window_id: *window_id,
            frame: *frame,
            surface_owner_id: None,
            display_like: false,
            panel_family_ids: Vec::new(),
            is_visible: true,
        })
        .chain(
            remote_content_frames
                .iter()
                .map(|(window_id, frame)| PointerTarget {
                    kind: PointerTargetKind::RemoteCompositor,
                    window_id: *window_id,
                    frame: *frame,
                    surface_owner_id: None,
                    display_like: false,
                    panel_family_ids: Vec::new(),
                    is_visible: true,
                }),
        )
        .collect()
}

// macOS-only: the signature names `compositor::PointerFamilyMeta`, and the
// `compositor` module itself is `#[cfg(target_os = "macos")]` (lib.rs:41).
// Windows has its own occlusion-aware path via `root_hwnds` in
// `select_windows_pointer_target`, so it never needs this.
#[cfg(target_os = "macos")]
fn pointer_targets_with_owners(
    local_frames: &[(u32, WindowFrame)],
    remote_content_frames: &[(u32, WindowFrame, String)],
    remote_family_meta: &[crate::compositor::PointerFamilyMeta],
    local_owner: &str,
) -> Vec<PointerTarget> {
    local_frames
        .iter()
        .map(|(window_id, frame)| PointerTarget {
            kind: PointerTargetKind::LocalShare,
            window_id: *window_id,
            frame: *frame,
            surface_owner_id: Some(local_owner.to_string()),
            display_like: crate::region_window::resolve(*window_id).is_some(),
            panel_family_ids: Vec::new(),
            is_visible: true,
        })
        .chain(remote_content_frames.iter().map(|(window_id, frame, owner)| {
            let meta = remote_family_meta
                .iter()
                .find(|meta| meta.window_id == *window_id && meta.owner_identity == *owner);
            PointerTarget {
                kind: PointerTargetKind::RemoteCompositor,
                window_id: *window_id,
                frame: *frame,
                surface_owner_id: Some(owner.clone()),
                display_like: false,
                panel_family_ids: meta.map(|meta| meta.family_ids.clone()).unwrap_or_default(),
                // No meta yet (e.g. the ~9Hz refresh hasn't run since this
                // window opened) fails CLOSED: never claim a panel is visible
                // without having actually observed it (#906 DoD).
                is_visible: meta.is_some_and(|meta| meta.is_visible),
            }
        }))
        .collect()
}

/// #906: resolve the real topmost WindowServer window id under the cursor.
/// Primary: `sls_hit`, the caller's `platform::sls::find_window_at` result
/// (off-main-thread, ~7.6us -- safe at the sender's 45Hz cadence). Fallback:
/// a pure front-to-back geometry walk over `registry_records` (as produced by
/// `window_registry::Snapshot::records_front_to_back`, ALREADY in front-to-
/// back z-order), used only when SLS is unavailable (private API missing on
/// a future macOS, or `PETAL_DISABLE_SLS=1`).
///
/// Deliberately does NOT filter by absolute `layer` (an earlier revision did,
/// and it was a real bug -- adversarial review, #906 follow-up): a layer !=0
/// window (a floating PiP player, Spotlight, a popover, even the menu bar)
/// can be every bit as visually occluding as a normal layer-0 window, so
/// skipping it and searching *behind* it for a layer-0 hit reintroduces the
/// exact bug this issue fixes -- the walk would report the buried panel as
/// "topmost" while a real window sits on top of it. The question this
/// function answers is purely "what is the FIRST thing in front-to-back
/// order whose frame contains the cursor" -- i.e. is anything at all above
/// the candidate at this point -- not "what is the first NORMAL window."
/// `records_front_to_back`'s order already encodes the real on-screen stack,
/// so the first frame-containing record, of any layer, IS the answer.
///
/// Explicit fail-CLOSED decision (Definition of Done): if `sls_hit` is `None`
/// AND no registry record's frame contains the cursor either (empty/stale
/// snapshot, or the registry isn't running), this returns `None`. A caller
/// that cannot prove ANY window -- let alone the remote panel -- is on top of
/// the cursor must never treat that as "nothing is occluding it"; `None`
/// means "hide," never "show."
fn resolve_topmost_window_id(
    cursor: (f64, f64),
    sls_hit: Option<u32>,
    registry_records: &[(u32, WindowFrame)],
) -> Option<u32> {
    sls_hit.or_else(|| {
        registry_records
            .iter()
            .find(|&&(_, frame)| normalize(cursor, &frame).2)
            .map(|&(wid, _)| wid)
    })
}

/// #906 finding 2 (adversarial review): which currently-tracked
/// `(kind, window_id, owner)` keys must receive a falling-edge
/// `visible: false` because they've disappeared from `targets` ENTIRELY
/// since the last tick, while `last_visible` still says they were last
/// published as visible. This is the case the per-target loop structurally
/// cannot reach: `for target in &targets` only ever visits keys CURRENTLY
/// present, so a retired remote panel that's dropped from
/// `compositor`'s `s.windows` (and therefore vanishes from
/// `open_content_frames_with_owners`/`targets`) before its async hide runs
/// is never visited again -- no falling edge is ever sent, and the sharer
/// holds a ghost pointer until the receiver's own idle-timeout. Mirrors
/// Windows' `windows_visibility_decisions`'s second `.extend(...)` clause
/// exactly (see also its `disappearing_selected_surface_gets_a_prompt_hide`
/// fixture). Pure + unit-tested.
fn vanished_visible_keys(
    targets: &[PointerTarget],
    last_visible: &HashMap<(PointerTargetKind, u32, Option<String>), bool>,
) -> Vec<(PointerTargetKind, u32, Option<String>)> {
    last_visible
        .iter()
        .filter(|(key, visible)| {
            **visible
                && !targets.iter().any(|target| {
                    (target.kind, target.window_id, target.surface_owner_id.clone()) == **key
                })
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// Publish the falling-edge `visible: false` for one `(kind, window_id,
/// owner)` key that has disappeared from the target set entirely (#906
/// finding 2) -- shared by both call sites below so the `PointerMessage`
/// shape can't drift between them.
fn publish_vanished_hide(
    publisher: &Arc<crate::transport::RoomConnection>,
    user_id: &str,
    key: &(PointerTargetKind, u32, Option<String>),
) {
    log::info!(
        "telepointer: local user '{user_id}' {} window {} vanished from the \
         target set (e.g. a retired panel) -- forcing a hide",
        key.0.label(),
        key.1
    );
    publish_pointer(
        publisher,
        PointerMessage {
            window_id: key.1,
            user_id: user_id.to_string(),
            x: 0.5,
            y: 0.5,
            visible: false,
            activity: None,
            surface_owner_id: key.2.clone(),
        },
    );
}

fn select_macos_pointer_target<'a>(
    cursor: (f64, f64),
    topmost_window_id: Option<u32>,
    targets: &'a [PointerTarget],
) -> Option<&'a PointerTarget> {
    let inside = |target: &&PointerTarget| normalize(cursor, &target.frame).2;
    // #906: a remote compositor panel only wins when the cursor's REAL
    // topmost window (see `resolve_topmost_window_id`) is a member of that
    // panel's own family (panel + its pointer/control/ai-chat overlay
    // children, `PointerTarget::panel_family_ids`) -- replacing the old
    // "remote always wins on bare frame containment" rule, which never
    // noticed a viewer's own window covering the panel (#906). A hidden or
    // not-yet-revealed panel (`is_visible == false`) can never win either,
    // even if its stale frame still contains the cursor.
    //
    // A Petal View region is display-like and must win over an underlying
    // LOCAL shared window; otherwise one cursor position would publish
    // multiple visible pointers. This branch is unchanged by #906 -- local
    // shares are the sharer's OWN windows on their OWN screen, out of this
    // issue's scope.
    targets
        .iter()
        .filter(|target| target.kind == PointerTargetKind::RemoteCompositor)
        .find(|target| {
            inside(target)
                && target.is_visible
                && topmost_window_id.is_some_and(|wid| target.panel_family_ids.contains(&wid))
        })
        .or_else(|| targets.iter().filter(|target| target.display_like).find(inside))
        // #906: this final fallback must NEVER re-match a `RemoteCompositor`
        // target -- it exists only to pick a plain LOCAL share when nothing
        // else matched, not to undo the occlusion gate above. Without this
        // exclusion, a remote target that FAILED the gate (occluded / hidden
        // / unknown topmost id) but is still geometrically "inside" its
        // stale frame would be re-selected right back here, silently
        // reintroducing the bug this issue fixes.
        .or_else(|| {
            targets
                .iter()
                .filter(|target| target.kind != PointerTargetKind::RemoteCompositor)
                .find(inside)
        })
}

fn overlay_delivery_labels(
    mut remote_pointer_labels: Vec<String>,
    sharer_overlay_labels: Vec<String>,
) -> Vec<String> {
    remote_pointer_labels.extend(sharer_overlay_labels);
    remote_pointer_labels.sort();
    remote_pointer_labels.dedup();
    remote_pointer_labels
}

static SENDER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Idempotent start of the sender-side cursor-poll thread. Safe to call
/// unconditionally (e.g. once at app launch, like `hover_tab::start`) --
/// the loop itself is cheap when nothing is shared (empty snapshot, tight
/// sleep, no publish calls) and `session::SessionState` is the source of
/// truth for what's actually shared, so this doesn't need its own
/// start/stop lifecycle tied to individual share toggles.
pub fn start_sender(app: &AppHandle) {
    if SENDER_ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    #[cfg(target_os = "macos")]
    std::thread::spawn(move || sender_loop(app));
    #[cfg(not(target_os = "macos"))]
    std::thread::spawn(move || sender_loop_windows(app));
}

/// The sender-side poll loop (see module doc comment for rate/rationale).
/// Runs on its own OS thread (mirrors `hover_tab`'s loop) because it needs a
/// tight, uninterrupted sleep cadence independent of the async runtime's
/// scheduling, but publishing over the data channel is itself async
/// (`LocalParticipant::publish_data`) -- so each tick hands off to a small
/// blocking `tokio` call via the app's already-running runtime handle
/// (Tauri's async commands already run on one; grabbing `tauri::async_runtime::spawn`
/// per-tick keeps this thread from needing its own executor).
#[cfg(target_os = "macos")]
fn sender_loop(app: AppHandle) {
    use tauri::Manager;

    // Tracks, per window_id, whether the LAST message sent for it was
    // visible=true -- so that the one tick where the cursor leaves a shared
    // window's bounds sends exactly one visible=false message (letting the
    // receiver hide the pointer promptly) rather than either (a) going
    // silent forever with no explicit "gone" signal, which would leave a
    // stale pointer glued to the last position until the receiver's own
    // idle-timeout eventually fades it, or (b) spamming visible=false on
    // every subsequent tick for a window the cursor left minutes ago.
    let mut last_visible: HashMap<(PointerTargetKind, u32, Option<String>), bool> = HashMap::new();
    // Tick counter for the ~9Hz frame refresh (issue #30) -- see the module
    // doc comment's "Coordinate model" section for the full rationale.
    let mut tick: u64 = 0;
    // #906: cached per-remote-window occlusion metadata (panel + overlay
    // native ids, visibility), refreshed at the same ~9Hz cadence as the
    // frame refresh below via ONE main-thread round trip
    // (`compositor::open_pointer_family_meta`) -- never per-tick at 45Hz, and
    // deliberately NOT gated on `!frames.is_empty()` like the block below: a
    // viewer with nothing of their own shared must still get this refreshed,
    // or their remote-panel occlusion gate would never update (the exact gap
    // this issue's report exercises -- Adam wasn't sharing anything himself).
    let mut remote_family_meta: Vec<crate::compositor::PointerFamilyMeta> = Vec::new();
    // #906 finding 3 (adversarial review, P2): whether the LAST refresh
    // attempt above failed to complete (main-thread scheduling error, or the
    // 150ms deadline passed while the main thread was busy -- a window drag
    // is enough). Used only to log the failure/recovery EDGE, not every
    // failed tick (#905: no hot per-tick log lines).
    let mut family_meta_refresh_degraded = false;

    loop {
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        tick = tick.wrapping_add(1);

        if tick % FRAME_REFRESH_TICKS == 0 {
            match crate::compositor::open_pointer_family_meta(&app) {
                Some(meta) => {
                    if family_meta_refresh_degraded {
                        log::info!(
                            "telepointer: remote-panel occlusion metadata refresh recovered"
                        );
                        family_meta_refresh_degraded = false;
                    }
                    remote_family_meta = meta;
                }
                None => {
                    // #906 finding 3: preserve the last known-good cache
                    // rather than overwriting it with empty -- an empty
                    // cache fails every remote target closed (hidden), so a
                    // transient main-thread stall must not silently disable
                    // every remote telepointer until the next successful
                    // refresh. `remote_family_meta` is deliberately left
                    // untouched here.
                    if !family_meta_refresh_degraded {
                        log::warn!(
                            "telepointer: remote-panel occlusion metadata refresh failed \
                             (main-thread scheduling error or >150ms stall) -- keeping the \
                             last known-good cache ({} entries) instead of hiding every \
                             remote target",
                            remote_family_meta.len()
                        );
                        family_meta_refresh_degraded = true;
                    }
                }
            }
        }

        let Some(state) = app.try_state::<SessionState>() else {
            continue;
        };
        let (publisher, identity, mut frames) = state.shared_windows_snapshot();

        // Issue #30: periodically re-read every shared window's real
        // on-screen frame so hit-testing doesn't go stale after a
        // move/resize. One CGWindowList snapshot for all shares (thread-safe
        // off the main thread -- same as `share_border`'s tracker, no AppKit
        // involved); the session lock is only taken when something actually
        // moved. The local `frames` copy is patched too so THIS tick already
        // hit-tests against the fresh bounds.
        if !frames.is_empty() && tick % FRAME_REFRESH_TICKS == 0 {
            // #744: read the shared window-registry snapshot instead of a
            // dedicated CGWindowList enumeration. The registry_snapshot_
            // reproduces_the_telepointer_stack test proves this stack is
            // byte-identical to the old onscreen_stack() for the fixtures.
            if let Some(stack) = app
                .try_state::<crate::window_registry::WindowRegistry>()
                .map(|reg| {
                    reg.snapshot()
                        .records_front_to_back()
                        .map(|r| (r.wid as i64, r.frame))
                        .collect::<Vec<_>>()
                })
                .filter(|s| !s.is_empty())
            {
                let changed = frames_to_apply(&frames, &stack);
                let visible_window_ids = visible_window_ids(&frames, &stack);
                state.update_share_frames_and_visibility(&changed, &visible_window_ids);
                for (id, fresh) in &changed {
                    if let Some(slot) = frames.iter_mut().find(|(fid, _)| fid == id) {
                        slot.1 = *fresh;
                    }
                }

                // #875: piggyback the same ~9Hz registry read to keep
                // `petalWindowZOrder` current. `set_shared_window_order`
                // internally republishes only when the shared subset's
                // front-to-back order actually changed, so a tick where
                // nothing moved (or where only an UNSHARED window reshuffled
                // elsewhere in `stack`) costs a cheap comparison, not a
                // `set_metadata` round trip.
                if let Some(room_connection) = publisher.clone() {
                    let z_order = shared_window_z_order(&frames, &stack);
                    tauri::async_runtime::spawn(async move {
                        room_connection.set_shared_window_order(z_order).await;
                    });
                }
            }
        }
        let Some(publisher) = publisher else {
            // No room connection at all (`SessionState::joined` is `None` --
            // we've left the call, not merely "nothing shared right now").
            // #906 finding 2 considered whether this needs the same
            // vanished-key hide pass as the branches below: it doesn't,
            // because there is no channel to publish on (`publisher` IS the
            // `Arc<RoomConnection>` those need) -- and there's no receiver to
            // tell, either. Leaving the room already fully disconnects this
            // participant from LiveKit, which is what makes every remote
            // share we published disappear on every receiver's side (the
            // `ParticipantDisconnected` handling fixed by #631) -- a stale
            // `last_visible` entry here can never reach a receiver because
            // there is nothing left to publish it to. Clear tracked
            // visibility so a fresh share later starts clean.
            last_visible.clear();
            continue;
        };
        // Real per-user identity (SPEC.md's onboarding identity, threaded
        // through `session::join_room`) -- retires this module's own
        // `DEV_USER_ID` stand-in. Falls back to the retired literal only in
        // the practically-unreachable case of a room connection existing
        // with no recorded join identity (shouldn't happen: `join_room`
        // always sets both together), so a bug here still produces a
        // labeled pointer instead of an empty user_id.
        let user_id = identity.unwrap_or_else(|| "unknown".to_string());
        let remote_frames = crate::compositor::open_content_frames_with_owners(&app);
        let targets =
            pointer_targets_with_owners(&frames, &remote_frames, &remote_family_meta, &user_id);
        if targets.is_empty() {
            // #906 finding 2: every previously-visible key just vanished at
            // once (e.g. the last remaining share/receive stopped) -- each
            // one still needs its falling-edge hide; a bare `clear()` here
            // silently drops that.
            for key in vanished_visible_keys(&[], &last_visible) {
                publish_vanished_hide(&publisher, &user_id, &key);
            }
            last_visible.clear();
            continue;
        }

        let cursor = crate::platform::cg::cursor_position();
        // #906: the real topmost-window occlusion gate (mirrors
        // `select_windows_pointer_target`'s `WindowFromPoint` gate). Primary:
        // `platform::sls::find_window_at`, off-main-thread and ~7.6us, so
        // this runs every tick at the full 45Hz cadence with no new
        // main-thread dependency. Fallback (SLS unavailable): a pure
        // front-to-back walk of the already-resident window-registry
        // snapshot -- see `resolve_topmost_window_id`'s doc comment for the
        // explicit fail-closed decision when neither source can answer.
        let topmost_window_id = cursor.and_then(|(cx, cy)| {
            let sls_hit = crate::platform::sls::find_window_at(cx, cy).map(|(wid, _)| wid);
            // No `layer` filtering here -- see `resolve_topmost_window_id`'s
            // doc comment (#906 adversarial-review follow-up): the
            // front-to-back ORDER already answers "what's on top," and a
            // layer-based filter would let a real, visually occluding
            // layer!=0 window (a floating PiP, Spotlight, the menu bar) get
            // skipped over in the walk.
            let registry_records: Vec<(u32, WindowFrame)> = app
                .try_state::<crate::window_registry::WindowRegistry>()
                .map(|reg| {
                    reg.snapshot()
                        .records_front_to_back()
                        .map(|r| (r.wid, r.frame))
                        .collect()
                })
                .unwrap_or_default();
            resolve_topmost_window_id((cx, cy), sls_hit, &registry_records)
        });
        let selected = cursor
            .and_then(|cursor| select_macos_pointer_target(cursor, topmost_window_id, &targets));

        for target in &targets {
            let (x, y, inside) = match cursor {
                Some(c) => {
                    let (x, y, within) = normalize(c, &target.frame);
                    let selected = selected.is_some_and(|selected| {
                        selected.kind == target.kind
                            && selected.window_id == target.window_id
                            && selected.surface_owner_id == target.surface_owner_id
                    });
                    (x, y, within && selected)
                }
                None => (0.5, 0.5, false),
            };

            let key = (target.kind, target.window_id, target.surface_owner_id.clone());
            let was_visible = last_visible.get(&key).copied().unwrap_or(false);
            if !inside && !was_visible {
                // Already told the receiver this pointer is hidden; nothing
                // new to say for this window this tick.
                continue;
            }
            last_visible.insert(key, inside);
            if inside && !was_visible {
                log::info!(
                    "telepointer: local user '{user_id}' entered {} window {} frame=({}, {}, {}, {})",
                    target.kind.label(),
                    target.window_id,
                    target.frame.x,
                    target.frame.y,
                    target.frame.width,
                    target.frame.height
                );
            } else if !inside && was_visible {
                log::info!(
                    "telepointer: local user '{user_id}' left {} window {} frame=({}, {}, {}, {})",
                    target.kind.label(),
                    target.window_id,
                    target.frame.x,
                    target.frame.y,
                    target.frame.width,
                    target.frame.height
                );
            }

            let message = PointerMessage {
                window_id: target.window_id,
                user_id: user_id.clone(),
                x,
                y,
                visible: inside,
                activity: None,
                surface_owner_id: target.surface_owner_id.clone(),
            };
            publish_pointer(&publisher, message);
        }

        // #906 finding 2: the loop above only ever visits keys CURRENTLY in
        // `targets`, so a key that disappeared from the target set entirely
        // between ticks (a retired remote panel dropped from
        // `compositor`'s `s.windows` before its async hide runs, or a local
        // share that just stopped) is never revisited -- without this pass
        // its last-known `visible: true` in `last_visible` would never get a
        // falling-edge hide, and the sharer keeps the viewer's ghost pointer
        // until the receiver's own idle-timeout.
        for key in vanished_visible_keys(&targets, &last_visible) {
            publish_vanished_hide(&publisher, &user_id, &key);
            last_visible.insert(key, false);
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WindowsPointerKey {
    kind: PointerTargetKind,
    window_id: u32,
    owner_identity: String,
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, PartialEq)]
struct WindowsPointerTarget {
    key: WindowsPointerKey,
    frame: WindowFrame,
    target_kind: crate::windows_capture_target::TargetKind,
    root_hwnds: Vec<isize>,
}

#[cfg(not(target_os = "macos"))]
fn select_windows_pointer_target<'a>(
    cursor: (f64, f64),
    topmost_root: Option<isize>,
    targets: &'a [WindowsPointerTarget],
) -> Option<&'a WindowsPointerTarget> {
    targets
        .iter()
        .find(|target| {
            normalize(cursor, &target.frame).2
                && topmost_root.is_some_and(|root| target.root_hwnds.contains(&root))
        })
        .or_else(|| {
            targets.iter().find(|target| {
                target.target_kind == crate::windows_capture_target::TargetKind::Display
                    && normalize(cursor, &target.frame).2
            })
        })
}

#[cfg(not(target_os = "macos"))]
fn windows_visibility_decisions(
    targets: &[WindowsPointerTarget],
    selected: Option<&WindowsPointerKey>,
    last_visible: &HashMap<WindowsPointerKey, bool>,
) -> Vec<(WindowsPointerKey, bool)> {
    let mut decisions = targets
        .iter()
        .filter_map(|target| {
            let visible = selected == Some(&target.key);
            let was_visible = last_visible.get(&target.key).copied().unwrap_or(false);
            (visible || was_visible).then(|| (target.key.clone(), visible))
        })
        .collect::<Vec<_>>();
    decisions.extend(
        last_visible
            .iter()
            .filter(|(key, visible)| **visible && !targets.iter().any(|target| target.key == **key))
            .map(|(key, _)| (key.clone(), false)),
    );
    decisions
}

/// Windows sender loop: the same 45Hz cursor poll + ~9Hz frame refresh, with
/// the two macOS-native inputs swapped for Windows equivalents:
///   - frame refresh: `GetWindowRect` per shared window (resolved through the
///     capture-target registry) instead of a `CGWindowList` snapshot;
///   - cursor: `GetCursorPos` (physical px) instead of CG logical points.
/// Normalization is a scale-invariant ratio, so mixing physical-px frames and
/// cursor is correct. Remote-compositor targets (your cursor over OTHER
/// participants' shared windows) use `windows_compositor`'s content-frame
/// snapshot and owner identity so only the exact shared surface is selected.
#[cfg(target_os = "windows")]
fn sender_loop_windows(app: AppHandle) {
    use tauri::Manager;

    let mut last_visible: HashMap<WindowsPointerKey, bool> = HashMap::new();
    let mut tick: u64 = 0;
    // Remote-compositor content frames, refreshed at the ~9Hz cadence only:
    // the snapshot is a BLOCKING cross-thread RPC into the compositor thread,
    // so it must never run at the 45Hz poll rate (it would collapse the loop).
    let mut remote_frames: Vec<crate::windows_compositor::PointerTargetSnapshot> = Vec::new();
    // Confirm-before-adopt candidates (#762): a shared window's GetWindowRect
    // can OSCILLATE on a loaded host (observed: alternates (192,161,1215,719)
    // <-> (187,161,1225,724), ~5-10px, ~9Hz — the same rect wobble that makes
    // the WGC capture look striped/fuzzy). A frame change is adopted only
    // after the SAME new rect is read on two consecutive refresh cycles, so a
    // genuine move/resize (which settles) adopts within ~220ms while a cyclic
    // oscillation never confirms and the normalization basis stays stable.
    let mut pending_frame_changes: std::collections::HashMap<u32, WindowFrame> =
        std::collections::HashMap::new();
    // The normalization BASIS this loop actually uses. Seeded from the share's
    // initial frame, then only advances via confirmed GetWindowRect changes
    // below — never re-read from `share.frame` per poll, so an external writer
    // oscillating `share.frame` (observed: GetWindowRect swinging 5-10px at
    // ~9Hz) cannot flip the basis under us.
    let mut basis_frames: std::collections::HashMap<u32, WindowFrame> =
        std::collections::HashMap::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        tick = tick.wrapping_add(1);
        let Some(state) = app.try_state::<SessionState>() else {
            continue;
        };
        let (publisher, identity, snapshot_frames) = state.shared_windows_snapshot();

        // Reconcile membership (share added/stopped), keeping held frame
        // values for windows we already track. New windows seed from their
        // snapshot frame.
        for (window_id, frame) in &snapshot_frames {
            basis_frames.entry(*window_id).or_insert(*frame);
        }
        basis_frames.retain(|window_id, _| snapshot_frames.iter().any(|(sid, _)| sid == window_id));
        let mut frames: Vec<(u32, WindowFrame)> = basis_frames
            .iter()
            .map(|(window_id, frame)| (*window_id, *frame))
            .collect();

        // ~9Hz: refresh each shared window's on-screen frame via its HWND
        // (GetWindowRect — the OUTER window frame). This is what WGC captures
        // and the receiver displays (the captured item is anchored at the
        // outer origin), so the sharer's normalized telepointer coords land on
        // the same pixels regardless of the source's DPI/monitor scale —
        // normalizing against the client rect would offset the tag by the
        // chrome band, worst on non-100% scaled displays. No CG window stack
        // on Windows; the capture-target registry maps token -> hwnd.
        if tick % FRAME_REFRESH_TICKS == 0 {
            if !frames.is_empty() {
                let mut changed: Vec<(u32, WindowFrame)> = Vec::new();
                for (window_id, stored) in &frames {
                    let Some(fresh) = crate::windows_capture_target::resolve(*window_id)
                        .ok()
                        .and_then(|target| {
                            crate::platform::windows::window_frame(
                                windows::Win32::Foundation::HWND(target.raw_handle() as *mut _),
                            )
                        })
                    else {
                        continue;
                    };
                    if fresh != *stored {
                        // #762 (cursor-oscillation): Windows GetWindowRect can
                        // jitter a shared window's reported frame by a few px
                        // (micro-jitter) or even OSCILLATE by 5-10px on a ~9Hz
                        // cadence (observed on a loaded host). Because this
                        // frame is the normalization basis AND the WGC capture
                        // anchor, either makes the telepointer tag (and the
                        // captured content) bob. Micro-jitter below the
                        // tolerance is dropped outright; larger changes are
                        // adopted only after two consecutive identical
                        // readings (see `pending_frame_changes`), so a cyclic
                        // oscillation never flips the basis while a genuine
                        // move/resize still tracks.
                        if within_frame_tolerance(*stored, fresh, FRAME_JITTER_TOLERANCE_PX) {
                            continue;
                        }
                        let confirmed = pending_frame_changes.get(window_id) == Some(&fresh);
                        pending_frame_changes.insert(*window_id, fresh);
                        if confirmed {
                            changed.push((*window_id, fresh));
                        }
                    } else {
                        pending_frame_changes.remove(window_id);
                    }
                }
                if !changed.is_empty() {
                    state.update_share_frames_and_visibility(&changed, &[]);
                    for (id, fresh) in &changed {
                        basis_frames.insert(*id, *fresh);
                        if let Some(slot) = frames.iter_mut().find(|(fid, _)| fid == id) {
                            slot.1 = *fresh;
                        }
                    }
                }
            }
            // Remote-compositor targets: your cursor over OTHER participants'
            // shared windows. Frames come from the Windows compositor (video
            // child screen rects, physical px — same space as the cursor).
            remote_frames = crate::windows_compositor::open_content_frames();
        }

        let Some(publisher) = publisher else {
            last_visible.clear();
            continue;
        };
        let user_id = identity.unwrap_or_else(|| "unknown".to_string());
        let mut targets = Vec::with_capacity(frames.len() + remote_frames.len());
        for (window_id, frame) in &frames {
            let Ok(target) = crate::windows_capture_target::resolve(*window_id) else {
                continue;
            };
            let is_region = crate::region_window::resolve(*window_id).is_some();
            let mut root_hwnds = Vec::new();
            if target.kind() == crate::windows_capture_target::TargetKind::Window {
                root_hwnds.push(target.raw_handle() as isize);
            }
            root_hwnds.extend(crate::windows_share_overlay::hwnd_for_local_share(
                *window_id,
            ));
            targets.push(WindowsPointerTarget {
                key: WindowsPointerKey {
                    kind: PointerTargetKind::LocalShare,
                    window_id: *window_id,
                    owner_identity: user_id.clone(),
                },
                frame: *frame,
                // A Petal View selector is an HWND for capture ownership but
                // a display ROI for pointer hit-testing. Its hollow center is
                // click-through, so WindowFromPoint returns the underlying
                // app instead of the selector HWND. Display classification
                // makes selection use the region bounds in that case.
                target_kind: if is_region {
                    crate::windows_capture_target::TargetKind::Display
                } else {
                    target.kind()
                },
                root_hwnds,
            });
        }
        targets.extend(remote_frames.iter().map(|snapshot| WindowsPointerTarget {
            key: WindowsPointerKey {
                kind: PointerTargetKind::RemoteCompositor,
                window_id: snapshot.window_id,
                owner_identity: snapshot.owner_identity.clone(),
            },
            frame: snapshot.frame,
            target_kind: crate::windows_capture_target::TargetKind::Window,
            root_hwnds: snapshot.root_hwnds.clone(),
        }));
        let cursor = crate::platform::windows::cursor_position();
        let topmost_root = cursor
            .and_then(crate::platform::windows::root_window_at)
            .map(|hwnd| hwnd.0 as isize);
        let selected = cursor
            .and_then(|point| select_windows_pointer_target(point, topmost_root, &targets))
            .map(|target| target.key.clone());

        for (key, visible) in
            windows_visibility_decisions(&targets, selected.as_ref(), &last_visible)
        {
            let target = targets.iter().find(|target| target.key == key);
            let (x, y) = match (cursor, target) {
                (Some(point), Some(target)) => {
                    let (x, y, _) = normalize(point, &target.frame);
                    (x, y)
                }
                _ => (0.5, 0.5),
            };
            let was_visible = last_visible.get(&key).copied().unwrap_or(false);
            if visible {
                last_visible.insert(key.clone(), true);
            } else {
                last_visible.remove(&key);
            }
            if visible && !was_visible {
                log::info!(
                    "telepointer: local user '{user_id}' entered {} window {}",
                    key.kind.label(),
                    key.window_id,
                );
            } else if !visible && was_visible {
                log::info!(
                    "telepointer: local user '{user_id}' left {} window {}",
                    key.kind.label(),
                    key.window_id,
                );
            }

            publish_pointer(
                &publisher,
                PointerMessage {
                    window_id: key.window_id,
                    user_id: user_id.clone(),
                    x,
                    y,
                    visible,
                    activity: None,
                    surface_owner_id: Some(key.owner_identity),
                },
            );
        }
    }
}

/// Broadcast one remote-control activity marker through the same per-window
/// telepointer channel. The host emits this only after the remote-control
/// packet has passed authorization and target-PID checks, so receivers can
/// render it as "this pointer is actively controlling" rather than a plain
/// telepointer gesture.
pub(crate) fn publish_activity(
    room_connection: &Arc<crate::transport::RoomConnection>,
    window_id: u32,
    user_id: String,
    x: f64,
    y: f64,
    activity: PointerActivity,
) {
    publish_pointer(
        room_connection,
        PointerMessage {
            window_id,
            user_id,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            visible: true,
            activity: Some(activity),
            surface_owner_id: None,
        },
    );
}

/// Serialize + publish one pointer message over the room's data channel.
/// Fire-and-forget (spawned, not awaited) -- a dropped/lossy telepointer
/// frame at 45Hz is invisible to the user (SPEC.md explicitly calls for
/// `Lossy`-style high-rate delivery, "interpolated on the receiver"), so this
/// never blocks the poll loop's sleep cadence waiting on network I/O.
fn publish_pointer(
    room_connection: &Arc<crate::transport::RoomConnection>,
    message: PointerMessage,
) {
    let room = room_connection.room();
    tauri::async_runtime::spawn(async move {
        let Ok(payload) = serde_json::to_vec(&message) else {
            return;
        };
        let packet = livekit::DataPacket {
            payload,
            topic: Some(TOPIC.to_string()),
            // Lossy, not reliable: SPEC.md §4.5 wants "high rate, ~30-60Hz,
            // interpolated on the receiver" -- a dropped position sample is
            // superseded by the next one 22ms later, so reliable delivery
            // (with its retransmit/ordering overhead) buys nothing here and
            // would only add latency under any packet loss. Reliable
            // delivery is the right choice for the *rare, must-not-drop*
            // events (e.g. a future click/annotation event riding the same
            // channel per SPEC.md §4.5's "sets up P1/P2" note), not this
            // continuous stream.
            reliable: false,
            destination_identities: Vec::new(), // broadcast to the whole room
        };
        if let Err(e) = room.local_participant().publish_data(packet).await {
            log::debug!("telepointer: publish_data failed: {e}");
        }
    });
}

/// Start the receiver-side task: subscribes to `publisher`'s room's events
/// and routes every telepointer-topic `RoomEvent::DataReceived`. Called once
/// per room connection -- see
/// `start_receiver_for_room`'s call site in `session.rs` for exactly when.
///
/// ## Which webview this targets
///
/// The real target is the compositor's per-window pointer overlay webview,
/// resolved from the owner-scoped compositor label when available. Tauri events do not
/// reach these `tauri_nspanel` child webviews reliably, so updates are pushed
/// with `webview.eval("window.__petalTelepointer(...)")`; the overlay then
/// filters by its own `windowId` and `ownerIdentity`. A global
/// `telepointer-update` event remains
/// only as the harmless `/dev/telepointer` fallback because plain frontend
/// `listen()` handlers receive global `emit`, not `emit_to(label, ...)`.
#[cfg(target_os = "macos")]
pub fn start_receiver_for_room(
    app: &AppHandle,
    room: Arc<livekit::Room>,
    generation: RoomGeneration,
) {
    let mut events = room.subscribe();
    let local_identity = room.local_participant().identity().to_string();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("telepointer: receiver exiting for stale room generation");
                break;
            }
            if let livekit::RoomEvent::DataReceived {
                payload,
                topic,
                participant,
                ..
            } = event
            {
                if topic.as_deref() != Some(TOPIC) {
                    continue;
                }
                let Ok(message) = serde_json::from_slice::<PointerMessage>(&payload) else {
                    continue;
                };
                let sender_identity = participant.as_ref().map(|p| p.identity().to_string());
                let display_name = participant.as_ref().and_then(|p| {
                    let name = p.name();
                    let trimmed = name.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                });
                let palette_index = participant.as_ref().and_then(|p| {
                    crate::transport::publisher::identity_palette_index_from_metadata(&p.metadata())
                });
                let surface_owner = message
                    .surface_owner_id
                    .clone()
                    .or_else(|| sender_identity.clone())
                    .unwrap_or_default();
                let Some(mut update) = update_for_authenticated_sender(
                    message,
                    sender_identity,
                    display_name,
                    palette_index,
                ) else {
                    log::warn!("telepointer: ignored update without authenticated sender identity");
                    continue;
                };
                update.surface_owner_id =
                    (!surface_owner.is_empty()).then_some(surface_owner.clone());
                let window_id = update.window_id;
                // Rate-limited receipt log (every 30th message ~ once/1.3s at
                // 22Hz) so a reader can confirm telepointer data is arriving and
                // being routed to the compositor overlay, without flooding.
                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static RX_COUNT: AtomicU64 = AtomicU64::new(0);
                    let n = RX_COUNT.fetch_add(1, Ordering::Relaxed);
                    if n % 30 == 0 {
                        let message = format!(
                            "telepointer: received update #{n} for window {window_id} from authenticated '{}' (x={:.2} y={:.2} visible={}) -> emitting to overlay",
                            update.user_id, update.x, update.y, update.visible
                        );
                        log::info!("{message}");
                        // Also journal this (n=0 fires on the very first
                        // received update, so the test-cockpit's TELE
                        // assertion -- which polls DiagnosticsState::journal(),
                        // not the file log -- sees it almost immediately.
                        // Prior to this, telepointer.rs never called
                        // journal_append at all, making that scenario's
                        // native-side check structurally unreachable.
                        if let Some(diagnostics) =
                            app.try_state::<crate::diagnostics::DiagnosticsState>()
                        {
                            diagnostics.journal_append(&app, "telepointer", message);
                        }
                    }
                }
                // Deliver by pushing the update DIRECTLY into the compositor
                // pointer-overlay webview via `eval` (WKWebView JS injection),
                // NOT via the Tauri event bus. Verified live that neither
                // `emit_to(pointer_label, ...)` nor a global `emit` reaches
                // these nspanel child webviews' `listen` handlers (the overlay's
                // rx counter stayed 0), while a static in-page test pointer DID
                // render -- so the event bus, not the render, was the gap.
                // `eval` is reliable for these webviews. The overlay page
                // exposes `window.__petalTelepointer(update)` (see
                // compositor/pointer/+page.svelte) and filters by windowId
                // itself by window id and surface owner.
                #[cfg(target_os = "macos")]
                if let Ok(json) = serde_json::to_string(&update) {
                    let labels = if surface_owner == local_identity {
                        overlay_delivery_labels(
                            Vec::new(),
                            crate::share_overlay::overlay_labels_for_window(window_id),
                        )
                    } else {
                        crate::compositor::pointer_label_for_remote_window(
                            window_id,
                            &surface_owner,
                        )
                        .into_iter()
                        .collect()
                    };
                    for label in labels {
                        let Some(overlay) = tauri::Manager::get_webview_window(&app, &label) else {
                            continue;
                        };
                        // JSON is safe to inline into a JS call argument.
                        if let Err(e) = overlay.eval(format!(
                            "window.__petalTelepointer && window.__petalTelepointer({json})"
                        )) {
                            log::warn!(
                                "telepointer: failed to eval update for window {window_id} overlay '{}': {e}",
                                overlay.label()
                            );
                        }
                    }
                }
                // Harmless fallback for the /dev/telepointer route. Global
                // `emit`, NOT `emit_to(TELEPOINTER_DEV_LABEL, ...)`: Tauri 2's
                // `emit_to("<label>")` never reaches a page's plain `listen()`
                // (EventTarget::AnyLabel vs Any — see hover_tab.rs's emit-site
                // comment / issue #22), so the old targeted emit was dead.
                let _ = tauri::Emitter::emit(&app, "telepointer-update", update);
            }
        }
    });
}

#[cfg(not(target_os = "macos"))]
pub fn start_receiver_for_room(
    app: &AppHandle,
    room: Arc<livekit::Room>,
    generation: RoomGeneration,
) {
    let mut events = room.subscribe();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("telepointer: receiver exiting for stale room generation");
                break;
            }
            if let livekit::RoomEvent::DataReceived {
                payload,
                topic,
                participant,
                ..
            } = event
            {
                if topic.as_deref() != Some(TOPIC) {
                    continue;
                }
                let Ok(message) = serde_json::from_slice::<PointerMessage>(&payload) else {
                    continue;
                };
                let sender_identity = participant.as_ref().map(|p| p.identity().to_string());
                let display_name = participant.as_ref().and_then(|p| {
                    let name = p.name();
                    let trimmed = name.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                });
                let palette_index = participant.as_ref().and_then(|p| {
                    crate::transport::publisher::identity_palette_index_from_metadata(&p.metadata())
                });
                // Surface owner for routing: a capable sender names the shared
                // surface (its owner) it is over, so a receiver renders the
                // cursor on exactly that surface. Old clients omit it and we
                // fall back to the sender (legacy single-sharer behavior).
                // Window tokens collide across sharers, so routing is by owner,
                // never by window id alone.
                let surface_owner = message
                    .surface_owner_id
                    .clone()
                    .unwrap_or_else(|| sender_identity.clone().unwrap_or_default());
                let local_identity = room.local_participant().identity().to_string();
                let Some(mut update) = update_for_authenticated_sender(
                    message,
                    sender_identity,
                    display_name,
                    palette_index,
                ) else {
                    log::warn!("telepointer: ignored update without authenticated sender identity");
                    continue;
                };
                let window_id = update.window_id;
                // Map the cursor through the receiver's letterbox crop: the tag
                // coords are over the FULL source frame, but the displayed child
                // shows only the crop region (bars removed). Without this, every
                // crop re-anchor (transient bars while typing) bobs the tag.
                if let Some((oxf, oyf, cwf, chf)) =
                    crate::windows_compositor::content_crop_fraction(&surface_owner, window_id)
                {
                    if cwf <= 0.0 || chf <= 0.0 {
                        continue;
                    }
                    let x = (update.x - oxf) / cwf;
                    let y = (update.y - oyf) / chf;
                    if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
                        continue;
                    }
                    update.x = x;
                    update.y = y;
                }
                // Rate-limited receipt log (every 30th message ~ once/1.3s at
                // 22Hz) + diagnostics journal, mirroring the macOS receiver so
                // the same cockpit/journal assertions work on Windows.
                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static RX_COUNT: AtomicU64 = AtomicU64::new(0);
                    let n = RX_COUNT.fetch_add(1, Ordering::Relaxed);
                    if n % 30 == 0 {
                        let message = format!(
                            "telepointer: received update #{n} for window {window_id} from authenticated '{}' (x={:.2} y={:.2} visible={}) -> emitting to overlay",
                            update.user_id, update.x, update.y, update.visible
                        );
                        log::info!("{message}");
                        if let Some(diagnostics) =
                            app.try_state::<crate::diagnostics::DiagnosticsState>()
                        {
                            diagnostics.journal_append(&app, "telepointer", message);
                        }
                    }
                }
                // Deliver by pushing the update DIRECTLY into each pointer
                // overlay webview via `eval` (`window.__petalTelepointer`).
                // (The Tauri-event `listen` fallback is deliberately NOT
                // subscribed on Windows — see +page.svelte — because WebView2
                // receives the global bus and a duplicate listener would apply
                // every update twice.)
                if let Ok(json) = serde_json::to_string(&update) {
                    let mut labels = crate::windows_compositor::pointer_overlay_labels_for(
                        &surface_owner,
                        window_id,
                    );
                    // Shared surface on this machine: if we are the sharer of
                    // this window, render every participant's cursor over our
                    // own app window too (macOS `share_overlay` parity).
                    if surface_owner == local_identity {
                        labels.extend(crate::windows_share_overlay::labels_for_local_share(
                            window_id,
                        ));
                    }
                    for label in labels {
                        if let Some(overlay) = app.get_webview_window(&label) {
                            let _ = overlay.eval(&format!("window.__petalTelepointer({json});"));
                        }
                    }
                }
                let _ = tauri::Emitter::emit(&app, "telepointer-update", update);
            }
        }
    });
}

/// Windows coverage for the pure telepointer geometry (the macOS test module
// is gated on `window_fixtures`/compositor paths). `normalize` and
// `pointer_targets` are platform-neutral — the physical-px-vs-logical-point
// frame convention is absorbed by the caller, not these functions.
#[cfg(all(test, not(target_os = "macos")))]
mod windows_tests {
    use super::*;

    #[test]
    fn normalize_maps_cursor_to_unit_square_and_reports_inside() {
        let frame = WindowFrame {
            x: 100,
            y: 50,
            width: 200,
            height: 100,
        };
        // Top-left corner.
        assert_eq!(normalize((100.0, 50.0), &frame), (0.0, 0.0, true));
        // Center.
        assert_eq!(normalize((200.0, 100.0), &frame), (0.5, 0.5, true));
        // Just outside the right edge: clamped, not inside.
        let (x, y, inside) = normalize((300.0, 100.0), &frame);
        assert_eq!((x, y), (1.0, 0.5));
        assert!(!inside);
        // Far outside: clamped to [0, 1].
        let (x, y, inside) = normalize((0.0, 0.0), &frame);
        assert_eq!((x, y), (0.0, 0.0));
        assert!(!inside);
    }

    #[test]
    fn pointer_targets_include_local_shares_and_remote_compositor_content() {
        let local = [(
            7u32,
            WindowFrame {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        )];
        let remote = [(
            3u32,
            WindowFrame {
                x: 500,
                y: 0,
                width: 200,
                height: 150,
            },
        )];
        let targets = pointer_targets(&local, &remote);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].kind, PointerTargetKind::LocalShare);
        assert_eq!(targets[0].window_id, 7);
        assert_eq!(targets[1].kind, PointerTargetKind::RemoteCompositor);
        assert_eq!(targets[1].window_id, 3);
        assert_eq!(targets[1].frame.width, 200);
    }

    fn windows_target(
        kind: PointerTargetKind,
        window_id: u32,
        owner: &str,
        frame: WindowFrame,
        roots: &[isize],
    ) -> WindowsPointerTarget {
        WindowsPointerTarget {
            key: WindowsPointerKey {
                kind,
                window_id,
                owner_identity: owner.to_string(),
            },
            frame,
            target_kind: crate::windows_capture_target::TargetKind::Window,
            root_hwnds: roots.to_vec(),
        }
    }

    #[test]
    fn display_target_selects_by_monitor_containment_without_a_root_window() {
        let mut target = windows_target(
            PointerTargetKind::LocalShare,
            7,
            "local",
            WindowFrame {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            &[],
        );
        target.target_kind = crate::windows_capture_target::TargetKind::Display;
        assert_eq!(
            select_windows_pointer_target((960.0, 540.0), None, &[target.clone()])
                .map(|selected| selected.key.clone()),
            Some(target.key.clone())
        );
        assert_eq!(
            select_windows_pointer_target((960.0, 540.0), Some(999), &[target.clone()])
                .map(|selected| selected.key.clone()),
            Some(target.key.clone())
        );
        assert!(select_windows_pointer_target((1920.0, 540.0), None, &[target]).is_none());
    }

    #[test]
    fn topmost_native_root_selects_exactly_one_overlapping_surface() {
        let overlap = WindowFrame {
            x: 0,
            y: 0,
            width: 200,
            height: 200,
        };
        let targets = vec![
            windows_target(
                PointerTargetKind::LocalShare,
                7,
                "local",
                overlap,
                &[70, 71],
            ),
            windows_target(
                PointerTargetKind::LocalShare,
                8,
                "local",
                overlap,
                &[80, 81],
            ),
        ];
        assert_eq!(
            select_windows_pointer_target((100.0, 100.0), Some(80), &targets)
                .map(|target| target.key.clone()),
            Some(targets[1].key.clone())
        );
        assert_eq!(
            select_windows_pointer_target((100.0, 100.0), Some(70), &targets)
                .map(|target| target.key.clone()),
            Some(targets[0].key.clone())
        );
    }

    #[test]
    fn selector_handles_remote_local_collisions_overlays_and_occluders() {
        let overlap = WindowFrame {
            x: 50,
            y: 50,
            width: 100,
            height: 100,
        };
        let targets = vec![
            windows_target(
                PointerTargetKind::LocalShare,
                7,
                "local",
                overlap,
                &[10, 11],
            ),
            windows_target(
                PointerTargetKind::RemoteCompositor,
                7,
                "alice",
                overlap,
                &[20, 21, 22],
            ),
            windows_target(
                PointerTargetKind::RemoteCompositor,
                7,
                "bob",
                overlap,
                &[30, 31, 32],
            ),
        ];
        for (root, expected) in [(11, 0), (20, 1), (22, 1), (31, 2)] {
            assert_eq!(
                select_windows_pointer_target((75.0, 75.0), Some(root), &targets)
                    .map(|target| target.key.clone()),
                Some(targets[expected].key.clone())
            );
        }
        assert!(select_windows_pointer_target((75.0, 75.0), Some(999), &targets).is_none());
        assert!(select_windows_pointer_target((10.0, 10.0), Some(20), &targets).is_none());
        assert!(select_windows_pointer_target((75.0, 75.0), None, &targets).is_none());
    }

    #[test]
    fn disjoint_and_unknown_roots_never_select_more_than_one_visible_target() {
        let targets = vec![
            windows_target(
                PointerTargetKind::LocalShare,
                1,
                "local",
                WindowFrame {
                    x: 0,
                    y: 0,
                    width: 50,
                    height: 50,
                },
                &[10],
            ),
            windows_target(
                PointerTargetKind::RemoteCompositor,
                2,
                "alice",
                WindowFrame {
                    x: 100,
                    y: 100,
                    width: 50,
                    height: 50,
                },
                &[20],
            ),
        ];
        for (cursor, root) in [
            ((25.0, 25.0), Some(10)),
            ((125.0, 125.0), Some(20)),
            ((25.0, 25.0), Some(20)),
            ((25.0, 25.0), Some(999)),
            ((25.0, 25.0), None),
        ] {
            let selected =
                select_windows_pointer_target(cursor, root, &targets).map(|target| &target.key);
            let decisions = windows_visibility_decisions(&targets, selected, &HashMap::new());
            assert!(decisions.iter().filter(|(_, visible)| *visible).count() <= 1);
        }
    }

    #[test]
    fn switching_overlapping_surfaces_hides_the_loser_and_shows_one_winner() {
        let overlap = WindowFrame {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let targets = vec![
            windows_target(
                PointerTargetKind::RemoteCompositor,
                1,
                "alice",
                overlap,
                &[10],
            ),
            windows_target(
                PointerTargetKind::RemoteCompositor,
                2,
                "bob",
                overlap,
                &[20],
            ),
        ];
        let mut last_visible = HashMap::from([(targets[0].key.clone(), true)]);
        let selected = select_windows_pointer_target((50.0, 50.0), Some(20), &targets)
            .map(|target| &target.key);
        let decisions = windows_visibility_decisions(&targets, selected, &last_visible);
        assert_eq!(
            decisions,
            vec![
                (targets[0].key.clone(), false),
                (targets[1].key.clone(), true)
            ]
        );
        assert_eq!(decisions.iter().filter(|(_, visible)| *visible).count(), 1);

        for (key, visible) in decisions {
            last_visible.insert(key, visible);
        }
        assert_eq!(
            windows_visibility_decisions(&targets, None, &last_visible),
            vec![(targets[1].key.clone(), false)]
        );
    }

    #[test]
    fn disappearing_selected_surface_gets_a_prompt_hide() {
        let key = WindowsPointerKey {
            kind: PointerTargetKind::RemoteCompositor,
            window_id: 9,
            owner_identity: "alice".to_string(),
        };
        assert_eq!(
            windows_visibility_decisions(&[], None, &HashMap::from([(key.clone(), true)])),
            vec![(key, false)]
        );
    }

    #[test]
    fn frame_deadband_suppresses_sub_tolerance_getwindowrect_wiggle() {
        // A stationary window can report a 1-2px GetWindowRect wiggle on the
        // ~9Hz refresh; that must NOT be adopted (it bobs the tag + capture).
        let stored = WindowFrame {
            x: 100,
            y: 50,
            width: 1215,
            height: 719,
        };
        let wiggle_x = WindowFrame {
            x: 101,
            y: 50,
            width: 1215,
            height: 719,
        };
        let wiggle_y = WindowFrame {
            x: 100,
            y: 52,
            width: 1215,
            height: 719,
        };
        assert!(within_frame_tolerance(
            stored,
            wiggle_x,
            FRAME_JITTER_TOLERANCE_PX
        ));
        assert!(within_frame_tolerance(
            stored,
            wiggle_y,
            FRAME_JITTER_TOLERANCE_PX
        ));
        // A real move/resize beyond the deadband is adopted.
        let real_move = WindowFrame {
            x: 120,
            y: 50,
            width: 1215,
            height: 719,
        };
        let real_size = WindowFrame {
            x: 100,
            y: 50,
            width: 1000,
            height: 700,
        };
        assert!(!within_frame_tolerance(
            stored,
            real_move,
            FRAME_JITTER_TOLERANCE_PX
        ));
        assert!(!within_frame_tolerance(
            stored,
            real_size,
            FRAME_JITTER_TOLERANCE_PX
        ));
        // A 3px (=tolerance) move is still within the deadband (<=) and
        // suppressed; only >3px is adopted.
        let at_tolerance = WindowFrame {
            x: 103,
            y: 50,
            width: 1215,
            height: 719,
        };
        assert!(within_frame_tolerance(
            stored,
            at_tolerance,
            FRAME_JITTER_TOLERANCE_PX
        ));
        let over_tolerance = WindowFrame {
            x: 104,
            y: 50,
            width: 1215,
            height: 719,
        };
        assert!(!within_frame_tolerance(
            stored,
            over_tolerance,
            FRAME_JITTER_TOLERANCE_PX
        ));
    }
}

// macOS-only: the tests replay `window_fixtures` (CG window snapshots) and
// exercise the macOS compositor/receiver paths.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct ContractFixture {
        topics: ContractTopics,
        #[serde(rename = "telepointerFields")]
        telepointer_fields: Vec<String>,
    }

    #[derive(serde::Deserialize)]
    struct ContractTopics {
        telepointer: String,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!("../../../../contracts/petal-contracts.json")).unwrap()
    }

    fn frame(x: i32, y: i32, w: i32, h: i32) -> WindowFrame {
        WindowFrame {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn telepointer_wire_shape_matches_shared_contract_fixture() {
        let fixture = contract_fixture();
        assert_eq!(TOPIC, fixture.topics.telepointer);
        let update = PointerMessage {
            window_id: 42,
            user_id: "web-1".to_string(),
            x: 0.5,
            y: 0.25,
            visible: true,
            activity: Some(PointerActivity::Click),
            surface_owner_id: Some("peter2".to_string()),
        };
        let value = serde_json::to_value(update).unwrap();
        let mut fields = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        fields.sort();
        assert_eq!(fields, fixture.telepointer_fields);
    }

    #[test]
    fn receiver_update_can_carry_display_name_without_changing_wire_message() {
        let message = PointerMessage {
            window_id: 42,
            user_id: "4f7b59e8-3d18-467c-985b-f8d477307b33".to_string(),
            x: 0.5,
            y: 0.25,
            visible: true,
            activity: None,
            surface_owner_id: None,
        };

        let update =
            TelepointerUpdate::from_message(message, Some("Ada Lovelace".to_string()), Some(5));
        let value = serde_json::to_value(update).unwrap();

        assert_eq!(value["displayName"], "Ada Lovelace");
        assert_eq!(value["paletteIndex"], 5);
        assert_eq!(value["userId"], "4f7b59e8-3d18-467c-985b-f8d477307b33");
    }

    #[test]
    fn receiver_update_overwrites_spoofed_payload_user_id_with_livekit_sender_identity() {
        let message = PointerMessage {
            window_id: 42,
            user_id: "victim-user".to_string(),
            x: 0.5,
            y: 0.25,
            visible: true,
            activity: Some(PointerActivity::Click),
            surface_owner_id: None,
        };

        let update = update_for_authenticated_sender(
            message,
            Some(" authenticated-sender ".to_string()),
            Some("Ada Lovelace".to_string()),
            Some(2),
        )
        .unwrap();

        assert_eq!(update.user_id, "authenticated-sender");
        assert_eq!(update.display_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(update.palette_index, Some(2));
        assert_eq!(update.activity, Some(PointerActivity::Click));
    }

    #[test]
    fn receiver_update_drops_payload_without_authenticated_sender_identity() {
        let message = PointerMessage {
            window_id: 42,
            user_id: "victim-user".to_string(),
            x: 0.5,
            y: 0.25,
            visible: true,
            activity: None,
            surface_owner_id: None,
        };

        assert!(update_for_authenticated_sender(message, None, None, None).is_none());
    }

    #[test]
    fn center_of_window_normalizes_to_half_half() {
        let (x, y, inside) = normalize((150.0, 150.0), &frame(100, 100, 100, 100));
        assert!(inside);
        assert!((x - 0.5).abs() < 1e-9);
        assert!((y - 0.5).abs() < 1e-9);
    }

    #[test]
    fn top_left_corner_is_zero_zero() {
        let (x, y, inside) = normalize((100.0, 100.0), &frame(100, 100, 100, 100));
        assert!(inside);
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.0);
    }

    #[test]
    fn cursor_outside_bounds_is_not_inside_and_is_clamped() {
        let (x, y, inside) = normalize((500.0, 500.0), &frame(0, 0, 100, 100));
        assert!(!inside);
        assert_eq!(x, 1.0);
        assert_eq!(y, 1.0);
    }

    #[test]
    fn cursor_left_of_and_above_window_clamps_to_zero() {
        let (x, y, inside) = normalize((-50.0, -50.0), &frame(0, 0, 100, 100));
        assert!(!inside);
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.0);
    }

    // --- visible_window_ids (#742 characterization) ---
    //
    // This derivation decides whether a share is reported OFF-SCREEN, which
    // downstream becomes "off-screen vs closed" in session::share. It ran
    // untested inside the sender loop until #742. Pinned here so the
    // window-registry migration must reproduce it.

    #[test]
    fn visible_window_ids_reports_only_shares_present_in_the_stack() {
        let shared = vec![
            (7u32, frame(100, 100, 640, 480)),
            (8u32, frame(0, 0, 320, 240)),
        ];
        let stack = vec![
            (999i64, frame(0, 0, 50, 50)),
            (7, frame(100, 100, 640, 480)),
        ];
        assert_eq!(
            visible_window_ids(&shared, &stack),
            vec![7],
            "8 is absent from the on-screen stack, so it is not visible"
        );
    }

    /// Presence alone is the signal -- the stack frame is never compared, so a
    /// share whose stored frame is stale is still "visible". A registry that
    /// added a geometry check here would newly report windows off-screen.
    #[test]
    fn visible_window_ids_ignores_frame_mismatch_presence_is_the_only_signal() {
        let shared = vec![(7u32, frame(100, 100, 640, 480))];
        let stack = vec![(7i64, frame(-5000, -5000, 1, 1))];
        assert_eq!(visible_window_ids(&shared, &stack), vec![7]);
    }

    #[test]
    fn visible_window_ids_is_empty_when_nothing_is_shared_or_stack_is_empty() {
        let shared = vec![(7u32, frame(0, 0, 10, 10))];
        assert!(visible_window_ids(&shared, &[]).is_empty());
        assert!(visible_window_ids(&[], &[(7i64, frame(0, 0, 10, 10))]).is_empty());
    }

    /// Order follows the SHARED list, not the stack; duplicate stack entries
    /// for one id must not duplicate the result.
    #[test]
    fn visible_window_ids_follows_shared_order_and_does_not_duplicate() {
        let shared = vec![(8u32, frame(0, 0, 10, 10)), (7u32, frame(0, 0, 10, 10))];
        let stack = vec![
            (7i64, frame(0, 0, 10, 10)),
            (8i64, frame(0, 0, 10, 10)),
            (7i64, frame(0, 0, 10, 10)),
        ];
        assert_eq!(visible_window_ids(&shared, &stack), vec![8, 7]);
    }

    /// #875: order follows the STACK (front-to-back), not the shared list --
    /// this is the inverse ordering convention from `visible_window_ids`
    /// above, and is exactly what makes the result a real z-order.
    #[test]
    fn shared_window_z_order_follows_stack_order_and_filters_to_shared() {
        let shared = vec![(8u32, frame(0, 0, 10, 10)), (7u32, frame(0, 0, 10, 10))];
        let stack = vec![
            (999i64, frame(0, 0, 10, 10)), // unshared window, frontmost
            (7i64, frame(0, 0, 10, 10)),
            (8i64, frame(0, 0, 10, 10)),
        ];
        assert_eq!(
            shared_window_z_order(&shared, &stack),
            vec![7, 8],
            "the unshared frontmost window must be omitted, and shared \
             windows must come out in the STACK's front-to-back order"
        );
    }

    #[test]
    fn shared_window_z_order_omits_a_shared_window_absent_from_the_stack() {
        let shared = vec![(7u32, frame(0, 0, 10, 10)), (8u32, frame(0, 0, 10, 10))];
        let stack = vec![(8i64, frame(0, 0, 10, 10))];
        assert_eq!(
            shared_window_z_order(&shared, &stack),
            vec![8],
            "a shared window momentarily off-screen (absent from the stack) \
             is simply omitted, matching visible_window_ids' presence-only rule"
        );
    }

    #[test]
    fn shared_window_z_order_is_empty_when_nothing_is_shared_or_stack_is_empty() {
        let shared = vec![(7u32, frame(0, 0, 10, 10))];
        assert!(shared_window_z_order(&shared, &[]).is_empty());
        assert!(shared_window_z_order(&[], &[(7i64, frame(0, 0, 10, 10))]).is_empty());
    }

    /// GOLDEN REPLAY (#742, plan §7.1): replay the recorded session through
    /// the REAL refresh pipeline exactly as the sender loop runs it --
    /// `frames_to_apply` + `visible_window_ids` per tick, with the known-frame
    /// map updated between ticks like production. The "shared" set is chosen
    /// deterministically from the fixture: the two most persistent layer-0
    /// foreign windows.
    #[test]
    fn frame_refresh_pipeline_matches_golden_over_recorded_session() {
        for fixture in crate::window_fixtures::REPLAY_FIXTURES {
            frame_refresh_golden_one(fixture);
        }
    }

    /// GOLDEN TRANSFER (#744, plan §7.1): the window_registry snapshot must
    /// reproduce the telepointer stack byte-for-byte. Ingest each fixture frame
    /// into a registry, rebuild the `(wid, frame)` stack from
    /// `registry.snapshot()`, and assert it equals the stack the live path
    /// builds directly from the CG list. If equal, every downstream telepointer
    /// decision (frames_to_apply / visible_window_ids) is identical whether the
    /// window state comes from onscreen_stack() or from the registry — which is
    /// exactly what the Phase-2 migration relies on.
    #[test]
    fn registry_snapshot_reproduces_the_telepointer_stack() {
        use crate::window_registry::{OwnChromeOracle, WindowRegistry};
        struct Foreign;
        impl OwnChromeOracle for Foreign {
            fn is_decorative(&self, _: &str) -> bool {
                false
            }
        }
        for fixture_name in crate::window_fixtures::REPLAY_FIXTURES {
            let fixture = crate::window_fixtures::load(
                &crate::window_fixtures::fixtures_dir().join(format!("{fixture_name}.jsonl")),
            );
            let reg = WindowRegistry::new();
            for f in &fixture {
                // The stack the LIVE telepointer path builds (onscreen_stack:
                // number + truncated frame).
                let direct: Vec<(i64, WindowFrame)> = f
                    .windows
                    .iter()
                    .map(|w| {
                        (
                            w.number,
                            WindowFrame {
                                x: w.x as i32,
                                y: w.y as i32,
                                width: w.w as i32,
                                height: w.h as i32,
                            },
                        )
                    })
                    .collect();
                // Feed the SAME frames into the registry, then rebuild the stack
                // from its snapshot.
                let rows: Vec<(u32, f64, f64, f64, f64, i64, f64, i32, String)> = f
                    .windows
                    .iter()
                    .filter_map(|w| {
                        let wid = u32::try_from(w.number).ok()?;
                        Some((
                            wid,
                            w.x,
                            w.y,
                            w.w,
                            w.h,
                            w.layer,
                            w.alpha,
                            i32::try_from(w.owner_pid).unwrap_or(-1),
                            w.name.clone(),
                        ))
                    })
                    .collect();
                reg.ingest_rows(&rows, 999, &Foreign);
                let snap = reg.snapshot();
                let via_registry: Vec<(i64, WindowFrame)> = snap
                    .records_front_to_back()
                    .map(|r| (r.wid as i64, r.frame))
                    .collect();
                // The direct stack keeps negative window numbers (u32 conversion
                // in the registry drops them); compare only the non-negative
                // ids, which is all telepointer's known-share set can contain.
                let direct_ok: Vec<(i64, WindowFrame)> =
                    direct.into_iter().filter(|(n, _)| *n >= 0).collect();
                assert_eq!(
                    via_registry, direct_ok,
                    "registry stack diverges from onscreen_stack for {fixture_name}"
                );
            }
        }
    }

    fn frame_refresh_golden_one(fixture_name: &str) {
        let fixture = crate::window_fixtures::load(
            &crate::window_fixtures::fixtures_dir().join(format!("{fixture_name}.jsonl")),
        );
        assert!(fixture.len() >= 10, "fixture {fixture_name} too short");
        // Pick the shared set deterministically: layer-0 windows ranked first
        // by how many DISTINCT frames they occupy (movers exercise
        // `frames_to_apply`; a static pick would characterize nothing), then
        // by presence, then id.
        let mut stats: std::collections::HashMap<
            i64,
            (std::collections::HashSet<(i64, i64)>, usize),
        > = Default::default();
        for f in &fixture {
            for w in &f.windows {
                if w.layer == 0 && w.w >= 40.0 && w.h >= 40.0 {
                    let e = stats.entry(w.number).or_default();
                    e.0.insert((w.x as i64, w.y as i64));
                    e.1 += 1;
                }
            }
        }
        let mut ranked: Vec<(i64, usize, usize)> = stats
            .into_iter()
            .map(|(n, (frames, count))| (n, frames.len(), count))
            .collect();
        ranked.sort_by_key(|&(n, distinct, count)| {
            (std::cmp::Reverse(distinct), std::cmp::Reverse(count), n)
        });
        let shared_ids: Vec<u32> = ranked.iter().take(2).map(|&(n, _, _)| n as u32).collect();
        assert!(
            !shared_ids.is_empty(),
            "fixture has no persistent layer-0 windows"
        );

        // Seed known frames from each id's first appearance, as start_share does.
        let mut known: Vec<(u32, WindowFrame)> = shared_ids
            .iter()
            .map(|&id| {
                let w = fixture
                    .iter()
                    .flat_map(|f| f.windows.iter())
                    .find(|w| w.number == id as i64)
                    .unwrap();
                (
                    id,
                    WindowFrame {
                        x: w.x as i32,
                        y: w.y as i32,
                        width: w.w as i32,
                        height: w.h as i32,
                    },
                )
            })
            .collect();

        #[derive(serde::Serialize)]
        struct TickDecision {
            t_ms: u64,
            visible: Vec<u32>,
            changed: Vec<(u32, i32, i32, i32, i32)>,
        }
        let mut decisions = Vec::new();
        for f in &fixture {
            let stack: Vec<(i64, WindowFrame)> = f
                .windows
                .iter()
                .map(|w| {
                    (
                        w.number,
                        WindowFrame {
                            x: w.x as i32,
                            y: w.y as i32,
                            width: w.w as i32,
                            height: w.h as i32,
                        },
                    )
                })
                .collect();
            let changed = frames_to_apply(&known, &stack);
            let visible = visible_window_ids(&known, &stack);
            for (id, fresh) in &changed {
                if let Some(slot) = known.iter_mut().find(|(kid, _)| kid == id) {
                    slot.1 = *fresh;
                }
            }
            decisions.push(TickDecision {
                t_ms: f.t_ms,
                visible,
                changed: changed
                    .iter()
                    .map(|(id, fr)| (*id, fr.x, fr.y, fr.width, fr.height))
                    .collect(),
            });
        }
        crate::window_fixtures::assert_golden(
            &format!("telepointer-refresh.{fixture_name}"),
            &decisions,
        );
    }

    // --- frames_to_apply (issue #30: live frame refresh decision logic) ---

    #[test]
    fn frames_to_apply_reports_a_moved_window() {
        let shared = vec![(7u32, frame(100, 100, 640, 480))];
        let stack = vec![
            (999i64, frame(0, 0, 50, 50)),
            (7, frame(300, 250, 640, 480)),
        ];
        assert_eq!(
            frames_to_apply(&shared, &stack),
            vec![(7, frame(300, 250, 640, 480))]
        );
    }

    #[test]
    fn frames_to_apply_reports_a_resized_window() {
        let shared = vec![(7u32, frame(100, 100, 640, 480))];
        let stack = vec![(7i64, frame(100, 100, 800, 600))];
        assert_eq!(
            frames_to_apply(&shared, &stack),
            vec![(7, frame(100, 100, 800, 600))]
        );
    }

    #[test]
    fn frames_to_apply_is_empty_when_nothing_moved() {
        let shared = vec![
            (7u32, frame(100, 100, 640, 480)),
            (8, frame(0, 0, 320, 240)),
        ];
        let stack = vec![
            (8i64, frame(0, 0, 320, 240)),
            (7, frame(100, 100, 640, 480)),
        ];
        assert!(frames_to_apply(&shared, &stack).is_empty());
    }

    #[test]
    fn frames_to_apply_retains_offscreen_windows_by_omission() {
        // Window 7 is minimized/on another Space (absent from the on-screen
        // stack): no update entry -- its last-known frame is retained, not
        // cleared or zeroed. Window 8 still refreshes normally.
        let shared = vec![
            (7u32, frame(100, 100, 640, 480)),
            (8, frame(0, 0, 320, 240)),
        ];
        let stack = vec![(8i64, frame(20, 30, 320, 240))];
        assert_eq!(
            frames_to_apply(&shared, &stack),
            vec![(8, frame(20, 30, 320, 240))]
        );
    }

    #[test]
    fn frames_to_apply_handles_multiple_moves_in_one_snapshot() {
        let shared = vec![
            (1u32, frame(0, 0, 100, 100)),
            (2, frame(200, 200, 100, 100)),
        ];
        let stack = vec![
            (2i64, frame(210, 220, 100, 100)),
            (1, frame(5, 5, 100, 100)),
        ];
        let mut changed = frames_to_apply(&shared, &stack);
        changed.sort_by_key(|(id, _)| *id);
        assert_eq!(
            changed,
            vec![(1, frame(5, 5, 100, 100)), (2, frame(210, 220, 100, 100))]
        );
    }

    #[test]
    fn pointer_targets_include_local_shares_and_remote_compositor_content() {
        let local = vec![(7u32, frame(10, 20, 300, 200))];
        let remote = vec![(8u32, frame(100, 120, 640, 400))];

        assert_eq!(
            pointer_targets(&local, &remote),
            vec![
                PointerTarget {
                    kind: PointerTargetKind::LocalShare,
                    window_id: 7,
                    frame: frame(10, 20, 300, 200),
                    surface_owner_id: None,
                    display_like: false,
                    panel_family_ids: Vec::new(),
                    is_visible: true,
                },
                PointerTarget {
                    kind: PointerTargetKind::RemoteCompositor,
                    window_id: 8,
                    frame: frame(100, 120, 640, 400),
                    surface_owner_id: None,
                    display_like: false,
                    panel_family_ids: Vec::new(),
                    is_visible: true,
                },
            ]
        );
    }

    #[test]
    fn owner_scoped_pointer_targets_preserve_local_and_remote_surface_identity() {
        let local = vec![(7u32, frame(0, 0, 100, 100))];
        let remote = vec![(7u32, frame(200, 200, 100, 100), "sharer-b".to_string())];
        let targets = pointer_targets_with_owners(&local, &remote, &[], "sharer-a");

        assert_eq!(targets[0].surface_owner_id.as_deref(), Some("sharer-a"));
        assert_eq!(targets[1].surface_owner_id.as_deref(), Some("sharer-b"));
    }

    fn remote_meta(
        window_id: u32,
        owner_identity: &str,
        family_ids: &[u32],
        is_visible: bool,
    ) -> crate::compositor::PointerFamilyMeta {
        crate::compositor::PointerFamilyMeta {
            window_id,
            owner_identity: owner_identity.to_string(),
            family_ids: family_ids.to_vec(),
            is_visible,
        }
    }

    /// #906: this test used to PIN the bug -- it asserted the remote target
    /// won on bare frame containment alone, with no real topmost-window
    /// check (`select_macos_pointer_target` took no such argument at all).
    /// Rewritten (not deleted) to assert the FIXED, occlusion-gated
    /// behavior: remote wins only when the cursor's real topmost window
    /// (now a required argument) is a member of that panel's own family --
    /// the region/single-window priority order below it is unchanged.
    #[test]
    fn mac_pointer_selection_prefers_remote_then_region_then_one_window() {
        let meta = [remote_meta(3, "remote", &[300], true)];
        let mut targets = pointer_targets_with_owners(
            &[(1, frame(0, 0, 200, 200)), (2, frame(0, 0, 200, 200))],
            &[(3, frame(0, 0, 200, 200), "remote".to_string())],
            &meta,
            "local",
        );
        targets[1].display_like = true;

        // The real topmost window IS the remote panel (id 300) -> remote
        // still wins first, same priority order as before #906.
        let selected = select_macos_pointer_target((100.0, 100.0), Some(300), &targets).unwrap();
        assert_eq!(selected.kind, PointerTargetKind::RemoteCompositor);

        // Remove the remote target; the region (display_like) target wins
        // next, exactly as before #906.
        targets.remove(2);
        let selected = select_macos_pointer_target((100.0, 100.0), Some(300), &targets).unwrap();
        assert_eq!(selected.window_id, 2);
    }

    /// #906 core fix, mirroring the Windows fixture
    /// `selector_handles_remote_local_collisions_overlays_and_occluders`'s
    /// "a foreign window is on top -> nothing selected" assertion: this is
    /// the exact field report (Eric shares window A; Adam's own window B
    /// covers the remote panel; Adam's cursor is over B, not the visible
    /// share) reproduced as a pure fixture.
    #[test]
    fn mac_remote_compositor_loses_to_a_foreign_window_on_top() {
        let meta = [remote_meta(3, "remote", &[300, 301, 302], true)];
        let targets = pointer_targets_with_owners(
            &[],
            &[(3, frame(0, 0, 200, 200), "remote".to_string())],
            &meta,
            "local",
        );

        // The real topmost window at the cursor is Adam's OWN window B
        // (id 999) -- not a member of the remote panel's family. Nothing is
        // selected, matching Windows' equivalent occluder case.
        assert!(select_macos_pointer_target((100.0, 100.0), Some(999), &targets).is_none());
        // Unknown/no topmost id at all (see `resolve_topmost_window_id`'s
        // explicit fail-closed contract) must not select either.
        assert!(select_macos_pointer_target((100.0, 100.0), None, &targets).is_none());
    }

    /// #906 step 1 (mandatory prerequisite): the click-through pointer/
    /// control/ai-chat overlay windows cover the whole video area, so a real
    /// hit-test at the cursor resolves to whichever OVERLAY window is
    /// frontmost there, never the bare panel id. A hit on any family member
    /// must count as a hit on the panel's visible surface.
    #[test]
    fn mac_remote_compositor_wins_when_the_topmost_hit_is_an_overlay_child_not_the_bare_panel() {
        let meta = [remote_meta(3, "remote", &[300, 301, 302, 303], true)];
        let targets = pointer_targets_with_owners(
            &[],
            &[(3, frame(0, 0, 200, 200), "remote".to_string())],
            &meta,
            "local",
        );
        for family_member in [300u32, 301, 302, 303] {
            let selected =
                select_macos_pointer_target((100.0, 100.0), Some(family_member), &targets)
                    .unwrap();
            assert_eq!(selected.window_id, 3);
        }
    }

    /// #906 DoD: a hidden or not-yet-revealed panel must never be selected,
    /// even if its stale frame still contains the cursor AND the topmost
    /// window id still matches its family (e.g. a warm-pool member that was
    /// hidden but whose family ids haven't churned yet).
    #[test]
    fn mac_hidden_remote_panel_is_never_selected_even_when_topmost_matches() {
        let meta = [remote_meta(3, "remote", &[300], false)];
        let targets = pointer_targets_with_owners(
            &[],
            &[(3, frame(0, 0, 200, 200), "remote".to_string())],
            &meta,
            "local",
        );
        assert!(select_macos_pointer_target((100.0, 100.0), Some(300), &targets).is_none());
    }

    /// #906 explicit fail-closed decision: no meta has arrived yet for this
    /// remote window (e.g. the ~9Hz refresh hasn't run since it opened) --
    /// `pointer_targets_with_owners` must never default a freshly-seen
    /// remote target to visible.
    #[test]
    fn mac_remote_compositor_without_family_meta_yet_is_never_selected() {
        let targets = pointer_targets_with_owners(
            &[],
            &[(3, frame(0, 0, 200, 200), "remote".to_string())],
            &[], // no PointerFamilyMeta published yet
            "local",
        );
        assert_eq!(targets[0].panel_family_ids, Vec::<u32>::new());
        assert!(!targets[0].is_visible);
        assert!(select_macos_pointer_target((100.0, 100.0), Some(3), &targets).is_none());
    }

    /// Mirrors the Windows fixture
    /// `disjoint_and_unknown_roots_never_select_more_than_one_visible_target`:
    /// two disjoint remote panels never both match, and an unknown topmost id
    /// (or one belonging to the OTHER panel) never cross-selects.
    #[test]
    fn mac_pointer_selection_never_cross_selects_disjoint_remote_targets() {
        let meta = [
            remote_meta(1, "alice", &[10], true),
            remote_meta(2, "bob", &[20], true),
        ];
        let targets = pointer_targets_with_owners(
            &[],
            &[
                (1, frame(0, 0, 50, 50), "alice".to_string()),
                (2, frame(100, 100, 50, 50), "bob".to_string()),
            ],
            &meta,
            "local",
        );
        for (cursor, topmost, expected_window_id) in [
            ((25.0, 25.0), Some(10), Some(1u32)),
            ((125.0, 125.0), Some(20), Some(2)),
            // Cursor over alice's frame, but the real topmost window is
            // bob's panel id (geometrically impossible in practice since the
            // frames are disjoint, but the gate must still reject it on
            // family membership alone, never on frame containment alone).
            ((25.0, 25.0), Some(20), None),
            ((25.0, 25.0), Some(999), None),
            ((25.0, 25.0), None, None),
        ] {
            let selected = select_macos_pointer_target(cursor, topmost, &targets)
                .map(|target| target.window_id);
            assert_eq!(selected, expected_window_id);
        }
    }

    /// #906 finding 2 (adversarial review, P1), mirroring the Windows
    /// fixture `disappearing_selected_surface_gets_a_prompt_hide`: a key that
    /// was last published visible, but has now disappeared from the target
    /// set ENTIRELY (e.g. a retired remote panel dropped from
    /// `compositor`'s window map), must get a falling-edge hide -- the
    /// per-target loop that emits normal enter/leave transitions never
    /// revisits a key that no longer has a target at all.
    #[test]
    fn vanished_visible_keys_reports_a_key_with_no_target_left() {
        let key = (PointerTargetKind::RemoteCompositor, 9, Some("alice".to_string()));
        let last_visible = HashMap::from([(key.clone(), true)]);
        assert_eq!(vanished_visible_keys(&[], &last_visible), vec![key]);
    }

    #[test]
    fn vanished_visible_keys_ignores_a_key_that_was_never_visible() {
        // Only entries the loop last published as visible=true need a
        // falling-edge hide; one already recorded false has nothing new to
        // say by disappearing.
        let key = (PointerTargetKind::RemoteCompositor, 9, Some("alice".to_string()));
        let last_visible = HashMap::from([(key, false)]);
        assert!(vanished_visible_keys(&[], &last_visible).is_empty());
    }

    #[test]
    fn vanished_visible_keys_ignores_a_key_still_present_in_targets() {
        let meta = [remote_meta(9, "alice", &[900], true)];
        let targets = pointer_targets_with_owners(
            &[],
            &[(9, frame(0, 0, 100, 100), "alice".to_string())],
            &meta,
            "local",
        );
        let key = (PointerTargetKind::RemoteCompositor, 9, Some("alice".to_string()));
        let last_visible = HashMap::from([(key, true)]);
        assert!(
            vanished_visible_keys(&targets, &last_visible).is_empty(),
            "the key still has a live target -- the normal per-target loop \
             handles its enter/leave transitions, not this vanish pass"
        );
    }

    #[test]
    fn resolve_topmost_window_id_prefers_the_sls_hit_over_the_registry_fallback() {
        let registry_records = [(42u32, frame(0, 0, 200, 200))];
        assert_eq!(
            resolve_topmost_window_id((100.0, 100.0), Some(7), &registry_records),
            Some(7),
            "an SLS hit is authoritative and must not be second-guessed by the \
             (staler, geometry-only) registry fallback"
        );
    }

    #[test]
    fn resolve_topmost_window_id_falls_back_to_the_registry_walk_when_sls_is_unavailable() {
        let registry_records = [
            (1u32, frame(0, 0, 50, 50)), // not under the cursor
            (2u32, frame(0, 0, 200, 200)),
        ];
        assert_eq!(
            resolve_topmost_window_id((100.0, 100.0), None, &registry_records),
            Some(2)
        );
    }

    /// #906 adversarial-review follow-up (P1): an EARLIER revision of this
    /// fallback filtered to `layer == 0`, which pinned exactly the bug this
    /// issue exists to fix -- a layer!=0 window (the menu bar here, but
    /// equally a floating PiP player, Spotlight, or a popover) can visually
    /// occlude a layer-0 panel underneath it just as completely as another
    /// normal window, so skipping it and searching BEHIND it for a layer-0
    /// hit made the walk report the buried, actually-occluded panel as
    /// "topmost." The fix is not "also accept layer != 0" -- it's that the
    /// walk was never supposed to filter by layer at all: front-to-back
    /// ORDER is what answers "what's on top," and the first frame-containing
    /// record, whatever its layer, IS the real topmost thing at that point.
    #[test]
    fn resolve_topmost_window_id_fallback_never_skips_a_layer_ineligible_occluder_above_the_panel() {
        // The occluder (e.g. the menu bar) is FIRST in front-to-back order,
        // ahead of the panel it covers -- the walk must return the occluder,
        // not reach past it to the panel.
        let occluder_above_panel = [
            (1u32, frame(0, 0, 200, 200)), // occluder, frontmost
            (2u32, frame(0, 0, 200, 200)), // panel, buried underneath
        ];
        assert_eq!(
            resolve_topmost_window_id((100.0, 100.0), None, &occluder_above_panel),
            Some(1),
            "the occluder is genuinely on top and must win -- reaching past it \
             to the panel is the exact bug #906 fixes"
        );

        // Same panel, no occluder in front of it this time -> the panel
        // itself is correctly the topmost hit.
        let panel_alone = [(2u32, frame(0, 0, 200, 200))];
        assert_eq!(
            resolve_topmost_window_id((100.0, 100.0), None, &panel_alone),
            Some(2)
        );
    }

    /// Explicit fail-CLOSED decision (Definition of Done): neither the SLS
    /// hit nor any registry record can answer -> `None`, which
    /// `select_macos_pointer_target` must treat as "hide," never "show."
    #[test]
    fn resolve_topmost_window_id_fails_closed_when_neither_source_has_a_hit() {
        assert_eq!(resolve_topmost_window_id((100.0, 100.0), None, &[]), None);
        let registry_records = [(1u32, frame(500, 500, 10, 10))];
        assert_eq!(
            resolve_topmost_window_id((100.0, 100.0), None, &registry_records),
            None
        );
    }

    #[test]
    fn pointer_target_kind_disambiguates_same_window_id_visibility_state() {
        let local = vec![(7u32, frame(0, 0, 100, 100))];
        let remote = vec![(7u32, frame(200, 200, 100, 100))];
        let targets = pointer_targets(&local, &remote);
        let keys: std::collections::HashSet<_> =
            targets.iter().map(|t| (t.kind, t.window_id)).collect();

        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&(PointerTargetKind::LocalShare, 7)));
        assert!(keys.contains(&(PointerTargetKind::RemoteCompositor, 7)));
    }

    #[test]
    fn overlay_delivery_labels_include_remote_and_sharer_surfaces_once() {
        let labels = overlay_delivery_labels(
            vec![
                "remote-window-pointer-a".to_string(),
                "share_overlay_1".to_string(),
            ],
            vec!["share_overlay_1".to_string(), "share_overlay_2".to_string()],
        );

        assert_eq!(
            labels,
            vec![
                "remote-window-pointer-a".to_string(),
                "share_overlay_1".to_string(),
                "share_overlay_2".to_string(),
            ]
        );
    }

    #[test]
    fn pointer_message_round_trips_through_json_in_spec_shape() {
        let msg = PointerMessage {
            window_id: 42,
            user_id: "petal-local-publisher".to_string(),
            x: 0.25,
            y: 0.75,
            visible: true,
            activity: Some(PointerActivity::Click),
            surface_owner_id: Some("peter2".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        // SPEC.md §4.5 plus the optional activity marker for #123/#124 and the
        // optional surfaceOwnerId for Windows shared-surface routing.
        assert!(json.contains("\"windowId\":42"));
        assert!(json.contains("\"userId\":\"petal-local-publisher\""));
        assert!(json.contains("\"visible\":true"));
        assert!(json.contains("\"activity\":\"click\""));
        assert!(json.contains("\"surfaceOwnerId\":\"peter2\""));
        let round_tripped: PointerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.window_id, 42);
        assert_eq!(round_tripped.visible, true);
        assert_eq!(round_tripped.activity, Some(PointerActivity::Click));
        assert_eq!(round_tripped.surface_owner_id.as_deref(), Some("peter2"));
    }

    #[test]
    fn pointer_message_activity_is_optional_for_old_payloads() {
        let round_tripped: PointerMessage = serde_json::from_str(
            r#"{"windowId":7,"userId":"old-client","x":0.1,"y":0.2,"visible":true}"#,
        )
        .unwrap();
        assert_eq!(round_tripped.activity, None);
    }
}
