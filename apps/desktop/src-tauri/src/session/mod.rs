//! In-app screenshare session state: wires `hover_tab`'s share/unshare toggle
//! to real per-window capture (`capture.rs`) and real LiveKit publish
//! (`transport::publisher`), and a real `join_room` join flow (SPEC.md §4.6)
//! to the room connection both of those ride on.
//!
//! ## What's real vs. what's a stand-in (updated: real join flow)
//!
//! **Real, permanent:** the capture-start/publish, capture-stop/unpublish
//! lifecycle per window, and the "share a second window while the first is
//! still sharing" multi-window path (SPEC.md §4.3) -- each shared window gets
//! its own `WindowCapture` + `PublishedTrack`, independent of the others.
//! **Also now real:** `join_room` (below) connects to a real, durable room
//! (looked up/created via `rooms::RoomsState`, LiveKit room name derived from
//! the local record's id -- see `rooms::livekit_room_name`) under the real
//! user identity/display name passed in from the frontend's onboarding store,
//! not a hardcoded constant.
//!
//! **RETIRED this task, not just superseded:** the old `DEV_ROOM_NAME`
//! (`"petal-dev-room"`) / `DEV_IDENTITY` (`"petal-local-publisher"`)
//! constants and the "connect to the dev room lazily on first share"
//! fallback (formerly `ensure_room_connected`) are DELETED, not kept as a
//! default -- exactly as this module's own prior doc comment said should
//! happen once a real join flow exists. `start_share` now just reads the
//! room this process already joined via `join_room` (`SessionInner.joined`)
//! and fails clearly if there isn't one -- sharing a window before joining a
//! room is now a real, surfaced error (`ShareSessionError::NotInRoom`)
//! instead of silently joining an ad-hoc dev room. See CLAUDE.md for the retirement note
//! and what (if anything) still points at the old behavior (nothing does --
//! grepped after this change).
//!
//! `telepointer.rs`'s own `DEV_USER_ID` stand-in constant is retired the same
//! way: `start_receiver_for_room`/the sender loop now read the real identity
//! off `SessionState`'s active room-join info instead of a hardcoded literal.
//!
//! Room teardown policy: **moved from "last unshare" to "leave the room"**
//! (SPEC.md §4.6's `leave_room`, called explicitly by the frontend or
//! implicitly by joining a different room -- see `leave_room`/`join_room`
//! below). Sharing/unsharing windows no longer connects or tears down the
//! room at all -- it only requires one to already exist (`ShareSessionError::
//! NotInRoom` if not). This retires the module's own prior "close the room
//! when the last share ends" policy, which was explicitly flagged as a
//! stand-in tied to "no meeting concept exists yet" -- that meeting concept
//! (a room you explicitly join/leave) now exists.
//!
//! ## Audio lifecycle (SPEC.md §4.9) -- tied to "room connected" (now real:
//! "joined the room"), not to any individual window share
//!
//! Audio is per-*user*, not per-shared-window (SPEC.md §4.9 doesn't gate
//! mic/speaker on screensharing at all -- a meeting with zero shared windows
//! still has a live voice call). Mic publish + speaker playout happen inside
//! `join_room` itself now (moved from `ensure_room_connected`, which used to
//! be the lazy "connect on first share" seam) -- exactly the move this
//! module's doc comment predicted before a join flow existed: "start audio as
//! soon as you're in the room, before/independent of any window share."
//! Joining a different room while already in one, or leaving, tears audio
//! down/rebuilds it the same way the room connection itself is torn down/
//! rebuilt (see `leave_room`). Sharing a second, third, or fourth window does
//! NOT publish additional mic tracks -- one mic track per process per room,
//! independent of how many windows that process happens to be sharing
//! (including zero).
//!
//! ## Concurrent-share cap (SPEC.md §4.3: "4 windows per user")
//!
//! `start_share` refuses a 5th concurrent share outright (see
//! `MAX_CONCURRENT_SHARES` / `ShareSessionError::TooManyShares`) rather than
//! silently dropping an existing one -- SPEC.md says "refuse the new share
//! (or make the user drop one first)"; refusing is the smaller, less
//! surprising behavior (a user who intentionally kept 4 windows live should
//! never have one of them silently vanish because they toggled a 5th).
//! Surfaced to the frontend via the exact same `share-error` event
//! `hover_tab.rs` already emits for other `ShareSessionError` variants -- no
//! second error channel.
//!
//! ## Focus model (SPEC.md §8's open question, resolved this session)
//!
//! SPEC.md §4.3 requires "only the focused shared window streams at full
//! fps/resolution; unfocused shares fall to a low-fps, lower-res glanceable
//! layer," but explicitly leaves open (§8) whether "focused" is
//! **sharer-decided** or **per-receiver** ("each viewer picks which remote
//! window is full-rate for them"). Per-receiver is the better experience in
//! principle (two viewers could care about different windows at once) but
//! costs per-receiver simulcast-layer selection. Full-tier window shares now
//! use explicit-layer simulcast (#181): one readable half-resolution layer
//! beside the top layer, avoiding LiveKit's low-fps screenshare defaults.
//! Reduced shares stay single-layer. Building real per-receiver quality
//! selection still needs receiver-side layer-request policy and live
//! validation, which is a bigger lift than this focus policy scopes to.
//!
//! **Decision: sharer-decided focus, v1 policy = "most recently
//! toggled-on share is focused."** Concretely: every call to `start_share`
//! makes its window the new focused one; the previously-focused share (if
//! any) drops to `ShareQuality::Reduced`. `stop_share`-ing the focused
//! window promotes the most-recently-started of the remaining shares (if
//! any) back to `Full`. Active remote control is treated as live viewer
//! demand and keeps/promotes that window at `Full` while control is active;
//! otherwise a controller can be interacting with a non-focused share that
//! is still capped at the glanceable 4fps tier.
//!
//! Why this over the cursor-hit-test alternative: `hover_tab.rs` already has
//! a cheap cursor hit-test that could make "the window your cursor is
//! currently over" the live focus signal. That's tempting but wrong for this
//! feature: the hit-test only fires while the mouse is over a *shareable
//! window a viewer could hover to reshare*, which is a totally different
//! condition from "which of MY OWN active shares do I care about right
//! now" -- most of the time the sharer's cursor is elsewhere entirely
//! (e.g. hovering the meeting chrome, or a window that isn't shared at
//! all), which would leave focus undefined or flapping unpredictably. It
//! also only reflects the *local* user's mouse, which has no clear meaning
//! once this is sharer-decided-but-broadcast-to-all-viewers (a receiver
//! would see the quality tier change every time the sharer's mouse
//! wandered over an unrelated window). "Most recently toggled on" is a
//! stable, explicit, low-surprise signal driven by the one action that
//! already exists (clicking the hover-tab share pill) rather than a passive
//! side effect of mouse position. Issue #37 adds a bounded exception to that
//! local-focus baseline: receiver-origin viewer demand (`petal.viewer-demand`)
//! and active remote control both count as live demand, so a passively viewed
//! or controlled non-focused share remains at `Full` until demand closes or
//! expires.

