//! Window-stack occlusion diagnostics.
//!
//! Why: the compositor's remote video window rendered BLACK on screen even
//! though its own content was fine (a `screencapture -l<id>` single-window
//! grab showed real video) -- sibling/child windows (the header/telepointer
//! chrome panels) were compositing OVER it on screen, which no single-window
//! capture can reveal. The only way to debug that class of bug is to dump the
//! actual on-screen window STACK: every window, front-to-back, with layer,
//! alpha, and bounds. That's what [`log_window_stack`] does.
//!
//! The raw CoreGraphics reads live in `platform::cg`; this module only folds
//! that shared snapshot down to compositor-relevant log lines.
//!
//! What gets logged (one single greppable `window-stack:`-prefixed line per
//! window, front-to-back = the array order CGWindowList returns):
//! - every on-screen window owned by THIS process, and
//! - every other process's window whose bounds intersect one of our open
//!   compositor windows' frames (i.e. anything that could be occluding a
//!   remote video window).

#![cfg(target_os = "macos")]

use tauri::AppHandle;

/// Dump the on-screen window stack relevant to occlusion debugging. See the
/// module doc comment. `reason` headers the dump so grep can tie a stack to
/// the event that triggered it.
pub fn log_window_stack(app: &AppHandle, reason: &str) {
    let self_pid = std::process::id() as i64;
    let compositor_frames = crate::compositor::open_window_frames(app);

    let Some(entries) = crate::platform::cg::onscreen_windows() else {
        log::info!("window-stack: [{reason}] CGWindowListCopyWindowInfo returned nothing");
        return;
    };

    log::info!(
        "window-stack: ===== [{reason}] {} on-screen window(s) total, front-to-back; ours (pid {self_pid}) + intersectors of {} compositor frame(s) =====",
        entries.len(),
        compositor_frames.len()
    );

    for (z, e) in entries.iter().enumerate() {
        let ours = e.owner_pid == self_pid;
        let intersects = compositor_frames.iter().any(|&(_, fx, fy, fw, fh)| {
            e.x < fx + fw && fx < e.x + e.w && e.y < fy + fh && fy < e.y + e.h
        });
        if !ours && !intersects {
            continue;
        }
        log::info!(
            "window-stack: z={z} id={} owner='{}'{} name='{}' layer={} alpha={:.2} bounds=({:.0},{:.0} {:.0}x{:.0}){}",
            e.number,
            crate::logging::log_safe_quoted(&e.owner_name),
            if ours { " (SELF)" } else { "" },
            crate::logging::log_safe_quoted(&e.name),
            e.layer,
            e.alpha,
            e.x,
            e.y,
            e.w,
            e.h,
            if intersects { " [intersects-compositor]" } else { "" },
        );
    }
}

/// Pure predicate: pick the frontmost "normal" content window number from an
/// already front-to-back-ordered window list, skipping any window owned by
/// `self_pid`.
///
/// Issue #356 mechanism B: without the `owner_pid` check, Petal's own
/// gallery window (the app's sole ordinary window) could be selected as the
/// anchor, and a newly-arrived remote window would be ordered directly
/// below it -- i.e. right on top of it, from the user's perspective the
/// gallery "jumps to the foreground" on every share. Extracted as a pure
/// function (no CoreGraphics calls) so it's unit-testable headlessly; see
/// `frontmost_normal_window_number` for the live-data wrapper.
fn select_frontmost_normal_anchor(
    entries: &[crate::platform::cg::WindowEntry],
    self_pid: i64,
) -> Option<i64> {
    entries.iter().find_map(|e| {
        (e.owner_pid != self_pid
            && e.number > 0
            && e.layer == 0
            && e.alpha > 0.01
            && e.w >= 40.0
            && e.h >= 40.0)
            .then_some(e.number)
    })
}

