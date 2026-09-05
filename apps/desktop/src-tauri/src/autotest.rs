//! Debug-only, env-gated end-to-end test driver.
//!
//! **This is OFF unless `PETAL_AUTOTEST_ROOM` or `PETAL_AUTOTEST_SOCK` is set.**
//! With no env var it is a pure no-op and has zero effect on a normal launch --
//! it exists solely so an automated/headless test can drive the app's real
//! room-join + window-share code paths and watch the result in the log / on
//! the wire.
//!
//! It does NOT invent a parallel share mechanism: it calls the exact same
//! `session::join_room` and `session::start_share` functions the frontend's
//! join button and the hover-tab share pill call. So a green run here exercises
//! the real thing, not a mock.
//!
//! Env vars (all optional except the first, which is the on/off switch):
//! - `PETAL_AUTOTEST_ROOM`     -- opaque QA room key (e.g. `webtest`). The
//!                                durable ownership mapping, not this label,
//!                                selects the room capability. Setting this
//!                                is what turns the hook on.
//! - `PETAL_AUTOTEST_FRESH_ROOM=1` -- explicitly create a dedicated room when
//!                                this QA key has no persisted mapping.
//! - `PETAL_AUTOTEST_IDENTITY` -- LiveKit identity to join as (default
//!                                `native-autotest`).
//! - `PETAL_AUTOTEST_NAME`     -- display name (default same as identity).
//! - `PETAL_AUTOTEST_SHARE`    -- `auto` to share the first non-Petal
//!                                shareable window, `owner:<AppName>` to share
//!                                only a window belonging to that exact app
//!                                (safe for tests: spawn a sacrificial app like
//!                                TextEdit and share only it, never the user's
//!                                own windows), or a specific `CGWindowID`
//!                                (u32). Omit to join without sharing.
//! - `PETAL_AUTOTEST_SHARE_WITH_BORDER` -- `1`/`true`/`yes` routes the initial
//!                                share through the exact hover-tab lifecycle,
//!                                including its native share border.
//! - `PETAL_AUTOTEST_PICKER_TARGET` -- debug-only exact target for the macOS
//!                                system picker path (`window:<CGWindowID>`,
//!                                `pid:<owner pid>`, `owner:<AppName>`, or a
//!                                bare window id). It bypasses the interactive
//!                                picker while still invoking the real picker
//!                                capture/publish path.
//! - `PETAL_AUTOTEST_SOCK`     -- path to a Unix socket that accepts
//!                                newline-delimited JSON commands for the
//!                                scenario runner (issue #35).
//!                                The `reconnect` command is a one-shot SDK
//!                                resume/full-reconnect simulator for the
//!                                cockpit/autotest build only (#298).
//!
//! Because a `tauri dev` binary inherits its launching shell's environment
//! (unlike a `.app` launched via `open`), the caller sets these before
//! `npm run dev:clean` / `cargo run` and they reach the app.

