//! Shared "make a transparent Tauri/nspanel webview window ACTUALLY
//! transparent on macOS" utility. Moved out of `compositor.rs` (where it was
//! root-caused and battle-tested against the compositor chrome windows) so
//! every overlay webview in the app -- compositor header/pointer chrome,
//! hover-tab share pill, share-border overlays, menubar popover -- uses
//! exactly ONE implementation.
//!
//! Why this exists at all: Tauri's `.transparent(true)` alone reliably left
//! these windows compositing an opaque BLACK rectangle on screen. Three
//! independent opacity layers all have to be defeated (see the doc comments
//! on the functions below): (1) the NSWindow itself not being non-opaque,
//! (2) the WKWebView/view-tree painting an opaque backing, and (3) on macOS
//! 12+, WKWebView's opaque `underPageBackgroundColor` composited UNDER the
//! page, which ignores both CSS transparency and `drawsBackground = NO`.
//!
//! Threading: must be called on the MAIN thread (it sends AppKit messages to
//! the NSWindow/view tree) -- every call site creates its window on the main
//! thread already (or marshals there via `run_on_main_thread`), so calling
//! this immediately after `build()` at those sites is safe.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Force a webview window's NSWindow to be genuinely transparent
/// (`opaque = NO`, `backgroundColor = clearColor`). Tauri's `.transparent(true)`
/// is supposed to do this, but on child/panel chrome windows it did not stick
/// reliably -- the window kept an opaque backing that composited a black
/// rectangle OVER whatever sits beneath it (first seen with the compositor's
/// header/pointer overlays painting black over the remote-window video).
/// Forcing it here makes the transparent regions of the page actually show
/// what's behind the window; the page's own CSS-painted pixels (e.g. the
/// hover pill's dark rounded rect, the share border's colored frame) still
/// render normally on top of that transparent backdrop.
/// Returns `true` iff the recursive view-tree walk actually FOUND (and
/// treated) at least one WKWebView. `false` means the treatment did NOT land
/// on the webview (webview not attached yet, or `ns_window()` failed) and the
/// caller should re-apply later -- see `apply_or_retry` below.
pub(crate) fn force_window_transparent(win: &tauri::WebviewWindow) -> bool {
    use objc2::{class, msg_send, runtime::AnyObject};
    match win.ns_window() {
        Ok(ns) => unsafe {
            let ns = ns as *mut AnyObject;
            let _: () = msg_send![ns, setOpaque: false];
            let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
            let _: () = msg_send![ns, setBackgroundColor: clear];

            // The NSWindow being non-opaque is NOT enough: the WKWebView
            // (content view + its subviews) can still paint an OPAQUE backing,
            // which composites a solid black rectangle over anything beneath it
            // (e.g. the sibling video panel). Walk the view tree from the
            // content view and force every view non-opaque, and set the
            // WKWebView's `drawsBackground = NO` via KVC (WKWebView honors that
            // key even though it's not in the public ObjC header). This is the
            // reliable fix for "transparent Tauri child window still renders
            // black on screen".
            let content_view: *mut AnyObject = msg_send![ns, contentView];
            let found_webview = if !content_view.is_null() {
                make_view_tree_transparent(content_view)
            } else {
                false
            };

            let n: i64 = msg_send![ns, windowNumber];
            note_window_server_id_seen(n);
            let level: i64 = msg_send![ns, level];
            let frame: objc2_foundation::NSRect = msg_send![ns, frame];
            log::info!(
                "webview_transparency: window '{}' CGWindowID={n} level={level} frame=({:.0},{:.0} {:.0}x{:.0}) forced transparent found_webview={found_webview}",
                win.label(),
                frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
            );
            found_webview
        },
        Err(e) => {
            log::warn!(
                "webview_transparency: window '{}' ns_window() failed ({e}); transparency NOT applied found_webview=false",
                win.label()
            );
            false
        }
    }
}

/// Apply `force_window_transparent` now (caller must be on the main thread),
/// and if the WKWebView wasn't found/treated (e.g. the window was just built
/// during `setup()` and the webview hasn't attached yet, or the page hasn't
/// loaded), schedule ONE retry ~500ms later on the main thread. Use this at
/// window-creation call sites; show-path call sites can call
/// `force_window_transparent` directly.
pub(crate) fn apply_or_retry(app: &tauri::AppHandle, win: &tauri::WebviewWindow) {
    if force_window_transparent(win) {
        return;
    }
    let app = app.clone();
    let label = win.label().to_string();
    log::info!("webview_transparency: window '{label}' -- scheduling one retry in 500ms");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let label2 = label.clone();
        let app2 = app.clone();
        // AppKit must only be touched on the main thread (see CLAUDE.md's
        // documented crash class).
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = tauri::Manager::get_webview_window(&app2, &label2) {
                let found = force_window_transparent(&w);
                log::info!(
                    "webview_transparency: retry for window '{label2}' complete found_webview={found}"
                );
            } else {
                log::warn!(
                    "webview_transparency: retry for window '{label2}' skipped -- window no longer exists"
                );
            }
        });
    });
}

