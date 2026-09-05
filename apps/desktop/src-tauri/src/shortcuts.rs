//! Global keyboard shortcut to toggle the last-shared window (SPEC.md §4.2:
//! "Keyboard: a global shortcut to toggle the last-shared window is the
//! power-user feature engineers will love"). macOS-only, matching every
//! other native-window-adjacent module in this crate (`hover_tab`,
//! `session`, `menubar`, `compositor`, `resilience`).
//!
//! ## Mechanism: reuses the real toggle path, does not build a second one
//!
//! This does NOT call `session::start_share`/`stop_share` directly. It goes
//! through `hover_tab::toggle_share_for_window` -- the exact same function
//! `hover_tab::toggle_window_share` (the Tauri command the hover-tab pill's
//! click handler calls) was refactored to expose, specifically so this
//! shortcut and the pill share one toggle implementation: border show/hide
//! bookkeeping, optimistic-then-rollback-on-failure semantics, and
//! `share-error` event emission all stay identical regardless of which
//! input method (mouse click vs. keyboard shortcut) triggered the toggle.
//! Building a second toggle path here would have meant either duplicating
//! that logic or letting the hover-tab pill's own `shared`/`borders`
//! bookkeeping (private to `hover_tab.rs`) drift out of sync with reality.
//!
//! ## Which window is "the last-shared window"
//!
//! `SessionState::last_toggled_window()` (set by `session::start_share`/
//! `stop_share` on every real toggle, keyboard or mouse) is the source of
//! truth -- see that field's doc comment in `session.rs`. Two cases when the
//! shortcut fires:
//! - That window is **currently shared** -> stop it (needs no fresh frame
//!   lookup; `hover_tab::toggle_share_for_window`'s stop path doesn't use
//!   the `frame` argument at all).
//! - That window is **not currently shared** (either never shared, or was
//!   shared then stopped) -> start sharing it again, which needs a *fresh*
//!   `WindowFrame` (the window may have moved/resized since it was last
//!   shared -- `ActiveShare::frame` is deliberately not kept live-updated,
//!   per that field's own doc comment, so the last-known frame could be
//!   stale). `platform::cg::frame_for_window_id` does a
//!   fresh `CGWindowListCopyWindowInfo` lookup by id for this. If the window
//!   has since closed, there's nothing to re-share -- handled as a clean
//!   no-op with a log line, not a crash or a surfaced error (the user
//!   pressing a shortcut for a window they already closed isn't a failure
//!   worth interrupting them over).
//!
//! If NO window has ever been shared this session (`last_toggled_window()`
//! is `None`), the shortcut is a no-op -- there is nothing to toggle yet,
//! which is the correct, honest behavior rather than guessing at a target.

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{Code, ShortcutState};

/// `Cmd+Ctrl+Shift+S` ("S" for Share). Chosen deliberately as a
/// quadruple-key combo (Cmd+Ctrl+Shift, not just Cmd+Shift) specifically to
/// minimize collision risk:
/// - `Cmd+Shift+S` is "Save As" in a huge number of macOS apps (Preview,
///   TextEdit, most Office/Adobe/Electron apps) -- a real, common,
///   already-claimed shortcut this app has no business overriding globally
///   (a GLOBAL shortcut registered by this app would win over the
///   frontmost app's own "Save As" while Petal is running, which would be
///   a genuinely disruptive collision for any engineer with Petal open in
///   the background while working in another app -- exactly the kind of
///   thing a "global" shortcut needs to avoid).
/// - `Cmd+Shift+3/4/5` are macOS system screenshot shortcuts (reserved at
///   the OS level).
/// - Cmd+Ctrl+Shift+<letter> combos are not claimed by any common macOS
///   app or system shortcut checked here (Preview, Finder, Safari, Chrome,
///   VS Code, Terminal, Slack, Zoom's own global mute is Cmd+Shift+A) --
///   this specific combo space is close to unused in practice, which is
///   exactly what a background-registered global shortcut should target.
pub const TOGGLE_LAST_SHARED_WINDOW_SHORTCUT: &str = "cmd+ctrl+shift+KeyS";
/// `Cmd+Ctrl+Shift+P` ("P" for Pill). Mirrors the existing share shortcut's
/// modifier shape so global shortcuts live in one low-collision namespace.
pub const RESTORE_MEETING_PILL_SHORTCUT: &str = "cmd+ctrl+shift+KeyP";
pub const RESTORE_MEETING_PILL_EVENT: &str = "meeting-restore-pill-requested";