#[cfg(target_os = "macos")]
pub fn maybe_start(app: &tauri::AppHandle) {
    use tauri::{Emitter, Manager};

    let sock = std::env::var("PETAL_AUTOTEST_SOCK").ok();
    if let Some(path) = sock.as_deref().filter(|s| !s.trim().is_empty()) {
        start_command_server(app.clone(), path.to_string());
    }

    // PETAL_AUTOTEST_REQUEST_AX=1: fire the real onboarding Accessibility
    // request at startup (headless rigs have no onboarding UI to click). With a
    // disclaimed launch this registers the DEV BINARY itself in the
    // Accessibility pane -- required because inherited (responsible-process)
    // trust returns redacted AX window elements on Darwin 25.2 (#747 §9.14).
    if std::env::var_os("PETAL_AUTOTEST_REQUEST_AX").is_some() {
        let outcome = crate::permissions::request_accessibility();
        log::warn!("autotest: PETAL_AUTOTEST_REQUEST_AX set -- request_accessibility() -> {outcome:?}");
    }

    let Some(room) = autotest_room_from_env(std::env::var("PETAL_AUTOTEST_ROOM").ok()) else {
        return; // startup sequence off -- command socket may still be on
    };
    let fresh_room = autotest_fresh_room_from_env(
        std::env::var("PETAL_AUTOTEST_FRESH_ROOM").ok(),
    );
    // Must match the backend's GENERATED_PARTICIPANT_ID shape
    // (`^p-[a-z0-9]+-[a-z0-9]+$`) -- found live verifying this session's other
    // autotest.rs changes: the old "native-autotest" default has never
    // actually been joinable against a real backend, it was rejected with
    // "identity must be a generated participant id" every time this env var
    // was left unset.
    let identity = std::env::var("PETAL_AUTOTEST_IDENTITY")
        .unwrap_or_else(|_| "p-native-autotest".to_string());
    let display = std::env::var("PETAL_AUTOTEST_NAME").unwrap_or_else(|_| identity.clone());
    let share = std::env::var("PETAL_AUTOTEST_SHARE").ok();

    log::warn!(
        "autotest: PETAL_AUTOTEST_ROOM='{room}' set -- DEBUG test hook active (join as '{identity}', share={share:?}). This is env-gated and never runs without that var."
    );

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Give the event loop + managed state a beat to settle before driving.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let rooms = app.state::<crate::rooms::RoomsState>();
        let session = app.state::<crate::session::SessionState>();

        let room = match rooms.resolve_autotest_room(&room, fresh_room) {
            Ok(record) => record.name,
            Err(error) => {
                let diagnostic = format!("autotest room ownership rejected: {error}");
                let result = AutotestJoinResult::Failed {
                    reason: "session_setup_failed",
                };
                app.state::<AutotestJoinState>().record(result.clone());
                log::error!("autotest: {diagnostic}; refusing to join any room");
                let _ = app.emit("autotest-join-result", result);
                return;
            }
        };

        match crate::session::join_room(
            &app,
            rooms.inner(),
            session.inner(),
            room.clone(),
            identity.clone(),
            display.clone(),
            crate::remote_control_core::RemoteControlPolicy::Auto,
            None,
        )
        .await
        {
            Ok(rec) => {
                let result = AutotestJoinResult::Joined {
                    room_name: rec.name.clone(),
                };
                app.state::<AutotestJoinState>().record(result.clone());
                log::warn!(
                    "autotest: join succeeded; requesting active meeting UI (record id {})",
                    rec.id
                );
                let _ = app.emit("autotest-join-result", result);
            }
            Err(e) => {
                let reason = autotest_join_failure_reason(&e);
                let result = AutotestJoinResult::Failed { reason };
                app.state::<AutotestJoinState>().record(result.clone());
                log::error!("autotest: join failed ({reason}); active meeting UI was not opened");
                let _ = app.emit("autotest-join-result", result);
                return;
            }
        }

        let Some(share) = share else {
            log::info!("autotest: joined without sharing (PETAL_AUTOTEST_SHARE unset)");
            return;
        };

        // Resolve which window to share.
        let window_id: Option<u32> = if share.trim().eq_ignore_ascii_case("auto") {
            match crate::window_source::list() {
                Ok(windows) => {
                    let pick = windows.iter().find(|w| {
                        // Don't share Petal's own windows -- that's both
                        // pointless and visually recursive.
                        if w.app_bundle_id == "com.petal.app" || w.app_name == "Petal" {
                            return false;
                        }
                        // Safety: never auto-grab what's obviously a live
                        // video call (the tester may be on a parallel call).
                        // Google Meet/Teams run inside a browser so we can't
                        // skip by app -- match the window title too.
                        let title = w.title.as_deref().unwrap_or("").to_ascii_lowercase();
                        let app = w.app_name.to_ascii_lowercase();
                        let bundle = w.app_bundle_id.to_ascii_lowercase();
                        let looks_like_a_call = ["zoom", "webex", "facetime", "google meet"]
                            .iter()
                            .any(|k| app.contains(k) || bundle.contains(k))
                            || ["meet -", "zoom", "webex", "facetime", "is sharing", "· meeting"]
                                .iter()
                                .any(|k| title.contains(k));
                        if looks_like_a_call {
                            log::info!(
                                "autotest: skipping call-like window {} ('{}' - {:?}) for auto-share",
                                w.window_id,
                                w.app_name,
                                w.title
                            );
                            return false;
                        }
                        true
                    });
                    match pick {
                        Some(w) => {
                            log::info!(
                                "autotest: auto-picked window {} ('{}' - {:?}) to share",
                                w.window_id,
                                w.app_name,
                                w.title
                            );
                            Some(w.window_id)
                        }
                        None => {
                            log::warn!(
                                "autotest: no non-Petal shareable window found to auto-share"
                            );
                            None
                        }
                    }
                }
                Err(e) => {
                    log::error!("autotest: window enumeration failed: {e:?}");
                    None
                }
            }
        } else if let Some(owner) = share.trim().strip_prefix("owner:") {
            // Share ONLY a window belonging to this exact app name -- the safe
            // e2e mode: the test spawns its own sacrificial app (e.g. TextEdit)
            // and can never grab one of the user's real windows.
            match crate::window_source::list() {
                Ok(windows) => match windows.iter().find(|w| w.app_name == owner) {
                    Some(w) => {
                        log::info!(
                            "autotest: owner-matched window {} ('{}' - {:?}) to share",
                            w.window_id,
                            w.app_name,
                            w.title
                        );
                        Some(w.window_id)
                    }
                    None => {
                        log::warn!("autotest: no shareable window owned by '{owner}' found");
                        None
                    }
                },
                Err(e) => {
                    log::error!("autotest: window enumeration failed: {e:?}");
                    None
                }
            }
        } else {
            match share.trim().parse::<u32>() {
                Ok(id) => Some(id),
                Err(_) => {
                    log::error!(
                        "autotest: PETAL_AUTOTEST_SHARE='{share}' is neither 'auto' nor a valid u32 window id"
                    );
                    None
                }
            }
        };

        let Some(window_id) = window_id else { return };

        let Some(frame) = crate::platform::cg::frame_for_window_id(window_id) else {
            log::error!(
                "autotest: couldn't resolve an on-screen frame for window {window_id} (closed?), skipping share"
            );
            return;
        };

        let share_with_border = std::env::var("PETAL_AUTOTEST_SHARE_WITH_BORDER")
            .ok()
            .is_some_and(|value| autotest_flag_enabled(&value));
        if share_with_border {
            let shared =
                crate::hover_tab::toggle_share_for_window(&app, session.inner(), window_id, frame)
                    .await;
            if !shared {
                log::error!(
                    "autotest: bordered initial share failed for source window {window_id}"
                );
                return;
            }
            log::info!(
                "autotest: bordered initial share active for source window {window_id}; exact hover-tab lifecycle exercised; frontmost={}",
                crate::platform::appkit::frontmost_app_label()
            );
        } else {
            match crate::session::start_share(&app, session.inner(), window_id, frame).await {
                Ok(()) => log::info!(
                    "autotest: start_share(window {window_id}) succeeded; frontmost={}",
                    crate::platform::appkit::frontmost_app_label()
                ),
                Err(e) => {
                    log::error!("autotest: start_share(window {window_id}) failed: {e:?}");
                    return;
                }
            }
        }

        // Optional enable/disable toggle exercise (SPEC.md §4.2 window tab):
        // `PETAL_AUTOTEST_TOGGLE_SECS=N` makes the driver disable then re-enable
        // sharing of THIS SAME window every N seconds, `PETAL_AUTOTEST_TOGGLE_CYCLES`
        // times (default 2) -- via `hover_tab::toggle_share_for_window`, the
        // EXACT function the hover-pill click and the global shortcut call,
        // share-border panel show/hide included. (This used to call bare
        // `session::stop_share`/`start_share`, which silently skipped the
        // native-panel path and MISSED the panel `close()` teardown abort a
        // real pill unshare click hit live on 2026-07-02 -- see
        // `share_border.rs`'s module doc.)
        //
        // Unless PETAL_AUTOTEST_SHARE_WITH_BORDER opted into the exact UI path,
        // the initial share above has no border. First stop either form and run
        // clean toggle-on/toggle-off pairs.
        let toggle_secs: Option<u64> = std::env::var("PETAL_AUTOTEST_TOGGLE_SECS")
            .ok()
            .and_then(|v| v.parse().ok());
        let Some(secs) = toggle_secs else { return };
        let cycles: u32 = std::env::var("PETAL_AUTOTEST_TOGGLE_CYCLES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        crate::remote_control::revoke_window(&app, window_id, "autotest stopped initial share");
        match crate::session::stop_share(&app, session.inner(), window_id).await {
            Ok(()) => log::info!(
                "autotest: stopped the initial plain share of window {window_id}; pill-toggle pairs take over"
            ),
            Err(e) => log::error!("autotest: initial stop_share failed: {e:?}"),
        }
        for cycle in 1..=cycles {
            for phase in ["ENABLE", "DISABLE"] {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                let Some(frame) = crate::platform::cg::frame_for_window_id(window_id) else {
                    log::warn!(
                        "autotest: toggle cycle {cycle} -- window {window_id} no longer on screen, stopping toggle"
                    );
                    return;
                };
                let on = crate::hover_tab::toggle_share_for_window(
                    &app,
                    session.inner(),
                    window_id,
                    frame,
                )
                .await;
                log::info!(
                    "autotest: toggle cycle {cycle}/{cycles} -- pill-toggle {phase} window {window_id} (now_shared={on})"
                );
            }
        }
        log::info!(
            "autotest: toggle exercise complete (pill-toggle path, borders and share bars included)"
        );
    });
}

