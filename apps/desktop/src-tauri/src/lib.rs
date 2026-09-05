// Windows and macOS both carry a working (still maturing) media stack: macOS
// uses the modules under `#[cfg(target_os = "macos")]`, Windows uses the
// `windows_*` modules and the real session in session_stub.rs. The non-macOS
// stubs below exist only where a macOS module has no Windows counterpart yet.
#![cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        unused_imports,
        clippy::collapsible_match,
        clippy::enum_variant_names,
        clippy::manual_clamp,
        clippy::too_many_arguments
    )
)]

// Debug/test-only, env-gated end-to-end test driver (see autotest.rs). Compiled
// out of release binaries unless built with the test-only `autotest` or
// `cockpit-privileged` feature (NEVER pass either to a customer distribution).
// No effect unless PETAL_AUTOTEST_* is set.
#[cfg(all(
    target_os = "macos",
    any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
))]
mod autotest;
mod browser_url;
#[cfg(target_os = "macos")]
pub mod capture;
#[cfg(not(target_os = "macos"))]
#[path = "capture_stub.rs"]
pub mod capture;
#[cfg(all(
    target_os = "macos",
    any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
))]
mod test_cockpit_bridge;
// Read-only receiver for the test-cockpit's `petal.cockpit` data topic
// (#254) -- logs/journals web-peer self-reports, no consumption yet.
#[cfg(target_os = "macos")]
mod cockpit_topic;
#[cfg(target_os = "macos")]
mod compositor;
// Crisp mode (#384 Phase 1 spike): native-to-native still-image path for
// static shared windows. See crisp_still.rs's module doc comment for exact
// scope (sender + wire protocol + invalidation logic implemented and unit
// tested; receiver-side native blit explicitly NOT implemented yet).
#[cfg(target_os = "macos")]
mod crisp_still;
// AI chat (Gemini Live on a shared window) — #656 feature, being built
// spike-first (#654). `protocol.rs` is pure `BidiGenerateContent` JSON
// builders + parser, unit-tested without a socket or key. The pure/serde half
// stays portable (same reasoning as `crisp_still`'s pure half — `pub` so the
// #654 spike probe examples can drive the real protocol builders); the session
// engine and all native surfaces are macOS-gated inside `ai_chat/mod.rs`, so
// Windows (which compiles `session_stub.rs`) never sees them.
pub mod ai_chat;
// Debug-mode user setting (#669): gates the remote-window header's Debug
// button. Cross-platform (unlike ai_chat, which is macOS-only) -- the Debug
// button exists on both the macOS and Windows compositors.
mod debug_settings;
// `petal://join/<room>` invite-link handling (issue #2): pure URL
// parsing (unit-tested) + the navigate-the-main-webview handler wired to
// tauri-plugin-deep-link in run()/setup() below. Not macOS-gated: plain
// string/Tauri-API code, same reasoning as `rooms`.
mod deep_link;
#[cfg(all(debug_assertions, target_os = "macos"))]
mod dev_telepointer;
#[cfg(target_os = "macos")]
mod dev_test_pattern;
// Network/system conditions diagnostics (issue #19 Phase A): the stats
// poller + event journal are portable (LiveKit stats, no platform APIs).
// The macOS-native display-stage feeds (glass-to-glass calibration,
// decoder/render stage sampling) stay macOS-gated inside the module, so on
// Windows those per-track fields are honestly absent (null) rather than
// fabricated.
pub mod diagnostics;
mod draw;
// UserDispatch feedback modal's opt-in, redacted log-attachment command
// (#292). Archive creation is portable, but the command's active-share
// privacy gate still depends on the macOS session state.
#[cfg(target_os = "macos")]
mod feedback;
// In-webview gallery video bridge (issue #26): mints the hidden
// subscribe-only LiveKit token the meeting route's livekit-client second
// participant connects with (remote camera tracks -> gallery tile <video>s).
// `pub` so probe examples can exercise the real token path, same pattern as
// `transport`/`rooms`. Not macOS-gated: pure token/serde code, same
// reasoning as `rooms`/`deep_link`.
pub mod gallery_bridge;
#[cfg(target_os = "macos")]
mod hover_tab;
// Platform-neutral hover-tab core (payload types, geometry math, hit-test
// classification, share-state/color bookkeeping) shared by the macOS hover
// tab (`hover_tab.rs`) and the Windows port. Compiled on every platform; no
// OS calls inside.
mod hover_core;
// Atomic startup gate closing a real TOCTOU race in `tauri-plugin-single-
// instance` 2.4.2's macOS backend (see the module doc comment); Windows's
// backend of that same plugin uses a real atomic `CreateMutexW`, so it isn't
// exposed to this race and needs no equivalent. `cfg`-gated to macOS because
// the module's mechanism (`flock`, `std::os::unix::net::UnixStream`) is
// genuinely Unix-only, not merely "unneeded elsewhere" -- an earlier,
// ungated version of this line compiled on macOS/Linux but failed outright
// on Windows (`std::os::unix` doesn't exist there).
#[cfg(target_os = "macos")]
mod instance_lock;
#[cfg(target_os = "macos")]
mod latency_probe;
// Top-center "<Name> is sharing a window" notice pill (#679).
#[cfg(target_os = "macos")]
mod share_notice;
// Sharer-side remote-control consent prompt (ask policy), same panel recipe.
#[cfg(target_os = "macos")]
mod control_consent;
#[cfg(target_os = "windows")]
#[path = "control_consent_windows.rs"]
mod control_consent;
// File-based logging sink -- see its own module doc comment for the full
// "why" (GUI-launched apps have no reachable stdout/stderr). `pub` so
// `main.rs` isn't the only caller if a future example/binary ever wants the
// same sink; today only `run()` below calls `logging::init()`.
mod analytics;
pub mod logging;
mod main_window;
mod meeting_core;
mod menubar;
mod network_cockpit;
// `pub` (not just crate-private) so `examples/compositor_probe.rs` -- a
// separate crate-root binary linking against `desktop_lib`, same as
// `capture`/`transport`/`window_source` already are for their own probes --
// can drive the real zero-copy display path directly for standalone
// verification (see that example's own doc comment for why a standalone
// harness is used instead of the full Tauri app for this).
#[cfg(target_os = "macos")]
pub mod native_display;
// Real macOS permission checks + requests (SPEC.md §4.1). Not macOS-gated at
// the module level: it exposes non-macOS stubs internally so the Tauri
// command surface stays uniform (see the module's own doc comment) -- only
// the real FFI inside is `#[cfg(target_os = "macos")]`.
mod permissions;
// Cross-peer pipeline stage snapshots for the Network Cockpit -- pure
// livekit data-channel messaging (see pipeline_stats.rs); ungated with the
// diagnostics port so the Windows stats poller can publish its stage views.
mod pipeline_stats;
// Shared camera session orchestration + all camera Tauri commands:
// cfg-free, one copy for both platforms.
mod camera_session;
mod room_generation;
// Windows native→webview self-view feed: the module is
// Windows-only; `camera_session` calls it under `#[cfg(target_os = "windows")]`.
#[cfg(target_os = "windows")]
mod camera_self_view;
mod platform;
mod presence;
mod region_window;
#[cfg(target_os = "macos")]
mod window_fixtures;
mod window_registry;
// Wall-clock helpers (now_ms/now_us), consolidated (#143).
mod time_util;
mod updater;
// Tiny quit command (issue #20) -- best-effort leave_room + app.exit(0).
mod quit;
mod remote_clipboard;
mod remote_control;
mod remote_control_core;
#[cfg(target_os = "windows")]
mod windows_remote_control;
#[cfg(target_os = "windows")]
mod windows_share_overlay;

mod resilience_event;

