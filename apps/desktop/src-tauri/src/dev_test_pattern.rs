//! Dev-only deterministic test-pattern window (#256).
//!
//! This is not a stand-in for remote video. It opens a borderless Tauri
//! window (no title bar -- #499: the cockpit's calibration-square check
//! samples fixed absolute pixel coordinates assuming the captured window IS
//! exactly the 960x600 canvas, so any native chrome on top throws that off)
//! hosting `/dev/test-pattern`, whose Svelte canvas renderer mirrors the
//! web-harness reference pattern for native/web capture comparisons.

#[cfg(feature = "cockpit-privileged")]
use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};

use tauri::{AppHandle, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};
#[cfg(feature = "cockpit-privileged")]
use tauri::WebviewWindow;

pub const TEST_PATTERN_DEV_LABEL: &str = "dev-test-pattern";
pub const TEST_PATTERN_STATUS_LABEL: &str = "dev-test-pattern-cockpit-status";
pub const TEST_PATTERN_SOURCE_WIDTH: f64 = 960.0;
pub const TEST_PATTERN_SOURCE_HEIGHT: f64 = 600.0;
const STATUS_HEIGHT: i32 = 72;
const STATUS_GAP: i32 = 8;

/// QA-only, privacy-safe proof that the deterministic source renderer mounted
/// and advanced. It deliberately stores no pixels, DOM, titles, identities, or
/// user data: only a cockpit generation and a monotonic synthetic counter.
#[cfg(feature = "cockpit-privileged")]
#[derive(Debug, Default)]
struct TestPatternLivenessState {
    generation: u64,
    armed: bool,
    first_counter: Option<u64>,
    last_counter: Option<u64>,
    advancing_reports: u8,
    report_sequence: u64,
    last_reported_at: Option<Instant>,
}

#[cfg(feature = "cockpit-privileged")]
static TEST_PATTERN_LIVENESS: OnceLock<Mutex<TestPatternLivenessState>> = OnceLock::new();

#[cfg(feature = "cockpit-privileged")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TestPatternLivenessSnapshot {
    pub generation: u64,
    pub advancing_reports: u8,
    pub counter_delta: u64,
    pub report_sequence: u64,
    pub fresh: bool,
}

#[cfg(feature = "cockpit-privileged")]
fn liveness_state() -> &'static Mutex<TestPatternLivenessState> {
    TEST_PATTERN_LIVENESS.get_or_init(|| Mutex::new(TestPatternLivenessState::default()))
}

/// Reset the proof on every cockpit source open. Old renderer heartbeats can
/// never satisfy a later capture attempt.
#[cfg(feature = "cockpit-privileged")]
pub(crate) fn arm_test_pattern_liveness() -> u64 {
    let mut state = liveness_state().lock().unwrap_or_else(|poison| poison.into_inner());
    arm_liveness_state(&mut state)
}

#[cfg(feature = "cockpit-privileged")]
fn arm_liveness_state(state: &mut TestPatternLivenessState) -> u64 {
    state.generation = state.generation.wrapping_add(1).max(1);
    state.armed = true;
    state.first_counter = None;
    state.last_counter = None;
    state.advancing_reports = 0;
    state.last_reported_at = None;
    state.generation
}

#[cfg(feature = "cockpit-privileged")]
fn record_liveness_counter(
    state: &mut TestPatternLivenessState,
    caller_label: &str,
    counter: u64,
) -> Result<(), String> {
    if caller_label != TEST_PATTERN_DEV_LABEL {
        return Err("test-pattern liveness caller rejected".to_string());
    }
    if !state.armed {
        return Err("test-pattern liveness is not armed".to_string());
    }
    if state.last_counter.is_some_and(|last| counter <= last) {
        return Err("test-pattern liveness counter did not advance".to_string());
    }
    state.first_counter.get_or_insert(counter);
    state.last_counter = Some(counter);
    state.advancing_reports = state.advancing_reports.saturating_add(1);
    state.report_sequence = state.report_sequence.wrapping_add(1).max(1);
    state.last_reported_at = Some(Instant::now());
    Ok(())
}