/// The frontend-only route owns the active meeting UI.  This event is emitted
/// only by the env-gated debug hook after the real Rust join reaches a terminal
/// result; ordinary user joins continue to navigate through their existing UI.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub(crate) enum AutotestJoinResult {
    Joined {
        #[serde(rename = "roomName")]
        room_name: String,
    },
    Failed {
        reason: &'static str,
    },
}

/// One-slot handoff for the env-gated debug hook. The frontend atomically takes
/// it after its own route/onboarding work is ready, so startup events cannot be
/// lost or replayed by a later route remount.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub(crate) struct AutotestJoinState {
    terminal_result: std::sync::Mutex<Option<AutotestJoinResult>>,
}

#[cfg(target_os = "macos")]
impl AutotestJoinState {
    fn record(&self, result: AutotestJoinResult) {
        let mut terminal = self
            .terminal_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *terminal = Some(result);
    }

    fn take(&self) -> Option<AutotestJoinResult> {
        self.terminal_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// Atomically take the terminal debug-autojoin outcome. This command shares the
/// same debug/test compilation gates as the env hook itself.
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn autotest_join_result(
    state: tauri::State<'_, AutotestJoinState>,
) -> Option<AutotestJoinResult> {
    state.take()
}

#[cfg(target_os = "macos")]
fn autotest_room_from_env(value: Option<String>) -> Option<String> {
    value.filter(|room| !room.trim().is_empty())
}

#[cfg(target_os = "macos")]
fn autotest_fresh_room_from_env(value: Option<String>) -> bool {
    matches!(value.as_deref().map(str::trim), Some("1"))
}

#[cfg(target_os = "macos")]
fn autotest_join_failure_reason(error: &crate::session::ShareSessionError) -> &'static str {
    use crate::session::ShareSessionError;

    match error {
        ShareSessionError::PermissionDenied => "permission_denied",
        ShareSessionError::Config(_) => "backend_token_unavailable",
        ShareSessionError::RoomConnect(_) => "room_connection_failed",
        ShareSessionError::JoinTimeout => "join_timeout",
        _ => "session_setup_failed",
    }
}

#[cfg(all(test, target_os = "macos"))]
mod join_result_tests {
    use super::{
        autotest_join_failure_reason, autotest_room_from_env, AutotestJoinResult, AutotestJoinState,
    };
    use crate::session::ShareSessionError;