#[cfg(target_os = "macos")]
mod resilience;
mod shutdown;
pub mod video_color;
// `rooms` (local room-metadata persistence, SPEC.md §4.6) is NOT macOS-only:
// it's plain std/serde file I/O with no ScreenCaptureKit/LiveKit/AppKit
// surface, so `list_rooms`/`create_room` work identically on every platform
// this app could someday target. Room membership is also portable through
// `meeting_core`; platform session modules attach their own media services
// after the shared LiveKit connect. `pub` (not just crate-private) so example harnesses under
// `examples/` -- which are separate crate-root binaries linking against
// `desktop_lib`, same as `window_source`/`transport` already are -- can call
// it directly for verification, same pattern this crate already uses.
pub mod rooms;
#[cfg(target_os = "macos")]
mod session;
#[cfg(not(target_os = "macos"))]
#[path = "session_stub.rs"]
mod session;
mod share_border;
mod share_overlay;
mod share_priority;
mod share_target;
// Poison-tolerant lock helpers (#143): `.lock_unpoisoned()` etc.
mod sync_ext;
// Global keyboard shortcut (SPEC.md §4.2) -- macOS-only, same as every other
// module that reaches into `session`/`hover_tab`'s real toggle path.
#[cfg(target_os = "macos")]
mod shortcuts;
// Telepointer (name-tagged remote cursor, SPEC §4.5, P0). Cross-platform:
// the wire types, normalization, and publisher are shared; the macOS and
// Windows sender loops + receiver overlays are cfg'd inside.
mod telepointer;
// Test Cockpit privileged-command scaffolding (#253 Phase -1). Compiled
// ONLY when built with `--features cockpit-privileged` -- a standard
// customer-distribution build has zero compiled code path to any
// privileged test-cockpit capability. See test_cockpit/mod.rs.
#[cfg(feature = "cockpit-privileged")]
mod test_cockpit;
pub mod transport;
// `pub` only so `examples/startup_layer_probe` can drive the real startup
// demand decision against a live SFU (#299); nothing outside the crate
// consumes it in the shipped app.
#[cfg(target_os = "macos")]
pub mod viewer_demand;
// On-screen window-stack occlusion diagnostics (see its module doc comment)
// -- macOS-only like the compositor it debugs.
// Shared "make a transparent overlay webview ACTUALLY transparent" AppKit
// treatment (moved out of compositor.rs) -- see its module doc comment.
#[cfg(target_os = "windows")]
mod autofill;
#[cfg(target_os = "macos")]
mod webview_transparency;
// Shared WebView2 browser arguments forcing GPU acceleration on Windows (see
// the module doc comment; applied to every Windows webview, including the
// config-created `main` window via tauri.conf.json `additionalBrowserArgs`).
#[cfg(target_os = "windows")]
mod webview2_args;
#[cfg(target_os = "windows")]
mod window_change_watcher;
#[cfg(target_os = "macos")]
mod window_diag;
mod window_picker;
#[cfg(target_os = "macos")]
mod window_resize;
pub mod window_source;
#[cfg(target_os = "windows")]
mod windows_audio_device;
#[cfg(target_os = "windows")]
mod windows_capture_target;
// `pub` (not crate-private) so `examples/windows_share_source_probe.rs` — a
// separate crate-root binary linking against `desktop_lib` — can drive the
// WGC live-capture session directly, same pattern as `window_source`.
#[cfg(target_os = "windows")]
mod windows_compositor;
#[cfg(target_os = "windows")]
pub mod windows_screen_capture;
// Native DWM corner radius for Petal's rectangular windows (see windows_corner.rs).
#[cfg(target_os = "windows")]
mod windows_corner;
#[cfg(target_os = "windows")]
mod windows_hover;

#[cfg(all(debug_assertions, target_os = "macos"))]
use dev_telepointer::open_dev_telepointer_window;
#[cfg(target_os = "macos")]
use dev_test_pattern::open_test_pattern_window;
#[cfg(target_os = "macos")]
use hover_tab::{toggle_window_share, HOVER_TAB_LABEL, HOVER_TAB_WINDOW_TITLE};
use menubar::{get_menubar_state, set_mic_muted, toggle_menubar_mic};
#[cfg(target_os = "macos")]
use menubar::{hide_menubar_popover, resize_menubar_popover};
use network_cockpit::open_network_cockpit_window;
use rooms::{
    create_room, forget_room, list_room_occupancy, list_rooms, rename_room, reset_local_rooms,
};
#[cfg(target_os = "macos")]
use share_border::update_share_border_frame;
use window_source::{ShareableWindow, WindowSourceError};

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BuildInfo {
    version: &'static str,
    commit: &'static str,
    build_date: &'static str,
    is_release_build: bool,
    cockpit_privileged: bool,
    bundle_identifier: String,
}

// Bundle id + Developer-ID team that a build treats as "an official release of
// itself". A fork signs with its own team and bundle id, so both are
// build-time configurable via PETAL_RELEASE_BUNDLE_ID / PETAL_RELEASE_TEAM_ID
// -- hardcoding them made a fork's correctly signed Developer-ID build report
// `is_release_build: false`. Unset falls back to Petal's own values, so the
// official release recipe is unchanged.
#[cfg(any(target_os = "macos", test))]
const RELEASE_SIGNING_BUNDLE_ID: &str = match option_env!("PETAL_RELEASE_BUNDLE_ID") {
    Some(value) => value,
    None => "com.petal.app",
};

#[cfg(any(target_os = "macos", test))]
const RELEASE_SIGNING_TEAM_ID: &str = match option_env!("PETAL_RELEASE_TEAM_ID") {
    Some(value) => value,
    None => "X83RP84J8Z",
};

#[cfg(any(target_os = "macos", test))]
fn release_signing_requirement() -> String {
    format!(
        "identifier \"{RELEASE_SIGNING_BUNDLE_ID}\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = {RELEASE_SIGNING_TEAM_ID}"
    )
}

// Fallback only: used on non-macOS builds (none ship today, but the crate
// compiles cross-platform) and if NSBundle somehow returns no identifier.
// The real, authoritative value comes from `resolve_bundle_identifier()`,
// which reads the actual running app's Info.plist at runtime (#270) --
// this constant must never drift into being treated as the source of
// truth again, since a differently-signed build or fork would silently
// misreport its bundle id if it were.
const PETAL_BUNDLE_IDENTIFIER_FALLBACK: &str = "com.petal.app";

/// Resolve the bundle identifier of the actual running app from its
/// Info.plist via `NSBundle.mainBundle`, rather than trusting a compile-time
/// constant that could drift from what actually got signed/shipped (#270).
/// Cached after first read since it cannot change for the life of the
/// process.
///
/// Safe off the main thread: this is a plain metadata read (no
/// window/panel creation), and Apple documents `NSBundle` instance methods
/// as thread-safe -- distinct from the AppKit window-lifecycle calls that
/// CLAUDE.md's "Crash classes" section requires on the main thread.
#[cfg(target_os = "macos")]
fn resolve_bundle_identifier() -> String {
    use std::sync::OnceLock;

    static BUNDLE_IDENTIFIER: OnceLock<String> = OnceLock::new();
    BUNDLE_IDENTIFIER
        .get_or_init(|| {
            use objc2_foundation::NSBundle;

            NSBundle::mainBundle()
                .bundleIdentifier()
                .map(|s| s.to_string())
                .unwrap_or_else(|| PETAL_BUNDLE_IDENTIFIER_FALLBACK.to_string())
        })
        .clone()
}

#[cfg(not(target_os = "macos"))]
fn resolve_bundle_identifier() -> String {
    PETAL_BUNDLE_IDENTIFIER_FALLBACK.to_string()
}

/// Logs a truthful "this window's frontend actually painted something"
/// marker, called once from `+layout.svelte`'s root `onMount` for every
/// window. Exists because the only prior startup signal
/// (`petal: main window activated (startup...)`, emitted from this file's
/// AppKit setup path) fires on native window activation -- which happens
/// before the SvelteKit SPA hydrates and paints, since the window itself is
/// fully transparent until `.app-shell`'s CSS background renders. An
/// external tool (e.g. a screenshot/automation harness) that treats
/// "activated" as "content is visible" can screenshot a real, on-screen,
/// fully capturable window and see nothing but desktop wallpaper -- not a
/// capture bug, just watching the wrong signal. This gives a real one.
#[tauri::command]
fn frontend_ready(app: tauri::AppHandle, window_label: String) {
    log::info!("petal: frontend first paint -- window='{window_label}'");
    // #636: this is the main window's reveal trigger. It is built invisible so
    // the user never sees the pre-paint state -- an opaque WKWebView underlay
    // with square corners, since the 24px radius is CSS the page has not run
    // yet. Revealing here means the first thing on screen is the real UI.
    if window_label == "main" && reveal_main_window(&app, "frontend-ready") {
        log::info!("petal: main window revealed on first paint (#636)");
    }
}

#[tauri::command]
fn get_build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        commit: env!("PETAL_GIT_COMMIT"),
        build_date: env!("PETAL_BUILD_DATE"),
        is_release_build: current_executable_is_release_build(),
        cockpit_privileged: cfg!(feature = "cockpit-privileged"),
        bundle_identifier: resolve_bundle_identifier(),
    }
}

#[cfg(target_os = "macos")]
fn current_executable_is_release_build() -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|executable| codesign_path_satisfies_release_requirement(&executable))
}

#[cfg(not(target_os = "macos"))]
fn current_executable_is_release_build() -> bool {
    false
}

/// The release marker is an evaluated Apple Code Signing requirement, never a
/// string parsed from `codesign -d` display output. That display can omit the
/// Authority chain and a designated requirement is self-declared by the app;
/// `codesign --verify -R` evaluates the actual certificate chain instead.
#[cfg(target_os = "macos")]
fn codesign_path_satisfies_release_requirement(path: &std::path::Path) -> bool {
    codesign_exit_status_is_release_build(
        std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict", "--verbose=0"])
            .arg(format!("-R={}", release_signing_requirement()))
            .arg(path)
            .status()
            .ok(),
    )
}

#[cfg(target_os = "macos")]
fn codesign_exit_status_is_release_build(status: Option<std::process::ExitStatus>) -> bool {
    status.is_some_and(|status| status.success())
}