mod commands;
mod room;
mod share;
mod url_refresh;

pub(crate) use crate::camera_session::{
    ensure_camera_published, repair_camera_publication_after_reconnect, stop_camera_publish,
};
pub use commands::{
    set_share_remote_control_allowed, share_remote_control_allowed,
    __cmd__current_room, __cmd__join_room_command, __cmd__leave_room_command,
    __cmd__remote_control_allowed, __cmd__remote_control_policy, __cmd__room_presence,
    __cmd__set_remote_control_allowed, __cmd__set_remote_control_policy,
    __cmd__set_share_remote_control_allowed, __cmd__set_share_resolution,
    __cmd__share_remote_control_allowed, __tauri_command_name_current_room,
    __tauri_command_name_join_room_command, __tauri_command_name_leave_room_command,
    __tauri_command_name_remote_control_allowed, __tauri_command_name_remote_control_policy,
    __tauri_command_name_room_presence, __tauri_command_name_set_remote_control_allowed,
    __tauri_command_name_set_remote_control_policy,
    __tauri_command_name_set_share_remote_control_allowed,
    __tauri_command_name_set_share_resolution,
    __tauri_command_name_share_remote_control_allowed,
    current_room, join_room_command, leave_room_command, remote_control_allowed,
    remote_control_policy, room_presence, set_remote_control_allowed, set_remote_control_policy,
    set_share_resolution,
};
pub(crate) use room::cleanup_for_forced_disconnect;
#[allow(unused_imports)]
pub use room::RoomLeftEvent;
pub use room::{join_room, leave_room};
#[cfg(test)]
pub(crate) use share::VIEWER_DEMAND_STALE_AFTER;
pub(crate) use share::MAX_CONCURRENT_SHARES;
pub(crate) use share::{
    expire_stale_viewer_demands, note_passive_viewer_demand, note_remote_interaction,
    reconcile_quality_for_window, repair_active_share_publication,
    repair_active_share_publications_after_reconnect,
    repair_local_track_publication_after_reconnect, restart_active_shares_after_wake,
    set_share_priority, start_share_with_system_picker_filter, ReconnectRepairGuard,
    SharedWindowScreenStatus, ViewerDemandEvent, ViewerDemandUpdate,
};
pub use share::{
    promote_quality_for_remote_control, reconcile_quality_after_remote_control_release,
    start_share, stop_share,
};
pub(crate) use share::{stop_share_explained, StopShareAnalytics};

use crate::sync_ext::MutexExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use crate::remote_control_core::RemoteControlPolicy;
use std::sync::{Arc, Mutex};

use crate::capture::CaptureError;
// Re-exported so `crate::session::RoomGeneration` keeps working for the many
// external consumers that imported it through the session module.
pub(crate) use crate::room_generation::RoomGeneration;
use crate::transport::audio::{AudioError, MicTrack};
use crate::transport::publisher::{RoomConnection, RoomConnectionError};