    #[test]
    fn no_autotest_room_keeps_the_startup_hook_off() {
        assert_eq!(autotest_room_from_env(None), None);
        assert_eq!(autotest_room_from_env(Some("  \t".to_string())), None);
        assert_eq!(
            autotest_room_from_env(Some("qa-room".to_string())),
            Some("qa-room".to_string())
        );
    }

    #[test]
    fn terminal_join_results_are_routeable_or_redacted() {
        let joined = serde_json::to_value(AutotestJoinResult::Joined {
            room_name: "room-test".to_string(),
        })
        .unwrap();
        assert_eq!(joined["status"], "joined");
        assert_eq!(joined["roomName"], "room-test");

        let failed = serde_json::to_value(AutotestJoinResult::Failed {
            reason: autotest_join_failure_reason(&ShareSessionError::Config(
                "https://backend.example/token?secret=redacted".to_string(),
            )),
        })
        .unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["reason"], "backend_token_unavailable");
        assert!(!failed.to_string().contains("backend.example"));
        assert!(!failed.to_string().contains("secret"));
    }

    #[test]
    fn issue569_join_timeout_has_a_stable_terminal_reason() {
        assert_eq!(
            autotest_join_failure_reason(&ShareSessionError::JoinTimeout),
            "join_timeout"
        );
    }

    #[test]
    fn terminal_result_is_consumed_after_the_startup_event() {
        let state = AutotestJoinState::default();
        assert!(state.take().is_none());

        state.record(AutotestJoinResult::Joined {
            room_name: "joined-before-frontend".to_string(),
        });

        let replay = serde_json::to_value(state.take().expect("terminal result"))
            .expect("serializable terminal result");
        assert_eq!(replay["status"], "joined");
        assert_eq!(replay["roomName"], "joined-before-frontend");
        assert!(
            state.take().is_none(),
            "a remounted main route cannot replay it"
        );
    }

    #[test]
    fn autotest_terminal_markers_are_warn_visible_and_unique() {
        let source = include_str!("autotest.rs");
        let production_source = source
            .split_once("\n#[cfg(all(test, target_os = \"macos\"))]")
            .map(|(production, _)| production)
            .expect("autotest source must keep tests after production code");

        assert_eq!(
            production_source
                .matches("autotest: join succeeded;")
                .count(),
            1,
            "successful autotest join must emit one authoritative terminal"
        );
        assert!(
            production_source.contains(
                "log::warn!(\n                    \"autotest: join succeeded;"
            ),
            "successful autotest terminal must survive RUST_LOG=warn"
        );
        assert_eq!(
            production_source.matches("autotest: join failed").count(),
            1,
            "failed autotest join must emit one authoritative terminal"
        );
        assert!(
            production_source.contains("log::error!(\"autotest: join failed"),
            "failed autotest terminal must remain warn-visible"
        );
    }
}

fn autotest_flag_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

#[cfg(test)]
mod initial_share_tests {
    use super::autotest_flag_enabled;