/// Registers the global shortcut and its handler. Called once from
/// `lib.rs`'s `setup()`. Returns an error if registration fails (e.g. the
/// exact combo is somehow already claimed at the OS level by another
/// running app) -- logged, not fatal to app startup, since a missing
/// power-user shortcut shouldn't block the whole app from launching.
/// Returns a plain `String` error (not `tauri_plugin_global_shortcut::Error`
/// or `tauri::Error` directly) since this function's own `?`-chain crosses
/// both of those distinct error types (shortcut parsing/registration is the
/// plugin crate's error; `AppHandle::plugin` registration is `tauri`'s own)
/// -- a shared string is the simplest common type for a one-shot,
/// log-and-continue-on-failure init path like this one, proportionate to
/// how small this function is.
pub fn init(app: &AppHandle) -> Result<(), String> {
    let plugin = tauri_plugin_global_shortcut::Builder::new()
        .with_shortcuts([
            TOGGLE_LAST_SHARED_WINDOW_SHORTCUT,
            RESTORE_MEETING_PILL_SHORTCUT,
        ])
        .map_err(|e| {
            format!(
                "failed to parse shortcuts '{TOGGLE_LAST_SHARED_WINDOW_SHORTCUT}'/'{RESTORE_MEETING_PILL_SHORTCUT}': {e}"
            )
        })?
        .with_handler(|app, shortcut, event| {
            // `global_hotkey` fires this handler on BOTH key-down and
            // key-up (see `ShortcutState::Pressed`/`Released`) -- without
            // this filter the toggle would fire twice per physical press
            // (once down, once up), immediately re-toggling back to the
            // original state.
            if event.state() != ShortcutState::Pressed {
                return;
            }
            match shortcut.key {
                Code::KeyS => {
                    log::info!(
                        "shortcuts: global shortcut fired -- toggling last-shared window"
                    );
                    handle_toggle_last_shared_window(app.clone());
                }
                Code::KeyP => {
                    log::info!(
                        "shortcuts: global shortcut fired -- requesting meeting pill restore"
                    );
                    handle_restore_meeting_pill(app);
                }
                other => log::warn!("shortcuts: unknown registered shortcut fired: {other:?}"),
            }
        })
        .build();
    app.plugin(plugin)
        .map_err(|e| format!("failed to register global-shortcut plugin: {e}"))?;
    log::info!(
        "shortcuts: global shortcuts registered ({}, {})",
        TOGGLE_LAST_SHARED_WINDOW_SHORTCUT,
        RESTORE_MEETING_PILL_SHORTCUT
    );
    Ok(())
}

fn handle_restore_meeting_pill(app: &AppHandle) {
    // Global `emit`, not `emit_to("main", ...)`: the meeting route uses
    // plain frontend `listen()`, and this codebase has verified Tauri 2
    // label-targeted emits do not reach those EventTarget::Any listeners.
    if let Err(e) = app.emit(RESTORE_MEETING_PILL_EVENT, ()) {
        log::warn!("shortcuts: failed to emit {RESTORE_MEETING_PILL_EVENT}: {e}");
    }
}

/// Does the actual toggle work described in this module's doc comment.
/// Spawned as its own async task from the (synchronous) shortcut handler
/// callback, since `hover_tab::toggle_share_for_window` is async (it awaits
/// real capture-start/publish or unpublish calls) and the global-shortcut
/// crate's handler callback is not.
fn handle_toggle_last_shared_window(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Some(state) = app.try_state::<crate::session::SessionState>() else {
            log::warn!("shortcuts: SessionState not managed yet, ignoring shortcut");
            return;
        };

        let Some(window_id) = state.last_toggled_window() else {
            log::info!("shortcuts: no window has been shared yet this session, nothing to toggle");
            return;
        };

        // `toggle_share_for_window` decides start-vs-stop from SessionState.
        // This check exists only to decide whether we NEED a fresh frame
        // (starting a share) or not (stopping one never reads `frame` at all,
        // see that function's stop branch).
        let already_shared = state.is_share_active(window_id);

        let frame = if already_shared {
            // Stopping: `toggle_share_for_window`'s stop path never reads
            // `frame`, so any value is safe here -- zeroed rather than a
            // real lookup, since there's nothing meaningful to look up for
            // "the frame of a share we're about to tear down."
            crate::platform::cg::WindowFrame {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }
        } else {
            // Re-sharing: needs a FRESH frame (the window may have moved or
            // resized since it was last shared -- `ActiveShare::frame` is
            // deliberately not kept live-updated, see its doc comment in
            // session.rs). If the window has since closed, there's nothing
            // to re-share -- a clean no-op, not an error.
            // #744: route the fresh single-id read through the registry
            // (frame_fresh == the cheap per-id CG query; identical behavior).
            match match crate::window_registry::global() {
                Some(reg) => reg.frame_fresh(window_id),
                None => crate::platform::cg::frame_for_window_id(window_id),
            } {
                Some(frame) => frame,
                None => {
                    log::info!(
                        "shortcuts: window {window_id} is no longer on screen, nothing to re-share"
                    );
                    return;
                }
            }
        };

        let now_shared =
            crate::hover_tab::toggle_share_for_window(&app, &state, window_id, frame).await;
        log::info!(
            "shortcuts: toggled last-shared window {window_id} via global shortcut (now_shared={now_shared})"
        );
    });
}