#[cfg(feature = "cockpit-privileged")]
pub(crate) fn test_pattern_liveness_snapshot() -> TestPatternLivenessSnapshot {
    let state = liveness_state().lock().unwrap_or_else(|poison| poison.into_inner());
    let counter_delta = state
        .first_counter
        .zip(state.last_counter)
        .map(|(first, last)| last.saturating_sub(first))
        .unwrap_or_default();
    TestPatternLivenessSnapshot {
        generation: state.generation,
        advancing_reports: state.advancing_reports,
        counter_delta,
        report_sequence: state.report_sequence,
        fresh: state.last_reported_at.is_some_and(|at| at.elapsed().as_secs_f32() < 2.0),
    }
}

/// Accept only the deterministic QA source window and only strictly advancing
/// synthetic counters. This is compiled into the cockpit QA artifact only.
#[cfg(feature = "cockpit-privileged")]
#[tauri::command]
pub fn report_test_pattern_frame(window: WebviewWindow, counter: u64) -> Result<(), String> {
    let mut state = liveness_state().lock().unwrap_or_else(|poison| poison.into_inner());
    record_liveness_counter(&mut state, window.label(), counter)
}

/// A native-authoritative state for the cockpit-only operator prompt. The
/// frontend renders it, but only the cockpit advances this state; ordinary
/// test-pattern callers never receive the overlay (#313).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CockpitTestPatternPhase {
    Prepare { deadline_epoch_ms: u128 },
    Starting,
    CaptureLocked,
    Failed { detail: &'static str },
}

impl CockpitTestPatternPhase {
    fn status_query(self) -> String {
        match self {
            Self::Prepare { deadline_epoch_ms } => {
                format!("cockpitPhase=prepare&deadlineEpochMs={deadline_epoch_ms}")
            }
            Self::Starting => "cockpitPhase=starting".to_string(),
            Self::CaptureLocked => "cockpitPhase=capture-locked".to_string(),
            Self::Failed { detail } => format!("cockpitPhase=failed&detail={detail}"),
        }
    }

    fn status_route(self) -> String {
        format!("dev/test-pattern-status.html?{}", self.status_query())
    }
}

/// Open (or focus, if already open) the ordinary `/dev/test-pattern` dev
/// window. It intentionally has no cockpit operator overlay.
#[tauri::command]
pub fn open_test_pattern_window(app: AppHandle) -> Result<(), String> {
    open_test_pattern_window_with_phase(app)
}

/// Open the test pattern at its exact captured source dimensions, then update
/// a separate operator surface. The operator surface is never the share source:
/// the 960x600 reference pixels remain byte-for-byte in their expected frame.
pub fn open_test_pattern_window_for_cockpit(
    app: AppHandle,
    phase: CockpitTestPatternPhase,
) -> Result<(), String> {
    open_test_pattern_window_with_phase(app.clone())?;
    #[cfg(feature = "cockpit-privileged")]
    arm_test_pattern_liveness();
    set_cockpit_test_pattern_phase(app, phase)
}