#[cfg(target_os = "macos")]
fn log_startup_signing_state() {
    let bundle_identifier = resolve_bundle_identifier();
    log::info!(
        "petal: startup build identity -- version={} commit={} build_date={} bundle_id={}",
        env!("CARGO_PKG_VERSION"),
        env!("PETAL_GIT_COMMIT"),
        env!("PETAL_BUILD_DATE"),
        bundle_identifier
    );

    match std::env::current_exe() {
        Ok(executable) => match codesign_summary_for_path(&executable) {
            Ok(summary) => log::info!(
                "petal: startup signing identity -- executable={} {summary}",
                executable.display()
            ),
            Err(error) => log::warn!(
                "petal: startup signing identity unavailable for {}: {error}",
                executable.display()
            ),
        },
        Err(error) => log::warn!("petal: startup signing identity unavailable: {error}"),
    }

    log::info!(
        "petal: startup TCC reset for new build note -- Screen Recording and Accessibility grants are tied to bundle_id={}; if a newly signed build is denied unexpectedly, reset and regrant TCC for this bundle",
        bundle_identifier
    );
}

#[cfg(target_os = "macos")]
fn codesign_summary_for_path(path: &std::path::Path) -> Result<String, String> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=4"])
        .arg(path)
        .output()
        .map_err(|e| format!("launch codesign: {e}"))?;
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.status.success() {
        return Err(format!(
            "codesign exited with {}: {}",
            output.status,
            text.lines().next().unwrap_or("no output")
        ));
    }

    let summary = text
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("Identifier=")
                || line.starts_with("TeamIdentifier=")
                || line.starts_with("Authority=")
                || line.starts_with("CDHash=")
                || line.starts_with("Signature=")
                || line.starts_with("flags=")
        })
        .map(|line| line.replace(' ', "_"))
        .collect::<Vec<_>>()
        .join(" ");

    Ok(if summary.is_empty() {
        "codesign=no-signing-fields".to_string()
    } else {
        summary
    })
}

#[cfg(test)]
mod build_info_tests {
    #[test]
    fn release_classifier_uses_the_evaluated_developer_id_requirement() {
        let requirement = super::release_signing_requirement();
        assert!(requirement.contains("anchor apple generic"));
        assert!(requirement.contains("1.2.840.113635.100.6.2.6"));
        assert!(requirement.contains("1.2.840.113635.100.6.1.13"));
        // Bundle id and team are build-time configurable so a fork's own
        // Developer-ID build is classified as a release of itself; assert
        // against the resolved values rather than Petal's literals.
        assert!(requirement.contains(&format!(
            "identifier \"{}\"",
            super::RELEASE_SIGNING_BUNDLE_ID
        )));
        assert!(requirement.contains(super::RELEASE_SIGNING_TEAM_ID));
    }

    #[test]
    fn release_signing_identity_defaults_to_petal_when_unconfigured() {
        // The official release recipe must keep working with no extra env.
        if option_env!("PETAL_RELEASE_BUNDLE_ID").is_none() {
            assert_eq!(super::RELEASE_SIGNING_BUNDLE_ID, "com.petal.app");
        }
        if option_env!("PETAL_RELEASE_TEAM_ID").is_none() {
            assert_eq!(super::RELEASE_SIGNING_TEAM_ID, "X83RP84J8Z");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn release_classifier_fails_closed_for_codesign_errors_or_rejections() {
        use std::os::unix::process::ExitStatusExt;

        assert!(super::codesign_exit_status_is_release_build(Some(
            std::process::ExitStatus::from_raw(0)
        )));
        assert!(!super::codesign_exit_status_is_release_build(Some(
            std::process::ExitStatus::from_raw(1)
        )));
        assert!(!super::codesign_exit_status_is_release_build(None));
    }
}

#[tauri::command]
fn restart_app(app: tauri::AppHandle, reason: Option<String>) -> bool {
    log::info!(
        "petal: restart requested from permission flow (reason: {})",
        reason.as_deref().unwrap_or("unspecified")
    );
    app.request_restart();
    true
}

/// Arm the reveal safety net (#636).
///
/// The main window is created `visible: false`, so if the frontend's
/// `frontend_ready` never arrives -- a bundle that fails to load, a JS
/// exception before `onMount`, a missing Tauri bridge -- the app would sit
/// running with NO window and no discoverable way to get one. Show it anyway
/// after a bounded wait: an unstyled window beats an invisible app.
///
/// Called from every platform's `setup()`. `visible: false` is set in
/// `tauri.conf.json` and is platform-agnostic, so a macOS-only safety net
/// would leave Windows with the unrecoverable case.
fn arm_main_window_reveal_fallback(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(MAIN_WINDOW_REVEAL_FALLBACK).await;
        if reveal_main_window(&handle, "startup-fallback") {
            log::warn!(
                "petal: main window revealed by the {}ms fallback -- the frontend never reported first paint; showing an unstyled window beats showing none (#636)",
                MAIN_WINDOW_REVEAL_FALLBACK.as_millis()
            );
        }
    });
}

/// Enumerate currently shareable windows for the window-tab picker
/// (SPEC.md §4.2). The frontend is expected to call this on a debounced
/// timer + on app-focus (simplest-that-works refresh strategy per the spec;
/// can be swapped for a push/event model later without changing this
/// command's shape).
///
/// Returns `Err(WindowSourceError::PermissionDenied(..))` distinctly from
/// other failures so the frontend can route to the Screen Recording
/// permission flow (SPEC.md §4.1) instead of showing a generic error.
#[tauri::command]
async fn list_shareable_windows() -> Result<Vec<ShareableWindow>, WindowSourceError> {
    tokio::task::spawn_blocking(window_source::list_cached)
        .await
        .map_err(|e| WindowSourceError::Other(format!("window enumeration task failed: {e}")))?
}

/// Fast check for whether Petal currently has Screen Recording access,
/// without paying for a full window enumeration. Lets the frontend poll
/// permission state (SPEC.md §4.1's "continuously poll / check on
/// app-focus" recovery flow) cheaply.
#[tauri::command]
fn has_screen_recording_access() -> bool {
    window_source::has_screen_recording_access()
}

/// Capture a cheap, one-shot JPEG thumbnail of a single window by its
/// `CGWindowID`, for the tab strip's periodic preview image. Much lighter
/// weight than the real `SCStream` capture path (SPEC.md §4.1) — this
/// shells out to `screencapture -l<id>` the same way takt's one-shot
/// screenshot picker does; it is not a live stream.
#[tauri::command]
async fn capture_window_thumbnail(window_id: u32, force: Option<bool>) -> Result<String, String> {
    let force = force.unwrap_or(false);
    let bytes = tokio::task::spawn_blocking(move || {
        if force {
            window_source::capture_window_thumbnail_force(window_id)
        } else {
            window_source::capture_window_thumbnail(window_id)
        }
    })
    .await
    .map_err(|e| format!("thumbnail capture task failed: {e}"))??;
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    // Windows WGC thumbnails are PNG-encoded; return a full data URL (the
    // frontend's `loadThumbnail` passes `data:`-prefixed strings through
    // untouched). macOS keeps its historical bare-JPEG-base64 contract.
    #[cfg(target_os = "windows")]
    {
        Ok(format!("data:image/png;base64,{encoded}"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(encoded)
    }
}

/// Create the hover-tab panel up front (hidden), mirroring takt's
/// `overlay::create_capture_tab`: pre-creating avoids first-show latency, and
/// this panel is a long-lived singleton reused for whichever window is
/// hovered (unlike share-border panels, which are created per-share).
#[cfg(target_os = "macos")]
fn create_hover_tab(app_handle: &tauri::AppHandle) {
    use tauri::{Manager, WebviewUrl};
    use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel};

    tauri_panel! {
        panel!(HoverTabPanel {
            config: {
                can_become_key_window: false,
                is_floating_panel: true
            }
        })
    }

    let (hover_tab_width, hover_tab_height) = hover_tab::hover_tab_panel_logical_size();

    match PanelBuilder::<_, HoverTabPanel>::new(app_handle, HOVER_TAB_LABEL)
        .url(WebviewUrl::App("hover-tab.html".into()))
        .title(HOVER_TAB_WINDOW_TITLE)
        .position(tauri::Position::Logical(tauri::LogicalPosition {
            x: -10000.0,
            y: -10000.0,
        }))
        .level(PanelLevel::Normal)
        .size(tauri::Size::Logical(tauri::LogicalSize {
            width: hover_tab_width,
            height: hover_tab_height,
        }))
        .has_shadow(false)
        .transparent(true)
        // NSPanel defaults to hiding when its app deactivates; the hover tab
        // must remain available over the user's active application.
        .hides_on_deactivate(false)
        .no_activate(true)
        .style_mask(tauri_nspanel::StyleMask::empty().nonactivating_panel())
        .corner_radius(0.0)
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
            if let Some(window) = tauri::Manager::get_webview_window(app_handle, HOVER_TAB_LABEL) {
                // Gallery's Invite tooltip is a DOM sibling; the hover tab
                // cannot use that pattern because its webview is clipped to
                // this fixed 40x40 panel. Keep this tooltip native instead.
                // It must be allowed while Petal is inactive because the
                // panel deliberately never activates Petal.
                if let Err(e) =
                    crate::platform::appkit::allow_tooltips_when_application_inactive(&window)
                {
                    log::warn!("hover_tab: failed to enable inactive-app tooltips: {e}");
                }
                // Click-through would defeat the "click to toggle share" button, so,
                // unlike share-border panels, the hover tab does NOT ignore cursor
                // events — it needs to receive the click.
                let _ = window.set_ignore_cursor_events(false);
                // Without this the panel composites an opaque black rect on
                // screen despite `.transparent(true)` -- see
                // webview_transparency.rs's doc for the three opacity layers.
                // `apply_or_retry`, not the bare call: during setup() the WKWebView
                // treatment can fail to land, and the pill show path
                // (hover_tab::position_tab) also re-applies until it has
                // verifiably found a WKWebView.
                webview_transparency::apply_or_retry(app_handle, &window);
            }
        }
        Err(e) => {
            log::error!("Failed to create hover tab panel: {}", e);
        }
    }
}