/// #878 Phase 3 item 5: `CGWindowID` (`NSWindow.windowNumber`) is
/// server-assigned and monotonically increasing for the life of the window
/// server -- it only regresses across a real window-server restart (see the
/// `SLSRequestNotificationsForWindows`-dead-port evidence this issue is
/// built on). Persisting the highest id seen and comparing it to the first
/// id seen next launch is a cheap, indirect signal that the window server
/// itself restarted between sessions, independent of whether Petal was even
/// running when it happened.
const WINDOW_SERVER_ID_FILE: &str = "window-server-id.json";
/// Write at most this often -- "cheap": once per minute or on join/leave,
/// not once per panel (#878).
const WINDOW_SERVER_ID_PERSIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WindowServerIdFile {
    highest_window_id: i64,
    /// `kern.boottime` (epoch seconds) of the boot the high water mark was
    /// recorded under. A CGWindowID regression across DIFFERENT boots is a
    /// routine reboot (every reboot restarts the window server and resets
    /// the id space) -- only a same-boot regression is the crash-shaped
    /// signal worth a Sentry event (#882 review; without this the
    /// `window-server-restart-detected` stream is one event per user per
    /// reboot, drowning the #878 signal it exists for). `Option` +
    /// `serde(default)` so files written by 0.9.2 (no field) load cleanly.
    #[serde(default)]
    boot_time_epoch: Option<i64>,
}

struct WindowServerIdStore {
    path: PathBuf,
    /// The value loaded from disk at startup -- the PREVIOUS session's high
    /// water mark. Never mutated after load; only compared against.
    previous_highest: i64,
    /// Boot time recorded alongside `previous_highest` (None for a file
    /// from before the field existed, or no prior file).
    previous_boot_time: Option<i64>,
    /// This process's own boot time, read once at load (None if the sysctl
    /// fails -- then no regression is ever reported, fail-quiet).
    current_boot_time: Option<i64>,
    highest_seen_this_session: i64,
    /// Set once the first-panel-of-the-session regression check has run, so
    /// it never re-fires for the second, third, ... panel.
    regression_checked: bool,
    last_persisted_at: Option<Instant>,
}

/// `kern.boottime` as epoch seconds. Stable across a window-server crash
/// (the machine does not reboot -- the session restarts) and different
/// after any reboot, which is exactly the discriminator the regression
/// check needs.
fn current_boot_time_epoch() -> Option<i64> {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = std::mem::size_of::<libc::timeval>();
    let name = c"kern.boottime";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut tv as *mut libc::timeval as *mut std::ffi::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || tv.tv_sec <= 0 {
        return None;
    }
    Some(tv.tv_sec)
}

impl WindowServerIdStore {
    fn load(app_data_dir: &Path) -> Self {
        let path = app_data_dir.join(WINDOW_SERVER_ID_FILE);
        let (previous_highest, previous_boot_time) = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str::<WindowServerIdFile>(&contents)
                .map(|file| (file.highest_window_id, file.boot_time_epoch))
                .unwrap_or_else(|error| {
                    log::warn!(
                        "webview_transparency: could not parse {} ({error}); treating as no prior state",
                        path.display()
                    );
                    (0, None)
                }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (0, None),
            Err(error) => {
                log::warn!(
                    "webview_transparency: could not read {} ({error}); treating as no prior state",
                    path.display()
                );
                (0, None)
            }
        };
        Self {
            path,
            previous_highest,
            previous_boot_time,
            current_boot_time: current_boot_time_epoch(),
            highest_seen_this_session: 0,
            regression_checked: false,
            last_persisted_at: None,
        }
    }

    fn persist(&mut self, now: Instant) {
        if self
            .last_persisted_at
            .is_some_and(|last| now.saturating_duration_since(last) < WINDOW_SERVER_ID_PERSIST_INTERVAL)
        {
            return;
        }
        self.last_persisted_at = Some(now);
        if let Err(error) = persist_window_server_id_to_path(
            &self.path,
            self.highest_seen_this_session,
            self.current_boot_time,
        ) {
            log::warn!("webview_transparency: failed to persist {}: {error}", self.path.display());
        }
    }
}