/// Frontmost normal content window in the global stack. Used by the
/// compositor to place a newly-arrived remote window behind the user's
/// current focus instead of stealing focus. Never selects a window owned by
/// Petal itself (issue #356) -- see [`select_frontmost_normal_anchor`].
pub fn frontmost_normal_window_number() -> Option<i64> {
    let self_pid = std::process::id() as i64;
    // #744: read the shared registry snapshot. `select_frontmost_normal_anchor`
    // (pure, unit-tested) is unchanged and applies window_diag's OWN predicate
    // (alpha > 0.01, not the registry's is_real which uses >= 0.99) over the
    // raw records -- so the anchor choice is byte-identical. Fall back to a
    // direct enumeration before the registry global is set (early boot).
    if let Some(reg) = crate::window_registry::global() {
        let snap = reg.snapshot();
        let entries: Vec<crate::platform::cg::WindowEntry> = snap
            .records_front_to_back()
            .map(|r| crate::platform::cg::WindowEntry {
                number: r.wid as i64,
                owner_pid: r.owner_pid as i64,
                owner_name: String::new(),
                name: String::new(),
                layer: r.layer,
                alpha: r.alpha,
                x: r.rx,
                y: r.ry,
                w: r.rw,
                h: r.rh,
            })
            .collect();
        return select_frontmost_normal_anchor(&entries, self_pid);
    }
    let entries = crate::platform::cg::onscreen_windows()?;
    select_frontmost_normal_anchor(&entries, self_pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::cg::WindowEntry;

    fn entry(number: i64, owner_pid: i64, layer: i64, alpha: f64, w: f64, h: f64) -> WindowEntry {
        WindowEntry {
            number,
            owner_pid,
            owner_name: String::new(),
            name: String::new(),
            layer,
            alpha,
            x: 0.0,
            y: 0.0,
            w,
            h,
        }
    }

    #[test]
    fn skips_self_owned_windows_even_when_frontmost() {
        const SELF_PID: i64 = 4242;
        let entries = vec![
            // Frontmost, but owned by Petal itself (e.g. the gallery) --
            // must never be selected as the anchor.
            entry(1, SELF_PID, 0, 1.0, 400.0, 300.0),
            // Next candidate: a foreign, qualifying normal window.
            entry(2, 999, 0, 1.0, 400.0, 300.0),
        ];
        assert_eq!(
            select_frontmost_normal_anchor(&entries, SELF_PID),
            Some(2)
        );
    }

    #[test]
    fn skips_multiple_self_owned_windows_in_front() {
        const SELF_PID: i64 = 4242;
        let entries = vec![
            entry(1, SELF_PID, 0, 1.0, 400.0, 300.0),
            entry(2, SELF_PID, 0, 1.0, 200.0, 200.0),
            entry(3, 777, 0, 1.0, 500.0, 500.0),
        ];
        assert_eq!(
            select_frontmost_normal_anchor(&entries, SELF_PID),
            Some(3)
        );
    }

    #[test]
    fn falls_through_non_qualifying_foreign_windows() {
        const SELF_PID: i64 = 4242;
        let entries = vec![
            entry(1, SELF_PID, 0, 1.0, 400.0, 300.0),
            // Foreign but too small / wrong layer / transparent -- skipped
            // by the existing non-pid filters, unrelated to issue #356.
            entry(2, 111, 0, 1.0, 10.0, 10.0),
            entry(3, 111, 5, 1.0, 400.0, 300.0),
            entry(4, 111, 0, 0.0, 400.0, 300.0),
            entry(5, 111, 0, 1.0, 400.0, 300.0),
        ];
        assert_eq!(
            select_frontmost_normal_anchor(&entries, SELF_PID),
            Some(5)
        );
    }

    #[test]
    fn no_qualifying_window_returns_none() {
        const SELF_PID: i64 = 4242;
        let entries = vec![entry(1, SELF_PID, 0, 1.0, 400.0, 300.0)];
        assert_eq!(select_frontmost_normal_anchor(&entries, SELF_PID), None);
    }
}

/// On-demand dump from the frontend/devtools:
/// `invoke('log_window_stack_command', { reason: '...' })`.
#[tauri::command]
pub fn log_window_stack_command(app: AppHandle, reason: String) {
    log_window_stack(&app, &reason);
}