/// Change the native-authoritative phase shown to the operator. The browser
/// only derives a remaining number from the native deadline; it never decides
/// when capture starts or locks.
pub fn set_cockpit_test_pattern_phase(
    app: AppHandle,
    phase: CockpitTestPatternPhase,
) -> Result<(), String> {
    let route = phase.status_route();
    // #499 root cause (confirmed live via app.asset_resolver().iter(): the
    // embedded asset bundle has ZERO "test-pattern*" keys in this dev build):
    // this project's tauri.conf.json sets `build.devUrl`, so a debug build
    // does not embed frontendDist assets at all -- WebviewUrl::App resolves
    // against `devUrl` instead (Tauri's own `get_app_url()`, manager/mod.rs),
    // which is exactly why the SIBLING dev/test-pattern.html window (built
    // via plain `WebviewUrl::App`, never an explicit tauri:// URL) has always
    // worked. This function instead hardcoded `tauri://localhost/{route}` --
    // the ONE and ONLY explicit tauri:// URL construction on this whole path
    // -- which always 404s against an empty embedded-asset table, regardless
    // of query strings or fresh rebuilds (both ruled out live before finding
    // this). Fix: mirror Tauri's own dev-vs-prod base URL choice instead of
    // assuming production's tauri:// scheme unconditionally.
    let dev_url = app.config().build.dev_url.clone();
    if let Some(window) = app.get_webview_window(TEST_PATTERN_STATUS_LABEL) {
        window
            .navigate(cockpit_status_url(&route, dev_url.as_ref())?)
            .map_err(|error| error.to_string())?;
        place_cockpit_status(&app, &window);
        return Ok(());
    }
    // Build with the bare path, no query string. `WebviewUrl::App` takes a
    // PathBuf and joins it onto the resolved app URL itself -- a query string
    // baked into that path (as this used to do: `route.into()`, where `route`
    // is "dev/test-pattern-status.html?cockpitPhase=...") gets treated as
    // part of the literal filename/path segment, not stripped as a query.
    // Create with the bare route, then immediately navigate to the real
    // phase-bearing URL via the same (now dev/prod-aware) helper the
    // existing-window branch above uses.
    WebviewWindowBuilder::new(
        &app,
        TEST_PATTERN_STATUS_LABEL,
        WebviewUrl::App("dev/test-pattern-status.html".into()),
    )
    .title("Petal Test Cockpit")
    .inner_size(TEST_PATTERN_SOURCE_WIDTH, 72.0)
    .resizable(false)
    .always_on_top(true)
    // This companion must not steal focus from the captured 960x600 source.
    .focused(false)
    .build()
    .map_err(|error| error.to_string())?;
    if let Some(window) = app.get_webview_window(TEST_PATTERN_STATUS_LABEL) {
        window
            .navigate(cockpit_status_url(&route, dev_url.as_ref())?)
            .map_err(|error| error.to_string())?;
        place_cockpit_status(&app, &window);
    }
    Ok(())
}

/// Build the app-owned status URL without asking WKWebView for its current
/// URL. A freshly-built status window can exist before WebKit has committed
/// its initial navigation, and Wry's macOS getter panics when that URL is nil.
///
/// Mirrors Tauri's own `AppManager::get_app_url()` (manager/mod.rs, private to
/// the tauri crate): dev mode with `build.devUrl` configured resolves
/// `WebviewUrl::App` against that dev server, not the embedded `tauri://`
/// asset table -- so this must do the same or a debug build 404s every time
/// (#499). `dev_url: None` (production, or a dev build with no devUrl
/// configured) keeps the original `tauri://localhost` behavior.
fn cockpit_status_url(route: &str, dev_url: Option<&tauri::Url>) -> Result<tauri::Url, String> {
    let base = match dev_url {
        Some(url) => url.clone(),
        None => tauri::Url::parse("tauri://localhost")
            .map_err(|error| format!("invalid cockpit status base url: {error}"))?,
    };
    base.join(route)
        .map_err(|error| format!("invalid cockpit status route '{route}': {error}"))
}

/// Retire the non-captured operator surface on every terminal cockpit path.
pub fn retire_cockpit_test_pattern_status(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(TEST_PATTERN_STATUS_LABEL) {
        let _ = window.hide();
        let _ = window.close();
    }
}

fn status_position_for_source(
    source: (i32, i32, i32, i32),
    display: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (source_x, source_y, source_w, source_h) = source;
    let (display_x, display_y, display_w, display_h) = display;
    let below = source_y + source_h + STATUS_GAP;
    let above = source_y - STATUS_HEIGHT - STATUS_GAP;
    let y = if below + STATUS_HEIGHT <= display_y + display_h {
        below
    } else if above >= display_y {
        above
    } else {
        // A tiny display cannot fit a 960x600 source plus status. Prefer a
        // visible edge over overlap; the source remains the only shared id.
        display_y.max(source_y + source_h)
    };
    let x = source_x.clamp(display_x, display_x + display_w - source_w);
    (x, y)
}