fn persist_window_server_id_to_path(
    path: &Path,
    highest_window_id: i64,
    boot_time_epoch: Option<i64>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&WindowServerIdFile {
        highest_window_id,
        boot_time_epoch,
    })
    .map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json)
        .map_err(|error| format!("writing {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        format!(
            "renaming {} to {}: {error}",
            temporary.display(),
            path.display()
        )
    })
}

static WINDOW_SERVER_ID_STORE: OnceLock<Mutex<WindowServerIdStore>> = OnceLock::new();

/// Called once from `setup()` with the same `app_data_dir` every other
/// persisted-preference store uses (see `share_priority::initialize`,
/// `debug_settings::initialize`).
pub(crate) fn initialize(app_data_dir: &Path) {
    if WINDOW_SERVER_ID_STORE
        .set(Mutex::new(WindowServerIdStore::load(app_data_dir)))
        .is_err()
    {
        log::debug!("webview_transparency: window-server-id store already initialized");
    }
}

/// Pure decision: does `first_seen_this_session` look like the window
/// server restarted since `previous_highest` was recorded? `previous_highest
/// <= 0` means no prior state (first launch, or a corrupt/missing file) --
/// never a regression. Half the previous value is a deliberately generous
/// margin: ordinary per-session growth never approaches halving, but a
/// genuine restart resets the id space near zero (#878).
fn window_server_id_regressed(previous_highest: i64, first_seen_this_session: i64) -> bool {
    if previous_highest <= 0 || first_seen_this_session <= 0 {
        return false;
    }
    first_seen_this_session < previous_highest / 2
}

/// Pure decision: is a detected CGWindowID regression worth a Sentry event?
/// Only when both sessions ran under the SAME boot (`kern.boottime`
/// unchanged) -- then the window server restarted *without* a reboot, the
/// crash-shaped #878 case. A regression across different boots is a routine
/// reboot; unknown boot lineage (either side `None`: pre-#882 file, first
/// launch, sysctl failure) stays local-log-only rather than guessing (#882
/// review).
fn window_server_restart_reportable(
    previous_boot: Option<i64>,
    current_boot: Option<i64>,
) -> bool {
    matches!((previous_boot, current_boot), (Some(prev), Some(cur)) if prev == cur)
}

/// Call with every panel's `CGWindowID` as it's observed. Checks for a
/// window-server restart only on the FIRST call this session; every call
/// updates the running high water mark and persists it at most once per
/// `WINDOW_SERVER_ID_PERSIST_INTERVAL`.
fn note_window_server_id_seen(window_id: i64) {
    let Some(store) = WINDOW_SERVER_ID_STORE.get() else {
        return;
    };
    let mut store = store.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !store.regression_checked {
        store.regression_checked = true;
        if window_server_id_regressed(store.previous_highest, window_id) {
            if window_server_restart_reportable(store.previous_boot_time, store.current_boot_time)
            {
                log::warn!(
                    "webview_transparency: window server restarted between sessions WITHOUT a \
                     reboot (CGWindowID regressed {} -> {window_id}, kern.boottime unchanged) -- #878",
                    store.previous_highest
                );
                crate::logging::capture_sentry_diagnostic(
                    crate::logging::SentryDiagnosticEvent::WindowServerRestartDetected(
                        crate::logging::WindowServerRestartDetectedDiagnostic {
                            role: crate::logging::DiagnosticRole::Both,
                        },
                    ),
                );
            } else {
                log::info!(
                    "webview_transparency: CGWindowID regressed {} -> {window_id} across a reboot \
                     or unknown boot lineage (prev_boot={:?} cur_boot={:?}) -- routine, not \
                     reported (#882 review)",
                    store.previous_highest,
                    store.previous_boot_time,
                    store.current_boot_time
                );
            }
        }
    }
    if window_id > store.highest_seen_this_session {
        store.highest_seen_this_session = window_id;
    }
    store.persist(Instant::now());
}

