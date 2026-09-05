//! Tiny app-quit command (issue #20) — backs the Quit Petal item in the
//! `MainMenu.svelte` profile dropdown (`/main` invokes `quit_app`).
//!
//! Deliberately NOT `tauri-plugin-process`: one command, zero new deps (per
//! the issue's own sketch). Best-effort clean teardown first — if this
//! process is currently joined to a room, `session::leave_room` stops every
//! active share, unpublishes audio, and closes the LiveKit room connection
//! (all pre-existing logic, reused not duplicated) — then `app.exit(0)`.

use tauri::Manager;

#[tauri::command]
pub async fn quit_app(app: tauri::AppHandle) {
    crate::shutdown::mark_quitting();
    if let Some(state) = app.try_state::<crate::session::SessionState>() {
        crate::session::leave_room(&app, state.inner()).await;
    }
    // #908: `meeting_left` (emitted by `leave_room` above) and anything else
    // still queued would otherwise be lost -- the worker is a detached task
    // with no flush/drain-on-quit. Bounded so a dead network can't delay
    // quit; returns immediately once the queue is empty on a healthy one.
    // NOTE: this only covers quits that route through THIS command (the
    // MainMenu "Quit Petal" item). Cmd-Q and Dock -> Quit are OS-level
    // quits that never call `quit_app`, so they do NOT flush -- reviewers
    // confirmed this gap; closing it needs `RunEvent::ExitRequested`
    // interception, deliberately NOT attempted here (blocking on async work
    // from the late, synchronous `RunEvent::Exit` teardown callback risks a
    // deadlock). Tracked as follow-up, not claimed as closed by #908.
    crate::analytics::flush(crate::analytics::SHUTDOWN_FLUSH_TIMEOUT).await;
    log::info!("quit: quit_app command -- exiting(0)");
    app.exit(0);
}