use room::RoomJoinInfo;
use share::{focused_window_of, ActiveShare, PassiveViewerDemand, ViewerDemandKey};
#[derive(Debug, thiserror::Error, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum ShareSessionError {
    #[error("Screen Recording permission has not been granted")]
    PermissionDenied,
    #[error("window {0} not found (closed, or invalid id)")]
    WindowNotFound(u32),
    /// #712: mirrors `WindowNotFound` for a `SharedSourceKind::Display` share
    /// whose display id (stored in the same `u32` slot as a window id) is no
    /// longer in `SCShareableContent::displays()` -- genuine disconnection,
    /// not a window closing.
    #[error("display {0} not found (disconnected, or invalid id)")]
    DisplayNotFound(u32),
    #[error("capture failed: {0}")]
    Capture(String),
    #[error("missing LiveKit configuration: {0}")]
    Config(String),
    #[error("failed to connect to LiveKit room: {0}")]
    RoomConnect(String),
    #[error("timed out joining the LiveKit room")]
    JoinTimeout,
    #[error("already sharing the maximum of {0} windows -- stop one before starting another")]
    TooManyShares(usize),
    #[error("microphone unavailable: {0}")]
    Microphone(String),
    #[error("not currently in a room -- join a room before sharing a window")]
    NotInRoom,
}

impl From<CaptureError> for ShareSessionError {
    fn from(e: CaptureError) -> Self {
        match e {
            CaptureError::PermissionDenied => Self::PermissionDenied,
            CaptureError::WindowNotFound(id) => Self::WindowNotFound(id),
            CaptureError::DisplayNotFound(id) => Self::DisplayNotFound(id),
            CaptureError::ScreenCaptureKit(msg) => Self::Capture(msg),
        }
    }
}

impl From<RoomConnectionError> for ShareSessionError {
    fn from(e: RoomConnectionError) -> Self {
        match e {
            RoomConnectionError::Connect(err) => Self::RoomConnect(err.to_string()),
            RoomConnectionError::InvalidVideoConfig(msg) => Self::Capture(msg),
        }
    }
}

impl From<crate::meeting_core::RoomJoinError> for ShareSessionError {
    fn from(error: crate::meeting_core::RoomJoinError) -> Self {
        match error {
            crate::meeting_core::RoomJoinError::Config(message) => Self::Config(message),
            crate::meeting_core::RoomJoinError::RoomConnect(message) => Self::RoomConnect(message),
        }
    }
}

impl From<AudioError> for ShareSessionError {
    fn from(e: AudioError) -> Self {
        Self::Microphone(e.to_string())
    }
}

#[derive(Default)]
struct SessionInner {
    /// The room this process is currently joined to via `join_room`, if any.
    /// `None` before the first `join_room` call, or after `leave_room`.
    joined: Option<RoomJoinInfo>,
    shares: HashMap<u32, ActiveShare>,
    /// Monotonic terminal generation for each window id. Async failure paths
    /// may apply UI/control effects only when their generation remains this
    /// window's most recently stopped share.
    last_stopped_share_seq: HashMap<u32, u64>,
    /// Passive receiver demand keyed by shared source window and viewer
    /// identity. A receiver owns one entry while it has that remote
    /// compositor window open and visible; stale entries are expired by the
    /// viewer-demand room watcher.
    viewer_demands: HashMap<ViewerDemandKey, PassiveViewerDemand>,
    /// Highest viewer-demand sequence observed per viewer/window. Kept even
    /// after `closed` so an older delayed heartbeat cannot reopen demand.
    viewer_demand_sequences: HashMap<ViewerDemandKey, u64>,
    /// Source of `ActiveShare::started_seq` values; incremented on every
    /// `start_share`, never reused or reset.
    next_share_seq: u64,
    /// This process's published microphone track, if the room connection has
    /// one (see module doc comment on the audio lifecycle -- tied to "joined
    /// a room," not to any individual window share). `None` before
    /// `join_room` completes its audio setup, or if mic publish failed (a
    /// missing/denied microphone should not prevent joining -- see
    /// `join_room`'s handling). `Arc`-wrapped (not just `MicTrack`) so
    /// `resilience::MicWatchHandle`'s device-hot-swap poll (SPEC.md §4.8) can
    /// hold a cheap clone of the current mic track without taking this
    /// struct's own mutex on every poll tick -- see `MicWatchHandle::new`'s
    /// call site in `join_room` below.
    mic: Option<Arc<MicTrack>>,
    /// Keeps this process's speaker playout enabled for as long as the room
    /// connection lives (SPEC.md §4.9). `Arc`-wrapped for the same reason as
    /// `mic`, so resilience's device-watch poll can hold a cheap clone;
    /// dropping the last wrapper still tears down playout.
    playout: Option<Arc<crate::transport::audio::SpeakerPlayout>>,
    /// The published local webcam, if the Video control turned it on.
    /// Torn down by `stop_camera_publish` and by
    /// `leave_room` (the AVCaptureSession/camera light is not LiveKit's to
    /// stop -- `Room::close()` alone would leave the camera running).
    camera: Option<crate::camera_session::ActiveCamera>,
}

impl SessionInner {
    /// The currently-focused window id, per this module's "most recently
    /// toggled-on share is focused" policy: the share with the highest
    /// `started_seq`. `None` when nothing is shared.
    fn focused_window(&self) -> Option<u32> {
        focused_window_of(
            self.shares
                .iter()
                .map(|(id, share)| (*id, share.started_seq)),
        )
    }
}