/// #823: `PETAL_ACCESSORY_UI=1` -- this instance was launched by a test
/// harness and must stay out of the Dock/Cmd-Tab and never steal focus.
/// Read once; the policy is process-wide and set during setup().
pub(crate) fn accessory_ui() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PETAL_ACCESSORY_UI").as_deref() == Ok("1"))
}

fn show_and_activate_main_window_now(app: &tauri::AppHandle, reason: &str) {
    if accessory_ui() {
        // Show without activating: reveal semantics (windows exist on screen
        // for geometry/pixel oracles) are preserved; only focus theft and the
        // Dock presence are gone.
        if let Some(window) = tauri::Manager::get_webview_window(app, "main") {
            let _ = window.show();
            let _ = window.unminimize();
            log::info!(
                "petal: main window shown without activation ({reason}) -- PETAL_ACCESSORY_UI"
            );
        }
        return;
    }
    if let Some(window) = tauri::Manager::get_webview_window(app, "main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        #[cfg(target_os = "macos")]
        {
            if let Err(e) = crate::platform::appkit::activate_window(&window) {
                log::warn!("petal: failed to activate main window ({reason}): {e}");
            } else {
                log::info!("petal: main window activated ({reason})");
            }
        }
        #[cfg(not(target_os = "macos"))]
        log::info!("petal: main window shown/focused ({reason})");
    } else {
        log::warn!("petal: main window unavailable while activating ({reason})");
    }
}

pub(crate) fn show_and_activate_main_window(app: &tauri::AppHandle, reason: &'static str) {
    #[cfg(target_os = "macos")]
    {
        let app_main = app.clone();
        crate::platform::on_main(
            app,
            format!("petal: activate main window ({reason})"),
            move || {
                show_and_activate_main_window_now(&app_main, reason);
            },
        );
    }
    #[cfg(not(target_os = "macos"))]
    show_and_activate_main_window_now(app, reason);
}

/// Has the main window been put on screen yet? (#636)
///
/// The window is built `visible: false` so nothing can see it before the
/// frontend has painted. It is revealed exactly once, by whichever of these
/// happens first:
///
///   * `frontend_ready` for the `main` label -- the real first-paint signal;
///   * [`MAIN_WINDOW_REVEAL_FALLBACK`] elapsing.
///
/// The fallback is not optional. Gating a window's only reveal on a frontend
/// signal means any failure that stops that signal -- a bundle that fails to
/// load, a JS exception before `onMount`, a missing Tauri bridge -- leaves the
/// user with NO window and no way to get one. An unstyled window beats an
/// invisible app.
static MAIN_WINDOW_REVEALED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Has the reveal gate already put the main window on screen once?
///
/// READ ONLY -- never swap it here. Callers use this to tell "hidden by the
/// user mid-session" (reveal already happened; showing is correct) from
/// "#636 cold start" (reveal has not run; showing races first paint).
pub(crate) fn main_window_revealed() -> bool {
    MAIN_WINDOW_REVEALED.load(std::sync::atomic::Ordering::Acquire)
}

/// Long enough for a cold start to prerender and paint, short enough that a
/// broken frontend does not read as a hung launch.
const MAIN_WINDOW_REVEAL_FALLBACK: std::time::Duration = std::time::Duration::from_millis(2500);

/// Reveal the main window once, then re-assert foreground a few beats later.
///
/// Strictly once: a second call is a no-op, NOT a show/activate. That
/// distinction is load-bearing. `frontend_ready` fires on every webview mount,
/// so any mid-session reload of the main webview (a deep link, an updater
/// relaunch of the view, any full navigation) would otherwise re-run
/// show+activate and yank the window in front of whatever the user was doing.
/// Callers that genuinely mean "foreground this now" -- an explicit second
/// launch -- call [`show_and_activate_main_window`] themselves.
///
/// Returns whether THIS call performed the reveal, so the caller can log which
/// trigger won without every later caller claiming it.
fn reveal_main_window(app: &tauri::AppHandle, reason: &'static str) -> bool {
    if MAIN_WINDOW_REVEALED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return false;
    }
    log::info!("petal: revealing main window ({reason})");
    // Re-assert transparency immediately before the window goes on screen, the
    // same way compositor.rs does on its reveal path. The setup()-time call
    // happens while this window is still hidden, and `apply_or_retry` has only
    // one +500ms retry -- if the WKWebView was not attached for either attempt,
    // its opaque `underPageBackgroundColor` underlay survives, and that
    // underlay IS the black box in this bug report.
    #[cfg(target_os = "macos")]
    if let Some(window) = tauri::Manager::get_webview_window(app, "main") {
        webview_transparency::apply_or_retry(app, &window);
    }
    show_and_activate_main_window(app, reason);

    // macOS 14+/26 often leaves the app BEHIND the launching terminal/Finder:
    // it won't foreground an app whose webview window isn't fully on screen
    // yet, and early self-activation trips focus-stealing prevention.
    // Re-assert shortly after the reveal, when macOS honors it. Idempotent and
    // best-effort; skipped under PETAL_AUTOTEST_* so headless/agent runs don't
    // grab focus.
    #[cfg(target_os = "macos")]
    if std::env::var_os("PETAL_AUTOTEST_ROOM").is_none()
        && std::env::var_os("PETAL_AUTOTEST_SOCK").is_none()
        && !accessory_ui()
    {
        let handle_defer = app.clone();
        tauri::async_runtime::spawn(async move {
            for delay_ms in [250u64, 600, 1200] {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                show_and_activate_main_window(&handle_defer, "startup-retry");
            }
        });
    }
    true
}

