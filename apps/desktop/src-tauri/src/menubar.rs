//! Menubar pill — a custom, full-color `NSStatusItem` (Petal-Build-Map.md
//! §2.3), plus the popover it opens into.
//!
//! ## Why a custom-drawn `NSImage` instead of a custom `NSView` subclass
//!
//! §2.3 calls for "a custom status-item view." The most robust way to get a
//! full-color, non-template rendering into an `NSStatusItem` without hand
//! writing a `drawRect:`-overriding `NSView` subclass (a much bigger and more
//! fragile undertaking in objc2 -- correct event-forwarding, layer
//! backing, etc.) is to draw the pill into an `NSImage` via
//! `NSImage::imageWithSize_flipped_drawingHandler` (AppKit's closure-based
//! drawing API) and assign it to the status item's existing `NSStatusBarButton`
//! image with `setTemplate(false)`. This is still genuinely custom, full-color
//! drawing (rounded pill, glyph, dot, avatar circle, count) -- just hosted in
//! the button AppKit already gives every `NSStatusItem`, rather than a
//! hand-rolled view class. Re-drawn (a fresh `NSImage`) on every state change.
//!
//! ## Minimal vs. full heuristic (judgment call -- see module docs below)
//!
//! See `effective_mode()`.
//!
//! macOS-only; no-op stubs elsewhere (matching `hover_tab.rs`/`share_border.rs`'s
//! platform-gating style).

use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// Pill/popover visual state. `mic_muted` mirrors the REAL mic-mute state
/// (SPEC.md §4.9, `session::SessionState::mic_muted` -- see
/// `toggle_menubar_mic`/`set_menubar_mic_muted` below). `in_meeting` and
/// `participant_count` are fed from REAL session/presence state (see
/// `update_meeting_state`, called from `session::leave_room` and
/// `presence.rs`'s emit path) -- the old fake `input_level` meter dot is
/// gone (the approved comp, canvas.html §3, has no meter in the pill: the
/// green pill background itself IS the live signal).
#[derive(Debug, Clone, Copy, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MenubarPillState {
    pub mic_muted: bool,
    /// True when this process is currently publishing its native webcam
    /// track (`petal-camera-*`) to the joined room.
    pub camera_publishing: bool,
    /// True when this process is currently joined to a room
    /// (`session::current_room_name().is_some()`), pushed via
    /// `update_meeting_state` rather than polled.
    pub in_meeting: bool,
    /// Real presence roster size (includes the local participant), from
    /// `presence.rs`. 0 when not in a meeting.
    pub participant_count: u32,
    /// True when rendering minimal mode (glyph + dot only) because the menu
    /// bar squeezed us -- see `effective_mode`.
    pub minimal: bool,
}

/// Label of the popover webview (a SvelteKit route), mirroring
/// `hover_tab::HOVER_TAB_LABEL` / `share_border`'s per-panel labels.
pub const MENUBAR_POPOVER_LABEL: &str = "menubar-popover";

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicMuteChanged {
    pub muted: bool,
}

// =============================================================================
// Cross-platform command surface
// =============================================================================