    #[test]
    fn bordered_share_flag_is_explicit_and_case_insensitive() {
        for enabled in ["1", "true", "TRUE", " yes "] {
            assert!(autotest_flag_enabled(enabled));
        }
        for disabled in ["", "0", "false", "on", "border"] {
            assert!(!autotest_flag_enabled(disabled));
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn maybe_start(_app: &tauri::AppHandle) {}

#[cfg(target_os = "macos")]
mod command_server {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde::{Deserialize, Serialize};
    use tauri::Manager;

    #[derive(Debug, Deserialize)]
    #[serde(tag = "cmd", rename_all = "snake_case")]
    enum Command {
        DumpState,
        CurrentRoom,
        Join {
            room: String,
            identity: Option<String>,
            name: Option<String>,
        },
        Leave,
        /// One deterministic SDK reconnect attempt for #298's local
        /// replacement-SID validation. The command server itself is opt-in,
        /// and this module is compiled out of normal release builds.
        Reconnect {
            mode: ReconnectMode,
        },
        #[serde(rename = "remote-control-disable", alias = "remote_control_disable")]
        RemoteControlDisable {
            window_id: u32,
        },
        /// Consent flow (ask policy): answer the parked request for one
        /// controller as the sharer would from the prompt.
        #[serde(rename = "remote-control-consent-answer", alias = "remote_control_consent_answer")]
        RemoteControlConsentAnswer {
            window_id: u32,
            controller_id: String,
            approve: bool,
        },
        /// Set the live meeting policy (`off` / `ask` / `auto`). The autotest
        /// join seeds `auto` so the legacy cases keep auto-granting; the
        /// consent cases switch to `ask` and back.
        #[serde(rename = "remote-control-policy", alias = "remote_control_policy")]
        RemoteControlPolicy {
            policy: String,
        },
        #[serde(rename = "remote-control-status", alias = "remote_control_status")]
        RemoteControlStatus {
            #[serde(default)]
            window_id: Option<u32>,
        },
        Share {
            window_id: u32,
        },
        StopShare {
            window_id: u32,
        },
        ShareBorderStack {
            window_id: u32,
        },
        ListWindows,
        ShareMatching {
            app_name: Option<String>,
            title_contains: Option<String>,
            pid: Option<i32>,
        },
        AccessibilityStatus,
        /// Test-cockpit walking-skeleton metric readback (#254): the existing
        /// `DiagnosticsState::snapshot()`/`journal()` (already computed by the
        /// live poller for the Network Cockpit UI) plus a bounded journal
        /// tail, exposed on the autotest socket so a scripted driver can poll
        /// for e.g. a `petal-window-*` track's fps without any UI. Read-only,
        /// synchronous (no `block_on`) -- cannot stall the socket thread.
        DumpMetrics {
            #[serde(default)]
            journal_tail: Option<usize>,
        },
    }

    /// The two LiveKit SDK reconnection paths we need to distinguish in the
    /// local #298 proof. Keep the wire names deliberately small and explicit:
    /// this is a test driver, not a user-facing network-control surface.
    #[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum ReconnectMode {
        Resume,
        Full,
    }

    impl ReconnectMode {
        const fn label(self) -> &'static str {
            match self {
                Self::Resume => "resume",
                Self::Full => "full",
            }
        }

        const fn scenario(self) -> livekit::SimulateScenario {
            match self {
                Self::Resume => livekit::SimulateScenario::SignalReconnect,
                Self::Full => livekit::SimulateScenario::FullReconnect,
            }
        }
    }

    /// A live SDK simulation cannot safely be repeated while the first
    /// reconnect is resolving. Keep this test driver deterministic: one
    /// process gets one request, including a failed request, and a fresh test
    /// process starts with a fresh gate.
    struct OneShotReconnect {
        requested: AtomicBool,
    }

    impl OneShotReconnect {
        const fn new() -> Self {
            Self {
                requested: AtomicBool::new(false),
            }
        }

        fn claim(&self) -> Result<(), String> {
            self.requested
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .map(|_| ())
                .map_err(|_| {
                    "reconnect already requested in this test process; restart it for another one-shot simulation"
                        .to_string()
                })
        }
    }

    static RECONNECT_REQUEST: OneShotReconnect = OneShotReconnect::new();

    /// Default number of trailing journal entries `dump_metrics` includes
    /// when the caller doesn't request a specific count.
    const DEFAULT_JOURNAL_TAIL: usize = 50;

    /// Pure shaping of a `dump_metrics` response from already-fetched
    /// diagnostics state -- split out from `run_command` so it is unit
    /// testable without a live `tauri::AppHandle` (this crate has no
    /// `tauri::test` mock-builder usage yet).
    fn dump_metrics_value(
        network: crate::diagnostics::NetworkSnapshot,
        mut journal: Vec<crate::diagnostics::JournalEntry>,
        journal_tail: Option<usize>,
    ) -> serde_json::Value {
        let tail = journal_tail
            .unwrap_or(DEFAULT_JOURNAL_TAIL)
            .min(journal.len());
        let tail_entries = journal.split_off(journal.len() - tail);
        serde_json::json!({
            "network": network,
            "journalTail": tail_entries,
        })
    }

    #[derive(Debug, Serialize)]
    struct Response<T: Serialize> {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<T>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    }

    #[derive(Debug, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct DumpState {
        current_room: Option<String>,
        session_shared_window_ids: Vec<u32>,
        hover_shared_window_ids: Vec<u32>,
        accessibility_trusted: bool,
    }