/// Recursively set `opaque = NO` on `view` and all descendants, and
/// `drawsBackground = NO` on any WKWebView (via KVC). Best-effort: unknown
/// selectors are simply not sent. Returns `true` iff at least one WKWebView
/// was found (and treated) anywhere in the subtree.
unsafe fn make_view_tree_transparent(view: *mut objc2::runtime::AnyObject) -> bool {
    use objc2::{class, msg_send, runtime::AnyObject};
    if view.is_null() {
        return false;
    }
    // setOpaque: exists on NSView.
    let responds_opaque: bool = msg_send![view, respondsToSelector: objc2::sel!(setOpaque:)];
    if responds_opaque {
        let _: () = msg_send![view, setOpaque: false];
    }
    // WKWebView: drawsBackground via KVC (also NSScrollView responds, harmless).
    let is_webview: bool = msg_send![view, isKindOfClass: class!(WKWebView)];
    let mut found_webview = is_webview;
    if is_webview {
        let key = objc2_foundation::NSString::from_str("drawsBackground");
        let no: *mut AnyObject = msg_send![class!(NSNumber), numberWithBool: false];
        let _: () = msg_send![view, setValue: no, forKey: &*key];

        // THE actual root cause of the on-screen black video (root-caused by
        // reading wry 0.54.1 + objc2-web-kit source): on macOS 12+, WKWebView
        // additionally composites an OPAQUE `underPageBackgroundColor` UNDER
        // the page -- independent of both CSS `background: transparent` and
        // `drawsBackground = NO` (which wry's `transparent(true)` sets on the
        // WKWebViewConfiguration). Once the overlay page renders any content,
        // that under-page background paints solid black over the sibling video
        // panel in the on-screen composite (while `screencapture -l` of the
        // video window alone still looks fine -- which is how this evaded
        // per-window verification). Clear it explicitly. Guarded by
        // respondsToSelector for pre-macOS-12 safety.
        let responds_upbc: bool =
            msg_send![view, respondsToSelector: objc2::sel!(setUnderPageBackgroundColor:)];
        if responds_upbc {
            let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
            let _: () = msg_send![view, setUnderPageBackgroundColor: clear];
        }
    }
    // Recurse into subviews.
    let subviews: *mut AnyObject = msg_send![view, subviews];
    if !subviews.is_null() {
        let count: usize = msg_send![subviews, count];
        for i in 0..count {
            let sub: *mut AnyObject = msg_send![subviews, objectAtIndex: i];
            found_webview |= unsafe { make_view_tree_transparent(sub) };
        }
    }
    found_webview
}

#[cfg(test)]
mod tests {
    use super::*;

    // #878 Phase 3 item 5: window-server-restart detection via CGWindowID
    // regression is a pure function over two integers -- unit-testable
    // without any real NSWindow.
    #[test]
    fn window_server_id_regression_true_when_far_below_half_previous() {
        assert!(window_server_id_regressed(10_000, 3_000));
    }

    #[test]
    fn window_server_id_regression_false_for_ordinary_growth() {
        assert!(!window_server_id_regressed(10_000, 10_500));
    }

    #[test]
    fn window_server_id_regression_false_at_exactly_half() {
        // Half is the boundary itself, not "at or below half" -- exactly
        // half must not trip the generous margin.
        assert!(!window_server_id_regressed(10_000, 5_000));
    }

    #[test]
    fn window_server_id_regression_false_with_no_prior_state() {
        assert!(!window_server_id_regressed(0, 5));
        assert!(!window_server_id_regressed(-1, 5));
    }

    #[test]
    fn window_server_id_regression_false_when_current_is_non_positive() {
        // A malformed/negative windowNumber should never itself be treated
        // as evidence of a restart.
        assert!(!window_server_id_regressed(10_000, 0));
    }

    // #882 review: a regression is Sentry-worthy only under an UNCHANGED
    // boot -- a reboot restarts the window server routinely and must stay
    // local-log-only, or the event stream is one entry per user per reboot.
    #[test]
    fn restart_reportable_only_when_boot_time_is_unchanged() {
        assert!(window_server_restart_reportable(Some(1000), Some(1000)));
        assert!(
            !window_server_restart_reportable(Some(1000), Some(2000)),
            "a different boot time is a reboot, not a window-server crash"
        );
    }

    #[test]
    fn restart_not_reportable_with_unknown_boot_lineage() {
        // Pre-#882 state file (no boot field), first launch, or a failed
        // sysctl: never guess -- skip the event rather than risk reboot
        // noise.
        assert!(!window_server_restart_reportable(None, Some(1000)));
        assert!(!window_server_restart_reportable(Some(1000), None));
        assert!(!window_server_restart_reportable(None, None));
    }

    #[test]
    fn current_boot_time_epoch_reads_a_plausible_value() {
        // Runs on a real Mac in CI-local: kern.boottime must exist and be a
        // positive epoch in the past. Pins that the sysctl plumbing (name,
        // buffer shape) actually works -- the gate-both-directions rule:
        // without this, a typo'd sysctl name would silently disable
        // reporting forever (every check returns None -> never reportable).
        let boot = current_boot_time_epoch().expect("kern.boottime must be readable on macOS");
        assert!(boot > 0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(boot <= now, "boot time must not be in the future");
    }
}