#[cfg(target_os = "macos")]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // MUST be the very first thing in `run()`, before anything else
    // (including `dotenvy::from_path` below) -- so that even an early-startup
    // failure in this function gets captured to the log file. See
    // `logging.rs`'s module doc comment for the full rationale (this app has
    // no reachable stdout/stderr once launched via `open`/Finder/Dock).
    let log_path = logging::init();
    log::info!(
        "petal: app startup begin (log file: {})",
        log_path.display()
    );
    #[cfg(target_os = "macos")]
    log_startup_signing_state();

    // Built once, up front: needed both for its `identifier` below (the
    // instance-lock gate must be scoped per build identifier -- see
    // `instance_lock.rs`) and later for `.build(context)`. Bound to a
    // variable rather than calling the macro twice, which would embed the
    // frontend asset bundle a second time.
    let context = tauri::generate_context!();

    // MUST run before EVERYTHING else that follows -- before env files, and
    // above all before `tauri::Builder`/the single-instance plugin below:
    // that plugin's macOS backend has a real, unsynchronized TOCTOU race (see
    // `instance_lock.rs`'s module doc comment) that a burst of near-
    // simultaneous launches can lose, each becoming its own independent
    // running instance. User-reported, 2026-08-11: six identical Dock icons,
    // all apparently running. This is a genuinely atomic OS-level gate in
    // front of that racy code, not a replacement for it -- the plugin below
    // is kept for its normal argv-forwarding/window-activation behavior.
    let _instance_lock = match instance_lock::acquire(&instance_lock::lock_path(
        &context.config().identifier,
    )) {
        Ok(instance_lock::Acquire::Acquired(lock)) => Some(lock),
        Ok(instance_lock::Acquire::AlreadyRunning) => {
            log::info!(
                "instance_lock: another Petal instance already holds the startup lock; handing off and exiting"
            );
            instance_lock::notify_running_instance(&context.config().identifier);
            return;
        }
        Err(e) => {
            // Fail OPEN: never block a legitimate solo launch because e.g.
            // the data directory is unwritable. The plugin's own (racy, but
            // usually fine) check remains the sole gate for this run.
            log::warn!(
                "instance_lock: could not acquire the startup lock ({e}); continuing without the extra guard"
            );
            None
        }
    };

    // #902: an in-place update can leave the bundle unregistered -- menu bar
    // fine, NO DOCK ICON, and it never heals on its own. Must stay HERE,
    // before `tauri::Builder`/`NSApplicationMain`: the Dock decides this
    // process's tile at check-in, so repairing from the `setup` hook is too
    // late for the launch already under way. No-op when healthy or unbundled.
    #[cfg(target_os = "macos")]
    platform::launch_services::repair_registration_if_missing();

    // Load local development env files without logging their contents. Process
    // env wins, then apps/desktop/.env, then legacy src-tauri/.env.
    load_env_file(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
    load_env_file(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"));

    #[cfg(feature = "cockpit-privileged")]
    let initial_test_case = test_cockpit::launch_spec_from_args_and_env(std::env::args());

    let builder = tauri::Builder::default()
        // Single-instance guard MUST be registered first (per the plugin's
        // docs): if another Petal is already running, this callback fires in
        // the ORIGINAL instance and the newly-launched one exits. This is the
        // structural fix for the "dev binary vs. stale .app bundle both
        // running com.petal.app" conflict -- a second launch activates the
        // existing window instead of coming up as a rogue duplicate.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if shutdown::request_restart_for_second_launch_if_quitting() {
                log::warn!(
                    "single-instance: second launch arrived while old process is quitting; requesting restart fallback"
                );
                app.request_restart();
                return;
            }
            #[cfg(feature = "cockpit-privileged")]
            if test_cockpit::handle_second_launch(app, argv) {
                return;
            }
            #[cfg(not(feature = "cockpit-privileged"))]
            let _ = argv;
            log::info!(
                "single-instance: a second Petal launch was blocked; activating the existing window"
            );
            // A second launch is an explicit request for the window: reveal it
            // if startup has not yet (it wins over waiting for first paint),
            // and foreground it either way (#636).
            reveal_main_window(app, "single-instance");
            show_and_activate_main_window(app, "single-instance");
        }))
        // Deep-link plugin registered immediately AFTER single-instance, per
        // the official plugin docs' combo guidance (single-instance's
        // `deep-link` feature forwards a second launch's URL-in-argv on
        // Windows/Linux; on macOS the `petal://` open reaches the running
        // instance via Apple Events and fires `on_open_url` directly). The
        // scheme itself is declared in tauri.conf.json
        // (`plugins.deep-link.desktop.schemes: ["petal"]`). NOTE: no runtime
        // `register_all()` call -- it is impossible on macOS (LaunchServices
        // only reads a bundled .app's Info.plist), so dev builds never
        // receive real link clicks; see deep_link.rs's module doc.
        .plugin(tauri_plugin_deep_link::init())
        // Clipboard (invite-link copy, issue #2): backs the frontend's
        // `writeText` from @tauri-apps/plugin-clipboard-manager with a real
        // NSPasteboard write (navigator.clipboard is unreliable in WKWebView).
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        // Silent auto-update (issue #103). The updater checks
        // `plugins.updater.endpoints` on startup from the frontend
        // (`src/lib/updater.ts`); `tauri-plugin-process` provides the relaunch
        // after install. The committed tauri.conf.json ships NO endpoint (OSS
        // builds never phone home) -- only the release overlay
        // tauri.release.conf.json sets it, and `updater.rs` short-circuits
        // to "up to date" with no network when the list is empty.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_nspanel::init())
        // Command index:
        // - app/window shell: restart, main-window route, picker, quit
        // - permissions/window capture: TCC checks, window list, thumbnails
        // - sharing UI: hover tab, share border, share bar
        // - menubar/network/dev routes: menubar pill, cockpit, logs
        // - rooms/session: room CRUD, join/leave/presence, camera, remote gate
        // - media/diagnostics/compositor: audio devices, network stats,
        //   remote native-window controls, remote control, window diagnostics
        .invoke_handler(tauri::generate_handler![
            restart_app,
            list_shareable_windows,
            has_screen_recording_access,
            permissions::check_screen_recording,
            permissions::request_screen_recording,
            permissions::check_microphone,
            permissions::check_camera,
            permissions::check_accessibility,
            permissions::request_microphone,
            permissions::request_camera,
            permissions::request_accessibility,
            permissions::open_privacy_settings,
            capture_window_thumbnail,
            toggle_window_share,
            hover_tab::share_window,
            hover_tab::shared_window_ids,
            hover_tab::hover_tab_page_mounted,
            hover_tab::hover_tab_drag,
            hover_tab::set_hover_tab_menu_open,
            hover_tab::set_hover_tab_tooltip,
            share_priority::get_share_priority,
            share_priority::set_share_priority,
            update_share_border_frame,
            main_window::open_main_route,
            main_window::show_main_window,
            toggle_menubar_mic,
            set_mic_muted,
            get_menubar_state,
            hide_menubar_popover,
            resize_menubar_popover,
            share_notice::share_notice_present,
            share_notice::share_notice_dismiss,
            control_consent::control_consent_present,
            control_consent::control_consent_dismiss,
            open_network_cockpit_window,
            region_window::open_region_window,
            region_window::close_region_window,
            region_window::region_placement_active,
            region_window::region_share_state,
            region_window::sync_region_window_frame,
            region_window::region_view_options_state,
            region_window::set_region_share_priority,
            region_window::set_region_draw_active,
            region_window::region_ai_chat_start,
            region_window::region_ai_chat_stop,
            region_window::toggle_region_share,
            window_picker::open_window_picker_window,
            window_picker::toggle_window_picker_window,
            get_build_info,
            frontend_ready,
            #[cfg(debug_assertions)]
            open_dev_telepointer_window,
            #[cfg(all(
                target_os = "macos",
                any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
            ))]
            autotest::autotest_join_result,
            open_test_pattern_window,
            list_rooms,
            list_room_occupancy,
            create_room,
            rename_room,
            forget_room,
            reset_local_rooms,
            gallery_bridge::gallery_bridge_config,
            quit::quit_app,
            diagnostics::get_network_snapshot,
            diagnostics::get_event_journal,
            diagnostics::set_cockpit_open,
            diagnostics::record_video_stream_state,
            logging::export_logs,
            logging::log_updater_event,
            logging::set_sentry_enabled,
            logging::record_camera_receive_health,
            feedback::prepare_feedback_diagnostics,
            updater::check_compatible_update_available,
            updater::run_launch_update_check,
            updater::download_and_install_compatible_update,
            #[cfg(feature = "cockpit-privileged")]
            test_cockpit::start_test_cockpit,
            #[cfg(feature = "cockpit-privileged")]
            test_cockpit::cockpit_status,
            #[cfg(feature = "cockpit-privileged")]
            test_cockpit::cancel_test_cockpit,
            #[cfg(feature = "cockpit-privileged")]
            test_cockpit::open_test_cockpit_results_folder,
            #[cfg(feature = "cockpit-privileged")]
            test_cockpit::list_test_cockpit_runs,
            #[cfg(feature = "cockpit-privileged")]
            test_cockpit::get_test_cockpit_run,
            #[cfg(feature = "cockpit-privileged")]
            test_cockpit::get_test_cockpit_artifact_data_url,
            #[cfg(feature = "cockpit-privileged")]
            test_cockpit::capture_window_pixels,
            #[cfg(feature = "cockpit-privileged")]
            dev_test_pattern::report_test_pattern_frame,
            #[cfg(target_os = "macos")]
            session::join_room_command,
            #[cfg(target_os = "macos")]
            session::leave_room_command,
            // Shared camera commands (cfg-free; the macOS session used to
            // register these via session::/transport::camera::).
            camera_session::list_camera_devices,
            camera_session::list_camera_modes,
            camera_session::set_camera_device,
            camera_session::set_camera_prefs,
            camera_session::start_camera_publish_command,
            camera_session::stop_camera_publish_command,
            camera_session::camera_publish_state,
            #[cfg(target_os = "macos")]
            session::current_room,
            #[cfg(target_os = "macos")]
            session::room_presence,
            #[cfg(target_os = "macos")]
            session::remote_control_allowed,
            #[cfg(target_os = "macos")]
            session::set_remote_control_allowed,
            session::remote_control_policy,
            session::set_remote_control_policy,
            session::set_share_remote_control_allowed,
            session::share_remote_control_allowed,
            #[cfg(target_os = "macos")]
            session::set_share_resolution,
            #[cfg(target_os = "macos")]
            transport::audio::list_audio_devices,
            #[cfg(target_os = "macos")]
            transport::audio::set_audio_devices,
            #[cfg(target_os = "macos")]
            compositor::compositor_activate_window,
            #[cfg(target_os = "macos")]
            compositor::compositor_raise_window_for_click,
            #[cfg(target_os = "macos")]
            compositor::compositor_raise_participant_windows,
            #[cfg(target_os = "macos")]
            compositor::compositor_list_windows,
            #[cfg(target_os = "macos")]
            compositor::compositor_window_debug_stats,
            #[cfg(target_os = "macos")]
            compositor::compositor_toggle_debug_panel,
            // Debug-mode setting (#669): cross-platform, not macOS-gated --
            // registered again on the Windows invoke_handler list below.
            debug_settings::debug_mode_settings,
            debug_settings::set_debug_mode,
            #[cfg(target_os = "macos")]
            compositor::compositor_set_draw_active,
            #[cfg(target_os = "macos")]
            compositor::compositor_start_drag,
            #[cfg(target_os = "macos")]
            compositor::compositor_begin_resize,
            #[cfg(target_os = "macos")]
            compositor::compositor_resize_window,
            #[cfg(target_os = "macos")]
            compositor::compositor_pop_out,
            #[cfg(target_os = "macos")]
            compositor::compositor_fit_to_source,
            #[cfg(target_os = "macos")]
            compositor::compositor_hide_window,
            #[cfg(target_os = "macos")]
            compositor::compositor_set_ai_chat_overlay_open,
            #[cfg(target_os = "macos")]
            compositor::compositor_ai_chat_overlay_is_open,
            #[cfg(target_os = "macos")]
            draw::draw_send,
            #[cfg(target_os = "macos")]
            share_overlay::share_overlay_set_draw_active,
            #[cfg(target_os = "macos")]
            share_overlay::share_overlay_draw_active,
            // AI chat (#656). macOS-only: the session captures a window and
            // walks its accessibility tree, both of which are macOS concepts.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_settings,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_set_enabled,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_set_api_key,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_is_active,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_start,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_stop,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_ptt_start,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_ptt_end,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_send_text,
            // Remote windows (#657 receiver half): a receiver never hosts, it
            // only asks the owner and reads back what the owner reports.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_request_start,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_request_stop,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_request_send_text,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_request_ptt_start,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_request_ptt_end,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_remote_session,
            // Window control (#658). Dark unless PETAL_AI_CONTROL=1.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_control_approve,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_control_reject,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_control_resume,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::panel::ai_chat_panel_present,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::panel::ai_chat_panel_dismiss,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::commands::ai_chat_control_status,
            #[cfg(target_os = "macos")]
            remote_control::remote_control_send,
            #[cfg(target_os = "macos")]
            remote_control::remote_control_set_active,
            #[cfg(target_os = "macos")]
            remote_control::remote_clipboard_copy,
            #[cfg(target_os = "macos")]
            remote_control::remote_clipboard_paste,
            #[cfg(target_os = "macos")]
            remote_control::remote_control_request_escalation,
            #[cfg(target_os = "macos")]
            remote_control::remote_control_request_timed_out,
            #[cfg(target_os = "macos")]
            remote_control::remote_control_revoke,
            remote_control::remote_control_answer_consent,
            #[cfg(target_os = "macos")]
            window_diag::log_window_stack_command,
            #[cfg(target_os = "macos")]
            window_resize::animate_main_window_resize
        ]);

    #[cfg(target_os = "macos")]
    let builder = builder.manage(session::SessionState::default());
    // #744: the single window-state source of truth, shared by every consumer.
    // Managed for AppHandle-holding consumers AND set as the process global so
    // callback/tight-loop consumers without an AppHandle can read it too.
    let registry = window_registry::WindowRegistry::new();
    window_registry::set_global(registry.clone());
    let builder = builder.manage(registry);
    #[cfg(all(
        target_os = "macos",
        any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
    ))]
    let builder = builder.manage(autotest::AutotestJoinState::default());
    #[cfg(target_os = "macos")]
    log::info!("petal: session state created (SessionState::default() managed)");
    #[cfg(target_os = "macos")]
    let builder = builder.manage(transport::audio::AudioDevicePreferences::default());
    #[cfg(target_os = "macos")]
    log::info!("petal: audio device preferences state created (managed)");
    #[cfg(target_os = "macos")]
    let builder = builder.manage(transport::camera::CameraDevicePreferences::default());
    #[cfg(target_os = "macos")]
    log::info!("petal: camera device preferences state created (managed)");

    // Diagnostics state (issue #19): unconditional (plain std/serde
    // state -- the cockpit commands answer honestly-empty snapshots even
    // before any room is joined; the livekit-backed poller only starts
    // per-room-join on macOS, see session::join_room).
    let builder = builder.manage(diagnostics::DiagnosticsState::default());

    #[cfg(feature = "cockpit-privileged")]
    let builder = builder.manage(test_cockpit::CockpitRuntimeState::default());

    builder
        .setup(move |app| {
            let handle = app.handle().clone();

            // #823: harness-launched instances must not pollute the Dock,
            // appear in Cmd-Tab, or self-activate. Accessory policy delivers
            // all three; the menubar status item is unaffected. Set FIRST,
            // before any window/panel work, so no Regular-policy Dock tile
            // ever flashes into existence.
            if accessory_ui() {
                match crate::platform::appkit::set_accessory_activation_policy() {
                    Ok(()) => log::info!(
                        "petal: PETAL_ACCESSORY_UI=1 -- Accessory activation policy (no Dock tile, no Cmd-Tab, no self-activation)"
                    ),
                    Err(error) => log::warn!(
                        "petal: PETAL_ACCESSORY_UI=1 but the policy could not be set: {error}"
                    ),
                }
            }

            use tauri::Manager;
            let app_data_dir = app.path().app_data_dir().unwrap_or_else(|error| {
                log::warn!(
                    "petal: could not resolve app_data_dir ({error}); persisted preferences will use a temporary fallback this run"
                );
                std::env::temp_dir().join("petal-app-data-fallback")
            });
            share_priority::initialize(app_data_dir.clone());
            // Debug-mode setting (#669): cross-platform, unlike AI chat below
            // -- the Debug button it gates exists on both compositors.
            debug_settings::initialize(&app_data_dir);
            analytics::init(&app_data_dir);
            // AI chat settings (#656): master switch + optional user Gemini
            // key. Loaded here so the very first hover-tab render knows whether
            // the feature is on — it must be entirely invisible when it is not.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::settings::initialize(&app_data_dir);
            // #878 Phase 3 item 5: window-server-restart detection via
            // CGWindowID regression.
            #[cfg(target_os = "macos")]
            webview_transparency::initialize(&app_data_dir);

            create_hover_tab(&handle);
            log::info!("petal: hover-tab panel created");

            share_notice::create_share_notice_panel(&handle);
            log::info!("petal: share-notice panel created");

            control_consent::create_control_consent_panel(&handle);
            log::info!("petal: control-consent panel created");

            #[cfg(any(target_os = "macos", target_os = "windows"))]
            ai_chat::panel::create_ai_chat_panel(&handle);
            #[cfg(target_os = "macos")]
            log::info!("petal: ai-chat panel created");

            // No session/meeting-join concept exists yet in this scaffold, so the
            // hover tracker starts unconditionally on launch. Once a real
            // screenshare-session lifecycle exists, gate this behind "session
            // active" instead of running it for the entire app lifetime.
            hover_tab::start(&handle);
            log::info!("petal: hover-tab tracker loop started");
            // #744: start the window-registry ingest thread (runs ~10Hz while
            // in a room; idle otherwise). Consumers read its snapshot.
            #[cfg(target_os = "macos")]
            if let Some(reg) = tauri::Manager::try_state::<window_registry::WindowRegistry>(&handle)
            {
                window_registry::ingest::start(&handle, reg.inner().clone());
            }
            // #742 rung-A fixture recorder -- inert unless
            // PETAL_RECORD_WINDOW_FIXTURES is set (dev-only characterization).
            #[cfg(target_os = "macos")]
            window_fixtures::start_if_enabled();

            // Share-border z-order/move tracker (issue #23): keeps each
            // border directly above its shared window in the global stack.
            share_border::start_tracker(&handle);
            log::info!("petal: share-border tracker loop started");

            menubar::init(&handle);
            log::info!("petal: menubar pill init complete");

            // Local room metadata must exist before deep-link handling, since
            // the handler persists the real access code before navigation.
            log::info!("petal: rooms persistence loading from {}", app_data_dir.display());
            app.manage(rooms::RoomsState::load(app_data_dir));
            log::info!("petal: rooms persistence loaded, RoomsState managed");

            // petal://join/<room> deep links (issue #2). Two delivery
            // paths, same handler (deep_link.rs):
            // - `get_current()`: the app was LAUNCHED by a link click
            //   (macOS: Apple Event delivered at launch; the plugin buffers
            //   it for exactly this call).
            // - `on_open_url()`: a link was clicked while the app is
            //   already running.
            {
                use tauri_plugin_deep_link::DeepLinkExt;

                match app.deep_link().get_current() {
                    Ok(Some(urls)) => {
                        let urls: Vec<String> =
                            urls.iter().map(|u| u.to_string()).collect();
                        log::info!("petal: launched with deep link(s): {urls:?}");
                        deep_link::handle_deep_link_urls(&handle, urls);
                    }
                    Ok(None) => {}
                    Err(e) => log::warn!("petal: deep_link get_current failed: {e}"),
                }

                let deep_link_handle = handle.clone();
                app.deep_link().on_open_url(move |event| {
                    let urls: Vec<String> =
                        event.urls().iter().map(|u| u.to_string()).collect();
                    deep_link::handle_deep_link_urls(&deep_link_handle, urls);
                });
                log::info!("petal: deep-link handler registered (scheme 'petal')");
            }

            // One-time startup permission snapshot (SPEC.md §4.1): logged at
            // launch so a reader of the log can immediately see whether
            // onboarding will need to run the Screen Recording grant flow,
            // without waiting for the frontend to poll `has_screen_recording_access`.
            #[cfg(target_os = "macos")]
            {
                let screen_recording_granted = window_source::has_screen_recording_access();
                let accessibility_granted = permissions::check_accessibility();
                // Microphone belongs on this line for the same reason the other
                // two do: a denied mic is otherwise completely invisible at
                // runtime. macOS hands a denied capture session digital
                // silence rather than an error, so "nobody can hear me" and
                // "everything is fine" produce identical logs, identical
                // outbound RTP, and identical byte counters (#821). Non-
                // prompting -- `authorizationStatusForMediaType:`, same as the
                // checks above.
                let microphone_status = permissions::check_microphone();
                log::info!(
                    "petal: startup permission check -- Screen Recording access: {}, Accessibility access: {}, Microphone: {microphone_status}",
                    if screen_recording_granted {
                        "GRANTED"
                    } else {
                        "DENIED"
                    },
                    if accessibility_granted {
                        "GRANTED"
                    } else {
                        "DENIED"
                    }
                );
            }

            // Telepointer sender loop (SPEC.md §4.5): same "start unconditionally,
            // cheap no-op when nothing is shared" reasoning as hover_tab::start
            // above -- see telepointer::start_sender's doc comment.
            #[cfg(target_os = "macos")]
            {
                telepointer::start_sender(&handle);
                log::info!("petal: telepointer sender loop started");
            }

            // The cockpit reads RoomsState while creating its randomized test
            // room. Start it only after that state is managed; spawning it
            // above this point races setup and can panic on a fast runtime.
            #[cfg(feature = "cockpit-privileged")]
            if let Some(spec) = initial_test_case.clone() {
                test_cockpit::run_launch_spec_and_exit(handle.clone(), spec);
            }

            // Main-window transparency (issue #11/#14): tauri.conf.json now
            // sets `transparent: true` on the main window so pill mode shows
            // ONLY the floating pill and the app shell reads with the comp's
            // 24px rounded corners (routes paint their own opaque rounded
            // shell -- see +layout.svelte). wry's transparent(true) does NOT
            // clear WKWebView's opaque `underPageBackgroundColor` black
            // underlay on macOS 12+ (the exact compositor-chrome lesson from
            // webview_transparency.rs) -- reuse the one battle-tested
            // treatment. setup() runs on the main thread, which
            // apply_or_retry requires (documented AppKit crash class).
            #[cfg(target_os = "macos")]
            if let Some(main_window) = app.get_webview_window("main") {
                webview_transparency::apply_or_retry(&handle, &main_window);
                log::info!("petal: main window transparency treatment applied");
                if let Err(e) = crate::platform::appkit::disallow_window_tiling(&main_window) {
                    log::warn!("petal: failed to disable main-window tiling: {e}");
                } else {
                    log::info!("petal: main-window macOS tiling disabled");
                }
                let _ = main_window.center();
            }
            // #636: do NOT show the main window here. It is built
            // `visible: false` and is revealed by `frontend_ready` once the
            // frontend has mounted -- otherwise the user watches an opaque
            // WKWebView underlay with square corners (the 24px radius is CSS
            // that has not run yet) until hydration finishes. Arm only the
            // safety net.
            arm_main_window_reveal_fallback(&handle);

            // Connection resilience / network-change monitor (SPEC.md §4.8):
            // started per-room-connection today (see session::join_room), not
            // here at app launch -- there is no "always on" resilience watcher
            // independent of an active room. Logged here for visibility so a
            // reader of the log doesn't wonder why no "resilience started" line
            // appears at startup: it's expected, not a missing feature.
            log::info!(
                "petal: startup setup() complete -- resilience/network-monitor starts per-room-join, not at launch (see session::join_room)"
            );

            // Global keyboard shortcut (SPEC.md §4.2) -- registered at
            // startup, independent of any room/session state, same as the
            // menubar pill and hover-tab tracker above. Non-fatal if
            // registration fails (e.g. combo already claimed at the OS
            // level) -- a missing power-user shortcut shouldn't block launch.
            #[cfg(target_os = "macos")]
            match shortcuts::init(&handle) {
                Ok(()) => {}
                Err(e) => log::error!("petal: failed to register global shortcut: {e}"),
            }

            // Debug/test-only, env-gated end-to-end test driver. Compiled out
            // of normal release binaries; only the test-only `autotest` or
            // `cockpit-privileged` QA feature can include it. NEVER pass either
            // feature to a customer distribution. No-op unless
            // PETAL_AUTOTEST_* is set (see autotest.rs). Placed last so all managed state
            // (SessionState, RoomsState) is in place first.
            #[cfg(any(debug_assertions, feature = "autotest", feature = "cockpit-privileged"))]
            autotest::maybe_start(&handle);

            log::info!("petal: app startup end -- entering Tauri event loop");

            Ok(())
        })
        .build(context)
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Red traffic dot hides the main window, so a Dock click MUST be
            // able to bring it back: with no handler here the event is
            // discarded and the user is stranded with an invisible app.
            // Deliberately NOT reveal_main_window -- that one-shot fires once
            // per process, so a second reopen would silently do nothing.
            //
            // Restore ONLY when main is actually gone. Activating on every
            // Dock click yanks the 400px main window over arranged remote
            // share windows mid-meeting and steals key focus. Ask the window
            // itself -- NOT the event's `has_visible_windows`, which a visible
            // pill or share panel makes true while main is hidden, re-stranding
            // exactly the user this handler exists for.
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(window) = tauri::Manager::get_webview_window(app_handle, "main") {
                    // Both defaults mean "assume there is nothing to restore".
                    // A failed query must degrade to a missed restore -- which
                    // a second Dock click fixes -- not to a spurious activation
                    // that yanks this window over the user's arranged share
                    // windows mid-meeting. Do NOT "tidy" these to false/true.
                    let hidden = !window.is_visible().unwrap_or(true);
                    let minimized = window.is_minimized().unwrap_or(false);
                    if hidden || minimized {
                        show_and_activate_main_window(app_handle, "dock-reopen");
                    }
                }
            }
        });
}