/// The user's intended local publishes, snapshotted at the moment a room is
/// left, so a rejoin of the SAME room can reconcile actual publish state back
/// to what the user wanted (the fix for the live 2026-07-30 incident where a
/// leave→rejoin re-published the mic — join-driven — but silently dropped the
/// camera, leaving the Video toggle ON with no track for 3.5 minutes).
///
/// This struct is THE inventory of local publish state a rejoin must
/// reconcile. If a new locally-published track type is ever added, it must be
/// added HERE (and consumed in `room.rs`'s `spawn_local_publish_reconcile`),
/// not bolted onto the join tail ad hoc:
/// - **mic** — join-driven; `join_room`'s audio tail republishes it on every
///   join, honoring `desired_mic_muted`. Nothing to carry over.
/// - **camera** — toggle-driven; carried here as `camera_on`.
/// - **window shares** — toggle-driven; carried here as `shares`.
///
/// Room-scoped on purpose: rejoining the *same* room restores what the user
/// was publishing to that room; joining a *different* room never auto-starts
/// the camera or auto-shares windows into an audience that never saw them.
#[derive(Debug, Clone, Default)]
pub(crate) struct LeavePublishCarryover {
    /// `rooms.json` id of the room that was left.
    room_id: String,
    /// Whether the user's camera intent was ON when the room was left.
    camera_on: bool,
    /// The windows that were actively shared when the room was left, in
    /// start order, with their last known frames (for the share border).
    shares: Vec<(u32, crate::hover_tab::WindowFrame)>,
}

/// The carryover's consumable payload for the room being (re)joined.
#[derive(Debug, Clone, Default)]
pub(crate) struct PublishReconcilePlan {
    pub(crate) camera_on: bool,
    pub(crate) shares: Vec<(u32, crate::hover_tab::WindowFrame)>,
}

