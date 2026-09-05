//! Test Cockpit privileged-command scaffolding (GitHub #253, Phase -1).
//!
//! Compiled ONLY when the `cockpit-privileged` Cargo feature is enabled (see
//! `Cargo.toml`) -- a standard customer-distributed build never links this
//! module, so there is no compiled code path to any privileged test-cockpit
//! capability (window-pixel capture, AX-based input injection, network
//! impairment) in a release build, not merely a runtime-disabled one. Only
//! the separate internal/QA build channel enables the feature.
//!
//! This issue only establishes the scaffolding: the feature flag (see
//! `Cargo.toml`), this stub module, and the reusable
//! `preflight_or_refuse(&AppHandle)` helper. Later phases (#255 pixel capture, #257
//! engine, #261 network impairment, #262 native-native) call
//! `preflight_or_refuse(&AppHandle)` before doing anything privileged, and replace
//! `privileged_commands_available()`'s placeholder with real per-capability
//! checks.
//!
//! ## The preflight-and-refuse contract
//!
//! Per docs/TESTING.md's "Test Cockpit" section: every privileged entry
//! point preflights required grants via non-prompting APIs FIRST and
//! refuses immediately on any miss with `INFRA-FAIL: run
//! scripts/cockpit-setup.sh`. It never calls a prompting code path (e.g.
//! `remote_control.rs`'s `prompt_accessibility()`) from inside a cockpit
//! run -- a run must never be interrupted or derailed by a permission
//! dialog or sudo prompt.

use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "macos")]
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader},
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    sync::oneshot,
};

use futures::StreamExt;
use livekit::prelude::RemoteTrack;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::dev_test_pattern::{TEST_PATTERN_SOURCE_HEIGHT, TEST_PATTERN_SOURCE_WIDTH};
use crate::test_cockpit_bridge::{CaptureWindowPixelsResult, PixelRect};

// Native Test Client (test-peer) support for SHARE-01 / SHARE-N2N: the
// test-peer build constants + the unit-tested window-geometry move oracle.
mod native_peer;

// P-3 gap journeys (SHARE-05/06/10, CAM-03/04, ROOM-01, PTR-02, UI-01..04):
// the unit-tested pure-logic pass-criteria oracles their live verdicts will
// use. The runnable scenarios below preflight and return INFRA-FAIL until the
// live orchestration is auto-driven; the oracle logic is already covered.
mod conclusions;
mod gap_oracles;

// RC-N2N / RC-N2W (journey RC-07, #819): Petal as the CONTROLLER. Holds the
// keystone gesture plan, the script that drives the REAL compositor/control
// route, and the pass/fail oracle over what a run observed.
mod rc_n2n;

/// Name of the one-time local marker file `scripts/cockpit-setup.sh` writes
/// (under the app's data directory) after every required grant -- Screen
/// Recording + Accessibility for both `target/debug/desktop` and the
/// test-peer binary, one-time Automation/AppleEvent consent, and the
/// manually-installed net-impair sudoers entry -- has been confirmed via
/// non-prompting checks.
///
/// Presence of this file is a necessary but not sufficient signal: it means
/// setup was *run and passed at some point in the past*, not that every
/// individual grant still holds right now (a grant could be revoked in
/// System Settings after setup ran). Callers that need a specific grant
/// (e.g. Screen Recording) should still preflight that grant directly (e.g.
/// `window_source::has_screen_recording_access()`) in addition to this
/// marker, exactly as `preflight_or_refuse` alone is not the complete
/// contract -- it is the first, cheapest check every privileged path must
/// pass before anything else.
const SETUP_MARKER_FILE: &str = ".cockpit-setup-complete";
const TEST_PROGRESS_EVENT: &str = "test-progress";
const TEST_CASE_ARG: &str = "--test-case";
const TEST_CASE_ENV: &str = "PETAL_TEST_CASE";
const FPS_THRESHOLD: f64 = 20.0;
/// Keep the test pattern frontmost while macOS establishes capture. The UI
/// tells an operator exactly what is happening; it never falsely promises
/// that moving focus away is safe while WebKit/SCK are focus-sensitive (#313).
const NATIVE_TEST_PATTERN_PREPARE_SECS: u8 = 5;
const NATIVE_TEST_PATTERN_READINESS_TIMEOUT: Duration = Duration::from_secs(3);
const NATIVE_TEST_PATTERN_READINESS_POLL: Duration = Duration::from_millis(100);
const NATIVE_TEST_PATTERN_REASSERT_INTERVAL: Duration = Duration::from_millis(300);
#[cfg(target_os = "macos")]
static COCKPIT_ACTIVATION_GENERATION: AtomicU64 = AtomicU64::new(0);
const ACTIVATION_QUEUED: u8 = 0;
const ACTIVATION_STARTED: u8 = 1;
const ACTIVATION_CANCELLED: u8 = 2;
const ACTIVATION_COMPLETED: u8 = 3;

#[cfg(target_os = "macos")]
fn appkit_dispatch_is_direct(caller_main: bool) -> bool {
    caller_main
}
#[cfg(target_os = "macos")]
fn queued_activation_try_start(queued: u64, current: u64, state: &AtomicU8) -> bool {
    queued == current
        && state
            .compare_exchange(
                ACTIVATION_QUEUED,
                ACTIVATION_STARTED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
}

#[cfg(target_os = "macos")]
fn queued_activation_cancel(state: &AtomicU8) -> bool {
    state
        .compare_exchange(
            ACTIVATION_QUEUED,
            ACTIVATION_CANCELLED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
}
const SHARER_FRAME_SAMPLE_ATTEMPTS: u8 = 4;
const SHARER_FRAME_SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
const COCKPIT_FRONTEND_PROVENANCE: &str = env!("PETAL_COCKPIT_FRONTEND_PROVENANCE");

fn verified_cockpit_frontend_provenance() -> Result<&'static str, String> {
    let provenance = COCKPIT_FRONTEND_PROVENANCE;
    if provenance == "unverified"
        || !provenance.contains("dev/test-pattern.html=")
        || !provenance.contains("dev/test-pattern-status.html=")
    {
        return Err(
            "INFRA-FAIL QA binary has no verified generated test-pattern/status asset provenance; rebuild via scripts/build-cockpit-primary.sh"
                .to_string(),
        );
    }
    Ok(provenance)
}

/// QA-only registration for the deterministic app-owned share source. It is
/// intentionally scoped to the cockpit feature module: production handback
/// behavior must never be changed for normal shares.
static COCKPIT_VISIBLE_SOURCE_IDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

pub(crate) fn cockpit_source_requires_visible_handback(window_id: u32) -> bool {
    COCKPIT_VISIBLE_SOURCE_IDS
        .get()
        .is_some_and(|ids| ids.lock().is_ok_and(|ids| ids.contains(&window_id)))
}

/// Owns a one-scenario exception to normal focus handback. Dropping this lease
/// removes the ID on every success, early return, cancellation, or error path;
/// a recycled CGWindowID therefore never inherits cockpit behavior.
struct CockpitVisibleSourceLease(u32);

impl Drop for CockpitVisibleSourceLease {
    fn drop(&mut self) {
        if let Some(ids) = COCKPIT_VISIBLE_SOURCE_IDS.get() {
            match ids.lock() {
                Ok(mut ids) => {
                    ids.remove(&self.0);
                }
                Err(_) => log::warn!(
                    "test-cockpit: visible-source removal lock poisoned; refusing to retain a QA handback exception"
                ),
            }
        }
    }
}

fn register_cockpit_visible_source(window_id: u32) -> CockpitVisibleSourceLease {
    let ids = COCKPIT_VISIBLE_SOURCE_IDS.get_or_init(|| Mutex::new(HashSet::new()));
    match ids.lock() {
        Ok(mut ids) => {
            ids.insert(window_id);
        }
        Err(_) => log::warn!(
            "test-cockpit: visible-source registration lock poisoned; generic handback will apply"
        ),
    }
    CockpitVisibleSourceLease(window_id)
}

struct NativeTestPatternShare {
    window_id: u32,
    _visible_source: CockpitVisibleSourceLease,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NativeTestPatternReadiness {
    registered_source: bool,
    cg_visible: bool,
    app_active: bool,
    window_key: bool,
    window_visible: bool,
    regular_policy: bool,
    policy_change_accepted: bool,
    can_become_key: bool,
    activation_requested: bool,
    activation_accepted: bool,
    ns_app_activate_requested: bool,
    legacy_activate_requested: bool,
    activation_caller_main: bool,
    activation_queue_latency_ms: u64,
    geometry_matches: bool,
    advancing_reports: u8,
    counter_delta: u64,
    liveness_fresh: bool,
    post_activation_report: bool,
}

#[cfg(target_os = "macos")]
impl NativeTestPatternReadiness {
    fn ready(self) -> bool {
        self.registered_source
            && self.cg_visible
            && self.app_active
            && self.window_key
            && self.window_visible
            && self.regular_policy
            && self.can_become_key
            && self.geometry_matches
            && self.advancing_reports >= 2
            && self.counter_delta > 0
            && self.liveness_fresh
            && self.post_activation_report
    }

    fn failure_code(self) -> &'static str {
        if !self.can_become_key {
            "cockpit-source-not-keyable"
        } else if !self.geometry_matches {
            "cockpit-source-geometry-drift"
        } else {
            "cockpit-source-not-active-or-drawing"
        }
    }
}

#[cfg(target_os = "macos")]
fn cockpit_activation_reassert_due(
    app_active: bool,
    window_key: bool,
    elapsed_since_last_reassert: Duration,
    remaining_budget: Duration,
) -> bool {
    (!app_active || !window_key)
        && elapsed_since_last_reassert >= NATIVE_TEST_PATTERN_REASSERT_INTERVAL
        && !remaining_budget.is_zero()
}

#[cfg(target_os = "macos")]
fn capped_readiness_sleep(deadline: Instant, requested: Duration) -> Duration {
    requested.min(deadline.saturating_duration_since(Instant::now()))
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, Eq)]
enum AppKitDispatchError {
    SourceMissing,
    ScheduleFailed(String),
    TimedOut,
    Superseded,
    ReceiverClosed,
    ExecutionFailed(String),
}

#[cfg(target_os = "macos")]
impl AppKitDispatchError {
    fn code(&self) -> &'static str {
        match self {
            Self::SourceMissing => "cockpit-main-thread-source-missing",
            Self::ScheduleFailed(_) => "cockpit-main-thread-dispatch-schedule-failed",
            Self::TimedOut => "cockpit-main-thread-dispatch-timeout",
            Self::Superseded => "cockpit-main-thread-dispatch-superseded",
            Self::ReceiverClosed => "cockpit-main-thread-dispatch-receiver-closed",
            Self::ExecutionFailed(_) => "cockpit-main-thread-dispatch-execution-failed",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::SourceMissing => "test-pattern source window missing",
            Self::ScheduleFailed(detail) | Self::ExecutionFailed(detail) => detail,
            Self::TimedOut => "absolute readiness deadline expired before dispatch completion",
            Self::Superseded => "a newer activation generation superseded queued work",
            Self::ReceiverClosed => "main-thread dispatch result channel closed",
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
struct CockpitActivationDispatch {
    observed: crate::platform::appkit::CockpitSourceActivation,
    caller_main: bool,
    queue_latency_ms: u64,
}

#[cfg(target_os = "macos")]
fn dispatch_schedule_result<T: std::fmt::Display>(
    result: Result<(), T>,
) -> Result<(), AppKitDispatchError> {
    result.map_err(|error| AppKitDispatchError::ScheduleFailed(error.to_string()))
}

#[cfg(target_os = "macos")]
async fn appkit_test_pattern_readiness(
    app: &AppHandle,
    deadline: Instant,
) -> Result<crate::platform::appkit::WindowReadiness, AppKitDispatchError> {
    let window = app
        .get_webview_window(crate::dev_test_pattern::TEST_PATTERN_DEV_LABEL)
        .ok_or(AppKitDispatchError::SourceMissing)?;
    if appkit_dispatch_is_direct(objc2::MainThreadMarker::new().is_some()) {
        return crate::platform::appkit::window_readiness(&window)
            .map_err(AppKitDispatchError::ExecutionFailed);
    }
    let (sender, receiver) = oneshot::channel();
    dispatch_schedule_result(app.run_on_main_thread(move || {
        let _ = sender.send(crate::platform::appkit::window_readiness(&window));
    }))?;
    tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), receiver)
        .await
        .map_err(|_| AppKitDispatchError::TimedOut)?
        .map_err(|_| AppKitDispatchError::ReceiverClosed)?
        .map_err(AppKitDispatchError::ExecutionFailed)
}

#[cfg(target_os = "macos")]
async fn activate_test_pattern_window(
    app: &AppHandle,
    deadline: Instant,
) -> Result<CockpitActivationDispatch, AppKitDispatchError> {
    let window = app
        .get_webview_window(crate::dev_test_pattern::TEST_PATTERN_DEV_LABEL)
        .ok_or(AppKitDispatchError::SourceMissing)?;
    let caller_main = appkit_dispatch_is_direct(objc2::MainThreadMarker::new().is_some());
    let queued_at = Instant::now();
    if caller_main {
        // Invalidate any older event-loop task before mutating directly.
        COCKPIT_ACTIVATION_GENERATION.fetch_add(1, Ordering::SeqCst);
        let observed = crate::platform::appkit::activate_cockpit_source_window(
            &window,
            TEST_PATTERN_SOURCE_WIDTH,
            TEST_PATTERN_SOURCE_HEIGHT,
        )
        .map_err(AppKitDispatchError::ExecutionFailed)?;
        return Ok(CockpitActivationDispatch {
            observed,
            caller_main: true,
            queue_latency_ms: 0,
        });
    }
    let generation = COCKPIT_ACTIVATION_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let state = Arc::new(AtomicU8::new(ACTIVATION_QUEUED));
    let state_for_main = state.clone();
    let (sender, receiver) = oneshot::channel();
    dispatch_schedule_result(app.run_on_main_thread(move || {
        if !queued_activation_try_start(
            generation,
            COCKPIT_ACTIVATION_GENERATION.load(Ordering::SeqCst),
            &state_for_main,
        ) {
            let _ = sender.send(Err(AppKitDispatchError::Superseded));
            return;
        }
        let result = crate::platform::appkit::activate_cockpit_source_window(
            &window,
            TEST_PATTERN_SOURCE_WIDTH,
            TEST_PATTERN_SOURCE_HEIGHT,
        )
        .map_err(AppKitDispatchError::ExecutionFailed);
        state_for_main.store(ACTIVATION_COMPLETED, Ordering::SeqCst);
        let _ = sender.send(result);
    }))?;
    match tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), receiver).await {
        Ok(Ok(Ok(observed))) => Ok(CockpitActivationDispatch {
            observed,
            caller_main: false,
            queue_latency_ms: queued_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        }),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(_)) => Err(AppKitDispatchError::ReceiverClosed),
        Err(_) => {
            let _ = queued_activation_cancel(&state);
            Err(AppKitDispatchError::TimedOut)
        }
    }
}

#[cfg(target_os = "macos")]
fn record_appkit_dispatch_failure(
    app: &AppHandle,
    writer: &mut ResultsWriter,
    scenario: ScenarioSpec,
    error: AppKitDispatchError,
) -> String {
    let failure_code = error.code();
    log::warn!(
        "test-cockpit: AppKit dispatch failed code={failure_code} detail={}",
        error.detail()
    );
    let _ = crate::dev_test_pattern::set_cockpit_test_pattern_phase(
        app.clone(),
        crate::dev_test_pattern::CockpitTestPatternPhase::Failed {
            detail: failure_code,
        },
    );
    let _ = writer.write(
        "native-test-pattern-readiness",
        Some(scenario.id),
        serde_json::json!({
            "ready": false,
            "failureCode": failure_code,
            "dispatchDetail": error.detail(),
            "dispatchTimedOut": error == AppKitDispatchError::TimedOut,
            "dispatchSuperseded": error == AppKitDispatchError::Superseded,
            "dispatchReceiverClosed": error == AppKitDispatchError::ReceiverClosed,
            "dispatchScheduleFailed": matches!(&error, AppKitDispatchError::ScheduleFailed(_)),
            "dispatchExecutionFailed": matches!(&error, AppKitDispatchError::ExecutionFailed(_)),
            "sourceMissing": error == AppKitDispatchError::SourceMissing,
        }),
    );
    format!("INFRA-FAIL {failure_code}")
}

#[cfg(target_os = "macos")]
fn activate_then_sample_liveness_sequence<A, T>(
    activate: impl FnOnce() -> Result<A, String>,
    sample: impl FnOnce() -> T,
) -> Result<(A, T), String> {
    let activation = activate()?;
    Ok((activation, sample()))
}

#[cfg(target_os = "macos")]
async fn await_native_test_pattern_readiness(
    app: &AppHandle,
    writer: &mut ResultsWriter,
    scenario: ScenarioSpec,
    window_id: u32,
    mut report_sequence_after_activation: u64,
    mut activation: CockpitActivationDispatch,
    deadline: Instant,
) -> Result<NativeTestPatternReadiness, String> {
    let mut last = NativeTestPatternReadiness::default();
    let mut last_reassert = Instant::now();
    loop {
        let mut appkit = appkit_test_pattern_readiness(app, deadline)
            .await
            .map_err(|error| record_appkit_dispatch_failure(app, writer, scenario, error))?;
        if cockpit_activation_reassert_due(
            appkit.app_active,
            appkit.window_key,
            last_reassert.elapsed(),
            deadline.saturating_duration_since(Instant::now()),
        ) {
            activation = activate_test_pattern_window(app, deadline)
                .await
                .map_err(|error| record_appkit_dispatch_failure(app, writer, scenario, error))?;
            report_sequence_after_activation =
                crate::dev_test_pattern::test_pattern_liveness_snapshot().report_sequence;
            appkit = appkit_test_pattern_readiness(app, deadline)
                .await
                .map_err(|error| record_appkit_dispatch_failure(app, writer, scenario, error))?;
            last_reassert = Instant::now();
        }
        let liveness = crate::dev_test_pattern::test_pattern_liveness_snapshot();
        last = NativeTestPatternReadiness {
            registered_source: cockpit_source_requires_visible_handback(window_id),
            cg_visible: native_peer::window_frame(window_id).is_some()
                && native_peer::window_exists_in_all_windows(window_id),
            app_active: appkit.app_active,
            window_key: appkit.window_key,
            window_visible: appkit.window_visible,
            regular_policy: activation.observed.regular_policy,
            policy_change_accepted: activation.observed.policy_change_accepted,
            can_become_key: activation.observed.can_become_key,
            activation_requested: activation.observed.activation_requested,
            activation_accepted: activation.observed.activation_accepted,
            ns_app_activate_requested: activation.observed.ns_app_activate_requested,
            legacy_activate_requested: activation.observed.legacy_activate_requested,
            activation_caller_main: activation.caller_main,
            activation_queue_latency_ms: activation.queue_latency_ms,
            geometry_matches: activation.observed.geometry_matches,
            advancing_reports: liveness.advancing_reports,
            counter_delta: liveness.counter_delta,
            liveness_fresh: liveness.fresh,
            post_activation_report: liveness.report_sequence > report_sequence_after_activation,
        };
        if last.ready() {
            let _ = writer.write(
                "native-test-pattern-readiness",
                Some(scenario.id),
                serde_json::json!({
                    "ready": true,
                    "generation": liveness.generation,
                    "registeredSource": last.registered_source,
                    "cgVisible": last.cg_visible,
                    "appActive": last.app_active,
                    "windowKey": last.window_key,
                    "windowVisible": last.window_visible,
                    "regularPolicy": last.regular_policy,
                    "policyChangeAccepted": last.policy_change_accepted,
                    "canBecomeKey": last.can_become_key,
                    "activationRequested": last.activation_requested,
                    "activationAccepted": last.activation_accepted,
                    "nsAppActivateRequested": last.ns_app_activate_requested,
                    "legacyActivateRequested": last.legacy_activate_requested,
                    "activationCallerMain": last.activation_caller_main,
                    "activationQueueLatencyMs": last.activation_queue_latency_ms,
                    "geometryMatches": last.geometry_matches,
                    "advancingReports": last.advancing_reports,
                    "counterDelta": last.counter_delta,
                    "reportSequence": liveness.report_sequence,
                    "postActivationReport": last.post_activation_report,
                }),
            );
            return Ok(last);
        }
        if Instant::now() >= deadline {
            break;
        }
        let sleep_for = capped_readiness_sleep(deadline, NATIVE_TEST_PATTERN_READINESS_POLL);
        if !sleep_for.is_zero() {
            tokio::time::sleep(sleep_for).await;
        }
    }
    let failure_code = last.failure_code();
    let _ = crate::dev_test_pattern::set_cockpit_test_pattern_phase(
        app.clone(),
        crate::dev_test_pattern::CockpitTestPatternPhase::Failed {
            detail: failure_code,
        },
    );
    let _ = writer.write(
        "native-test-pattern-readiness",
        Some(scenario.id),
        serde_json::json!({
            "ready": false,
            "failureCode": last.failure_code(),
            "registeredSource": last.registered_source,
            "cgVisible": last.cg_visible,
            "appActive": last.app_active,
            "windowKey": last.window_key,
            "windowVisible": last.window_visible,
            "regularPolicy": last.regular_policy,
            "policyChangeAccepted": last.policy_change_accepted,
            "canBecomeKey": last.can_become_key,
            "activationRequested": last.activation_requested,
            "activationAccepted": last.activation_accepted,
            "nsAppActivateRequested": last.ns_app_activate_requested,
            "legacyActivateRequested": last.legacy_activate_requested,
            "activationCallerMain": last.activation_caller_main,
            "activationQueueLatencyMs": last.activation_queue_latency_ms,
            "geometryMatches": last.geometry_matches,
            "advancingReports": last.advancing_reports,
            "counterDelta": last.counter_delta,
            "livenessFresh": last.liveness_fresh,
            "postActivationReport": last.post_activation_report,
        }),
    );
    Err(format!("INFRA-FAIL {failure_code}"))
}

#[cfg(target_os = "macos")]
async fn ensure_native_test_pattern_readiness(
    app: &AppHandle,
    writer: &mut ResultsWriter,
    scenario: ScenarioSpec,
    window_id: u32,
) -> Result<NativeTestPatternReadiness, String> {
    // A failed direct-window start can hand focus away. Re-activate on the
    // AppKit main thread, then require a fresh bounded readiness observation
    // before this attempt is allowed to touch the share toggle.
    let readiness_deadline = Instant::now() + NATIVE_TEST_PATTERN_READINESS_TIMEOUT;
    let (activation, report_sequence_after_activation) =
        match activate_test_pattern_window(app, readiness_deadline).await {
            Ok(observed) => (
                observed,
                crate::dev_test_pattern::test_pattern_liveness_snapshot().report_sequence,
            ),
            Err(error) => {
                return Err(record_appkit_dispatch_failure(app, writer, scenario, error));
            }
        };
    let sleep_for = capped_readiness_sleep(readiness_deadline, Duration::from_millis(100));
    if !sleep_for.is_zero() {
        tokio::time::sleep(sleep_for).await;
    }
    await_native_test_pattern_readiness(
        app,
        writer,
        scenario,
        window_id,
        report_sequence_after_activation,
        activation,
        readiness_deadline,
    )
    .await
}

#[cfg(target_os = "macos")]
fn toggle_after_native_test_pattern_readiness<T>(
    readiness: NativeTestPatternReadiness,
    toggle: impl FnOnce() -> T,
) -> Result<T, String> {
    readiness
        .ready()
        .then(toggle)
        .ok_or_else(|| "INFRA-FAIL cockpit-source-not-active-or-drawing".to_string())
}

#[cfg(target_os = "macos")]
fn appkit_test_pattern_frame(app: &AppHandle) -> Option<crate::platform::cg::WindowFrame> {
    let window = app.get_webview_window(crate::dev_test_pattern::TEST_PATTERN_DEV_LABEL)?;
    let position = window.outer_position().ok()?;
    let size = window.outer_size().ok()?;
    Some(crate::platform::cg::WindowFrame {
        x: position.x,
        y: position.y,
        width: size.width as i32,
        height: size.height as i32,
    })
}

#[cfg(target_os = "macos")]
fn sharer_frame_diagnostic(
    app: &AppHandle,
    window_id: u32,
    attempt: u8,
) -> native_peer::SharerFrameSample {
    native_peer::SharerFrameSample {
        attempt,
        on_screen_frame: native_peer::window_frame(window_id),
        exists_in_all_windows: native_peer::window_exists_in_all_windows(window_id),
        owner_pid: crate::platform::cg::owner_pid_for_window_id(window_id),
        frontmost_app: crate::platform::appkit::frontmost_app_label(),
        petal_active: crate::platform::appkit::app_is_active(),
        appkit_frame: appkit_test_pattern_frame(app),
    }
}

#[cfg(target_os = "macos")]
async fn sample_fresh_sharer_frame(
    app: &AppHandle,
    window_id: u32,
    writer: &mut ResultsWriter,
    scenario: ScenarioSpec,
    phase: &str,
) -> native_peer::SharerFrameSample {
    let _ = writer.write("native-peer-sharer-sampler-start", Some(scenario.id), serde_json::json!({"phase":phase,"sourceWindowId":window_id,"maxAttempts":SHARER_FRAME_SAMPLE_ATTEMPTS}));
    let mut samples = Vec::with_capacity(SHARER_FRAME_SAMPLE_ATTEMPTS as usize);
    for attempt in 1..=SHARER_FRAME_SAMPLE_ATTEMPTS {
        let sample = sharer_frame_diagnostic(app, window_id, attempt);
        let captured = sample.on_screen_frame.is_some();
        let _ = writer.write(
            "native-peer-sharer-sampler-attempt",
            Some(scenario.id),
            serde_json::json!({"phase":phase,"sourceWindowId":window_id,"sample":sample}),
        );
        samples.push(sample);
        if captured {
            break;
        }
        if attempt < SHARER_FRAME_SAMPLE_ATTEMPTS {
            tokio::time::sleep(SHARER_FRAME_SAMPLE_INTERVAL).await;
        }
    }
    let classification = native_peer::sharer_sample_classification(&samples);
    let selected = native_peer::first_fresh_on_screen_sample(samples);
    let _ = writer.write("native-peer-sharer-sampler-complete", Some(scenario.id), serde_json::json!({"phase":phase,"sourceWindowId":window_id,"classification":classification,"selected":selected}));
    selected
}

fn unix_epoch_ms() -> Result<u128, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| format!("INFRA-FAIL system clock precedes Unix epoch: {error}"))
}

fn capture_lock_phase_after_share(
    shared: bool,
) -> Option<crate::dev_test_pattern::CockpitTestPatternPhase> {
    shared.then_some(crate::dev_test_pattern::CockpitTestPatternPhase::CaptureLocked)
}

fn cockpit_cancel_requested(app: &AppHandle) -> bool {
    app.try_state::<CockpitRuntimeState>()
        .is_some_and(|state| state.cancel_requested.load(Ordering::SeqCst))
}

async fn await_test_pattern_prepare_or_cancel(
    app: &AppHandle,
    scenario: ScenarioSpec,
    writer: &mut ResultsWriter,
) -> Result<(), String> {
    for _ in 0..NATIVE_TEST_PATTERN_PREPARE_SECS * 4 {
        if cockpit_cancel_requested(app) {
            let detail = "cancelled-during-prepare";
            let _ = crate::dev_test_pattern::set_cockpit_test_pattern_phase(
                app.clone(),
                crate::dev_test_pattern::CockpitTestPatternPhase::Failed { detail },
            );
            let _ = writer.write(
                "native-test-pattern-phase",
                Some(scenario.id),
                serde_json::json!({ "phase": "FAILED", "detail": detail }),
            );
            return Err("CANCELLED native test-pattern preparation".to_string());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}
// SHARE-N2W-Q shares Petal's OWN WKWebView test-pattern window. macOS throttles
// an app self-capturing its own WebView -- both the WebView's JS timers/rendering
// (worse when Petal isn't the frontmost app, as in CLI-driven cockpit runs) and
// ScreenCaptureKit's raw stream, which stops re-fetching the WebView's GPU layer.
// Petal correctly falls back to <=10fps snapshot-pull (#183), so the receiver
// gets the share at the correct source resolution with LIVE (advancing) frames
// but well below 30fps. This is a self-capture/environment artifact, NOT a
// capture defect for real third-party app windows (which dirty their surface
// normally and stream at full fps -- SHARE-W2N-Q proves the 30fps media path in
// reverse). N2W therefore gates on delivered LIVENESS (received at source
// dimensions, frames advancing), not a raw-window fps this synthetic self-capture
// source structurally cannot reach.
const N2W_LIVENESS_FPS: f64 = 0.0;
const REQUIRED_GOOD_READS: u32 = 2;
const ASSERT_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TOKEN_REQUEST_INTERVAL: Duration = Duration::from_secs(3);
const WEB_REPORT_TIMEOUT: Duration = Duration::from_secs(45);
const NET_IMPAIR_SCRIPT_RELATIVE_PATH: &str = "scripts/net-impair.sh";
const ARTIFACT_RETENTION_DAYS_ENV: &str = "PETAL_COCKPIT_ARTIFACT_RETENTION_DAYS";
const ARTIFACT_RETENTION_RUNS_ENV: &str = "PETAL_COCKPIT_ARTIFACT_RETENTION_RUNS";
const DEFAULT_ARTIFACT_RETENTION_DAYS: u64 = 14;
const DEFAULT_ARTIFACT_RETENTION_RUNS_PER_SCENARIO: usize = 20;
const AUDIO_ARTIFACT_SAMPLE_RATE: u32 = 48_000;
const AUDIO_ARTIFACT_CHANNELS: u32 = 1;
const AUDIO_ARTIFACT_SECONDS: u32 = 3;
const AFCONVERT_PATH: &str = "/usr/bin/afconvert";
/// Internal selector accepted only by the separately-built `test-peer` binary.
/// It is not a user-facing cockpit case and never appears in the scenario table.
const NATIVE_PEER_RECEIVER_SELECTOR: &str = "__petal-native-peer-receiver";
const NATIVE_PEER_SOCKET_ENV: &str = "PETAL_COCKPIT_NATIVE_PEER_SOCKET";
const NATIVE_PEER_TOKEN_ENV: &str = "PETAL_COCKPIT_NATIVE_PEER_TOKEN";
/// Carries the parent's real ACCESS CODE, not the bare internal room
/// credential -- see the doc comment on `run_native_to_native_scenario`'s
/// `room_name` binding for why.
const NATIVE_PEER_ROOM_ENV: &str = "PETAL_COCKPIT_NATIVE_PEER_ROOM";
const NATIVE_PEER_OWNER_ENV: &str = "PETAL_COCKPIT_NATIVE_PEER_OWNER";
const NATIVE_PEER_WINDOW_ENV: &str = "PETAL_COCKPIT_NATIVE_PEER_WINDOW";
const NATIVE_PEER_IDENTITY_ENV: &str = "PETAL_COCKPIT_NATIVE_PEER_IDENTITY";
/// RC-N2N (#819): the SECOND test-peer role. Here the peer is the sharer and
/// remote-control HOST, and the primary is the controller -- the reverse of
/// `NATIVE_PEER_RECEIVER_SELECTOR`'s roles.
const NATIVE_PEER_CONTROL_HOST_SELECTOR: &str = "__petal-native-peer-control-host";
/// App name of the sacrificial target the peer shares and the controller
/// drives (TextEdit, same target the 30-case web suite uses).
const NATIVE_PEER_TARGET_APP_ENV: &str = "PETAL_COCKPIT_NATIVE_PEER_TARGET_APP";
/// Unique per-run marker in the sacrificial document's title. The peer matches
/// its share target on this substring and NEVER on an ordinal: stale documents
/// accumulate across runs, and macOS injects its own overlay window once
/// ScreenCaptureKit is capturing, so "window 1" silently resolves to a phantom
/// (a confirmed bug in the web suite -- see remote-control-scenario.mjs).
const NATIVE_PEER_TARGET_TITLE_ENV: &str = "PETAL_COCKPIT_NATIVE_PEER_TARGET_TITLE";
const NATIVE_PEER_TIMEOUT: Duration = Duration::from_secs(45);

/// Non-prompting capability probe for the already-landed privileged cockpit
/// paths. This deliberately checks live TCC state, not just the setup marker:
/// the marker proves setup once passed, while these calls prove grants have
/// not been revoked since.
#[allow(dead_code)]
pub fn privileged_commands_available() -> bool {
    crate::permissions::check_screen_recording() && crate::permissions::check_accessibility()
}

fn marker_path_under(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(SETUP_MARKER_FILE)
}

/// The non-negotiable preflight-and-refuse rule, as a reusable helper: every
/// later privileged test-cockpit entry point (capture_window_pixels, input
/// injection, network impairment) MUST call this first and propagate the
/// error immediately on any miss -- never fall through to a prompting API.
///
/// Returns `Ok(())` when the one-time setup marker is present for this exact
/// Tauri identity; otherwise a clear, greppable `INFRA-FAIL: run
/// scripts/cockpit-setup.sh` message. The test-peer has a distinct identifier
/// and app-data directory, so never substitute the primary's marker here.
pub fn preflight_or_refuse(app: &AppHandle) -> Result<(), String> {
    let app_data_dir = app.path().app_data_dir().map_err(|error| {
        format!("INFRA-FAIL: could not resolve cockpit app data directory: {error}")
    })?;
    preflight_or_refuse_under(&app_data_dir)?;
    if !crate::permissions::check_screen_recording() {
        return Err(
            "INFRA-FAIL: run scripts/cockpit-setup.sh (Screen Recording permission is not currently granted)"
                .to_string(),
        );
    }
    if !crate::permissions::check_accessibility() {
        return Err(
            "INFRA-FAIL: run scripts/cockpit-setup.sh (Accessibility permission is not currently granted)"
                .to_string(),
        );
    }
    Ok(())
}

fn preflight_or_refuse_under(app_data_dir: &Path) -> Result<(), String> {
    if !marker_path_under(app_data_dir).exists() {
        return Err("INFRA-FAIL: run scripts/cockpit-setup.sh".to_string());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    selector: String,
    source: LaunchSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LaunchSource {
    Arg,
    Env,
    SecondLaunch,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StartTestCockpitArgs {
    pub selector: String,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CockpitStatus {
    pub running: bool,
    pub run_id: Option<String>,
    pub selector: Option<String>,
    pub results_dir: Option<String>,
    pub summary: Option<CockpitSummary>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CockpitSummary {
    pub status: CockpitRunStatus,
    pub passed: u32,
    pub failed: u32,
    pub skipped: Vec<CockpitSkippedScenario>,
    pub message: String,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CockpitRunStatus {
    Passed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CockpitSkippedScenario {
    pub id: String,
    pub reason: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestProgressEvent {
    pub run_id: String,
    pub selector: String,
    pub phase: String,
    pub scenario_id: Option<String>,
    pub message: String,
    pub completed: u32,
    pub total: u32,
    pub skipped: Vec<CockpitSkippedScenario>,
    pub summary: Option<CockpitSummary>,
    pub results_dir: Option<String>,
}

#[derive(Default)]
pub struct CockpitRuntimeState {
    status: Mutex<CockpitStatus>,
    cancel_requested: AtomicBool,
}

impl Default for CockpitStatus {
    fn default() -> Self {
        Self {
            running: false,
            run_id: None,
            selector: None,
            results_dir: None,
            summary: None,
        }
    }
}

fn normalize_selector(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_test_case_arg<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let arg = arg.as_ref();
        if let Some(value) = arg.strip_prefix("--test-case=") {
            return normalize_selector(value);
        }
        if arg == TEST_CASE_ARG {
            if let Some(value) = iter.next() {
                return normalize_selector(value.as_ref());
            }
            return None;
        }
    }
    None
}

pub fn launch_spec_from_args_and_env<I, S>(args: I) -> Option<LaunchSpec>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    launch_spec_from_args_and_env_value(args, std::env::var(TEST_CASE_ENV).ok())
}

fn launch_spec_from_args_and_env_value<I, S>(
    args: I,
    env_value: Option<String>,
) -> Option<LaunchSpec>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if let Some(selector) = parse_test_case_arg(args) {
        return Some(LaunchSpec {
            selector,
            source: LaunchSource::Arg,
        });
    }
    env_value
        .and_then(|value| normalize_selector(&value))
        .map(|selector| LaunchSpec {
            selector,
            source: LaunchSource::Env,
        })
}

pub fn handle_second_launch(app: &AppHandle, argv: Vec<String>) -> bool {
    let Some(selector) = parse_test_case_arg(argv) else {
        return false;
    };
    run_launch_spec_no_exit(
        app.clone(),
        LaunchSpec {
            selector,
            source: LaunchSource::SecondLaunch,
        },
    );
    true
}

pub fn run_launch_spec_and_exit(app: AppHandle, spec: LaunchSpec) {
    tauri::async_runtime::spawn(async move {
        let exit_code = run_launch_spec(app.clone(), spec).await;
        app.exit(exit_code);
    });
}

fn run_launch_spec_no_exit(app: AppHandle, spec: LaunchSpec) {
    tauri::async_runtime::spawn(async move {
        let _ = run_launch_spec(app, spec).await;
    });
}

async fn run_launch_spec(app: AppHandle, spec: LaunchSpec) -> i32 {
    log::info!(
        "test-cockpit: launch-param trigger source={:?} selector={}",
        spec.source,
        spec.selector
    );
    if spec.selector == NATIVE_PEER_RECEIVER_SELECTOR {
        return match run_native_peer_receiver(app).await {
            Ok(()) => 0,
            Err(error) => {
                log::error!("test-cockpit: native peer receiver failed: {error}");
                eprintln!("PETAL_TEST_COCKPIT_ERROR={error}");
                1
            }
        };
    }
    if spec.selector == NATIVE_PEER_CONTROL_HOST_SELECTOR {
        return match run_native_peer_control_host(app).await {
            Ok(()) => 0,
            Err(error) => {
                log::error!("test-cockpit: native peer control host failed: {error}");
                eprintln!("PETAL_TEST_COCKPIT_ERROR={error}");
                1
            }
        };
    }
    match start_test_cockpit(
        app.clone(),
        app.state::<CockpitRuntimeState>(),
        StartTestCockpitArgs {
            selector: spec.selector,
        },
    )
    .await
    {
        Ok(status) => {
            if let Some(results_dir) = status.results_dir.as_deref() {
                log::info!("test-cockpit: results-dir={results_dir}");
                println!("PETAL_TEST_COCKPIT_RESULTS_DIR={results_dir}");
            }
            match status.summary.map(|summary| summary.status) {
                Some(CockpitRunStatus::Passed) => 0,
                _ => 1,
            }
        }
        Err(error) => {
            log::error!("test-cockpit: launch-param run failed: {error}");
            eprintln!("PETAL_TEST_COCKPIT_ERROR={error}");
            1
        }
    }
}

fn test_runs_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("Library/Logs/Petal/test-runs")
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TestCockpitRunSummary {
    pub run_id: String,
    pub results_dir: String,
    pub updated_at_unix_ms: u64,
    pub status: String,
    pub pass: u32,
    pub fail: u32,
    pub skipped: u32,
    pub parse_errors: u32,
    pub conclusion: Option<serde_json::Value>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TestCockpitRunDetail {
    pub summary: TestCockpitRunSummary,
    pub events: Vec<serde_json::Value>,
    pub artifacts: Vec<serde_json::Value>,
    pub scorecard: Option<serde_json::Value>,
    pub parse_errors: u32,
    pub conclusion: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RunCounts {
    pass: u32,
    fail: u32,
    skipped: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ParsedRunArtifacts {
    events: Vec<serde_json::Value>,
    artifacts: Vec<serde_json::Value>,
    scorecard: Option<serde_json::Value>,
    parse_errors: u32,
    counts: RunCounts,
    run_id: Option<String>,
    updated_at_unix_ms: Option<u64>,
    conclusion: Option<serde_json::Value>,
    incomplete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ArtifactRetentionConfig {
    max_age_days: u64,
    max_runs_per_scenario: usize,
}

#[derive(Clone, Debug, Default, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ArtifactRetentionReport {
    max_age_days: u64,
    max_runs_per_scenario: usize,
    scanned_runs: u32,
    pruned_files: u32,
    kept_files: u32,
    skipped_files: u32,
}

impl Default for ArtifactRetentionConfig {
    fn default() -> Self {
        Self {
            max_age_days: DEFAULT_ARTIFACT_RETENTION_DAYS,
            max_runs_per_scenario: DEFAULT_ARTIFACT_RETENTION_RUNS_PER_SCENARIO,
        }
    }
}

impl ArtifactRetentionConfig {
    fn from_env() -> Self {
        fn env_u64(name: &str, fallback: u64) -> u64 {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(fallback)
        }

        Self {
            max_age_days: env_u64(ARTIFACT_RETENTION_DAYS_ENV, DEFAULT_ARTIFACT_RETENTION_DAYS),
            max_runs_per_scenario: env_u64(
                ARTIFACT_RETENTION_RUNS_ENV,
                DEFAULT_ARTIFACT_RETENTION_RUNS_PER_SCENARIO as u64,
            )
            .try_into()
            .unwrap_or(DEFAULT_ARTIFACT_RETENTION_RUNS_PER_SCENARIO),
        }
    }
}

fn unix_ms_from_system_time(time: SystemTime) -> Option<u64> {
    let millis = time.duration_since(UNIX_EPOCH).ok()?.as_millis();
    Some(millis.try_into().unwrap_or(u64::MAX))
}

fn updated_at_unix_ms(path: &Path) -> u64 {
    let mut updated = path
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(unix_ms_from_system_time)
        .unwrap_or(0);
    for file_name in ["run.jsonl", "scorecard.json"] {
        if let Some(file_updated) = path
            .join(file_name)
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(unix_ms_from_system_time)
        {
            updated = updated.max(file_updated);
        }
    }
    updated
}

fn run_status(counts: RunCounts) -> String {
    if counts.fail > 0 {
        "failed".to_string()
    } else if counts.pass > 0 {
        "passed".to_string()
    } else if counts.skipped > 0 {
        "skipped".to_string()
    } else {
        "unknown".to_string()
    }
}

fn add_verdict(counts: &mut RunCounts, verdict: &str) -> bool {
    match verdict.trim().to_ascii_lowercase().as_str() {
        "pass" | "passed" | "ok" | "success" => {
            counts.pass += 1;
            true
        }
        "test-fail" | "infra-fail" | "fail" | "failed" | "failure" | "error" => {
            counts.fail += 1;
            true
        }
        "skipped" | "skip" | "cancelled" | "canceled" => {
            counts.skipped += 1;
            true
        }
        _ => false,
    }
}

fn number_field(value: &serde_json::Value, fields: &[&str]) -> Option<u32> {
    for field in fields {
        if let Some(number) = value.get(*field).and_then(serde_json::Value::as_u64) {
            return Some(number.try_into().unwrap_or(u32::MAX));
        }
        if let Some(number) = value.get(*field).and_then(serde_json::Value::as_i64) {
            if number >= 0 {
                return Some(number.try_into().unwrap_or(u32::MAX));
            }
        }
    }
    None
}

fn string_field<'a>(value: &'a serde_json::Value, fields: &[&str]) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|field| value.get(*field).and_then(serde_json::Value::as_str))
}

fn counts_from_scorecard(scorecard: &serde_json::Value) -> Option<RunCounts> {
    let summary = scorecard
        .get("summary")
        .filter(|value| value.is_object())
        .unwrap_or(scorecard);
    let pass = number_field(summary, &["passed", "pass"]);
    let fail = number_field(summary, &["failed", "fail"]);
    let skipped = number_field(summary, &["skipped", "skip"]);
    if let (Some(pass), Some(fail), Some(skipped)) = (pass, fail, skipped) {
        return Some(RunCounts {
            pass,
            fail,
            skipped,
        });
    }

    let mut counts = RunCounts::default();
    let mut verdicts = 0;
    if let Some(scenarios) = scorecard
        .get("scenarios")
        .and_then(serde_json::Value::as_array)
    {
        for scenario in scenarios {
            if let Some(verdict) =
                string_field(scenario, &["verdict", "status", "result", "outcome"])
            {
                if add_verdict(&mut counts, verdict) {
                    verdicts += 1;
                }
            }
        }
    }
    (verdicts > 0).then_some(counts)
}

fn generated_at_from_scorecard(scorecard: &serde_json::Value) -> Option<u64> {
    number_field(scorecard, &["generatedAtUnixMs", "generated_at_unix_ms"]).map(u64::from)
}

fn apply_event_to_artifacts(parsed: &mut ParsedRunArtifacts, event: &serde_json::Value) {
    if event.get("kind").and_then(serde_json::Value::as_str) == Some("conclusion") {
        parsed.conclusion = event.get("payload").cloned();
    }
    if event.get("kind").and_then(serde_json::Value::as_str) == Some("artifact") {
        parsed.artifacts.push(
            event
                .get("payload")
                .cloned()
                .unwrap_or_else(|| event.clone()),
        );
    }

    if event.get("kind").and_then(serde_json::Value::as_str) == Some("meta") {
        if let Some(run_id) = event
            .get("payload")
            .and_then(|payload| payload.get("runId"))
            .and_then(serde_json::Value::as_str)
        {
            parsed.run_id = Some(run_id.to_string());
        }
    }

    if event.get("kind").and_then(serde_json::Value::as_str) == Some("scenario-verdict") {
        if let Some(verdict) = event
            .get("payload")
            .and_then(|payload| payload.get("verdict"))
            .and_then(serde_json::Value::as_str)
        {
            let _ = add_verdict(&mut parsed.counts, verdict);
        }
    }
}

fn parse_run_jsonl(path: &Path, include_events: bool) -> ParsedRunArtifacts {
    let mut parsed = ParsedRunArtifacts::default();
    let Ok(text) = fs::read_to_string(path.join("run.jsonl")) else {
        parsed.incomplete = true;
        parsed.conclusion = Some(serde_json::json!({
            "status": "aborted",
            "message": "run aborted before verdict",
            "scenarios": [],
            "notChecked": [],
        }));
        return parsed;
    };
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(event) => {
                apply_event_to_artifacts(&mut parsed, &event);
                if include_events {
                    parsed.events.push(event);
                }
            }
            Err(_) => {
                parsed.parse_errors += 1;
                parsed.incomplete = true;
                parsed.conclusion.get_or_insert_with(|| {
                    serde_json::json!({
                        "status": "aborted",
                        "message": "run aborted before verdict",
                        "scenarios": [],
                        "notChecked": [],
                    })
                });
            }
        }
    }
    parsed
}

fn parse_scorecard(path: &Path, parsed: &mut ParsedRunArtifacts) {
    let scorecard_path = path.join("scorecard.json");
    if !scorecard_path.exists() {
        parsed.incomplete = true;
        parsed.conclusion.get_or_insert_with(|| {
            serde_json::json!({
                "status": "aborted",
                "message": "run aborted before verdict",
                "scenarios": [],
                "notChecked": [],
            })
        });
        return;
    }
    match fs::read_to_string(&scorecard_path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    {
        Some(scorecard) => {
            if let Some(counts) = counts_from_scorecard(&scorecard) {
                parsed.counts = counts;
            }
            parsed.updated_at_unix_ms = generated_at_from_scorecard(&scorecard);
            if let Some(run_id) = scorecard
                .get("runId")
                .or_else(|| scorecard.get("run_id"))
                .and_then(serde_json::Value::as_str)
            {
                parsed.run_id = Some(run_id.to_string());
            }
            parsed.scorecard = Some(scorecard);
        }
        None => {
            parsed.parse_errors += 1;
            parsed.incomplete = true;
            parsed.conclusion = Some(serde_json::json!({
                "status": "aborted",
                "message": "run aborted before verdict",
                "scenarios": [],
                "notChecked": [],
            }));
        }
    }
}

fn scenario_ids_from_parsed_run(parsed: &ParsedRunArtifacts) -> HashSet<String> {
    let mut ids = HashSet::new();
    if let Some(scorecard) = &parsed.scorecard {
        if let Some(scenarios) = scorecard
            .get("scenarios")
            .and_then(serde_json::Value::as_array)
        {
            for scenario in scenarios {
                if let Some(name) = string_field(scenario, &["scenarioName", "scenario_name"]) {
                    ids.insert(name.to_string());
                }
            }
        }
    }
    for event in &parsed.events {
        if event.get("kind").and_then(serde_json::Value::as_str) == Some("scenario-verdict") {
            if let Some(id) = event
                .get("scenarioId")
                .or_else(|| event.get("scenario_id"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    event
                        .get("payload")
                        .and_then(|payload| string_field(payload, &["scenarioId", "scenario_id"]))
                })
            {
                ids.insert(id.to_string());
            }
        }
    }
    ids
}

fn artifact_type(value: &serde_json::Value) -> Option<&str> {
    string_field(value, &["type", "artifactType", "artifact_type", "label"])
}

fn artifact_is_prunable(value: &serde_json::Value) -> bool {
    matches!(
        artifact_type(value).map(|kind| kind.to_ascii_lowercase()),
        Some(kind) if kind == "video" || kind == "audio"
    )
}

fn resolve_artifact_path(
    root: &Path,
    run_dir: &Path,
    artifact: &serde_json::Value,
) -> Option<PathBuf> {
    let raw = string_field(artifact, &["path", "file", "relativePath", "relative_path"])?;
    let candidate = PathBuf::from(raw);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        run_dir.join(candidate)
    };
    let root = root.canonicalize().ok()?;
    let parent = absolute.parent()?.canonicalize().ok()?;
    let file_name = absolute.file_name()?;
    let resolved = parent.join(file_name);
    if resolved.starts_with(&root) {
        Some(resolved)
    } else {
        None
    }
}

fn prune_artifacts_under(
    root: &Path,
    config: ArtifactRetentionConfig,
    now: SystemTime,
) -> ArtifactRetentionReport {
    let cutoff = now
        .checked_sub(Duration::from_secs(
            config.max_age_days.saturating_mul(24 * 60 * 60),
        ))
        .unwrap_or(UNIX_EPOCH);
    let cutoff_ms = unix_ms_from_system_time(cutoff).unwrap_or(0);
    let Ok(entries) = fs::read_dir(root) else {
        return ArtifactRetentionReport {
            max_age_days: config.max_age_days,
            max_runs_per_scenario: config.max_runs_per_scenario,
            ..Default::default()
        };
    };

    let mut runs = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|path| {
            let mut parsed = parse_run_jsonl(&path, true);
            parse_scorecard(&path, &mut parsed);
            let updated_at = parsed
                .updated_at_unix_ms
                .unwrap_or_else(|| updated_at_unix_ms(&path));
            let scenario_ids = scenario_ids_from_parsed_run(&parsed);
            (path, parsed, updated_at, scenario_ids)
        })
        .collect::<Vec<_>>();
    runs.sort_by(|a, b| b.2.cmp(&a.2));

    let mut report = ArtifactRetentionReport {
        max_age_days: config.max_age_days,
        max_runs_per_scenario: config.max_runs_per_scenario,
        scanned_runs: runs.len().try_into().unwrap_or(u32::MAX),
        ..Default::default()
    };
    let mut kept_by_scenario: HashMap<String, usize> = HashMap::new();
    for (run_dir, parsed, updated_at, scenario_ids) in runs {
        let within_age = updated_at >= cutoff_ms;
        let within_count = scenario_ids.iter().any(|scenario| {
            let count = kept_by_scenario.entry(scenario.clone()).or_insert(0);
            if *count < config.max_runs_per_scenario {
                *count += 1;
                true
            } else {
                false
            }
        });
        let keep_run = within_age || within_count;
        for artifact in parsed
            .artifacts
            .iter()
            .filter(|artifact| artifact_is_prunable(artifact))
        {
            let Some(path) = resolve_artifact_path(root, &run_dir, artifact) else {
                report.skipped_files = report.skipped_files.saturating_add(1);
                continue;
            };
            if keep_run {
                report.kept_files = report.kept_files.saturating_add(1);
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => report.pruned_files = report.pruned_files.saturating_add(1),
                Err(_) if !path.exists() => {
                    report.pruned_files = report.pruned_files.saturating_add(1)
                }
                Err(_) => report.skipped_files = report.skipped_files.saturating_add(1),
            }
        }
    }
    report
}

fn prune_test_cockpit_artifacts() -> ArtifactRetentionReport {
    prune_artifacts_under(
        &test_runs_root(),
        ArtifactRetentionConfig::from_env(),
        SystemTime::now(),
    )
}

fn run_summary_from_dir(path: &Path) -> TestCockpitRunSummary {
    let mut parsed = parse_run_jsonl(path, false);
    parse_scorecard(path, &mut parsed);
    let run_id = parsed.run_id.unwrap_or_else(|| {
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown")
            .to_string()
    });
    let updated_at_unix_ms = parsed
        .updated_at_unix_ms
        .unwrap_or_else(|| updated_at_unix_ms(path));
    TestCockpitRunSummary {
        run_id,
        results_dir: path.display().to_string(),
        updated_at_unix_ms,
        status: if parsed.incomplete {
            "incomplete".to_string()
        } else {
            run_status(parsed.counts)
        },
        pass: parsed.counts.pass,
        fail: parsed.counts.fail,
        skipped: parsed.counts.skipped,
        parse_errors: parsed.parse_errors,
        conclusion: parsed.conclusion,
    }
}

fn resolve_listed_results_dir(results_dir: &str) -> Result<PathBuf, String> {
    let root = test_runs_root()
        .canonicalize()
        .map_err(|_| "test cockpit results directory does not exist".to_string())?;
    let requested = PathBuf::from(results_dir)
        .canonicalize()
        .map_err(|_| "test cockpit run does not exist".to_string())?;
    if requested.parent() == Some(root.as_path()) && requested.is_dir() {
        Ok(requested)
    } else {
        Err("test cockpit run must be a direct child of the local test-runs directory".to_string())
    }
}

#[tauri::command]
pub fn list_test_cockpit_runs() -> Vec<TestCockpitRunSummary> {
    let root = test_runs_root();
    let Ok(entries) = fs::read_dir(root) else {
        return vec![];
    };
    let mut runs = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|path| run_summary_from_dir(&path))
        .collect::<Vec<_>>();
    runs.sort_by(|a, b| {
        b.updated_at_unix_ms
            .cmp(&a.updated_at_unix_ms)
            .then_with(|| b.run_id.cmp(&a.run_id))
    });
    runs
}

#[tauri::command]
pub fn get_test_cockpit_run(results_dir: String) -> Result<TestCockpitRunDetail, String> {
    let path = resolve_listed_results_dir(&results_dir)?;
    let mut parsed = parse_run_jsonl(&path, true);
    parse_scorecard(&path, &mut parsed);
    let summary = run_summary_from_dir(&path);
    Ok(TestCockpitRunDetail {
        parse_errors: parsed.parse_errors,
        summary,
        events: parsed.events,
        artifacts: parsed.artifacts,
        scorecard: parsed.scorecard,
        conclusion: parsed.conclusion,
    })
}

fn mime_for_preview_artifact(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(OsStr::to_str)
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("m4a") => Some("audio/mp4"),
        Some("mov") => Some("video/quicktime"),
        Some("mp4") => Some("video/mp4"),
        _ => None,
    }
}

fn resolve_run_child_file(run_dir: &Path, relative_path: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(relative_path);
    if candidate.is_absolute() {
        return Err("test cockpit artifact path must be relative".to_string());
    }
    let absolute = run_dir.join(candidate);
    let run_dir = run_dir
        .canonicalize()
        .map_err(|_| "test cockpit run does not exist".to_string())?;
    let parent = absolute
        .parent()
        .ok_or("test cockpit artifact path is invalid".to_string())?
        .canonicalize()
        .map_err(|_| "test cockpit artifact parent does not exist".to_string())?;
    let file_name = absolute
        .file_name()
        .ok_or("test cockpit artifact path is invalid".to_string())?;
    let resolved = parent.join(file_name);
    if resolved.starts_with(&run_dir) && resolved.is_file() {
        Ok(resolved)
    } else {
        Err("test cockpit artifact must be inside the selected run directory".to_string())
    }
}

#[tauri::command]
pub fn get_test_cockpit_artifact_data_url(
    results_dir: String,
    path: String,
) -> Result<String, String> {
    const MAX_PREVIEW_BYTES: u64 = 64 * 1024 * 1024;
    let run_dir = resolve_listed_results_dir(&results_dir)?;
    let artifact_path = resolve_run_child_file(&run_dir, &path)?;
    let mime = mime_for_preview_artifact(&artifact_path).ok_or(
        "test cockpit artifact preview supports image, audio, and video files only".to_string(),
    )?;
    let metadata = artifact_path
        .metadata()
        .map_err(|e| format!("could not read test cockpit artifact metadata: {e}"))?;
    if metadata.len() > MAX_PREVIEW_BYTES {
        return Err("test cockpit artifact is too large to preview inline".to_string());
    }
    let bytes = fs::read(&artifact_path)
        .map_err(|e| format!("could not read test cockpit artifact: {e}"))?;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{mime};base64,{encoded}"))
}

fn run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    millis.to_string()
}

fn update_status(state: &CockpitRuntimeState, status: CockpitStatus) {
    match state.status.lock() {
        Ok(mut guard) => *guard = status,
        Err(poisoned) => *poisoned.into_inner() = status,
    }
}

fn current_status(state: &CockpitRuntimeState) -> CockpitStatus {
    match state.status.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScenarioKind {
    NativeToWebShare,
    WebToNativeShare,
    Draw,
    Camera,
    /// CAM-N2W (journey CAM-05, #815): the reverse of `Camera`. The NATIVE
    /// camera publishes and the WEB peer is the oracle -- it measures whether
    /// the received tile is rendering ADVANCING, NON-BLACK frames, never that
    /// a track was subscribed ("arrived" is not "visible", #806). On a machine
    /// with no camera, `PETAL_CAMERA_SYNTH_SOURCE=1` substitutes an NV12 test
    /// pattern for camera INPUT while leaving the whole publish path real.
    CameraNativeToWeb,
    Audio,
    /// AUD-N2W (journey AUD-04, #812): the reverse of `Audio`. The NATIVE mic
    /// publishes and the WEB peer is the oracle -- it measures post-decode
    /// signal energy on the received track. Requires `PETAL_DISABLE_AUDIO=0`
    /// (the launcher refuses otherwise) and, on an agent machine with no real
    /// sound in the room, `PETAL_AUDIO_SYNTH_TONE=1` to substitute a 440Hz
    /// tone for mic INPUT while leaving the whole publish path real.
    AudioNativeToWeb,
    Telepointer,
    ChaosDevice,
    ChaosDisplayChange,
    ChaosNet,
    ChaosLifecycle,
    MultiPeer,
    RemoteControlScaled,
    SoakStallWatch,
    /// SHARE-N2N: sharer = primary instance, receiver = the Native Test Client
    /// (test-peer). Validates the defining feature — a real, borderless,
    /// independently movable native window on the receiver.
    NativeToNativeShare,
    /// SHARE-05: share several windows at once; focus-weighted cap keeps
    /// non-focused shares live. Oracle: gap_oracles::evaluate_focus_weighted_cap.
    MultiWindowShare,
    /// SHARE-06: share windows across multiple displays/Spaces; a window dragged
    /// across displays keeps flowing. Needs >=2 displays (opt-in tier).
    MultiDisplayShare,
    /// SHARE-10: share a whole display (not a window); composites on the peer,
    /// sharer border persists (#199).
    FullDesktopShare,
    /// CAM-03: camera bitrate tracks the resolution/quality tier, not just fps>0
    /// (#246). Oracle: gap_oracles::assert_bitrate_tracks_tier.
    CameraBitrateScaling,
    /// CAM-04: no-new-frame-for-N watchdog on the gallery camera tile (#247).
    /// Oracle: gap_oracles::detect_camera_stall.
    CameraStall,
    /// ROOM-01: one-click join; roster matches on all sides. Oracle:
    /// gap_oracles::assert_rosters_match.
    JoinRoom,
    /// UI-01..04: nat-local screenshot + text-overflow assertion (no peer).
    /// Oracle: gap_oracles::assert_no_text_overflow.
    UiScreenshot,
    /// RC-N2N (journey RC-07, #819): Petal as the CONTROLLER against a second
    /// native instance. The test-peer shares a sacrificial target and acts as
    /// the remote-control HOST; this instance drives the REAL compositor/
    /// control route and asserts host-side effects on the peer.
    RemoteControlNativeToNative,
    /// RC-N2W (journey RC-07, #819): the same native controller against a WEB
    /// peer. A browser cannot inject OS input, so this leg proves DELIVERY --
    /// request, grant handshake and inputs arriving intact -- and nothing more.
    RemoteControlNativeToWeb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScenarioSpec {
    id: &'static str,
    tier: &'static str,
    kind: ScenarioKind,
    requires_native_share: bool,
}

const SCENARIO_TABLE: &[ScenarioSpec] = &[
    ScenarioSpec {
        id: "SHARE-N2W-Q",
        tier: "quick",
        kind: ScenarioKind::NativeToWebShare,
        requires_native_share: true,
    },
    ScenarioSpec {
        id: "SHARE-W2N-Q",
        tier: "quick",
        kind: ScenarioKind::WebToNativeShare,
        requires_native_share: false,
    },
    ScenarioSpec {
        // DRAW-N needs a native-owned shared window: the native draw-delivery
        // journal (which the assertion polls for) only fires on the SharerOverlay
        // path, i.e. when THIS machine owns the drawn-on window. The web peer
        // draws on the native share tile; without a native share there is nothing
        // to draw on and the assertion is structurally unreachable.
        id: "DRAW-N",
        tier: "quick",
        kind: ScenarioKind::Draw,
        requires_native_share: true,
    },
    ScenarioSpec {
        id: "CAM",
        tier: "quick",
        kind: ScenarioKind::Camera,
        requires_native_share: false,
    },
    ScenarioSpec {
        // Gap tier, not quick, for the same reason AUD-N2W is: it needs an
        // opt-in source hook (PETAL_CAMERA_SYNTH_SOURCE=1, or a real camera
        // pointed at something), so it stays out of the default sweep rather
        // than silently skipping inside it.
        id: "CAM-N2W",
        tier: "gap",
        kind: ScenarioKind::CameraNativeToWeb,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "AUD",
        tier: "quick",
        kind: ScenarioKind::Audio,
        requires_native_share: false,
    },
    ScenarioSpec {
        // Gap tier, not quick: this is the only scenario that cannot run in a
        // video-only (PETAL_DISABLE_AUDIO=1) sweep, so it stays opt-in rather
        // than silently skipping inside the default tier.
        id: "AUD-N2W",
        tier: "gap",
        kind: ScenarioKind::AudioNativeToWeb,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "TELE",
        tier: "quick",
        kind: ScenarioKind::Telepointer,
        requires_native_share: true,
    },
    ScenarioSpec {
        id: "CHAOS-DEVICE",
        tier: "full",
        kind: ScenarioKind::ChaosDevice,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "CHAOS-DISPLAY-CHANGE",
        tier: "full",
        kind: ScenarioKind::ChaosDisplayChange,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "CHAOS-NET",
        tier: "full",
        kind: ScenarioKind::ChaosNet,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "CHAOS-LIFECYCLE",
        tier: "full",
        kind: ScenarioKind::ChaosLifecycle,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "MULTI-3",
        tier: "full",
        kind: ScenarioKind::MultiPeer,
        requires_native_share: false,
    },
    ScenarioSpec {
        // RC-P1080 is intentionally a narrow smoke check, not a replacement
        // for the 29-case suite; see issue #482 for the full rationale.
        id: "RC-P1080",
        tier: "full",
        kind: ScenarioKind::RemoteControlScaled,
        requires_native_share: true,
    },
    ScenarioSpec {
        id: "SOAK-W2N-STALL",
        tier: "soak",
        kind: ScenarioKind::SoakStallWatch,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "SOAK-N2W-STALL",
        tier: "soak",
        kind: ScenarioKind::SoakStallWatch,
        requires_native_share: true,
    },
    ScenarioSpec {
        id: "CHAOS-NET-SOAK",
        tier: "soak",
        kind: ScenarioKind::ChaosNet,
        requires_native_share: false,
    },
    // The native-native proof is part of Full, never Quick. A missing
    // separately-built/test-peer-granted receiver is a per-scenario setup skip,
    // rather than an excuse to remove Petal's defining journey from Full.
    ScenarioSpec {
        id: "SHARE-N2N",
        tier: "full",
        kind: ScenarioKind::NativeToNativeShare,
        requires_native_share: true,
    },
    // P-3 gap-journey scenarios. All on opt-in tiers (never quick/full/soak) so
    // a headless quick/full/soak run never blocks on live orchestration that is
    // not auto-driven yet. Each preflights + returns INFRA-FAIL (honest scaffold,
    // never a false pass); its pass-criteria oracle is unit-tested in
    // `gap_oracles`. Reachable via journey id (SHARE-05 etc.) or direct id.
    ScenarioSpec {
        id: "SHARE-MULTIWIN",
        tier: "native",
        kind: ScenarioKind::MultiWindowShare,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "SHARE-MULTIDISP",
        tier: "multi-display",
        kind: ScenarioKind::MultiDisplayShare,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "SHARE-DESKTOP",
        tier: "native",
        kind: ScenarioKind::FullDesktopShare,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "CAM-BITRATE",
        tier: "gap",
        kind: ScenarioKind::CameraBitrateScaling,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "CAM-STALL",
        tier: "gap",
        kind: ScenarioKind::CameraStall,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "ROOM-JOIN",
        tier: "gap",
        kind: ScenarioKind::JoinRoom,
        requires_native_share: false,
    },
    // RC-N2N / RC-N2W (#819). Opt-in tier, never quick/full/soak: RC-N2N needs
    // the test-peer's ACCESSIBILITY grant on top of Screen Recording, plus a
    // sacrificial target app, so folding it into `full` would turn a missing
    // one-time grant into a broken sweep. AUD-N2W set this precedent for the
    // same reason.
    ScenarioSpec {
        id: "RC-N2N",
        tier: "gap",
        kind: ScenarioKind::RemoteControlNativeToNative,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "RC-N2W",
        tier: "gap",
        kind: ScenarioKind::RemoteControlNativeToWeb,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "UI-MAIN",
        tier: "ui",
        kind: ScenarioKind::UiScreenshot,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "UI-GALLERY",
        tier: "ui",
        kind: ScenarioKind::UiScreenshot,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "UI-PILL",
        tier: "ui",
        kind: ScenarioKind::UiScreenshot,
        requires_native_share: false,
    },
    ScenarioSpec {
        id: "UI-DOCK",
        tier: "ui",
        kind: ScenarioKind::UiScreenshot,
        requires_native_share: false,
    },
];

/// Feature the journey belongs to (the project history sections A-H).
/// `code` is the single-letter selector, `name` the human label, `slug` an
/// alternate selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Feature {
    code: &'static str,
    name: &'static str,
    slug: &'static str,
}

const FEATURES: &[Feature] = &[
    Feature {
        code: "A",
        name: "Screen Sharing",
        slug: "screen-sharing",
    },
    Feature {
        code: "B",
        name: "Camera",
        slug: "camera",
    },
    Feature {
        code: "C",
        name: "Audio",
        slug: "audio",
    },
    Feature {
        code: "D",
        name: "Remote Control",
        slug: "remote-control",
    },
    Feature {
        code: "E",
        name: "Telepointers & Annotation",
        slug: "telepointers-annotation",
    },
    Feature {
        code: "F",
        name: "Resilience",
        slug: "resilience",
    },
    Feature {
        code: "G",
        name: "Rooms & Multi-peer",
        slug: "rooms-multi-peer",
    },
    Feature {
        code: "H",
        name: "UI Correctness",
        slug: "ui-correctness",
    },
    Feature {
        code: "I",
        name: "Install & Release",
        slug: "install-release",
    },
];

/// Journey metadata layer (the project history) — the presentation +
/// selection surface over the runnable `SCENARIO_TABLE`. Feature-first, so a
/// user picks "Screen Sharing / P0 / Short" instead of a mystery tier.
///
/// Each journey optionally links to a runnable scenario id (`runnable`); gap
/// journeys (`runnable: None`) are declared-but-not-yet-executable (status
/// "gap"/"blind-spot") so the map stays honest. `legacy` records the old
/// mechanics IDs that still resolve for back-compat. This table is mirrored into
/// `contracts/petal-contracts.json` (`testCockpitJourneys`) and kept in lockstep
/// by `journey_contract_parity` below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Journey {
    id: &'static str,
    title: &'static str,
    /// Feature code "A".."H".
    feature: &'static str,
    /// Direction: "nat-nat" | "web-nat" | "nat-web" | "both" | "nat-local"
    /// | "web-local" (browser-only journey, no native peer involved).
    direction: &'static str,
    /// Priority: "P0" | "P1" | "P2".
    priority: &'static str,
    /// Depth: "short" | "long" | "short-long".
    depth: &'static str,
    /// Status: "covered" | "partial" | "gap" | "blind-spot".
    status: &'static str,
    /// Runnable scenario id this journey executes today, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    runnable: Option<&'static str>,
    legacy: &'static [&'static str],
}

const JOURNEY_TABLE: &[Journey] = &[
    // A · Screen Sharing
    // SHARE-01's TCC-granted physical run PASSED 2026-08-14 (run
    // 1786746104499): receiver window translated by (120,60), size preserved,
    // sharer source window independent -- the WindowServer geometry oracle on
    // a real two-instance share. The defining feature is live-proven.
    Journey {
        id: "SHARE-01",
        title: "Real native window",
        feature: "A",
        direction: "nat-nat",
        priority: "P0",
        depth: "short",
        status: "covered",
        runnable: Some("SHARE-N2N"),
        legacy: &[],
    },
    Journey {
        id: "SHARE-02",
        title: "Crisp text",
        feature: "A",
        direction: "web-nat",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: Some("SHARE-W2N-Q"),
        legacy: &["SHARE-W2N-Q"],
    },
    Journey {
        id: "SHARE-03",
        title: "Smooth & fast",
        feature: "A",
        direction: "web-nat",
        priority: "P0",
        depth: "short",
        status: "covered",
        runnable: Some("SHARE-W2N-Q"),
        legacy: &["SHARE-W2N-Q"],
    },
    Journey {
        id: "SHARE-04",
        title: "No stall (endurance)",
        feature: "A",
        direction: "both",
        priority: "P1",
        depth: "long",
        status: "covered",
        runnable: Some("SOAK-W2N-STALL"),
        legacy: &["SOAK-W2N-STALL", "SOAK-N2W-STALL"],
    },
    // SHARE-05/06/10: P-3 landed the focus-weighted-cap oracle
    // (gap_oracles::evaluate_focus_weighted_cap) + honest INFRA-FAIL scaffolds
    // (SHARE-MULTIWIN/-MULTIDISP/-DESKTOP). "partial", not "covered": the live
    // multi-window / multi-display / full-desktop capture orchestration is not
    // auto-driven headlessly yet — the scenarios refuse to false-pass.
    Journey {
        id: "SHARE-05",
        title: "Multi-window",
        feature: "A",
        direction: "nat-web",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("SHARE-MULTIWIN"),
        legacy: &[],
    },
    Journey {
        id: "SHARE-06",
        title: "Multi-display",
        feature: "A",
        direction: "nat-web",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("SHARE-MULTIDISP"),
        legacy: &[],
    },
    Journey {
        id: "SHARE-07",
        title: "Clean lifecycle",
        feature: "A",
        direction: "nat-web",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: Some("CHAOS-LIFECYCLE"),
        legacy: &["CHAOS-LIFECYCLE"],
    },
    Journey {
        id: "SHARE-08",
        title: "Occluded window",
        feature: "A",
        direction: "nat-web",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: Some("SHARE-N2W-Q"),
        legacy: &["SHARE-N2W-Q"],
    },
    Journey {
        id: "SHARE-09",
        title: "Network recovery",
        feature: "A",
        direction: "web-nat",
        priority: "P1",
        depth: "short-long",
        status: "partial",
        runnable: Some("CHAOS-NET"),
        legacy: &["CHAOS-NET"],
    },
    Journey {
        id: "SHARE-10",
        title: "Full desktop",
        feature: "A",
        direction: "nat-web",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("SHARE-DESKTOP"),
        legacy: &[],
    },
    // B · Camera
    Journey {
        id: "CAM-01",
        title: "Camera on",
        feature: "B",
        direction: "web-nat",
        priority: "P0",
        depth: "short",
        status: "covered",
        runnable: Some("CAM"),
        legacy: &["CAM"],
    },
    Journey {
        id: "CAM-02",
        title: "Camera off tile",
        feature: "B",
        direction: "web-nat",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: Some("CAM"),
        legacy: &["CAM"],
    },
    // CAM-03/04: P-3 landed the bitrate-tier and stall-watchdog oracles
    // (gap_oracles::assert_bitrate_tracks_tier / detect_camera_stall) + honest
    // INFRA-FAIL scaffolds (CAM-BITRATE / CAM-STALL). "partial" — live camera
    // bitrate/stall telemetry orchestration is not auto-driven headlessly yet.
    Journey {
        id: "CAM-03",
        title: "Bitrate scaling",
        feature: "B",
        direction: "web-nat",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("CAM-BITRATE"),
        legacy: &[],
    },
    Journey {
        id: "CAM-04",
        title: "Camera stall",
        feature: "B",
        direction: "web-nat",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("CAM-STALL"),
        legacy: &[],
    },
    // CAM-05: every camera journey above verifies web->native. CAM-N2W is the
    // reverse leg (#815) -- the native camera on a WEB peer's tile, judged on
    // advancing non-black decoded frames rather than "a track was subscribed".
    // "partial", not "covered", for the same reason as AUD-04: on a machine
    // with no camera the input is a synthetic NV12 pattern
    // (PETAL_CAMERA_SYNTH_SOURCE=1), so everything from the publish path
    // onward is proven and AVFoundation CAPTURE itself is not -- that half
    // stays human-verified.
    Journey {
        id: "CAM-05",
        title: "Camera seen (reverse)",
        feature: "B",
        direction: "nat-web",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: Some("CAM-N2W"),
        legacy: &[],
    },
    // C · Audio
    Journey {
        id: "AUD-01",
        title: "Voice heard",
        feature: "C",
        direction: "web-nat",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: Some("AUD"),
        legacy: &["AUD"],
    },
    Journey {
        id: "AUD-02",
        title: "Mute toggle",
        feature: "C",
        direction: "nat-web",
        priority: "P1",
        depth: "short",
        status: "covered",
        runnable: Some("AUD"),
        legacy: &["AUD"],
    },
    Journey {
        id: "AUD-03",
        title: "Audio device swap",
        feature: "C",
        direction: "nat-local",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("CHAOS-DEVICE"),
        legacy: &["CHAOS-DEVICE"],
    },
    // AUD-04: the reverse leg of #787. AUD-01 proves web->native decode;
    // AUD-N2W proves the NATIVE mic reaches a web listener as AUDIBLE audio
    // (post-decode RMS/peak measured in the browser, never packet counters).
    // "partial", not "covered": on a machine with no real sound in the room
    // the input is a synthetic tone (PETAL_AUDIO_SYNTH_TONE=1), so everything
    // from the publish path onward is proven and CoreAudio mic CAPTURE itself
    // is not -- that half stays human-verified.
    Journey {
        id: "AUD-04",
        title: "Voice heard (reverse)",
        feature: "C",
        direction: "nat-web",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: Some("AUD-N2W"),
        legacy: &[],
    },
    // D · Remote Control
    // RC-01..06 remain honest journey-table gaps until the live RC-P1080 run
    // is exercised by the orchestrator. RC-P1080 now has a real drive/verify
    // engine: the LiveKit harness drives inputs, while the sentinel ledger is
    // the host-effect oracle. Baseline diffing remains tracked separately in
    // #379; the #337 freeze is overridden for this cockpit scope (#456).
    Journey {
        id: "RC-01",
        title: "Click",
        feature: "D",
        direction: "web-nat",
        priority: "P2",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &["RC-P1080"],
    },
    Journey {
        id: "RC-02",
        title: "Drag-select",
        feature: "D",
        direction: "web-nat",
        priority: "P2",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &["RC-P1080"],
    },
    Journey {
        id: "RC-03",
        title: "Type",
        feature: "D",
        direction: "web-nat",
        priority: "P2",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &["RC-P1080"],
    },
    Journey {
        id: "RC-04",
        title: "Shortcuts",
        feature: "D",
        direction: "web-nat",
        priority: "P2",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &["RC-P1080"],
    },
    Journey {
        id: "RC-05",
        title: "Latency",
        feature: "D",
        direction: "web-nat",
        priority: "P2",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &["RC-P1080"],
    },
    Journey {
        id: "RC-06",
        title: "Scaled tier",
        feature: "D",
        direction: "web-nat",
        priority: "P2",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &["RC-P1080"],
    },
    // RC-07 (#819): rc-live-suite.sh proves web->native control exhaustively.
    // RC-N2N now drives the reverse -- a NATIVE controller against the
    // test-peer as host -- through the real compositor/control route, and
    // asserts host-side effects on the peer (grant, replay dispositions, the
    // sacrificial document's own text). RC-N2W covers native->web.
    //
    // "partial", not "covered", and deliberately so:
    //   - RC-N2W is a DELIVERY proof only. A browser cannot inject OS input,
    //     so nothing in that leg says an input was applied.
    //   - both scenarios are opt-in tier and need the test-peer's one-time
    //     Accessibility grant, so a headless sweep does not exercise them.
    Journey {
        id: "RC-07",
        title: "Control (reverse & nat-nat)",
        feature: "D",
        direction: "nat-nat",
        priority: "P2",
        depth: "short",
        status: "partial",
        runnable: Some("RC-N2N"),
        legacy: &[],
    },
    // E · Telepointers & Annotation
    Journey {
        id: "PTR-01",
        title: "Telepointer",
        feature: "E",
        direction: "web-nat",
        priority: "P0",
        depth: "short",
        status: "covered",
        runnable: Some("TELE"),
        legacy: &["TELE"],
    },
    // PTR-02: "both directions" bar. Native→web is covered by DRAW-N; P-3 added
    // the bidirectional oracle (gap_oracles::assert_bidirectional_draw) that
    // encodes the both-directions pass bar. Stays "partial" — the web→native
    // half's live orchestration is not auto-driven headlessly yet.
    Journey {
        id: "PTR-02",
        title: "Draw stroke",
        feature: "E",
        direction: "both",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("DRAW-N"),
        legacy: &["DRAW-N"],
    },
    // F · Resilience
    Journey {
        id: "RES-01",
        title: "Bad network",
        feature: "F",
        direction: "web-nat",
        priority: "P1",
        depth: "short-long",
        status: "covered",
        runnable: Some("CHAOS-NET"),
        legacy: &["CHAOS-NET", "CHAOS-NET-SOAK"],
    },
    Journey {
        id: "RES-02",
        title: "Device swap",
        feature: "F",
        direction: "nat-local",
        priority: "P1",
        depth: "short",
        status: "covered",
        runnable: Some("CHAOS-DEVICE"),
        legacy: &["CHAOS-DEVICE"],
    },
    Journey {
        id: "RES-03",
        title: "Display change",
        feature: "F",
        direction: "nat-local",
        priority: "P2",
        depth: "short",
        status: "covered",
        runnable: Some("CHAOS-DISPLAY-CHANGE"),
        legacy: &["CHAOS-DISPLAY-CHANGE"],
    },
    // RES-04: #379 step 1 — downgraded from a false "covered" to honest
    // "gap". There is no runnable scenario and never was (`runnable: None`);
    // claiming "covered" with nothing to run was the same category of lie as
    // RC-01..06 above.
    Journey {
        id: "RES-04",
        title: "Display sleep",
        feature: "F",
        direction: "web-nat",
        priority: "P1",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &[],
    },
    Journey {
        id: "RES-05",
        title: "Peer leaves",
        feature: "F",
        direction: "web-nat",
        priority: "P1",
        depth: "short",
        status: "covered",
        runnable: Some("CHAOS-LIFECYCLE"),
        legacy: &["CHAOS-LIFECYCLE"],
    },
    // RES-06: hide the shared app 5+ min, then unhide -- the share must
    // survive and resume. Known broken today (#810: the 300s defensive
    // restart cannot enumerate a hidden window and treats it as closed).
    Journey {
        id: "RES-06",
        title: "Hidden app share survives",
        feature: "F",
        direction: "nat-web",
        priority: "P1",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &[],
    },
    // RES-07: the sharer's Mac sleeps (lid close) mid-meeting and wakes; the
    // meeting and shares recover. Resilience wiring exists (#734/#749) but no
    // scenario drives a real sleep/wake cycle.
    Journey {
        id: "RES-07",
        title: "System sleep/wake",
        feature: "F",
        direction: "nat-web",
        priority: "P1",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &[],
    },
    // G · Rooms & multi-peer
    // ROOM-01: P-3 landed the roster-match oracle (gap_oracles::assert_rosters_match)
    // + the ROOM-JOIN scaffold. Stays "partial" — the two-sided live roster
    // comparison is not auto-driven headlessly yet.
    Journey {
        id: "ROOM-01",
        title: "Join room",
        feature: "G",
        direction: "nat-web",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: Some("ROOM-JOIN"),
        legacy: &[],
    },
    Journey {
        id: "ROOM-02",
        title: "Multi-peer",
        feature: "G",
        direction: "nat-web",
        priority: "P0",
        depth: "short",
        status: "covered",
        runnable: Some("MULTI-3"),
        legacy: &["MULTI-3"],
    },
    // H · UI correctness
    // UI-01..04: P-3 landed the text-overflow oracle
    // (gap_oracles::assert_no_text_overflow — the scrollWidth<=clientWidth
    // UI-text hard rule) + per-view UI-* scaffolds. "partial" — the live
    // screenshot capture + in-webview measurement is not auto-driven headlessly.
    // JOIN-03: the real onboarding path -- a meet.petal.live/<label>/<code>
    // join link lands in the meeting, including with the native app hidden
    // (the hot-mic-no-visible-UI class, #783 check 3).
    Journey {
        id: "JOIN-03",
        title: "Join link",
        feature: "G",
        direction: "web-local",
        priority: "P0",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &[],
    },
    // JOIN-04: leave then rejoin, twice; camera (#638) and display shares
    // (#722) are the known-shipped regression classes this pins.
    Journey {
        id: "JOIN-04",
        title: "Leave & rejoin",
        feature: "G",
        direction: "nat-web",
        priority: "P1",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &[],
    },
    Journey {
        id: "UI-01",
        title: "Main menu",
        feature: "H",
        direction: "nat-local",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("UI-MAIN"),
        legacy: &[],
    },
    Journey {
        id: "UI-02",
        title: "Gallery view",
        feature: "H",
        direction: "nat-local",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("UI-GALLERY"),
        legacy: &[],
    },
    Journey {
        id: "UI-03",
        title: "Menubar pill",
        feature: "H",
        direction: "nat-local",
        priority: "P1",
        depth: "short",
        status: "partial",
        runnable: Some("UI-PILL"),
        legacy: &[],
    },
    Journey {
        id: "UI-04",
        title: "Dock icon",
        feature: "H",
        direction: "nat-local",
        priority: "P2",
        depth: "short",
        status: "partial",
        runnable: Some("UI-DOCK"),
        legacy: &[],
    },
    // I · Install & Release (the GET IN phase). These journeys are the release
    // walk's first phase; none is cockpit-runnable -- INST-01/INST-02 are the
    // release-smoke script + its human clean-TCC checklist, INST-03 needs a
    // staging updater channel that does not exist yet, INST-04 is the
    // deployed-web-app health scripts. Declared here so the phase structure is
    // complete and the selectors answer honestly instead of "unknown".
    Journey {
        id: "INST-01",
        title: "DMG launches clean",
        feature: "I",
        direction: "nat-local",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: None,
        legacy: &[],
    },
    Journey {
        id: "INST-02",
        title: "First-run permissions",
        feature: "I",
        direction: "nat-local",
        priority: "P0",
        depth: "short",
        status: "blind-spot",
        runnable: None,
        legacy: &[],
    },
    Journey {
        id: "INST-03",
        title: "Auto-update from previous",
        feature: "I",
        direction: "nat-local",
        priority: "P0",
        depth: "short",
        status: "gap",
        runnable: None,
        legacy: &[],
    },
    Journey {
        id: "INST-04",
        title: "Web app loads",
        feature: "I",
        direction: "web-local",
        priority: "P0",
        depth: "short",
        status: "partial",
        runnable: None,
        legacy: &[],
    },
];

/// The human release walk (docs/TEST_PLAN.md): the order a human tester moves
/// through the product, each phase naming its journeys. This is a selection +
/// presentation layer over JOURNEY_TABLE -- phases add no runnable state of
/// their own, so they are deliberately NOT mirrored into the wire contract.
/// `phase_table_covers_every_journey` below pins that no journey is ever left
/// out of a phase (the failure mode that made the old tier system illegible).
#[derive(Clone, Copy, Debug)]
struct Phase {
    slug: &'static str,
    title: &'static str,
    journeys: &'static [&'static str],
}

const PHASE_TABLE: &[Phase] = &[
    Phase {
        slug: "get-in",
        title: "Get in — install, launch, trust",
        journeys: &["INST-01", "INST-02", "INST-03", "INST-04"],
    },
    Phase {
        slug: "join",
        title: "Join a meeting",
        journeys: &["ROOM-01", "ROOM-02", "JOIN-03", "JOIN-04"],
    },
    Phase {
        slug: "speak",
        title: "Hear and be heard",
        journeys: &["AUD-01", "AUD-02", "AUD-03", "AUD-04"],
    },
    Phase {
        slug: "see",
        title: "See and be seen",
        journeys: &["CAM-01", "CAM-02", "CAM-03", "CAM-04", "CAM-05"],
    },
    Phase {
        slug: "share",
        title: "Share windows and desktops",
        journeys: &[
            "SHARE-01", "SHARE-02", "SHARE-03", "SHARE-04", "SHARE-05", "SHARE-06", "SHARE-07",
            "SHARE-08", "SHARE-09", "SHARE-10",
        ],
    },
    Phase {
        slug: "control",
        title: "Control remote windows",
        journeys: &["RC-01", "RC-02", "RC-03", "RC-04", "RC-05", "RC-06", "RC-07"],
    },
    Phase {
        slug: "point",
        title: "Point and draw",
        journeys: &["PTR-01", "PTR-02"],
    },
    Phase {
        slug: "survive",
        title: "Survive real-world entropy",
        journeys: &["RES-01", "RES-02", "RES-03", "RES-04", "RES-05", "RES-06", "RES-07"],
    },
    Phase {
        slug: "look",
        title: "The UI itself",
        journeys: &["UI-01", "UI-02", "UI-03", "UI-04"],
    },
];

fn phase_by_slug(slug: &str) -> Option<&'static Phase> {
    PHASE_TABLE
        .iter()
        .find(|phase| phase.slug.eq_ignore_ascii_case(slug.trim()))
}

fn journey_in_phase(phase: &Phase, journey: &Journey) -> bool {
    phase
        .journeys
        .iter()
        .any(|id| id.eq_ignore_ascii_case(journey.id))
}

fn scenario_by_id(id: &str) -> Option<ScenarioSpec> {
    SCENARIO_TABLE
        .iter()
        .copied()
        .find(|scenario| scenario.id.eq_ignore_ascii_case(id))
}

fn journey_by_id(id: &str) -> Option<Journey> {
    JOURNEY_TABLE
        .iter()
        .copied()
        .find(|journey| journey.id.eq_ignore_ascii_case(id))
}

/// The primary journey for a runnable scenario id — the first journey in table
/// order that executes it. Used to tag scenario results with feature/journey
/// metadata so the results UI can group them, without threading journey context
/// through the whole run pipeline.
fn primary_journey_for_scenario(scenario_id: &str) -> Option<Journey> {
    JOURNEY_TABLE.iter().copied().find(|journey| {
        journey
            .runnable
            .is_some_and(|runnable| runnable.eq_ignore_ascii_case(scenario_id))
    })
}

fn feature_name(code: &str) -> &'static str {
    FEATURES
        .iter()
        .find(|feature| feature.code.eq_ignore_ascii_case(code))
        .map(|feature| feature.name)
        .unwrap_or("Unknown")
}

/// Collect the deduped runnable scenarios for every journey matching `pred`,
/// preserving SCENARIO_TABLE order. Journeys with no runnable are skipped (their
/// gap status is surfaced in the UI, not run here).
fn runnable_scenarios_for_journeys(pred: impl Fn(&Journey) -> bool) -> Vec<ScenarioSpec> {
    let mut ids: Vec<&'static str> = Vec::new();
    for journey in JOURNEY_TABLE.iter().filter(|journey| pred(journey)) {
        if let Some(runnable) = journey.runnable {
            if !ids
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(runnable))
            {
                ids.push(runnable);
            }
        }
    }
    SCENARIO_TABLE
        .iter()
        .copied()
        .filter(|scenario| ids.iter().any(|id| id.eq_ignore_ascii_case(scenario.id)))
        .collect()
}

/// Does `selector` (a valid axis intersection) match `journey`? Only used to
/// enrich the matched-but-unrunnable error; returns false on a parse miss.
fn token_axes_match(selector: &str, journey: &Journey) -> bool {
    selector
        .trim()
        .split(':')
        .map(parse_journey_axis)
        .all(|axis| axis.is_some_and(|axis| axis.matches(journey)))
}

/// One axis of a modular selector: a predicate over the journey table.
/// Axes intersect with `:` -- `speak:web-nat` is "the SPEAK phase, restricted
/// to the web->native direction" (docs/TEST_PLAN.md's modular-run grammar).
enum JourneyAxis {
    Phase(&'static Phase),
    Feature(&'static str),
    Priority(String),
    Depth(String),
    Direction(String),
}

impl JourneyAxis {
    fn matches(&self, journey: &Journey) -> bool {
        match self {
            JourneyAxis::Phase(phase) => journey_in_phase(phase, journey),
            JourneyAxis::Feature(code) => journey.feature.eq_ignore_ascii_case(code),
            JourneyAxis::Priority(pri) => journey.priority == pri,
            JourneyAxis::Depth(depth) => {
                journey.depth == *depth || journey.depth == "short-long"
            }
            // A "both"-direction journey satisfies either directional query;
            // an exact match satisfies everything else.
            JourneyAxis::Direction(dir) => {
                journey.direction.eq_ignore_ascii_case(dir)
                    || (journey.direction == "both"
                        && matches!(dir.as_str(), "web-nat" | "nat-web"))
            }
        }
    }
}

/// Parse a single axis token: a phase slug (join/speak/see/...), feature code
/// or slug, priority (p0/p1/p2), depth (short/long), or direction
/// (web-nat/nat-web/nat-nat/nat-local/web-local/both). None if it is none of
/// those, so callers can fall through to journey/scenario id resolution.
fn parse_journey_axis(token: &str) -> Option<JourneyAxis> {
    let t = token.trim();
    if let Some(phase) = phase_by_slug(t) {
        return Some(JourneyAxis::Phase(phase));
    }
    if let Some(feature) = FEATURES.iter().find(|feature| {
        feature.code.eq_ignore_ascii_case(t) || feature.slug.eq_ignore_ascii_case(t)
    }) {
        return Some(JourneyAxis::Feature(feature.code));
    }
    if matches!(t.to_ascii_lowercase().as_str(), "p0" | "p1" | "p2") {
        return Some(JourneyAxis::Priority(t.to_ascii_uppercase()));
    }
    if t.eq_ignore_ascii_case("short") || t.eq_ignore_ascii_case("long") {
        return Some(JourneyAxis::Depth(t.to_ascii_lowercase()));
    }
    if matches!(
        t.to_ascii_lowercase().as_str(),
        "web-nat" | "nat-web" | "nat-nat" | "nat-local" | "web-local" | "both"
    ) {
        return Some(JourneyAxis::Direction(t.to_ascii_lowercase()));
    }
    None
}

/// Resolve a single non-list selector that is one axis token or a
/// `:`-intersection of axis tokens (`speak`, `speak:web-nat`, `p0:short`).
/// Returns None when ANY segment is not an axis token, so the caller can fall
/// through to id-list resolution -- a half-parsed intersection must not
/// silently widen into something else.
fn resolve_journey_group_selector(token: &str) -> Option<Vec<ScenarioSpec>> {
    let axes: Option<Vec<JourneyAxis>> = token
        .trim()
        .split(':')
        .map(parse_journey_axis)
        .collect();
    let axes = axes?;
    if axes.is_empty() {
        return None;
    }
    Some(runnable_scenarios_for_journeys(move |journey| {
        axes.iter().all(|axis| axis.matches(journey))
    }))
}

#[derive(Clone, Debug, serde::Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum ScenarioVerdict {
    Pass,
    TestFail,
    InfraFail,
    Skipped,
    Cancelled,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ScenarioOutcome {
    scenario_id: String,
    verdict: ScenarioVerdict,
    message: String,
    delivered_fps: f64,
    delivered_width: u32,
    delivered_height: u32,
    assertions: Vec<AssertionOutcome>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AssertionOutcome {
    name: String,
    passed: bool,
    detail: String,
}

/// A `scenario-verdict` run.jsonl record: the raw `ScenarioOutcome` flattened
/// with the primary journey's metadata (feature/title/direction/priority/depth)
/// so the results UI can group results by feature/journey without re-deriving it
/// client-side. Extra fields are ignored by older parsers (back-compat).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ScenarioVerdictRecord<'a> {
    #[serde(flatten)]
    outcome: &'a ScenarioOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    journey_id: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    journey_title: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    feature: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    feature_name: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    depth: Option<&'static str>,
    evidence_basis: conclusions::EvidenceBasis,
}

impl<'a> ScenarioVerdictRecord<'a> {
    fn new(outcome: &'a ScenarioOutcome) -> Self {
        let journey = primary_journey_for_scenario(&outcome.scenario_id);
        Self {
            outcome,
            journey_id: journey.map(|journey| journey.id),
            journey_title: journey.map(|journey| journey.title),
            feature: journey.map(|journey| journey.feature),
            feature_name: journey.map(|journey| feature_name(journey.feature)),
            direction: journey.map(|journey| journey.direction),
            priority: journey.map(|journey| journey.priority),
            depth: journey.map(|journey| journey.depth),
            evidence_basis: conclusions::for_scenario(&outcome.scenario_id),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RunMeta {
    run_id: String,
    selector: String,
    room_name: String,
    native_identity: String,
    backend_url: String,
    harness_url: String,
    app_version: &'static str,
    app_commit: &'static str,
    /// The parent process's persisted capability, populated only after its
    /// join succeeds. Keep it out of the human-readable/redacted run journal.
    #[serde(skip_serializing)]
    joined_room_credential: Option<String>,
    /// The parent process's real access code for the joined room. SHARE-N2N's
    /// native test peer is a genuinely separate process with its own
    /// `RoomsState` store (never having seen this room before), so it must
    /// join the same way any second real participant would -- via the real
    /// access code -- not by being handed the bare internal `room-<hex>`
    /// credential. `rooms::room_credential_for_input` (Refs #421/#430)
    /// deliberately refuses to mint a capability for a bare credential a
    /// store has never joined by its real code, so forwarding
    /// `joined_room_credential` alone here fails closed with "room name must
    /// not be empty" -- forwarding the access code instead lets the peer
    /// derive the identical credential via `internal_credential_for_access_code`
    /// (a pure deterministic hash), exactly like the existing unit test
    /// `native_peer_uses_the_parent_joined_capability_across_separate_room_stores`
    /// already validates. Keep it out of the human-readable/redacted run
    /// journal.
    #[serde(skip_serializing)]
    access_code: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RunEvent<'a, T: serde::Serialize> {
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scenario_id: Option<&'a str>,
    payload: T,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactEventPayload {
    #[serde(rename = "type")]
    artifact_type: &'static str,
    path: String,
    step_id: String,
    t_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_id: Option<u32>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HarnessCompatibleScorecard {
    generated_at_unix_ms: u128,
    scenarios: Vec<HarnessCompatibleScenarioResult>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HarnessCompatibleScenarioResult {
    scenario_name: String,
    row_id: Option<String>,
    source_issue: Option<String>,
    coverage_kind: Option<String>,
    participant_count: u32,
    shares_per_bot: u32,
    impairment_profile: String,
    latency: Option<HarnessCompatibleLatencyStats>,
    freeze: HarnessCompatibleFreezeStats,
    delivered_fps: f64,
    delivered_width: u32,
    delivered_height: u32,
    reconnect_ms: Option<f64>,
    evidence_basis: conclusions::EvidenceBasis,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HarnessCompatibleLatencyStats {
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    sample_count: u32,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    by_input: HashMap<String, HarnessCompatibleLatencyStatsByInput>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HarnessCompatibleLatencyStatsByInput {
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    sample_count: u32,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HarnessCompatibleFreezeStats {
    freeze_count: u32,
    longest_freeze_ms: f64,
    total_freeze_ms: f64,
}

struct ResultsWriter {
    dir: PathBuf,
    run_log: BufWriter<File>,
}

impl ResultsWriter {
    fn create(dir: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&dir)
            .map_err(|e| format!("could not create results dir {}: {e}", dir.display()))?;
        let run_log_path = dir.join("run.jsonl");
        let run_log = File::create(&run_log_path)
            .map(BufWriter::new)
            .map_err(|e| format!("could not create {}: {e}", run_log_path.display()))?;
        Ok(Self { dir, run_log })
    }

    fn write<T: serde::Serialize>(
        &mut self,
        kind: &str,
        scenario_id: Option<&str>,
        payload: T,
    ) -> Result<(), String> {
        let value = RunEvent {
            kind,
            scenario_id,
            payload,
        };
        let line = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        let redacted = crate::logging::redact_for_export(&line);
        self.run_log
            .write_all(redacted.as_bytes())
            .and_then(|_| self.run_log.write_all(b"\n"))
            .map_err(|e| format!("could not append run.jsonl: {e}"))
    }

    fn write_scorecard(&mut self, outcomes: &[ScenarioOutcome]) -> Result<(), String> {
        let scorecard = scorecard_from_outcomes(outcomes);
        let json = serde_json::to_string_pretty(&scorecard).map_err(|e| e.to_string())?;
        fs::write(
            self.dir.join("scorecard.json"),
            crate::logging::redact_for_export(&json),
        )
        .map_err(|e| format!("could not write scorecard.json: {e}"))
    }

    fn write_conclusion(
        &mut self,
        outcomes: &[ScenarioOutcome],
        selector: &str,
    ) -> Result<(), String> {
        let verdicts = outcomes
            .iter()
            .filter_map(|outcome| serde_json::to_value(ScenarioVerdictRecord::new(outcome)).ok())
            .collect::<Vec<_>>();
        let scorecard = scorecard_from_outcomes(outcomes);
        let scorecard = serde_json::to_value(scorecard).map_err(|e| e.to_string())?;
        let baseline_comparison = conclusions::compare_baseline(
            self.dir.parent().unwrap_or(&self.dir),
            selector,
            &verdicts,
            &scorecard,
            env!("CARGO_PKG_VERSION"),
        );
        self.write(
            "conclusion",
            None,
            conclusions::from_verdicts_with_baseline(
                &verdicts,
                false,
                Some(baseline_comparison.clone()),
            ),
        )
        .and_then(|_| {
            let json =
                serde_json::to_string_pretty(&baseline_comparison).map_err(|e| e.to_string())?;
            fs::write(
                self.dir.join("baseline-comparison.json"),
                format!("{json}\n"),
            )
            .map_err(|e| format!("could not write baseline-comparison.json: {e}"))
        })
    }

    fn flush(&mut self) {
        let _ = self.run_log.flush();
    }
}

fn scorecard_from_outcomes(outcomes: &[ScenarioOutcome]) -> HarnessCompatibleScorecard {
    HarnessCompatibleScorecard {
        generated_at_unix_ms: now_ms(),
        scenarios: outcomes
            .iter()
            .map(|outcome| HarnessCompatibleScenarioResult {
                scenario_name: outcome.scenario_id.clone(),
                row_id: None,
                source_issue: Some(source_issue_for_scenario(&outcome.scenario_id).to_string()),
                coverage_kind: Some(coverage_kind_for_scenario(&outcome.scenario_id).to_string()),
                participant_count: if outcome.scenario_id == "MULTI-3" {
                    3
                } else {
                    2
                },
                shares_per_bot: if outcome.delivered_fps > 0.0 { 1 } else { 0 },
                impairment_profile: "none".to_string(),
                latency: remote_control_latency_from_outcome(outcome),
                freeze: HarnessCompatibleFreezeStats {
                    freeze_count: 0,
                    longest_freeze_ms: 0.0,
                    total_freeze_ms: 0.0,
                },
                delivered_fps: outcome.delivered_fps,
                delivered_width: outcome.delivered_width,
                delivered_height: outcome.delivered_height,
                reconnect_ms: None,
                evidence_basis: conclusions::for_scenario(&outcome.scenario_id),
            })
            .collect(),
    }
}

fn remote_control_latency_from_outcome(
    outcome: &ScenarioOutcome,
) -> Option<HarnessCompatibleLatencyStats> {
    outcome
        .assertions
        .iter()
        .find(|assertion| assertion.name == "remote-control-latency")
        .and_then(|assertion| serde_json::from_str(&assertion.detail).ok())
}

fn source_issue_for_scenario(scenario_id: &str) -> &'static str {
    if scenario_id == "SHARE-N2N" {
        "#262"
    } else if scenario_id == "CAM-BITRATE" {
        "#246"
    } else if scenario_id == "CAM-STALL" {
        "#247"
    } else if scenario_id == "SHARE-DESKTOP" {
        "#199"
    } else if scenario_id == "SHARE-MULTIWIN"
        || scenario_id == "SHARE-MULTIDISP"
        || scenario_id == "ROOM-JOIN"
        || scenario_id.starts_with("UI-")
    {
        "P-3"
    } else if scenario_id == "RC-N2N" || scenario_id == "RC-N2W" {
        "#819"
    } else if scenario_id == "CAM-N2W" {
        "#815"
    } else if scenario_id == "AUD-N2W" {
        "#812"
    } else if scenario_id == "MULTI-3" || scenario_id == "RC-P1080" {
        "#261"
    } else if scenario_id.starts_with("SOAK-") || scenario_id.ends_with("-SOAK") {
        "#261"
    } else if scenario_id.starts_with("CHAOS-") {
        "#260/#261"
    } else {
        "#257"
    }
}

fn coverage_kind_for_scenario(scenario_id: &str) -> &'static str {
    if scenario_id == "SHARE-N2N" {
        "test-cockpit-native-native"
    } else if scenario_id == "SHARE-MULTIWIN"
        || scenario_id == "SHARE-MULTIDISP"
        || scenario_id == "SHARE-DESKTOP"
        || scenario_id == "CAM-BITRATE"
        || scenario_id == "CAM-STALL"
        || scenario_id == "ROOM-JOIN"
        || scenario_id.starts_with("UI-")
    {
        "test-cockpit-gap-scaffold"
    } else if scenario_id == "MULTI-3" {
        "test-cockpit-multi-peer"
    } else if scenario_id == "RC-P1080" {
        "test-cockpit-remote-control-scaled"
    } else if scenario_id == "RC-N2N" {
        "test-cockpit-native-controller-native-host"
    } else if scenario_id == "RC-N2W" {
        "test-cockpit-native-controller-web-host"
    } else if scenario_id == "CAM-N2W" {
        "test-cockpit-native-to-web-camera"
    } else if scenario_id == "AUD-N2W" {
        "test-cockpit-native-to-web-audio"
    } else if scenario_id.starts_with("SOAK-") || scenario_id.ends_with("-SOAK") {
        "test-cockpit-soak"
    } else if scenario_id.starts_with("CHAOS-") {
        "test-cockpit-full"
    } else {
        "test-cockpit-quick"
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn artifact_name_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "artifact".to_string()
    } else {
        out
    }
}

fn screenshot_artifact_relative_path(scenario_id: &str, step_id: &str, window_id: u32) -> PathBuf {
    PathBuf::from("artifacts").join(format!(
        "{}-{}-window-{}.png",
        artifact_name_component(scenario_id),
        artifact_name_component(step_id),
        window_id
    ))
}

fn video_artifact_relative_path(scenario_id: &str, step_id: &str, window_id: u32) -> PathBuf {
    PathBuf::from("artifacts").join(format!(
        "{}-{}-window-{}.mov",
        artifact_name_component(scenario_id),
        artifact_name_component(step_id),
        window_id
    ))
}

fn audio_artifact_relative_path(scenario_id: &str, step_id: &str) -> PathBuf {
    PathBuf::from("artifacts").join(format!(
        "{}-{}-tone.m4a",
        artifact_name_component(scenario_id),
        artifact_name_component(step_id)
    ))
}

fn audio_artifact_temp_wav_relative_path(scenario_id: &str, step_id: &str) -> PathBuf {
    PathBuf::from("artifacts").join(format!(
        "{}-{}-tone.wav",
        artifact_name_component(scenario_id),
        artifact_name_component(step_id)
    ))
}

fn write_wav_pcm16_mono(path: &Path, samples: &[i16], sample_rate: u32) -> Result<(), String> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or("audio snippet is too large to write as wav")?;
    let riff_len = 36u32
        .checked_add(data_len)
        .ok_or("audio snippet wav header would overflow")?;
    let mut file = File::create(path).map_err(|error| {
        format!(
            "could not create audio artifact wav {}: {error}",
            path.display()
        )
    })?;
    file.write_all(b"RIFF")
        .and_then(|_| file.write_all(&riff_len.to_le_bytes()))
        .and_then(|_| file.write_all(b"WAVE"))
        .and_then(|_| file.write_all(b"fmt "))
        .and_then(|_| file.write_all(&16u32.to_le_bytes()))
        .and_then(|_| file.write_all(&1u16.to_le_bytes()))
        .and_then(|_| file.write_all(&(AUDIO_ARTIFACT_CHANNELS as u16).to_le_bytes()))
        .and_then(|_| file.write_all(&sample_rate.to_le_bytes()))
        .and_then(|_| {
            let byte_rate = sample_rate * AUDIO_ARTIFACT_CHANNELS * 2;
            file.write_all(&byte_rate.to_le_bytes())
        })
        .and_then(|_| {
            let block_align = (AUDIO_ARTIFACT_CHANNELS * 2) as u16;
            file.write_all(&block_align.to_le_bytes())
        })
        .and_then(|_| file.write_all(&16u16.to_le_bytes()))
        .and_then(|_| file.write_all(b"data"))
        .and_then(|_| file.write_all(&data_len.to_le_bytes()))
        .map_err(|error| format!("could not write audio artifact wav header: {error}"))?;
    for sample in samples {
        file.write_all(&sample.to_le_bytes())
            .map_err(|error| format!("could not write audio artifact sample data: {error}"))?;
    }
    Ok(())
}

fn find_current_remote_audio_track(
    app: &AppHandle,
) -> Result<livekit::prelude::RemoteAudioTrack, String> {
    let state = app
        .try_state::<crate::session::SessionState>()
        .ok_or("session state is unavailable for audio artifact capture")?;
    let (room_connection, _) = state
        .control_channel_snapshot()
        .ok_or("not joined to a room for audio artifact capture")?;
    for participant in room_connection.room().remote_participants().values() {
        for publication in participant.track_publications().values() {
            if let Some(RemoteTrack::Audio(audio)) = publication.track() {
                return Ok(audio);
            }
        }
    }
    Err("no subscribed remote audio track found for audio artifact capture".to_string())
}

async fn collect_audio_artifact_samples(
    audio_track: livekit::prelude::RemoteAudioTrack,
) -> Result<Vec<i16>, String> {
    let mut stream = NativeAudioStream::new(
        audio_track.rtc_track(),
        AUDIO_ARTIFACT_SAMPLE_RATE as i32,
        AUDIO_ARTIFACT_CHANNELS as i32,
    );
    let target_samples = (AUDIO_ARTIFACT_SAMPLE_RATE * AUDIO_ARTIFACT_SECONDS) as usize;
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs((AUDIO_ARTIFACT_SECONDS + 3) as u64);
    let mut samples = Vec::with_capacity(target_samples);

    while samples.len() < target_samples {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break };
                if frame.num_channels != AUDIO_ARTIFACT_CHANNELS {
                    return Err(format!(
                        "expected mono decoded audio, got {} channel(s)",
                        frame.num_channels
                    ));
                }
                samples.extend_from_slice(&frame.data);
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }

    if samples.is_empty() {
        return Err("subscribed audio track produced zero decoded PCM samples".to_string());
    }
    samples.truncate(target_samples);
    Ok(samples)
}

/// Capture the decoded PCM for the current remote audio track, measure its
/// energy, and (best-effort) write the .m4a artifact from those same samples.
///
/// #787: the samples are captured and measured FIRST, and the energy is
/// returned to the caller, so the `AUD` oracle can assert on what was actually
/// decoded. The artifact conversion is downstream of that on purpose -- a
/// missing `afconvert` must not be able to swallow the verdict, which is
/// exactly what happened before: this function returned `()` and recorded its
/// own failures as a side note nothing ever read.
async fn record_audio_snippet_artifact(
    app: &AppHandle,
    writer: &mut ResultsWriter,
    scenario: ScenarioSpec,
    step_id: &str,
) -> Result<crate::transport::audio::DecodedPcmEnergy, String> {
    let relative_path = audio_artifact_relative_path(scenario.id, step_id);
    let temp_wav_relative_path = audio_artifact_temp_wav_relative_path(scenario.id, step_id);
    let output_path = writer.dir.join(&relative_path);
    let temp_wav_path = writer.dir.join(&temp_wav_relative_path);

    let audio_track = find_current_remote_audio_track(app)?;
    let samples = collect_audio_artifact_samples(audio_track).await?;
    let energy = crate::transport::audio::decoded_pcm_energy(&samples);
    let _ = writer.write(
        "audio-energy",
        Some(scenario.id),
        serde_json::json!({
            "stepId": step_id,
            "samples": energy.samples,
            "peakAbs": energy.peak_abs,
            "nonzeroSamples": energy.nonzero_samples,
            "audible": energy.is_audible(),
        }),
    );

    let result = async {
        if !Path::new(AFCONVERT_PATH).is_file() {
            return Err(format!(
                "{AFCONVERT_PATH} is not available for .m4a artifact conversion"
            ));
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "could not create audio artifact directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        write_wav_pcm16_mono(&temp_wav_path, &samples, AUDIO_ARTIFACT_SAMPLE_RATE)?;
        let _ = fs::remove_file(&output_path);
        let output = Command::new(AFCONVERT_PATH)
            .arg("-f")
            .arg("m4af")
            .arg("-d")
            .arg("aac")
            .arg(&temp_wav_path)
            .arg(&output_path)
            .output()
            .map_err(|error| format!("could not run afconvert for audio artifact: {error}"))?;
        let _ = fs::remove_file(&temp_wav_path);
        if !output.status.success() {
            return Err(format!(
                "afconvert failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let size = output_path
            .metadata()
            .map_err(|error| format!("could not stat converted audio artifact: {error}"))?
            .len();
        if size == 0 {
            return Err("converted audio artifact is empty".to_string());
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            let _ = writer.write(
                "artifact",
                Some(scenario.id),
                ArtifactEventPayload {
                    artifact_type: "audio",
                    path: relative_path.display().to_string(),
                    step_id: step_id.to_string(),
                    t_ms: now_ms(),
                    window_id: None,
                },
            );
        }
        Err(error) => {
            let _ = fs::remove_file(&temp_wav_path);
            let _ = writer.write(
                "artifact-error",
                Some(scenario.id),
                serde_json::json!({
                    "type": "audio",
                    "stepId": step_id,
                    "path": relative_path.display().to_string(),
                    "error": error,
                }),
            );
        }
    }

    Ok(energy)
}

struct WindowVideoArtifact {
    stream: screencapturekit::stream::SCStream,
    recording: screencapturekit::recording_output::SCRecordingOutput,
    finished: mpsc::Receiver<Result<(), String>>,
    relative_path: PathBuf,
    window_id: u32,
}

impl WindowVideoArtifact {
    fn stop_and_record(self, writer: &mut ResultsWriter, scenario: ScenarioSpec, step_id: &str) {
        let stop_result = self
            .stream
            .stop_capture()
            .map_err(|error| format!("stop_capture failed: {error}"))
            .and_then(|_| {
                self.stream
                    .remove_recording_output(&self.recording)
                    .map_err(|error| format!("remove_recording_output failed: {error}"))
            });
        let finish_result = self
            .finished
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| Err("recording did not finish within 5s".to_string()));
        let output_path = writer.dir.join(&self.relative_path);
        let file_size = output_path.metadata().ok().map(|metadata| metadata.len());
        match (stop_result, finish_result, file_size) {
            (Ok(()), Ok(()), Some(size)) if size > 0 => {
                let _ = writer.write(
                    "artifact",
                    Some(scenario.id),
                    ArtifactEventPayload {
                        artifact_type: "video",
                        path: self.relative_path.display().to_string(),
                        step_id: step_id.to_string(),
                        t_ms: now_ms(),
                        window_id: Some(self.window_id),
                    },
                );
            }
            (stop_result, finish_result, file_size) => {
                let _ = writer.write(
                    "artifact-error",
                    Some(scenario.id),
                    serde_json::json!({
                        "type": "video",
                        "stepId": step_id,
                        "windowId": self.window_id,
                        "path": self.relative_path.display().to_string(),
                        "stopError": stop_result.err(),
                        "finishError": finish_result.err(),
                        "fileSize": file_size,
                    }),
                );
            }
        }
    }
}

fn start_window_video_artifact(
    writer: &mut ResultsWriter,
    scenario: ScenarioSpec,
    step_id: &str,
    window_id: u32,
) -> Option<WindowVideoArtifact> {
    let relative_path = video_artifact_relative_path(scenario.id, step_id, window_id);
    let output_path = writer.dir.join(&relative_path);
    if let Some(parent) = output_path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            let _ = writer.write(
                "artifact-error",
                Some(scenario.id),
                serde_json::json!({
                    "type": "video",
                    "stepId": step_id,
                    "windowId": window_id,
                    "error": format!("could not create artifact directory: {error}"),
                }),
            );
            return None;
        }
    }

    let content = match screencapturekit::shareable_content::SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
    {
        Ok(content) => content,
        Err(error) => {
            let _ = writer.write(
                "artifact-error",
                Some(scenario.id),
                serde_json::json!({
                    "type": "video",
                    "stepId": step_id,
                    "windowId": window_id,
                    "error": format!("could not list shareable content: {error}"),
                }),
            );
            return None;
        }
    };
    let Some(window) = content
        .windows()
        .into_iter()
        .find(|window| window.window_id() == window_id)
    else {
        let _ = writer.write(
            "artifact-error",
            Some(scenario.id),
            serde_json::json!({
                "type": "video",
                "stepId": step_id,
                "windowId": window_id,
                "error": "window not found for video artifact recording",
            }),
        );
        return None;
    };
    let filter = screencapturekit::stream::content_filter::SCContentFilter::create()
        .with_window(&window)
        .build();
    let frame = window.frame();
    let scale = f64::from(filter.point_pixel_scale()).max(1.0);
    let width = (frame.size.width.round().max(1.0) * scale).round().max(1.0) as u32;
    let height = (frame.size.height.round().max(1.0) * scale)
        .round()
        .max(1.0) as u32;
    let config = screencapturekit::stream::configuration::SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_shows_cursor(false)
        .with_queue_depth(8)
        .with_fps(30);
    let recording_config =
        screencapturekit::recording_output::SCRecordingOutputConfiguration::new()
            .with_output_url(&output_path)
            .with_video_codec(screencapturekit::recording_output::SCRecordingOutputCodec::H264)
            .with_output_file_type(
                screencapturekit::recording_output::SCRecordingOutputFileType::MOV,
            );
    let (tx, rx) = mpsc::channel();
    let fail_tx = tx.clone();
    let callbacks = screencapturekit::recording_output::RecordingCallbacks::new()
        .on_finish(move || {
            let _ = tx.send(Ok(()));
        })
        .on_fail(move |error| {
            let _ = fail_tx.send(Err(error));
        });
    let Some(recording) = screencapturekit::recording_output::SCRecordingOutput::new_with_delegate(
        &recording_config,
        callbacks,
    ) else {
        let _ = writer.write(
            "artifact-skip",
            Some(scenario.id),
            serde_json::json!({
                "type": "video",
                "stepId": step_id,
                "windowId": window_id,
                "reason": "ScreenCaptureKit recording output is unavailable on this system",
            }),
        );
        return None;
    };
    let stream = screencapturekit::stream::SCStream::new(&filter, &config);
    if let Err(error) = stream.add_recording_output(&recording) {
        let _ = writer.write(
            "artifact-error",
            Some(scenario.id),
            serde_json::json!({
                "type": "video",
                "stepId": step_id,
                "windowId": window_id,
                "error": format!("could not add recording output: {error}"),
            }),
        );
        return None;
    }
    if let Err(error) = stream.start_capture() {
        let _ = writer.write(
            "artifact-error",
            Some(scenario.id),
            serde_json::json!({
                "type": "video",
                "stepId": step_id,
                "windowId": window_id,
                "error": format!("could not start recording stream: {error}"),
            }),
        );
        return None;
    }
    let _ = writer.write(
        "artifact-recording-start",
        Some(scenario.id),
        serde_json::json!({
            "type": "video",
            "stepId": step_id,
            "windowId": window_id,
            "path": relative_path.display().to_string(),
            "width": width,
            "height": height,
        }),
    );
    Some(WindowVideoArtifact {
        stream,
        recording,
        finished: rx,
        relative_path,
        window_id,
    })
}

fn record_window_screenshot_artifact(
    writer: &mut ResultsWriter,
    scenario: ScenarioSpec,
    step_id: &str,
    window_id: u32,
) {
    let relative_path = screenshot_artifact_relative_path(scenario.id, step_id, window_id);
    let output_path = writer.dir.join(&relative_path);
    match crate::test_cockpit_bridge::capture_window_pixels(
        window_id,
        None,
        Some(output_path.display().to_string()),
    ) {
        Ok(CaptureWindowPixelsResult { path: Some(_), .. }) => {
            let _ = writer.write(
                "artifact",
                Some(scenario.id),
                ArtifactEventPayload {
                    artifact_type: "screenshot",
                    path: relative_path.display().to_string(),
                    step_id: step_id.to_string(),
                    t_ms: now_ms(),
                    window_id: Some(window_id),
                },
            );
        }
        Ok(result) => {
            let _ = writer.write(
                "artifact-skip",
                Some(scenario.id),
                serde_json::json!({
                    "type": "screenshot",
                    "stepId": step_id,
                    "windowId": window_id,
                    "reason": result.reason.unwrap_or_else(|| "capture skipped".to_string()),
                }),
            );
        }
        Err(error) => {
            let _ = writer.write(
                "artifact-error",
                Some(scenario.id),
                serde_json::json!({
                    "type": "screenshot",
                    "stepId": step_id,
                    "windowId": window_id,
                    "error": error,
                }),
            );
        }
    }
}

fn test_pattern_content_check(path: &Path) -> Result<(), String> {
    let image = image::open(path)
        .map_err(|error| format!("could not decode captured frame: {error}"))?
        .to_rgb8();
    if image.width() < 960 || image.height() < 600 {
        return Err(format!(
            "captured frame is {}x{}, expected at least 960x600",
            image.width(),
            image.height()
        ));
    }
    // #499: this check's coordinates are all defined against the logical
    // 960x600 test-pattern canvas (TEST_PATTERN_SOURCE_WIDTH/HEIGHT), but
    // `capture_window_pixels`/ScreenCaptureKit captures at the DISPLAY'S
    // NATIVE PIXEL SCALE -- 2x on any Retina display, which is the default
    // on real dev hardware. A hardcoded (28,28) landed on this machine's
    // capture (1920x1200, confirmed live) inside plain background, not the
    // calibration square, which only starts around physical (34,34) at 2x --
    // silently reading the wrong pixel rather than failing on frame size,
    // since 1920x1200 already clears the >=960x600 floor above. Scale every
    // logical coordinate by the capture's actual size instead of assuming
    // 1x, so this passes identically on any display scale factor.
    let scale_x = image.width() as f64 / TEST_PATTERN_SOURCE_WIDTH;
    let scale_y = image.height() as f64 / TEST_PATTERN_SOURCE_HEIGHT;
    let scaled = |x: f64, y: f64| -> (u32, u32) {
        ((x * scale_x).round() as u32, (y * scale_y).round() as u32)
    };
    // #499: even after landing on the right pixel, a real ScreenCaptureKit ->
    // PNG round trip does not reproduce a canvas fillRect's color byte-exact
    // -- confirmed live, consistently, across repeated captures of the same
    // static frame (same input always produces the same output, ruling out
    // capture noise/timing): the two calibration squares nearer the window's
    // right edge showed a real, solid, reproducible tint (e.g. the green
    // square's should-be-zero red channel reading 117, not sensor noise or
    // antialiasing -- a uniform block, not a gradient at the edge). Exact
    // equality was never going to survive a real display/compositor/codec
    // path; a bounded color-distance tolerance still catches what actually
    // matters here (frozen frame, wrong pattern entirely, blank capture)
    // while tolerating legitimate capture-pipeline color reproduction error.
    const COLOR_DISTANCE_TOLERANCE: f64 = 140.0;
    let expected = [
        (28.0, 28.0, [255, 45, 85]),
        (932.0, 28.0, [0, 255, 136]),
        (28.0, 572.0, [45, 125, 255]),
        (932.0, 572.0, [255, 212, 0]),
    ];
    for (x, y, color) in expected {
        let (px, py) = scaled(x, y);
        let pixel = image.get_pixel(px, py).0;
        let distance = pixel
            .iter()
            .zip(color.iter())
            .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
            .sum::<f64>()
            .sqrt();
        if distance > COLOR_DISTANCE_TOLERANCE {
            return Err(format!(
                "calibration square missing at ({x},{y}) [captured px ({px},{py}) in a {}x{} frame]: observed {:?}, expected {:?}, distance={distance:.1}",
                image.width(), image.height(), pixel, color
            ));
        }
    }
    let samples = [
        (480.0, 100.0),
        (360.0, 240.0),
        (480.0, 300.0),
        (600.0, 360.0),
        (480.0, 500.0),
    ];
    let scaled_samples: Vec<(u32, u32)> = samples.iter().map(|(x, y)| scaled(*x, *y)).collect();
    let first = image.get_pixel(scaled_samples[0].0, scaled_samples[0].1).0;
    if scaled_samples
        .iter()
        .all(|(x, y)| image.get_pixel(*x, *y).0 == first)
    {
        return Err("captured test-pattern frame is uniform".to_string());
    }
    Ok(())
}

fn resolve_scenarios(selector: &str) -> Result<Vec<ScenarioSpec>, String> {
    let normalized = selector.trim();
    if ["quick", "full", "soak"]
        .iter()
        .any(|tier| normalized.eq_ignore_ascii_case(tier))
    {
        let tier = if normalized.eq_ignore_ascii_case("quick") {
            "quick"
        } else if normalized.eq_ignore_ascii_case("full") {
            "full"
        } else {
            "soak"
        };
        return Ok(SCENARIO_TABLE
            .iter()
            .copied()
            .filter(|scenario| scenario.tier == tier)
            .collect());
    }
    // Single-token feature/priority/depth selectors (e.g. "A", "screen-sharing",
    // "p0", "short") expand to the deduped runnable scenarios of the matching
    // journeys. Commas mean an explicit id list, so only try this for a bare token.
    if !normalized.contains(',') {
        if let Some(scenarios) = resolve_journey_group_selector(normalized) {
            if scenarios.is_empty() {
                let matched: Vec<&str> = JOURNEY_TABLE
                    .iter()
                    .filter(|journey| {
                        // Re-derive the same axis match purely for the error
                        // message; unwrap is safe because the selector already
                        // parsed once above.
                        token_axes_match(normalized, journey)
                    })
                    .map(|journey| journey.id)
                    .collect();
                return Err(format!(
                    "selector '{normalized}' matched only gap journeys with no runnable scenario yet: [{}] -- see docs/TEST_PLAN.md's gap list",
                    matched.join(", ")
                ));
            }
            return Ok(scenarios);
        }
    }
    // Otherwise a comma list where each token is a journey id (mapped to its
    // runnable scenario) or a legacy runnable scenario id.
    let mut scenarios: Vec<ScenarioSpec> = Vec::new();
    for raw_id in normalized.split(',') {
        let id = raw_id.trim();
        if id.is_empty() {
            continue;
        }
        let scenario = if let Some(journey) = journey_by_id(id) {
            let runnable = journey.runnable.ok_or_else(|| {
                format!(
                    "journey '{}' is not runnable yet (status: {})",
                    journey.id, journey.status
                )
            })?;
            scenario_by_id(runnable).ok_or_else(|| {
                format!(
                    "journey '{}' points at unknown scenario '{runnable}'",
                    journey.id
                )
            })?
        } else {
            scenario_by_id(id)
                .ok_or_else(|| format!("unknown test cockpit scenario or journey '{id}'"))?
        };
        if !scenarios.iter().any(|existing| existing.id == scenario.id) {
            scenarios.push(scenario);
        }
    }
    if scenarios.is_empty() {
        Err("test case selector did not resolve to any scenarios".to_string())
    } else {
        Ok(scenarios)
    }
}

fn rand_hex() -> String {
    format!("{:x}{:x}", now_ms(), std::process::id())
}

fn cockpit_room_name() -> String {
    format!("rctest-{}", rand_hex())
}

fn cockpit_identity() -> String {
    format!("p-cockpit-{}", rand_hex())
}

fn harness_url() -> String {
    std::env::var("PETAL_HARNESS_URL").unwrap_or_else(|_| "https://meet.petal.live".to_string())
}

fn backend_url() -> String {
    std::env::var("PETAL_BACKEND_URL").unwrap_or_else(|_| "https://app.petal.live".to_string())
}

fn assert_cockpit_room(room_name: &str) -> Result<(), String> {
    if room_name.starts_with("rctest-") {
        Ok(())
    } else {
        Err(format!(
            "INFRA-FAIL: test cockpit refused non-rctest room '{room_name}'"
        ))
    }
}

/// Used to validate/log the parent's canonical capability and to gate
/// whether a real access code exists at all -- not to hand the peer this
/// bare credential directly. The peer is a genuinely separate process/
/// `RoomsState` store, so it joins via the real access code instead (see
/// `run_native_to_native_scenario`'s `room_name` binding); a bare label or
/// bare internal credential can never substitute for it across stores.
fn joined_room_credential(record: &crate::rooms::RoomRecord) -> Result<String, String> {
    crate::rooms::normalize_room_credential(&record.name)
        .ok_or_else(|| "joined cockpit room did not expose a canonical room capability".to_string())
}

fn detect_tool_in_path(name: &str, path: Option<&OsStr>) -> bool {
    if name.contains('/') || name.is_empty() {
        return false;
    }
    let Some(path) = path else {
        return false;
    };
    env::split_paths(path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file() && is_executable_file(&candidate)
    })
}

fn detect_tool(name: &str) -> bool {
    detect_tool_in_path(name, env::var_os("PATH").as_deref())
}

fn net_impair_script_path_from_manifest_dir(manifest_dir: &Path) -> Option<PathBuf> {
    manifest_dir
        .ancestors()
        .map(|dir| dir.join(NET_IMPAIR_SCRIPT_RELATIVE_PATH))
        .find(|candidate| candidate.is_file())
}

fn net_impair_script_path() -> Option<PathBuf> {
    net_impair_script_path_from_manifest_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn skipped_outcome(scenario: ScenarioSpec, reason: impl Into<String>) -> ScenarioOutcome {
    let reason = reason.into();
    ScenarioOutcome {
        scenario_id: scenario.id.to_string(),
        verdict: ScenarioVerdict::Skipped,
        message: format!("{} SKIPPED {reason}", scenario.id),
        delivered_fps: 0.0,
        delivered_width: 0,
        delivered_height: 0,
        assertions: vec![AssertionOutcome {
            name: "skip-classification".to_string(),
            passed: true,
            detail: reason,
        }],
    }
}

/// True when the console session is locked (lock screen / login window).
/// A locked console composites no app windows: SCK delivers idle no-buffer
/// samples forever (a peer's `start_share` waits on a first real frame that
/// never comes) and compositor panels report zero-sized WindowServer
/// geometry. Both surfaced as baffling timeouts on the first RC-N2N/RC-N2W
/// live attempts -- name the condition instead. Fail-open: this exists for a
/// clearer message, not as a gate.
#[cfg(target_os = "macos")]
fn console_is_locked() -> bool {
    std::process::Command::new("/usr/sbin/ioreg")
        .args(["-n", "Root", "-d1", "-a"])
        .output()
        .ok()
        .and_then(|out| {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            text.split("IOConsoleLocked")
                .nth(1)
                .map(|rest| rest[..rest.len().min(80)].contains("<true/>"))
        })
        .unwrap_or(false)
}

fn infra_fail_outcome(scenario: ScenarioSpec, reason: impl Into<String>) -> ScenarioOutcome {
    let reason = reason.into();
    ScenarioOutcome {
        scenario_id: scenario.id.to_string(),
        verdict: ScenarioVerdict::InfraFail,
        message: format!("{} INFRA-FAIL {reason}", scenario.id),
        delivered_fps: 0.0,
        delivered_width: 0,
        delivered_height: 0,
        assertions: vec![AssertionOutcome {
            name: "infra-precondition".to_string(),
            passed: false,
            detail: reason,
        }],
    }
}

struct WebPeer {
    child: Option<Child>,
    mode: &'static str,
    url: String,
}

impl WebPeer {
    fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(std::process::Child::id)
    }
}

impl Drop for WebPeer {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn chrome_path() -> PathBuf {
    std::env::var("PETAL_CHROME_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

/// Build the Chrome invocation, forcing it to run as the host's own
/// architecture on Apple Silicon. Found live (2026-07-16, MULTI-3/
/// CHAOS-DEVICE investigation): a plain `Command::new(chrome_path())` can
/// launch Chrome's x86_64 slice under Rosetta even on an arm64 host
/// (confirmed by the "Rosetta... neither tested nor maintained" banner in
/// the resulting chrome log), and headless Chrome running under Rosetta
/// throttles its own render loop badly enough to fail the web-harness
/// self-check (`selfCheckPatternAdvancing` in `web-harness/src/cockpit.ts`)
/// -- which every green run's log lacked and every failing run's log had.
/// `arch -arm64 <path>` forces the native slice; there is nothing to force
/// on an Intel host, where the plain path is already correct.
fn chrome_command(chrome: &Path) -> Command {
    if cfg!(target_arch = "aarch64") {
        let mut command = Command::new("arch");
        command.arg("-arm64").arg(chrome);
        command
    } else {
        Command::new(chrome)
    }
}

fn spawn_web_peer(
    scenario: ScenarioSpec,
    access_code: &str,
    results_dir: &Path,
) -> Result<WebPeer, String> {
    spawn_web_peer_labeled(scenario, access_code, results_dir, None)
}

fn spawn_web_peer_labeled(
    scenario: ScenarioSpec,
    access_code: &str,
    results_dir: &Path,
    label: Option<&str>,
) -> Result<WebPeer, String> {
    spawn_web_peer_labeled_with_cdp(scenario, access_code, results_dir, label, false)
}

fn spawn_web_peer_with_cdp(
    scenario: ScenarioSpec,
    access_code: &str,
    results_dir: &Path,
) -> Result<WebPeer, String> {
    spawn_web_peer_labeled_with_cdp(scenario, access_code, results_dir, None, true)
}

fn spawn_web_peer_labeled_with_cdp(
    scenario: ScenarioSpec,
    access_code: &str,
    results_dir: &Path,
    label: Option<&str>,
    cdp_enabled: bool,
) -> Result<WebPeer, String> {
    let url = format!(
        "{}/?code={}&auto={}",
        harness_url().trim_end_matches('/'),
        access_code,
        scenario.id.to_ascii_lowercase()
    );
    let chrome = chrome_path();
    if chrome.exists() {
        let log_name = label
            .map(|label| format!("chrome-{}-{label}.log", scenario.id))
            .unwrap_or_else(|| format!("chrome-{}.log", scenario.id));
        let log_path = results_dir.join(log_name);
        let log = File::create(&log_path)
            .map_err(|e| format!("could not create {}: {e}", log_path.display()))?;
        let err = log
            .try_clone()
            .map_err(|e| format!("could not clone chrome log handle: {e}"))?;
        let profile_name = label
            .map(|label| {
                format!(
                    "petal-cockpit-chrome-{}-{label}-{}",
                    scenario.id,
                    rand_hex()
                )
            })
            .unwrap_or_else(|| format!("petal-cockpit-chrome-{}", rand_hex()));
        let user_data = std::env::temp_dir().join(profile_name);
        // AUD-N2W must run HEADED. Headless Chrome has no audio output device,
        // so it never decodes a remote audio track at all: `packetsReceived`
        // climbs while `totalSamplesReceived` and `jitterBufferEmittedCount`
        // stay 0 and `totalSamplesDuration` advances on the playout clock.
        // Measured 2026-08-15: a native tone a real Chrome renders at rms 0.35
        // read exactly 0.0000 headless, through BOTH the inbound-rtp stats and
        // a MediaRecorder capture -- a silence-shaped instrument failure in the
        // one scenario built to detect silence (#821). Positioned off-screen to
        // keep it out of the way -- note a headed launch still ACTIVATES
        // Chrome, so it does take key focus briefly; that was not engineered
        // away and no flag removes it.
        //
        // This is an AUDIO-only limitation. Headless Chrome decodes VIDEO
        // normally, so CAM-N2W and every other video scenario stay headless
        // (#815) -- do not widen the headed branch to them, or every video
        // run starts stealing the user's key focus for nothing.
        let needs_audio_decode = matches!(scenario.kind, ScenarioKind::AudioNativeToWeb);
        let mut command = chrome_command(&chrome);
        if needs_audio_decode {
            command.args(["--window-position=-3000,0", "--window-size=900,700"]);
        } else {
            command.arg("--headless=new");
        }
        // A first headed launch against a fresh --user-data-dir can raise the
        // macOS "Chrome Safe Storage" keychain prompt, which hangs an
        // unattended run. Headless never hits it. Harmless in both modes.
        command.arg("--use-mock-keychain");
        let child = command
            .args([
                "--disable-renderer-backgrounding",
                "--disable-background-timer-throttling",
                "--disable-backgrounding-occluded-windows",
                // Headless Chrome suspends AudioContext without a user
                // gesture, turning the AUD tone into published SILENCE
                // (measured 2026-08-15: kbps=0.0, 144k zero samples -- a
                // harness artifact initially misread as #787). The web side
                // also refuses to publish from a non-running context now;
                // this flag makes the resume actually succeed.
                "--autoplay-policy=no-user-gesture-required",
                "--no-first-run",
                "--no-default-browser-check",
            ])
            .args(cdp_enabled.then_some("--remote-debugging-port=9222"))
            .arg(format!("--user-data-dir={}", user_data.display()))
            .arg(&url)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .spawn()
            .map_err(|e| format!("INFRA-FAIL: could not launch Google Chrome: {e}"))?;
        return Ok(WebPeer {
            child: Some(child),
            // Truthful: this string lands in the run's `web-peer` evidence
            // record, and the headed/headless split is the whole point of the
            // AUD-N2W fix. Evidence that lies about the one thing a change
            // altered is its own bug class.
            mode: if needs_audio_decode {
                "headed-chrome-offscreen"
            } else {
                "headless-chrome"
            },
            url,
        });
    }

    if cdp_enabled {
        return Err(
            "INFRA-FAIL: Chrome is required for RC-P1080's auto-provisioned CDP web peer"
                .to_string(),
        );
    }
    let status = Command::new("open").arg(&url).status().map_err(|e| {
        format!("INFRA-FAIL: Google Chrome missing and default-browser fallback failed: {e}")
    })?;
    if status.success() {
        Ok(WebPeer {
            child: None,
            mode: "default-browser",
            url,
        })
    } else {
        Err(format!(
            "INFRA-FAIL: Google Chrome missing and default-browser fallback exited with {status}"
        ))
    }
}

fn cockpit_harness_url_match() -> String {
    if let Ok(value) = env::var("PETAL_WEB_HARNESS_URL_MATCH") {
        return value;
    }
    let url = harness_url();
    let host_and_path = url
        .split_once("//")
        .map_or(url.as_str(), |(_, value)| value);
    host_and_path
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(target_os = "macos")]
    {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        false
    }
}

#[derive(Default)]
struct RunChildren {
    web_peer_pids: Vec<u32>,
    native_peer_pids: Vec<u32>,
}

impl RunChildren {
    fn record_web_peer(&mut self, peer: &WebPeer) {
        if let Some(pid) = peer.pid() {
            self.web_peer_pids.push(pid);
        }
    }

    fn record_native_peer(&mut self, child: &Child) {
        self.native_peer_pids.push(child.id());
    }
}

fn recv_window_track(
    snapshot: &crate::diagnostics::NetworkSnapshot,
) -> Option<&crate::diagnostics::TrackHealth> {
    snapshot.tracks.iter().find(|track| {
        track.direction == "recv"
            && track.name.starts_with("petal-window-")
            && track.kind == "video"
    })
}

fn recv_camera_track(
    snapshot: &crate::diagnostics::NetworkSnapshot,
) -> Option<&crate::diagnostics::TrackHealth> {
    snapshot.tracks.iter().find(|track| {
        track.direction == "recv"
            && track.kind == "video"
            && track
                .name
                .starts_with(crate::transport::publisher::CAMERA_TRACK_PREFIX)
    })
}

fn sent_camera_track(
    snapshot: &crate::diagnostics::NetworkSnapshot,
) -> Option<&crate::diagnostics::TrackHealth> {
    snapshot.tracks.iter().find(|track| {
        track.direction == "send"
            && track.kind == "video"
            && track
                .name
                .starts_with(crate::transport::publisher::CAMERA_TRACK_PREFIX)
    })
}

fn sent_audio_track(
    snapshot: &crate::diagnostics::NetworkSnapshot,
) -> Option<&crate::diagnostics::TrackHealth> {
    snapshot
        .tracks
        .iter()
        .find(|track| track.direction == "send" && track.kind == "audio")
}

fn recv_audio_track(
    snapshot: &crate::diagnostics::NetworkSnapshot,
) -> Option<&crate::diagnostics::TrackHealth> {
    snapshot
        .tracks
        .iter()
        .find(|track| track.direction == "recv" && track.kind == "audio")
}

#[derive(Debug, Clone, PartialEq)]
struct WebCockpitReport {
    sender: String,
    payload: serde_json::Value,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MultiPeerRosterReport {
    reporter_id: String,
    ok: bool,
    participant_count: usize,
    remote_participant_count: usize,
    roster_fingerprint: String,
    roster_fingerprint_algorithm: String,
    roster_includes_reporter: bool,
    roster_unique: bool,
}

#[derive(Debug, Clone)]
struct MultiPeerNativeEvidence {
    roster_before: Vec<String>,
    roster_after: Vec<String>,
    menubar_in_meeting: bool,
    menubar_participant_count: u32,
    clock_calibration_ok: Option<bool>,
    keyframe_storm_free: Option<bool>,
}

fn normalized_roster(mut identities: Vec<String>) -> Vec<String> {
    identities.retain(|identity| !identity.trim().is_empty());
    identities.sort();
    identities.dedup();
    identities
}

fn roster_fingerprint(identities: &[String]) -> String {
    let canonical = serde_json::to_vec(identities).expect("string roster always serializes");
    format!("{:x}", Sha256::digest(canonical))
}

fn parse_web_cockpit_report_line(message: &str) -> Option<WebCockpitReport> {
    let prefix = "test-cockpit report from '";
    let rest = message.strip_prefix(prefix)?;
    let (sender, payload) = rest.split_once("': ")?;
    let payload = serde_json::from_str(payload).ok()?;
    Some(WebCockpitReport {
        sender: sender.to_string(),
        payload,
    })
}

fn report_text_field<'a>(payload: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    payload.get(field).and_then(serde_json::Value::as_str)
}

fn report_matches_scenario(report: &WebCockpitReport, scenario: ScenarioSpec) -> bool {
    let scenario_id = report_text_field(&report.payload, "scenarioId")
        .or_else(|| report_text_field(&report.payload, "scenario"))
        .or_else(|| report_text_field(&report.payload, "auto"));
    let step = report_text_field(&report.payload, "step");
    scenario_id
        .map(|id| id.eq_ignore_ascii_case(scenario.id))
        .unwrap_or_else(|| {
            step.map(|step| step.eq_ignore_ascii_case(scenario.id))
                .unwrap_or(false)
        })
}

fn report_ok(payload: &serde_json::Value) -> bool {
    payload
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            report_text_field(payload, "classification")
                .map(|value| value.eq_ignore_ascii_case("PASS"))
        })
        .or_else(|| {
            report_text_field(payload, "status").map(|value| value.eq_ignore_ascii_case("pass"))
        })
        .unwrap_or(false)
}

fn report_number(payload: &serde_json::Value, fields: &[&str]) -> Option<f64> {
    for field in fields {
        if let Some(number) = payload.get(*field).and_then(serde_json::Value::as_f64) {
            return Some(number);
        }
        if let Some(number) = payload
            .get("stats")
            .and_then(|stats| stats.get(*field))
            .and_then(serde_json::Value::as_f64)
        {
            return Some(number);
        }
    }
    None
}

fn web_report_outcome(scenario: ScenarioSpec, report: &WebCockpitReport) -> ScenarioOutcome {
    let ok = report_ok(&report.payload);
    let fps = report_number(&report.payload, &["fps", "deliveredFps", "decodedFps"]).unwrap_or(0.0);
    let width = report_number(&report.payload, &["width", "deliveredWidth"]).unwrap_or(0.0) as u32;
    let height =
        report_number(&report.payload, &["height", "deliveredHeight"]).unwrap_or(0.0) as u32;
    let detail = report_text_field(&report.payload, "detail")
        .or_else(|| report_text_field(&report.payload, "message"))
        .unwrap_or("web harness reported scenario result");
    let passed = match scenario.kind {
        // Liveness (frames advancing) at the correct source resolution proves the
        // native->web share pipeline delivered live video. Raw fps is
        // self-capture-limited for the WKWebView test source (see N2W_LIVENESS_FPS)
        // -- gate on liveness + dimensions, not a 30fps quality bar this source
        // can't reach in a CLI-driven run.
        // The web peer compares decoded dimensions against its actual v2
        // device-pixel demand. Fixed source-size floors would reject a valid,
        // network-conscious lower layer that still exceeds the displayed
        // pixels. Native only adds the independent advancing-frame check.
        ScenarioKind::NativeToWebShare => ok && fps > N2W_LIVENESS_FPS,
        _ => ok,
    };
    ScenarioOutcome {
        scenario_id: scenario.id.to_string(),
        verdict: if passed {
            ScenarioVerdict::Pass
        } else {
            ScenarioVerdict::TestFail
        },
        message: if passed {
            format!("{} PASS {detail}", scenario.id)
        } else {
            format!("{} TEST-FAIL {detail}", scenario.id)
        },
        delivered_fps: fps,
        delivered_width: width,
        delivered_height: height,
        assertions: vec![AssertionOutcome {
            name: "web-cockpit-report".to_string(),
            passed,
            detail: format!("sender={} payload={}", report.sender, report.payload),
        }],
    }
}

fn journal_messages_contain_pair<I, S>(messages: I, begin: &str, end: &str) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut saw_begin = false;
    for message in messages {
        let message = message.as_ref();
        if message.contains(begin) {
            saw_begin = true;
        }
        if saw_begin && message.contains(end) {
            return true;
        }
    }
    false
}

async fn assert_web_to_native_video(
    app: &AppHandle,
    scenario: ScenarioSpec,
    writer: &mut ResultsWriter,
) -> ScenarioOutcome {
    let deadline = Instant::now() + ASSERT_TIMEOUT;
    let mut good_reads = 0;
    let mut last_fps = 0.0;
    let mut last_width = 0;
    let mut last_height = 0;
    let mut assertions = Vec::new();

    while Instant::now() < deadline {
        if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
            let snapshot = diagnostics.snapshot();
            let _ = writer.write("metrics", Some(scenario.id), &snapshot);
            if let Some(track) = recv_window_track(&snapshot) {
                last_fps = track.fps;
                last_width = track.width;
                last_height = track.height;
                if track.fps > FPS_THRESHOLD && track.stream_state == "active" {
                    good_reads += 1;
                    assertions.push(AssertionOutcome {
                        name: "recv-window-active-fps".to_string(),
                        passed: true,
                        detail: format!(
                            "{} fps={:.1} streamState={} ({good_reads}/{REQUIRED_GOOD_READS})",
                            track.name, track.fps, track.stream_state
                        ),
                    });
                    if good_reads >= REQUIRED_GOOD_READS {
                        return ScenarioOutcome {
                            scenario_id: scenario.id.to_string(),
                            verdict: ScenarioVerdict::Pass,
                            message: format!(
                                "{} PASS fps={:.1} size={}x{}",
                                scenario.id, track.fps, track.width, track.height
                            ),
                            delivered_fps: track.fps,
                            delivered_width: track.width,
                            delivered_height: track.height,
                            assertions,
                        };
                    }
                } else {
                    good_reads = 0;
                    assertions.push(AssertionOutcome {
                        name: "recv-window-active-fps".to_string(),
                        passed: false,
                        detail: format!(
                            "{} fps={:.1} streamState={} below threshold",
                            track.name, track.fps, track.stream_state
                        ),
                    });
                }
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    ScenarioOutcome {
        scenario_id: scenario.id.to_string(),
        verdict: ScenarioVerdict::TestFail,
        message: format!(
            "{} TEST-FAIL no active recv petal-window-* track above fps>{FPS_THRESHOLD} within {:?}",
            scenario.id, ASSERT_TIMEOUT
        ),
        delivered_fps: last_fps,
        delivered_width: last_width,
        delivered_height: last_height,
        assertions,
    }
}

/// A report's step is "terminal" when it carries the scenario's outcome: the
/// web peer emits `join` -> `<action>` -> `done` (or `scenario`/`aborted` on
/// failure). The action/`done` reports carry the result fields (e.g. delivered
/// fps for NativeToWebShare); `join` does not. Returning early on the FIRST
/// matching report (always `join`) made NativeToWebShare read fps=0 from the
/// join line AND tore the scenario down before the web peer could even receive
/// the native share. So we must wait for a terminal report.
fn is_terminal_report_step(payload: &serde_json::Value) -> bool {
    match report_text_field(payload, "step") {
        Some(step) => {
            let step = step.to_ascii_lowercase();
            step == "done"
                || step == "scenario"
                || step == "aborted"
                || (step == "join" && !report_ok(payload))
        }
        // A consolidated report with no per-step field is itself terminal.
        None => true,
    }
}

async fn await_web_report(
    app: &AppHandle,
    scenario: ScenarioSpec,
    writer: &mut ResultsWriter,
) -> Option<WebCockpitReport> {
    let deadline = Instant::now() + WEB_REPORT_TIMEOUT;
    let mut newest: Option<WebCockpitReport> = None;
    while Instant::now() < deadline {
        if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
            for entry in diagnostics.journal().into_iter().rev() {
                let Some(report) = parse_web_cockpit_report_line(&entry.message) else {
                    continue;
                };
                if report_matches_scenario(&report, scenario) {
                    // `journal()` is oldest-first, iterated in reverse, so this is
                    // the most-recent matching report. If it's terminal, that's
                    // the scenario outcome -- return it. Otherwise remember it and
                    // keep waiting for the terminal (`done`/`scenario`) report.
                    if is_terminal_report_step(&report.payload) {
                        let _ = writer.write(
                            "web-report",
                            Some(scenario.id),
                            serde_json::json!({
                                "sender": report.sender,
                                "payload": report.payload,
                                "journalCategory": entry.category,
                                "journalTimestampMs": entry.t_ms,
                                "terminal": true,
                            }),
                        );
                        return Some(report);
                    }
                    newest = Some(report);
                    break;
                }
            }
        }
        let _ = writer.write(
            "web-report-poll",
            Some(scenario.id),
            "waiting for terminal petal.cockpit report",
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Timed out without a terminal report. Record the newest non-terminal
    // report for debugging, but do NOT return it as an assertion input: a lone
    // `join ok:true` must never combine with unrelated native telemetry and
    // produce a false scenario PASS.
    if let Some(report) = &newest {
        let _ = writer.write(
            "web-report",
            Some(scenario.id),
            serde_json::json!({
                "sender": report.sender,
                "payload": report.payload,
                "terminal": false,
                "note": "WEB_REPORT_TIMEOUT elapsed before a terminal step",
            }),
        );
    }
    None
}

fn collect_distinct_terminal_reports_from_journal<I, S>(
    messages: I,
    scenario: ScenarioSpec,
    expected: usize,
) -> Vec<WebCockpitReport>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut reports = Vec::new();
    let mut senders = HashSet::new();
    for message in messages {
        let Some(report) = parse_web_cockpit_report_line(message.as_ref()) else {
            continue;
        };
        if !report_matches_scenario(&report, scenario) || !is_terminal_report_step(&report.payload)
        {
            continue;
        }
        if senders.insert(report.sender.clone()) {
            reports.push(report);
            if reports.len() >= expected {
                break;
            }
        }
    }
    reports
}

async fn await_distinct_web_reports(
    app: &AppHandle,
    scenario: ScenarioSpec,
    writer: &mut ResultsWriter,
    expected: usize,
) -> Vec<WebCockpitReport> {
    let deadline = Instant::now() + WEB_REPORT_TIMEOUT;
    let mut reports = Vec::new();
    while Instant::now() < deadline {
        if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
            let journal = diagnostics.journal();
            reports = collect_distinct_terminal_reports_from_journal(
                journal.iter().rev().map(|entry| entry.message.as_str()),
                scenario,
                expected,
            );
            if reports.len() >= expected {
                let _ = writer.write(
                    "web-report",
                    Some(scenario.id),
                    serde_json::json!({
                        "senders": reports.iter().map(|report| report.sender.clone()).collect::<Vec<_>>(),
                        "count": reports.len(),
                        "expected": expected,
                        "terminal": true,
                    }),
                );
                return reports;
            }
        }
        let _ = writer.write(
            "web-report-poll",
            Some(scenario.id),
            format!("waiting for {expected} distinct terminal petal.cockpit reports"),
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = writer.write(
        "web-report",
        Some(scenario.id),
        serde_json::json!({
            "senders": reports.iter().map(|report| report.sender.clone()).collect::<Vec<_>>(),
            "count": reports.len(),
            "expected": expected,
            "terminal": false,
            "note": "WEB_REPORT_TIMEOUT elapsed before all distinct terminal reports arrived",
        }),
    );
    reports
}

#[cfg(target_os = "macos")]
async fn start_native_test_pattern_share(
    app: &AppHandle,
    scenario: ScenarioSpec,
    writer: &mut ResultsWriter,
) -> Result<NativeTestPatternShare, String> {
    let frontend_provenance = verified_cockpit_frontend_provenance()?;
    let _ = writer.write(
        "cockpit-frontend-provenance",
        Some(scenario.id),
        serde_json::json!({
            "embedded": true,
            "provenance": frontend_provenance,
            "requiredAssets": ["dev/test-pattern.html", "dev/test-pattern-status.html"],
        }),
    );
    let deadline_epoch_ms = unix_epoch_ms()? + u128::from(NATIVE_TEST_PATTERN_PREPARE_SECS) * 1000;
    crate::dev_test_pattern::open_test_pattern_window_for_cockpit(
        app.clone(),
        crate::dev_test_pattern::CockpitTestPatternPhase::Prepare { deadline_epoch_ms },
    )
    .map_err(|error| format!("INFRA-FAIL opening native test-pattern window: {error}"))?;
    let _ = writer.write(
        "native-test-pattern-phase",
        Some(scenario.id),
        serde_json::json!({
            "phase": "PREPARE",
            "seconds": NATIVE_TEST_PATTERN_PREPARE_SECS,
            "operator": "keep the test-pattern window frontmost until capture is locked",
        }),
    );
    await_test_pattern_prepare_or_cancel(app, scenario, writer).await?;

    crate::dev_test_pattern::set_cockpit_test_pattern_phase(
        app.clone(),
        crate::dev_test_pattern::CockpitTestPatternPhase::Starting,
    )
    .map_err(|error| format!("INFRA-FAIL starting native test-pattern capture: {error}"))?;
    let _ = writer.write(
        "native-test-pattern-phase",
        Some(scenario.id),
        serde_json::json!({
            "phase": "STARTING",
            "operator": "capture setup is starting; keep the test-pattern window frontmost",
        }),
    );
    tokio::time::sleep(Duration::from_millis(750)).await;

    let entries = match crate::platform::cg::onscreen_windows() {
        Some(entries) => entries,
        None => {
            let _ = crate::dev_test_pattern::set_cockpit_test_pattern_phase(
                app.clone(),
                crate::dev_test_pattern::CockpitTestPatternPhase::Failed {
                    detail: "window-enumeration-failed",
                },
            );
            return Err(
                "INFRA-FAIL could not enumerate on-screen windows for test-pattern share"
                    .to_string(),
            );
        }
    };
    let entry = entries
        .into_iter()
        .find(|entry| entry.name.contains("Petal — Test Pattern") && entry.number > 0)
        .ok_or_else(|| {
            let _ = crate::dev_test_pattern::set_cockpit_test_pattern_phase(
                app.clone(),
                crate::dev_test_pattern::CockpitTestPatternPhase::Failed {
                    detail: "source-window-not-visible",
                },
            );
            "INFRA-FAIL native test-pattern window was not visible to CoreGraphics".to_string()
        })?;
    let window_id = u32::try_from(entry.number).map_err(|_| {
        let _ = crate::dev_test_pattern::set_cockpit_test_pattern_phase(
            app.clone(),
            crate::dev_test_pattern::CockpitTestPatternPhase::Failed {
                detail: "invalid-window-id",
            },
        );
        format!("INFRA-FAIL invalid test-pattern window id {}", entry.number)
    })?;
    // `toggle_share_for_window` normally hands the foreground away after a
    // successful share. This QA-owned source is the one exception: preserve it
    // visibly on screen until SHARE-N2N has sampled the independent-move
    // oracle. The registration is feature-gated and keyed by this exact
    // CGWindowID, never by an arbitrary app-owned window.
    let visible_source = register_cockpit_visible_source(window_id);
    let frame = crate::platform::cg::frame_for_window_id(window_id).unwrap_or(
        crate::platform::cg::WindowFrame {
            x: entry.x as i32,
            y: entry.y as i32,
            width: entry.w as i32,
            height: entry.h as i32,
        },
    );
    let _ = writer.write(
        "native-share-source",
        Some(scenario.id),
        serde_json::json!({
            "windowId": window_id,
            "title": entry.name,
            "owner": entry.owner_name,
            "frame": {
                "x": frame.x,
                "y": frame.y,
                "width": frame.width,
                "height": frame.height,
            },
        }),
    );
    let state = app.state::<crate::session::SessionState>();
    // A window opened <1s ago is often not yet listed by SCShareableContent
    // (documented truncation/latency -- see window_source.rs), so start_share's
    // direct-window lookup can miss it and fail with "window not found in
    // current shareable content". Retry the share a few times; each attempt
    // re-queries SCShareableContent, and toggle_share_for_window is retry-safe
    // (a failed start never marks the window active, so this re-attempts START,
    // never toggles OFF). ~4s total budget before giving up.
    let mut shared = false;
    for attempt in 1..=8 {
        let toggle = toggle_after_native_test_pattern_readiness(
            ensure_native_test_pattern_readiness(app, writer, scenario, window_id).await?,
            || crate::hover_tab::toggle_share_for_window(app, state.inner(), window_id, frame),
        )?;
        shared = toggle.await;
        if shared {
            if attempt > 1 {
                log::info!(
                    "test-cockpit: native test-pattern window {window_id} entered shared state on attempt {attempt}"
                );
            }
            break;
        }
        log::warn!(
            "test-cockpit: native test-pattern share attempt {attempt}/8 for window {window_id} not yet shareable; retrying"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !shared {
        let detail = "share-failed";
        let _ = crate::dev_test_pattern::set_cockpit_test_pattern_phase(
            app.clone(),
            crate::dev_test_pattern::CockpitTestPatternPhase::Failed { detail },
        );
        let _ = writer.write(
            "native-test-pattern-phase",
            Some(scenario.id),
            serde_json::json!({ "phase": "FAILED", "detail": detail }),
        );
        return Err(format!(
            "INFRA-FAIL native test-pattern window {window_id} did not enter shared state after retries"
        ));
    }
    let capture_active = shared && state.inner().is_share_active(window_id);
    if let Some(phase) = capture_lock_phase_after_share(capture_active) {
        crate::dev_test_pattern::set_cockpit_test_pattern_phase(app.clone(), phase).map_err(
            |error| format!("INFRA-FAIL locking active native test-pattern capture: {error}"),
        )?;
        let _ = writer.write(
            "native-test-pattern-phase",
            Some(scenario.id),
            serde_json::json!({
                "phase": "CAPTURE_LOCKED",
                "operator": "capture is active; keep the test-pattern window frontmost",
            }),
        );
    } else {
        let detail = "share-not-active";
        let _ = crate::dev_test_pattern::set_cockpit_test_pattern_phase(
            app.clone(),
            crate::dev_test_pattern::CockpitTestPatternPhase::Failed { detail },
        );
        let _ = writer.write(
            "native-test-pattern-phase",
            Some(scenario.id),
            serde_json::json!({ "phase": "FAILED", "detail": detail }),
        );
        return Err(
            "INFRA-FAIL test-pattern toggle returned without an active capture state".to_string(),
        );
    }
    Ok(NativeTestPatternShare {
        window_id,
        _visible_source: visible_source,
    })
}

#[cfg(not(target_os = "macos"))]
async fn start_native_test_pattern_share(
    _app: &AppHandle,
    _scenario: ScenarioSpec,
    _writer: &mut ResultsWriter,
) -> Result<NativeTestPatternShare, String> {
    Err("INFRA-FAIL native test-pattern sharing is macOS-only".to_string())
}

fn report_payload_bool(payload: &serde_json::Value, fields: &[&str]) -> bool {
    fields
        .iter()
        .any(|field| payload.get(*field).and_then(serde_json::Value::as_bool) == Some(true))
}

fn validate_scenario_web_report(
    scenario: ScenarioSpec,
    report: &WebCockpitReport,
) -> Result<(), String> {
    if !report_ok(&report.payload) {
        return Err(format!(
            "web harness terminal report for {} was not ok: {}",
            scenario.id, report.payload
        ));
    }
    let missing_marker = match scenario.kind {
        ScenarioKind::Camera => !report_payload_bool(&report.payload, &["cameraPublished"]),
        ScenarioKind::CameraNativeToWeb => {
            !report_payload_bool(&report.payload, &["remoteCameraVisible"])
        }
        ScenarioKind::Audio => !report_payload_bool(&report.payload, &["audioPublished"]),
        ScenarioKind::AudioNativeToWeb => {
            !report_payload_bool(&report.payload, &["remoteAudioAudible"])
        }
        ScenarioKind::Draw => !report_payload_bool(
            &report.payload,
            &[
                "strokeDelivered",
                "strokeDeliveryLogged",
                "beginEndDelivered",
            ],
        ),
        ScenarioKind::Telepointer => !report_payload_bool(
            &report.payload,
            &["telepointerMoved", "pointerMoved", "positionChanged"],
        ),
        _ => false,
    };
    if missing_marker {
        return Err(format!(
            "web harness report for {} did not include the required scenario-specific marker: {}",
            scenario.id, report.payload
        ));
    }
    Ok(())
}

/// A web report that classifies ITSELF as an infrastructure failure must not be
/// verdicted as a product failure. The web side sets this when it could not
/// measure at all (e.g. a browser that cannot decode remote audio) -- treating
/// that as TEST-FAIL is what turned a blind receiver into a P0 bug report
/// against a working product (#821).
fn web_report_declares_infra_failure(report: &WebCockpitReport) -> bool {
    report_text_field(&report.payload, "classification")
        .is_some_and(|value| value.eq_ignore_ascii_case("INFRA-FAIL"))
}

fn failed_native_assertion_outcome(
    scenario: ScenarioSpec,
    report: &WebCockpitReport,
    assertion: &str,
    detail: String,
) -> ScenarioOutcome {
    ScenarioOutcome {
        scenario_id: scenario.id.to_string(),
        verdict: ScenarioVerdict::TestFail,
        message: format!("{} TEST-FAIL {detail}", scenario.id),
        delivered_fps: 0.0,
        delivered_width: 0,
        delivered_height: 0,
        assertions: vec![
            AssertionOutcome {
                name: "web-cockpit-report".to_string(),
                passed: report_ok(&report.payload),
                detail: report.payload.to_string(),
            },
            AssertionOutcome {
                name: assertion.to_string(),
                passed: false,
                detail,
            },
        ],
    }
}

async fn assert_reported_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    report: &WebCockpitReport,
    writer: &mut ResultsWriter,
) -> ScenarioOutcome {
    match scenario.kind {
        ScenarioKind::NativeToWebShare => web_report_outcome(scenario, report),
        ScenarioKind::Camera => {
            if let Err(detail) = validate_scenario_web_report(scenario, report) {
                return failed_native_assertion_outcome(
                    scenario,
                    report,
                    "web-camera-report",
                    format!("{detail}; refusing to trust generic web ok=true"),
                );
            }
            let deadline = Instant::now() + ASSERT_TIMEOUT;
            while Instant::now() < deadline {
                if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
                    let snapshot = diagnostics.snapshot();
                    let _ = writer.write("metrics", Some(scenario.id), &snapshot);
                    if let Some(track) = recv_camera_track(&snapshot) {
                        if track.fps > 0.0 || track.frames_decoded > 0 {
                            return ScenarioOutcome {
                                scenario_id: scenario.id.to_string(),
                                verdict: ScenarioVerdict::Pass,
                                message: format!(
                                    "{} PASS camera recv track active fps={:.1} framesDecoded={}",
                                    scenario.id, track.fps, track.frames_decoded
                                ),
                                delivered_fps: track.fps,
                                delivered_width: track.width,
                                delivered_height: track.height,
                                assertions: vec![
                                    AssertionOutcome {
                                        name: "web-cockpit-report".to_string(),
                                        passed: report_ok(&report.payload),
                                        detail: report.payload.to_string(),
                                    },
                                    AssertionOutcome {
                                        name: "native-camera-telemetry".to_string(),
                                        passed: true,
                                        detail: format!("{} fps={:.1}", track.name, track.fps),
                                    },
                                ],
                            };
                        }
                    }
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            failed_native_assertion_outcome(
                scenario,
                report,
                "native-camera-telemetry",
                format!(
                    "camera publication was not observed in native recv telemetry within {:?}; refusing to trust web ok=true alone",
                    ASSERT_TIMEOUT
                ),
            )
        }
        ScenarioKind::CameraNativeToWeb => {
            // Native send telemetry FIRST, reported on pass and on fail alike:
            // when the web viewer says "nothing visible", the only question
            // that matters next is which side produced it, and a failure
            // message that cannot answer that costs a whole rebuild cycle.
            let mut native_send = None;
            let deadline = Instant::now() + ASSERT_TIMEOUT;
            while Instant::now() < deadline {
                if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
                    let snapshot = diagnostics.snapshot();
                    let _ = writer.write("metrics", Some(scenario.id), &snapshot);
                    if let Some(track) = sent_camera_track(&snapshot) {
                        native_send = Some(format!(
                            "{} state={} kbps={:.1}",
                            track.name, track.stream_state, track.actual_kbps
                        ));
                        if track.actual_kbps > 0.0 || track.stream_state == "active" {
                            break;
                        }
                    }
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            let native_send = native_send.unwrap_or_else(|| "NO SEND CAMERA TRACK".to_string());

            if let Err(detail) = validate_scenario_web_report(scenario, report) {
                if web_report_declares_infra_failure(report) {
                    return infra_fail_outcome(
                        scenario,
                        format!(
                            "the web viewer could not MEASURE the camera tile (not the same thing as seeing nothing): {detail}. Native send side at the same moment: {native_send}"
                        ),
                    );
                }
                return failed_native_assertion_outcome(
                    scenario,
                    report,
                    "web-remote-camera-report",
                    format!(
                        "{detail}; the web viewer did not report a visible, advancing camera tile. Native send side at the same moment: {native_send}"
                    ),
                );
            }
            let fps = report_number(&report.payload, &["remoteCameraFps"]).unwrap_or(0.0);
            let width = report_number(&report.payload, &["remoteCameraWidth"]).unwrap_or(0.0);
            let height = report_number(&report.payload, &["remoteCameraHeight"]).unwrap_or(0.0);
            let non_black =
                report_number(&report.payload, &["remoteCameraNonBlackRatio"]).unwrap_or(0.0);
            if native_send == "NO SEND CAMERA TRACK" {
                return failed_native_assertion_outcome(
                    scenario,
                    report,
                    "native-camera-publish-telemetry",
                    format!(
                        "the web viewer reported a visible camera tile ({width:.0}x{height:.0} at {fps:.1}fps) but no native SEND camera track appeared in telemetry within {ASSERT_TIMEOUT:?} -- refusing to credit this run to a camera it cannot prove published (is PETAL_CAMERA_SYNTH_SOURCE=1 set, or a real camera available?)"
                    ),
                );
            }
            ScenarioOutcome {
                scenario_id: scenario.id.to_string(),
                verdict: ScenarioVerdict::Pass,
                message: format!(
                    "{} PASS native camera published ({native_send}) and the web viewer measured advancing non-black frames ({width:.0}x{height:.0} at {fps:.1}fps, nonBlackRatio={non_black:.2})",
                    scenario.id
                ),
                delivered_fps: fps,
                delivered_width: width.max(0.0) as u32,
                delivered_height: height.max(0.0) as u32,
                assertions: vec![
                    AssertionOutcome {
                        name: "native-camera-publish-telemetry".to_string(),
                        passed: true,
                        detail: native_send,
                    },
                    AssertionOutcome {
                        name: "web-remote-camera-frames".to_string(),
                        passed: true,
                        detail: report.payload.to_string(),
                    },
                ],
            }
        }
        ScenarioKind::AudioNativeToWeb => {
            // Native send telemetry is read FIRST, and reported whether the
            // run passes or fails. When the web listener says "silence", the
            // only question that matters next is which side produced it --
            // a failure message that cannot answer that sends the reader back
            // for another 4-minute rebuild (it did, twice, on 2026-08-15).
            let mut native_send = None;
            let deadline = Instant::now() + ASSERT_TIMEOUT;
            while Instant::now() < deadline {
                if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
                    let snapshot = diagnostics.snapshot();
                    let _ = writer.write("metrics", Some(scenario.id), &snapshot);
                    if let Some(track) = sent_audio_track(&snapshot) {
                        native_send = Some(format!(
                            "{} state={} kbps={:.1}",
                            track.name, track.stream_state, track.actual_kbps
                        ));
                        if track.actual_kbps > 0.0 || track.stream_state == "active" {
                            break;
                        }
                    }
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            let native_send = native_send.unwrap_or_else(|| "NO SEND AUDIO TRACK".to_string());

            if let Err(detail) = validate_scenario_web_report(scenario, report) {
                if web_report_declares_infra_failure(report) {
                    return infra_fail_outcome(
                        scenario,
                        format!(
                            "the web listener could not MEASURE audio (not the same thing as hearing none): {detail}. Native send side at the same moment: {native_send}"
                        ),
                    );
                }
                return failed_native_assertion_outcome(
                    scenario,
                    report,
                    "web-remote-audio-report",
                    format!(
                        "{detail}; the web listener did not report audible received audio. Native send side at the same moment: {native_send}"
                    ),
                );
            }
            let rms = report_number(&report.payload, &["remoteAudioRms"]).unwrap_or(0.0);
            let decoded_seconds =
                report_number(&report.payload, &["remoteAudioDurationDelta"]).unwrap_or(0.0);
            if native_send == "NO SEND AUDIO TRACK" {
                return failed_native_assertion_outcome(
                    scenario,
                    report,
                    "native-audio-publish-telemetry",
                    format!(
                        "the web listener reported audible audio (rms={rms:.4} over {decoded_seconds:.2}s decoded) but no native SEND audio track appeared in telemetry within {ASSERT_TIMEOUT:?} -- refusing to credit this run to a mic it cannot prove published (is PETAL_DISABLE_AUDIO unset or 0?)"
                    ),
                );
            }
            ScenarioOutcome {
                scenario_id: scenario.id.to_string(),
                verdict: ScenarioVerdict::Pass,
                message: format!(
                    "{} PASS native mic published ({native_send}) and the web listener measured audible audio (rms={rms:.4} over {decoded_seconds:.2}s decoded)",
                    scenario.id
                ),
                delivered_fps: 0.0,
                delivered_width: 0,
                delivered_height: 0,
                assertions: vec![
                    AssertionOutcome {
                        name: "native-audio-publish-telemetry".to_string(),
                        passed: true,
                        detail: native_send,
                    },
                    AssertionOutcome {
                        name: "web-remote-audio-energy".to_string(),
                        passed: true,
                        detail: report.payload.to_string(),
                    },
                ],
            }
        }
        ScenarioKind::Audio => {
            if let Err(detail) = validate_scenario_web_report(scenario, report) {
                return failed_native_assertion_outcome(
                    scenario,
                    report,
                    "web-audio-report",
                    format!("{detail}; refusing to trust generic web ok=true"),
                );
            }
            let deadline = Instant::now() + ASSERT_TIMEOUT;
            while Instant::now() < deadline {
                if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
                    let snapshot = diagnostics.snapshot();
                    let _ = writer.write("metrics", Some(scenario.id), &snapshot);
                    if let Some(track) = recv_audio_track(&snapshot) {
                        if track.actual_kbps > 0.0 || track.stream_state == "active" {
                            // #787: RTP arrival is NOT the verdict. This gate
                            // used to return Pass right here, which is why it
                            // passed green through an entire meeting in which
                            // the native listener heard nothing -- packets
                            // arriving says nothing about whether anything was
                            // decoded. The scenario only passes if the decoded
                            // PCM actually carries energy.
                            let energy = match record_audio_snippet_artifact(
                                app, writer, scenario, "verdict",
                            )
                            .await
                            {
                                Ok(energy) => energy,
                                Err(error) => {
                                    return failed_native_assertion_outcome(
                                            scenario,
                                            report,
                                            "native-audio-pcm-energy",
                                            format!(
                                                "recv telemetry was healthy ({} state={} kbps={:.1}) but the decoded PCM could not be captured: {error}",
                                                track.name, track.stream_state, track.actual_kbps
                                            ),
                                        );
                                }
                            };
                            if !energy.is_audible() {
                                return failed_native_assertion_outcome(
                                    scenario,
                                    report,
                                    "native-audio-pcm-energy",
                                    format!(
                                        "recv telemetry was healthy ({} state={} kbps={:.1}) but the subscribed track decoded to SILENCE ({}) -- audio arrived and was not audible (#787)",
                                        track.name,
                                        track.stream_state,
                                        track.actual_kbps,
                                        energy.summary()
                                    ),
                                );
                            }
                            return ScenarioOutcome {
                                scenario_id: scenario.id.to_string(),
                                verdict: ScenarioVerdict::Pass,
                                message: format!(
                                    "{} PASS audio recv track active kbps={:.1}, decoded PCM audible ({})",
                                    scenario.id,
                                    track.actual_kbps,
                                    energy.summary()
                                ),
                                delivered_fps: 0.0,
                                delivered_width: 0,
                                delivered_height: 0,
                                assertions: vec![
                                    AssertionOutcome {
                                        name: "web-cockpit-report".to_string(),
                                        passed: report_ok(&report.payload),
                                        detail: report.payload.to_string(),
                                    },
                                    AssertionOutcome {
                                        name: "native-audio-telemetry".to_string(),
                                        passed: true,
                                        detail: format!(
                                            "{} state={} kbps={:.1}",
                                            track.name, track.stream_state, track.actual_kbps
                                        ),
                                    },
                                    AssertionOutcome {
                                        name: "native-audio-pcm-energy".to_string(),
                                        passed: true,
                                        detail: energy.summary(),
                                    },
                                ],
                            };
                        }
                    }
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            failed_native_assertion_outcome(
                scenario,
                report,
                "native-audio-telemetry",
                format!(
                    "audio publication was not observed in native recv telemetry within {:?}; refusing to trust web ok=true alone",
                    ASSERT_TIMEOUT
                ),
            )
        }
        ScenarioKind::Draw => {
            let journal = app
                .try_state::<crate::diagnostics::DiagnosticsState>()
                .map(|diagnostics| diagnostics.journal())
                .unwrap_or_default();
            let native_logged = journal_messages_contain_pair(
                journal.iter().map(|entry| entry.message.as_str()),
                "draw: delivered Begin stroke",
                "draw: delivered End stroke",
            );
            let web_asserted = report_payload_bool(
                &report.payload,
                &[
                    "strokeDelivered",
                    "strokeDeliveryLogged",
                    "beginEndDelivered",
                ],
            );
            let mut outcome = web_report_outcome(scenario, report);
            let passed = report_ok(&report.payload) && web_asserted && native_logged;
            outcome.verdict = if passed {
                ScenarioVerdict::Pass
            } else {
                ScenarioVerdict::TestFail
            };
            outcome.message = if passed {
                format!("{} PASS draw stroke delivery observed", scenario.id)
            } else {
                format!(
                    "{} TEST-FAIL draw stroke delivery was not observed",
                    scenario.id
                )
            };
            outcome.assertions.push(AssertionOutcome {
                name: "draw-stroke-delivery".to_string(),
                passed,
                detail: format!(
                    "nativeLogged={native_logged} webAsserted={web_asserted}; native log evidence is required"
                ),
            });
            outcome
        }
        ScenarioKind::Telepointer => {
            let journal = app
                .try_state::<crate::diagnostics::DiagnosticsState>()
                .map(|diagnostics| diagnostics.journal())
                .unwrap_or_default();
            let native_logged = journal.iter().any(|entry| {
                entry.message.contains("telepointer") || entry.message.contains("remote pointer")
            });
            let web_asserted = report_payload_bool(
                &report.payload,
                &["telepointerMoved", "pointerMoved", "positionChanged"],
            );
            let mut outcome = web_report_outcome(scenario, report);
            let passed = report_ok(&report.payload) && web_asserted && native_logged;
            outcome.verdict = if passed {
                ScenarioVerdict::Pass
            } else {
                ScenarioVerdict::TestFail
            };
            outcome.message = if passed {
                format!("{} PASS telepointer movement observed", scenario.id)
            } else {
                format!(
                    "{} TEST-FAIL telepointer movement was not observed",
                    scenario.id
                )
            };
            outcome.assertions.push(AssertionOutcome {
                name: "telepointer-delivery".to_string(),
                passed,
                detail: format!(
                    "nativeLogged={native_logged} webAsserted={web_asserted}; native log evidence is required"
                ),
            });
            outcome
        }
        ScenarioKind::WebToNativeShare => assert_web_to_native_video(app, scenario, writer).await,
        ScenarioKind::ChaosDevice
        | ScenarioKind::ChaosDisplayChange
        | ScenarioKind::ChaosNet
        | ScenarioKind::ChaosLifecycle
        | ScenarioKind::MultiPeer
        | ScenarioKind::RemoteControlScaled
        | ScenarioKind::SoakStallWatch
        | ScenarioKind::NativeToNativeShare
        | ScenarioKind::RemoteControlNativeToNative
        | ScenarioKind::RemoteControlNativeToWeb
        | ScenarioKind::MultiWindowShare
        | ScenarioKind::MultiDisplayShare
        | ScenarioKind::FullDesktopShare
        | ScenarioKind::CameraBitrateScaling
        | ScenarioKind::CameraStall
        | ScenarioKind::JoinRoom
        | ScenarioKind::UiScreenshot => infra_fail_outcome(
            scenario,
            "non-generic scenario was routed through web-report assertions unexpectedly",
        ),
    }
}

fn available_display_count(app: &AppHandle) -> Option<usize> {
    app.available_monitors().ok().map(|monitors| monitors.len())
}

fn chaos_device_outcome_from_report(
    scenario: ScenarioSpec,
    report: &WebCockpitReport,
    switch_audio_available: bool,
) -> ScenarioOutcome {
    let camera_disappeared = report_payload_bool(&report.payload, &["cameraDisappeared"]);
    let passed = report_ok(&report.payload) && camera_disappeared;
    ScenarioOutcome {
        scenario_id: scenario.id.to_string(),
        verdict: if passed {
            ScenarioVerdict::Pass
        } else {
            ScenarioVerdict::TestFail
        },
        message: if passed {
            format!(
                "{} PASS synthetic camera disappeared; audio-device switch {}",
                scenario.id,
                if switch_audio_available {
                    "not attempted in this nondestructive pass"
                } else {
                    "skipped because SwitchAudioSource is not installed"
                }
            )
        } else {
            format!(
                "{} TEST-FAIL synthetic camera disappearance was not confirmed",
                scenario.id
            )
        },
        delivered_fps: 0.0,
        delivered_width: 0,
        delivered_height: 0,
        assertions: vec![
            AssertionOutcome {
                name: "web-cockpit-report".to_string(),
                passed: report_ok(&report.payload),
                detail: report.payload.to_string(),
            },
            AssertionOutcome {
                name: "camera-disappearance".to_string(),
                passed: camera_disappeared,
                detail: format!("payload={}", report.payload),
            },
            AssertionOutcome {
                name: "audio-device-switch".to_string(),
                passed: true,
                detail: if switch_audio_available {
                    "SwitchAudioSource is installed, but the destructive audio-device switch is not attempted in this nondestructive cockpit pass".to_string()
                } else {
                    "SKIPPED(tooling): SwitchAudioSource is not installed".to_string()
                },
            },
        ],
    }
}

async fn run_chaos_device_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    access_code: &str,
    writer: &mut ResultsWriter,
    children: &mut RunChildren,
) -> ScenarioOutcome {
    let switch_audio_available = detect_tool("SwitchAudioSource");
    let _ = writer.write(
        "chaos-preflight",
        Some(scenario.id),
        serde_json::json!({
            "tool": "SwitchAudioSource",
            "available": switch_audio_available,
            "subcases": ["audio-device-switch", "camera-disappearance"],
        }),
    );

    let web_peer = match spawn_web_peer(scenario, access_code, &writer.dir) {
        Ok(peer) => peer,
        Err(error) => return infra_fail_outcome(scenario, error),
    };
    children.record_web_peer(&web_peer);
    let _ = writer.write(
        "web-peer",
        Some(scenario.id),
        serde_json::json!({
            "mode": web_peer.mode,
            "url": web_peer.url,
            "pid": web_peer.pid(),
            "cameraDisappearanceExpected": true,
            "audioDeviceSwitchAttempted": false,
            "audioDeviceSwitchAvailable": switch_audio_available,
        }),
    );

    let Some(report) = await_web_report(app, scenario, writer).await else {
        return infra_fail_outcome(
            scenario,
            "web harness did not report the CHAOS-DEVICE camera disappearance result",
        );
    };
    chaos_device_outcome_from_report(scenario, &report, switch_audio_available)
}

async fn run_chaos_display_change_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    writer: &mut ResultsWriter,
) -> ScenarioOutcome {
    let displayplacer_available = detect_tool("displayplacer");
    let display_count = available_display_count(app).unwrap_or(0);
    let _ = writer.write(
        "chaos-preflight",
        Some(scenario.id),
        serde_json::json!({
            "tool": "displayplacer",
            "available": displayplacer_available,
            "displayCount": display_count,
            "subcases": ["resolution-change", "cross-display-drag"],
        }),
    );

    if !displayplacer_available {
        return skipped_outcome(
            scenario,
            "SKIPPED(tooling): displayplacer is not installed; resolution-change chaos cannot run on this machine",
        );
    }
    if display_count < 2 {
        return skipped_outcome(
            scenario,
            format!(
                "SKIPPED(hardware): only {display_count} display(s) detected; cross-display-drag chaos requires at least 2"
            ),
        );
    }

    infra_fail_outcome(
        scenario,
        "displayplacer and multiple displays are available, but CHAOS-DISPLAY-CHANGE live resolution/drag execution is not wired yet",
    )
}

fn net_impair_not_live_outcome(
    scenario: ScenarioSpec,
    script_path: Option<&Path>,
) -> ScenarioOutcome {
    match script_path {
        Some(path) => infra_fail_outcome(
            scenario,
            format!(
                "{} is present at {}, but test cockpit does not run live network impairment yet; refusing to invoke sudo or mutate pf/dnctl state",
                NET_IMPAIR_SCRIPT_RELATIVE_PATH,
                path.display()
            ),
        ),
        None => skipped_outcome(
            scenario,
            format!(
                "SKIPPED(tooling): {} was not found from CARGO_MANIFEST_DIR; network chaos cannot run on this checkout",
                NET_IMPAIR_SCRIPT_RELATIVE_PATH
            ),
        ),
    }
}

async fn run_net_impair_scenario(
    scenario: ScenarioSpec,
    writer: &mut ResultsWriter,
) -> ScenarioOutcome {
    let script_path = net_impair_script_path();
    let _ = writer.write(
        "chaos-preflight",
        Some(scenario.id),
        serde_json::json!({
            "tool": NET_IMPAIR_SCRIPT_RELATIVE_PATH,
            "available": script_path.is_some(),
            "path": script_path.as_ref().map(|path| path.display().to_string()),
            "subcases": match scenario.kind {
                ScenarioKind::ChaosNet => vec!["mild-loss", "lossy-3pct", "high-latency", "constrained-4g"],
                ScenarioKind::ChaosLifecycle => vec!["impair-on", "reconnect-under-impairment", "impair-off"],
                _ => vec![],
            },
            "liveMutationAttempted": false,
        }),
    );

    net_impair_not_live_outcome(scenario, script_path.as_deref())
}

async fn run_scaffold_only_scenario(
    scenario: ScenarioSpec,
    writer: &mut ResultsWriter,
) -> ScenarioOutcome {
    let (subcases, missing_live_layer) = match scenario.kind {
        ScenarioKind::MultiPeer => (
            vec![
                "native-plus-two-web-peers",
                "roster-count",
                "clock-calibration",
                "menubar-count",
                "keyframe-storm-guard",
            ],
            "MULTI-3 needs the cockpit runner to launch and coordinate two independent web peers in one prod room before it can make a real verdict",
        ),
        ScenarioKind::RemoteControlScaled => (
            vec![
                "force-1080p-share-tier",
                "remote-control-handshake",
                "injection-completion-budget",
                "press-to-eye-budget",
            ],
            "RC-P1080 needs a TCC-granted native host and a live remote-control transport path before it can make a real verdict",
        ),
        _ => (
            vec!["unknown-scaffold"],
            "scenario was routed through scaffold-only execution unexpectedly",
        ),
    };
    let _ = writer.write(
        "scenario-scaffold",
        Some(scenario.id),
        serde_json::json!({
            "tier": scenario.tier,
            "sourceIssue": source_issue_for_scenario(scenario.id),
            "coverageKind": coverage_kind_for_scenario(scenario.id),
            "subcases": subcases,
            "liveExecutionAttempted": false,
            "destructiveExecutionAttempted": false,
            "nativeShareAttempted": false,
            "webPeerSpawnAttempted": false,
        }),
    );

    infra_fail_outcome(
        scenario,
        format!(
            "{missing_live_layer}; scaffold is selector/metadata-only and refuses to false-pass"
        ),
    )
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteControlDrivenInput {
    kind: String,
    started_at_ms: u128,
    finished_at_ms: u128,
    expected: RemoteControlExpectedEvents,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct RemoteControlExpectedEvents {
    types: Vec<String>,
    button: Option<i64>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct RemoteControlObservedEvent {
    #[serde(rename = "tMs")]
    t_ms: u128,
    #[serde(rename = "type")]
    event_type: String,
    button: Option<i64>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct RemoteControlDriveReport {
    driven: Vec<RemoteControlDrivenInput>,
    observed: Vec<RemoteControlObservedEvent>,
}

fn percentile_ms(values: &[f64], percentile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = (sorted.len() as f64 * percentile).ceil().max(1.0) as usize - 1;
    sorted[index.min(sorted.len() - 1)]
}

fn verify_remote_control_drive(
    report: &RemoteControlDriveReport,
) -> Result<HarnessCompatibleLatencyStats, String> {
    if report.driven.is_empty() || report.observed.is_empty() {
        return Err("drive or sentinel event ledger is empty".to_string());
    }
    let mut used = vec![false; report.observed.len()];
    let mut latencies = Vec::new();
    let mut by_input = HashMap::<String, Vec<f64>>::new();
    for driven in &report.driven {
        // `finishedAtMs` is recorded after the settle window, so keep the
        // match bounded to this gesture. A broad window would let the typing
        // gesture consume the shortcut's key events and hide an ordering bug.
        let window_end = driven.finished_at_ms.saturating_add(100);
        let candidates: Vec<_> = report
            .observed
            .iter()
            .enumerate()
            .filter(|(index, event)| {
                !used[*index]
                    && event.t_ms >= driven.started_at_ms.saturating_sub(100)
                    && event.t_ms <= window_end
                    && driven
                        .expected
                        .types
                        .iter()
                        .any(|kind| kind == &event.event_type)
                    && driven
                        .expected
                        .button
                        .map(|button| event.button == Some(button))
                        .unwrap_or(true)
            })
            .collect();
        let mut selected = Vec::new();
        for (index, event) in candidates {
            used[index] = true;
            selected.push(event);
        }
        for expected_type in &driven.expected.types {
            if !selected
                .iter()
                .any(|event| &event.event_type == expected_type)
            {
                return Err(format!(
                    "{} gesture missing host event {expected_type}",
                    driven.kind
                ));
            }
        }
        if let Some(first) = selected.iter().min_by_key(|event| event.t_ms) {
            latencies.push((first.t_ms.saturating_sub(driven.started_at_ms)) as f64);
            by_input
                .entry(driven.kind.clone())
                .or_default()
                .push((first.t_ms.saturating_sub(driven.started_at_ms)) as f64);
        }
    }
    if let Some(index) = used.iter().position(|used| !used) {
        return Err(format!(
            "sentinel ledger contains unmatched/phantom event at index {index}"
        ));
    }
    if latencies.is_empty() {
        return Err("no host-event latency samples were observed".to_string());
    }
    let by_input = by_input
        .into_iter()
        .map(|(kind, values)| {
            (
                kind,
                HarnessCompatibleLatencyStatsByInput {
                    p50_ms: percentile_ms(&values, 0.5),
                    p95_ms: percentile_ms(&values, 0.95),
                    max_ms: values.iter().copied().fold(0.0, f64::max),
                    sample_count: values.len() as u32,
                },
            )
        })
        .collect();
    Ok(HarnessCompatibleLatencyStats {
        p50_ms: percentile_ms(&latencies, 0.5),
        p95_ms: percentile_ms(&latencies, 0.95),
        max_ms: latencies.iter().copied().fold(0.0, f64::max),
        sample_count: latencies.len() as u32,
        by_input,
    })
}

async fn run_remote_control_scaled_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    access_code: &str,
    run_meta: &RunMeta,
    writer: &mut ResultsWriter,
    children: &mut RunChildren,
) -> ScenarioOutcome {
    let socket = match env::var("PETAL_AUTOTEST_SOCK") {
        Ok(socket) => socket,
        Err(_) => return infra_fail_outcome(scenario, "PETAL_AUTOTEST_SOCK is not set"),
    };
    // The remote-control driver expects a published native target before it
    // requests control. Keep this setup in the cockpit path, just like the
    // other native-share scenarios, rather than relying on an external suite.
    let native_share = match start_native_test_pattern_share(app, scenario, writer).await {
        Ok(share) => share,
        Err(error) => return infra_fail_outcome(scenario, error),
    };
    record_window_screenshot_artifact(writer, scenario, "share-start", native_share.window_id);
    let web_peer = match spawn_web_peer_with_cdp(scenario, access_code, &writer.dir) {
        Ok(peer) => peer,
        Err(error) => return infra_fail_outcome(scenario, error),
    };
    children.record_web_peer(&web_peer);
    let _ = writer.write(
        "web-peer",
        Some(scenario.id),
        serde_json::json!({
            "mode": web_peer.mode,
            "url": web_peer.url,
            "pid": web_peer.pid(),
            "cdp": true,
        }),
    );
    // Chrome needs a moment to publish its debugging endpoint before the Node
    // driver performs its first /json request. The driver still reports a
    // useful infra failure if the endpoint never becomes available.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let report_path = writer.dir.join("remote-control-drive.json");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scripts/remote-control-scenario.mjs");
    let status = Command::new("node")
        .arg(&script)
        .arg(&socket)
        .arg("--cockpit-drive")
        .arg("--json")
        .arg(&report_path)
        .env("PETAL_AUTOTEST_SOCK", &socket)
        .env("PETAL_WEB_HARNESS_URL_MATCH", cockpit_harness_url_match())
        // The driver's default target identity ("native-autotest") never
        // matches a cockpit run's actual native identity, which is randomly
        // generated per run (`cockpit_identity()`, e.g. "p-cockpit-<hex>").
        // Without this, `api.request(target)` publishes its LiveKit data
        // message scoped via `destinationIdentities: [targetUserId]` to an
        // identity nobody in the room has -- LiveKit silently delivers it to
        // no one, the native remote_control receiver logs nothing at all
        // (confirmed live: zero log activity after "receiver starting"), and
        // the driver's `waitForActiveStatus` poll times out every time. #470.
        .env(
            "PETAL_REMOTE_CONTROL_TARGET_IDENTITY",
            &run_meta.native_identity,
        )
        .status();
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            return infra_fail_outcome(
                scenario,
                format!("could not launch remote-control driver: {error}"),
            )
        }
    };
    if !status.success() {
        return infra_fail_outcome(
            scenario,
            format!("remote-control driver exited with {status}"),
        );
    }
    let report: RemoteControlDriveReport = match fs::read_to_string(&report_path)
        .map_err(|error| error.to_string())
        .and_then(|json| serde_json::from_str(&json).map_err(|error| error.to_string()))
    {
        Ok(report) => report,
        Err(error) => {
            return infra_fail_outcome(scenario, format!("invalid remote-control ledger: {error}"))
        }
    };
    let latency = match verify_remote_control_drive(&report) {
        Ok(latency) => latency,
        Err(error) => {
            let mut outcome = infra_fail_outcome(scenario, error);
            outcome.verdict = ScenarioVerdict::TestFail;
            outcome.message = format!(
                "{} TEST-FAIL host-side sentinel ledger comparison failed",
                scenario.id
            );
            return outcome;
        }
    };
    let _ = writer.write("remote-control-ledger", Some(scenario.id), &report);
    ScenarioOutcome {
        scenario_id: scenario.id.to_string(),
        verdict: ScenarioVerdict::Pass,
        message: format!("{} PASS host-side sentinel event ledger matched every driven input", scenario.id),
        delivered_fps: 0.0,
        delivered_width: 0,
        delivered_height: 0,
        assertions: vec![
            AssertionOutcome { name: "remote-control-host-ledger".to_string(), passed: true, detail: "sentinel event ledger matched driven gestures in both directions; wire echo is diagnostic only".to_string() },
            AssertionOutcome { name: "remote-control-latency".to_string(), passed: true, detail: serde_json::to_string(&latency).unwrap_or_default() },
        ],
    }
}

/// Resolve the test-peer binary path from this crate's manifest dir. It must be
/// an executable distinct from this running primary; a same-binary launch would
/// hit the single-instance guard and is never evidence of a native receiver.
fn test_peer_binary_path() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest_dir.join(native_peer::TEST_PEER_BIN_RELATIVE);
    is_executable_file(&candidate).then_some(candidate)
}

/// One authenticated message on the private per-run Unix socket. The random
/// token prevents an unrelated local process from making a test look ready or
/// steering the receiver panel. The socket is bound inside the run artifact
/// directory and removed on every terminal path.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativePeerSocketMessage {
    token: String,
    event: String,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    #[serde(default)]
    binding: Option<crate::compositor::CockpitRemoteWindowBinding>,
    #[serde(default)]
    error: Option<String>,
    // RC-N2N (#819) additions. The peer is the SHARER/HOST in that scenario,
    // so its readiness has to carry what the controller needs to address it.
    /// Window id the peer is sharing (its own CGWindowID for the target app).
    #[serde(default)]
    shared_window_id: Option<u32>,
    /// The peer's LiveKit identity, i.e. the remote window's owner on the
    /// controller side. Generated per run, so the parent cannot guess it.
    #[serde(default)]
    peer_identity: Option<String>,
    /// Whether the peer process is AX-trusted. Without it the host can accept
    /// a grant and then inject nothing, which must be reported as an
    /// instrument failure with a remedy -- never as a product failure.
    #[serde(default)]
    accessibility_trusted: Option<bool>,
    /// Host-side effect record: `remote_control::autotest_status_snapshot()`
    /// plus the host replay ledger.
    #[serde(default)]
    host_report: Option<serde_json::Value>,
}

#[cfg(target_os = "macos")]
fn native_peer_socket_path(_results_dir: &Path) -> PathBuf {
    // AF_UNIX has a short path limit on macOS. The run id is enough to make the
    // name unique while keeping the socket address below that limit.
    std::env::temp_dir().join(format!("petal-n2n-{}.sock", run_id()))
}

#[cfg(target_os = "macos")]
fn native_peer_token() -> String {
    // This is an authentication nonce for a same-user local QA socket, not a
    // service credential. Use OS randomness nonetheless: predictable time/PID
    // values would let another local process attach to a live test run.
    let mut bytes = [0_u8; 24];
    if File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .is_ok()
    {
        return bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    // The fallback preserves liveness on an abnormal host while making the
    // degraded authentication visible in artifact logs through the token's
    // unusual `fallback-` prefix. It is never used as a network credential.
    format!("fallback-{}-{}-{}", run_id(), std::process::id(), now_ms())
}

#[cfg(target_os = "macos")]
async fn write_native_peer_message(
    stream: &mut OwnedWriteHalf,
    message: &NativePeerSocketMessage,
) -> Result<(), String> {
    let payload = serde_json::to_string(message).map_err(|error| error.to_string())?;
    stream
        .write_all(payload.as_bytes())
        .await
        .map_err(|error| format!("native peer socket write: {error}"))?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|error| format!("native peer socket write: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("native peer socket write: {error}"))
}

#[cfg(target_os = "macos")]
async fn read_native_peer_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<NativePeerSocketMessage, String> {
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .await
        .map_err(|error| format!("native peer socket read: {error}"))?;
    if bytes == 0 {
        return Err("native peer socket closed unexpectedly".to_string());
    }
    serde_json::from_str(line.trim_end())
        .map_err(|error| format!("native peer socket sent invalid JSON: {error}"))
}

#[cfg(target_os = "macos")]
async fn write_native_peer_message_with_timeout(
    stream: &mut OwnedWriteHalf,
    message: &NativePeerSocketMessage,
    stage: &str,
) -> Result<(), String> {
    tokio::time::timeout(
        NATIVE_PEER_TIMEOUT,
        write_native_peer_message(stream, message),
    )
    .await
    .map_err(|_| format!("native peer {stage} timed out"))?
}

#[cfg(target_os = "macos")]
async fn read_native_peer_message_with_timeout<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    stage: &str,
) -> Result<NativePeerSocketMessage, String> {
    tokio::time::timeout(NATIVE_PEER_TIMEOUT, read_native_peer_message(reader))
        .await
        .map_err(|_| format!("native peer {stage} timed out"))?
}

#[cfg(target_os = "macos")]
const NATIVE_PEER_LOG_MAX_BYTES: usize = 64 * 1024;

#[cfg(target_os = "macos")]
fn retain_native_peer_log_tail(path: &Path) {
    let Ok(contents) = fs::read(path) else {
        return;
    };
    if contents.len() <= NATIVE_PEER_LOG_MAX_BYTES {
        return;
    }
    let start = contents.len() - NATIVE_PEER_LOG_MAX_BYTES;
    let _ = fs::write(path, &contents[start..]);
}

#[cfg(target_os = "macos")]
fn native_peer_stderr_tail(stderr_path: &Path) -> String {
    const MAX_TAIL_CHARS: usize = 4_096;
    let text = String::from_utf8_lossy(
        &fs::read(stderr_path)
            .unwrap_or_else(|error| format!("<unavailable: {error}>").into_bytes()),
    )
    .into_owned();
    let mut tail = text.chars().rev().take(MAX_TAIL_CHARS).collect::<Vec<_>>();
    tail.reverse();
    crate::logging::redact_for_export(&tail.into_iter().collect::<String>())
}

#[cfg(target_os = "macos")]
fn record_native_peer_failure(
    writer: &mut ResultsWriter,
    scenario_id: &str,
    stage: &str,
    detail: &str,
    stdout_path: &Path,
    stderr_path: &Path,
    exit: &Result<std::process::ExitStatus, std::io::Error>,
) {
    retain_native_peer_log_tail(stdout_path);
    retain_native_peer_log_tail(stderr_path);
    let _ = writer.write(
        "native-peer-failure",
        Some(scenario_id),
        serde_json::json!({
            "stage": stage,
            "detail": detail,
            "stdoutArtifact": stdout_path.display().to_string(),
            "stderrArtifact": stderr_path.display().to_string(),
            "stderrTail": native_peer_stderr_tail(stderr_path),
            "exitCode": exit.as_ref().ok().and_then(|status| status.code()),
            "exitSuccess": exit.as_ref().is_ok_and(|status| status.success()),
            "exitError": exit.as_ref().err().map(ToString::to_string),
        }),
    );
}

#[cfg(target_os = "macos")]
fn stop_native_peer_with_failure(
    writer: &mut ResultsWriter,
    scenario_id: &str,
    child: &mut Child,
    stage: &str,
    detail: &str,
    stdout_path: &Path,
    stderr_path: &Path,
) -> Result<std::process::ExitStatus, std::io::Error> {
    // #823: SIGTERM first with a short grace. A SIGKILLed GUI app leaves a
    // ghost Dock tile behind (a clean quit deregisters it); the user's Dock
    // collected a row of them from harness runs.
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = child.kill();
    let exit = child.wait();
    record_native_peer_failure(
        writer,
        scenario_id,
        stage,
        detail,
        stdout_path,
        stderr_path,
        &exit,
    );
    exit
}

/// Receiver half of SHARE-N2N. This runs only in the separately-built
/// test-peer process. It joins the exact room supplied by the parent, waits
/// for the authenticated remote compositor binding to receive a real display
/// enqueue, then accepts one move and one shutdown command over the per-run
/// authenticated Unix socket.
#[cfg(target_os = "macos")]
async fn run_native_peer_receiver(app: AppHandle) -> Result<(), String> {
    if !cfg!(feature = "cockpit-privileged") {
        return Err(
            "native peer was not built with cockpit-privileged; run scripts/build-test-peer.sh"
                .to_string(),
        );
    }
    preflight_or_refuse(&app)?;
    let socket_path = env::var(NATIVE_PEER_SOCKET_ENV)
        .map(PathBuf::from)
        .map_err(|_| "native peer missing per-run socket path".to_string())?;
    let token = env::var(NATIVE_PEER_TOKEN_ENV)
        .map_err(|_| "native peer missing per-run socket token".to_string())?;
    let room_name =
        env::var(NATIVE_PEER_ROOM_ENV).map_err(|_| "native peer missing room name".to_string())?;
    let owner_identity = env::var(NATIVE_PEER_OWNER_ENV)
        .map_err(|_| "native peer missing expected sharer identity".to_string())?;
    let source_window_id = env::var(NATIVE_PEER_WINDOW_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or("native peer missing expected source window id")?;
    let identity = env::var(NATIVE_PEER_IDENTITY_ENV)
        .map_err(|_| "native peer missing identity".to_string())?;

    let rooms = app.state::<crate::rooms::RoomsState>();
    let session = app.state::<crate::session::SessionState>();
    crate::session::join_room(
        &app,
        rooms.inner(),
        session.inner(),
        room_name,
        identity,
        "Petal Native Test Receiver".to_string(),
        crate::remote_control_core::RemoteControlPolicy::Off,
        Some(7),
    )
    .await
    .map_err(|error| format!("native peer failed to join parent room: {error}"))?;

    let stream = tokio::time::timeout(NATIVE_PEER_TIMEOUT, UnixStream::connect(&socket_path))
        .await
        .map_err(|_| "native peer connect timed out".to_string())?
        .map_err(|error| format!("native peer could not connect to parent socket: {error}"))?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = TokioBufReader::new(read_half);

    let started = Instant::now();
    let mut readiness = native_peer::ReceiverReadinessTracker::new();
    let binding = loop {
        match crate::compositor::cockpit_remote_window_binding(
            &app,
            &owner_identity,
            source_window_id,
        ) {
            Ok(binding) if readiness.observe(&binding) => break binding,
            Ok(_) if started.elapsed() < NATIVE_PEER_TIMEOUT => {
                tokio::time::sleep(native_peer::RECEIVER_READINESS_SAMPLE_INTERVAL).await;
            }
            Err(error) if started.elapsed() < NATIVE_PEER_TIMEOUT => {
                readiness.observe_error(&error);
                tokio::time::sleep(native_peer::RECEIVER_READINESS_SAMPLE_INTERVAL).await;
            }
            Err(error) => {
                readiness.observe_error(&error);
                return Err(readiness.timeout_error(&owner_identity, source_window_id));
            }
            Ok(_) => return Err(readiness.timeout_error(&owner_identity, source_window_id)),
        }
    };
    write_native_peer_message_with_timeout(
        &mut write_half,
        &NativePeerSocketMessage {
            token: token.clone(),
            event: "ready".to_string(),
            x: None,
            y: None,
            binding: Some(binding),
            error: None,
            ..Default::default()
        },
        "ready write",
    )
    .await?;

    let command = read_native_peer_message_with_timeout(&mut reader, "move read").await?;
    if !native_peer_command_is_authorized(&command, &token, "move") {
        return Err("native peer rejected unauthenticated or unexpected move command".to_string());
    }
    let x = command.x.ok_or("native peer move missing x")?;
    let y = command.y.ok_or("native peer move missing y")?;
    crate::compositor::cockpit_move_remote_window(&app, &owner_identity, source_window_id, x, y)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let moved =
        crate::compositor::cockpit_remote_window_binding(&app, &owner_identity, source_window_id)?;
    write_native_peer_message_with_timeout(
        &mut write_half,
        &NativePeerSocketMessage {
            token: token.clone(),
            event: "moved".to_string(),
            x: None,
            y: None,
            binding: Some(moved),
            error: None,
            ..Default::default()
        },
        "moved write",
    )
    .await?;
    let shutdown = read_native_peer_message_with_timeout(&mut reader, "shutdown read").await?;
    if !native_peer_command_is_authorized(&shutdown, &token, "shutdown") {
        return Err(
            "native peer rejected unauthenticated or unexpected shutdown command".to_string(),
        );
    }
    leave_cockpit_room(&app).await;
    Ok(())
}

/// Host half of RC-N2N (#819). Runs only in the separately-built test-peer.
/// The roles are the reverse of `run_native_peer_receiver`: this process joins
/// the parent's room, SHARES the sacrificial target window, and then serves as
/// the remote-control HOST while the parent drives it as the controller.
///
/// It reports host-side effects on request rather than deciding anything
/// itself: the verdict belongs to the parent, which is the only side that can
/// compare what the controller published against what the host did with it.
#[cfg(target_os = "macos")]
async fn run_native_peer_control_host(app: AppHandle) -> Result<(), String> {
    if !cfg!(feature = "cockpit-privileged") {
        return Err(
            "native peer was not built with cockpit-privileged; run scripts/build-test-peer.sh"
                .to_string(),
        );
    }
    preflight_or_refuse(&app)?;
    let socket_path = env::var(NATIVE_PEER_SOCKET_ENV)
        .map(PathBuf::from)
        .map_err(|_| "native peer missing per-run socket path".to_string())?;
    let token = env::var(NATIVE_PEER_TOKEN_ENV)
        .map_err(|_| "native peer missing per-run socket token".to_string())?;
    let room_name =
        env::var(NATIVE_PEER_ROOM_ENV).map_err(|_| "native peer missing room name".to_string())?;
    let identity = env::var(NATIVE_PEER_IDENTITY_ENV)
        .map_err(|_| "native peer missing identity".to_string())?;
    let target_app = env::var(NATIVE_PEER_TARGET_APP_ENV)
        .map_err(|_| "native peer missing target app name".to_string())?;
    let target_title = env::var(NATIVE_PEER_TARGET_TITLE_ENV)
        .map_err(|_| "native peer missing target title marker".to_string())?;

    let rooms = app.state::<crate::rooms::RoomsState>();
    let session = app.state::<crate::session::SessionState>();
    crate::session::join_room(
        &app,
        rooms.inner(),
        session.inner(),
        room_name,
        identity.clone(),
        "Petal Native Test Host".to_string(),
        // remote_control_allowed=true: this peer exists to ACCEPT control.
        // `false` (copied from the receiver flow, where it is right) made the
        // host answer every request with status 'disabled' -- caught live on
        // the first unlocked RC-N2N run.
        crate::remote_control_core::RemoteControlPolicy::Auto,
        Some(7),
    )
    .await
    .map_err(|error| format!("native peer failed to join parent room: {error}"))?;

    // Match the sacrificial document by its per-run title marker, never by
    // ordinal -- see NATIVE_PEER_TARGET_TITLE_ENV.
    let started = Instant::now();
    let window = loop {
        let candidates = crate::window_source::list().unwrap_or_default();
        let matched: Vec<_> = candidates
            .into_iter()
            .filter(|candidate| {
                candidate.app_name == target_app
                    && candidate
                        .title
                        .as_deref()
                        .is_some_and(|title| title.contains(&target_title))
            })
            .collect();
        match matched.as_slice() {
            [only] => break only.clone(),
            many if many.len() > 1 => {
                return Err(format!(
                    "native peer found {} windows matching {target_app}/{target_title}; the run \
                     marker is supposed to be unique",
                    many.len()
                ))
            }
            _ if started.elapsed() < NATIVE_PEER_TIMEOUT => {
                tokio::time::sleep(native_peer::RECEIVER_READINESS_SAMPLE_INTERVAL).await;
            }
            _ => {
                return Err(format!(
                    "native peer never saw a shareable {target_app} window titled '*{target_title}*'"
                ))
            }
        }
    };

    // Share through the REAL UI path, exactly as a user's click does. Going
    // straight to the session layer would skip the share border, overlay and
    // hover-tab state a live host actually has (CLAUDE.md crash-class 2).
    let frame = crate::platform::cg::frame_for_window_id(window.window_id)
        .ok_or_else(|| format!("native peer target window {} is not on screen", window.window_id))?;
    crate::hover_tab::toggle_share_for_window(&app, session.inner(), window.window_id, frame).await;
    if !session.inner().is_share_active(window.window_id) {
        return Err(format!(
            "native peer could not start sharing target window {}",
            window.window_id
        ));
    }

    log::info!(
        "test-cockpit: control-host share active for window {}, dialing the parent socket",
        window.window_id
    );
    let stream = tokio::time::timeout(NATIVE_PEER_TIMEOUT, UnixStream::connect(&socket_path))
        .await
        .map_err(|_| "native peer connect timed out".to_string())?
        .map_err(|error| format!("native peer could not connect to parent socket: {error}"))?;
    log::info!("test-cockpit: control-host connected to the parent socket");
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = TokioBufReader::new(read_half);

    write_native_peer_message_with_timeout(
        &mut write_half,
        &NativePeerSocketMessage {
            token: token.clone(),
            event: "ready".to_string(),
            shared_window_id: Some(window.window_id),
            peer_identity: Some(identity),
            accessibility_trusted: Some(crate::permissions::check_accessibility()),
            ..Default::default()
        },
        "control-host ready write",
    )
    .await?;

    // Reports until shutdown; anything else on this socket is refused. The
    // parent POLLS the report while the AX replay queue drains (a single
    // fixed settle converted rig timing into product verdicts -- Fable
    // review), so "one report then shutdown" is no longer the protocol.
    loop {
        let request = read_native_peer_message_with_timeout(&mut reader, "command read").await?;
        if native_peer_command_is_authorized(&request, &token, "report") {
            write_native_peer_message_with_timeout(
                &mut write_half,
                &NativePeerSocketMessage {
                    token: token.clone(),
                    event: "report".to_string(),
                    host_report: Some(native_peer_host_report()),
                    ..Default::default()
                },
                "control-host report write",
            )
            .await?;
            continue;
        }
        if native_peer_command_is_authorized(&request, &token, "shutdown") {
            break;
        }
        return Err(
            "native peer rejected unauthenticated or unexpected command on the control socket"
                .to_string(),
        );
    }
    crate::hover_tab::clear_share_state_for_window(&app, window.window_id);
    let _ = crate::session::stop_share(&app, session.inner(), window.window_id).await;
    leave_cockpit_room(&app).await;
    Ok(())
}

/// The host-side effect record the parent compares against what the controller
/// published: live grant/pressed-input state plus every input this host
/// actually replayed and its own terminal disposition.
#[cfg(all(target_os = "macos", feature = "cockpit-privileged"))]
fn native_peer_host_report() -> serde_json::Value {
    let mut report = crate::remote_control::autotest_status_snapshot();
    let ledger = crate::remote_control::cockpit_ledger::snapshot();
    if let Some(object) = report.as_object_mut() {
        object.insert(
            "replays".to_string(),
            serde_json::to_value(&ledger.replays).unwrap_or(serde_json::Value::Null),
        );
        object.insert(
            "clipboardReplays".to_string(),
            serde_json::to_value(&ledger.clipboard_replays).unwrap_or(serde_json::Value::Null),
        );
    }
    report
}

/// Commands on the private native-peer socket are deliberately strict: the
/// per-run nonce and the expected protocol step must both match.  Keeping the
/// decision in one helper makes the rejection behaviour testable without a
/// live AppKit peer process.
fn native_peer_command_is_authorized(
    message: &NativePeerSocketMessage,
    token: &str,
    expected_event: &str,
) -> bool {
    message.token == token && message.event == expected_event
}

/// SHARE-N2N (SHARE-01): the ONLY scenario that validates Petal's defining
/// feature — a shared window rendering on the receiver as a real, borderless,
/// independently movable NATIVE window. Sharer = this (primary) instance,
/// receiver = the Native Test Client (test-peer) built by
/// scripts/build-test-peer.sh.
///
/// A missing receiver build is a *per-scenario* setup skip. Any receiver that
/// does start but fails protocol, TCC, compositor, capture or cleanup checks is
/// an infra/test failure; it must never be folded into this setup skip.
async fn run_native_to_native_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    run_meta: &RunMeta,
    writer: &mut ResultsWriter,
    children: &mut RunChildren,
) -> ScenarioOutcome {
    let peer_bin = test_peer_binary_path();
    let _ = writer.write(
        "scenario-scaffold",
        Some(scenario.id),
        serde_json::json!({
            "tier": scenario.tier,
            "sourceIssue": source_issue_for_scenario(scenario.id),
            "coverageKind": coverage_kind_for_scenario(scenario.id),
            "testPeerIdentifier": native_peer::TEST_PEER_IDENTIFIER,
            "testPeerTargetSubdir": native_peer::TEST_PEER_TARGET_SUBDIR,
            "testPeerBinRelative": native_peer::TEST_PEER_BIN_RELATIVE,
            "testPeerBinaryPresent": peer_bin.is_some(),
            "testPeerBinaryPath": peer_bin.as_ref().map(|p| p.display().to_string()),
            "subcases": [
                "launch-test-peer-receiver",
                "join-same-prod-room",
                "receiver-native-window-exists",
                "receiver-window-borderless",
                "receiver-window-independently-movable",
                "receiver-window-crisp-screencapture",
            ],
            "oracle": "CGWindowListCopyWindowInfo geometry before/after a programmatic move (native_peer::evaluate_independent_move); screencapture -l is supporting evidence only",
            "liveExecutionAttempted": false,
            "webPeerSpawnAttempted": false,
        }),
    );

    let Some(path) = peer_bin else {
        return skipped_outcome(
            scenario,
            format!(
                "SKIPPED(setup): Native Test Client binary missing at {}; run scripts/build-test-peer.sh then scripts/cockpit-setup.sh",
                native_peer::TEST_PEER_BIN_RELATIVE
            ),
        );
    };

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, writer, children, path);
        return infra_fail_outcome(scenario, "SHARE-N2N is macOS-only");
    }

    #[cfg(target_os = "macos")]
    {
        let socket_path = native_peer_socket_path(&writer.dir);
        let token = native_peer_token();
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(error) => {
                return infra_fail_outcome(
                    scenario,
                    format!(
                        "INFRA-FAIL create native-peer socket {}: {error}",
                        socket_path.display()
                    ),
                )
            }
        };
        let source_share = match start_native_test_pattern_share(app, scenario, writer).await {
            Ok(share) => share,
            Err(error) => {
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(scenario, error);
            }
        };
        let source_window_id = source_share.window_id;
        // The peer must join the already-running primary's exact room and
        // target its exact LiveKit identity. Generating another rctest name
        // here silently creates a second room and can never prove N2N.
        //
        // Forward the real ACCESS CODE, not the bare `joined_room_credential`
        // (the internal `room-<hex>` slug). The test peer is a genuinely
        // separate process with its own never-before-seen `RoomsState` store,
        // so it must bootstrap the same way any second real participant
        // would -- by access code. Refs #421/#430 deliberately hardened
        // `rooms::room_credential_for_input` to refuse minting a capability
        // for a bare credential a store has never joined by its real code
        // (closing a security hole), which as a side effect broke this
        // exact bare-credential handoff: it used to fail closed here with
        // "missing LiveKit configuration: room name must not be empty"
        // (found live running SHARE-N2N end to end). The access code
        // round-trips through the same deterministic
        // `internal_credential_for_access_code` hash on both sides, so the
        // peer converges on the identical room -- see
        // `native_peer_uses_the_parent_joined_capability_across_separate_room_stores`.
        let room_name = match run_meta.access_code.clone() {
            Some(access_code) => access_code,
            None => {
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(
                    scenario,
                    "INFRA-FAIL parent join did not provide a real access code",
                );
            }
        };
        let owner_identity = run_meta.native_identity.clone();
        // Must match the backend's GENERATED_PARTICIPANT_ID shape
        // (`^p-[a-z0-9]+-[a-z0-9]+$`, same as `cockpit_identity()` above) --
        // found live: the old `cockpit-native-peer-<run_id>` shape was
        // rejected with "identity must be a generated participant id" once
        // the room-credential bug above was fixed and the peer's token
        // request actually reached the backend for the first time.
        let peer_identity = format!("p-nativepeer-{}", rand_hex());
        let peer_stdout_path = writer.dir.join("native-peer.stdout.log");
        let peer_stderr_path = writer.dir.join("native-peer.stderr.log");
        let peer_stdout = match File::create(&peer_stdout_path) {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(
                    scenario,
                    format!("INFRA-FAIL create Native Test Client stdout artifact: {error}"),
                );
            }
        };
        let peer_stderr = match File::create(&peer_stderr_path) {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(
                    scenario,
                    format!("INFRA-FAIL create Native Test Client stderr artifact: {error}"),
                );
            }
        };
        let mut child = match Command::new(&path)
            .arg(TEST_CASE_ARG)
            .arg(NATIVE_PEER_RECEIVER_SELECTOR)
            .env(NATIVE_PEER_SOCKET_ENV, &socket_path)
            .env(NATIVE_PEER_TOKEN_ENV, &token)
            .env(NATIVE_PEER_ROOM_ENV, &room_name)
            .env(NATIVE_PEER_OWNER_ENV, &owner_identity)
            .env(NATIVE_PEER_WINDOW_ENV, source_window_id.to_string())
            .env(NATIVE_PEER_IDENTITY_ENV, peer_identity)
            .env("PETAL_DISABLE_AUDIO", "1")
            // #823: peers must not pollute the Dock or steal focus (their
            // self-activation broke RC-N2N's AX focus live).
            .env("PETAL_ACCESSORY_UI", "1")
            .stdout(Stdio::from(peer_stdout))
            .stderr(Stdio::from(peer_stderr))
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(
                    scenario,
                    format!("INFRA-FAIL launch Native Test Client: {error}"),
                );
            }
        };
        children.record_native_peer(&child);
        let _ = writer.write(
            "native-peer-launch",
            Some(scenario.id),
            serde_json::json!({
                "pid": child.id(),
                "binary": path.display().to_string(),
                "socket": socket_path.display().to_string(),
                "roomName": run_meta.room_name,
                "canonicalCredentialForwarded": true,
                "stdoutArtifact": peer_stdout_path.display().to_string(),
                "stderrArtifact": peer_stderr_path.display().to_string(),
                "ownerIdentity": owner_identity,
                "sourceWindowId": source_window_id,
                "liveExecutionAttempted": true,
            }),
        );
        let _ = writer.write(
            "scenario-live-execution",
            Some(scenario.id),
            serde_json::json!({
                "liveExecutionAttempted": true,
                "nativePeerLaunchAttempted": true,
                "nativePeerPid": child.id(),
            }),
        );

        let stream = match tokio::time::timeout(NATIVE_PEER_TIMEOUT, listener.accept()).await {
            Ok(Ok((stream, _))) => stream,
            Ok(Err(error)) => {
                let detail = format!("accept Native Test Client socket: {error}");
                let _ = stop_native_peer_with_failure(
                    writer,
                    scenario.id,
                    &mut child,
                    "accept",
                    &detail,
                    &peer_stdout_path,
                    &peer_stderr_path,
                );
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
            }
            Err(_) => {
                let detail = "native peer accept timed out";
                let _ = stop_native_peer_with_failure(
                    writer,
                    scenario.id,
                    &mut child,
                    "accept",
                    detail,
                    &peer_stdout_path,
                    &peer_stderr_path,
                );
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
            }
        };
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = TokioBufReader::new(read_half);
        let ready = match read_native_peer_message_with_timeout(&mut reader, "readiness").await {
            Ok(message) if message.token == token && message.event == "ready" => message,
            Ok(message) => {
                let detail = format!("unauthenticated native peer readiness: {}", message.event);
                let _ = stop_native_peer_with_failure(
                    writer,
                    scenario.id,
                    &mut child,
                    "readiness",
                    &detail,
                    &peer_stdout_path,
                    &peer_stderr_path,
                );
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
            }
            Err(error) => {
                let detail = format!("native peer readiness: {error}");
                let _ = stop_native_peer_with_failure(
                    writer,
                    scenario.id,
                    &mut child,
                    "readiness",
                    &detail,
                    &peer_stdout_path,
                    &peer_stderr_path,
                );
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
            }
        };
        let Some(before) = ready.binding else {
            let detail = "native peer ready message omitted compositor binding";
            let _ = stop_native_peer_with_failure(
                writer,
                scenario.id,
                &mut child,
                "readiness",
                detail,
                &peer_stdout_path,
                &peer_stderr_path,
            );
            let _ = fs::remove_file(&socket_path);
            return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
        };
        let target_x = before.frame.x + 120;
        let target_y = before.frame.y + 60;
        // These are deliberately taken immediately around the requested
        // receiver move, not at share startup. `on_screen_frame` remains the
        // strict oracle input; OptionAll/AppKit/focus readings below classify
        // any failure without creating an evidence bypass.
        let sharer_before =
            sample_fresh_sharer_frame(app, source_window_id, writer, scenario, "before-move").await;
        let _ = writer.write(
            "native-peer-sharer-frame",
            Some(scenario.id),
            serde_json::json!({
                "phase": "before-move",
                "sourceWindowId": source_window_id,
                "sample": sharer_before,
            }),
        );
        if let Err(error) = write_native_peer_message_with_timeout(
            &mut write_half,
            &NativePeerSocketMessage {
                token: token.clone(),
                event: "move".to_string(),
                x: Some(target_x),
                y: Some(target_y),
                binding: None,
                error: None,
                ..Default::default()
            },
            "move write",
        )
        .await
        {
            let detail = format!("command native peer move: {error}");
            let _ = stop_native_peer_with_failure(
                writer,
                scenario.id,
                &mut child,
                "move",
                &detail,
                &peer_stdout_path,
                &peer_stderr_path,
            );
            let _ = fs::remove_file(&socket_path);
            return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
        }
        let moved = match read_native_peer_message_with_timeout(&mut reader, "move response").await
        {
            Ok(message) if message.token == token && message.event == "moved" => message,
            Ok(message) => {
                let detail = format!(
                    "unauthenticated native peer move response: {}",
                    message.event
                );
                let _ = stop_native_peer_with_failure(
                    writer,
                    scenario.id,
                    &mut child,
                    "move",
                    &detail,
                    &peer_stdout_path,
                    &peer_stderr_path,
                );
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
            }
            Err(error) => {
                let detail = format!("native peer move response: {error}");
                let _ = stop_native_peer_with_failure(
                    writer,
                    scenario.id,
                    &mut child,
                    "move",
                    &detail,
                    &peer_stdout_path,
                    &peer_stderr_path,
                );
                let _ = fs::remove_file(&socket_path);
                return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
            }
        };
        let Some(after) = moved.binding else {
            let detail = "native peer moved message omitted compositor binding";
            let _ = stop_native_peer_with_failure(
                writer,
                scenario.id,
                &mut child,
                "move",
                detail,
                &peer_stdout_path,
                &peer_stderr_path,
            );
            let _ = fs::remove_file(&socket_path);
            return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
        };
        let sharer_after =
            sample_fresh_sharer_frame(app, source_window_id, writer, scenario, "after-move").await;
        let _ = writer.write(
            "native-peer-sharer-frame",
            Some(scenario.id),
            serde_json::json!({
                "phase": "after-move",
                "sourceWindowId": source_window_id,
                "sample": sharer_after,
            }),
        );
        let move_result = native_peer::assert_same_receiver_surface(
            &before.panel_label,
            before.cg_window_id,
            &after.panel_label,
            after.cg_window_id,
        )
        .and_then(|()| {
            native_peer::require_sharer_frame_samples(
                sharer_before.on_screen_frame,
                sharer_after.on_screen_frame,
            )
        })
        .and_then(|sharer_frames| {
            native_peer::evaluate_independent_move(
                before.frame,
                after.frame,
                120,
                60,
                sharer_frames,
            )
        });
        let shutdown_result = write_native_peer_message_with_timeout(
            &mut write_half,
            &NativePeerSocketMessage {
                token,
                event: "shutdown".to_string(),
                x: None,
                y: None,
                binding: None,
                error: None,
                ..Default::default()
            },
            "shutdown write",
        )
        .await;
        let exit = child.wait();
        retain_native_peer_log_tail(&peer_stdout_path);
        retain_native_peer_log_tail(&peer_stderr_path);
        let _ = fs::remove_file(&socket_path);
        let _ = writer.write("native-peer-verdict", Some(scenario.id), serde_json::json!({
            "before": before, "after": after,
            "sharerBefore": sharer_before, "sharerAfter": sharer_after,
            "sameReceiverSurface": before.panel_label == after.panel_label && before.cg_window_id == after.cg_window_id,
            "move": move_result.as_ref().ok(),
            "shutdownSent": shutdown_result.is_ok(), "exit": exit.as_ref().ok().map(|status| status.code()),
        }));
        if let Err(error) = shutdown_result {
            let detail = format!("native peer shutdown command: {error}");
            record_native_peer_failure(
                writer,
                scenario.id,
                "shutdown",
                &detail,
                &peer_stdout_path,
                &peer_stderr_path,
                &exit,
            );
            return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
        }
        if !exit.as_ref().is_ok_and(|status| status.success()) {
            let detail = format!("Native Test Client exited unsuccessfully: {exit:?}");
            record_native_peer_failure(
                writer,
                scenario.id,
                "shutdown",
                &detail,
                &peer_stdout_path,
                &peer_stderr_path,
                &exit,
            );
            return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
        }
        match move_result {
            Ok(detail) => ScenarioOutcome {
                scenario_id: scenario.id.to_string(),
                verdict: ScenarioVerdict::Pass,
                message: format!("SHARE-N2N PASS {detail}"),
                delivered_fps: 0.0,
                delivered_width: before.frame.width.max(0) as u32,
                delivered_height: before.frame.height.max(0) as u32,
                assertions: vec![
                    AssertionOutcome {
                        name: "authenticated-native-peer-binding".to_string(),
                        passed: before.frames_display_enqueued > 0,
                        detail: format!(
                            "{}:{} -> {} (CGWindowID {})",
                            before.owner_identity,
                            before.source_window_id,
                            before.panel_label,
                            before.cg_window_id
                        ),
                    },
                    AssertionOutcome {
                        name: "independent-native-panel-move".to_string(),
                        passed: true,
                        detail,
                    },
                ],
            },
            Err(error) => infra_fail_outcome(scenario, format!("SHARE-N2N TEST-FAIL {error}")),
        }
    }
}

// ---------------------------------------------------------------------------
// RC-N2N / RC-N2W (journey RC-07, #819): Petal as the CONTROLLER.
// ---------------------------------------------------------------------------

/// Seeded into the sacrificial document before the run. Static, blank content
/// starves ScreenCaptureKit (it only delivers a callback on an actual screen
/// change), so an empty document can sit for seconds producing no frames and
/// the share never reads ready -- confirmed live in the 30-case web suite.
/// Deliberately does NOT contain `rc_n2n::KEYSTONE_TEXT`, so "the text landed"
/// can never be satisfied by the seed.
#[cfg(target_os = "macos")]
const RC_N2N_DOCUMENT_SEED_LINE: &str = "petal remote control target\n";

#[cfg(target_os = "macos")]
const RC_N2N_OSASCRIPT_TIMEOUT: Duration = Duration::from_secs(8);

/// The sacrificial TextEdit document the controller types into. Addressed by
/// its per-run `marker` everywhere -- see `NATIVE_PEER_TARGET_TITLE_ENV`.
#[cfg(target_os = "macos")]
struct SacrificialDocument {
    marker: String,
    path: PathBuf,
}

/// Run one AppleScript with a real deadline. A wedged TextEdit or cfprefsd
/// (observed live, a sustained 44-minute hang on a busy shared Mac) otherwise
/// blocks forever and takes the whole run with it. On expiry this kills the
/// child and returns an error -- it never returns an empty string, because
/// "could not read" and "read nothing" are the two things the oracle must be
/// able to tell apart.
#[cfg(target_os = "macos")]
fn osascript(lines: &[String]) -> Result<String, String> {
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    match crate::platform::osascript::run_osascript(&lines, RC_N2N_OSASCRIPT_TIMEOUT) {
        crate::platform::osascript::OsascriptOutcome::Ok(stdout) => {
            // Strip only the single trailing newline osascript itself
            // appends -- never trim(), or the document's own leading/
            // trailing whitespace disappears from the comparison.
            Ok(stdout.strip_suffix('\n').unwrap_or(&stdout).to_string())
        }
        crate::platform::osascript::OsascriptOutcome::Timeout => Err(format!(
            "osascript exceeded its {}s deadline (a wedged target app)",
            RC_N2N_OSASCRIPT_TIMEOUT.as_secs()
        )),
        crate::platform::osascript::OsascriptOutcome::Failed { status, stderr } => {
            Err(format!("osascript failed ({status}): {stderr}"))
        }
        crate::platform::osascript::OsascriptOutcome::Spawn(error) => Err(error),
    }
}

/// Open a fresh sacrificial document whose title carries a unique run marker.
#[cfg(target_os = "macos")]
fn open_sacrificial_document() -> Result<SacrificialDocument, String> {
    let marker = format!("petal-rc-n2n-{}", run_id());
    let path = std::env::temp_dir().join(format!("{marker}.txt"));
    fs::write(&path, RC_N2N_DOCUMENT_SEED_LINE.repeat(20))
        .map_err(|error| format!("write sacrificial document: {error}"))?;
    let status = Command::new("/usr/bin/open")
        .arg("-a")
        .arg("TextEdit")
        .arg(&path)
        .status()
        .map_err(|error| format!("open sacrificial document: {error}"))?;
    if !status.success() {
        return Err(format!("`open -a TextEdit` exited with {status}"));
    }
    // Make the fresh document TextEdit's KEY window once. `open -a` loads the
    // doc without ever making its window key, so the app-internal first
    // responder is nowhere and CGEventPostToPid keyboard replay types into
    // nothing -- measured live: 33/33 key inputs 'applied' by the host,
    // document byte-identical afterwards. One brief activation at open (the
    // same cost the 30-case suite's reader pays every case) fixes the whole
    // drive; focus can move elsewhere afterwards, keyboard routing is
    // responder-based (CLAUDE.md crash-class 4).
    let _ = osascript(&[
        "tell application \"System Events\"".to_string(),
        "tell process \"TextEdit\"".to_string(),
        "set frontmost to true".to_string(),
        "end tell".to_string(),
        "end tell".to_string(),
    ]);
    std::thread::sleep(Duration::from_millis(500));
    Ok(SacrificialDocument { marker, path })
}

#[cfg(target_os = "macos")]
fn read_sacrificial_document(marker: &str) -> Result<String, String> {
    // No AppleScript try/on-error here: a failed read must surface as `Err`
    // (-> InfraFail), never as "" -- "" is what an empty DOCUMENT reads as,
    // and conflating the two manufactures a product verdict from a blind
    // instrument (#821 shape; #819 review).
    osascript(&[
        "tell application \"TextEdit\"".to_string(),
        format!("return text of (first document whose name contains \"{marker}\")"),
        "end tell".to_string(),
    ])
}

/// The Cmd+A oracle. `None` means the selection could not be read at all --
/// the scenario reports that as unmeasured, never as "nothing was selected".
#[cfg(target_os = "macos")]
fn read_sacrificial_selection(marker: &str) -> Option<String> {
    // No AppleScript try/on-error: an unresolvable AX path (the incantation
    // is unproven for a native controller) must come back `None` (unmeasured,
    // -> InfraFail), never "" ("nothing was selected" -- a product verdict).
    osascript(&[
        "tell application \"System Events\"".to_string(),
        "tell process \"TextEdit\"".to_string(),
        // `set frontmost to true` is load-bearing and copied from the
        // 30-case suite's proven reader: AXSelectedText of a BACKGROUND
        // process's text area reads empty even when the selection is real
        // (measured live -- 35/35 inputs replayed, text landed, selection
        // read ''). Briefly frontmosting TextEdit is the cost the suite
        // already accepts for an observable answer.
        "set frontmost to true".to_string(),
        format!("set theWin to first window whose name contains \"{marker}\""),
        "return value of attribute \"AXSelectedText\" of text area 1 of scroll area 1 of theWin"
            .to_string(),
        "end tell".to_string(),
        "end tell".to_string(),
    ])
    .ok()
}

#[cfg(target_os = "macos")]
fn close_sacrificial_document(document: &SacrificialDocument) {
    let _ = osascript(&[
        "tell application \"TextEdit\"".to_string(),
        "try".to_string(),
        format!(
            "close (first document whose name contains \"{}\") saving no",
            document.marker
        ),
        "end try".to_string(),
        "end tell".to_string(),
    ]);
    let _ = fs::remove_file(&document.path);
}

/// Project the cockpit-privileged controller ledger into the oracle's inputs.
#[cfg(target_os = "macos")]
fn controller_ledger_projection(
    window_id: u32,
) -> (Vec<rc_n2n::DrivenInput>, Vec<rc_n2n::GrantStatus>) {
    let ledger = crate::remote_control::cockpit_ledger::snapshot();
    let mut driven: Vec<rc_n2n::DrivenInput> = ledger
        .published
        .iter()
        .filter(|entry| entry.window_id == window_id)
        .map(|entry| rc_n2n::DrivenInput {
            kind: wire_kind_label(entry.kind).to_string(),
            action: entry.action.map(|action| wire_action_label(action).to_string()),
            key: entry.key.clone(),
            meta: entry.meta,
            t_ms: entry.t_ms,
        })
        .collect();
    driven.extend(
        ledger
            .clipboard_published
            .iter()
            .filter(|entry| entry.window_id == window_id)
            .map(|entry| rc_n2n::DrivenInput {
                kind: entry.operation.clone(),
                action: None,
                key: None,
                meta: false,
                t_ms: entry.t_ms,
            }),
    );
    driven.sort_by_key(|entry| entry.t_ms);
    let statuses = ledger
        .statuses
        .iter()
        .filter(|entry| entry.window_id == window_id)
        .map(|entry| rc_n2n::GrantStatus {
            status: entry.status.clone(),
            has_grant_token: entry.has_grant_token,
            t_ms: entry.t_ms,
        })
        .collect();
    (driven, statuses)
}

#[cfg(target_os = "macos")]
fn wire_kind_label(kind: crate::remote_control_core::RemoteControlType) -> &'static str {
    use crate::remote_control_core::RemoteControlType as T;
    match kind {
        T::Request => "request",
        T::Release => "release",
        T::Status => "status",
        T::Pointer => "pointer",
        T::Wheel => "wheel",
        T::Key => "key",
        T::Text => "text",
        T::Result => "result",
        T::Unknown => "unknown",
    }
}

#[cfg(target_os = "macos")]
fn wire_action_label(action: crate::remote_control_core::RemoteControlAction) -> &'static str {
    use crate::remote_control_core::RemoteControlAction as A;
    match action {
        A::Move => "move",
        A::Down => "down",
        A::Up => "up",
        A::Click => "click",
        A::Unknown => "unknown",
    }
}

/// Parse the peer's host report into the oracle's inputs. A report the parent
/// cannot parse is an instrument failure, so this returns `Err` rather than an
/// empty ledger that would read as "the host replayed nothing".
#[cfg(target_os = "macos")]
fn host_report_projection(
    report: &serde_json::Value,
    controller_identity: &str,
) -> Result<(Vec<rc_n2n::HostEffect>, usize, usize), String> {
    // `mut`: the clipboard-replay block below extends and re-sorts this.
    let mut replays = report
        .get("replays")
        .and_then(|value| value.as_array())
        .ok_or("host report carried no replay ledger")?
        .iter()
        .filter(|entry| {
            entry
                .get("controllerId")
                .and_then(|value| value.as_str())
                .is_none_or(|id| id == controller_identity)
        })
        .map(|entry| rc_n2n::HostEffect {
            kind: entry
                .get("kind")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_string(),
            action: entry
                .get("action")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            key: entry
                .get("key")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            outcome: entry
                .get("outcome")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_string(),
            t_ms: entry.get("tMs").and_then(|value| value.as_u64()).unwrap_or(0),
        })
        .collect::<Vec<_>>();
    if let Some(clipboard_replays) = report
        .get("clipboardReplays")
        .and_then(|value| value.as_array())
    {
        replays.extend(
            clipboard_replays
                .iter()
                .filter(|entry| {
                    entry
                        .get("controllerId")
                        .and_then(|value| value.as_str())
                        .is_none_or(|id| id == controller_identity)
                })
                .map(|entry| rc_n2n::HostEffect {
                    kind: entry
                        .get("operation")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    action: None,
                    key: None,
                    outcome: entry
                        .get("outcome")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    t_ms: entry.get("tMs").and_then(|value| value.as_u64()).unwrap_or(0),
                }),
        );
        replays.sort_by_key(|entry| entry.t_ms);
    }
    let sessions = report
        .get("sessions")
        .and_then(|value| value.as_array())
        .ok_or("host report carried no session list")?
        .iter()
        .filter(|entry| {
            entry
                .get("controllerId")
                .and_then(|value| value.as_str())
                .is_none_or(|id| id == controller_identity)
        })
        .count();
    let pressed = report
        .get("pressedInputs")
        .and_then(|value| value.as_array())
        .ok_or("host report carried no pressed-input list")?
        .iter()
        .map(|entry| {
            let buttons = entry
                .get("buttons")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let keys = entry.get("keys").and_then(|value| value.as_u64()).unwrap_or(0);
            (buttons + keys) as usize
        })
        .sum();
    Ok((replays, pressed, sessions))
}

/// Poll the controller ledger for the host's grant. Returns whether it landed;
/// the run continues either way so the oracle -- not this helper -- decides
/// what a missing grant means.
#[cfg(target_os = "macos")]
async fn await_controller_grant(window_id: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let (_, statuses) = controller_ledger_projection(window_id);
        if statuses
            .iter()
            .any(|status| status.status == "active" && status.has_grant_token)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Does the rendered remote window advertise that its owner accepts remote
/// control? `compositor::set_remote_control_active` refuses to arm the overlay
/// when it does not, and it refuses from inside the host's status handler --
/// so without this preflight the only symptom is a control route that silently
/// publishes nothing, which is the hardest failure in this scenario to read.
#[cfg(target_os = "macos")]
fn remote_control_is_offered(owner_identity: &str, window_id: u32) -> Result<bool, String> {
    crate::compositor::cockpit_remote_control_is_offered(owner_identity, window_id)
}

/// RC-N2N (journey RC-07, #819): the reverse of everything
/// `scripts/rc-live-suite.sh` covers. Controller = this (primary) instance,
/// driving the REAL compositor/control route; host = the Native Test Client,
/// sharing a sacrificial TextEdit document and replaying the input into it.
///
/// A missing test-peer build is a per-scenario setup skip, exactly as in
/// SHARE-N2N. A peer that starts but is not Accessibility-trusted is an
/// INFRA-FAIL with the remedy, never a product failure: an un-trusted host can
/// accept a grant and then inject nothing at all.
#[cfg(target_os = "macos")]
async fn run_remote_control_native_to_native_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    run_meta: &RunMeta,
    writer: &mut ResultsWriter,
    children: &mut RunChildren,
) -> ScenarioOutcome {
    let peer_bin = test_peer_binary_path();
    let _ = writer.write(
        "scenario-scaffold",
        Some(scenario.id),
        serde_json::json!({
            "tier": scenario.tier,
            "sourceIssue": source_issue_for_scenario(scenario.id),
            "coverageKind": coverage_kind_for_scenario(scenario.id),
            "testPeerBinaryPresent": peer_bin.is_some(),
            "subcases": [
                "launch-test-peer-control-host",
                "peer-shares-sacrificial-target",
                "controller-remote-window-rendered",
                "controller-requests-control-through-the-real-route",
                "keystone-gestures-published-by-the-control-route",
                "host-replayed-every-input",
                "typed-text-landed-in-the-target-document",
                "release-left-no-held-input-or-session",
            ],
            "oracle": "rc_n2n::evaluate over the controller publish ledger, the peer's host replay ledger, and the sacrificial document's own text",
        }),
    );

    let Some(peer_path) = peer_bin else {
        return skipped_outcome(
            scenario,
            format!(
                "SKIPPED(setup): Native Test Client binary missing at {}; run apps/desktop/scripts/build-test-peer.sh then apps/desktop/scripts/cockpit-setup.sh",
                native_peer::TEST_PEER_BIN_RELATIVE
            ),
        );
    };
    let Some(access_code) = run_meta.access_code.clone() else {
        return infra_fail_outcome(
            scenario,
            "INFRA-FAIL parent join did not provide a real access code",
        );
    };

    let document = match open_sacrificial_document() {
        Ok(document) => document,
        Err(error) => {
            return infra_fail_outcome(scenario, format!("INFRA-FAIL sacrificial document: {error}"))
        }
    };
    let socket_path = native_peer_socket_path(&writer.dir);
    let token = native_peer_token();
    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            close_sacrificial_document(&document);
            return infra_fail_outcome(
                scenario,
                format!(
                    "INFRA-FAIL create native-peer socket {}: {error}",
                    socket_path.display()
                ),
            );
        }
    };

    let peer_identity_env = format!("p-rcn2nhost-{}", rand_hex());
    let peer_stdout_path = writer.dir.join("rc-n2n-peer.stdout.log");
    let peer_stderr_path = writer.dir.join("rc-n2n-peer.stderr.log");
    let (peer_stdout, peer_stderr) = match (
        File::create(&peer_stdout_path),
        File::create(&peer_stderr_path),
    ) {
        (Ok(out), Ok(err)) => (out, err),
        _ => {
            close_sacrificial_document(&document);
            let _ = fs::remove_file(&socket_path);
            return infra_fail_outcome(
                scenario,
                "INFRA-FAIL create Native Test Client log artifacts",
            );
        }
    };
    let mut child = match Command::new(&peer_path)
        .arg(TEST_CASE_ARG)
        .arg(NATIVE_PEER_CONTROL_HOST_SELECTOR)
        .env(NATIVE_PEER_SOCKET_ENV, &socket_path)
        .env(NATIVE_PEER_TOKEN_ENV, &token)
        .env(NATIVE_PEER_ROOM_ENV, &access_code)
        .env(NATIVE_PEER_IDENTITY_ENV, &peer_identity_env)
        .env(NATIVE_PEER_TARGET_APP_ENV, "TextEdit")
        .env(NATIVE_PEER_TARGET_TITLE_ENV, &document.marker)
        .env("PETAL_DISABLE_AUDIO", "1")
            // #823: peers must not pollute the Dock or steal focus (their
            // self-activation broke RC-N2N's AX focus live).
            .env("PETAL_ACCESSORY_UI", "1")
        .stdout(Stdio::from(peer_stdout))
        .stderr(Stdio::from(peer_stderr))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            close_sacrificial_document(&document);
            let _ = fs::remove_file(&socket_path);
            return infra_fail_outcome(
                scenario,
                format!("INFRA-FAIL launch Native Test Client: {error}"),
            );
        }
    };
    children.record_native_peer(&child);
    let _ = writer.write(
        "native-peer-launch",
        Some(scenario.id),
        serde_json::json!({
            "role": "control-host",
            "pid": child.id(),
            "binary": peer_path.display().to_string(),
            "socket": socket_path.display().to_string(),
            "sacrificialDocument": document.path.display().to_string(),
            "targetTitleMarker": document.marker,
            "stdoutArtifact": peer_stdout_path.display().to_string(),
            "stderrArtifact": peer_stderr_path.display().to_string(),
            "liveExecutionAttempted": true,
        }),
    );

    // Everything past this point must tear the peer and the document down on
    // every path, so the body runs in a closure whose result is post-processed.
    macro_rules! give_up {
        ($writer:expr, $child:expr, $step:expr, $detail:expr) => {{
            let detail: String = $detail;
            let _ = stop_native_peer_with_failure(
                $writer,
                scenario.id,
                $child,
                $step,
                &detail,
                &peer_stdout_path,
                &peer_stderr_path,
            );
            let _ = fs::remove_file(&socket_path);
            close_sacrificial_document(&document);
            return infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"));
        }};
    }

    let stream = match tokio::time::timeout(NATIVE_PEER_TIMEOUT, listener.accept()).await {
        Ok(Ok((stream, _))) => stream,
        Ok(Err(error)) => give_up!(
            writer,
            &mut child,
            "accept",
            format!("accept Native Test Client socket: {error}")
        ),
        Err(_) => give_up!(
            writer,
            &mut child,
            "accept",
            "native peer accept timed out".to_string()
        ),
    };
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = TokioBufReader::new(read_half);
    let ready = match read_native_peer_message_with_timeout(&mut reader, "readiness").await {
        Ok(message) if message.token == token && message.event == "ready" => message,
        Ok(message) => give_up!(
            writer,
            &mut child,
            "readiness",
            format!("unauthenticated native peer readiness: {}", message.event)
        ),
        Err(error) => give_up!(
            writer,
            &mut child,
            "readiness",
            format!("native peer readiness: {error}")
        ),
    };
    let (Some(host_window_id), Some(host_identity)) =
        (ready.shared_window_id, ready.peer_identity.clone())
    else {
        give_up!(
            writer,
            &mut child,
            "readiness",
            "native peer ready message omitted its shared window or identity".to_string()
        )
    };
    if ready.accessibility_trusted != Some(true) {
        give_up!(
            writer,
            &mut child,
            "readiness",
            format!(
                "the Native Test Client is not Accessibility-trusted, so it can accept a control \
                 grant and inject nothing. Grant Accessibility to {} in System Settings > Privacy \
                 & Security > Accessibility (apps/desktop/scripts/cockpit-setup.sh prints the \
                 exact steps), then re-run",
                peer_path.display()
            )
        );
    }

    // The controller side must actually be rendering the peer's window before
    // control means anything: a grant against a window with no decoded frame
    // would prove nothing about the direction under test.
    let readiness_started = Instant::now();
    let mut readiness = native_peer::ReceiverReadinessTracker::new();
    loop {
        match crate::compositor::cockpit_remote_window_binding(app, &host_identity, host_window_id)
        {
            Ok(binding) if readiness.observe(&binding) => break,
            Ok(_) | Err(_) if readiness_started.elapsed() >= NATIVE_PEER_TIMEOUT => {
                give_up!(
                    writer,
                    &mut child,
                    "remote-window",
                    readiness.timeout_error(&host_identity, host_window_id)
                )
            }
            Ok(_) => tokio::time::sleep(native_peer::RECEIVER_READINESS_SAMPLE_INTERVAL).await,
            Err(error) => {
                readiness.observe_error(&error);
                tokio::time::sleep(native_peer::RECEIVER_READINESS_SAMPLE_INTERVAL).await;
            }
        }
    }

    match remote_control_is_offered(&host_identity, host_window_id) {
        Ok(true) => {}
        Ok(false) => give_up!(
            writer,
            &mut child,
            "remote-control-offer",
            "the peer's shared window does not advertise remote control, so the controller's \
             grant can never arm the overlay and the drive would publish nothing. Check the \
             peer's remote-control-allowed setting"
                .to_string()
        ),
        Err(error) => give_up!(
            writer,
            &mut child,
            "remote-control-offer",
            format!("could not read the remote window's control availability: {error}")
        ),
    }

    let document_before = match read_sacrificial_document(&document.marker) {
        Ok(text) => text,
        Err(error) => give_up!(
            writer,
            &mut child,
            "document",
            format!("read the sacrificial document before the drive: {error}")
        ),
    };
    if document_before.contains(rc_n2n::KEYSTONE_TEXT) {
        give_up!(
            writer,
            &mut child,
            "document",
            format!(
                "the sacrificial document already contained '{}' before the drive, so 'the text \
                 landed' could not have distinguished this run from any other",
                rc_n2n::KEYSTONE_TEXT
            )
        );
    }

    // From here the run is measuring the product. Arm the ledger last so it
    // holds this scenario's traffic and nothing else.
    crate::remote_control::cockpit_ledger::reset();
    if let Err(error) = crate::remote_control::remote_control_set_active(
        app.clone(),
        host_window_id,
        Some(host_identity.clone()),
        true,
    )
    .await
    {
        give_up!(
            writer,
            &mut child,
            "request",
            format!("the controller could not publish a control request: {error}")
        );
    }
    let granted = await_controller_grant(host_window_id, Duration::from_secs(15)).await;
    let _ = writer.write(
        "rc-n2n-grant",
        Some(scenario.id),
        serde_json::json!({
            "hostIdentity": host_identity,
            "hostWindowId": host_window_id,
            "controllerIdentity": run_meta.native_identity,
            "granted": granted,
        }),
    );

    // The host's AX shortcut route (select-all/copy via AXSelectedTextRange,
    // the reliable background-capable path) only trusts a target resolved
    // from GENUINE focus -- AXFocusedUIElement must be the text area, or the
    // destructive-op guard passes Cmd+A through to a CGEvent key-equivalent
    // that does nothing for a non-key window. Measured live: typing landed
    // (the unicode path is focus-independent, which masks this), while
    // Cmd+A/Cmd+C resolved 'via BFS fallback (untrusted)' and no-opped.
    // Activate TextEdit AND set AXFocused on the text area explicitly so the
    // host resolves trusted focus.
    let focus_result = osascript(&[
        "tell application \"System Events\"".to_string(),
        "tell process \"TextEdit\"".to_string(),
        "set frontmost to true".to_string(),
        format!(
            "set value of attribute \"AXFocused\" of text area 1 of scroll area 1 of (first window whose name contains \"{}\") to true",
            document.marker
        ),
        "end tell".to_string(),
        "end tell".to_string(),
    ]);
    if let Err(error) = focus_result {
        log::warn!(
            "test-cockpit: could not pre-focus the sacrificial text area (AX shortcuts may fall back): {error}"
        );
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Drive the keystone set through the REAL control route. Each step is a
    // DOM dispatch into the overlay webview; nothing here builds a draft.
    let mut drive_errors: Vec<String> = Vec::new();
    for step in rc_n2n::keystone_steps() {
        // Single-Mac wrinkle: the PEER (another `desktop` binary starting up
        // on this machine) re-activates itself during the drive, backgrounding
        // TextEdit. Backgrounded, AXFocusedUIElement stops resolving, the
        // host's AX shortcut route loses trusted focus, and Cmd+A/Cmd+C
        // silently no-op (measured live via a frontmost timeline). Re-assert
        // TextEdit right before each chord; on two real Macs this is a no-op
        // condition that never fires.
        if matches!(step, rc_n2n::DriveStep::MetaChord(_)) {
            let _ = osascript(&[
                "tell application \"System Events\"".to_string(),
                "tell process \"TextEdit\"".to_string(),
                "set frontmost to true".to_string(),
                "end tell".to_string(),
                "end tell".to_string(),
            ]);
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let script = rc_n2n::drive_script(&step);
        if let Err(error) =
            crate::compositor::cockpit_eval_in_control_overlay(app, &host_identity, host_window_id, script)
        {
            drive_errors.push(format!("{}: {error}", rc_n2n::step_id(&step)));
        }
        tokio::time::sleep(Duration::from_millis(rc_n2n::DRIVE_STEP_SETTLE_MS.into())).await;
    }
    if !drive_errors.is_empty() {
        give_up!(
            writer,
            &mut child,
            "drive",
            format!(
                "could not reach the control overlay for {} of the keystone steps: {}",
                drive_errors.len(),
                drive_errors.join("; ")
            )
        );
    }
    // The host's replay queue is asynchronous, and a single fixed settle
    // converts a slow AX sequence on a loaded Mac into a product TEST-FAIL
    // ("the host never replayed ...") -- a verdict drawn from rig timing
    // (#819 review). Poll the effect report until the replay ledger reaches
    // the published count or stops growing, deadline-bounded; transport and
    // parse failures stay hard give_up!s exactly as before.
    let expected_replays = controller_ledger_projection(host_window_id).0.len();
    let mut host_report = serde_json::Value::Null;
    let mut host_effects = Vec::new();
    let mut pressed_inputs_after = 0usize;
    let mut sessions_after = 0usize;
    let mut previous_count: Option<usize> = None;
    const REPLAY_POLL_LIMIT: usize = 12;
    for poll in 1..=REPLAY_POLL_LIMIT {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if let Err(error) = write_native_peer_message_with_timeout(
            &mut write_half,
            &NativePeerSocketMessage {
                token: token.clone(),
                event: "report".to_string(),
                ..Default::default()
            },
            "report write",
        )
        .await
        {
            give_up!(
                writer,
                &mut child,
                "report",
                format!("request the host-side effect report: {error}")
            );
        }
        let report =
            match read_native_peer_message_with_timeout(&mut reader, "report response").await {
                Ok(message) if message.token == token && message.event == "report" => message,
                Ok(message) => give_up!(
                    writer,
                    &mut child,
                    "report",
                    format!("unauthenticated native peer report: {}", message.event)
                ),
                Err(error) => give_up!(
                    writer,
                    &mut child,
                    "report",
                    format!("native peer report: {error}")
                ),
            };
        let Some(report_payload) = report.host_report.clone() else {
            give_up!(
                writer,
                &mut child,
                "report",
                "native peer report carried no host-side effect record".to_string()
            )
        };
        let (effects, pressed, sessions) =
            match host_report_projection(&report_payload, &run_meta.native_identity) {
                Ok(projection) => projection,
                Err(error) => give_up!(
                    writer,
                    &mut child,
                    "report",
                    format!("host-side effect report: {error}")
                ),
            };
        let count = effects.len();
        host_report = report_payload;
        host_effects = effects;
        pressed_inputs_after = pressed;
        sessions_after = sessions;
        let complete = expected_replays > 0 && count >= expected_replays;
        let stalled = previous_count == Some(count) && poll >= 3;
        previous_count = Some(count);
        if complete || stalled {
            break;
        }
    }

    let document_after = match read_sacrificial_document(&document.marker) {
        Ok(text) => text,
        Err(error) => give_up!(
            writer,
            &mut child,
            "document",
            format!("read the sacrificial document after the drive: {error}")
        ),
    };
    let selection = read_sacrificial_selection(&document.marker);
    let (driven, statuses) = controller_ledger_projection(host_window_id);

    let verdict = rc_n2n::evaluate(rc_n2n::RcN2nObservations {
        driven: &driven,
        statuses: &statuses,
        host_effects: &host_effects,
        pressed_inputs_after,
        sessions_after,
        document_text: &document_after,
        selection_after_select_all: selection.as_deref(),
        expected_text: rc_n2n::KEYSTONE_TEXT,
    });

    let _ = writer.write(
        "rc-n2n-evidence",
        Some(scenario.id),
        serde_json::json!({
            "driven": driven,
            "statuses": statuses,
            "hostEffects": host_effects,
            "hostReport": host_report,
            "pressedInputsAfter": pressed_inputs_after,
            "sessionsAfter": sessions_after,
            "documentBefore": document_before,
            "documentAfter": document_after,
            "selectionAfterSelectAll": selection,
        }),
    );

    let shutdown_result = write_native_peer_message_with_timeout(
        &mut write_half,
        &NativePeerSocketMessage {
            token,
            event: "shutdown".to_string(),
            ..Default::default()
        },
        "shutdown write",
    )
    .await;
    let exit = child.wait();
    retain_native_peer_log_tail(&peer_stdout_path);
    retain_native_peer_log_tail(&peer_stderr_path);
    let _ = fs::remove_file(&socket_path);
    close_sacrificial_document(&document);
    if let Err(error) = shutdown_result {
        record_native_peer_failure(
            writer,
            scenario.id,
            "shutdown",
            &format!("native peer shutdown command: {error}"),
            &peer_stdout_path,
            &peer_stderr_path,
            &exit,
        );
    }

    match verdict {
        rc_n2n::RcVerdict::Pass(detail) => ScenarioOutcome {
            scenario_id: scenario.id.to_string(),
            verdict: ScenarioVerdict::Pass,
            message: format!("{} PASS {detail}", scenario.id),
            delivered_fps: 0.0,
            delivered_width: 0,
            delivered_height: 0,
            assertions: vec![
                AssertionOutcome {
                    name: "native-controller-published-through-the-real-route".to_string(),
                    passed: true,
                    detail: format!("{} controller messages recorded", driven.len()),
                },
                AssertionOutcome {
                    name: "host-side-effects".to_string(),
                    passed: true,
                    detail: format!(
                        "{} replayed inputs, all applied; document contains '{}'",
                        host_effects.len(),
                        rc_n2n::KEYSTONE_TEXT
                    ),
                },
            ],
        },
        rc_n2n::RcVerdict::TestFail(detail) => {
            let mut outcome = infra_fail_outcome(scenario, format!("{} TEST-FAIL {detail}", scenario.id));
            outcome.verdict = ScenarioVerdict::TestFail;
            outcome.message = format!("{} TEST-FAIL {detail}", scenario.id);
            outcome
        }
        rc_n2n::RcVerdict::InfraFail(detail) => {
            infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"))
        }
    }
}

#[cfg(not(target_os = "macos"))]
async fn run_remote_control_native_to_native_scenario(
    _app: &AppHandle,
    scenario: ScenarioSpec,
    _run_meta: &RunMeta,
    _writer: &mut ResultsWriter,
    _children: &mut RunChildren,
) -> ScenarioOutcome {
    infra_fail_outcome(scenario, "RC-N2N is macOS-only")
}

/// Find the remote window a web peer is publishing: its owner identity and the
/// window id parsed from the `petal-window-<id>` track name.
#[cfg(target_os = "macos")]
async fn await_remote_share_owner(app: &AppHandle, timeout: Duration) -> Option<(String, u32)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
            let snapshot = diagnostics.snapshot();
            if let Some(track) = recv_window_track(&snapshot) {
                if let (Some(owner), Some(window_id)) =
                    (track.owner_identity.clone(), track.window_id)
                {
                    return Some((owner, window_id));
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// RC-N2W (journey RC-07, #819): the same native controller, a WEB host.
///
/// This leg is a DELIVERY proof and nothing more. A browser cannot inject OS
/// input, so the harness records what it received and never claims an input
/// was applied; the verdict language says so rather than borrowing RC-N2N's.
/// What it does prove is the half RC-N2N cannot isolate: that the native
/// controller's request, grant handshake and input messages are well formed
/// and arrive intact at an independent peer implementation.
#[cfg(target_os = "macos")]
async fn run_remote_control_native_to_web_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    access_code: &str,
    run_meta: &RunMeta,
    writer: &mut ResultsWriter,
    children: &mut RunChildren,
) -> ScenarioOutcome {
    let _ = writer.write(
        "scenario-scaffold",
        Some(scenario.id),
        serde_json::json!({
            "tier": scenario.tier,
            "sourceIssue": source_issue_for_scenario(scenario.id),
            "coverageKind": coverage_kind_for_scenario(scenario.id),
            "proves": "delivery of the native controller's request/grant/input messages to an independent peer",
            "doesNotProve": "that any input was applied -- a browser cannot inject OS input",
            "oracle": "rc_n2n::evaluate_delivery_only over the controller publish ledger and the harness received-input ledger",
        }),
    );
    let web_peer = match spawn_web_peer(scenario, access_code, &writer.dir) {
        Ok(peer) => peer,
        Err(error) => return infra_fail_outcome(scenario, error),
    };
    children.record_web_peer(&web_peer);
    let _ = writer.write(
        "web-peer",
        Some(scenario.id),
        serde_json::json!({ "mode": web_peer.mode, "url": web_peer.url, "pid": web_peer.pid() }),
    );

    let Some((host_identity, host_window_id)) =
        await_remote_share_owner(app, Duration::from_secs(45)).await
    else {
        return infra_fail_outcome(
            scenario,
            "INFRA-FAIL the web peer never published a window share for the controller to target",
        );
    };
    let readiness_started = Instant::now();
    let mut readiness = native_peer::ReceiverReadinessTracker::new();
    loop {
        match crate::compositor::cockpit_remote_window_binding(app, &host_identity, host_window_id)
        {
            Ok(binding) if readiness.observe(&binding) => break,
            Ok(_) | Err(_) if readiness_started.elapsed() >= NATIVE_PEER_TIMEOUT => {
                return infra_fail_outcome(
                    scenario,
                    format!(
                        "INFRA-FAIL {}",
                        readiness.timeout_error(&host_identity, host_window_id)
                    ),
                )
            }
            Ok(_) => tokio::time::sleep(native_peer::RECEIVER_READINESS_SAMPLE_INTERVAL).await,
            Err(error) => {
                readiness.observe_error(&error);
                tokio::time::sleep(native_peer::RECEIVER_READINESS_SAMPLE_INTERVAL).await;
            }
        }
    }

    match remote_control_is_offered(&host_identity, host_window_id) {
        Ok(true) => {}
        Ok(false) => {
            return infra_fail_outcome(
                scenario,
                "INFRA-FAIL the web peer's share does not advertise remote control, so the \
                 controller's grant can never arm the overlay and the drive would publish nothing",
            )
        }
        Err(error) => {
            return infra_fail_outcome(
                scenario,
                format!("INFRA-FAIL could not read the remote window's control availability: {error}"),
            )
        }
    }

    crate::remote_control::cockpit_ledger::reset();
    if let Err(error) = crate::remote_control::remote_control_set_active(
        app.clone(),
        host_window_id,
        Some(host_identity.clone()),
        true,
    )
    .await
    {
        return infra_fail_outcome(
            scenario,
            format!("INFRA-FAIL the controller could not publish a control request: {error}"),
        );
    }
    let granted_locally = await_controller_grant(host_window_id, Duration::from_secs(15)).await;
    let mut drive_errors: Vec<String> = Vec::new();
    for step in rc_n2n::delivery_keystone_steps() {
        let script = rc_n2n::drive_script(&step);
        if let Err(error) = crate::compositor::cockpit_eval_in_control_overlay(
            app,
            &host_identity,
            host_window_id,
            script,
        ) {
            drive_errors.push(format!("{}: {error}", rc_n2n::step_id(&step)));
        }
        tokio::time::sleep(Duration::from_millis(rc_n2n::DRIVE_STEP_SETTLE_MS.into())).await;
    }
    if !drive_errors.is_empty() {
        return infra_fail_outcome(
            scenario,
            format!(
                "INFRA-FAIL could not reach the control overlay for {} of the keystone steps: {}",
                drive_errors.len(),
                drive_errors.join("; ")
            ),
        );
    }

    let Some(report) = await_web_report(app, scenario, writer).await else {
        return infra_fail_outcome(
            scenario,
            "INFRA-FAIL the web peer never reported its received-input ledger",
        );
    };
    if web_report_declares_infra_failure(&report) {
        return infra_fail_outcome(
            scenario,
            format!(
                "INFRA-FAIL web peer could not measure: {}",
                report_text_field(&report.payload, "detail").unwrap_or("no detail")
            ),
        );
    }
    let received_kinds: Vec<String> = report
        .payload
        .get("receivedControlKinds")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let web_granted = report_payload_bool(&report.payload, &["controlGranted"]);
    let (driven, _statuses) = controller_ledger_projection(host_window_id);
    let verdict = rc_n2n::evaluate_delivery_only(&driven, &received_kinds, granted_locally && web_granted);
    let _ = writer.write(
        "rc-n2w-evidence",
        Some(scenario.id),
        serde_json::json!({
            "hostIdentity": host_identity,
            "hostWindowId": host_window_id,
            "controllerIdentity": run_meta.native_identity,
            "grantedOnTheController": granted_locally,
            "grantedOnTheWebHost": web_granted,
            "driven": driven,
            "receivedControlKinds": received_kinds,
        }),
    );
    match verdict {
        rc_n2n::RcVerdict::Pass(detail) => ScenarioOutcome {
            scenario_id: scenario.id.to_string(),
            verdict: ScenarioVerdict::Pass,
            message: format!("{} PASS {detail}", scenario.id),
            delivered_fps: 0.0,
            delivered_width: 0,
            delivered_height: 0,
            assertions: vec![AssertionOutcome {
                name: "native-controller-message-delivery".to_string(),
                passed: true,
                detail: format!("web peer received kinds: {}", received_kinds.join(", ")),
            }],
        },
        rc_n2n::RcVerdict::TestFail(detail) => {
            let mut outcome =
                infra_fail_outcome(scenario, format!("{} TEST-FAIL {detail}", scenario.id));
            outcome.verdict = ScenarioVerdict::TestFail;
            outcome.message = format!("{} TEST-FAIL {detail}", scenario.id);
            outcome
        }
        rc_n2n::RcVerdict::InfraFail(detail) => {
            infra_fail_outcome(scenario, format!("INFRA-FAIL {detail}"))
        }
    }
}

#[cfg(not(target_os = "macos"))]
async fn run_remote_control_native_to_web_scenario(
    _app: &AppHandle,
    scenario: ScenarioSpec,
    _access_code: &str,
    _run_meta: &RunMeta,
    _writer: &mut ResultsWriter,
    _children: &mut RunChildren,
) -> ScenarioOutcome {
    infra_fail_outcome(scenario, "RC-N2W is macOS-only")
}

/// Per-kind scaffold metadata for the P-3 gap scenarios: the subcases the live
/// verdict will exercise, the unit-tested pass-criteria oracle it will call,
/// and the honest "what live layer is missing" reason.
fn gap_scaffold_metadata(kind: ScenarioKind) -> (Vec<&'static str>, &'static str, &'static str) {
    match kind {
        ScenarioKind::MultiWindowShare => (
            vec![
                "share-four-windows-at-once",
                "focus-weighted-cap",
                "non-focused-stay-live",
                "no-keyframe-storm",
            ],
            "gap_oracles::evaluate_focus_weighted_cap",
            "SHARE-05 needs the cockpit runner to open and share four real native windows at once and sample each share's delivered fps on a receiver before it can make a real verdict",
        ),
        ScenarioKind::MultiDisplayShare => (
            vec![
                "share-windows-across-displays",
                "capture-and-composite-survive",
                "window-dragged-across-displays-keeps-flowing",
            ],
            "native_peer::evaluate_independent_move (cross-display translation) + gap_oracles::evaluate_focus_weighted_cap",
            "SHARE-06 needs >=2 physical displays/Spaces and live capture+composite across them; the cross-display drag is not auto-driven headlessly yet",
        ),
        ScenarioKind::FullDesktopShare => (
            vec![
                "share-whole-display",
                "composites-on-peer",
                "sharer-border-persists",
            ],
            "gap_oracles::assert_no_text_overflow is N/A; verdict uses receiver liveness + sharer-border presence (#199)",
            "SHARE-10 needs a live full-display capture published to a receiver and the sharer-border overlay checked around the whole display (#199); not auto-driven headlessly yet",
        ),
        ScenarioKind::CameraBitrateScaling => (
            vec![
                "publish-camera-at-tier",
                "measure-bitrate-vs-resolution",
                "bitrate-tracks-tier-not-just-fps",
            ],
            "gap_oracles::assert_bitrate_tracks_tier",
            "CAM-03 needs a live web camera peer and native recv bitrate/resolution telemetry sampled together before it can make a real verdict (#246)",
        ),
        ScenarioKind::CameraStall => (
            vec![
                "publish-camera",
                "sample-decoded-frame-count-over-time",
                "no-new-frame-for-n-watchdog",
            ],
            "gap_oracles::detect_camera_stall",
            "CAM-04 needs a live camera peer and a decoded-frame-count time series on the gallery tile before the stall watchdog can make a real verdict (#247)",
        ),
        ScenarioKind::JoinRoom => (
            vec![
                "one-click-join",
                "native-roster-snapshot",
                "web-roster-snapshot",
                "rosters-match-all-sides",
            ],
            "gap_oracles::assert_rosters_match",
            "ROOM-01 needs a live web peer whose roster can be read back and compared against the native presence snapshot before it can make a real verdict",
        ),
        ScenarioKind::UiScreenshot => (
            vec![
                "capture-view-screenshot",
                "measure-text-boxes-in-webview",
                "assert-scrollwidth-le-clientwidth",
            ],
            "gap_oracles::assert_no_text_overflow (scrollWidth<=clientWidth, UI-text hard rule)",
            "UI journeys need a live screenshot of the view plus an in-webview scrollWidth/clientWidth measurement of every text element; neither is auto-driven headlessly yet",
        ),
        _ => (
            vec!["unknown-scaffold"],
            "none",
            "scenario was routed through the gap scaffold unexpectedly",
        ),
    }
}

/// Honest scaffold for the P-3 gap journeys. Writes a `scenario-scaffold` event
/// naming the subcases + the unit-tested oracle the live verdict will call, then
/// returns INFRA-FAIL — it NEVER false-passes. Mirrors run_scaffold_only_scenario
/// / run_native_to_native_scenario. The pass-criteria logic is covered by
/// `gap_oracles`' unit tests.
async fn run_gap_scaffold_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    writer: &mut ResultsWriter,
) -> ScenarioOutcome {
    let (subcases, oracle, missing_live_layer) = gap_scaffold_metadata(scenario.kind);
    let display_count = available_display_count(app);
    let _ = writer.write(
        "scenario-scaffold",
        Some(scenario.id),
        serde_json::json!({
            "tier": scenario.tier,
            "sourceIssue": source_issue_for_scenario(scenario.id),
            "coverageKind": coverage_kind_for_scenario(scenario.id),
            "subcases": subcases,
            "oracle": oracle,
            "displayCount": display_count,
            "liveExecutionAttempted": false,
            "destructiveExecutionAttempted": false,
            "nativeShareAttempted": false,
            "webPeerSpawnAttempted": false,
        }),
    );

    // SHARE-06 additionally needs the hardware; surface it as a specific skip
    // reason when there is only one display so the verdict is honest about why.
    if scenario.kind == ScenarioKind::MultiDisplayShare && display_count.unwrap_or(1) < 2 {
        return skipped_outcome(
            scenario,
            format!(
                "SKIPPED(hardware): only {} display(s) detected; SHARE-06 multi-display share requires at least 2",
                display_count.unwrap_or(0)
            ),
        );
    }

    infra_fail_outcome(
        scenario,
        format!(
            "{missing_live_layer}; oracle {oracle} is unit-tested but the live orchestration is not auto-driven yet, so this scaffold refuses to false-pass"
        ),
    )
}

fn multi_peer_outcome_from_reports(
    scenario: ScenarioSpec,
    reports: &[WebCockpitReport],
    expected: usize,
    native: Option<&MultiPeerNativeEvidence>,
) -> ScenarioOutcome {
    let distinct_count = reports.len();
    let mut assertions = Vec::new();
    let distinct_ok = distinct_count == expected;
    assertions.push(AssertionOutcome {
        name: "multi-peer-distinct-reports".to_string(),
        passed: distinct_ok,
        detail: format!("expected={expected}; observed={distinct_count}"),
    });

    let parsed = reports
        .iter()
        .map(|report| {
            serde_json::from_value::<MultiPeerRosterReport>(report.payload.clone())
                .map(|payload| (report, payload))
        })
        .collect::<Result<Vec<_>, _>>();
    let schema_ok = parsed.is_ok();
    assertions.push(AssertionOutcome {
        name: "multi-peer-typed-roster-reports".to_string(),
        passed: schema_ok,
        detail: if schema_ok {
            "all terminal reports contain the required typed roster proof".to_string()
        } else {
            "one or more terminal reports are missing or malformed roster-proof fields".to_string()
        },
    });

    let mut browser_ok = false;
    let mut native_ok = false;
    if let Ok(parsed) = &parsed {
        let fingerprints = parsed
            .iter()
            .map(|(_, payload)| payload.roster_fingerprint.as_str())
            .collect::<HashSet<_>>();
        browser_ok = distinct_ok
            && parsed.iter().all(|(report, payload)| {
                payload.ok
                    && payload.reporter_id == report.sender
                    && payload.participant_count == expected + 1
                    && payload.remote_participant_count == expected
                    && payload.roster_fingerprint_algorithm == "sha-256"
                    && payload.roster_fingerprint.len() == 64
                    && payload
                        .roster_fingerprint
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    && payload.roster_includes_reporter
                    && payload.roster_unique
            })
            && fingerprints.len() == 1;
        assertions.push(AssertionOutcome {
            name: "multi-peer-browser-roster-consensus".to_string(),
            passed: browser_ok,
            detail: format!(
                "expectedParticipants={}; expectedRemote={expected}; commonFingerprint={}",
                expected + 1,
                fingerprints.len() == 1
            ),
        });

        if let Some(native) = native {
            let before = normalized_roster(native.roster_before.clone());
            let after = normalized_roster(native.roster_after.clone());
            let native_fingerprint = roster_fingerprint(&after);
            let common_browser_fingerprint = parsed
                .first()
                .map(|(_, payload)| payload.roster_fingerprint.as_str());
            let reporter_ids = parsed
                .iter()
                .map(|(_, payload)| payload.reporter_id.as_str())
                .collect::<HashSet<_>>();
            native_ok = before == after
                && after.len() == expected + 1
                && reporter_ids.len() == expected
                && reporter_ids
                    .iter()
                    .all(|identity| after.iter().any(|item| item == identity))
                && common_browser_fingerprint == Some(native_fingerprint.as_str())
                && native.menubar_in_meeting
                && native.menubar_participant_count as usize == expected + 1;
            assertions.push(AssertionOutcome {
                name: "multi-peer-native-roster-and-menubar".to_string(),
                passed: native_ok,
                detail: format!(
                    "stableRoster={}; nativeCount={}; reportersPresent={}; fingerprintMatches={}; inMeeting={}; menubarCount={}",
                    before == after,
                    after.len(),
                    reporter_ids.iter().all(|identity| after.iter().any(|item| item == identity)),
                    common_browser_fingerprint == Some(native_fingerprint.as_str()),
                    native.menubar_in_meeting,
                    native.menubar_participant_count
                ),
            });
            assertions.push(AssertionOutcome {
                name: "multi-peer-clock-calibration".to_string(),
                passed: native.clock_calibration_ok == Some(true),
                detail: match native.clock_calibration_ok {
                    Some(true) => "native clock-calibration evidence passed".to_string(),
                    Some(false) => "native clock-calibration evidence failed".to_string(),
                    None => {
                        "authoritative per-peer clock-calibration evidence unavailable".to_string()
                    }
                },
            });
            assertions.push(AssertionOutcome {
                name: "multi-peer-keyframe-storm".to_string(),
                passed: native.keyframe_storm_free == Some(true),
                detail: match native.keyframe_storm_free {
                    Some(true) => "native keyframe-storm guard passed".to_string(),
                    Some(false) => "native keyframe-storm guard failed".to_string(),
                    None => "authoritative per-peer keyframe attribution unavailable".to_string(),
                },
            });
        }
    }
    let subcase_evidence_available = native.is_some_and(|evidence| {
        evidence.clock_calibration_ok.is_some() && evidence.keyframe_storm_free.is_some()
    });
    let subcases_ok = native.is_some_and(|evidence| {
        evidence.clock_calibration_ok == Some(true) && evidence.keyframe_storm_free == Some(true)
    });
    let all_ok = distinct_ok && schema_ok && browser_ok && native_ok && subcases_ok;
    ScenarioOutcome {
        scenario_id: scenario.id.to_string(),
        verdict: if all_ok {
            ScenarioVerdict::Pass
        } else if !distinct_ok || !schema_ok || !subcase_evidence_available {
            ScenarioVerdict::InfraFail
        } else if distinct_count >= expected {
            ScenarioVerdict::TestFail
        } else {
            ScenarioVerdict::InfraFail
        },
        message: if all_ok {
            format!(
                "{} PASS proved one stable {}-participant roster across native presence, menubar, and {expected} authenticated web peers",
                scenario.id,
                expected + 1,
            )
        } else if !distinct_ok {
            format!(
                "{} INFRA-FAIL observed only {distinct_count}/{expected} distinct web peer reports",
                scenario.id
            )
        } else if !schema_ok {
            format!(
                "{} INFRA-FAIL MULTI-3 terminal reports did not contain the required typed roster evidence",
                scenario.id
            )
        } else if !subcase_evidence_available {
            format!(
                "{} INFRA-FAIL authoritative clock-calibration or keyframe-storm evidence is unavailable",
                scenario.id
            )
        } else if distinct_ok {
            format!(
                "{} TEST-FAIL MULTI-3 reports arrived but roster consensus or native UI evidence failed",
                scenario.id
            )
        } else {
            unreachable!("distinct report failure handled before typed evidence")
        },
        delivered_fps: 0.0,
        delivered_width: 0,
        delivered_height: 0,
        assertions,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyframeTrackDelta {
    sid: String,
    direction: String,
    key_frames_encoded: u64,
    key_frames_decoded: u64,
    pli_count: u64,
    fir_count: u64,
    counter_reset: bool,
    storm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyframeControlEvidence {
    sampling_duration_ms: u64,
    evidence_available: bool,
    track_set_changed: bool,
    storm: bool,
    tracks: Vec<KeyframeTrackDelta>,
}

fn keyed_video_tracks(
    tracks: &[crate::diagnostics::TrackHealth],
) -> Option<HashMap<(String, String), &crate::diagnostics::TrackHealth>> {
    let mut keyed = HashMap::new();
    for track in tracks.iter().filter(|track| track.kind == "video") {
        if track.sid.trim().is_empty() || track.direction.trim().is_empty() {
            return None;
        }
        if keyed
            .insert((track.sid.clone(), track.direction.clone()), track)
            .is_some()
        {
            return None;
        }
    }
    Some(keyed)
}

fn keyframe_control_evidence(
    baseline: &[crate::diagnostics::TrackHealth],
    end: &[crate::diagnostics::TrackHealth],
    sampling_duration_ms: u64,
    per_track_storm_budget: u64,
) -> KeyframeControlEvidence {
    let Some(baseline) = keyed_video_tracks(baseline) else {
        return KeyframeControlEvidence {
            sampling_duration_ms,
            evidence_available: false,
            track_set_changed: true,
            storm: true,
            tracks: vec![],
        };
    };
    let Some(end) = keyed_video_tracks(end) else {
        return KeyframeControlEvidence {
            sampling_duration_ms,
            evidence_available: false,
            track_set_changed: true,
            storm: true,
            tracks: vec![],
        };
    };
    if baseline.is_empty()
        || baseline.keys().collect::<HashSet<_>>() != end.keys().collect::<HashSet<_>>()
    {
        return KeyframeControlEvidence {
            sampling_duration_ms,
            evidence_available: false,
            track_set_changed: true,
            storm: true,
            tracks: vec![],
        };
    }
    let mut keys = baseline.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    let mut tracks = Vec::with_capacity(keys.len());
    for (sid, direction) in keys {
        let previous = baseline[&(sid.clone(), direction.clone())];
        let current = end[&(sid.clone(), direction.clone())];
        let counters = [
            (previous.key_frames_encoded, current.key_frames_encoded),
            (previous.key_frames_decoded, current.key_frames_decoded),
            (previous.pli_count, current.pli_count),
            (previous.fir_count, current.fir_count),
        ];
        let counter_reset = counters.iter().any(|(before, after)| after < before);
        let mut delta = KeyframeTrackDelta {
            sid,
            direction,
            key_frames_encoded: 0,
            key_frames_decoded: 0,
            pli_count: 0,
            fir_count: 0,
            counter_reset,
            storm: counter_reset,
        };
        if !counter_reset {
            delta.key_frames_encoded =
                u64::from(current.key_frames_encoded - previous.key_frames_encoded);
            delta.key_frames_decoded =
                u64::from(current.key_frames_decoded - previous.key_frames_decoded);
            delta.pli_count = u64::from(current.pli_count - previous.pli_count);
            delta.fir_count = u64::from(current.fir_count - previous.fir_count);
            delta.storm = delta.key_frames_encoded
                + delta.key_frames_decoded
                + delta.pli_count
                + delta.fir_count
                > per_track_storm_budget;
        }
        tracks.push(delta);
    }
    KeyframeControlEvidence {
        sampling_duration_ms,
        evidence_available: sampling_duration_ms > 0,
        track_set_changed: false,
        storm: sampling_duration_ms == 0 || tracks.iter().any(|track| track.storm),
        tracks,
    }
}

async fn run_multi_peer_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    access_code: &str,
    writer: &mut ResultsWriter,
    children: &mut RunChildren,
) -> ScenarioOutcome {
    let labels = ["web-1", "web-2"];
    let expected = labels.len();
    let mut peers = Vec::new();
    for label in labels.iter().copied() {
        let web_peer = match spawn_web_peer_labeled(scenario, access_code, &writer.dir, Some(label))
        {
            Ok(peer) => peer,
            Err(error) => return infra_fail_outcome(scenario, error),
        };
        children.record_web_peer(&web_peer);
        let _ = writer.write(
            "web-peer",
            Some(scenario.id),
            serde_json::json!({
                "label": label,
                "mode": web_peer.mode,
                "url": web_peer.url,
                "pid": web_peer.pid(),
                "expectedDistinctWebPeers": expected,
                "nativeShareAttempted": false,
                "destructiveExecutionAttempted": false,
            }),
        );
        peers.push(web_peer);
    }

    let reports = await_distinct_web_reports(app, scenario, writer, expected).await;
    let roster_before = app
        .try_state::<crate::session::SessionState>()
        .map(|state| {
            state
                .presence_snapshot()
                .into_iter()
                .map(|participant| participant.identity)
                .collect()
        })
        .unwrap_or_default();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let roster_after = app
        .try_state::<crate::session::SessionState>()
        .map(|state| {
            state
                .presence_snapshot()
                .into_iter()
                .map(|participant| participant.identity)
                .collect()
        })
        .unwrap_or_default();
    let menubar = crate::menubar::get_menubar_state(app.clone());
    let native = MultiPeerNativeEvidence {
        roster_before,
        roster_after,
        menubar_in_meeting: menubar.in_meeting,
        menubar_participant_count: menubar.participant_count,
        // Diagnostics currently does not expose attributable per-peer clock
        // calibration or keyframe-storm evidence to the cockpit. Refuse to
        // pass until those authoritative sources are wired (#261).
        clock_calibration_ok: None,
        keyframe_storm_free: None,
    };
    drop(peers);
    multi_peer_outcome_from_reports(scenario, &reports, expected, Some(&native))
}

async fn run_soak_stall_watch_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    access_code: &str,
    writer: &mut ResultsWriter,
    children: &mut RunChildren,
) -> ScenarioOutcome {
    let mut native_share = None;
    let mut native_video_artifact = None;
    if scenario.requires_native_share {
        match start_native_test_pattern_share(app, scenario, writer).await {
            Ok(share) => {
                let window_id = share.window_id;
                native_share = Some(share);
                let _ = writer.write(
                    "native-share-ready",
                    Some(scenario.id),
                    serde_json::json!({
                        "windowId": window_id,
                        "purpose": "soak-stall-watch",
                    }),
                );
                record_window_screenshot_artifact(writer, scenario, "share-start", window_id);
                native_video_artifact =
                    start_window_video_artifact(writer, scenario, "scenario", window_id);
            }
            Err(error) => return infra_fail_outcome(scenario, error),
        }
    }

    let web_peer = match spawn_web_peer(scenario, access_code, &writer.dir) {
        Ok(peer) => peer,
        Err(error) => return infra_fail_outcome(scenario, error),
    };
    children.record_web_peer(&web_peer);
    let _ = writer.write(
        "web-peer",
        Some(scenario.id),
        serde_json::json!({
            "mode": web_peer.mode,
            "url": web_peer.url,
            "pid": web_peer.pid(),
            "heartbeatExpected": true,
            "liveMutationAttempted": false,
        }),
    );
    if let Some(share) = native_share.as_ref() {
        record_window_screenshot_artifact(writer, scenario, "mid-scenario", share.window_id);
    }

    let Some(report) = await_web_report(app, scenario, writer).await else {
        if let Some(recording) = native_video_artifact.take() {
            recording.stop_and_record(writer, scenario, "failure");
        }
        return infra_fail_outcome(
            scenario,
            "web harness did not report a terminal Soak heartbeat/stall-watch result",
        );
    };
    let mut outcome = web_report_outcome(scenario, &report);
    let heartbeat_count =
        report_number(&report.payload, &["heartbeatCount", "heartbeats"]).unwrap_or(0.0);
    let heartbeat_ok = report_payload_bool(&report.payload, &["heartbeatOk", "stallWatchOk"])
        || heartbeat_count >= 2.0;
    let passed = report_ok(&report.payload) && heartbeat_ok;
    outcome.verdict = if passed {
        ScenarioVerdict::Pass
    } else {
        ScenarioVerdict::TestFail
    };
    outcome.message = if passed {
        format!("{} PASS Soak heartbeat/stall-watch reported", scenario.id)
    } else {
        format!(
            "{} TEST-FAIL Soak heartbeat/stall-watch report was missing or unhealthy",
            scenario.id
        )
    };
    outcome.assertions.push(AssertionOutcome {
        name: "soak-heartbeat".to_string(),
        passed,
        detail: format!(
            "heartbeatCount={heartbeat_count}; payload={}",
            report.payload
        ),
    });
    if let Some(share) = native_share.as_ref() {
        record_window_screenshot_artifact(writer, scenario, "verdict", share.window_id);
    }
    if let Some(recording) = native_video_artifact.take() {
        recording.stop_and_record(writer, scenario, "verdict");
    }
    outcome
}

async fn run_scenario(
    app: &AppHandle,
    scenario: ScenarioSpec,
    access_code: &str,
    run_meta: &RunMeta,
    writer: &mut ResultsWriter,
    children: &mut RunChildren,
) -> ScenarioOutcome {
    struct StatusRetirement(AppHandle);
    impl Drop for StatusRetirement {
        fn drop(&mut self) {
            crate::dev_test_pattern::retire_cockpit_test_pattern_status(&self.0);
        }
    }
    let _status_retirement = scenario
        .requires_native_share
        .then(|| StatusRetirement(app.clone()));
    let _ = writer.write("scenario-start", Some(scenario.id), scenario.id);

    match scenario.kind {
        ScenarioKind::ChaosDevice => {
            return run_chaos_device_scenario(app, scenario, access_code, writer, children).await
        }
        ScenarioKind::ChaosDisplayChange => {
            return run_chaos_display_change_scenario(app, scenario, writer).await
        }
        ScenarioKind::ChaosNet | ScenarioKind::ChaosLifecycle => {
            return run_net_impair_scenario(scenario, writer).await
        }
        ScenarioKind::MultiPeer => {
            return run_multi_peer_scenario(app, scenario, access_code, writer, children).await
        }
        ScenarioKind::RemoteControlScaled => {
            return run_remote_control_scaled_scenario(
                app,
                scenario,
                access_code,
                run_meta,
                writer,
                children,
            )
            .await
        }
        ScenarioKind::NativeToNativeShare => {
            return run_native_to_native_scenario(app, scenario, run_meta, writer, children).await
        }
        ScenarioKind::RemoteControlNativeToNative => {
            #[cfg(target_os = "macos")]
            if console_is_locked() {
                return infra_fail_outcome(
                    scenario,
                    "the console session is LOCKED -- SCK cannot capture the peer's window and \
                     compositor panels get zero-sized geometry; unlock the Mac and re-run",
                );
            }
            return run_remote_control_native_to_native_scenario(
                app, scenario, run_meta, writer, children,
            )
            .await;
        }
        ScenarioKind::RemoteControlNativeToWeb => {
            #[cfg(target_os = "macos")]
            if console_is_locked() {
                return infra_fail_outcome(
                    scenario,
                    "the console session is LOCKED -- compositor panels get zero-sized \
                     WindowServer geometry; unlock the Mac and re-run",
                );
            }
            return run_remote_control_native_to_web_scenario(
                app,
                scenario,
                access_code,
                run_meta,
                writer,
                children,
            )
            .await
        }
        ScenarioKind::SoakStallWatch => {
            return run_soak_stall_watch_scenario(app, scenario, access_code, writer, children)
                .await
        }
        ScenarioKind::MultiWindowShare
        | ScenarioKind::MultiDisplayShare
        | ScenarioKind::FullDesktopShare
        | ScenarioKind::CameraBitrateScaling
        | ScenarioKind::CameraStall
        | ScenarioKind::JoinRoom
        | ScenarioKind::UiScreenshot => {
            return run_gap_scaffold_scenario(app, scenario, writer).await
        }
        _ => {}
    }

    // Bring up the native share BEFORE spawning the web peer, so the share
    // tile is already published when the web peer joins and runs its scenario
    // (e.g. SHARE-N2W-Q's waitForRemoteShareVideo, DRAW-N's draw-on-remote-tile).
    // Otherwise the web peer joins into an empty room, finds no share to
    // receive/draw on, and the scenario fails as a pure ordering race -- the
    // native share (which can take a few seconds while SCShareableContent
    // catches up to a just-opened window) would land only after the web peer
    // already gave up.
    // AUD-N2W: Petal joins with the mic MUTED (transport::audio pre-mutes the
    // track before publishing) -- correct product behaviour, and exactly why
    // the first live run of this scenario measured peak=0/128 off a perfectly
    // healthy published track. A human unmutes; so does the scenario, through
    // the same SessionState path the menubar toggle uses. The mute apply is
    // spawned (livekit needs an ambient runtime -- see the crash note in
    // session::set_mic_muted), so give it time to reach the SFU before the
    // web listener starts sampling.
    if matches!(scenario.kind, ScenarioKind::AudioNativeToWeb) {
        if let Some(state) = app.try_state::<crate::session::SessionState>() {
            state.set_mic_muted(false);
            log::info!("test-cockpit: {} unmuted the native mic for the listener", scenario.id);
            tokio::time::sleep(Duration::from_millis(1500)).await;
            log::info!(
                "test-cockpit: {} mic state after unmute: session_reports_muted={}",
                scenario.id,
                state.mic_muted()
            );
        } else {
            log::warn!("test-cockpit: {} could not reach SessionState to unmute the mic", scenario.id);
        }
    }

    // CAM-N2W: the native camera is OFF until a human clicks Video, so the
    // scenario turns it on through that same product path -- this is the body
    // of `start_camera_publish_command`, which is only a thin #[tauri::command]
    // wrapper over it. Started before the web peer for the same ordering
    // reason as the share above: the publication must exist before the viewer
    // starts waiting for it.
    if matches!(scenario.kind, ScenarioKind::CameraNativeToWeb) {
        let state = app.try_state::<crate::session::SessionState>();
        let preferences = app.try_state::<crate::transport::camera::CameraDevicePreferences>();
        match (state, preferences) {
            (Some(state), Some(preferences)) => {
                let _control = state.lock_camera_control().await;
                state.set_camera_intent(true);
                if let Err(error) = crate::camera_session::start_camera_publish_with_device(
                    app,
                    &state,
                    preferences.preferred_device(),
                    preferences.preferred_mode(),
                )
                .await
                {
                    // A camera that will not OPEN is a rig problem, not a
                    // verdict about whether the product renders on a peer.
                    return infra_fail_outcome(
                        scenario,
                        format!(
                            "could not start the native camera publish: {error} (no camera on this machine? set PETAL_CAMERA_SYNTH_SOURCE=1 to publish the NV12 test pattern through the real publish path)"
                        ),
                    );
                }
                log::info!(
                    "test-cockpit: {} started the native camera publish for the web viewer",
                    scenario.id
                );
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            _ => {
                return infra_fail_outcome(
                    scenario,
                    "could not reach SessionState/CameraDevicePreferences to start the native camera".to_string(),
                );
            }
        }
    }

    let mut native_share = None;
    let mut native_video_artifact = None;
    if scenario.requires_native_share {
        match start_native_test_pattern_share(app, scenario, writer).await {
            Ok(share) => {
                let window_id = share.window_id;
                native_share = Some(share);
                record_window_screenshot_artifact(writer, scenario, "share-start", window_id);
                native_video_artifact =
                    start_window_video_artifact(writer, scenario, "scenario", window_id);
            }
            Err(error) => {
                return ScenarioOutcome {
                    scenario_id: scenario.id.to_string(),
                    verdict: ScenarioVerdict::InfraFail,
                    message: format!("{} {error}", scenario.id),
                    delivered_fps: 0.0,
                    delivered_width: 0,
                    delivered_height: 0,
                    assertions: vec![],
                };
            }
        }
    }

    let web_peer = match spawn_web_peer(scenario, access_code, &writer.dir) {
        Ok(peer) => peer,
        Err(error) => {
            if let Some(recording) = native_video_artifact.take() {
                recording.stop_and_record(writer, scenario, "failure");
            }
            return ScenarioOutcome {
                scenario_id: scenario.id.to_string(),
                verdict: ScenarioVerdict::InfraFail,
                message: error,
                delivered_fps: 0.0,
                delivered_width: 0,
                delivered_height: 0,
                assertions: vec![],
            };
        }
    };
    let _ = writer.write(
        "web-peer",
        Some(scenario.id),
        serde_json::json!({ "mode": web_peer.mode, "url": web_peer.url, "pid": web_peer.pid() }),
    );
    children.record_web_peer(&web_peer);
    if let Some(share) = native_share.as_ref() {
        record_window_screenshot_artifact(writer, scenario, "mid-scenario", share.window_id);
    }

    let report = await_web_report(app, scenario, writer).await;
    let mut outcome = if let Some(report) = report {
        assert_reported_scenario(app, scenario, &report, writer).await
    } else if scenario.kind == ScenarioKind::WebToNativeShare {
        assert_web_to_native_video(app, scenario, writer).await
    } else {
        ScenarioOutcome {
            scenario_id: scenario.id.to_string(),
            verdict: ScenarioVerdict::InfraFail,
            message: format!(
                "{} INFRA-FAIL web harness did not report a petal.cockpit result; this Quick scenario may not be implemented web-side yet",
                scenario.id
            ),
            delivered_fps: 0.0,
            delivered_width: 0,
            delivered_height: 0,
            assertions: vec![],
        }
    };
    if let Some(share) = native_share.as_ref() {
        record_window_screenshot_artifact(writer, scenario, "verdict", share.window_id);
        if matches!(
            scenario.kind,
            ScenarioKind::NativeToWebShare | ScenarioKind::Draw
        ) {
            let path = writer.dir.join(screenshot_artifact_relative_path(
                scenario.id,
                "verdict",
                share.window_id,
            ));
            let content = test_pattern_content_check(&path);
            let passed = content.is_ok();
            let detail = match content {
                Ok(()) => {
                    "calibration squares and sampled pixels matched the expected test pattern"
                        .to_string()
                }
                Err(error) => error,
            };
            let _ = writer.write(
                "content-check",
                Some(scenario.id),
                serde_json::json!({ "passed": passed, "detail": detail, "path": path }),
            );
            outcome.assertions.push(AssertionOutcome {
                name: "test-pattern-content".to_string(),
                passed,
                detail,
            });
            if !passed {
                outcome.verdict = ScenarioVerdict::TestFail;
                outcome.message = format!(
                    "{} TEST-FAIL captured test-pattern content check failed",
                    scenario.id
                );
            }
        }
    }
    if let Some(recording) = native_video_artifact.take() {
        recording.stop_and_record(writer, scenario, "verdict");
    }
    outcome
}

#[derive(Debug, Clone)]
struct CleanupVerification {
    passed: bool,
    payload: serde_json::Value,
}

fn cleanup_verifier(
    app: &AppHandle,
    room_name: &str,
    children: &RunChildren,
) -> CleanupVerification {
    let participants = app
        .try_state::<crate::session::SessionState>()
        .map(|state| state.presence_snapshot())
        .unwrap_or_default();
    let participants = serde_json::to_value(participants).unwrap_or_else(|_| serde_json::json!([]));
    let alive_web_peer_pids = children
        .web_peer_pids
        .iter()
        .copied()
        .filter(|pid| process_is_running(*pid))
        .collect::<Vec<_>>();
    let mut payload = cleanup_payload(
        room_name,
        participants,
        children.web_peer_pids.clone(),
        alive_web_peer_pids,
    );
    let alive_native_peer_pids = children
        .native_peer_pids
        .iter()
        .copied()
        .filter(|pid| process_is_running(*pid))
        .collect::<Vec<_>>();
    let native_passed = alive_native_peer_pids.is_empty();
    payload.passed &= native_passed;
    payload.payload["orphanedNativePeerProcessCheck"] = serde_json::json!({
        "trackedNativePeerPids": children.native_peer_pids,
        "aliveNativePeerPids": alive_native_peer_pids,
        "passed": native_passed,
    });
    payload.payload["passed"] = serde_json::json!(payload.passed);
    payload
}

fn cleanup_payload(
    room_name: &str,
    participants: serde_json::Value,
    tracked_web_peer_pids: Vec<u32>,
    alive_web_peer_pids: Vec<u32>,
) -> CleanupVerification {
    let passed = alive_web_peer_pids.is_empty();
    let payload = serde_json::json!({
        "roomName": room_name,
        "impairmentProfileActive": false,
        "impairmentProfileCheck": "not applied by Quick tier; #261 owns network impairment assertions",
        "orphanedChromeProcessCheck": {
            "trackedWebPeerPids": tracked_web_peer_pids,
            "aliveWebPeerPids": alive_web_peer_pids,
            "passed": passed
        },
        "staleParticipantsBeforeLeave": participants,
        "passed": passed
    });
    CleanupVerification { passed, payload }
}

async fn leave_cockpit_room(app: &AppHandle) {
    if let Some(session) = app.try_state::<crate::session::SessionState>() {
        crate::session::leave_room(app, session.inner()).await;
    }
}

#[tauri::command]
pub async fn start_test_cockpit(
    app: AppHandle,
    state: State<'_, CockpitRuntimeState>,
    args: StartTestCockpitArgs,
) -> Result<CockpitStatus, String> {
    let selector = normalize_selector(&args.selector).ok_or("test case selector is required")?;
    preflight_or_refuse(&app)?;
    let scenarios = resolve_scenarios(&selector)?;
    let scenario_total = scenarios.len() as u32;
    if current_status(&state).running {
        return Err("test cockpit is already running".to_string());
    }
    state.cancel_requested.store(false, Ordering::SeqCst);

    let run_id = run_id();
    let results_dir = test_runs_root().join(&run_id);
    let mut writer = ResultsWriter::create(results_dir.clone())?;
    let results_dir_string = results_dir.display().to_string();
    let room_name = cockpit_room_name();
    assert_cockpit_room(&room_name)?;
    let native_identity = cockpit_identity();
    let meta = RunMeta {
        run_id: run_id.clone(),
        selector: selector.clone(),
        room_name: room_name.clone(),
        native_identity: native_identity.clone(),
        backend_url: backend_url(),
        harness_url: harness_url(),
        app_version: env!("CARGO_PKG_VERSION"),
        app_commit: env!("PETAL_GIT_COMMIT"),
        joined_room_credential: None,
        access_code: None,
    };
    writer.write("meta", None, &meta)?;

    let started = CockpitStatus {
        running: true,
        run_id: Some(run_id.clone()),
        selector: Some(selector.clone()),
        results_dir: Some(results_dir_string.clone()),
        summary: None,
    };
    update_status(&state, started.clone());
    let _ = app.emit(
        TEST_PROGRESS_EVENT,
        TestProgressEvent {
            run_id: run_id.clone(),
            selector: selector.clone(),
            phase: "running".to_string(),
            scenario_id: None,
            message: format!("Starting test cockpit: {}", selector),
            completed: 0,
            total: scenario_total,
            skipped: vec![],
            summary: None,
            results_dir: Some(results_dir_string.clone()),
        },
    );

    let display_sleep_assertion = crate::platform::power::DisplaySleepAssertion::acquire(&format!(
        "Petal test cockpit: {run_id}"
    ));
    let join_result = {
        let rooms = app.state::<crate::rooms::RoomsState>();
        let session = app.state::<crate::session::SessionState>();
        crate::session::join_room(
            &app,
            rooms.inner(),
            session.inner(),
            room_name.clone(),
            native_identity.clone(),
            "Petal Test Cockpit".to_string(),
            crate::remote_control_core::RemoteControlPolicy::Auto,
            None,
        )
        .await
    };
    let mut outcomes = Vec::new();
    let mut skipped = Vec::new();
    let mut completed_count = 0_u32;
    let mut children = RunChildren::default();

    let (access_code, joined_room_credential) = match join_result {
        Ok(record) => {
            let credential = joined_room_credential(&record);
            match (record.access_code.clone(), credential) {
                (Some(code), Ok(credential)) => (code, Some(credential)),
                (Some(_), Err(reason)) => {
                    skipped.push(CockpitSkippedScenario {
                        id: selector.clone(),
                        reason,
                    });
                    (String::new(), None)
                }
                (None, _) => {
                    skipped.push(CockpitSkippedScenario {
                        id: selector.clone(),
                        reason: "joined cockpit room did not expose an access code".to_string(),
                    });
                    (String::new(), None)
                }
            }
        }
        Err(error) => {
            skipped.push(CockpitSkippedScenario {
                id: selector.clone(),
                reason: format!("native join failed: {error}"),
            });
            (String::new(), None)
        }
    };
    let mut meta = meta;
    meta.joined_room_credential = joined_room_credential;
    meta.access_code = if access_code.is_empty() {
        None
    } else {
        Some(access_code.clone())
    };

    if !access_code.is_empty() {
        for scenario in scenarios {
            if completed_count > 0 {
                tokio::time::sleep(TOKEN_REQUEST_INTERVAL).await;
            }
            if state.cancel_requested.load(Ordering::SeqCst) {
                outcomes.push(ScenarioOutcome {
                    scenario_id: scenario.id.to_string(),
                    verdict: ScenarioVerdict::Cancelled,
                    message: "Test cockpit run cancelled".to_string(),
                    delivered_fps: 0.0,
                    delivered_width: 0,
                    delivered_height: 0,
                    assertions: vec![],
                });
                break;
            }
            let _ = app.emit(
                TEST_PROGRESS_EVENT,
                TestProgressEvent {
                    run_id: run_id.clone(),
                    selector: selector.clone(),
                    phase: "scenario".to_string(),
                    scenario_id: Some(scenario.id.to_string()),
                    message: format!("Running {}", scenario.id),
                    completed: completed_count,
                    total: scenario_total,
                    skipped: skipped.clone(),
                    summary: None,
                    results_dir: Some(results_dir_string.clone()),
                },
            );
            let outcome = run_scenario(
                &app,
                scenario,
                &access_code,
                &meta,
                &mut writer,
                &mut children,
            )
            .await;
            // #815 (review): CAM-N2W turns the camera ON through the real
            // product path; mirror it OFF on EVERY outcome path here, or the
            // synthetic camera keeps publishing through every later scenario.
            if matches!(scenario.kind, ScenarioKind::CameraNativeToWeb) {
                if let Some(state) = app.try_state::<crate::session::SessionState>() {
                    let _control = state.lock_camera_control().await;
                    state.set_camera_intent(false);
                    crate::camera_session::stop_camera_publish(&state).await;
                    log::info!(
                        "test-cockpit: {} stopped the native camera publish (scenario epilogue)",
                        scenario.id
                    );
                }
            }
            if matches!(
                outcome.verdict,
                ScenarioVerdict::InfraFail | ScenarioVerdict::Skipped
            ) {
                skipped.push(CockpitSkippedScenario {
                    id: outcome.scenario_id.clone(),
                    reason: outcome.message.clone(),
                });
            }
            let _ = writer.write(
                "scenario-verdict",
                Some(scenario.id),
                &ScenarioVerdictRecord::new(&outcome),
            );
            outcomes.push(outcome);
            completed_count += 1;
        }
    }

    let cleanup = cleanup_verifier(&app, &room_name, &children);
    let cleanup_passed = cleanup.passed;
    let _ = writer.write("cleanup", None, cleanup.payload);
    leave_cockpit_room(&app).await;
    drop(display_sleep_assertion);
    writer.write_scorecard(&outcomes)?;
    writer.write_conclusion(&outcomes, &selector)?;
    let retention = prune_test_cockpit_artifacts();
    let _ = writer.write("artifact-retention", None, retention);
    writer.flush();

    let passed = outcomes
        .iter()
        .filter(|outcome| outcome.verdict == ScenarioVerdict::Pass)
        .count() as u32;
    let failed = outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome.verdict,
                ScenarioVerdict::TestFail | ScenarioVerdict::InfraFail
            )
        })
        .count() as u32
        + if access_code.is_empty() { 1 } else { 0 }
        + if cleanup_passed { 0 } else { 1 };
    let cancelled = outcomes
        .iter()
        .any(|outcome| outcome.verdict == ScenarioVerdict::Cancelled)
        || state.cancel_requested.load(Ordering::SeqCst);
    let status = if cancelled {
        CockpitRunStatus::Cancelled
    } else if failed == 0 && !outcomes.is_empty() {
        CockpitRunStatus::Passed
    } else {
        CockpitRunStatus::Failed
    };
    let message = match status {
        CockpitRunStatus::Passed => format!("Test cockpit passed: {passed} scenario(s)"),
        CockpitRunStatus::Failed => {
            format!("Test cockpit failed: {passed} passed, {failed} failed")
        }
        CockpitRunStatus::Cancelled => "Test cockpit run cancelled".to_string(),
    };
    let summary = CockpitSummary {
        status,
        passed,
        failed,
        skipped,
        message,
    };
    let completed = CockpitStatus {
        running: false,
        run_id: Some(run_id.clone()),
        selector: Some(selector.clone()),
        results_dir: Some(results_dir_string.clone()),
        summary: Some(summary.clone()),
    };
    update_status(&state, completed.clone());
    let _ = app.emit(
        TEST_PROGRESS_EVENT,
        TestProgressEvent {
            run_id,
            selector,
            phase: "completed".to_string(),
            scenario_id: None,
            message: summary.message.clone(),
            completed: 1,
            total: 1,
            skipped: summary.skipped.clone(),
            summary: Some(summary),
            results_dir: Some(results_dir_string),
        },
    );
    Ok(completed)
}

#[tauri::command]
pub fn cockpit_status(state: State<'_, CockpitRuntimeState>) -> CockpitStatus {
    current_status(&state)
}

#[tauri::command]
pub fn cancel_test_cockpit(state: State<'_, CockpitRuntimeState>) -> CockpitStatus {
    state.cancel_requested.store(true, Ordering::SeqCst);
    let mut status = current_status(&state);
    if status.running {
        status.running = false;
        status.summary = Some(CockpitSummary {
            status: CockpitRunStatus::Cancelled,
            passed: 0,
            failed: 0,
            skipped: vec![],
            message: "Test cockpit run cancelled".to_string(),
        });
        update_status(&state, status.clone());
    }
    status
}

#[tauri::command]
pub fn open_test_cockpit_results_folder(path: String) -> bool {
    let path = PathBuf::from(path);
    if !path.exists() {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&path)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        false
    }
}

#[tauri::command]
pub fn capture_window_pixels(
    app: AppHandle,
    window_id: u32,
    rect: Option<PixelRect>,
    path: Option<String>,
) -> Result<CaptureWindowPixelsResult, String> {
    preflight_or_refuse(&app)?;
    crate::test_cockpit_bridge::capture_window_pixels(window_id, rect, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "petal-cockpit-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn write_text(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn refuses_when_marker_missing() {
        let dir = scratch_dir("missing");
        let _ = std::fs::remove_dir_all(&dir);

        let result = preflight_or_refuse_under(&dir);

        assert_eq!(
            result,
            Err("INFRA-FAIL: run scripts/cockpit-setup.sh".to_string())
        );
    }

    #[test]
    fn passes_when_marker_present() {
        let dir = scratch_dir("present");
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        std::fs::write(marker_path_under(&dir), b"ok").expect("write marker");

        let result = preflight_or_refuse_under(&dir);

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn primary_marker_does_not_authorize_the_distinct_test_peer_directory() {
        let root = scratch_dir("distinct-identities");
        let primary = root.join("com.petal.app");
        let peer = root.join("com.petal.app.testpeer");
        std::fs::create_dir_all(&primary).expect("create primary dir");
        std::fs::write(marker_path_under(&primary), b"primary").expect("write primary marker");

        assert_eq!(preflight_or_refuse_under(&primary), Ok(()));
        assert_eq!(
            preflight_or_refuse_under(&peer),
            Err("INFRA-FAIL: run scripts/cockpit-setup.sh".to_string())
        );

        std::fs::create_dir_all(&peer).expect("create peer dir");
        std::fs::write(marker_path_under(&peer), b"peer").expect("write peer marker");
        assert_eq!(preflight_or_refuse_under(&peer), Ok(()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn capability_probe_is_callable_without_prompting() {
        let _ = privileged_commands_available();
    }

    #[test]
    fn capture_lock_requires_a_confirmed_active_share() {
        assert_eq!(capture_lock_phase_after_share(false), None);
        assert_eq!(
            capture_lock_phase_after_share(true),
            Some(crate::dev_test_pattern::CockpitTestPatternPhase::CaptureLocked)
        );
    }

    #[test]
    fn visible_source_lease_removes_exception_before_a_recycled_window_id_can_share() {
        let recycled_window_id = 4_294_967_000;
        assert!(!cockpit_source_requires_visible_handback(
            recycled_window_id
        ));
        let lease = register_cockpit_visible_source(recycled_window_id);
        assert!(cockpit_source_requires_visible_handback(recycled_window_id));
        drop(lease);
        assert!(
            !cockpit_source_requires_visible_handback(recycled_window_id),
            "an ordinary source reusing a former cockpit CGWindowID must use normal handback"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cockpit_appkit_dispatch_state_machine_cancels_only_queued_work() {
        assert!(appkit_dispatch_is_direct(true));
        assert!(!appkit_dispatch_is_direct(false));

        let queued = AtomicU8::new(ACTIVATION_QUEUED);
        assert!(queued_activation_cancel(&queued));
        assert_eq!(queued.load(Ordering::SeqCst), ACTIVATION_CANCELLED);
        assert!(
            !queued_activation_try_start(7, 7, &queued),
            "a timed-out queued activation must never mutate AppKit later"
        );

        let started = AtomicU8::new(ACTIVATION_QUEUED);
        assert!(queued_activation_try_start(7, 7, &started));
        assert_eq!(started.load(Ordering::SeqCst), ACTIVATION_STARTED);
        assert!(
            !queued_activation_cancel(&started),
            "started main-thread work has crossed the mutation boundary"
        );
        assert!(!queued_activation_try_start(
            7,
            8,
            &AtomicU8::new(ACTIVATION_QUEUED)
        ));

        // N timing out after N+1 is queued must never invalidate N+1.
        let current_generation = AtomicU64::new(8);
        let generation_n = AtomicU8::new(ACTIVATION_QUEUED);
        let generation_n_plus_one = AtomicU8::new(ACTIVATION_QUEUED);
        assert!(queued_activation_cancel(&generation_n));
        assert_eq!(current_generation.load(Ordering::SeqCst), 8);
        assert!(queued_activation_try_start(
            8,
            current_generation.load(Ordering::SeqCst),
            &generation_n_plus_one,
        ));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn cockpit_appkit_dispatch_deadline_allows_delayed_success_and_expires_once() {
        let deadline = Instant::now() + Duration::from_millis(50);
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = sender.send(7_u8);
        });
        let value =
            tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), receiver)
                .await
                .expect("delayed dispatch stayed inside the one shared deadline")
                .expect("sender completed");
        assert_eq!(value, 7);

        let expired = Instant::now();
        let (_sender, receiver) = oneshot::channel::<()>();
        assert!(
            tokio::time::timeout(expired.saturating_duration_since(Instant::now()), receiver,)
                .await
                .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cockpit_appkit_dispatch_errors_are_stable_and_schedule_is_testable() {
        assert_eq!(
            AppKitDispatchError::TimedOut.code(),
            "cockpit-main-thread-dispatch-timeout"
        );
        assert_eq!(
            AppKitDispatchError::ReceiverClosed.code(),
            "cockpit-main-thread-dispatch-receiver-closed"
        );
        assert_eq!(
            dispatch_schedule_result::<&str>(Err("closed")),
            Err(AppKitDispatchError::ScheduleFailed("closed".to_string()))
        );
        assert_eq!(
            AppKitDispatchError::SourceMissing.code(),
            "cockpit-main-thread-source-missing"
        );
        assert_eq!(
            AppKitDispatchError::ExecutionFailed("leaf".to_string()).detail(),
            "leaf"
        );

        let deadline = Instant::now() + Duration::from_millis(5);
        assert!(
            capped_readiness_sleep(deadline, Duration::from_secs(1)) <= Duration::from_millis(5)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_test_pattern_readiness_requires_active_key_visible_and_two_advancing_beats() {
        let ready = NativeTestPatternReadiness {
            registered_source: true,
            cg_visible: true,
            app_active: true,
            window_key: true,
            window_visible: true,
            regular_policy: true,
            policy_change_accepted: true,
            can_become_key: true,
            activation_requested: true,
            activation_accepted: true,
            ns_app_activate_requested: false,
            legacy_activate_requested: false,
            activation_caller_main: false,
            activation_queue_latency_ms: 1,
            geometry_matches: true,
            advancing_reports: 2,
            counter_delta: 1,
            liveness_fresh: true,
            post_activation_report: true,
        };
        assert!(ready.ready());
        assert!(!NativeTestPatternReadiness {
            app_active: false,
            ..ready
        }
        .ready());
        assert!(!NativeTestPatternReadiness {
            advancing_reports: 1,
            ..ready
        }
        .ready());
        assert!(!NativeTestPatternReadiness {
            counter_delta: 0,
            ..ready
        }
        .ready());
        assert!(
            NativeTestPatternReadiness {
                activation_accepted: false,
                ..ready
            }
            .ready(),
            "observed AppKit state, not activation's Bool, is authoritative"
        );
        let not_keyable = NativeTestPatternReadiness {
            can_become_key: false,
            ..ready
        };
        assert!(!not_keyable.ready());
        assert_eq!(not_keyable.failure_code(), "cockpit-source-not-keyable");
        let drifted_geometry = NativeTestPatternReadiness {
            geometry_matches: false,
            ..ready
        };
        assert!(!drifted_geometry.ready());
        assert_eq!(
            drifted_geometry.failure_code(),
            "cockpit-source-geometry-drift"
        );
        assert!(!cockpit_activation_reassert_due(
            true,
            true,
            Duration::from_secs(1),
            Duration::from_secs(1)
        ));
        assert!(!cockpit_activation_reassert_due(
            false,
            false,
            Duration::from_millis(299),
            Duration::from_secs(1)
        ));
        assert!(cockpit_activation_reassert_due(
            false,
            true,
            Duration::from_millis(300),
            Duration::from_secs(1)
        ));
        assert!(!cockpit_activation_reassert_due(
            false,
            true,
            Duration::from_millis(300),
            Duration::ZERO
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_test_pattern_attempt_requires_new_report_after_activation() {
        let activation_complete = std::cell::Cell::new(false);
        let (_, baseline) = activate_then_sample_liveness_sequence(
            || {
                activation_complete.set(true);
                Ok::<(), String>(())
            },
            || {
                assert!(
                    activation_complete.get(),
                    "activation must complete before sampling"
                );
                7_u64
            },
        )
        .expect("activation succeeded before sampling the baseline");
        assert_eq!(baseline, 7);

        // A retry establishes a fresh baseline: a beat that was new for the
        // initial activation is stale if it has not advanced since reassert.
        let initial_baseline = 7_u64;
        let after_initial_activation = 8_u64;
        assert!(after_initial_activation > initial_baseline);
        let reassert_baseline = after_initial_activation;
        assert!(
            !(after_initial_activation > reassert_baseline),
            "a pre-reassert heartbeat must not satisfy the reasserted source"
        );
        assert!(9_u64 > reassert_baseline);

        let ready = NativeTestPatternReadiness {
            registered_source: true,
            cg_visible: true,
            app_active: true,
            window_key: true,
            window_visible: true,
            regular_policy: true,
            policy_change_accepted: true,
            can_become_key: true,
            activation_requested: true,
            activation_accepted: true,
            ns_app_activate_requested: false,
            legacy_activate_requested: false,
            activation_caller_main: false,
            activation_queue_latency_ms: 1,
            geometry_matches: true,
            advancing_reports: 2,
            counter_delta: 1,
            liveness_fresh: true,
            post_activation_report: true,
        };
        let mut toggled = false;
        let error = toggle_after_native_test_pattern_readiness(
            NativeTestPatternReadiness {
                post_activation_report: false,
                ..ready
            },
            || {
                toggled = true;
            },
        )
        .unwrap_err();
        assert_eq!(error, "INFRA-FAIL cockpit-source-not-active-or-drawing");
        assert!(
            !toggled,
            "an unchanged post-activation heartbeat must not reach the toggle"
        );

        let toggled_after_ready = toggle_after_native_test_pattern_readiness(ready, || true)
            .expect("a ready source may enter the toggle");
        assert!(toggled_after_ready);
    }

    #[test]
    fn unverified_binary_provenance_is_rejected_before_window_launch() {
        if COCKPIT_FRONTEND_PROVENANCE == "unverified" {
            let error = verified_cockpit_frontend_provenance().unwrap_err();
            assert!(error.contains("no verified generated"));
        } else {
            assert!(verified_cockpit_frontend_provenance().is_ok());
        }
    }

    #[test]
    fn parses_equals_form_before_env() {
        let spec = launch_spec_from_args_and_env_value(
            ["desktop", "--test-case=quick"],
            Some("full".to_string()),
        )
        .unwrap();

        assert_eq!(
            spec,
            LaunchSpec {
                selector: "quick".to_string(),
                source: LaunchSource::Arg
            }
        );
    }

    #[test]
    fn parses_split_arg_form() {
        assert_eq!(
            parse_test_case_arg(["desktop", "--test-case", "SHARE-W2N-Q"]),
            Some("SHARE-W2N-Q".to_string())
        );
    }

    #[test]
    fn parses_env_when_arg_missing() {
        let spec =
            launch_spec_from_args_and_env_value(["desktop"], Some("soak".to_string())).unwrap();

        assert_eq!(
            spec,
            LaunchSpec {
                selector: "soak".to_string(),
                source: LaunchSource::Env
            }
        );
    }

    #[test]
    fn ignores_blank_launch_values() {
        let spec =
            launch_spec_from_args_and_env_value(["desktop", "--test-case="], Some(" ".to_string()));

        assert_eq!(spec, None);
    }

    #[test]
    fn resolves_quick_tier_to_all_phase_three_scenarios() {
        let scenarios = resolve_scenarios("quick").unwrap();

        assert_eq!(
            scenarios
                .iter()
                .map(|scenario| scenario.id)
                .collect::<Vec<_>>(),
            vec!["SHARE-N2W-Q", "SHARE-W2N-Q", "DRAW-N", "CAM", "AUD", "TELE"]
        );
    }

    #[test]
    fn resolves_full_tier_to_all_full_scenarios() {
        let scenarios = resolve_scenarios("full").unwrap();

        assert_eq!(
            scenarios
                .iter()
                .map(|scenario| (scenario.id, scenario.tier, scenario.kind))
                .collect::<Vec<_>>(),
            vec![
                ("CHAOS-DEVICE", "full", ScenarioKind::ChaosDevice),
                (
                    "CHAOS-DISPLAY-CHANGE",
                    "full",
                    ScenarioKind::ChaosDisplayChange
                ),
                ("CHAOS-NET", "full", ScenarioKind::ChaosNet),
                ("CHAOS-LIFECYCLE", "full", ScenarioKind::ChaosLifecycle),
                ("MULTI-3", "full", ScenarioKind::MultiPeer),
                ("RC-P1080", "full", ScenarioKind::RemoteControlScaled),
                ("SHARE-N2N", "full", ScenarioKind::NativeToNativeShare)
            ]
        );
    }

    #[test]
    fn resolves_soak_tier_to_stall_watches_and_network_chaos() {
        let scenarios = resolve_scenarios("soak").unwrap();

        assert_eq!(
            scenarios
                .iter()
                .map(|scenario| (scenario.id, scenario.tier, scenario.kind))
                .collect::<Vec<_>>(),
            vec![
                ("SOAK-W2N-STALL", "soak", ScenarioKind::SoakStallWatch),
                ("SOAK-N2W-STALL", "soak", ScenarioKind::SoakStallWatch),
                ("CHAOS-NET-SOAK", "soak", ScenarioKind::ChaosNet)
            ]
        );
        assert!(!scenarios[0].requires_native_share);
        assert!(scenarios[1].requires_native_share);
    }

    #[test]
    fn launchd_soak_selector_resolves_to_real_scenarios() {
        let scenarios = resolve_scenarios("soak").unwrap();

        assert!(!scenarios.is_empty());
        assert!(scenarios.iter().all(|scenario| scenario.tier == "soak"));
    }

    #[test]
    fn resolves_net_chaos_scenarios_case_insensitively() {
        let scenarios = resolve_scenarios("chaos-net, CHAOS-LIFECYCLE").unwrap();

        assert_eq!(
            scenarios
                .iter()
                .map(|scenario| (scenario.id, scenario.kind))
                .collect::<Vec<_>>(),
            vec![
                ("CHAOS-NET", ScenarioKind::ChaosNet),
                ("CHAOS-LIFECYCLE", ScenarioKind::ChaosLifecycle)
            ]
        );
    }

    #[test]
    fn resolves_comma_separated_scenarios_case_insensitively() {
        let scenarios = resolve_scenarios("share-w2n-q, TELE").unwrap();

        assert_eq!(
            scenarios
                .iter()
                .map(|scenario| scenario.id)
                .collect::<Vec<_>>(),
            vec!["SHARE-W2N-Q", "TELE"]
        );
    }

    #[test]
    fn rejects_unknown_scenario_ids() {
        assert_eq!(
            resolve_scenarios("NOPE").unwrap_err(),
            "unknown test cockpit scenario or journey 'NOPE'"
        );
    }

    #[test]
    fn tool_detection_rejects_empty_paths_and_path_injection() {
        assert!(!detect_tool_in_path("", None));
        assert!(!detect_tool_in_path("../displayplacer", None));
        assert!(!detect_tool_in_path("displayplacer", None));
    }

    #[test]
    fn tool_detection_finds_executable_in_path() {
        let dir = scratch_dir("tool-path");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let tool = dir.join("SwitchAudioSource");
        std::fs::write(&tool, b"#!/bin/sh\nexit 0\n").expect("write tool");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
                .expect("chmod tool");
        }

        assert!(detect_tool_in_path(
            "SwitchAudioSource",
            Some(dir.as_os_str())
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn net_impair_script_resolution_walks_up_to_repo_root() {
        let root = scratch_dir("net-impair-root");
        let manifest_dir = root.join("apps/desktop/src-tauri");
        let script = root.join(NET_IMPAIR_SCRIPT_RELATIVE_PATH);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&manifest_dir).expect("create manifest dir");
        std::fs::create_dir_all(script.parent().unwrap()).expect("create scripts dir");
        std::fs::write(&script, b"#!/usr/bin/env bash\n").expect("write script");

        assert_eq!(
            net_impair_script_path_from_manifest_dir(&manifest_dir),
            Some(script)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn net_impair_script_resolution_returns_none_when_missing() {
        let root = scratch_dir("net-impair-missing");
        let manifest_dir = root.join("apps/desktop/src-tauri");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&manifest_dir).expect("create manifest dir");

        assert_eq!(
            net_impair_script_path_from_manifest_dir(&manifest_dir),
            None
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Look scenarios up by id, never by position. These tests indexed
    /// `SCENARIO_TABLE` directly, so inserting AUD-N2W silently re-pointed
    /// nine of them at neighbouring scenarios -- they still compiled and
    /// still asserted, just about the wrong thing.
    fn named_scenario(id: &str) -> ScenarioSpec {
        scenario_by_id(id).unwrap_or_else(|| panic!("unknown scenario id {id}"))
    }

    #[test]
    fn present_net_impair_script_is_not_a_false_pass() {
        let script = PathBuf::from("/tmp/petal-test/scripts/net-impair.sh");
        let outcome = net_impair_not_live_outcome(named_scenario("CHAOS-NET"), Some(&script));

        assert_eq!(outcome.scenario_id, "CHAOS-NET");
        assert_eq!(outcome.verdict, ScenarioVerdict::InfraFail);
        assert!(outcome
            .message
            .contains("does not run live network impairment yet"));
        assert!(outcome.message.contains("refusing to invoke sudo"));
        assert!(!outcome.assertions[0].passed);
    }

    #[test]
    fn missing_net_impair_script_is_a_skip_not_pass() {
        let outcome = net_impair_not_live_outcome(named_scenario("CHAOS-LIFECYCLE"), None);

        assert_eq!(outcome.scenario_id, "CHAOS-LIFECYCLE");
        assert_eq!(outcome.verdict, ScenarioVerdict::Skipped);
        assert!(outcome.message.contains("SKIPPED(tooling)"));
        assert!(outcome.assertions[0].passed);
    }

    #[test]
    fn multi_peer_requires_two_distinct_terminal_reports() {
        let messages = [
            r#"test-cockpit report from 'web-2': {"scenarioId":"MULTI-3","step":"done","reporterId":"web-2","ok":true,"participantCount":3,"remoteParticipantCount":2,"rosterFingerprint":"dd01961d26e172fc6190cdab03bfb25eadb46651ad8b6d867c1f0b8fe00dff63","rosterFingerprintAlgorithm":"sha-256","rosterIncludesReporter":true,"rosterUnique":true}"#,
            r#"test-cockpit report from 'web-1': {"scenarioId":"MULTI-3","step":"done","reporterId":"web-1","ok":true,"participantCount":3,"remoteParticipantCount":2,"rosterFingerprint":"dd01961d26e172fc6190cdab03bfb25eadb46651ad8b6d867c1f0b8fe00dff63","rosterFingerprintAlgorithm":"sha-256","rosterIncludesReporter":true,"rosterUnique":true}"#,
        ];
        let reports =
            collect_distinct_terminal_reports_from_journal(messages, named_scenario("MULTI-3"), 2);
        let native = MultiPeerNativeEvidence {
            roster_before: vec!["native".into(), "web-2".into(), "web-1".into()],
            roster_after: vec!["web-1".into(), "native".into(), "web-2".into()],
            menubar_in_meeting: true,
            menubar_participant_count: 3,
            clock_calibration_ok: Some(true),
            keyframe_storm_free: Some(true),
        };
        let outcome =
            multi_peer_outcome_from_reports(named_scenario("MULTI-3"), &reports, 2, Some(&native));

        assert_eq!(reports.len(), 2);
        assert_eq!(outcome.scenario_id, "MULTI-3");
        assert_eq!(outcome.verdict, ScenarioVerdict::Pass);
        assert!(outcome.assertions.iter().all(|assertion| assertion.passed));
    }

    #[test]
    fn multi_peer_rejects_duplicate_sender_reports() {
        let messages = [
            r#"test-cockpit report from 'web-1': {"scenarioId":"MULTI-3","step":"done","ok":true}"#,
            r#"test-cockpit report from 'web-1': {"scenarioId":"MULTI-3","step":"done","ok":true}"#,
        ];
        let reports =
            collect_distinct_terminal_reports_from_journal(messages, named_scenario("MULTI-3"), 2);
        let outcome = multi_peer_outcome_from_reports(named_scenario("MULTI-3"), &reports, 2, None);

        assert_eq!(reports.len(), 1);
        assert_eq!(outcome.verdict, ScenarioVerdict::InfraFail);
        assert!(outcome
            .message
            .contains("only 1/2 distinct web peer reports"));
        assert!(!outcome.assertions[0].passed);
    }

    #[test]
    fn keyframe_control_evidence_is_stable_per_track_and_storm_bounded() {
        let mut before = crate::diagnostics::TrackHealth::default();
        before.sid = "TR_one".into();
        before.kind = "video".into();
        before.direction = "recv".into();
        before.key_frames_decoded = 4;
        before.pli_count = 2;
        let mut after = before.clone();
        after.key_frames_decoded = 6;
        after.pli_count = 3;

        let evidence = keyframe_control_evidence(&[before], &[after], 2_000, 4);
        assert!(evidence.evidence_available);
        assert_eq!(evidence.sampling_duration_ms, 2_000);
        assert_eq!(evidence.tracks[0].direction, "recv");
        assert_eq!(evidence.tracks[0].key_frames_decoded, 2);
        assert_eq!(evidence.tracks[0].pli_count, 1);
        assert!(!evidence.tracks[0].counter_reset);
        assert!(!evidence.storm);
    }

    #[test]
    fn keyframe_control_evidence_fails_closed_on_reset_or_storm() {
        let mut before = crate::diagnostics::TrackHealth::default();
        before.sid = "TR_one".into();
        before.kind = "video".into();
        before.direction = "send".into();
        before.key_frames_encoded = 10;
        let mut reset = before.clone();
        reset.key_frames_encoded = 1;
        assert!(keyframe_control_evidence(&[before.clone()], &[reset], 2_000, 5).storm);

        let mut storm = before.clone();
        storm.key_frames_encoded = 20;
        let evidence = keyframe_control_evidence(&[before], &[storm], 2_000, 5);
        assert!(!evidence.tracks[0].counter_reset);
        assert!(evidence.storm);
    }

    #[test]
    fn keyframe_control_evidence_rejects_changed_empty_or_duplicate_scope() {
        let track = |sid: &str| {
            let mut track = crate::diagnostics::TrackHealth::default();
            track.sid = sid.into();
            track.kind = "video".into();
            track.direction = "recv".into();
            track
        };
        let stable = track("TR_one");
        for evidence in [
            keyframe_control_evidence(&[stable.clone()], &[track("TR_two")], 2_000, 5),
            keyframe_control_evidence(&[stable.clone()], &[], 2_000, 5),
            keyframe_control_evidence(&[track("")], &[stable.clone()], 2_000, 5),
            keyframe_control_evidence(
                &[stable.clone(), stable.clone()],
                &[stable.clone()],
                2_000,
                5,
            ),
        ] {
            assert!(!evidence.evidence_available);
            assert!(evidence.track_set_changed);
            assert!(evidence.storm);
            assert!(evidence.tracks.is_empty());
        }
    }

    #[test]
    fn multi_peer_treats_missing_typed_evidence_as_infrastructure_failure() {
        let messages = [
            r#"test-cockpit report from 'web-1': {"scenarioId":"MULTI-3","step":"done","ok":true}"#,
            r#"test-cockpit report from 'web-2': {"scenarioId":"MULTI-3","step":"done","ok":true}"#,
        ];
        let reports =
            collect_distinct_terminal_reports_from_journal(messages, named_scenario("MULTI-3"), 2);
        let outcome = multi_peer_outcome_from_reports(named_scenario("MULTI-3"), &reports, 2, None);

        assert_eq!(outcome.verdict, ScenarioVerdict::InfraFail);
        assert!(outcome.message.contains("required typed roster evidence"));
        assert!(!outcome.assertions[1].passed);
    }

    #[test]
    fn multi_peer_rejects_fingerprint_disagreement_without_leaking_roster() {
        let messages = [
            r#"test-cockpit report from 'web-1': {"scenarioId":"MULTI-3","step":"done","reporterId":"web-1","ok":true,"participantCount":3,"remoteParticipantCount":2,"rosterFingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","rosterFingerprintAlgorithm":"sha-256","rosterIncludesReporter":true,"rosterUnique":true}"#,
            r#"test-cockpit report from 'web-2': {"scenarioId":"MULTI-3","step":"done","reporterId":"web-2","ok":true,"participantCount":3,"remoteParticipantCount":2,"rosterFingerprint":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","rosterFingerprintAlgorithm":"sha-256","rosterIncludesReporter":true,"rosterUnique":true}"#,
        ];
        let reports =
            collect_distinct_terminal_reports_from_journal(messages, named_scenario("MULTI-3"), 2);
        let native = MultiPeerNativeEvidence {
            roster_before: vec!["native".into(), "web-1".into(), "web-2".into()],
            roster_after: vec!["native".into(), "web-1".into(), "web-2".into()],
            menubar_in_meeting: true,
            menubar_participant_count: 3,
            clock_calibration_ok: Some(true),
            keyframe_storm_free: Some(true),
        };
        let outcome =
            multi_peer_outcome_from_reports(named_scenario("MULTI-3"), &reports, 2, Some(&native));

        assert_eq!(outcome.verdict, ScenarioVerdict::TestFail);
        assert!(
            !outcome
                .assertions
                .iter()
                .find(|assertion| assertion.name == "multi-peer-browser-roster-consensus")
                .expect("browser consensus assertion")
                .passed
        );
        assert!(!outcome.message.contains("web-1"));
        assert!(!outcome.message.contains("web-2"));
    }

    fn valid_multi_peer_reports() -> Vec<WebCockpitReport> {
        ["web-1", "web-2"]
            .into_iter()
            .map(|sender| WebCockpitReport {
                sender: sender.to_string(),
                payload: serde_json::json!({
                    "scenarioId": "MULTI-3",
                    "step": "done",
                    "reporterId": sender,
                    "ok": true,
                    "participantCount": 3,
                    "remoteParticipantCount": 2,
                    "rosterFingerprint": "dd01961d26e172fc6190cdab03bfb25eadb46651ad8b6d867c1f0b8fe00dff63",
                    "rosterFingerprintAlgorithm": "sha-256",
                    "rosterIncludesReporter": true,
                    "rosterUnique": true,
                }),
            })
            .collect()
    }

    #[test]
    fn multi_peer_rejects_divergent_native_third_member() {
        let native = MultiPeerNativeEvidence {
            roster_before: vec!["other-native".into(), "web-1".into(), "web-2".into()],
            roster_after: vec!["other-native".into(), "web-1".into(), "web-2".into()],
            menubar_in_meeting: true,
            menubar_participant_count: 3,
            clock_calibration_ok: Some(true),
            keyframe_storm_free: Some(true),
        };
        let outcome = multi_peer_outcome_from_reports(
            named_scenario("MULTI-3"),
            &valid_multi_peer_reports(),
            2,
            Some(&native),
        );

        assert_eq!(outcome.verdict, ScenarioVerdict::TestFail);
        assert!(outcome
            .assertions
            .iter()
            .find(|assertion| assertion.name == "multi-peer-native-roster-and-menubar")
            .is_some_and(|assertion| !assertion.passed));
    }

    #[test]
    fn multi_peer_clock_and_keyframe_evidence_fail_closed() {
        let mut native = MultiPeerNativeEvidence {
            roster_before: vec!["native".into(), "web-1".into(), "web-2".into()],
            roster_after: vec!["native".into(), "web-1".into(), "web-2".into()],
            menubar_in_meeting: true,
            menubar_participant_count: 3,
            clock_calibration_ok: None,
            keyframe_storm_free: None,
        };
        let reports = valid_multi_peer_reports();
        let unavailable =
            multi_peer_outcome_from_reports(named_scenario("MULTI-3"), &reports, 2, Some(&native));
        assert_eq!(unavailable.verdict, ScenarioVerdict::InfraFail);
        assert!(unavailable.message.contains("evidence is unavailable"));

        native.clock_calibration_ok = Some(false);
        native.keyframe_storm_free = Some(true);
        let mismatch =
            multi_peer_outcome_from_reports(named_scenario("MULTI-3"), &reports, 2, Some(&native));
        assert_eq!(mismatch.verdict, ScenarioVerdict::TestFail);
        assert!(mismatch
            .assertions
            .iter()
            .find(|assertion| assertion.name == "multi-peer-clock-calibration")
            .is_some_and(|assertion| !assertion.passed));
    }

    #[tokio::test]
    async fn remote_control_scaled_scaffold_is_not_a_false_pass() {
        let root = scratch_dir("remote-control-scaled-scaffold");
        let _ = std::fs::remove_dir_all(&root);
        let mut writer = ResultsWriter::create(root.clone()).expect("writer");

        let outcome = run_scaffold_only_scenario(named_scenario("RC-P1080"), &mut writer).await;

        assert_eq!(outcome.scenario_id, "RC-P1080");
        assert_eq!(outcome.verdict, ScenarioVerdict::InfraFail);
        assert!(outcome.message.contains("TCC-granted native host"));
        assert!(outcome.message.contains("refuses to false-pass"));
        assert!(!outcome.assertions[0].passed);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skipped_outcome_is_not_a_failure_verdict() {
        let outcome = skipped_outcome(named_scenario("CHAOS-DEVICE"), "SKIPPED(tooling): missing tool");

        assert_eq!(outcome.verdict, ScenarioVerdict::Skipped);
        assert_eq!(outcome.scenario_id, "CHAOS-DEVICE");
        assert!(outcome.message.contains("SKIPPED(tooling)"));
        assert_eq!(outcome.assertions[0].name, "skip-classification");
        assert!(outcome.assertions[0].passed);
    }

    #[test]
    fn chaos_device_camera_disappearance_can_pass_without_audio_switch_tool() {
        let report = WebCockpitReport {
            sender: "web-peer".to_string(),
            payload: serde_json::json!({
                "scenarioId": "CHAOS-DEVICE",
                "step": "done",
                "ok": true,
                "cameraPublished": true,
                "cameraDisappeared": true
            }),
        };

        let outcome = chaos_device_outcome_from_report(named_scenario("CHAOS-DEVICE"), &report, false);

        assert_eq!(outcome.verdict, ScenarioVerdict::Pass);
        assert!(outcome.message.contains("camera disappeared"));
        assert!(outcome
            .message
            .contains("SwitchAudioSource is not installed"));
        assert_eq!(outcome.assertions[1].name, "camera-disappearance");
        assert!(outcome.assertions[1].passed);
        assert_eq!(outcome.assertions[2].name, "audio-device-switch");
        assert!(outcome.assertions[2].passed);
        assert!(outcome.assertions[2].detail.contains("SKIPPED(tooling)"));
    }

    #[test]
    fn parses_web_cockpit_report_and_matches_scenario() {
        let report = parse_web_cockpit_report_line(
            "test-cockpit report from 'web-peer': {\"scenarioId\":\"SHARE-N2W-Q\",\"ok\":true,\"fps\":31}",
        )
        .unwrap();

        assert_eq!(report.sender, "web-peer");
        assert!(report_matches_scenario(&report, named_scenario("SHARE-N2W-Q")));
        assert!(report_ok(&report.payload));
        assert_eq!(
            report_number(&report.payload, &["fps", "deliveredFps"]),
            Some(31.0)
        );
    }

    #[test]
    fn native_to_web_accepts_demand_sized_layer_reported_ok_by_web_peer() {
        let report = WebCockpitReport {
            sender: "web-peer".to_string(),
            payload: serde_json::json!({
                "scenarioId": "SHARE-N2W-Q",
                "step": "done",
                "ok": true,
                "fps": 0.25,
                "width": 480,
                "height": 300
            }),
        };

        let outcome = web_report_outcome(named_scenario("SHARE-N2W-Q"), &report);
        assert_eq!(outcome.verdict, ScenarioVerdict::Pass);
        assert!(outcome.assertions[0].passed);
    }

    #[test]
    fn terminal_report_detection_refuses_join_only_success_as_scenario_result() {
        let join = serde_json::json!({
            "scenarioId": "CAM",
            "step": "join",
            "ok": true
        });
        let done = serde_json::json!({
            "scenarioId": "CAM",
            "step": "done",
            "ok": true,
            "cameraPublished": true
        });

        assert!(!is_terminal_report_step(&join));
        assert!(is_terminal_report_step(&done));
    }

    #[test]
    fn scenario_specific_web_report_markers_are_required() {
        let generic_ok = WebCockpitReport {
            sender: "web-peer".to_string(),
            payload: serde_json::json!({
                "scenarioId": "CAM",
                "step": "done",
                "ok": true
            }),
        };
        let camera_ok = WebCockpitReport {
            sender: "web-peer".to_string(),
            payload: serde_json::json!({
                "scenarioId": "CAM",
                "step": "done",
                "ok": true,
                "cameraPublished": true
            }),
        };

        assert!(validate_scenario_web_report(named_scenario("CAM"), &generic_ok).is_err());
        assert!(validate_scenario_web_report(named_scenario("CAM"), &camera_ok).is_ok());
    }

    #[test]
    fn tele_scenario_requires_native_share_for_remote_tile_targeting() {
        let scenario = SCENARIO_TABLE
            .iter()
            .find(|scenario| scenario.id == "TELE")
            .expect("TELE scenario");

        assert!(scenario.requires_native_share);
    }

    #[test]
    fn cleanup_payload_fails_when_tracked_web_peer_is_alive() {
        let cleanup = cleanup_payload(
            "rctest-example",
            serde_json::json!([]),
            vec![111, 222],
            vec![222],
        );

        let payload = cleanup.payload;
        assert!(!cleanup.passed);
        assert_eq!(payload["passed"], false);
        assert_eq!(payload["orphanedChromeProcessCheck"]["passed"], false);
        assert_eq!(
            payload["orphanedChromeProcessCheck"]["aliveWebPeerPids"],
            serde_json::json!([222])
        );
    }

    #[test]
    fn draw_journal_pair_requires_ordered_begin_then_end() {
        assert!(journal_messages_contain_pair(
            [
                "draw: delivered Begin stroke 'a'",
                "draw: delivered End stroke 'a'"
            ],
            "Begin stroke",
            "End stroke"
        ));
        assert!(!journal_messages_contain_pair(
            [
                "draw: delivered End stroke 'a'",
                "draw: delivered Begin stroke 'a'"
            ],
            "Begin stroke",
            "End stroke"
        ));
    }

    #[test]
    fn screenshot_artifact_paths_are_relative_and_step_scoped() {
        let path = screenshot_artifact_relative_path("SHARE-N2W-Q", "mid scenario", 42);

        assert_eq!(
            path,
            PathBuf::from("artifacts").join("share-n2w-q-mid-scenario-window-42.png")
        );
        assert!(!path.is_absolute());
    }

    #[test]
    fn video_artifact_paths_are_relative_and_step_scoped() {
        let path = video_artifact_relative_path("SOAK-N2W-STALL", "scenario", 42);

        assert_eq!(
            path,
            PathBuf::from("artifacts").join("soak-n2w-stall-scenario-window-42.mov")
        );
        assert!(!path.is_absolute());
    }

    #[test]
    fn audio_artifact_paths_are_relative_and_step_scoped() {
        let path = audio_artifact_relative_path("AUD", "verdict");
        let temp = audio_artifact_temp_wav_relative_path("AUD", "verdict");

        assert_eq!(
            path,
            PathBuf::from("artifacts").join("aud-verdict-tone.m4a")
        );
        assert_eq!(
            temp,
            PathBuf::from("artifacts").join("aud-verdict-tone.wav")
        );
        assert!(!path.is_absolute());
        assert!(!temp.is_absolute());
    }

    #[test]
    fn wav_writer_emits_pcm16_mono_header() {
        let root = scratch_dir("audio-artifact-wav");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch");
        let path = root.join("snippet.wav");

        write_wav_pcm16_mono(&path, &[0, 1024, -1024], AUDIO_ARTIFACT_SAMPLE_RATE).expect("wav");
        let bytes = std::fs::read(&path).expect("read wav");

        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 6);
        assert_eq!(i16::from_le_bytes(bytes[46..48].try_into().unwrap()), 1024);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn screenshot_artifact_payload_uses_viewer_contract_fields() {
        let payload = serde_json::to_value(ArtifactEventPayload {
            artifact_type: "screenshot",
            path: "artifacts/share-n2w-q-verdict-window-42.png".to_string(),
            step_id: "verdict".to_string(),
            t_ms: 123,
            window_id: Some(42),
        })
        .unwrap();

        assert_eq!(payload["type"], "screenshot");
        assert_eq!(payload["stepId"], "verdict");
        assert_eq!(payload["tMs"], 123);
        assert_eq!(payload["windowId"], 42);
    }

    #[test]
    fn artifact_preview_resolves_only_run_child_media() {
        let run = scratch_dir("artifact-preview");
        let outside = scratch_dir("artifact-preview-outside");
        let _ = std::fs::remove_dir_all(&run);
        let _ = std::fs::remove_dir_all(&outside);
        write_text(&run.join("artifacts/frame.png"), "png");
        write_text(&outside.join("escape.png"), "png");

        let resolved = resolve_run_child_file(&run, "artifacts/frame.png").unwrap();
        assert_eq!(
            resolved,
            run.join("artifacts/frame.png").canonicalize().unwrap()
        );
        assert_eq!(mime_for_preview_artifact(&resolved), Some("image/png"));
        assert!(resolve_run_child_file(&run, "../escape.png").is_err());
        assert!(
            resolve_run_child_file(&run, &outside.join("escape.png").display().to_string())
                .is_err()
        );
        assert_eq!(
            mime_for_preview_artifact(&run.join("movie.mov")),
            Some("video/quicktime")
        );
        assert_eq!(
            mime_for_preview_artifact(&run.join("clip.mp4")),
            Some("video/mp4")
        );
        assert_eq!(
            mime_for_preview_artifact(&run.join("tone.m4a")),
            Some("audio/mp4")
        );
        assert_eq!(mime_for_preview_artifact(&run.join("notes.txt")), None);

        let _ = std::fs::remove_dir_all(&run);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn cockpit_room_invariant_allows_only_rctest_rooms() {
        assert_eq!(assert_cockpit_room("rctest-abc"), Ok(()));
        assert_eq!(
            assert_cockpit_room("eng-sync"),
            Err("INFRA-FAIL: test cockpit refused non-rctest room 'eng-sync'".to_string())
        );
    }

    #[test]
    fn scorecard_uses_harness_compatible_shape() {
        let scorecard = scorecard_from_outcomes(&[
            ScenarioOutcome {
                scenario_id: "SHARE-W2N-Q".to_string(),
                verdict: ScenarioVerdict::Pass,
                message: "ok".to_string(),
                delivered_fps: 29.5,
                delivered_width: 1280,
                delivered_height: 720,
                assertions: vec![],
            },
            ScenarioOutcome {
                scenario_id: "MULTI-3".to_string(),
                verdict: ScenarioVerdict::Pass,
                message: "ok".to_string(),
                delivered_fps: 0.0,
                delivered_width: 0,
                delivered_height: 0,
                assertions: vec![],
            },
        ]);
        let value = serde_json::to_value(scorecard).unwrap();

        assert_eq!(value["scenarios"][0]["scenarioName"], "SHARE-W2N-Q");
        assert_eq!(value["scenarios"][0]["sourceIssue"], "#257");
        assert_eq!(value["scenarios"][0]["participantCount"], 2);
        assert_eq!(value["scenarios"][0]["deliveredFps"], 29.5);
        assert!(value["scenarios"][0]["latency"].is_null());
        assert_eq!(value["scenarios"][0]["freeze"]["freezeCount"], 0);
        assert_eq!(value["scenarios"][1]["scenarioName"], "MULTI-3");
        assert_eq!(value["scenarios"][1]["participantCount"], 3);
    }

    #[test]
    fn parses_run_jsonl_skips_malformed_lines_and_extracts_artifacts() {
        let dir = scratch_dir("viewer-jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        write_text(
            &dir.join("run.jsonl"),
            r#"{"kind":"meta","payload":{"runId":"run-1"}}
not-json
{"kind":"scenario-verdict","payload":{"verdict":"pass"}}
{"kind":"scenario-verdict","payload":{"verdict":"infra-fail"}}
{"kind":"artifact","payload":{"path":"redacted.png","label":"screenshot"}}"#,
        );

        let parsed = parse_run_jsonl(&dir, true);

        assert_eq!(parsed.parse_errors, 1);
        assert_eq!(parsed.run_id.as_deref(), Some("run-1"));
        assert_eq!(parsed.counts.pass, 1);
        assert_eq!(parsed.counts.fail, 1);
        assert_eq!(parsed.events.len(), 4);
        assert_eq!(parsed.artifacts.len(), 1);
        assert_eq!(parsed.artifacts[0]["path"], "redacted.png");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifact_retention_prunes_only_old_video_and_audio() {
        let root = scratch_dir("artifact-retention");
        let _ = std::fs::remove_dir_all(&root);
        let old_run = root.join("old-run");
        let recent_run = root.join("recent-run");
        for (run, generated, scenario) in [
            (&old_run, 1_u64, "SHARE-W2N-Q"),
            (&recent_run, 20_000_u64, "SHARE-W2N-Q"),
        ] {
            write_text(
                &run.join("run.jsonl"),
                &format!(
                    r#"{{"kind":"artifact","payload":{{"type":"video","path":"screen.mov"}}}}
{{"kind":"artifact","payload":{{"type":"audio","path":"tone.m4a"}}}}
{{"kind":"artifact","payload":{{"type":"screenshot","path":"verdict.png"}}}}"#
                ),
            );
            write_text(
                &run.join("scorecard.json"),
                &format!(
                    r#"{{
                      "generatedAtUnixMs": {generated},
                      "scenarios": [{{ "scenarioName": "{scenario}", "status": "pass" }}]
                    }}"#
                ),
            );
            write_text(&run.join("screen.mov"), "video");
            write_text(&run.join("tone.m4a"), "audio");
            write_text(&run.join("verdict.png"), "screenshot");
        }

        let report = prune_artifacts_under(
            &root,
            ArtifactRetentionConfig {
                max_age_days: 1,
                max_runs_per_scenario: 1,
            },
            UNIX_EPOCH + Duration::from_millis(100_000_000),
        );

        assert_eq!(report.scanned_runs, 2);
        assert_eq!(report.pruned_files, 2);
        assert_eq!(report.kept_files, 2);
        assert!(!old_run.join("screen.mov").exists());
        assert!(!old_run.join("tone.m4a").exists());
        assert!(old_run.join("verdict.png").exists());
        assert!(old_run.join("run.jsonl").exists());
        assert!(old_run.join("scorecard.json").exists());
        assert!(recent_run.join("screen.mov").exists());
        assert!(recent_run.join("tone.m4a").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn artifact_retention_refuses_paths_outside_test_runs_root() {
        let root = scratch_dir("artifact-retention-confine");
        let outside = scratch_dir("artifact-retention-outside");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        let run = root.join("run");
        write_text(
            &run.join("run.jsonl"),
            &format!(
                r#"{{"kind":"artifact","payload":{{"type":"video","path":"{}"}}}}"#,
                outside.join("escape.mov").display()
            ),
        );
        write_text(
            &run.join("scorecard.json"),
            r#"{"generatedAtUnixMs":1,"scenarios":[{"scenarioName":"SHARE-W2N-Q","status":"pass"}]}"#,
        );
        write_text(&outside.join("escape.mov"), "video");

        let report = prune_artifacts_under(
            &root,
            ArtifactRetentionConfig {
                max_age_days: 1,
                max_runs_per_scenario: 0,
            },
            UNIX_EPOCH + Duration::from_millis(100_000_000),
        );

        assert_eq!(report.pruned_files, 0);
        assert_eq!(report.skipped_files, 1);
        assert!(outside.join("escape.mov").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn run_summary_prefers_scorecard_counts_when_present() {
        let dir = scratch_dir("viewer-summary");
        let _ = std::fs::remove_dir_all(&dir);
        write_text(
            &dir.join("run.jsonl"),
            r#"{"kind":"scenario-verdict","payload":{"verdict":"pass"}}"#,
        );
        write_text(
            &dir.join("scorecard.json"),
            r#"{
              "runId": "score-run",
              "generatedAtUnixMs": 42,
              "summary": { "passed": 2, "failed": 0, "skipped": 1 }
            }"#,
        );

        let summary = run_summary_from_dir(&dir);

        assert_eq!(summary.run_id, "score-run");
        assert_eq!(summary.updated_at_unix_ms, 42);
        assert_eq!(summary.status, "passed");
        assert_eq!(summary.pass, 2);
        assert_eq!(summary.fail, 0);
        assert_eq!(summary.skipped, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Journey model (the project history) ----

    #[test]
    fn every_journey_runnable_points_at_a_real_scenario() {
        for journey in JOURNEY_TABLE {
            if let Some(runnable) = journey.runnable {
                assert!(
                    scenario_by_id(runnable).is_some(),
                    "journey {} points at unknown runnable scenario '{runnable}'",
                    journey.id
                );
            }
            for legacy in journey.legacy {
                assert!(
                    scenario_by_id(legacy).is_some(),
                    "journey {} lists unknown legacy scenario '{legacy}'",
                    journey.id
                );
            }
            assert!(
                FEATURES.iter().any(|f| f.code == journey.feature),
                "journey {} has unknown feature code '{}'",
                journey.id,
                journey.feature
            );
            assert!(
                matches!(journey.priority, "P0" | "P1" | "P2"),
                "journey {} has invalid priority '{}'",
                journey.id,
                journey.priority
            );
            assert!(
                matches!(journey.depth, "short" | "long" | "short-long"),
                "journey {} has invalid depth '{}'",
                journey.id,
                journey.depth
            );
            assert!(
                matches!(journey.status, "covered" | "partial" | "gap" | "blind-spot"),
                "journey {} has invalid status '{}'",
                journey.id,
                journey.status
            );
            // Consistency: a runnable journey must not be gap/blind-spot, and a
            // gap/blind-spot journey must not be runnable. (covered/partial with
            // no runnable would be allowed in principle — e.g. a journey proven
            // by native unit tests rather than a cockpit scenario — but no
            // journey currently claims that. RES-04 used to be exactly this
            // false claim — "covered" with `runnable: None` — and RC-01..06
            // used to claim "covered" pointing at a scaffold that always
            // INFRA-FAILs; both are now honest "gap"s instead, see #379.)
            let has_runnable = journey.runnable.is_some();
            let is_gap = matches!(journey.status, "gap" | "blind-spot");
            if has_runnable {
                assert!(
                    !is_gap,
                    "journey {} is runnable but marked '{}'",
                    journey.id, journey.status
                );
            }
            if is_gap {
                assert!(
                    !has_runnable,
                    "journey {} is '{}' but has a runnable scenario",
                    journey.id, journey.status
                );
            }
        }
    }

    #[test]
    fn journey_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for journey in JOURNEY_TABLE {
            assert!(
                seen.insert(journey.id),
                "duplicate journey id {}",
                journey.id
            );
        }
    }

    #[test]
    fn resolves_journey_id_to_its_runnable_scenario() {
        // SHARE-03 runs SHARE-W2N-Q.
        let scenarios = resolve_scenarios("SHARE-03").unwrap();
        assert_eq!(
            scenarios.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["SHARE-W2N-Q"]
        );
        // Legacy id still resolves.
        let legacy = resolve_scenarios("SHARE-W2N-Q").unwrap();
        assert_eq!(
            legacy.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["SHARE-W2N-Q"]
        );
    }

    #[test]
    fn p3_gap_journeys_resolve_to_opt_in_scaffold_scenarios() {
        // P-3: the former ⛔ gap journeys now resolve to their honest INFRA-FAIL
        // scaffold scenarios, each on an opt-in tier (never quick/full/soak) so a
        // headless quick/full/soak run never blocks on them.
        for (journey, scenario_id, kind, tier) in [
            (
                "SHARE-05",
                "SHARE-MULTIWIN",
                ScenarioKind::MultiWindowShare,
                "native",
            ),
            (
                "SHARE-06",
                "SHARE-MULTIDISP",
                ScenarioKind::MultiDisplayShare,
                "multi-display",
            ),
            (
                "SHARE-10",
                "SHARE-DESKTOP",
                ScenarioKind::FullDesktopShare,
                "native",
            ),
            (
                "CAM-03",
                "CAM-BITRATE",
                ScenarioKind::CameraBitrateScaling,
                "gap",
            ),
            ("CAM-04", "CAM-STALL", ScenarioKind::CameraStall, "gap"),
            ("ROOM-01", "ROOM-JOIN", ScenarioKind::JoinRoom, "gap"),
            ("UI-01", "UI-MAIN", ScenarioKind::UiScreenshot, "ui"),
            ("UI-04", "UI-DOCK", ScenarioKind::UiScreenshot, "ui"),
        ] {
            let scenarios = resolve_scenarios(journey).unwrap();
            assert_eq!(
                scenarios.iter().map(|s| s.id).collect::<Vec<_>>(),
                vec![scenario_id],
                "journey {journey} did not resolve to {scenario_id}"
            );
            assert_eq!(scenarios[0].kind, kind, "journey {journey} kind mismatch");
            assert_eq!(scenarios[0].tier, tier, "journey {journey} tier mismatch");
            assert!(
                !["quick", "full", "soak"].contains(&scenarios[0].tier),
                "gap scaffold {scenario_id} must not be on a headless tier"
            );
        }
    }

    #[test]
    fn gap_scaffold_metadata_names_its_oracle_and_subcases() {
        let (subcases, oracle, missing) = gap_scaffold_metadata(ScenarioKind::CameraStall);
        assert!(subcases.contains(&"no-new-frame-for-n-watchdog"));
        assert!(oracle.contains("detect_camera_stall"));
        assert!(missing.contains("#247"));

        let (_, oracle, _) = gap_scaffold_metadata(ScenarioKind::MultiWindowShare);
        assert!(oracle.contains("evaluate_focus_weighted_cap"));
        let (_, oracle, _) = gap_scaffold_metadata(ScenarioKind::UiScreenshot);
        assert!(oracle.contains("assert_no_text_overflow"));
    }

    #[test]
    fn share_01_resolves_to_native_to_native_scenario() {
        let scenarios = resolve_scenarios("SHARE-01").unwrap();
        assert_eq!(
            scenarios.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["SHARE-N2N"]
        );
        assert_eq!(scenarios[0].kind, ScenarioKind::NativeToNativeShare);
        assert_eq!(scenarios[0].tier, "full");
    }

    /// The failure mode the phase layer exists to prevent: a journey nobody
    /// can find because it belongs to no phase. Every journey must appear in
    /// exactly one phase, and every phase entry must name a real journey.
    #[test]
    fn phase_table_covers_every_journey_exactly_once() {
        for journey in JOURNEY_TABLE {
            let phases: Vec<&str> = PHASE_TABLE
                .iter()
                .filter(|phase| journey_in_phase(phase, journey))
                .map(|phase| phase.slug)
                .collect();
            assert_eq!(
                phases.len(),
                1,
                "journey {} must be in exactly one phase, found {:?}",
                journey.id,
                phases
            );
        }
        for phase in PHASE_TABLE {
            for id in phase.journeys {
                assert!(
                    journey_by_id(id).is_some(),
                    "phase '{}' names unknown journey '{}'",
                    phase.slug,
                    id
                );
            }
        }
    }

    /// docs/TEST_PLAN.md's modular grammar: a phase selects its runnable
    /// journeys; a `:` intersection narrows to one direction. `speak:web-nat`
    /// is the user's literal "run only audio, one way".
    #[test]
    fn phase_and_direction_selectors_run_modular_slices() {
        // The whole SPEAK phase resolves to its runnable scenarios (AUD-01/02
        // -> AUD, AUD-03 -> CHAOS-DEVICE, AUD-04 -> AUD-N2W).
        let speak = resolve_scenarios("speak").unwrap();
        let speak_ids: Vec<&str> = speak.iter().map(|s| s.id).collect();
        assert_eq!(speak_ids, vec!["AUD", "AUD-N2W", "CHAOS-DEVICE"]);

        // Direction-narrowed the OTHER way: nat->web audio only. Before #812
        // this intersection resolved to AUD (via AUD-02's mute toggle) alone;
        // AUD-N2W is what makes "only audio, the native direction" a real
        // slice a human can ask for.
        let reverse = resolve_scenarios("speak:nat-web").unwrap();
        assert!(
            reverse.iter().any(|s| s.id == "AUD-N2W"),
            "speak:nat-web must include the native->web audio scenario, got {:?}",
            reverse.iter().map(|s| s.id).collect::<Vec<_>>()
        );

        // Direction-narrowed: web->nat audio only.
        let one_way = resolve_scenarios("speak:web-nat").unwrap();
        assert_eq!(
            one_way.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["AUD"],
            "speak:web-nat must be exactly the one-way audio slice"
        );

        // Order of axes must not matter.
        let flipped = resolve_scenarios("web-nat:speak").unwrap();
        assert_eq!(
            flipped.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["AUD"]
        );

        // The CONTROL phase used to match only gap journeys, so resolving it
        // was a named error. #819 gave RC-07 a runnable scenario, so it now
        // resolves to exactly that one. RC-01..06 are still gaps and still say
        // so in the journey table -- a phase resolves to whatever it can
        // actually run, the same as every other phase here.
        let control = resolve_scenarios("control").unwrap();
        assert_eq!(
            control.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["RC-N2N"],
            "the CONTROL phase runs the native-controller scenario RC-07 gained in #819"
        );
        assert!(
            JOURNEY_TABLE
                .iter()
                .filter(|journey| journey.id.starts_with("RC-0") && journey.id != "RC-07")
                .all(|journey| journey.status == "gap"),
            "RC-01..06 must stay honest gaps: #819 covered RC-07 only"
        );

        // An intersection with one bad segment is NOT a group selector; it
        // falls through to id resolution and fails as an unknown id rather
        // than silently widening to the parseable half.
        let err = resolve_scenarios("speak:nonsense").unwrap_err();
        assert!(err.contains("unknown"), "half-parsed intersection: {err}");
    }

    /// A "both"-direction journey satisfies either directional query -- the
    /// SHARE phase narrowed to either direction keeps its both-direction
    /// endurance journey.
    #[test]
    fn both_direction_journeys_match_either_directional_query() {
        let share_w2n = resolve_scenarios("share:web-nat").unwrap();
        assert!(share_w2n.iter().any(|s| s.id == "SOAK-W2N-STALL"));
        let share_n2w = resolve_scenarios("share:nat-web").unwrap();
        assert!(share_n2w.iter().any(|s| s.id == "SOAK-W2N-STALL"));
    }

    #[test]
    fn share_n2n_is_full_only_never_quick() {
        let quick = resolve_scenarios("quick").unwrap();
        let full = resolve_scenarios("full").unwrap();
        assert!(!quick.iter().any(|scenario| scenario.id == "SHARE-N2N"));
        assert!(full.iter().any(|scenario| scenario.id == "SHARE-N2N"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_peer_socket_reads_a_fragmented_delayed_ready_message() {
        let path = std::env::temp_dir().join(format!("petal-peer-ready-{}.sock", run_id()));
        let listener = UnixListener::bind(&path).expect("bind unix listener");
        let token = native_peer_token();
        let writer_token = token.clone();
        let writer_path = path.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let stream = UnixStream::connect(writer_path)
                .await
                .expect("connect peer");
            let (_, mut write_half) = stream.into_split();
            let payload = serde_json::to_string(&NativePeerSocketMessage {
                token: writer_token,
                event: "ready".to_string(),
                x: None,
                y: None,
                binding: None,
                error: None,
                ..Default::default()
            })
            .expect("serialize ready");
            let split = payload.len() / 2;
            write_half
                .write_all(&payload.as_bytes()[..split])
                .await
                .expect("write first fragment");
            tokio::time::sleep(Duration::from_millis(20)).await;
            write_half
                .write_all(&payload.as_bytes()[split..])
                .await
                .expect("write second fragment");
            write_half
                .write_all(b"\n")
                .await
                .expect("write line ending");
        });

        let (stream, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .expect("accept did not time out")
            .expect("accept peer");
        let (read_half, _) = stream.into_split();
        let mut reader = TokioBufReader::new(read_half);
        let received = tokio::time::timeout(
            Duration::from_secs(1),
            read_native_peer_message(&mut reader),
        )
        .await
        .expect("fragmented line did not time out")
        .expect("read peer readiness");
        assert_eq!(received.token, token);
        assert_eq!(received.event, "ready");
        assert!(received.binding.is_none());
        writer.await.expect("writer task");
        drop(listener);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_peer_socket_no_data_read_times_out_without_blocking_cleanup() {
        let path = std::env::temp_dir().join(format!("petal-peer-silent-{}.sock", run_id()));
        let listener = UnixListener::bind(&path).expect("bind unix listener");
        let client_path = path.clone();
        let client = tokio::spawn(async move {
            let _stream = UnixStream::connect(client_path)
                .await
                .expect("connect silent peer");
            tokio::time::sleep(Duration::from_millis(60)).await;
        });

        let (stream, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .expect("accept did not time out")
            .expect("accept peer");
        let (read_half, _) = stream.into_split();
        let mut reader = TokioBufReader::new(read_half);
        let timeout = tokio::time::timeout(
            Duration::from_millis(20),
            read_native_peer_message(&mut reader),
        )
        .await;
        assert!(timeout.is_err(), "silent peer read must time out");
        drop(reader);
        client.await.expect("silent peer task");
        drop(listener);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_peer_stderr_tail_is_bounded_and_keeps_the_terminal_failure() {
        let path = std::env::temp_dir().join(format!("petal-peer-stderr-{}.log", run_id()));
        let room_secret = "room-0123456789abcdef0123456789abcdef";
        let terminal = format!("receiver panic: no reactor running in {room_secret}");
        let contents = format!("{}{}", "x".repeat(NATIVE_PEER_LOG_MAX_BYTES + 1), terminal);
        std::fs::write(&path, contents).expect("write stderr fixture");

        retain_native_peer_log_tail(&path);
        let tail = native_peer_stderr_tail(&path);
        assert!(tail.contains("receiver panic: no reactor running"));
        assert!(
            !tail.contains(room_secret),
            "journal tail must be redacted first"
        );
        assert!(tail.chars().count() <= 4_096);
        assert!(
            std::fs::metadata(&path).expect("stderr metadata").len()
                <= NATIVE_PEER_LOG_MAX_BYTES as u64
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn native_peer_socket_rejects_wrong_token_and_wrong_protocol_step() {
        let expected = "expected-token";
        let move_message = NativePeerSocketMessage {
            token: expected.to_string(),
            event: "move".to_string(),
            x: Some(1),
            y: Some(2),
            binding: None,
            error: None,
            ..Default::default()
        };
        assert!(native_peer_command_is_authorized(
            &move_message,
            expected,
            "move"
        ));
        assert!(!native_peer_command_is_authorized(
            &move_message,
            "wrong-token",
            "move"
        ));
        assert!(!native_peer_command_is_authorized(
            &move_message,
            expected,
            "shutdown"
        ));
    }

    #[test]
    fn native_peer_uses_the_parent_joined_capability_across_separate_room_stores() {
        let root = std::env::temp_dir().join(format!("petal-cockpit-room-test-{}", run_id()));
        let primary = crate::rooms::RoomsState::load(root.join("primary"));
        let peer = crate::rooms::RoomsState::load(root.join("peer"));

        let parent_record = primary.create("rctest-human-label", true).unwrap();
        let peer_local_record = peer.create("rctest-human-label", true).unwrap();
        let forwarded = joined_room_credential(&parent_record).unwrap();
        let parent_access_code = parent_record
            .access_code
            .as_deref()
            .expect("created rooms must retain their access code");
        let peer_joined = peer.create(parent_access_code, true).unwrap();

        assert_ne!(
            parent_record.name, peer_local_record.name,
            "independent local stores must not be used to regenerate the peer credential"
        );
        assert_eq!(forwarded, parent_record.name);
        assert_eq!(peer_joined.name, forwarded);
        assert_eq!(
            crate::rooms::livekit_room_name(&peer_joined),
            crate::rooms::livekit_room_name(&parent_record)
        );
        assert!(
            crate::rooms::normalize_room_credential(
                parent_record.display_name.as_deref().expect("label")
            )
            .is_none(),
            "a bare human label must not substitute for the parent capability"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn native_peer_rejects_missing_or_noncanonical_joined_room_credential() {
        let missing = crate::rooms::RoomRecord {
            id: "room-id".to_string(),
            name: "rctest-human-label".to_string(),
            access_code: None,
            display_name: None,
            slug: "rctest-human-label".to_string(),
            created_at_ms: 0,
            last_joined_ms: None,
            open: true,
        };
        assert!(joined_room_credential(&missing).is_err());
    }

    #[test]
    fn resolves_feature_selector_to_deduped_scenarios() {
        // Feature A (Screen Sharing) journeys map to several runnable scenarios;
        // SHARE-02 and SHARE-03 both point at SHARE-W2N-Q -> deduped to one.
        let by_code = resolve_scenarios("A").unwrap();
        let by_slug = resolve_scenarios("screen-sharing").unwrap();
        assert_eq!(
            by_code.iter().map(|s| s.id).collect::<Vec<_>>(),
            by_slug.iter().map(|s| s.id).collect::<Vec<_>>()
        );
        // No duplicates.
        let ids: Vec<_> = by_code.iter().map(|s| s.id).collect();
        let mut deduped = ids.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            ids.len(),
            deduped.len(),
            "feature A scenarios contained duplicates"
        );
        assert!(ids.contains(&"SHARE-W2N-Q"));
    }

    #[test]
    fn resolves_priority_selector() {
        let p0 = resolve_scenarios("p0").unwrap();
        // Every P0 journey with a runnable must be represented.
        for journey in JOURNEY_TABLE.iter().filter(|j| j.priority == "P0") {
            if let Some(runnable) = journey.runnable {
                assert!(
                    p0.iter().any(|s| s.id.eq_ignore_ascii_case(runnable)),
                    "P0 selector missing runnable {runnable} for {}",
                    journey.id
                );
            }
        }
    }

    #[test]
    fn resolves_depth_selector() {
        let long = resolve_scenarios("long").unwrap();
        assert!(long.iter().any(|s| s.id == "SOAK-W2N-STALL"));
    }

    #[test]
    fn primary_journey_tags_scenarios() {
        // SHARE-W2N-Q's primary journey is SHARE-02 (first in table order).
        let journey = primary_journey_for_scenario("SHARE-W2N-Q").unwrap();
        assert_eq!(journey.id, "SHARE-02");
        assert_eq!(feature_name(journey.feature), "Screen Sharing");
    }

    /// #821: an instrument failure must not be verdicted as a product failure.
    /// The web side sets `classification: "INFRA-FAIL"` when it could not
    /// measure at all; if the native side ignores that field (it did), a blind
    /// receiver produces `TEST-FAIL ... the web listener did not report
    /// audible received audio` -- which is how a working product got a P0
    /// filed against it. Verified live in both directions: headed peer PASS,
    /// forced-headless peer INFRA-FAIL.
    #[test]
    fn a_web_report_that_declares_infra_failure_is_not_a_product_failure() {
        let infra = WebCockpitReport {
            sender: "web-test".to_string(),
            payload: serde_json::json!({
                "ok": false,
                "classification": "INFRA-FAIL",
                "detail": "received 200 RTP packets but the decoder emitted only 0 samples",
            }),
        };
        assert!(web_report_declares_infra_failure(&infra));

        // A real product failure must stay a product failure.
        let product = WebCockpitReport {
            sender: "web-test".to_string(),
            payload: serde_json::json!({
                "ok": false,
                "classification": "TEST-FAIL",
                "detail": "rms=0.0000 over 4.00s of decoded samples",
            }),
        };
        assert!(!web_report_declares_infra_failure(&product));

        // And a report with no classification at all (older harness build)
        // must not be silently upgraded to infra.
        let legacy = WebCockpitReport {
            sender: "web-test".to_string(),
            payload: serde_json::json!({ "ok": false, "detail": "silence" }),
        };
        assert!(!web_report_declares_infra_failure(&legacy));
    }

    /// #815, the same rule in video form. A viewer whose canvas readback is
    /// blind CANNOT tell a black tile from a working one, so its report must
    /// never be printed as "the product rendered black" -- while a viewer that
    /// could see, and saw black, must still be a product failure. Both shapes
    /// are `ok: false` on the wire; `classification` is the only thing that
    /// separates them.
    #[test]
    fn a_blind_camera_viewer_is_not_a_product_failure() {
        let blind = WebCockpitReport {
            sender: "web-test".to_string(),
            payload: serde_json::json!({
                "ok": false,
                "classification": "INFRA-FAIL",
                "remoteCameraVisible": false,
                "detail": "canvas readback control failed; a black reading proves nothing",
            }),
        };
        assert!(web_report_declares_infra_failure(&blind));

        let rendered_black = WebCockpitReport {
            sender: "web-test".to_string(),
            payload: serde_json::json!({
                "ok": false,
                "classification": "TEST-FAIL",
                "remoteCameraVisible": false,
                "remoteCameraNonBlackRatio": 0.0,
                "detail": "maxLuma=2 over 2 sampled frames with the canvas control green",
            }),
        };
        assert!(!web_report_declares_infra_failure(&rendered_black));
    }

    /// #815: CAM-05 is only covered if the SEE phase actually resolves to the
    /// reverse-direction scenario. The journey table can say "runnable" while
    /// the selector still resolves to nothing a human can run.
    #[test]
    fn cam_n2w_is_the_reverse_camera_slice() {
        let see = resolve_scenarios("see").unwrap();
        assert!(
            see.iter().any(|s| s.id == "CAM-N2W"),
            "the SEE phase must include the native->web camera scenario, got {:?}",
            see.iter().map(|s| s.id).collect::<Vec<_>>()
        );

        let reverse = resolve_scenarios("see:nat-web").unwrap();
        assert!(
            reverse.iter().any(|s| s.id == "CAM-N2W"),
            "see:nat-web must include the native->web camera scenario, got {:?}",
            reverse.iter().map(|s| s.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn journey_table_matches_contract() {
        let contract: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../../contracts/petal-contracts.json"
        ))
        .expect("parse contracts");
        let expected = serde_json::to_value(JOURNEY_TABLE).expect("serialize journeys");
        let actual = contract
            .get("testCockpitJourneys")
            .expect("contracts missing testCockpitJourneys");
        assert_eq!(
            actual, &expected,
            "contracts/petal-contracts.json testCockpitJourneys is out of sync with JOURNEY_TABLE"
        );
    }
}