#[cfg(not(target_os = "macos"))]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let log_path = logging::init();
    log::info!(
        "petal: app startup begin on unsupported native-media platform (log file: {})",
        log_path.display()
    );

    // Windows: declare per-monitor-v2 DPI awareness up front so
    // `GetWindowRect`/`EnumDisplayMonitors`/WGC report physical pixels,
    // matching what the capture pipeline and picker assume. Failure is
    // non-fatal: Tauri may already have set it (FALSE return).
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        match unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
            Ok(()) => log::info!("petal: process DPI awareness set to per-monitor v2"),
            Err(error) => log::warn!(
                "petal: SetProcessDpiAwarenessContext failed (likely already set): {error}"
            ),
        }
    }

    // Room membership uses the same token configuration on every platform.
    // Match the macOS startup order: process env wins, then the desktop .env,
    // then the legacy src-tauri/.env fallback.
    load_env_file(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
    load_env_file(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"));

    tauri::Builder::default()
        .manage(session::SessionState::default())
        .manage(transport::audio::AudioDevicePreferences::default())
        .manage(transport::camera::CameraDevicePreferences::default())
        // Network Cockpit diagnostics state (issue #19): portable ring
        // buffer + journal; the poller is started per room join in
        // session_stub::join_room_command, same seam as macOS.
        .manage(diagnostics::DiagnosticsState::default())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            reveal_main_window(app, "single-instance");
            show_and_activate_main_window(app, "single-instance");
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            use tauri::Manager;

            let app_data_dir = app.path().app_data_dir().unwrap_or_else(|error| {
                log::warn!(
                    "petal: could not resolve Windows app_data_dir ({error}); room metadata will use a temporary fallback this run"
                );
                std::env::temp_dir().join("petal-app-data-fallback")
            });
            log::info!(
                "petal: Windows rooms persistence loading from {}",
                app_data_dir.display()
            );
            // Share priority and hover-tab placement are native preferences;
            // initialize before `app_data_dir` is moved into `RoomsState`.
            share_priority::initialize(app_data_dir.clone());
            // Debug-mode setting (#669): same cross-platform init as macOS
            // above, before `app_data_dir` is moved into `RoomsState::load`.
            debug_settings::initialize(&app_data_dir);
            analytics::init(&app_data_dir);
            // AI chat settings (#656): master switch + optional user Gemini
            // key. Loaded here so the very first hover-tab render knows whether
            // the feature is on — it must be entirely invisible when it is not.
            ai_chat::settings::initialize(&app_data_dir);
            app.manage(rooms::RoomsState::load(app_data_dir));
            log::info!("petal: Windows rooms persistence loaded, RoomsState managed");
            // #636: the main window is created `visible: false` on every
            // platform, so the reveal safety net must exist here too. WebView2
            // also suspends rendering for hidden windows; without this a failed
            // frontend load would leave Windows with no window at all, forever.
            arm_main_window_reveal_fallback(app.handle());
            // Desktop app, not a browser: no autofill dropdowns / password-save
            // prompts on any input, ever (engine-level, sticks for new inputs).
            autofill::disable_autofill(app.handle());
            // Native corner radius: every rectangular Petal window is created
            // `transparent: true` from tauri.conf.json (shared with macOS), but
            // Windows transparency is DWM blur-behind, which DWM will not round.
            // Flip it to a real opaque window with native rounded corners here —
            // the window is still hidden (revealed by frontend_ready / the #636
            // fallback), so nothing flashes. Pill mode re-enables the
            // transparent blur-behind window via `set_main_pill_mode`.
            #[cfg(target_os = "windows")]
            if let Some(main_window) = app.get_webview_window("main") {
                crate::windows_corner::make_native_rounded(&main_window);
            }
            // Windows hover tab: the pill webview is created on the MAIN
            // thread (WebView2 cannot be built from the tracker thread), then
            // the cursor-following tracker starts.
            let _ = windows_hover::create_pill_window(app.handle());
            windows_hover::start(app.handle());
            // Windows telepointer sender: name-tagged cursor over shared windows.
            telepointer::start_sender(app.handle());
            // AI chat floating panel (#738): a hidden WebviewWindow singleton
            // hosting the ai-chat-panel route (macOS NSPanel parity).
            ai_chat::panel::create_ai_chat_panel(app.handle());
            log::info!("petal: ai-chat panel created");
            #[cfg(target_os = "windows")]
            {
                control_consent::create_control_consent_panel(app.handle());
                log::info!("petal: control-consent panel created");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            restart_app,
            frontend_ready,
            get_build_info,
            list_shareable_windows,
            has_screen_recording_access,
            capture_window_thumbnail,
            permissions::check_screen_recording,
            permissions::request_screen_recording,
            permissions::check_microphone,
            permissions::check_camera,
            permissions::check_accessibility,
            permissions::request_microphone,
            permissions::request_camera,
            permissions::request_accessibility,
            permissions::open_privacy_settings,
            list_rooms,
            create_room,
            rename_room,
            forget_room,
            reset_local_rooms,
            list_room_occupancy,
            session::join_room_command,
            session::leave_room_command,
            session::current_room,
            session::room_presence,
            session::remote_control_allowed,
            session::set_remote_control_allowed,
            session::remote_control_policy,
            session::set_remote_control_policy,
            session::set_share_remote_control_allowed,
            session::share_remote_control_allowed,
            get_menubar_state,
            set_mic_muted,
            toggle_menubar_mic,
            window_picker::open_window_picker_window,
            window_picker::toggle_window_picker_window,
            region_window::open_region_window,
            region_window::close_region_window,
            region_window::region_placement_active,
            region_window::region_share_state,
            region_window::sync_region_window_frame,
            region_window::region_view_options_state,
            region_window::set_region_share_priority,
            region_window::set_region_draw_active,
            region_window::region_ai_chat_start,
            region_window::region_ai_chat_stop,
            region_window::toggle_region_share,
            open_network_cockpit_window,
            transport::audio::list_audio_devices,
            transport::audio::set_audio_devices,
            camera_session::list_camera_devices,
            camera_session::list_camera_modes,
            camera_session::set_camera_device,
            camera_session::set_camera_prefs,
            camera_session::start_camera_publish_command,
            camera_session::stop_camera_publish_command,
            camera_session::camera_publish_state,
            #[cfg(target_os = "windows")]
            camera_self_view::next_self_view_frame,
            windows_compositor::compositor_list_windows,
            windows_compositor::compositor_hide_window,
            windows_compositor::compositor_activate_window,
            windows_compositor::compositor_raise_participant_windows,
            windows_compositor::compositor_start_drag,
            windows_compositor::compositor_fit_to_source,
            windows_compositor::compositor_begin_resize,
            windows_compositor::compositor_resize_window,
            windows_compositor::compositor_toggle_debug_panel,
            windows_compositor::compositor_set_draw_active,
            draw::draw_send,
            windows_share_overlay::share_overlay_set_draw_active,
            windows_share_overlay::share_overlay_draw_active,
            windows_compositor::compositor_window_debug_stats,
            // Debug-mode setting (#669): cross-platform -- also registered on
            // the macOS invoke_handler list above.
            debug_settings::debug_mode_settings,
            debug_settings::set_debug_mode,
            remote_control::remote_control_send,
            remote_control::remote_control_set_active,
            remote_control::remote_clipboard_copy,
            remote_control::remote_clipboard_paste,
            remote_control::remote_control_request_escalation,
            remote_control::remote_control_request_timed_out,
            remote_control::remote_control_revoke,
            remote_control::remote_control_answer_consent,
            remote_control::remote_control_answer_escalation,
            session::share_window,
            session::set_share_control_mode,
            session::shared_window_ids,
            // Windows hover tab: same wire contract as the macOS hover-tab
            // commands.
            windows_hover::toggle_window_share,
            windows_hover::hover_tab_page_mounted,
            windows_hover::hover_tab_drag,
            windows_hover::set_hover_tab_menu_open,
            share_priority::get_share_priority,
            share_priority::set_share_priority,
            gallery_bridge::gallery_bridge_config,
            main_window::open_main_route,
            main_window::show_main_window,
            // Windows-only native corner radius toggle (pill mode flips the
            // main window between opaque-native-rounded and transparent).
            windows_corner::set_main_pill_mode,
            quit::quit_app,
            // Network Cockpit live diagnostics (issue #19): poller + journal
            // are portable; macOS-only display-stage feeds stay gated inside
            // the module (honest nulls on Windows).
            diagnostics::get_network_snapshot,
            diagnostics::get_event_journal,
            diagnostics::set_cockpit_open,
            diagnostics::record_video_stream_state,
            // Cross-platform commands: Export logs (archive + redaction are
            // neutral; the reveal uses Explorer) and the updater (plugin API;
            // the arch guard verifies the NSIS PE machine type on Windows).
            logging::export_logs,
            logging::log_updater_event,
            updater::check_compatible_update_available,
            updater::run_launch_update_check,
            updater::download_and_install_compatible_update,
            // AI chat (#656, Windows parity): the session engine and native
            // surfaces compile on every platform now — same command surface
            // as the macOS invoke_handler list above, so the frontend's
            // Settings toggle, hover-tab button, and panel all work.
            ai_chat::commands::ai_chat_settings,
            ai_chat::commands::ai_chat_set_enabled,
            ai_chat::commands::ai_chat_set_api_key,
            ai_chat::commands::ai_chat_is_active,
            ai_chat::commands::ai_chat_start,
            ai_chat::commands::ai_chat_stop,
            ai_chat::commands::ai_chat_ptt_start,
            ai_chat::commands::ai_chat_ptt_end,
            ai_chat::commands::ai_chat_send_text,
            ai_chat::commands::ai_chat_request_start,
            ai_chat::commands::ai_chat_request_stop,
            ai_chat::commands::ai_chat_request_send_text,
            ai_chat::commands::ai_chat_request_ptt_start,
            ai_chat::commands::ai_chat_request_ptt_end,
            ai_chat::commands::ai_chat_remote_session,
            ai_chat::commands::ai_chat_control_approve,
            ai_chat::commands::ai_chat_control_reject,
            ai_chat::commands::ai_chat_control_resume,
            ai_chat::commands::ai_chat_control_status,
            ai_chat::panel::ai_chat_panel_present,
            ai_chat::panel::ai_chat_panel_dismiss,
            #[cfg(target_os = "windows")]
            control_consent::control_consent_present,
            #[cfg(target_os = "windows")]
            control_consent::control_consent_dismiss,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Petal");
}

fn load_env_file(path: &'static str) {
    match dotenvy::from_path(path) {
        Ok(_) => log::info!("petal: loaded .env from {path}"),
        Err(e) => log::info!("petal: no .env loaded from {path} ({e})"),
    }
}

#[cfg(test)]
mod main_window_reveal_tests {
    use super::{main_window_revealed, MAIN_WINDOW_REVEALED};
    use std::sync::atomic::Ordering;

    /// Restores `MAIN_WINDOW_REVEALED` on the way out. It is process-global and
    /// cargo runs tests as threads in ONE process -- #780 was a test leaving a
    /// global flipped for a sibling to read. Drop, not hand-written cleanup, so
    /// a panicking assert cannot skip it.
    struct RevealFlagGuard(bool);

    impl Drop for RevealFlagGuard {
        fn drop(&mut self) {
            MAIN_WINDOW_REVEALED.store(self.0, Ordering::Release);
        }
    }

    fn hold_reveal_flag() -> RevealFlagGuard {
        RevealFlagGuard(MAIN_WINDOW_REVEALED.load(Ordering::Acquire))
    }

    /// The reopen paths ask `main_window_revealed()` on EVERY reopen, whereas
    /// `reveal_main_window` consumes its flag with a one-shot `swap(true)`. If
    /// this read consumed the flag the same way, the FIRST Dock reopen after a
    /// hide would work and every later one would silently do nothing -- the
    /// exact defect a manual first-reopen check cannot see. One test, not two,
    /// so two cases cannot race each other over the same global.
    #[test]
    fn reading_the_reveal_flag_never_consumes_it() {
        let _restore = hold_reveal_flag();

        MAIN_WINDOW_REVEALED.store(false, Ordering::Release);
        assert!(
            !main_window_revealed(),
            "before the first reveal this must read false, or #636's cold start \
             is mistaken for a user-initiated hide and open_main_route shows an \
             unpainted window"
        );

        MAIN_WINDOW_REVEALED.store(true, Ordering::Release);
        for attempt in 1..=5 {
            assert!(
                main_window_revealed(),
                "read #{attempt} must still observe the reveal -- a consuming read \
                 makes every reopen after the first a no-op"
            );
        }
        assert!(
            MAIN_WINDOW_REVEALED.load(Ordering::Acquire),
            "the flag itself must be untouched by reading it"
        );
    }
}
