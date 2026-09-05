pub mod appkit;
#[cfg(target_os = "macos")]
pub mod ax;
#[cfg(target_os = "macos")]
pub mod ax_observer;
#[cfg(target_os = "macos")]
pub mod gesture_tap;
#[cfg(target_os = "macos")]
pub mod launch_services;
#[cfg(target_os = "macos")]
pub mod osascript;
#[cfg(target_os = "macos")]
pub mod sls;
pub mod cg;
pub mod mem;
pub mod power;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
use tauri::AppHandle;

/// Marshal a closure onto the AppKit main thread, logging (never panicking) if
/// the dispatch itself fails.
///
/// # Why this exists
/// Every `NSWindow`/`NSView`/`NSPanel`/`CALayer` mutation in this app MUST run
/// on the main thread — building or closing a panel, attaching a display layer,
/// reordering windows, redrawing the menubar pill. Doing any of it from a
/// background thread (an async Tauri command, a spawned RoomEvent handler, a
/// decode task) traps with `EXC_BREAKPOINT` / "Must only be used from the main
/// thread". See CLAUDE.md's "AppKit off the main thread" crash class.
///
/// `AppHandle::run_on_main_thread` is the marshalling primitive; this wrapper
/// standardises the "dispatch, and log with a tag if the hop fails" pattern so
/// call sites don't each re-implement it. `tag` is a `Display` label naming the
/// operation (e.g. `format!("compositor: ensure_window {window_id}")`) so a
/// failed dispatch is attributable.
///
/// Note: sites that need to compute *extra* diagnostic context inside the
/// failure branch, that intentionally fire-and-forget without logging, or that
/// return the `Result` to their caller are left calling `run_on_main_thread`
/// directly — this helper only covers the uniform tag-and-log case.
#[cfg(target_os = "macos")]
pub fn on_main<F>(app: &AppHandle, tag: impl std::fmt::Display, f: F)
where
    F: FnOnce() + Send + 'static,
{
    if let Err(e) = app.run_on_main_thread(f) {
        log::error!("{tag}: run_on_main_thread failed: {e}");
    }
}