    pub(super) fn start(app: tauri::AppHandle, path: String) {
        log::warn!("autotest: PETAL_AUTOTEST_SOCK='{path}' set -- DEBUG command channel active");
        std::thread::spawn(move || {
            let _ = std::fs::remove_file(&path);
            let listener = match UnixListener::bind(&path) {
                Ok(listener) => listener,
                Err(e) => {
                    log::error!("autotest: failed to bind command socket '{path}': {e}");
                    return;
                }
            };
            // Debug-only local control channel. Trust boundary is the local
            // user account running Petal; keep the socket owner-only so other
            // users on the same machine cannot drive the autotest API.
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                log::warn!("autotest: failed to chmod command socket '{path}' to 0600: {e}");
            }
            log::info!("autotest: command socket listening at {path}");
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => handle_stream(app.clone(), stream),
                    Err(e) => log::warn!("autotest: command socket accept failed: {e}"),
                }
            }
        });
    }

    fn handle_stream(app: tauri::AppHandle, stream: UnixStream) {
        let Ok(writer) = stream.try_clone() else {
            return;
        };
        let mut writer = writer;
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let line = match line {
                Ok(line) if !line.trim().is_empty() => line,
                Ok(_) => continue,
                Err(e) => {
                    log::warn!("autotest: command socket read failed: {e}");
                    break;
                }
            };
            let response = match serde_json::from_str::<Command>(&line) {
                Ok(command) => run_command(&app, command),
                Err(e) => json_response(Err(format!("bad command JSON: {e}"))),
            };
            let _ = writer.write_all(response.as_bytes());
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
        }
    }

    fn run_command(app: &tauri::AppHandle, command: Command) -> String {
        let result = match command {
            Command::DumpState => dump_state(app)
                .and_then(|state| serde_json::to_value(state).map_err(|e| e.to_string())),
            Command::CurrentRoom => current_room(app),
            Command::Join {
                room,
                identity,
                name,
            } => {
                let identity = identity.unwrap_or_else(|| "p-native-autotest".to_string());
                let name = name.unwrap_or_else(|| identity.clone());
                let app = app.clone();
                tauri::async_runtime::block_on(async move {
                    let rooms = app.state::<crate::rooms::RoomsState>();
                    let session = app.state::<crate::session::SessionState>();
                    crate::session::join_room(
                        &app,
                        rooms.inner(),
                        session.inner(),
                        room,
                        identity,
                        name,
                        crate::remote_control_core::RemoteControlPolicy::Auto,
                        None,
                    )
                    .await
                    .map(|record| serde_json::json!({ "room": record.name, "id": record.id }))
                    .map_err(|e| e.to_string())
                })
            }
            Command::Leave => {
                let app = app.clone();
                tauri::async_runtime::block_on(async move {
                    let session = app.state::<crate::session::SessionState>();
                    crate::session::leave_room(&app, session.inner()).await;
                    Ok(serde_json::json!({ "left": true }))
                })
            }
            Command::Reconnect { mode } => reconnect(app, mode),
            Command::RemoteControlDisable { window_id } => {
                crate::remote_control::revoke_window(
                    app,
                    window_id,
                    "autotest remote-control disable",
                );
                Ok(serde_json::json!({ "windowId": window_id, "disabled": true }))
            }
            Command::RemoteControlConsentAnswer { window_id, controller_id, approve } => {
                let answered = crate::remote_control::answer_consent(
                    app,
                    window_id,
                    &controller_id,
                    approve,
                    crate::remote_control_core::RemoteControlReason::ConsentDenied,
                );
                Ok(serde_json::json!({
                    "windowId": window_id,
                    "controllerId": controller_id,
                    "approve": approve,
                    "answered": answered,
                }))
            }
            Command::RemoteControlPolicy { policy } => {
                let policy = crate::remote_control_core::RemoteControlPolicy::from_wire(&policy);
                let session = app.state::<crate::session::SessionState>();
                session.seed_remote_control_policy(policy);
                if !policy.allows_requests() {
                    crate::remote_control::revoke_all(app);
                }
                Ok(serde_json::json!({ "policy": policy.as_wire() }))
            }
            Command::RemoteControlStatus { window_id } => {
                let mut snapshot = crate::remote_control::autotest_status_snapshot();
                if let Some(window_id) = window_id {
                    for key in ["sessions", "pressedInputs", "pending"] {
                        if let Some(items) =
                            snapshot.get_mut(key).and_then(|value| value.as_array_mut())
                        {
                            items.retain(|item| {
                                item.get("windowId").and_then(|value| value.as_u64())
                                    == Some(window_id as u64)
                            });
                        }
                    }
                }
                Ok(snapshot)
            }
            Command::Share { window_id } => share_window(app, window_id),
            Command::StopShare { window_id } => {
                let app = app.clone();
                tauri::async_runtime::block_on(async move {
                    let session = app.state::<crate::session::SessionState>();
                    if session.inner().is_share_active(window_id) {
                        if let Some(frame) = crate::platform::cg::frame_for_window_id(window_id) {
                            crate::hover_tab::toggle_share_for_window(
                                &app,
                                session.inner(),
                                window_id,
                                frame,
                            )
                            .await;
                        } else {
                            crate::hover_tab::clear_share_state_for_window(&app, window_id);
                            crate::remote_control::revoke_window(
                                &app,
                                window_id,
                                "autotest stopped missing share",
                            );
                            crate::session::stop_share(&app, session.inner(), window_id)
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    Ok(serde_json::json!({ "windowId": window_id, "shared": false }))
                })
            }
            Command::ShareBorderStack { window_id } => {
                crate::share_border::qa_share_border_stack_report(app, window_id)
                    .and_then(|report| serde_json::to_value(report).map_err(|e| e.to_string()))
            }
            Command::ListWindows => crate::window_source::list()
                .map(|windows| serde_json::json!({ "windows": windows }))
                .map_err(|e| e.to_string()),
            Command::ShareMatching {
                app_name,
                title_contains,
                pid,
            } => matching_window(app_name, title_contains, pid).and_then(|window| {
                share_window(app, window.window_id).map(|mut value| {
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert("window".to_string(), serde_json::json!(window));
                    }
                    value
                })
            }),
            Command::AccessibilityStatus => Ok(serde_json::json!({
                "trusted": accessibility_trusted()
            })),
            Command::DumpMetrics { journal_tail } => {
                match app.try_state::<crate::diagnostics::DiagnosticsState>() {
                    Some(diagnostics) => Ok(dump_metrics_value(
                        diagnostics.snapshot(),
                        diagnostics.journal(),
                        journal_tail,
                    )),
                    None => Err("diagnostics state unavailable".to_string()),
                }
            }
        };
        json_response(result)
    }

    fn share_window(app: &tauri::AppHandle, window_id: u32) -> Result<serde_json::Value, String> {
        let app = app.clone();
        tauri::async_runtime::block_on(async move {
            let session = app.state::<crate::session::SessionState>();
            if !session.inner().is_share_active(window_id) {
                let frame = crate::platform::cg::frame_for_window_id(window_id)
                    .ok_or_else(|| format!("window {window_id} is not on screen"))?;
                crate::hover_tab::toggle_share_for_window(&app, session.inner(), window_id, frame)
                    .await;
            }
            Ok(serde_json::json!({ "windowId": window_id, "shared": true }))
        })
    }

    /// Invoke exactly one SDK-owned reconnect scenario on the already-joined
    /// room. This deliberately does not close/rejoin or touch publications:
    /// #298 needs the real resume/full-reconnect lifecycle to exercise its
    /// existing publication-recovery code. The socket is owner-only and only
    /// exists in debug/autotest/cockpit-privileged builds.
    fn reconnect(app: &tauri::AppHandle, mode: ReconnectMode) -> Result<serde_json::Value, String> {
        let app = app.clone();
        tauri::async_runtime::block_on(async move {
            let session = app.state::<crate::session::SessionState>();
            let (publisher, _identity, _shares) = session.inner().shared_windows_snapshot();
            let publisher = publisher.ok_or_else(|| {
                "reconnect requires an already-joined room; run join before reconnect".to_string()
            })?;

            // Claim before entering LiveKit so two socket clients cannot queue
            // overlapping resume/full scenarios against the same session.
            RECONNECT_REQUEST.claim()?;

            publisher
                .room()
                .simulate_scenario(mode.scenario())
                .await
                .map_err(|e| format!("SDK {} reconnect simulation failed: {e}", mode.label()))?;

            log::warn!(
                "autotest: requested one-shot SDK {} reconnect simulation for #298",
                mode.label()
            );
            Ok(serde_json::json!({
                "requested": true,
                "mode": mode.label(),
            }))
        })
    }

    fn matching_window(
        app_name: Option<String>,
        title_contains: Option<String>,
        pid: Option<i32>,
    ) -> Result<crate::window_source::ShareableWindow, String> {
        let app_name = app_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let title_contains = title_contains
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        if app_name.is_none() && title_contains.is_none() && pid.is_none() {
            return Err(
                "share_matching requires at least one selector: app_name, title_contains, or pid"
                    .to_string(),
            );
        }

        let windows = crate::window_source::list().map_err(|e| e.to_string())?;
        let matches: Vec<_> = windows
            .into_iter()
            .filter(|w| {
                app_name.as_ref().map_or(true, |name| &w.app_name == name)
                    && title_contains.as_ref().map_or(true, |needle| {
                        w.title
                            .as_deref()
                            .unwrap_or("")
                            .to_ascii_lowercase()
                            .contains(needle)
                    })
                    && pid.map_or(true, |expected| w.app_pid == expected)
            })
            .collect();

        match matches.as_slice() {
            [window] => Ok(window.clone()),
            [] => Err("no shareable window matched the provided selectors".to_string()),
            many => Err(format!(
                "share_matching matched {} windows; use a unique title_contains or pid selector: {}",
                many.len(),
                many.iter()
                    .take(5)
                    .map(|w| format!("{}:{}:{:?}", w.window_id, w.app_name, w.title))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn dump_state(app: &tauri::AppHandle) -> Result<DumpState, String> {
        let state = app
            .try_state::<crate::session::SessionState>()
            .ok_or_else(|| "session state unavailable".to_string())?;
        Ok(DumpState {
            current_room: state.current_room_name(),
            session_shared_window_ids: state.active_share_ids(),
            hover_shared_window_ids: crate::hover_tab::autotest_ui_shared_window_ids(),
            accessibility_trusted: accessibility_trusted(),
        })
    }

    fn current_room(app: &tauri::AppHandle) -> Result<serde_json::Value, String> {
        let state = app
            .try_state::<crate::session::SessionState>()
            .ok_or_else(|| "session state unavailable".to_string())?;
        let record = state
            .current_room_record()
            .ok_or_else(|| "not currently joined to a room".to_string())?;
        let credential = crate::rooms::normalize_room_credential(&record.name)
            .or_else(|| crate::rooms::normalize_room_credential(&record.slug))
            .ok_or_else(|| {
                format!(
                    "current room '{}' does not contain a full credential",
                    record.name
                )
            })?;
        Ok(serde_json::json!({
            "name": record.name,
            "credential": credential,
            "accessCode": record.access_code,
            "livekitRoom": crate::rooms::livekit_room_name(&record)
        }))
    }

    fn json_response(result: Result<serde_json::Value, String>) -> String {
        match result {
            Ok(value) => serde_json::to_string(&Response {
                ok: true,
                result: Some(value),
                error: None,
            })
            .unwrap(),
            Err(error) => serde_json::to_string(&Response::<serde_json::Value> {
                ok: false,
                result: None,
                error: Some(error),
            })
            .unwrap(),
        }
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }

    fn accessibility_trusted() -> bool {
        unsafe { AXIsProcessTrusted() }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn dump_metrics_command_parses_with_and_without_tail() {
            let cmd: Command = serde_json::from_str(r#"{"cmd":"dump_metrics"}"#).unwrap();
            assert!(matches!(cmd, Command::DumpMetrics { journal_tail: None }));

            let cmd: Command =
                serde_json::from_str(r#"{"cmd":"dump_metrics","journal_tail":5}"#).unwrap();
            assert!(matches!(
                cmd,
                Command::DumpMetrics {
                    journal_tail: Some(5)
                }
            ));
        }

        #[test]
        fn reconnect_command_selects_only_the_two_sdk_paths() {
            let resume: Command =
                serde_json::from_str(r#"{"cmd":"reconnect","mode":"resume"}"#).unwrap();
            assert!(matches!(
                resume,
                Command::Reconnect {
                    mode: ReconnectMode::Resume
                }
            ));

            let full: Command =
                serde_json::from_str(r#"{"cmd":"reconnect","mode":"full"}"#).unwrap();
            assert!(matches!(
                full,
                Command::Reconnect {
                    mode: ReconnectMode::Full
                }
            ));

            assert!(serde_json::from_str::<Command>(
                r#"{"cmd":"reconnect","mode":"network_drop"}"#
            )
            .is_err());
            assert_eq!(ReconnectMode::Resume.label(), "resume");
            assert_eq!(ReconnectMode::Full.label(), "full");
            assert!(matches!(
                ReconnectMode::Resume.scenario(),
                livekit::SimulateScenario::SignalReconnect
            ));
            assert!(matches!(
                ReconnectMode::Full.scenario(),
                livekit::SimulateScenario::FullReconnect
            ));
        }

        #[test]
        fn remote_control_lifecycle_commands_parse_with_hyphenated_names() {
            let disable: Command =
                serde_json::from_str(r#"{"cmd":"remote-control-disable","window_id":17}"#)
                    .unwrap();
            assert!(matches!(disable, Command::RemoteControlDisable { window_id: 17 }));

            let status: Command =
                serde_json::from_str(r#"{"cmd":"remote-control-status","window_id":17}"#)
                    .unwrap();
            assert!(matches!(status, Command::RemoteControlStatus { window_id: Some(17) }));
        }

        #[test]
        fn share_border_stack_command_is_explicit_and_read_only() {
            let command: Command =
                serde_json::from_str(r#"{"cmd":"share_border_stack","window_id":4242}"#).unwrap();
            assert!(matches!(
                command,
                Command::ShareBorderStack { window_id: 4242 }
            ));
        }

        #[test]
        fn reconnect_request_gate_allows_exactly_one_request_per_process() {
            let gate = OneShotReconnect::new();
            assert!(gate.claim().is_ok());
            let error = gate.claim().unwrap_err();
            assert!(error.contains("already requested"), "unexpected: {error}");
        }

        #[test]
        fn dump_metrics_value_shapes_network_and_tails_journal() {
            let network = crate::diagnostics::NetworkSnapshot::default();
            let journal: Vec<crate::diagnostics::JournalEntry> = (0..10)
                .map(|i| crate::diagnostics::JournalEntry {
                    t_ms: i,
                    category: "connection".to_string(),
                    message: format!("entry {i}"),
                })
                .collect();

            let value = dump_metrics_value(network, journal, Some(3));
            let tail = value["journalTail"].as_array().expect("journalTail array");
            assert_eq!(tail.len(), 3);
            assert_eq!(tail[0]["message"], "entry 7");
            assert_eq!(tail[2]["message"], "entry 9");
            assert!(value.get("network").is_some());
        }

        #[test]
        fn dump_metrics_value_tail_never_exceeds_available_entries() {
            let network = crate::diagnostics::NetworkSnapshot::default();
            let journal = vec![crate::diagnostics::JournalEntry {
                t_ms: 1,
                category: "connection".to_string(),
                message: "only entry".to_string(),
            }];

            let value = dump_metrics_value(network, journal, Some(50));
            let tail = value["journalTail"].as_array().expect("journalTail array");
            assert_eq!(tail.len(), 1);
        }
    }
}

#[cfg(target_os = "macos")]
fn start_command_server(app: tauri::AppHandle, path: String) {
    command_server::start(app, path);
}