fn place_cockpit_status(app: &AppHandle, status: &tauri::WebviewWindow) {
    let Some(source) = app.get_webview_window(TEST_PATTERN_DEV_LABEL) else {
        return;
    };
    let (Ok(position), Ok(size)) = (source.outer_position(), source.outer_size()) else {
        return;
    };
    let display = source
        .current_monitor()
        .ok()
        .flatten()
        .map(|monitor| {
            (
                monitor.position().x,
                monitor.position().y,
                monitor.size().width as i32,
                monitor.size().height as i32,
            )
        })
        .unwrap_or((0, 0, i32::MAX / 4, i32::MAX / 4));
    let (x, y) = status_position_for_source(
        (
            position.x,
            position.y,
            size.width as i32,
            size.height as i32,
        ),
        display,
    );
    let _ = status.set_position(PhysicalPosition::new(x, y));
}

fn open_test_pattern_window_with_phase(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(TEST_PATTERN_DEV_LABEL) {
        let _ = w.set_focus();
        return Ok(());
    }

    WebviewWindowBuilder::new(
        &app,
        TEST_PATTERN_DEV_LABEL,
        WebviewUrl::App("dev/test-pattern.html".into()),
    )
    .title("Petal — Test Pattern (dev)")
    .inner_size(TEST_PATTERN_SOURCE_WIDTH, TEST_PATTERN_SOURCE_HEIGHT)
    // Borderless non-resizable windows can be refused key status by AppKit on
    // some macOS builds. Keep the source's capture geometry invariant while
    // allowing normal key-window participation.
    .resizable(true)
    .min_inner_size(TEST_PATTERN_SOURCE_WIDTH, TEST_PATTERN_SOURCE_HEIGHT)
    .max_inner_size(TEST_PATTERN_SOURCE_WIDTH, TEST_PATTERN_SOURCE_HEIGHT)
    // #499: the Test Cockpit's calibration squares are checked at fixed
    // absolute pixel coordinates (e.g. (28,28) for the top-left square) in
    // the captured window screenshot -- an implicit assumption that the
    // window's captured bounds are EXACTLY the 960x600 canvas with nothing
    // else. A standard native title bar adds ~28-56pt on top of that
    // (varies by macOS version/scale), so the calibration square silently
    // renders 28-56px lower than the check expects, and (28,28) instead
    // samples whatever's actually in the title bar area -- a plausible-
    // looking but wrong color, not a color-space bug (confirmed live: with
    // decorations on, the captured artifact showed the title bar's own
    // pixels at (28,28), not the red calibration square). No decorations:
    // the window's full bounds are exactly its content, matching what the
    // check has always assumed.
    .decorations(false)
    // Keep the window on top and focused while it is being shared. macOS
    // SUSPENDS webview rendering for a FULLY-occluded window
    // (NSWindowOcclusionState), which froze the canvas -> ScreenCaptureKit
    // captured a static frame and the receiver saw <1fps (the SHARE-N2W-Q
    // failure). An always-on-top window is never fully covered, so it keeps
    // drawing at full framerate. (#254 fps ceiling, native sender side.)
    .always_on_top(true)
    .focused(true)
    .build()
    .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        cockpit_status_url, status_position_for_source, CockpitTestPatternPhase,
        TEST_PATTERN_SOURCE_HEIGHT, TEST_PATTERN_SOURCE_WIDTH,
    };

    #[cfg(feature = "cockpit-privileged")]
    use super::{
        arm_liveness_state, record_liveness_counter, TestPatternLivenessState, TEST_PATTERN_DEV_LABEL,
    };

    #[cfg(feature = "cockpit-privileged")]
    #[test]
    fn liveness_rejects_wrong_caller_and_resets_each_generation() {
        let mut state = TestPatternLivenessState::default();
        let first_generation = arm_liveness_state(&mut state);
        assert!(record_liveness_counter(&mut state, "main", 1).is_err());
        record_liveness_counter(&mut state, TEST_PATTERN_DEV_LABEL, 4).unwrap();
        record_liveness_counter(&mut state, TEST_PATTERN_DEV_LABEL, 5).unwrap();
        assert_eq!(state.advancing_reports, 2);
        let next_generation = arm_liveness_state(&mut state);
        assert_ne!(first_generation, next_generation);
        assert_eq!(state.advancing_reports, 0);
        assert!(state.last_counter.is_none());
    }

    #[test]
    fn cockpit_prepare_url_carries_only_native_authoritative_state() {
        let route = CockpitTestPatternPhase::Prepare {
            deadline_epoch_ms: 5000,
        }
        .status_route();
        assert!(route.contains("cockpitPhase=prepare"));
        assert!(route.contains("deadlineEpochMs=5000"));
    }

    #[test]
    fn cockpit_capture_locked_url_has_no_countdown() {
        let route = CockpitTestPatternPhase::CaptureLocked.status_route();
        assert!(route.contains("cockpitPhase=capture-locked"));
        assert!(!route.contains("deadlineEpochMs"));
    }

    #[test]
    fn cockpit_status_navigation_url_is_fresh_and_does_not_need_webview_state() {
        // This models PREPARE -> STARTING while the status WKWebView exists but
        // has not committed its first URL. Route construction is independent of
        // WebviewWindow::url(), which would panic in Wry for that nil URL.
        // No devUrl (production, or a dev build with none configured): falls
        // back to the tauri:// asset protocol.
        let starting = cockpit_status_url(&CockpitTestPatternPhase::Starting.status_route(), None)
            .expect("static starting route is a valid Tauri app URL");

        assert_eq!(starting.scheme(), "tauri");
        assert_eq!(starting.host_str(), Some("localhost"));
        assert_eq!(starting.path(), "/dev/test-pattern-status.html");
        assert_eq!(starting.query(), Some("cockpitPhase=starting"));
    }

    #[test]
    fn cockpit_status_navigation_url_resolves_against_dev_url_when_configured() {
        // #499: tauri.conf.json sets build.devUrl, so a debug build does NOT
        // embed frontendDist assets -- WebviewUrl::App (and this function)
        // must resolve against devUrl instead of tauri://, or every status
        // navigation 404s against an empty embedded-asset table regardless of
        // how correctly the route/query string itself is constructed.
        let dev_url = tauri::Url::parse("http://localhost:1420").unwrap();
        let starting = cockpit_status_url(
            &CockpitTestPatternPhase::Starting.status_route(),
            Some(&dev_url),
        )
        .expect("dev-server-relative starting route is a valid url");

        assert_eq!(starting.scheme(), "http");
        assert_eq!(starting.host_str(), Some("localhost"));
        assert_eq!(starting.port(), Some(1420));
        assert_eq!(starting.path(), "/dev/test-pattern-status.html");
        assert_eq!(starting.query(), Some("cockpitPhase=starting"));
    }

    #[test]
    fn source_dimensions_stay_exact_for_capture_comparisons() {
        assert_eq!(TEST_PATTERN_SOURCE_WIDTH, 960.0);
        assert_eq!(TEST_PATTERN_SOURCE_HEIGHT, 600.0);
    }

    #[test]
    fn status_uses_below_or_above_source_without_overlap() {
        assert_eq!(
            status_position_for_source((100, 100, 960, 600), (0, 0, 1440, 1000)),
            (100, 708)
        );
        assert_eq!(
            status_position_for_source((100, 350, 960, 600), (0, 0, 1440, 1000)),
            (100, 270)
        );
    }
}