/// Toggle the REAL mic-mute state (SPEC.md §4.9) and redraw the pill.
/// Returns the new `mic_muted` value.
///
/// Real wiring: reads `session::SessionState`'s current mic state (via
/// `app.try_state`, the same pattern `telepointer.rs` already uses to reach
/// managed session state from a non-Tauri-command context), flips it through
/// `SessionState::set_mic_muted` -- which calls the actual
/// `LocalAudioTrack::mute()`/`unmute()` on the published mic track, see
/// `transport::audio`'s module doc comment -- and mirrors the result into
/// the pill's own local `MenubarPillState` purely so `redraw()` has
/// something synchronous to paint from without re-locking session state on
/// every frame.
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn toggle_menubar_mic(app: AppHandle) -> Result<bool, String> {
    let muted = !session_mic_muted(&app);
    set_session_mic_muted(&app, muted);
    let muted = platform::set_mic_muted_and_redraw(&app, muted);
    emit_mic_mute_changed(&app, muted);
    Ok(muted)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn toggle_menubar_mic(app: AppHandle) -> Result<bool, String> {
    use tauri::Manager;
    let state = app
        .try_state::<crate::session::SessionState>()
        .ok_or_else(|| "microphone session unavailable".to_string())?;
    let _transaction = state.lock_mic_control().await;
    let muted = state.set_mic_muted(!state.mic_muted())?;
    emit_mic_mute_changed(&app, muted);
    Ok(muted)
}

/// Explicit set (vs. `toggle_menubar_mic`'s flip) -- the Tauri command the
/// frontend's `ControlButton` mic toggle calls when it already knows the
/// target state (matches the `active`-driven, not toggle-driven, prop shape
/// `ControlButton.svelte` already uses elsewhere). Same real wiring as
/// `toggle_menubar_mic`, just parameterized instead of flipped.
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn set_mic_muted(app: AppHandle, muted: bool) -> Result<bool, String> {
    set_session_mic_muted(&app, muted);
    let muted = platform::set_mic_muted_and_redraw(&app, muted);
    emit_mic_mute_changed(&app, muted);
    Ok(muted)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn set_mic_muted(app: AppHandle, muted: bool) -> Result<bool, String> {
    use tauri::Manager;
    let state = app
        .try_state::<crate::session::SessionState>()
        .ok_or_else(|| "microphone session unavailable".to_string())?;
    let _transaction = state.lock_mic_control().await;
    let muted = state.set_mic_muted(muted)?;
    emit_mic_mute_changed(&app, muted);
    Ok(muted)
}

/// Fetch the current pill state (for the popover / dev route to mirror).
/// This is deliberately read-only: it overlays the session-owned mic/camera
/// truth onto the last painted menubar state without triggering a redraw.
/// Redraws happen from state-changing commands/events.
#[tauri::command]
pub fn get_menubar_state(app: AppHandle) -> MenubarPillState {
    #[cfg(target_os = "macos")]
    {
        let muted = session_mic_muted(&app);
        let camera_publishing = session_camera_publishing(&app);
        let mut state = platform::current_state();
        state.mic_muted = muted;
        state.camera_publishing = camera_publishing;
        state
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut state = MenubarPillState::default();
        state.mic_muted = session_mic_muted(&app);
        state
    }
}

/// Read `session::SessionState::mic_muted()` via managed Tauri state,
/// defaulting to `false` (unmuted) if `SessionState` isn't registered yet
/// (shouldn't happen once `lib.rs`'s `.manage()` call has run, but this
/// mirrors `telepointer.rs`'s own defensive `try_state` use rather than
/// assuming the registration order).
#[cfg(target_os = "macos")]
fn session_mic_muted(app: &AppHandle) -> bool {
    use tauri::Manager;
    app.try_state::<crate::session::SessionState>()
        .map(|state| state.mic_muted())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn session_mic_muted(app: &AppHandle) -> bool {
    use tauri::Manager;
    app.try_state::<crate::session::SessionState>()
        .map(|state| state.mic_muted())
        .unwrap_or(true)
}

#[cfg(target_os = "macos")]
fn session_camera_publishing(app: &AppHandle) -> bool {
    use tauri::Manager;
    app.try_state::<crate::session::SessionState>()
        .map(|state| state.camera_publishing())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn set_session_mic_muted(app: &AppHandle, muted: bool) {
    use tauri::Manager;
    if let Some(state) = app.try_state::<crate::session::SessionState>() {
        state.set_mic_muted(muted);
    } else {
        log::warn!("menubar: SessionState not available yet -- mic mute intent not persisted");
    }
}

fn emit_mic_mute_changed(app: &AppHandle, muted: bool) {
    let _ = app.emit("mic-mute-changed", MicMuteChanged { muted });
}

/// Close the popover panel (called by the popover's own "Leave"/dismiss UI).
#[tauri::command]
pub fn hide_menubar_popover(app: AppHandle) {
    #[cfg(target_os = "macos")]
    {
        platform::hide_popover(&app);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
    }
}

/// Resize the popover panel to fit its real content height (issue #5:
/// native popovers hug their content; the old fixed 280x420 left a blank
/// band or clipped). Called by the popover webview after it measures its
/// own rendered content (on mount, on roster change, on show). Height is
/// clamped to a sane range; beyond the max the popover's own CSS caps the
/// content and scrolls internally.
#[tauri::command]
pub fn resize_menubar_popover(app: AppHandle, height: f64) {
    #[cfg(target_os = "macos")]
    {
        platform::resize_popover(&app, height);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, height);
    }
}

/// Push the REAL meeting state into the pill (issue #4): whether this
/// process is currently in a room, and the live presence roster size.
/// Safe to call from any thread -- the AppKit redraw is marshalled onto the
/// main thread (CLAUDE.md's "AppKit off the main thread" crash class), which
/// matters because the presence event loop that calls this runs on a tokio
/// worker thread.
pub fn update_meeting_state(app: &AppHandle, in_meeting: bool, participant_count: u32) {
    #[cfg(target_os = "macos")]
    {
        let _ = app.run_on_main_thread(move || {
            platform::set_meeting_state_and_redraw(in_meeting, participant_count);
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, in_meeting, participant_count);
    }
}

/// Idempotent init, called from `lib.rs`'s `.setup()` hook.
pub fn init(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        platform::init(app);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
    }
}

// =============================================================================
// macOS implementation
// =============================================================================

#[cfg(target_os = "macos")]
mod platform {
    use super::{MenubarPillState, MENUBAR_POPOVER_LABEL};
    use crate::sync_ext::MutexExt;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{
        define_class, msg_send, sel, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly,
    };
    use objc2_app_kit::{
        NSApplication, NSApplicationDidChangeScreenParametersNotification, NSBezierPath, NSColor,
        NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSStatusBar,
        NSStatusItem,
    };
    use objc2_foundation::{
        NSDictionary, NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
        NSString,
    };
    use std::cell::RefCell;
    use std::sync::Mutex;

    // -------------------------------------------------------------------
    // Pill geometry -- kept narrow per Petal-Build-Map.md §2.3 ("width is
    // the real risk"): the right menu bar is a fixed, shrink-only budget,
    // and an item that doesn't fit silently disappears. Height is hard-capped
    // at 24pt for interactive content (the constraint holds regardless of
    // how tall the bar looks on notched MacBooks).
    // -------------------------------------------------------------------
    const PILL_HEIGHT: f64 = 22.0; // leaves a hairline of margin under the 24pt cap

    // The full layout grows by only 1pt: its larger left-side elements reclaim
    // gap space, while the extra point keeps the 19pt leave circle's left edge
    // and exact right margin unchanged. This minimizes squeeze demotions.
    const PILL_WIDTH_FULL: f64 = 85.0;
    const PILL_WIDTH_MINIMAL: f64 = 34.0; // glyph (+ live dot when in a meeting) only

    // Full-pill layout (canvas.html §3, scaled from its 24pt-tall comp to
    // this 22pt pill): left pad 9, mic 14, gap 5, people icon 13, gap 3,
    // 11pt count text, then the 19pt leave circle right-aligned with a 2pt
    // margin. Positions are derived so this remains one coherent layout table.
    const FULL_MIC_X: f64 = 9.0;
    const FULL_MIC_SIZE: f64 = 14.0;
    const FULL_MIC_TO_PEOPLE_GAP: f64 = 5.0;
    const FULL_PEOPLE_X: f64 = FULL_MIC_X + FULL_MIC_SIZE + FULL_MIC_TO_PEOPLE_GAP;
    const FULL_PEOPLE_SIZE: f64 = 13.0;
    const FULL_PEOPLE_TO_COUNT_GAP: f64 = 3.0;
    const FULL_COUNT_X: f64 = FULL_PEOPLE_X + FULL_PEOPLE_SIZE + FULL_PEOPLE_TO_COUNT_GAP;
    const FULL_COUNT_FONT_SIZE: f64 = 11.0;
    const FULL_COUNT_BASELINE_OFFSET: f64 = 7.0; // re-centered for the 11pt font
    const LEAVE_CIRCLE_SIZE: f64 = 19.0; // 20pt is the exact cap-tangency maximum
    const LEAVE_CIRCLE_MARGIN: f64 = 2.0;
    const MINIMAL_IDLE_GLYPH_SIZE: f64 = 15.0;
    const MINIMAL_MIC_SIZE: f64 = 16.0;

    // Click-zone boundaries (see `click_zone`): the mic zone covers the mic
    // glyph through the midpoint of its following gap; the leave zone covers
    // the right-aligned circle, its margin, and a small left-side slop.
    const MIC_ZONE_MAX_X: f64 = FULL_MIC_X + FULL_MIC_SIZE + FULL_MIC_TO_PEOPLE_GAP / 2.0;
    const LEAVE_ZONE_LEFT_SLOP: f64 = 4.0;
    const LEAVE_ZONE_WIDTH: f64 = LEAVE_CIRCLE_SIZE + LEAVE_CIRCLE_MARGIN + LEAVE_ZONE_LEFT_SLOP;

    const POPOVER_WIDTH: f64 = 280.0;
    const POPOVER_MIN_HEIGHT: f64 = 80.0;
    const POPOVER_MAX_HEIGHT: f64 = 480.0;

    // Approved-comp colors (canvas.html §3) -- the ONLY place these greens/
    // inks appear; the pill is the one full-color surface the design allows.
    const LIVE_GREEN: (f64, f64, f64) = (0.204, 0.780, 0.349); // #34C759
    const DARK_INK: (f64, f64, f64) = (0.024, 0.169, 0.071); // #062B12
    const LEAVE_BG: (f64, f64, f64) = (0.039, 0.039, 0.047); // #0a0a0c
    const LEAVE_RED: (f64, f64, f64) = (1.0, 0.42, 0.369); // #FF6B5E

    static STATE: Mutex<Option<MenubarPillState>> = Mutex::new(None);

    /// The width `redraw()` asked AppKit for on its LAST pass -- what the
    /// squeeze heuristic must compare the granted frame against. This used
    /// to be re-derived from `state.minimal` alone, which broke the moment
    /// width also depended on `in_meeting` (a not-in-meeting pill is minimal-
    /// width by design, and comparing that granted minimal width against a
    /// freshly requested full width on join would spuriously read as
    /// "squeezed").
    static LAST_REQUESTED_WIDTH: Mutex<Option<f64>> = Mutex::new(None);

    /// Which zone of the pill a click at `x` (button-local, points) lands
    /// in. Full in-meeting pill has THREE zones (canvas.html §3): mic glyph
    /// = mute toggle, leave circle = leave meeting, everything between =
    /// open the popover. Minimal mode and the not-in-meeting pill are a
    /// single glyph -- the whole item opens the popover (judgment call: an
    /// invisible sub-glyph hit boundary on such a compact item would be a
    /// mystery mis-click generator; mute/leave both remain reachable inside
    /// the popover).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum ClickZone {
        Mic,
        Body,
        Leave,
    }

    fn click_zone(x: f64, width: f64, minimal: bool, in_meeting: bool) -> ClickZone {
        if !in_meeting || minimal {
            return ClickZone::Body;
        }
        if x <= MIC_ZONE_MAX_X {
            ClickZone::Mic
        } else if x >= width - LEAVE_ZONE_WIDTH {
            ClickZone::Leave
        } else {
            ClickZone::Body
        }
    }

    fn with_state<R>(f: impl FnOnce(&mut MenubarPillState) -> R) -> R {
        let mut guard = STATE.lock_unpoisoned();
        let state = guard.get_or_insert_with(MenubarPillState::default);
        f(state)
    }

    pub fn current_state() -> MenubarPillState {
        with_state(|s| *s)
    }

    /// Set the pill's `mic_muted` visual flag to match the real (already
    /// applied by the caller, via `session::SessionState::set_mic_muted`)
    /// mute state, and redraw. This module's `STATE`/`redraw()` stay purely
    /// visual/synchronous -- the actual mic mute/unmute call happens in the
    /// cross-platform command wrapper above (`toggle_menubar_mic`/
    /// `set_mic_muted`) before this runs, matching the "real state lives in
    /// `session.rs`, this module just paints it" split.
    ///
    /// The redraw is marshalled onto the main thread via `run_on_main_thread`
    /// (immediate when already there, e.g. the pill's own click handler):
    /// Tauri commands run on worker threads, and `redraw()`'s AppKit +
    /// thread-local access is main-thread-only -- previously the redraw was
    /// silently SKIPPED for any mute toggled from the popover/meeting-route
    /// commands (`MainThreadMarker::new()` early-return), so the pill could
    /// show stale mute state until its next unrelated repaint.
    pub fn set_mic_muted_and_redraw(app: &tauri::AppHandle, muted: bool) -> bool {
        with_state(|s| s.mic_muted = muted);
        let _ = app.run_on_main_thread(redraw);
        muted
    }

    /// Same split as `set_mic_muted_and_redraw`: the REAL state lives in
    /// `session.rs`/`presence.rs`; this just mirrors it into the pill's own
    /// synchronous paint state and redraws. Main-thread only (called via
    /// `run_on_main_thread` from `super::update_meeting_state`).
    pub fn set_meeting_state_and_redraw(in_meeting: bool, participant_count: u32) {
        let changed = with_state(|s| {
            let changed = s.in_meeting != in_meeting || s.participant_count != participant_count;
            s.in_meeting = in_meeting;
            s.participant_count = participant_count;
            changed
        });
        if changed {
            redraw();
        }
    }

    // ---------------------------------------------------------------------
    // Minimal-vs-full heuristic
    //
    // There is no public API to ask "how much room is left in the menu bar"
    // before adding an item -- macOS just silently drops items that don't
    // fit (Petal-Build-Map.md §2.3). This is a genuinely unsolved problem in
    // general (the task brief calls it out as a judgment call, not a solved
    // one), so the v1 policy here is:
    //
    //   1. Always ATTEMPT the full pill first (`PILL_WIDTH_FULL`) --
    //      "minimal" is the safe fallback, not the default, so the richer
    //      state is what most users with normal screen-width budgets see.
    //   2. After the status item is inserted (and after every redraw), read
    //      back `NSStatusItem.button.window.frame` -- if AppKit actually
    //      granted us less width than we asked for, a degenerate (near-zero)
    //      height, or the button's window is nil, treat that as "we got
    //      squeezed" and fall back to minimal on the NEXT redraw. (On a menu
    //      bar with zero free room, AppKit doesn't shrink the width
    //      proportionally -- observed on a real, fully-packed menu bar during
    //      development: `isVisible` stayed `true` but the button's window
    //      frame came back as `(0, 0, 46, 0)`, i.e. zero HEIGHT and a
    //      meaningless origin, not a shrunk width. Checking height alongside
    //      width catches this.)
    //   3. Re-check on `NSApplication.didChangeScreenParametersNotification`
    //      (display/resolution changes, and notch-relevant changes to the
    //      available menu-bar width) so a squeeze that resolves later (e.g.
    //      user closes another app's menu-bar item) can opportunistically
    //      upgrade back to full on the next state change.
    //
    // This is reactive, not predictive -- it can't stop the FIRST insertion
    // from silently vanishing if there's truly zero room, but it means a
    // pill that gets rendered starts in the richest state that actually fit,
    // and self-corrects on subsequent redraws rather than staying wrong
    // forever. A predictive version would need to sum sibling
    // `NSStatusItem` widths against `NSScreen` width, which is fragile
    // (third-party items aren't introspectable) and out of scope for v1.
    // ---------------------------------------------------------------------
    fn effective_mode(
        button_window_width: Option<f64>,
        button_window_height: Option<f64>,
        requested_width: f64,
    ) -> bool {
        // Degenerate height (observed real value: 0.0) means AppKit didn't
        // actually give the button any real screen space, regardless of what
        // width it reports -- see doc comment above.
        if matches!(button_window_height, Some(h) if h < 1.0) {
            return true;
        }
        match button_window_width {
            // AppKit reports a width that's meaningfully smaller than what we
            // asked for -- treat as squeezed, fall back to minimal.
            Some(w) if w + 0.5 < requested_width => true,
            // No window (not yet visible / got dropped) -- can't confirm we
            // fit; be conservative and prefer minimal until it resolves.
            None => true,
            _ => false,
        }
    }

    struct MenubarIvars {
        status_item: Retained<NSStatusItem>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "PetalMenubarTarget"]
        #[thread_kind = MainThreadOnly]
        #[ivars = MenubarIvars]
        struct MenubarTarget;

        unsafe impl NSObjectProtocol for MenubarTarget {}

        impl MenubarTarget {
            /// Single click target for the status item button. Hit-tests the
            /// click's x-position within the button against `click_zone` --
            /// per Petal-Build-Map.md §2.3 / canvas.html §3: mic glyph =
            /// mute toggle, leave circle = leave the meeting, pill body =
            /// toggle the popover. Rather than hand-building literal extra
            /// NSStatusItems for the mic/leave hit-zones (real width budget
            /// is already tight), this hit-tests within the single item's
            /// button, which stays within the narrow-width goal.
            #[unsafe(method(statusItemClicked:))]
            fn status_item_clicked(&self, sender: Option<&AnyObject>) {
                let Some(mtm) = MainThreadMarker::new() else {
                    log::warn!(
                        "menubar: statusItemClicked: invoked off the main thread; ignoring"
                    );
                    return;
                };
                let Some(app) = APP_HANDLE.with(|cell| cell.borrow().clone()) else {
                    return;
                };

                let _ = sender;
                let click_x = NSApplication::sharedApplication(mtm)
                    .currentEvent()
                    .map(|event| event.locationInWindow().x)
                    .unwrap_or(f64::MAX);

                let ivars = self.ivars();
                let state = with_state(|s| *s);
                let width = if state.in_meeting && !state.minimal {
                    PILL_WIDTH_FULL
                } else {
                    PILL_WIDTH_MINIMAL
                };

                match click_zone(click_x, width, state.minimal, state.in_meeting) {
                    ClickZone::Mic => {
                        // Real mute (SPEC.md §4.9): read+flip actual session
                        // mic state via the same `super::` helpers the
                        // `toggle_menubar_mic` Tauri command uses, not a
                        // local `platform`-only toggle -- clicking the
                        // pill's mic hit-zone directly must mute the real
                        // published track, not just this module's own
                        // visual state.
                        let muted = !super::session_mic_muted(&app);
                        super::set_session_mic_muted(&app, muted);
                        set_mic_muted_and_redraw(&app, muted);
                        super::emit_mic_mute_changed(&app, muted);
                    }
                    ClickZone::Leave => {
                        // Real leave (issue #4/#5 shared path): the exact
                        // same `session::leave_room` the popover's Leave
                        // button reaches via `leave_room_command` -- room
                        // closed, shares stopped, audio unpublished, and
                        // `session.rs` itself emits `room-left` + resets
                        // this pill via `update_meeting_state`.
                        leave_current_room(&app);
                    }
                    ClickZone::Body => {
                        let button_frame = ivars
                            .status_item
                            .button(mtm)
                            .map(|b| b.window().map(|w| w.frame()));
                        toggle_popover(&app, button_frame.flatten());
                    }
                }
            }
        }
    );

    impl MenubarTarget {
        fn new(mtm: MainThreadMarker, status_item: Retained<NSStatusItem>) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(MenubarIvars { status_item });
            unsafe { msg_send![super(this), init] }
        }
    }

    // Stash the AppHandle for the target's action method (an Objective-C
    // selector can't capture Rust closures' environment) -- main-thread-only
    // by construction (AppKit callbacks always land on the main thread), so
    // a thread-local (not a Mutex) is enough and avoids Sync bounds on
    // AppHandle mattering here.
    thread_local! {
        static APP_HANDLE: RefCell<Option<tauri::AppHandle>> = const { RefCell::new(None) };
    }
    thread_local! {
        static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
        static TARGET: RefCell<Option<Retained<MenubarTarget>>> = const { RefCell::new(None) };
    }

    pub fn init(app: &tauri::AppHandle) {
        let Some(mtm) = MainThreadMarker::new() else {
            log::warn!("menubar: init() called off the main thread; skipping");
            return;
        };

        APP_HANDLE.with(|cell| *cell.borrow_mut() = Some(app.clone()));

        let status_bar = NSStatusBar::systemStatusBar();
        let status_item =
            status_bar.statusItemWithLength(objc2_app_kit::NSVariableStatusItemLength);

        let target = MenubarTarget::new(mtm, status_item.clone());

        if let Some(button) = status_item.button(mtm) {
            unsafe {
                button.setTarget(Some(&target));
                button.setAction(Some(sel!(statusItemClicked:)));
            }
        }

        STATUS_ITEM.with(|cell| *cell.borrow_mut() = Some(status_item));
        TARGET.with(|cell| *cell.borrow_mut() = Some(target));

        redraw();
        create_popover(app);
        observe_screen_parameter_changes();
        log::info!("menubar: NSStatusItem created");
    }

    /// Point 3 of the minimal/full heuristic (see the doc comment above
    /// `effective_mode`): re-`redraw()` whenever the OS posts
    /// `NSApplicationDidChangeScreenParametersNotification` (fired on
    /// display/resolution changes and other menu-bar-geometry-affecting
    /// events), so a squeeze that resolves later -- e.g. the user closes
    /// another app's menu-bar item, freeing up room -- can opportunistically
    /// upgrade back to the full pill on the next notification, without
    /// requiring the user to click anything. The observer block is retained
    /// for the lifetime of the process (never removed) since this pill's
    /// `NSStatusItem` is itself a permanent, app-lifetime singleton.
    fn observe_screen_parameter_changes() {
        let center = NSNotificationCenter::defaultCenter();
        let block = block2::RcBlock::new(
            |_note: std::ptr::NonNull<objc2_foundation::NSNotification>| {
                redraw();
            },
        );
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSApplicationDidChangeScreenParametersNotification),
                None,
                None,
                &block,
            )
        };
        // Leak the observer token deliberately -- see doc comment above (this
        // observer lives for the whole process, matching the NSStatusItem's
        // own lifetime; there is no corresponding "menubar shutdown" path yet
        // to pair a `removeObserver` call with).
        std::mem::forget(observer);
    }

    /// Redraw the pill image for the current state, applying the
    /// minimal/full heuristic (see `effective_mode` doc comment above).
    /// Operates entirely on the module's own thread-locals (the status item
    /// + state), so it needs no `AppHandle`.
    pub fn redraw() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        STATUS_ITEM.with(|cell| {
            let borrow = cell.borrow();
            let Some(status_item) = borrow.as_ref() else {
                return;
            };

            // What we ACTUALLY asked AppKit for last time -- the squeeze
            // heuristic must compare granted-vs-requested from the same
            // pass, or a deliberate width change (e.g. the not-in-meeting
            // pill is minimal-width by design, then the user joins a
            // meeting) would spuriously read as "squeezed". First pass ever:
            // fall back to the full width so a genuinely-missing button
            // window still reads conservative (effective_mode's `None` arm).
            let requested_width = LAST_REQUESTED_WIDTH
                .lock_unpoisoned()
                .unwrap_or(PILL_WIDTH_FULL);

            // Read back actual granted geometry from the LAST redraw to
            // decide whether to flip to minimal THIS redraw (see
            // effective_mode doc comment -- reactive, not predictive).
            // Confirmed on a real, fully-packed menu bar during development
            // (every menu-extra slot taken, no room left): AppKit still
            // reports `NSStatusItem.isVisible == true` in this case, but the
            // button's window is granted a degenerate (0,0,46,0) frame --
            // zero HEIGHT, not a proportionally-shrunk width. So this checks
            // height as well as width, not width alone.
            let button_window_frame = status_item
                .button(mtm)
                .and_then(|b| b.window())
                .map(|w| w.frame());
            let should_be_minimal = effective_mode(
                button_window_frame.map(|f| f.size.width),
                button_window_frame.map(|f| f.size.height),
                requested_width,
            );
            with_state(|s| s.minimal = should_be_minimal);
            let state = with_state(|s| *s);

            // Not-in-meeting renders the minimal-size neutral glyph by
            // design (canvas.html §3 only designs the in-call pill; see
            // issue #4 Notes), independent of the squeeze heuristic.
            let width = if state.minimal || !state.in_meeting {
                PILL_WIDTH_MINIMAL
            } else {
                PILL_WIDTH_FULL
            };
            status_item.setLength(width);
            *LAST_REQUESTED_WIDTH.lock_unpoisoned() = Some(width);

            let image = draw_pill(state, width);
            if let Some(button) = status_item.button(mtm) {
                button.setImage(Some(&image));
            }
        });
    }

    /// Draw the pill into an `NSImage` at `width` x `PILL_HEIGHT` (logical
    /// points, @2x backing). Full color throughout -- `setTemplate(false)`
    /// below is what makes this NOT the plain black/white template rendering
    /// macOS defaults menu-bar icons to (Petal-Build-Map.md §2.3: "full
    /// color IS possible via a custom status-item view").
    fn draw_pill(state: MenubarPillState, width: f64) -> Retained<NSImage> {
        let size = NSSize {
            width,
            height: PILL_HEIGHT,
        };

        let block = block2::RcBlock::new(move |rect: NSRect| -> Bool {
            unsafe { paint(rect, state) };
            Bool::YES
        });

        let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &block);
        image.setTemplate(false);
        // AppKit invokes the drawing handler at the screen's actual backing
        // scale (2x on any Retina display, which is effectively all Macs
        // this app targets), so no manual @2x bitmap juggling is needed here
        // -- just draw in logical points as usual.
        image.setSize(size);
        image
    }

    /// The actual drawing calls, run inside AppKit's drawing-handler block
    /// (current graphics context is already set up by the caller).
    ///
    /// Three renderings (canvas.html §3 + the issue #4 not-in-meeting
    /// judgment call):
    /// - in meeting, full: live-green pill, dark-ink stroke mic glyph,
    ///   dark-ink people icon + participant count, and a separate dark
    ///   circle with a red leave glyph (its own click zone).
    /// - in meeting, minimal (squeezed): light stroke mic glyph + small
    ///   green live dot, no pill background.
    /// - NOT in meeting: the comp only designs the in-call pill, so this is
    ///   a neutral minimal glyph -- dimmer light mic stroke, no dot, no
    ///   green anywhere (green = "live" and nothing is live).
    ///
    /// Glyphs are the comp's own SVG paths hand-converted to
    /// rounded-cap/join NSBezierPath strokes (rects, lines, and circular
    /// arcs with centers/angles derived from the SVG arc commands), drawn in
    /// the image's y-up coordinate space.
    unsafe fn paint(rect: NSRect, state: MenubarPillState) {
        if state.in_meeting && !state.minimal {
            paint_full(rect, state);
        } else {
            paint_minimal(rect, state);
        }
    }

    /// Full in-call pill (canvas.html §3's "OUR ITEM: full-color, as
    /// requested" build).
    unsafe fn paint_full(rect: NSRect, state: MenubarPillState) {
        let height = rect.size.height;
        let width = rect.size.width;

        // Live-green pill background, fully rounded.
        srgb(LIVE_GREEN, 1.0).set();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, height / 2.0, height / 2.0)
            .fill();

        let ink = srgb(DARK_INK, 1.0);

        // Mic glyph, dark ink, stroke style (comp: 12px, stroke-width 2.8).
        draw_mic_glyph(
            FULL_MIC_X,
            (height - FULL_MIC_SIZE) / 2.0,
            FULL_MIC_SIZE,
            &ink,
            2.8,
            state.mic_muted,
        );

        // People icon + participant count, grouped, dark ink (comp: 11px
        // icon, stroke-width 2.6, JetBrains Mono 700 11px count -- drawn
        // with the system's own bold monospaced font; JetBrains Mono isn't
        // installable from native drawing code, see issue #4 Notes).
        draw_people_glyph(
            FULL_PEOPLE_X,
            (height - FULL_PEOPLE_SIZE) / 2.0,
            FULL_PEOPLE_SIZE,
            &ink,
            2.6,
        );
        let count = state.participant_count.min(99);
        let count_string = NSString::from_str(&count.to_string());
        // `+[NSFont monospacedSystemFontOfSize:weight:]` is header-annotated
        // non-null, so objc2 generates a non-nullable binding that panic-ABORTS
        // the whole app on the main thread if the method ever returns nil. It
        // DOES return nil in the wild (font-server hiccup / memory pressure) --
        // this crashed a live 0.6.1 meeting while re-rendering this pill's
        // participant count (`unexpected NULL returned from
        // +[NSFont monospacedSystemFontOfSize:weight:]`). Fetch it NULLABLY via
        // msg_send, fall back to the bold system font, and if even that is nil
        // skip drawing the count rather than crash the app over a cosmetic glyph.
        let font: Option<Retained<NSFont>> = unsafe {
            msg_send![
                NSFont::class(),
                monospacedSystemFontOfSize: FULL_COUNT_FONT_SIZE,
                weight: objc2_app_kit::NSFontWeightBold,
            ]
        };
        let font = font.or_else(|| unsafe {
            msg_send![NSFont::class(), boldSystemFontOfSize: FULL_COUNT_FONT_SIZE]
        });
        if let Some(font) = font {
            let keys: [&NSString; 2] = [
                NSFontAttributeName.as_ref(),
                NSForegroundColorAttributeName.as_ref(),
            ];
            let font_any: Retained<AnyObject> = unsafe { Retained::cast_unchecked(font) };
            let ink_any: Retained<AnyObject> = unsafe { Retained::cast_unchecked(ink.clone()) };
            let objs: [&AnyObject; 2] = [&font_any, &ink_any];
            let attrs = NSDictionary::from_slices(&keys, &objs);
            let point = NSPoint {
                x: FULL_COUNT_X,
                y: height / 2.0 - FULL_COUNT_BASELINE_OFFSET,
            };
            objc2_app_kit::NSStringDrawing::drawAtPoint_withAttributes(
                &*count_string,
                point,
                Some(&attrs),
            );
        }

        // Leave affordance: separate dark circle with a red leave/exit
        // glyph -- its own click zone (see `click_zone`).
        let circle_x = width - LEAVE_CIRCLE_MARGIN - LEAVE_CIRCLE_SIZE;
        let circle_y = (height - LEAVE_CIRCLE_SIZE) / 2.0;
        srgb(LEAVE_BG, 1.0).set();
        NSBezierPath::bezierPathWithOvalInRect(NSRect {
            origin: NSPoint {
                x: circle_x,
                y: circle_y,
            },
            size: NSSize {
                width: LEAVE_CIRCLE_SIZE,
                height: LEAVE_CIRCLE_SIZE,
            },
        })
        .fill();
        let leave_glyph_size = 10.0;
        draw_leave_glyph(
            circle_x + (LEAVE_CIRCLE_SIZE - leave_glyph_size) / 2.0,
            circle_y + (LEAVE_CIRCLE_SIZE - leave_glyph_size) / 2.0,
            leave_glyph_size,
            &srgb(LEAVE_RED, 1.0),
            2.8,
        );
    }

    /// Minimal glyph (canvas.html §3's "MINIMAL MODE -- under notch
    /// pressure"): bare mic stroke glyph; a green live dot bottom-right
    /// only when actually in a meeting. Not-in-meeting is this same
    /// rendering, dimmer and dotless (judgment call, see `paint` doc).
    unsafe fn paint_minimal(rect: NSRect, state: MenubarPillState) {
        let height = rect.size.height;
        let width = rect.size.width;

        // NOT in a meeting: show the Petal brand mark, not a mic. A dim mic
        // glyph while idle read as "why is Petal using my microphone?" — the
        // menubar should be branded when nothing is live (menubar-icon fix
        // 2026-07-05). The 15pt filled mark stays distinct from the larger
        // 16pt live mic while retaining enough room for its counters.
        if !state.in_meeting {
            let glyph_size = MINIMAL_IDLE_GLYPH_SIZE;
            let gx = (width - glyph_size) / 2.0;
            let gy = (height - glyph_size) / 2.0;
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.85);
            draw_petal_glyph(gx, gy, glyph_size, &color);
            return;
        }

        // In a meeting, minimal (squeezed): bare mic stroke glyph + green live
        // dot. Here the mic is meaningful — it reflects real mic-mute state.
        let glyph_size = MINIMAL_MIC_SIZE;
        let gx = (width - glyph_size) / 2.0;
        let gy = (height - glyph_size) / 2.0;
        let glyph_color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.85);
        draw_mic_glyph(gx, gy, glyph_size, &glyph_color, 2.3, state.mic_muted);

        // Small live-green dot, bottom-right of the glyph (comp: right:-1
        // bottom:1 of the 22pt hit box).
        srgb(LIVE_GREEN, 1.0).set();
        NSBezierPath::bezierPathWithOvalInRect(NSRect {
            origin: NSPoint {
                x: gx + glyph_size - 4.5,
                y: gy - 1.0,
            },
            size: NSSize {
                width: 6.0,
                height: 6.0,
            },
        })
        .fill();
    }

    fn srgb((r, g, b): (f64, f64, f64), alpha: f64) -> Retained<NSColor> {
        NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, alpha)
    }

    /// Round-capped stroke setup shared by all glyph paths.
    fn prep_stroke(path: &NSBezierPath, line_width: f64) {
        path.setLineWidth(line_width);
        path.setLineCapStyle(objc2_app_kit::NSLineCapStyle::Round);
        path.setLineJoinStyle(objc2_app_kit::NSLineJoinStyle::Round);
    }

    /// The approved Petal mark — the fill path copied verbatim from
    /// `icons/icon-source.svg` (native box ~936x962, M/C commands only,
    /// fill-rule even-odd so the petal counters show through). Filled into a
    /// `size`-pt box at (`origin_x`,`origin_y`) (bottom-left, y-up), preserving
    /// aspect and centered horizontally. This is the menubar glyph when NOT in
    /// a meeting (branding, not a mic — an idle mic glyph misread as "Petal is
    /// using my mic"; menubar-icon fix 2026-07-05). Keep in sync with
    /// icon-source.svg if the logo path ever changes.
    unsafe fn draw_petal_glyph(origin_x: f64, origin_y: f64, size: f64, color: &NSColor) {
        const PETAL_PATH: &str = include_str!("menubar_petal_glyph.txt");
        const BOX_W: f64 = 936.0;
        const BOX_H: f64 = 962.0;
        let scale = size / BOX_H;
        let xoff = origin_x + (size - BOX_W * scale) / 2.0;
        let tx = |x: f64| xoff + x * scale;
        let ty = |y: f64| origin_y + (BOX_H - y) * scale; // SVG y-down -> AppKit y-up

        let path = NSBezierPath::bezierPath();
        let toks: Vec<&str> = PETAL_PATH
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|t| !t.is_empty())
            .collect();
        let mut i = 0usize;
        let mut cmd = 'M';
        macro_rules! num {
            () => {{
                let v = if i < toks.len() {
                    toks[i].parse::<f64>().unwrap_or(0.0)
                } else {
                    0.0
                };
                i += 1;
                v
            }};
        }
        while i < toks.len() {
            match toks[i] {
                "M" => {
                    cmd = 'M';
                    i += 1;
                }
                "L" => {
                    cmd = 'L';
                    i += 1;
                }
                "C" => {
                    cmd = 'C';
                    i += 1;
                }
                _ => {}
            }
            match cmd {
                'M' => {
                    let x = num!();
                    let y = num!();
                    path.moveToPoint(NSPoint { x: tx(x), y: ty(y) });
                    cmd = 'L'; // subsequent implicit coords after M are lineto (SVG spec)
                }
                'L' => {
                    let x = num!();
                    let y = num!();
                    path.lineToPoint(NSPoint { x: tx(x), y: ty(y) });
                }
                'C' => {
                    let c1x = num!();
                    let c1y = num!();
                    let c2x = num!();
                    let c2y = num!();
                    let ex = num!();
                    let ey = num!();
                    path.curveToPoint_controlPoint1_controlPoint2(
                        NSPoint {
                            x: tx(ex),
                            y: ty(ey),
                        },
                        NSPoint {
                            x: tx(c1x),
                            y: ty(c1y),
                        },
                        NSPoint {
                            x: tx(c2x),
                            y: ty(c2y),
                        },
                    );
                }
                _ => break,
            }
        }
        path.setWindingRule(objc2_app_kit::NSWindingRule::EvenOdd);
        color.set();
        path.fill();
    }

    /// The comp's mic SVG (`viewBox 0 0 24 24`: capsule rect(9,3,6,11,rx3),
    /// bowl arc `M5 11a7 7 0 0 0 14 0`, stem `M12 18v3`, muted slash
    /// `M3 3l18 18`) scaled into a `size`-pt box at (`origin_x`,`origin_y`)
    /// (bottom-left, y-up). `stroke_svg_units` is the SVG stroke-width
    /// (comp: 2.8 full / 2.3 minimal), scaled with the glyph.
    unsafe fn draw_mic_glyph(
        origin_x: f64,
        origin_y: f64,
        size: f64,
        color: &NSColor,
        stroke_svg_units: f64,
        muted: bool,
    ) {
        let s = size / 24.0;
        let px = |x: f64| origin_x + x * s;
        let py = |y: f64| origin_y + (24.0 - y) * s; // SVG y-down -> AppKit y-up
        let lw = stroke_svg_units * s;
        color.set();

        // Capsule body.
        let body = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
            NSRect {
                origin: NSPoint {
                    x: px(9.0),
                    y: py(14.0),
                },
                size: NSSize {
                    width: 6.0 * s,
                    height: 11.0 * s,
                },
            },
            3.0 * s,
            3.0 * s,
        );
        prep_stroke(&body, lw);
        body.stroke();

        // Bowl: lower semicircle, center (12,11) r7 (y-up: 180deg -> 360deg
        // counterclockwise passes through the bottom).
        let bowl = NSBezierPath::bezierPath();
        bowl.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
            NSPoint {
                x: px(12.0),
                y: py(11.0),
            },
            7.0 * s,
            180.0,
            360.0,
            false,
        );
        prep_stroke(&bowl, lw);
        bowl.stroke();

        // Stem.
        let stem = NSBezierPath::bezierPath();
        stem.moveToPoint(NSPoint {
            x: px(12.0),
            y: py(18.0),
        });
        stem.lineToPoint(NSPoint {
            x: px(12.0),
            y: py(21.0),
        });
        prep_stroke(&stem, lw);
        stem.stroke();

        if muted {
            let slash = NSBezierPath::bezierPath();
            slash.moveToPoint(NSPoint {
                x: px(3.0),
                y: py(3.0),
            });
            slash.lineToPoint(NSPoint {
                x: px(21.0),
                y: py(21.0),
            });
            prep_stroke(&slash, lw);
            slash.stroke();
        }
    }

    /// The comp's people icon (`viewBox 0 0 24 24`: head circle(9,8,r3.5),
    /// front shoulders `M3 20a6 6 0 0 1 12 0`, second head
    /// `M16 5.5a3.5 3.5 0 0 1 0 7`, second shoulder
    /// `M19 20a6 6 0 0 0 -4 -5.6`). Arc centers/angles derived from the SVG
    /// arc commands (endpoint + radius + sweep flag -> circle center).
    unsafe fn draw_people_glyph(
        origin_x: f64,
        origin_y: f64,
        size: f64,
        color: &NSColor,
        stroke_svg_units: f64,
    ) {
        let s = size / 24.0;
        let px = |x: f64| origin_x + x * s;
        let py = |y: f64| origin_y + (24.0 - y) * s;
        let lw = stroke_svg_units * s;
        color.set();

        // Front head.
        let head = NSBezierPath::bezierPathWithOvalInRect(NSRect {
            origin: NSPoint {
                x: px(5.5),
                y: py(11.5),
            },
            size: NSSize {
                width: 7.0 * s,
                height: 7.0 * s,
            },
        });
        prep_stroke(&head, lw);
        head.stroke();

        // Front shoulders: upper semicircle, center (9,20) r6 (y-up: 180deg
        // -> 0deg clockwise passes through the top).
        let shoulders = NSBezierPath::bezierPath();
        shoulders.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
            NSPoint {
                x: px(9.0),
                y: py(20.0),
            },
            6.0 * s,
            180.0,
            0.0,
            true,
        );
        prep_stroke(&shoulders, lw);
        shoulders.stroke();

        // Second head: right-side half-arc, center (16,9) r3.5 (y-up: 90deg
        // -> -90deg clockwise passes through the right).
        let head2 = NSBezierPath::bezierPath();
        head2.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
            NSPoint {
                x: px(16.0),
                y: py(9.0),
            },
            3.5 * s,
            90.0,
            -90.0,
            true,
        );
        prep_stroke(&head2, lw);
        head2.stroke();

        // Second shoulder: from (19,20) to (15,14.4), r6, sweep 0 -> center
        // (13.0, 20.06) in SVG coords, counterclockwise ~0.6deg -> ~70.5deg
        // in y-up.
        let shoulder2 = NSBezierPath::bezierPath();
        shoulder2.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
            NSPoint {
                x: px(13.0),
                y: py(20.06),
            },
            6.0 * s,
            0.6,
            70.5,
            false,
        );
        prep_stroke(&shoulder2, lw);
        shoulder2.stroke();
    }

    /// The comp's leave/exit SVG (`viewBox 0 0 24 24`: door frame
    /// `M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4`, arrow
    /// `M16 17l5-5-5-5` + `M21 12H9`).
    unsafe fn draw_leave_glyph(
        origin_x: f64,
        origin_y: f64,
        size: f64,
        color: &NSColor,
        stroke_svg_units: f64,
    ) {
        let s = size / 24.0;
        let px = |x: f64| origin_x + x * s;
        let py = |y: f64| origin_y + (24.0 - y) * s;
        let lw = stroke_svg_units * s;
        color.set();

        // Door frame with 2-unit rounded corners (arc centers (5,19) and
        // (5,5) in SVG coords).
        let door = NSBezierPath::bezierPath();
        door.moveToPoint(NSPoint {
            x: px(9.0),
            y: py(21.0),
        });
        door.lineToPoint(NSPoint {
            x: px(5.0),
            y: py(21.0),
        });
        door.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
            NSPoint {
                x: px(5.0),
                y: py(19.0),
            },
            2.0 * s,
            270.0,
            180.0,
            true,
        );
        door.lineToPoint(NSPoint {
            x: px(3.0),
            y: py(5.0),
        });
        door.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
            NSPoint {
                x: px(5.0),
                y: py(5.0),
            },
            2.0 * s,
            180.0,
            90.0,
            true,
        );
        door.lineToPoint(NSPoint {
            x: px(9.0),
            y: py(3.0),
        });
        prep_stroke(&door, lw);
        door.stroke();

        // Arrow: chevron + shaft.
        let arrow = NSBezierPath::bezierPath();
        arrow.moveToPoint(NSPoint {
            x: px(16.0),
            y: py(17.0),
        });
        arrow.lineToPoint(NSPoint {
            x: px(21.0),
            y: py(12.0),
        });
        arrow.lineToPoint(NSPoint {
            x: px(16.0),
            y: py(7.0),
        });
        arrow.moveToPoint(NSPoint {
            x: px(21.0),
            y: py(12.0),
        });
        arrow.lineToPoint(NSPoint {
            x: px(9.0),
            y: py(12.0),
        });
        prep_stroke(&arrow, lw);
        arrow.stroke();
    }

    // ---------------------------------------------------------------------
    // Popover
    // ---------------------------------------------------------------------

    /// The last time the popover was hidden because it resigned key window
    /// (i.e. a click-away dismiss). Needed to disambiguate "clicked the pill
    /// body to CLOSE the popover": on that click AppKit may deliver the
    /// resign-key (hiding the popover) BEFORE our button action runs, so
    /// without this guard the action would see a hidden popover and
    /// immediately re-show it -- the pill could then never close its own
    /// popover.
    static LAST_RESIGN_HIDE: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    const RESIGN_REOPEN_GUARD_MS: u128 = 300;

    fn create_popover(app: &tauri::AppHandle) {
        use tauri::{Manager, WebviewUrl};
        use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel};

        tauri_panel! {
            panel!(MenubarPopoverPanel {
                config: {
                    can_become_key_window: true,
                    is_floating_panel: true
                }
            })

            panel_event!(MenubarPopoverPanelEvent {
                window_did_resign_key(notification: &NSNotification) -> ()
            })
        }

        match PanelBuilder::<_, MenubarPopoverPanel>::new(app, MENUBAR_POPOVER_LABEL)
            .url(WebviewUrl::App("menubar-popover.html".into()))
            .title("Petal")
            .position(tauri::Position::Logical(tauri::LogicalPosition {
                x: -10000.0,
                y: -10000.0,
            }))
            .level(PanelLevel::Status)
            .size(tauri::Size::Logical(tauri::LogicalSize {
                width: POPOVER_WIDTH,
                height: 360.0, // provisional; content-fit resize on mount (resize_menubar_popover)
            }))
            .has_shadow(true)
            .transparent(true)
            .no_activate(false)
            .corner_radius(14.0)
            .with_window(|w| w.decorations(false).transparent(true))
            .collection_behavior(
                CollectionBehavior::new()
                    .can_join_all_spaces()
                    .full_screen_auxiliary(),
            )
            .build()
        {
            Ok(panel) => {
                panel.hide();

                // Click-away dismiss (issue #5): the panel CAN become key
                // (config above); when it stops being key -- the user
                // clicked the desktop, another app, or one of our own other
                // windows -- hide it, like a real NSPopover with transient
                // behavior. The delegate callback lands on the main thread
                // (AppKit invariant), so `hide_popover`'s AppKit work is
                // main-thread-safe here. `set_event_handler` retains the
                // handler (checked in tauri_nspanel's panel.rs), so letting
                // our local `Retained` go out of scope is fine.
                let handler = MenubarPopoverPanelEvent::new();
                let app_for_resign = app.clone();
                handler.window_did_resign_key(move |_notification| {
                    *LAST_RESIGN_HIDE.lock_unpoisoned() = Some(std::time::Instant::now());
                    hide_popover(&app_for_resign);
                });
                panel.set_event_handler(Some(handler.as_ref()));

                if let Some(window) = app.get_webview_window(MENUBAR_POPOVER_LABEL) {
                    // Without this the panel composites an opaque black rect
                    // on screen despite `.transparent(true)` -- see
                    // webview_transparency.rs's doc for why.
                    crate::webview_transparency::apply_or_retry(app, &window);
                }
            }
            Err(e) => {
                log::error!("menubar: failed to create popover panel: {e}");
            }
        }
    }

    /// Toggle the popover from a pill-body click: hide it if it's showing,
    /// otherwise show it positioned just under the status item's button (or
    /// under the primary-screen top-left as a degenerate fallback if the
    /// button frame isn't available).
    fn toggle_popover(app: &tauri::AppHandle, button_screen_frame: Option<NSRect>) {
        use tauri::Manager;
        let Some(window) = app.get_webview_window(MENUBAR_POPOVER_LABEL) else {
            return;
        };

        // Already visible -> this click closes it.
        if window.is_visible().unwrap_or(false) {
            hide_popover(app);
            return;
        }

        // Just hidden by a resign-key fired from THIS same click (see
        // LAST_RESIGN_HIDE doc comment) -> treat the click as the dismissal
        // it was, don't immediately re-open.
        let recently_resigned = LAST_RESIGN_HIDE
            .lock_unpoisoned()
            .map(|t| t.elapsed().as_millis() < RESIGN_REOPEN_GUARD_MS)
            .unwrap_or(false);
        if recently_resigned {
            return;
        }

        let (x, y) = if let Some(frame) = button_screen_frame {
            // Center the popover under the button, top edge just below it.
            let cx = frame.origin.x + frame.size.width / 2.0 - POPOVER_WIDTH / 2.0;
            let top_y = frame.origin.y - 6.0; // AppKit y grows upward; tauri Logical y is top-down
            (cx, top_y)
        } else {
            (0.0, 0.0)
        };

        if button_screen_frame.is_some() {
            // Convert AppKit bottom-left-origin screen coords to Tauri's
            // top-left-origin logical position using the primary monitor's
            // height, mirroring the coordinate-flip `hover_tab.rs` already
            // does implicitly via Tauri's own Position types elsewhere.
            if let Ok(Some(monitor)) = app.primary_monitor() {
                let screen_height = monitor.size().height as f64 / monitor.scale_factor();
                let flipped_y = screen_height - y;
                let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition {
                    x,
                    y: flipped_y,
                }));
            }
        }

        // Re-apply webview transparency on every show (house precedent: the
        // hover pill needed the same re-apply-on-show, commit c095d98).
        crate::webview_transparency::apply_or_retry(app, &window);

        // Show AND make key -- key status is what the click-away dismiss
        // (window_did_resign_key above) keys off.
        use tauri_nspanel::ManagerExt;
        if let Ok(panel) = app.get_webview_panel(MENUBAR_POPOVER_LABEL) {
            panel.show_and_make_key();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }

        // Belt-and-suspenders data refresh: nudge the popover page to
        // re-fetch room/presence + re-measure its height. The page also
        // listens for `presence-update`/`room-left` Tauri events (a regular
        // labeled webview receives events fine, unlike compositor CHILD
        // webviews -- CLAUDE.md's eval-vs-events lesson), but that event
        // path hasn't been live-verified for THIS webview yet, so the show
        // path doesn't depend on it.
        let _ = window.eval("window.__petalPopoverShown && window.__petalPopoverShown()");
    }

    pub fn hide_popover(app: &tauri::AppHandle) {
        use tauri::Manager;
        if let Some(window) = app.get_webview_window(MENUBAR_POPOVER_LABEL) {
            let _ = window.hide();
        }
    }

    /// Resize the popover to fit its measured content height (see the
    /// `resize_menubar_popover` command). Growing extends downward from the
    /// anchored top edge (Tauri positions are top-left-origin), so no
    /// repositioning is needed. Marshalled to the main thread: the popover
    /// is an NSPanel and this is reachable from a Tauri command thread
    /// (CLAUDE.md's AppKit-off-main-thread crash class).
    pub fn resize_popover(app: &tauri::AppHandle, height: f64) {
        let height = height.clamp(POPOVER_MIN_HEIGHT, POPOVER_MAX_HEIGHT);
        let app = app.clone();
        let _ = app.clone().run_on_main_thread(move || {
            use tauri::Manager;
            if let Some(window) = app.get_webview_window(MENUBAR_POPOVER_LABEL) {
                let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
                    width: POPOVER_WIDTH,
                    height,
                }));
            }
        });
    }

    /// Leave the currently-joined room from the pill's leave circle --
    /// the SAME real teardown path the popover's Leave button uses
    /// (`leave_room_command` -> `session::leave_room`): stops shares,
    /// unpublishes audio, closes the room, emits `room-left`, and resets
    /// this pill via `session.rs`'s own `update_meeting_state` call.
    fn leave_current_room(app: &tauri::AppHandle) {
        log::info!("menubar: leave circle clicked -- leaving current room");
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            use tauri::Manager;
            let Some(state) = app.try_state::<crate::session::SessionState>() else {
                log::warn!("menubar: SessionState not available -- cannot leave room");
                return;
            };
            crate::session::leave_room(&app, &state).await;
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn full_width_granted_stays_full() {
            assert!(!effective_mode(
                Some(PILL_WIDTH_FULL),
                Some(PILL_HEIGHT),
                PILL_WIDTH_FULL
            ));
        }

        #[test]
        fn squeezed_width_falls_back_to_minimal() {
            assert!(effective_mode(
                Some(PILL_WIDTH_FULL - 10.0),
                Some(PILL_HEIGHT),
                PILL_WIDTH_FULL
            ));
        }

        #[test]
        fn missing_window_is_conservative_and_picks_minimal() {
            assert!(effective_mode(None, None, PILL_WIDTH_FULL));
        }

        #[test]
        fn exact_width_match_stays_full() {
            assert!(!effective_mode(
                Some(PILL_WIDTH_MINIMAL),
                Some(PILL_HEIGHT),
                PILL_WIDTH_MINIMAL
            ));
        }

        /// Real-world finding (see `effective_mode` doc comment): on a menu
        /// bar with zero free room, AppKit grants the button a degenerate
        /// ZERO-HEIGHT window rather than a proportionally shrunk width --
        /// observed directly on a real, fully-packed menu bar during
        /// development (every menu-extra slot taken). Width alone would miss
        /// this (a stale/cached width can still equal the requested width),
        /// so height must be checked too.
        #[test]
        fn degenerate_zero_height_is_treated_as_squeezed_even_with_full_width() {
            assert!(effective_mode(
                Some(PILL_WIDTH_FULL),
                Some(0.0),
                PILL_WIDTH_FULL
            ));
        }

        // ------------------------------------------------------------------
        // Click-zone hit-testing (canvas.html §3's three zones -- issue #4)
        // ------------------------------------------------------------------

        #[test]
        fn full_pill_layout_preserves_spacing_and_bounds() {
            assert_eq!(PILL_HEIGHT, 22.0);
            assert_eq!(PILL_WIDTH_FULL, 85.0);
            assert_eq!(FULL_MIC_X, 9.0);
            assert_eq!(FULL_MIC_SIZE, 14.0);
            assert_eq!(FULL_MIC_TO_PEOPLE_GAP, 5.0);
            assert_eq!(FULL_PEOPLE_X, 28.0);
            assert_eq!(FULL_PEOPLE_SIZE, 13.0);
            assert_eq!(FULL_PEOPLE_TO_COUNT_GAP, 3.0);
            assert_eq!(FULL_COUNT_X, 44.0);
            assert_eq!(FULL_COUNT_FONT_SIZE, 11.0);
            assert_eq!(FULL_COUNT_BASELINE_OFFSET, 7.0);
            assert_eq!(LEAVE_CIRCLE_SIZE, 19.0);
            assert_eq!(LEAVE_CIRCLE_MARGIN, 2.0);
            assert_eq!(MINIMAL_IDLE_GLYPH_SIZE, 15.0);
            assert_eq!(MINIMAL_MIC_SIZE, 16.0);
            assert_eq!(MIC_ZONE_MAX_X, 25.5);

            let leave_circle_x = PILL_WIDTH_FULL - LEAVE_CIRCLE_MARGIN - LEAVE_CIRCLE_SIZE;
            assert_eq!(leave_circle_x, 64.0);
            assert_eq!(
                leave_circle_x + LEAVE_CIRCLE_SIZE,
                PILL_WIDTH_FULL - LEAVE_CIRCLE_MARGIN
            );

            let pill_cap_radius = PILL_HEIGHT / 2.0;
            let pill_cap_center_x = PILL_WIDTH_FULL - pill_cap_radius;
            let leave_circle_radius = LEAVE_CIRCLE_SIZE / 2.0;
            let leave_circle_center_x = leave_circle_x + leave_circle_radius;
            let cap_center_offset = (pill_cap_center_x - leave_circle_center_x).abs();
            let rounded_cap_clearance = pill_cap_radius - cap_center_offset - leave_circle_radius;
            assert_eq!(rounded_cap_clearance, 1.0);

            assert!(FULL_COUNT_X < leave_circle_x);
            assert!(LEAVE_CIRCLE_SIZE < PILL_HEIGHT);
        }

        #[test]
        fn full_pill_mic_glyph_zone_toggles_mic() {
            // Dead-center of the mic glyph (derived from the layout constants).
            assert_eq!(
                click_zone(
                    FULL_MIC_X + FULL_MIC_SIZE / 2.0,
                    PILL_WIDTH_FULL,
                    false,
                    true
                ),
                ClickZone::Mic
            );
            // The full visible glyph remains inside the mic zone.
            assert_eq!(
                click_zone(FULL_MIC_X + FULL_MIC_SIZE, PILL_WIDTH_FULL, false, true),
                ClickZone::Mic
            );
            // Zone boundary is inclusive.
            assert_eq!(
                click_zone(MIC_ZONE_MAX_X, PILL_WIDTH_FULL, false, true),
                ClickZone::Mic
            );
        }

        #[test]
        fn full_pill_body_between_mic_and_leave_opens_popover() {
            // People icon / count area -- pill body.
            assert_eq!(
                click_zone(FULL_PEOPLE_X, PILL_WIDTH_FULL, false, true),
                ClickZone::Body
            );
            assert_eq!(
                click_zone(FULL_COUNT_X, PILL_WIDTH_FULL, false, true),
                ClickZone::Body
            );
            assert_eq!(
                click_zone(MIC_ZONE_MAX_X + 0.1, PILL_WIDTH_FULL, false, true),
                ClickZone::Body
            );
        }

        #[test]
        fn full_pill_leave_circle_zone_leaves() {
            // Dead-center of the right-aligned leave circle, derived from the
            // same width, margin, and size constants used by the painter.
            assert_eq!(
                click_zone(
                    PILL_WIDTH_FULL - LEAVE_CIRCLE_MARGIN - LEAVE_CIRCLE_SIZE / 2.0,
                    PILL_WIDTH_FULL,
                    false,
                    true
                ),
                ClickZone::Leave
            );
            // Rightmost edge too.
            assert_eq!(
                click_zone(PILL_WIDTH_FULL, PILL_WIDTH_FULL, false, true),
                ClickZone::Leave
            );
        }

        #[test]
        fn minimal_mode_is_a_single_popover_zone() {
            // Judgment call (see `click_zone` doc): the squeezed minimal glyph
            // is one target -- no invisible mic/leave sub-zones.
            for x in [0.0, PILL_WIDTH_MINIMAL / 2.0, PILL_WIDTH_MINIMAL] {
                assert_eq!(
                    click_zone(x, PILL_WIDTH_MINIMAL, true, true),
                    ClickZone::Body
                );
            }
        }

        #[test]
        fn not_in_meeting_is_a_single_popover_zone() {
            // No meeting -> no mic track to mute, nothing to leave; the
            // whole neutral glyph opens the popover ("Not in a meeting").
            for x in [0.0, PILL_WIDTH_MINIMAL / 2.0, PILL_WIDTH_MINIMAL] {
                assert_eq!(
                    click_zone(x, PILL_WIDTH_MINIMAL, false, false),
                    ClickZone::Body
                );
            }
        }

        /// Regression test for `status_item_clicked`'s hardening (issue
        /// #681): `.expect("must be called on the main thread")` on
        /// `MainThreadMarker::new()` used to panic if the ObjC callback ever
        /// ran off the main thread; it now bails via
        /// `let Some(mtm) = MainThreadMarker::new() else { ...; return; }`,
        /// matching the sibling `init`/`redraw` guards below it in this file.
        ///
        /// `status_item_clicked` itself can't be invoked directly in a unit
        /// test: it's an ObjC selector on a `define_class!`-generated type
        /// whose only constructor, `MenubarTarget::new`, itself requires a
        /// `MainThreadMarker` to call `Self::alloc(mtm)` -- so the object
        /// can only ever be built FROM the main thread in the first place,
        /// which puts exercising the off-main-thread bail path for this
        /// exact function outside what a `#[test]` (always run on a spawned
        /// worker thread, never the process's real main thread) can drive
        /// without a live AppKit main-thread context. What IS directly
        /// testable, and is the load-bearing part of the fix, is that the
        /// same `let Some(mtm) = MainThreadMarker::new() else { return }`
        /// guard now used in `status_item_clicked` takes the bail branch
        /// (never the panic the old `.expect(...)` would have taken) under
        /// the precise condition it exists to handle: not being on the
        /// process main thread. This test body IS that condition.
        #[test]
        fn main_thread_marker_guard_bails_instead_of_panicking_off_the_main_thread() {
            assert!(
                MainThreadMarker::new().is_none(),
                "expected this test thread not to be the process main thread"
            );

            let bailed = match MainThreadMarker::new() {
                Some(_mtm) => false,
                None => true,
            };
            assert!(
                bailed,
                "status_item_clicked's guard must take the bail branch, not proceed, off-main-thread"
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {}
