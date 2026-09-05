//! Dev-only receiver surface for telepointers (`telepointer.rs`), per the
//! task brief's "pick the most honest integration point available today"
//! guidance.
//!
//! ## Why this exists / what it honestly is
//!
//! SPEC.md §4.4's real receiver-side compositor (one borderless native
//! `NSWindow` per incoming shared-window track) does **not** exist yet in
//! this codebase (checked: no code anywhere subscribes to a remote video
//! track and renders it as a native window -- `subscriber.rs` only exists as
//! an M0 latency-measurement harness, not a real compositor, and nothing
//! calls it from the app itself). So there is no real "shared window
//! surface" to draw a received telepointer onto today.
//!
//! Rather than fake that surface silently, this module opens a plain,
//! ordinary Tauri window (not a borderless panel -- a real titled window, so
//! it's obviously a dev tool, not mistaken for the real compositor) hosting
//! the `/dev/telepointer` SvelteKit route: a static mock "shared window"
//! rectangle with a known logical size, real `Pointer.svelte`/`NamePill.svelte`
//! components layered on top, driven by REAL `telepointer-update` events
//! from the REAL data-channel round trip (`telepointer.rs`). What's real vs.
//! stand-in here, precisely:
//! - Real: the LiveKit data-channel publish/subscribe, the coordinate math
//!   (cursor -> normalized 0-1 -> pixel position against this window's
//!   actual rendered mock-surface size), the Svelte rendering components,
//!   the idle-fade timeout.
//! - Stand-in: the "mock shared window" rectangle itself -- a static colored
//!   box with a hardcoded logical size, not a real incoming video frame or a
//!   per-window native compositor window.
//!
//! This window's label (`TELEPOINTER_DEV_LABEL`) is exactly what
//! `telepointer.rs`'s receiver loop targets via `emit_to` -- see that
//! module's `start_receiver_for_room` doc comment.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const TELEPOINTER_DEV_LABEL: &str = "dev-telepointer";

/// Open (or focus, if already open) the `/dev/telepointer` dev window.
/// Ordinary command, callable from anywhere in the frontend (there's no
/// dedicated launcher UI for it -- it's a dev tool, reached the same way
/// every other `/dev/*` route is reached today: by navigating the app's own
/// webview there in dev, or, since this one specifically needs its own
/// window to receive `emit_to` events under its own label, via this
/// command).
#[tauri::command]
pub fn open_dev_telepointer_window(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(TELEPOINTER_DEV_LABEL) {
        let _ = w.set_focus();
        return Ok(());
    }

    WebviewWindowBuilder::new(
        &app,
        TELEPOINTER_DEV_LABEL,
        WebviewUrl::App("dev/telepointer.html".into()),
    )
    .title("Petal — Telepointer (dev)")
    .inner_size(720.0, 480.0)
    .build()
    .map_err(|e| e.to_string())?;

    Ok(())
}