/// App-wide screenshare session state. Registered as Tauri managed state in
/// `lib.rs`.
pub struct SessionState {
    inner: Mutex<SessionInner>,
    /// The user's *intended* camera-publish state, independent of whether a
    /// camera track currently exists (same pattern as `desired_mic_muted`
    /// below). Set true/false ONLY at user-action boundaries
    /// (`start_camera_publish_command` / `stop_camera_publish_command`), by
    /// the rejoin reconcile when it consumes a carryover, and cleared by the
    /// camera self-heal loop when its bounded retries terminally fail (so the
    /// UI toggle is never left claiming ON while nothing publishes and
    /// nothing is still trying). Internal teardown (`leave_room`,
    /// `set_camera_device`'s stop+restart) deliberately does NOT touch it.
    desired_camera_on: AtomicBool,
    /// One-shot, room-scoped snapshot of local publish intent taken by
    /// `cleanup_left_room`; consumed by the next `join_room`'s reconcile.
    leave_publish_carryover: Mutex<Option<LeavePublishCarryover>>,
    /// Ensures at most one camera self-heal loop runs at a time (see
    /// `camera_session::ensure_camera_published`).
    camera_heal_active: AtomicBool,
    /// Serializes camera start/stop/device-switch sequences process-wide —
    /// the unified replacement for the old `CameraDevicePreferences.switch_lock`
    /// (equivalent serialization, moved onto the session so the shared
    /// `camera_session` module can hold it).
    camera_control_lock: tokio::sync::Mutex<()>,
    /// The user's *intended* mic-muted state, independent of whether a mic
    /// track currently exists to apply it to. Set by `set_mic_muted`
    /// (menubar/`ControlButton` mute toggle) and read by `join_room` so a
    /// mute requested *before* any room/mic exists yet (e.g. muting from the
    /// menubar pill before ever joining a room) is honored the instant a mic
    /// track is actually published, rather than silently reverting to
    /// unmuted on connect. Defaults to muted: joining a meeting is audio-off
    /// until the user explicitly unmutes.
    desired_mic_muted: AtomicBool,
    /// The most recently toggled-on-or-off shared window, independent of
    /// whether it's *currently* shared. Backs the global keyboard shortcut
    /// (SPEC.md §4.2: "a global shortcut to toggle the last-shared window")
    /// -- see `shortcuts.rs`. Updated by both `start_share` and `stop_share`
    /// (toggling a window either direction makes it "the last one you
    /// touched"), so the shortcut always acts on whichever window the user
    /// most recently interacted with via the hover-tab pill, not just
    /// whichever happens to still be actively sharing.
    last_toggled_window: Mutex<Option<u32>>,
    /// Meeting-scoped remote-control allow switch. Seeded from the frontend's
    /// persisted global preference on every room join, then mutable for the
    /// current meeting only. Defaults to allowed per issue #122; the
    /// replay gate still keeps #109's PID-scoped safety rails.
    /// Stored as `RemoteControlPolicy::as_u8` (see remote_control_core.rs):
    /// `remote_control_policy` is the LIVE meeting gate (the per-meeting pill
    /// flips it between Off and the default); `remote_control_default_policy`
    /// is what "on" restores to, seeded from Settings on join and by
    /// `set_remote_control_policy`.
    remote_control_policy: AtomicU8,
    remote_control_default_policy: AtomicU8,
    /// Incremented on every joined-room generation and invalidated on leave.
    /// Per-room watcher loops hold a snapshot token so stale loops from an
    /// older room cannot mutate UI/native state after a fast rejoin.
    room_generation: Arc<AtomicU64>,
    /// Monotonic guard for async mic mute applies. The UI state is
    /// `desired_mic_muted`; this counter prevents an older spawned SDK call
    /// from applying after a newer user intent.
    mic_mute_generation: Arc<AtomicU64>,
    mic_mute_apply_lock: Arc<tokio::sync::Mutex<()>>,
    /// #845: transient mic ducking while the AI-chat assistant's local
    /// playback is audible (see `ai_chat::audio::MicDuckGate`) -- separate
    /// from `desired_mic_muted` so the user's own mute intent/UI state is
    /// never touched by it. The live track's EFFECTIVE mute is
    /// `desired_mic_muted || ai_chat_ducking`; see `apply_effective_mic_mute`.
    ai_chat_ducking: AtomicBool,
    /// Serializes shared-window metadata writes. A stale title-clear waits,
    /// revalidates, and cannot race a new share's title publish.
    share_metadata_apply_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(SessionInner::default()),
            desired_camera_on: AtomicBool::new(false),
            leave_publish_carryover: Mutex::new(None),
            camera_heal_active: AtomicBool::new(false),
            camera_control_lock: tokio::sync::Mutex::new(()),
            desired_mic_muted: AtomicBool::new(true),
            last_toggled_window: Mutex::new(None),
            remote_control_policy: AtomicU8::new(RemoteControlPolicy::default().as_u8()),
            remote_control_default_policy: AtomicU8::new(RemoteControlPolicy::default().as_u8()),
            room_generation: Arc::new(AtomicU64::new(0)),
            mic_mute_generation: Arc::new(AtomicU64::new(0)),
            mic_mute_apply_lock: Arc::new(tokio::sync::Mutex::new(())),
            ai_chat_ducking: AtomicBool::new(false),
            share_metadata_apply_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl SessionState {
    pub(crate) fn begin_room_generation(&self) -> RoomGeneration {
        let value = self.room_generation.fetch_add(1, Ordering::SeqCst) + 1;
        RoomGeneration::new(self.room_generation.clone(), value)
    }

    pub(crate) fn invalidate_room_generation(&self) {
        self.room_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// A non-incrementing token for the CURRENT room generation, for loops
    /// (the camera self-heal, the rejoin publish reconcile) that must stop
    /// acting the moment the room is left or superseded.
    pub(crate) fn current_room_generation(&self) -> RoomGeneration {
        RoomGeneration::new(
            self.room_generation.clone(),
            self.room_generation.load(Ordering::SeqCst),
        )
    }

    /// The user's intended camera state (see `desired_camera_on`).
    pub(crate) fn camera_intent(&self) -> bool {
        self.desired_camera_on.load(Ordering::SeqCst)
    }

    pub(crate) fn set_camera_intent(&self, on: bool) {
        self.desired_camera_on.store(on, Ordering::SeqCst);
    }

    /// Record the local-publish intent snapshot at leave time (see
    /// `LeavePublishCarryover`). Overwrites any previous unconsumed snapshot:
    /// only the most recent leave is a rejoin candidate.
    pub(crate) fn record_leave_publish_carryover(
        &self,
        room_id: String,
        camera_on: bool,
        shares: Vec<(u32, crate::hover_tab::WindowFrame)>,
    ) {
        *self.leave_publish_carryover.lock_unpoisoned() = Some(LeavePublishCarryover {
            room_id,
            camera_on,
            shares,
        });
    }

    /// Consume the leave carryover for the room being joined. ALWAYS clears
    /// the stored snapshot (one-shot); returns a non-empty plan only when the
    /// joined room matches the room that was left, so intent never leaks into
    /// a different room.
    pub(crate) fn take_leave_publish_carryover(&self, room_id: &str) -> PublishReconcilePlan {
        let carryover = self.leave_publish_carryover.lock_unpoisoned().take();
        match carryover {
            Some(c) if c.room_id == room_id => PublishReconcilePlan {
                camera_on: c.camera_on,
                shares: c.shares,
            },
            _ => PublishReconcilePlan::default(),
        }
    }

    /// Try to become the single active camera self-heal loop. Returns false
    /// if one is already running (it will converge on the current intent).
    pub(crate) fn try_begin_camera_heal(&self) -> bool {
        self.camera_heal_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub(crate) fn end_camera_heal(&self) {
        self.camera_heal_active.store(false, Ordering::SeqCst);
    }

    /// Serialized against `set_camera_device`/the manual toggle, same lock
    /// discipline as the commands in `camera_session` (mirrors the Windows
    /// `SessionState::lock_camera_control`).
    pub(crate) async fn lock_camera_control(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.camera_control_lock.lock().await
    }

    pub(crate) fn is_in_room(&self) -> bool {
        self.inner.lock_unpoisoned().joined.is_some()
    }

    /// Whether the local webcam is currently being published to the room.
    /// This is the native-publish state, not the webview-local self-preview
    /// stream. Used by secondary controls like the menubar popover so they
    /// reflect the real room-visible camera state instead of keeping a local
    /// placeholder toggle.
    pub(crate) fn camera_publishing(&self) -> bool {
        self.inner.lock_unpoisoned().camera.is_some()
    }

    /// Take the current camera out of the session, if any.
    pub(crate) fn take_active_camera(&self) -> Option<crate::camera_session::ActiveCamera> {
        self.inner.lock_unpoisoned().camera.take()
    }

    /// Store the camera into the session, atomically re-checking that the
    /// room is still joined AND no camera raced on (leave_room can race a
    /// mid-start camera). Takes the value out of `camera` ONLY on success; on
    /// rejection the caller keeps ownership and tears the capture down itself.
    pub(crate) fn put_active_camera(
        &self,
        camera: &mut Option<crate::camera_session::ActiveCamera>,
    ) -> bool {
        let mut guard = self.inner.lock_unpoisoned();
        let stored = match camera.as_ref() {
            Some(_) => guard.joined.is_some() && guard.camera.is_none(),
            None => true,
        };
        if stored {
            guard.camera = camera.take();
        }
        stored
    }

    /// Take the camera ONLY when it is the capture `status` belongs to — a
    /// stop+restart since the caller's last poll must leave the newer camera
    /// alone. Used by the loss monitor.
    pub(crate) fn take_active_camera_matching(
        &self,
        status: &crate::transport::camera::CameraStatus,
    ) -> Option<crate::camera_session::ActiveCamera> {
        let mut guard = self.inner.lock_unpoisoned();
        if guard
            .camera
            .as_ref()
            .map(|camera| camera.status.same_capture(status))
            .unwrap_or(false)
        {
            guard.camera.take()
        } else {
            None
        }
    }

    /// #713 camera reconnect publication-repair snapshot: the current room
    /// connection + identity + the live camera's published track, read under
    /// ONE lock acquisition while the reconnect guard is still current — same
    /// "check currency, then snapshot, under the same lock" shape as
    /// `share.rs`'s window-share snapshots and `mic_reconnect_repair_snapshot`,
    /// so a leave/rejoin racing this read can never hand back state from a
    /// stale generation.
    pub(crate) fn camera_reconnect_repair_snapshot(
        &self,
        reconnect_guard: &ReconnectRepairGuard,
    ) -> Option<(
        Arc<crate::transport::publisher::RoomConnection>,
        String,
        Arc<crate::transport::publisher::PublishedTrack>,
    )> {
        let guard = self.inner.lock_unpoisoned();
        if !reconnect_guard.is_current_with_inner(&guard) {
            return None;
        }
        match (&guard.joined, &guard.camera) {
            (Some(joined), Some(cam)) => Some((
                joined.room_connection.clone(),
                joined.identity.clone(),
                cam.published.clone(),
            )),
            _ => None,
        }
    }

    pub(crate) fn remote_control_allowed(&self) -> bool {
        self.remote_control_policy().allows_requests()
    }

    /// Legacy boolean setter (per-meeting pill / `set_remote_control_allowed`):
    /// `false` -> Off, `true` -> the stored default policy (never Auto unless
    /// that is the default).
    pub(crate) fn set_remote_control_allowed(&self, allowed: bool) {
        let next = RemoteControlPolicy::from_allowed(allowed, self.remote_control_default_policy());
        self.remote_control_policy.store(next.as_u8(), Ordering::Relaxed);
    }

    pub(crate) fn remote_control_policy(&self) -> RemoteControlPolicy {
        RemoteControlPolicy::from_u8(self.remote_control_policy.load(Ordering::Relaxed))
    }

    pub(crate) fn remote_control_default_policy(&self) -> RemoteControlPolicy {
        RemoteControlPolicy::from_u8(self.remote_control_default_policy.load(Ordering::Relaxed))
    }

    /// Seed BOTH the live meeting policy and the default it restores to.
    /// Used on join (from Settings) and by the autotest rig.
    pub(crate) fn seed_remote_control_policy(&self, policy: RemoteControlPolicy) {
        self.remote_control_default_policy
            .store(policy.as_u8(), Ordering::Relaxed);
        self.remote_control_policy.store(policy.as_u8(), Ordering::Relaxed);
    }

    /// Settings change (possibly mid-meeting): always update the default,
    /// but if the per-meeting pill has the live gate OFF, leave it off --
    /// the meeting route renders that pill from a one-time read and must
    /// not silently disagree with reality (adversarial review P3). Turning
    /// the default Off turns the live gate off too.
    pub(crate) fn set_remote_control_policy(&self, policy: RemoteControlPolicy) {
        self.remote_control_default_policy
            .store(policy.as_u8(), Ordering::Relaxed);
        if self.remote_control_policy().allows_requests() || !policy.allows_requests() {
            self.remote_control_policy.store(policy.as_u8(), Ordering::Relaxed);
        }
    }

    /// Live published microphone track, if this process is currently in a
    /// room with audio enabled. Used by Settings' device picker to hot-swap
    /// the recording device without reaching into the session lock.
    pub(crate) fn current_mic(&self) -> Option<Arc<MicTrack>> {
        let guard = self.inner.lock_unpoisoned();
        guard.mic.clone()
    }

    pub(crate) fn current_playout(
        &self,
    ) -> Option<Arc<crate::transport::audio::SpeakerPlayout>> {
        let guard = self.inner.lock_unpoisoned();
        guard.playout.clone()
    }

    /// #713 reconnect publication-repair snapshot for the mic: the current
    /// room connection + mic track, read together under ONE lock acquisition
    /// while the reconnect guard is still current -- same "check currency,
    /// then snapshot, under the same lock" shape as `share.rs`'s
    /// `active_share_publication_repair_snapshot`, so a leave/rejoin racing
    /// this read can never hand back a room connection or mic track from a
    /// stale generation.
    fn mic_reconnect_repair_snapshot(
        &self,
        reconnect_guard: &ReconnectRepairGuard,
    ) -> Option<(Arc<RoomConnection>, Arc<MicTrack>)> {
        let guard = self.inner.lock_unpoisoned();
        if !reconnect_guard.is_current_with_inner(&guard) {
            return None;
        }
        let room_connection = guard.joined.as_ref()?.room_connection.clone();
        let mic = guard.mic.clone()?;
        Some((room_connection, mic))
    }

    /// Current mic-muted state: reads the live track's real state if a mic
    /// track has been published, otherwise falls back to the last
    /// `set_mic_muted` intent (`desired_mic_muted`) so the UI can show the
    /// right state even before any room/mic connection exists yet. There is
    /// always a concrete answer (never `None`) -- this is the single source
    /// of truth `get_menubar_state`/the frontend popover read from.
    pub(crate) fn mic_muted(&self) -> bool {
        self.desired_mic_muted.load(Ordering::SeqCst)
    }

    /// Mute/unmute the mic (SPEC.md §4.9, `hover_tab.rs`/`menubar.rs`'s real
    /// mic-mute wiring -- see the `set_mic_muted` Tauri command in
    /// `lib.rs`). Always records `muted` as the new intent
    /// (`desired_mic_muted`), so a mute requested before any mic track
    /// exists yet still takes effect the instant one is published (see
    /// `join_room`). Also applies it immediately to a live mic
    /// track if one already exists. Returns the resulting muted state --
    /// this always succeeds (there's no failure mode: muting before a mic
    /// exists is a valid, meaningful action, not an error).
    pub(crate) fn set_mic_muted(&self, muted: bool) -> bool {
        self.desired_mic_muted.store(muted, Ordering::SeqCst);
        self.apply_effective_mic_mute();
        muted
    }

    /// #845: engage/release AI-chat mic ducking. Never touches
    /// `desired_mic_muted` (the user's own mute intent/UI state) -- only the
    /// live track's effective mute, computed in `apply_effective_mic_mute`.
    /// A no-op call (duck requested when already ducking, or release when
    /// already released) still safely re-applies the same effective state,
    /// which is harmless and keeps the caller's poll loop simple.
    pub(crate) fn set_ai_chat_ducking(&self, duck: bool) {
        if self.ai_chat_ducking.swap(duck, Ordering::SeqCst) == duck {
            return;
        }
        self.apply_effective_mic_mute();
    }

    #[cfg(test)]
    pub(crate) fn ai_chat_ducking(&self) -> bool {
        self.ai_chat_ducking.load(Ordering::SeqCst)
    }

    /// Apply `desired_mic_muted || ai_chat_ducking` to the live mic track, if
    /// one exists. Shared by `set_mic_muted` and `set_ai_chat_ducking` so
    /// both the user's own mute toggle and #845's transient AI-chat ducking
    /// go through the identical generation-guarded, crash-safe apply path
    /// (see the comment on the SDK call below for why it must be this way).
    fn apply_effective_mic_mute(&self) {
        let muted =
            self.desired_mic_muted.load(Ordering::SeqCst) || self.ai_chat_ducking.load(Ordering::SeqCst);
        let generation = self.mic_mute_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let latest_generation = self.mic_mute_generation.clone();
        let apply_lock = self.mic_mute_apply_lock.clone();
        let mic = {
            let guard = self.inner.lock_unpoisoned();
            guard.mic.clone()
        };
        if let Some(mic) = mic {
            // livekit's LocalAudioTrack::mute()/unmute() internally
            // `tokio::spawn`s (room::participant::add_publication) and PANICS
            // with "no reactor running" when called with no ambient tokio
            // runtime -- exactly what happens when this is reached from the
            // MAIN thread (menubar NSStatusItem click / the sync
            // `toggle_menubar_mic` Tauri command). User-hit hard crash
            // 2026-07-02 (mute click aborted the app; only reproduces with a
            // real published mic track, which PETAL_DISABLE_AUDIO test runs
            // never have). Record the intent synchronously for immediate UI
            // reads, then run the SDK call inside tauri's tokio runtime
            // context without blocking the AppKit/Tauri command caller.
            tauri::async_runtime::spawn(async move {
                let _guard = apply_lock.lock().await;
                if !should_apply_mic_mute(generation, &latest_generation) {
                    log::debug!("session: skipping stale mic mute apply generation {generation}");
                    return;
                }
                mic.set_muted(muted);
            });
        }
    }
}

fn should_apply_mic_mute(generation: u64, latest_generation: &AtomicU64) -> bool {
    latest_generation.load(Ordering::SeqCst) == generation
}

/// #713: mic reconnect publication repair, wiring real LiveKit calls into
/// `share::repair_local_track_publication_after_reconnect`'s shared
/// generation-guarded/bounded-single-retry core. Called from
/// `resilience.rs`'s post-`Reconnected` repair pass, the same seam that
/// already drives `repair_active_share_publications_after_reconnect` for
/// window shares -- this only fires when the vendored SDK's own
/// `handle_restarted` republish attempt timed out and left the local
/// participant with no `petal-mic` publication at all.
pub(crate) async fn repair_mic_publication_after_reconnect(
    app: &tauri::AppHandle,
    state: &SessionState,
    reconnect_guard: &ReconnectRepairGuard,
) {
    let Some((room_connection, mic)) = state.mic_reconnect_repair_snapshot(reconnect_guard) else {
        return;
    };
    let sid = mic.track_sid().to_string();
    let local_publications: Vec<(String, String)> = room_connection
        .room()
        .local_participant()
        .track_publications()
        .values()
        .map(|publication| (publication.sid().to_string(), publication.name()))
        .collect();
    let room = room_connection.room();
    let mic_for_republish = mic.clone();
    let app_for_failure = app.clone();
    share::repair_local_track_publication_after_reconnect(
        "mic",
        &sid,
        crate::transport::audio::MIC_TRACK_NAME,
        &local_publications,
        || state.reconnect_repair_guard_is_current(reconnect_guard),
        move || async move {
            mic_for_republish
                .republish_after_reconnect(&room)
                .await
                .map(|_| mic_for_republish.track_sid().to_string())
                .map_err(|e| e.to_string())
        },
        move |message| {
            crate::resilience::emit_mic_publication_repair_failed(
                &app_for_failure,
                format!(
                    "Reconnect could not restore your microphone -- try muting and unmuting to reconnect it ({message})"
                ),
            );
        },
    )
    .await;
}

#[cfg(test)]
mod tests {

    /// #898 PRIVACY INVARIANT: a window you were sharing in room A must never
    /// follow you into room B. The carryover exists so that rejoining the
    /// SAME room restores what you had; it must return nothing for any other
    /// room. This was previously untested, and the owner reported a share
    /// surviving a join-link room switch -- if that reproduces, this test is
    /// the first place to confirm the session layer is still honest.
    #[test]
    fn leave_carryover_never_restores_shares_into_a_different_room() {
        let state = SessionState::default();
        let frame = crate::hover_tab::WindowFrame {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        state.record_leave_publish_carryover(
            "room-aaaa".to_string(),
            true,
            vec![(4242, frame.clone())],
        );

        // Joining a DIFFERENT room must carry nothing across -- not the
        // shares, and not the camera intent.
        let plan = state.take_leave_publish_carryover("room-bbbb");
        assert!(
            plan.shares.is_empty(),
            "a share must never be republished into a room the user did not share it in"
        );
        assert!(!plan.camera_on, "camera intent must not leak across rooms either");
    }

    #[test]
    fn leave_carryover_restores_into_the_same_room_and_is_one_shot() {
        let state = SessionState::default();
        let frame = crate::hover_tab::WindowFrame {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        state.record_leave_publish_carryover(
            "room-aaaa".to_string(),
            true,
            vec![(4242, frame)],
        );

        let plan = state.take_leave_publish_carryover("room-aaaa");
        assert_eq!(
            plan.shares.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![4242],
            "rejoining the same room restores what the user had"
        );
        assert!(plan.camera_on);

        // One-shot: a later join must not resurrect a stale intent.
        let second = state.take_leave_publish_carryover("room-aaaa");
        assert!(second.shares.is_empty(), "carryover must be consumed exactly once");
    }
    use super::*;

    #[test]
    fn mic_defaults_to_muted_before_join() {
        let state = SessionState::default();
        assert!(state.mic_muted());
    }

    #[test]
    fn mic_muted_tracks_immediate_desired_state_without_live_track() {
        let state = SessionState::default();

        assert!(!state.set_mic_muted(false));
        assert!(!state.mic_muted());

        assert!(state.set_mic_muted(true));
        assert!(state.mic_muted());
    }

    // #845: AI-chat ducking must be layered UNDER the user's own mute
    // intent/UI state, never fold into it -- `mic_muted()` (what the UI and
    // `desired_mic_muted` reflect) must stay exactly what the user set.
    #[test]
    fn ai_chat_ducking_does_not_touch_the_users_own_mute_intent() {
        let state = SessionState::default();
        state.set_mic_muted(false);
        assert!(!state.mic_muted(), "user chose unmuted");

        state.set_ai_chat_ducking(true);
        assert!(state.ai_chat_ducking());
        assert!(
            !state.mic_muted(),
            "ducking must not flip the user's own mute intent/UI state"
        );

        state.set_ai_chat_ducking(false);
        assert!(!state.ai_chat_ducking());
        assert!(!state.mic_muted());
    }

    #[test]
    fn ai_chat_ducking_toggle_is_idempotent() {
        let state = SessionState::default();
        assert!(!state.ai_chat_ducking());
        state.set_ai_chat_ducking(true);
        assert!(state.ai_chat_ducking());
        // A repeated call with the same value (what a poll loop naturally
        // does every tick while playback continues) must be a safe no-op.
        state.set_ai_chat_ducking(true);
        assert!(state.ai_chat_ducking());
        state.set_ai_chat_ducking(false);
        assert!(!state.ai_chat_ducking());
    }

    #[test]
    fn stale_mic_mute_generations_are_not_eligible_to_apply() {
        let latest = AtomicU64::new(2);

        assert!(!should_apply_mic_mute(1, &latest));
        assert!(should_apply_mic_mute(2, &latest));
    }
}
