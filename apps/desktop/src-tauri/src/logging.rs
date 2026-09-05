//! File-based logging sink for the real, GUI-launched app binary.
//!
//! **Why this exists:** a GUI app launched via `open`/Finder/Dock has no
//! attached terminal — nobody can `tail` stdout/stderr from a normally
//! launched `.app`. Before this module, the only place `env_logger::init()`
//! was ever called was inside the standalone `examples/*_probe.rs`
//! harnesses (grepped directly — zero hits in `lib.rs`/`main.rs` before this
//! change); the real app binary had `log`/`thiserror` as dependencies and
//! dozens of `log::info!`/`log::warn!`/`log::error!` call sites already
//! sprinkled through `session.rs`, `resilience.rs`, `menubar.rs`,
//! `compositor.rs`, etc., but **no logger was ever installed**, so every one
//! of those calls was a silent no-op in the shipped app (the `log` facade
//! drops records with no registered `log::Log` implementation). That's why
//! every prior debugging phase had to fall back to throwaway CLI examples
//! instead of watching the real app.
//!
//! **Fix:** a `fern`-configured dispatcher, installed exactly once at the
//! very start of `lib.rs::run()`, before the Tauri builder is even
//! constructed, so even early-startup failures are captured. It writes to a
//! predictable, OS-appropriate location:
//!
//! - macOS: `~/Library/Logs/Petal/petal.log`
//! - Windows: `%APPDATA%\Petal\logs\petal.log`
//!
//! ...chosen over Tauri's `app_handle.path().app_log_dir()` because that API
//! needs a live `AppHandle`, which doesn't exist yet this early in `run()`
//! (the whole point is to capture failures that happen *before* the Tauri
//! builder is up) — a fixed path sidesteps that chicken-and-egg problem
//! entirely. Each platform path follows its native convention: the macOS
//! location is indexed by Console.app, while Windows uses the current user's
//! roaming application-data directory.
//!
//! **Level:** `info` by default (not `debug`) so a plain `open Petal.app`
//! run is informative without anyone needing to know to set `RUST_LOG` —
//! per the task brief, a user/future-agent debugging a normally-launched
//! app has no shell to set that env var in anyway. `RUST_LOG` still
//! overrides this if present (e.g. `RUST_LOG=debug npx tauri dev`), so nothing
//! about the existing dev workflow regresses.
//!
//! **`RUST_LOG` directive syntax (#595):** `resolve_log_filter` accepts the
//! standard `env_logger`-style comma-separated per-module directive grammar
//! (e.g. `RUST_LOG=info,desktop::remote_control=debug`), not just a bare
//! level word — parsed via `env_filter` (the same crate `env_logger` itself
//! uses internally for this). A value that fails to parse (a malformed
//! directive, not merely an unfamiliar one) is NEVER silently substituted:
//! `init()` emits a visible warning — both `eprintln!` immediately and
//! `log::warn!` once the sink is live, so it lands in `petal.log` too —
//! naming the exact value received and the level actually applied instead.
//! This replaces a prior bug where a per-module spec failed
//! `str::parse::<log::LevelFilter>()` and fell back to `info` with no
//! warning at all.
//!
//! **Chosen over extending `env_logger`:** `env_logger` (already a
//! dependency, already used by the example probes) only writes to
//! stdout/stderr — there's no supported way to redirect its output to a file
//! from inside the same process without replacing the whole backend, and a
//! GUI app has no stdout/stderr any observer can reach. `fern` is a thin
//! builder over the same `log` facade (zero new macros, zero changes needed
//! at any of the ~70 existing `log::info!`/`log::warn!`/`log::error!`/
//! `log::debug!` call sites across the crate) that supports multiple
//! simultaneous sinks, so this also keeps stdout output for `cargo run`/
//! `tauri dev` (still useful when a terminal IS attached) while adding the
//! file sink everything else was missing. This avoids a second, competing
//! logging stack (no `tracing` was in use anywhere in this crate before this
//! change — checked directly, zero hits) — `log` + `fern` is additive to the
//! ecosystem already in place, not a replacement.
//!
//! **Panic capture:** `std::panic::set_hook` wired here logs the panic
//! message + location (file:line) at `error` level to the same sink before
//! chaining to the previous (default) hook, so a real crash is diagnosable
//! from `petal.log` alone even with no attached debugger/terminal.
//!
//! **Bounded disk use (#905):** the active file is `petal.log.<YYYY-MM-DD>`
//! (UTC), rolled to a new file at the date boundary MID-SESSION by
//! `DailyLogSink` -- no restart needed, fixing an earlier version of this
//! module where rotation only ran once at startup and a single long-running
//! launch could grow one file unboundedly (a real 6-day session reached
//! 263 MB, 26x its own cap). Each completed day is gzip'd; today's file is
//! left uncompressed so `tail -f` keeps working. A same-day size backstop
//! (`SAME_DAY_SIZE_BACKSTOP_BYTES`) still bounds one pathological all-day
//! meeting, reusing the same legacy-shaped `petal-<timestamp>.log` rotation
//! this module used everywhere before #905. Startup additionally prunes any
//! file (either naming shape, compressed or not) older than
//! `MAX_LOG_AGE_DAYS`. This keeps local instrumentation useful without
//! letting a long-lived install grow the log directory forever.
//!
//! **Redaction boundary:** local logs intentionally keep rich room/identity
//! context for same-machine debugging. Any future off-device export path must
//! call `redact_for_export()` before data leaves the machine; this module owns
//! that policy so #107/#121 don't invent separate redaction behavior.
//!
//! **Off-device crash/error reporting (#281):** `sentry::init()` (compile-time
//! `PETAL_SENTRY_DSN`-gated -- see `sentry_dsn()`) is the first statement of
//! `init()` below. It reuses this module's EXISTING panic hook and ObjC
//! uncaught-exception hook (no second hook chain), bridges `log::error!`/
//! `log::warn!` via `sentry-log`'s `SentryLogger` chained around the `fern`
//! dispatch, and enforces an allowlist-first PII policy in `before_send`/
//! `before_breadcrumb` that reuses `redact_for_export()` as a scrub backstop
//! -- see `scrub_event_for_sentry()`'s doc comment for the exact policy.
//! Sentry is for crashes and unreachable failures (panic, ObjC exception,
//! hard join/backend errors). Quality watchdogs (stalled video, silent
//! audio, capture restart-in-place, LiveKit reconnect *attempts*) log at
//! `warn!` so they stay in petal.log as breadcrumbs and do not open issues.
//! Rates of those events are a future PostHog allowlist
//! (`docs/POSTHOG_EVENT_ALLOWLIST.md`), not a Sentry issue class.
//! Native `.ips` crash symbolication is explicitly out of scope. A runtime
//! opt-out (Settings -> Diagnostics, general-purpose, not panic-only) is
//! enforced by `SENTRY_ENABLED`, checked at the top of both scrub hooks.
//!
//! **Idempotency / no double-init:** `log::set_boxed_logger` can only
//! succeed once per process; `init()` returns the resolved log file path on
//! success and is safe to call at most once (from `run()`). The example
//! probes' own `env_logger::init()` calls are untouched and unaffected —
//! they're separate `fn main()` binaries that never link against this
//! function.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use zip::write::FileOptions;

const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const MAX_ROTATED_LOG_FILES: usize = 5;

/// Same-day size backstop (#905): a single day's active `petal.log.<date>`
/// file that grows past this is rotated out mid-day via the legacy
/// `rename_oversized_log`/`prune_rotated_logs` mechanism (renamed to `petal-<timestamp>.log`,
/// same as the pre-#905 startup-only rotation) so one pathological all-day
/// meeting still can't grow one file forever. Reuses `MAX_LOG_BYTES` rather
/// than a second constant: this is the same "how big is too big for one
/// file" judgment call, just now applied per-day instead of only at
/// startup.
const SAME_DAY_SIZE_BACKSTOP_BYTES: u64 = MAX_LOG_BYTES;

/// Age-based retention for the per-day log scheme (#905): a
/// `petal.log.<date>[.gz]` (or legacy `petal.log`/`petal-*.log[.gz]`) file
/// older than this many days is deleted at startup. Chosen so a user can
/// still report "yesterday" or "a few days ago" without the whole directory
/// growing forever -- the field log that motivated this issue spanned 6
/// days in a single unrotated file.
const MAX_LOG_AGE_DAYS: i64 = 7;

/// Default window for the "Export logs" command when the caller doesn't
/// specify one (#905): the current day plus the previous one, so a report
/// filed just after midnight still carries a full day of context.
const DEFAULT_EXPORT_LOG_DAYS: i64 = 2;

/// README text for the manual "Export logs" button (Settings -> Export
/// logs). This copy of the archive is never sent anywhere by Petal itself.
const LOCAL_EXPORT_README: &[u8] = b"Petal local log export. Log text is redacted for room and identity values before export.\nNo data was sent off this machine.\n";

/// README text for the bounded, opt-in diagnostic attachment offered by the
/// UserDispatch feedback modal (#292). Unlike `LOCAL_EXPORT_README`, THIS
/// copy may be transmitted off-device if the user opts in and submits
/// feedback with it attached -- the wording must say so plainly and must
/// never repeat the "no data was sent off this machine" claim above.
const FEEDBACK_ATTACHMENT_README: &[u8] = b"Petal feedback diagnostic attachment. Log text is redacted for room and identity values before export.\nThis file was attached to a UserDispatch feedback submission and may be sent off this machine if you submit it.\n";

/// Most recent bytes of the active `petal.log` considered for a feedback
/// attachment (#292) -- bounded so a long-running session's full log can
/// never inflate a single feedback submission. Cuts from the *front* (i.e.
/// keeps the tail), since the most recent activity is the most relevant to
/// "what just happened" for a support attachment.
const FEEDBACK_ATTACHMENT_LOG_TAIL_BYTES: usize = 256 * 1024;

/// Hard cap on the finished, redacted feedback-attachment zip returned to
/// the frontend/webview. `feedback::prepare_feedback_diagnostics` fails
/// closed (an `Err`, never a partial/truncated payload) if building the
/// archive would exceed this -- the frontend then offers "submit without
/// diagnostics" instead of silently sending a truncated file.
const FEEDBACK_ATTACHMENT_MAX_ZIP_BYTES: usize = 512 * 1024;

/// Timeout for the explicit pre-abort Sentry flush (#281 point 6), on both
/// the panic and ObjC-exception paths. Also used as `ClientOptions::
/// shutdown_timeout` so the two stay consistent. Pinned per the issue's
/// "~2s timeout" guidance rather than left to a default.
const SENTRY_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// `log::Record` target used ONLY by the panic hook's own local-file
/// summary line (`install_panic_hook()`). The `sentry-log` bridge's filter
/// (see `init_sentry()`'s `SentryLogger` construction) ignores this target
/// specifically so that line does NOT ALSO become a second, lower-quality
/// Sentry event (no backtrace, generic "message" type) alongside the
/// purpose-built one `forward_panic_to_sentry` sends directly -- confirmed
/// live during #281's verification (a single test panic produced two
/// separate Sentry issues before this filter existed). Does not affect the
/// local `petal.log`/stdout sinks at all -- fern's own per-target rules
/// don't reference this target, so it logs there exactly as before.
const PANIC_HOOK_LOG_TARGET: &str = "petal::panic_hook_internal";

/// Same reasoning as `PANIC_HOOK_LOG_TARGET`, for the ObjC uncaught-exception
/// handler's own local-file summary line.
const OBJC_EXCEPTION_HOOK_LOG_TARGET: &str = "petal::objc_exception_hook_internal";

/// The process-wide sink for native WebRTC's `RTC_LOG` lines (#787), plus the
/// Rust crate's own module targets under it. Held to breadcrumbs in the Sentry
/// bridge: see the filter in `init_logging` for why an `error!` here must not
/// become a Sentry event.
const NATIVE_WEBRTC_LOG_TARGET: &str = "libwebrtc";
const NATIVE_WEBRTC_LOG_TARGET_PREFIX: &str = "libwebrtc::";

/// Holds the Sentry client guard for the process lifetime (#281 point 4).
/// `sentry::init()`'s returned `ClientInitGuard` drops (and silently
/// disables all reporting, per its own doc comment) as soon as it goes out
/// of scope -- a `OnceLock` static set once from `init_sentry()` keeps it
/// alive until process exit. Never read for its value elsewhere; its only
/// job is to not be dropped. Doubles as the "is Sentry active" check used
/// by the panic/ObjC-exception forwarders (`SENTRY_GUARD.get().is_some()`).
static SENTRY_GUARD: OnceLock<sentry::ClientInitGuard> = OnceLock::new();

/// General "send diagnostics to Sentry" runtime switch -- deliberately not
/// panic-only (per the task brief: "we'll use it for other stuff in the
/// future"). Gated at a single choke point in `scrub_event_for_sentry`/
/// `scrub_breadcrumb_for_sentry` (their `before_send`/`before_breadcrumb`
/// hooks run for every capture path: panic hook, ObjC uncaught-exception
/// hook, and the `log::error!`/`warn!` bridge), so every future Sentry call
/// site is covered for free without its own check. Defaults to `true` --
/// same default-on-then-seeded-from-frontend posture as
/// `SessionState::remote_control_allowed` (see `session/mod.rs`), because
/// `init()` runs before any `AppHandle`/webview exists and so cannot read
/// the frontend's persisted `sentryEnabled` preference at process boot; the
/// frontend syncs the real value down via `set_sentry_enabled` as soon as
/// the app UI loads (`+layout.svelte`) or the Settings toggle changes.
static SENTRY_ENABLED: AtomicBool = AtomicBool::new(true);

/// Closed, privacy-safe diagnostics emitted by native capture/camera code.
///
/// This deliberately accepts no strings: callers must classify local state
/// into these bounded enums before crossing the off-device boundary (#550).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentryDiagnosticEvent {
    CaptureLayoutInvalid(CaptureLayoutDiagnostic),
    CameraHealth(CameraHealthDiagnostic),
    CameraSizeMismatchRecovery(CameraSizeMismatchDiagnostic),
    PlayoutDeviceRepointed(PlayoutDeviceDiagnostic),
    RepublishStorm(StormDiagnostic),
    PublishDropStreak(StormDiagnostic),
    WatchdogRepeatStorm(StormDiagnostic),
    UpdateInstallFailed(UpdateInstallFailedDiagnostic),
    ShareOverlayCursorCaptureCleared(ShareOverlayCursorCaptureDiagnostic),
    WindowServerPortDead(WindowServerPortDeadDiagnostic),
    PreviousSessionVanished(PreviousSessionVanishedDiagnostic),
    WindowServerRestartDetected(WindowServerRestartDetectedDiagnostic),
    MemoryPressure(MemoryPressureDiagnostic),
    DecoderAllocationFailed(DecoderAllocationFailedDiagnostic),
    BrowserUrlExtractionFailed(BrowserUrlExtractionFailedDiagnostic),
}

/// Emitted (rate-limited) when the OS memory-pressure level transitions to
/// warn or critical while in a room (#884) -- the earliest system-visible
/// stage of the #878 failure chain (leak -> pressure -> allocation failures
/// -> jetsam-class session teardown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPressureDiagnostic {
    pub level: PressureLevelTag,
}

/// Emitted (rate-limited) when libwebrtc reports a decode failure with
/// kCVReturnAllocationFailed (-6662) -- the system failing to allocate
/// pixel buffers/IOSurfaces. In the #878 field logs this signature only
/// survived because libwebrtc happened to print to the log; it is the
/// machine-wide allocation-pressure smoking gun and must be first-class
/// (#884).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderAllocationFailedDiagnostic {
    pub role: DiagnosticRole,
}

/// Emitted once per share (at the first extraction failure for that share --
/// never per poll) when macOS AppleScript URL extraction for a shared
/// browser window's tab fails, so a failure signature that previously never
/// left the user's machine (every browser-share URL miss logged, at most,
/// to `petal.log`) becomes field-visible (#915). Never emitted for
/// `Unsupported` (not a recognised browser bundle id) or a success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserUrlExtractionFailedDiagnostic {
    pub cause: BrowserUrlExtractionCauseTag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureLayoutDiagnostic {
    pub role: DiagnosticRole,
    pub source: SourceSelectionClass,
    pub capture_geometry: GeometryBucket,
    pub configured_geometry: GeometryBucket,
    pub pixel_format: PixelFormatClass,
    pub scale: ScaleBucket,
    pub encoder: EncoderImplementationClass,
    pub stage: CaptureLayoutStage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraHealthDiagnostic {
    pub role: DiagnosticRole,
    pub direction: CameraDirection,
    pub capture_cadence: CadenceBucket,
    pub encode_cadence: CadenceBucket,
    pub queue_backpressure: QueueBackpressureBucket,
    pub decoder_render: DecoderRenderHealth,
}

/// Emitted once per episode when a camera whose frames no longer match the
/// published track size recovers instead of dropping frames forever (#866).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraSizeMismatchDiagnostic {
    pub role: DiagnosticRole,
    pub direction: CameraDirection,
    pub capture_geometry: GeometryBucket,
    pub configured_geometry: GeometryBucket,
    pub action: CameraRecoveryActionTag,
}

/// Emitted once per re-point episode when live speaker playout follows a new
/// default device or reports that the default is unavailable (#867).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutDeviceDiagnostic {
    pub role: DiagnosticRole,
    pub transition: PlayoutTransitionTag,
}

/// Emitted when a signature that is unremarkable once fires at a rate that means
/// sustained failure (#788). Rate-triggered, then deduped by `DiagnosticRateLimiter`,
/// so a storm of thousands of samples pages at most once per 60 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StormDiagnostic {
    pub role: DiagnosticRole,
    pub scope: StormScopeTag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateInstallFailedDiagnostic {
    pub stage: InstallFailureStageTag,
    pub kind: InstallFailureKindTag,
    pub boundary: InstallVolumeBoundaryTag,
    pub destination: InstallDestinationClassTag,
}

/// Emitted once per episode when the sharer-overlay watchdog forces an overlay
/// back to click-through because it was capturing the cursor with no live
/// publication behind it -- the state that made the user's whole desktop
/// unclickable in #872. Nothing recorded `draw_active` before this.
/// Emitted once when `SLSRequestNotificationsForWindows` reports
/// `MACH_SEND_INVALID_DEST` -- the window-server Mach port is dead and the
/// `winsrv-sls` subscription thread is about to stop itself (#878).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowServerPortDeadDiagnostic {
    pub role: DiagnosticRole,
}

/// Emitted once at startup when the previous session's log tail shows a
/// room join with no shutdown marker and no crash report to explain the gap
/// (#878's vanished-session detector).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviousSessionVanishedDiagnostic {
    pub crash_report: VanishedSessionCrashReportTag,
}

/// Emitted once at startup when the highest `CGWindowID` this session has
/// seen so far is less than half the previous session's persisted high
/// water mark -- window IDs are server-assigned and monotonically
/// increasing except across a real window-server restart (#878).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowServerRestartDetectedDiagnostic {
    pub role: DiagnosticRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareOverlayCursorCaptureDiagnostic {
    pub role: DiagnosticRole,
    pub reason: OverlayClearReasonTag,
}

macro_rules! diagnostic_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl $name {
            const fn tag(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }
    };
}

diagnostic_enum!(DiagnosticRole { Sharer => "sharer", Receiver => "receiver", Both => "both" });
diagnostic_enum!(SourceSelectionClass { Window => "window", Display => "display", SystemPicker => "system_picker", Unknown => "unknown" });
diagnostic_enum!(GeometryBucket { Tiny => "tiny", Small => "small", Medium => "medium", Large => "large", VeryLarge => "very_large", Unknown => "unknown", NotApplicable => "not_applicable" });
diagnostic_enum!(PixelFormatClass { Bgra => "bgra", Nv12 => "nv12", OtherSupported => "other_supported", Unknown => "unknown", NotApplicable => "not_applicable" });
diagnostic_enum!(ScaleBucket { OneX => "1x", TwoX => "2x", Fractional => "fractional", Other => "other", Unknown => "unknown", NotApplicable => "not_applicable" });
diagnostic_enum!(EncoderImplementationClass { Hardware => "hardware", Software => "software", Unknown => "unknown", NotApplicable => "not_applicable" });
diagnostic_enum!(CaptureLayoutStage { Validation => "validation", Reconfiguration => "reconfiguration", FirstFrame => "first_frame", Publish => "publish", Unknown => "unknown" });
diagnostic_enum!(CameraDirection { Publish => "publish", Receive => "receive" });
diagnostic_enum!(OverlayClearReasonTag { NoPublication => "no_publication", Retired => "retired", HidePending => "hide_pending" });
diagnostic_enum!(CadenceBucket { Healthy => "healthy", Reduced => "reduced", Severe => "severe", Stalled => "stalled", Unknown => "unknown", NotApplicable => "not_applicable" });
diagnostic_enum!(QueueBackpressureBucket { None => "none", Low => "low", High => "high", Saturated => "saturated", Unknown => "unknown", NotApplicable => "not_applicable" });
diagnostic_enum!(DecoderRenderHealth { Healthy => "healthy", DecoderDegraded => "decoder_degraded", RenderDegraded => "render_degraded", BothDegraded => "both_degraded", Unknown => "unknown", NotApplicable => "not_applicable" });
diagnostic_enum!(CameraRecoveryActionTag { Reanchor => "reanchor", Letterbox => "letterbox", NotApplicable => "not_applicable" });
diagnostic_enum!(PlayoutTransitionTag { Repointed => "repointed", Unavailable => "unavailable", NotApplicable => "not_applicable" });
diagnostic_enum!(StormScopeTag {
    WindowShare => "window_share",
    Camera => "camera",
    RemoteWindow => "remote_window",
    Unknown => "unknown",
    NotApplicable => "not_applicable"
});

diagnostic_enum!(InstallFailureStageTag { Resolve => "resolve", Stage => "stage", Extract => "extract", Backup => "backup", Promote => "promote", Rollback => "rollback", Privileged => "privileged", NotApplicable => "not_applicable" });
diagnostic_enum!(InstallFailureKindTag { CrossDevice => "cross_device", PermissionDenied => "permission_denied", ReadOnly => "read_only", NoSpace => "no_space", NotFound => "not_found", Other => "other", NotApplicable => "not_applicable" });
diagnostic_enum!(InstallVolumeBoundaryTag { SameVolume => "same_volume", CrossVolume => "cross_volume", Unknown => "unknown", NotApplicable => "not_applicable" });
diagnostic_enum!(InstallDestinationClassTag { Applications => "applications", UserApplications => "user_applications", DiskImage => "disk_image", RemovableVolume => "removable_volume", Other => "other", NotApplicable => "not_applicable" });
diagnostic_enum!(VanishedSessionCrashReportTag { Found => "found", NotFound => "not_found", NotApplicable => "not_applicable" });
diagnostic_enum!(PressureLevelTag { Warn => "warn", Critical => "critical", NotApplicable => "not_applicable" });
/// Mirrors `browser_url::UrlExtraction::cause()`'s string set exactly (minus
/// "ok"/"unsupported", which are never logged as a failure) -- keep the two
/// in sync by hand; there is no shared source because `browser_url` must not
/// depend on `logging`'s Sentry machinery.
diagnostic_enum!(BrowserUrlExtractionCauseTag {
    Denied => "denied",
    Timeout => "timeout",
    Ambiguous => "ambiguous",
    NoMatch => "no-match",
    Spawn => "spawn",
    Failed => "failed",
    NotApplicable => "not_applicable"
});

const SENTRY_DIAGNOSTIC_SCHEMA_VERSION: &str = "1";
const SENTRY_DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(60);
const DIAGNOSTIC_EVENT_NAMES: [&str; 15] = [
    "capture-layout-invalid",
    "camera-health",
    "camera-size-mismatch-recovery",
    "playout-device-repointed",
    "republish-storm",
    "publish-drop-streak",
    "watchdog-repeat-storm",
    "update-install-failed",
    "share-overlay-cursor-capture-cleared",
    "winsrv-port-dead",
    "previous-session-vanished",
    "window-server-restart-detected",
    "memory-pressure",
    "decoder-allocation-failed",
    "browser-url-extraction-failed",
];
const DIAGNOSTIC_TAGS: &[&str] = &[
    "event_name",
    "schema_version",
    "build_version",
    "os_version",
    "architecture",
    "session_role",
    "source_selection",
    "capture_geometry",
    "configured_geometry",
    "pixel_format",
    "scale_bucket",
    "encoder_implementation",
    "stage_code",
    "camera_direction",
    "capture_cadence",
    "encode_cadence",
    "queue_backpressure",
    "decoder_render_health",
    "recovery_action",
    "playout_transition",
    "storm_scope",
    "install_failure_stage",
    "install_failure_kind",
    "install_volume_boundary",
    "install_destination_class",
    "overlay_clear_reason",
    "crash_report_status",
    "pressure_level",
    "browser_url_extraction_cause",
    "dedup_count_bucket",
];

const CAPTURE_LAYOUT_MESSAGE_TAGS: &[&str] = &[
    "session_role",
    "source_selection",
    "capture_geometry",
    "configured_geometry",
    "pixel_format",
    "scale_bucket",
    "encoder_implementation",
    "stage_code",
];
const CAMERA_HEALTH_MESSAGE_TAGS: &[&str] = &[
    "session_role",
    "camera_direction",
    "capture_cadence",
    "encode_cadence",
    "queue_backpressure",
    "decoder_render_health",
];

const SHARE_OVERLAY_CURSOR_CAPTURE_MESSAGE_TAGS: &[&str] = &["session_role", "overlay_clear_reason"];
const WINSRV_PORT_DEAD_MESSAGE_TAGS: &[&str] = &["session_role"];
const PREVIOUS_SESSION_VANISHED_MESSAGE_TAGS: &[&str] = &["crash_report_status"];
const WINDOW_SERVER_RESTART_DETECTED_MESSAGE_TAGS: &[&str] = &["session_role"];
const MEMORY_PRESSURE_MESSAGE_TAGS: &[&str] = &["pressure_level"];
const DECODER_ALLOCATION_FAILED_MESSAGE_TAGS: &[&str] = &["session_role"];
const BROWSER_URL_EXTRACTION_FAILED_MESSAGE_TAGS: &[&str] = &["browser_url_extraction_cause"];
const CAMERA_SIZE_MISMATCH_MESSAGE_TAGS: &[&str] = &[
    "session_role",
    "camera_direction",
    "recovery_action",
    "capture_geometry",
    "configured_geometry",
];
const PLAYOUT_DEVICE_MESSAGE_TAGS: &[&str] = &["session_role", "playout_transition"];
const STORM_MESSAGE_TAGS: &[&str] = &["session_role", "storm_scope"];
const UPDATE_INSTALL_FAILED_MESSAGE_TAGS: &[&str] = &[
    "install_failure_stage",
    "install_failure_kind",
    "install_volume_boundary",
    "install_destination_class",
];

/// Every event name in `DIAGNOSTIC_EVENT_NAMES` must map here. A missing arm
/// returns None, `diagnostic_message` then yields None, and the event ships
/// with no title -- `<unlabeled event>` in Sentry, the exact defect #788 was
/// filed to fix. `valid_sentry_diagnostic_event` will NOT catch it: it is
/// fail-closed on tag count but fail-open on an absent message (None == None).
/// `every_diagnostic_event_name_has_message_tags` is what actually pins this.
fn diagnostic_message_tags(event_name: &str) -> Option<&'static [&'static str]> {
    match event_name {
        "capture-layout-invalid" => Some(CAPTURE_LAYOUT_MESSAGE_TAGS),
        "camera-health" => Some(CAMERA_HEALTH_MESSAGE_TAGS),
        "camera-size-mismatch-recovery" => Some(CAMERA_SIZE_MISMATCH_MESSAGE_TAGS),
        "share-overlay-cursor-capture-cleared" => Some(SHARE_OVERLAY_CURSOR_CAPTURE_MESSAGE_TAGS),
        "playout-device-repointed" => Some(PLAYOUT_DEVICE_MESSAGE_TAGS),
        "republish-storm" | "publish-drop-streak" | "watchdog-repeat-storm" => {
            Some(STORM_MESSAGE_TAGS)
        }
        "update-install-failed" => Some(UPDATE_INSTALL_FAILED_MESSAGE_TAGS),
        "winsrv-port-dead" => Some(WINSRV_PORT_DEAD_MESSAGE_TAGS),
        "previous-session-vanished" => Some(PREVIOUS_SESSION_VANISHED_MESSAGE_TAGS),
        "window-server-restart-detected" => Some(WINDOW_SERVER_RESTART_DETECTED_MESSAGE_TAGS),
        "memory-pressure" => Some(MEMORY_PRESSURE_MESSAGE_TAGS),
        "decoder-allocation-failed" => Some(DECODER_ALLOCATION_FAILED_MESSAGE_TAGS),
        "browser-url-extraction-failed" => Some(BROWSER_URL_EXTRACTION_FAILED_MESSAGE_TAGS),
        _ => None,
    }
}

/// The ONE definition of a diagnostic's title, derived purely from closed-enum
/// tag values so `valid_sentry_diagnostic_event` can recompute and byte-compare
/// it -- an arbitrary string can never survive as a diagnostic message (#788).
fn diagnostic_message(tags: &sentry::protocol::Map<String, String>) -> Option<String> {
    let event_name = tags.get("event_name").map(String::as_str)?;
    let keys = diagnostic_message_tags(event_name)?;
    let mut message = format!("diagnostic: {event_name}");
    for key in keys {
        let value = tags.get(*key)?;
        message.push(' ');
        message.push_str(key);
        message.push('=');
        message.push_str(value);
    }
    Some(message)
}

/// Eight completed republishes in ten seconds is far above normal resize churn,
/// while still detecting #788's 232-in-73-second field storm quickly.
const REPUBLISH_STORM_THRESHOLD: u32 = 8;
const REPUBLISH_STORM_WINDOW: Duration = Duration::from_secs(10);
/// Thirty consecutive drops sustained for a second distinguishes total media
/// loss from isolated backpressure without letting a high-fps source trip early.
const PUBLISH_DROP_STREAK_FRAMES: u32 = 30;
const PUBLISH_DROP_STREAK_MIN_DURATION: Duration = Duration::from_secs(1);
/// Three creation stalls in thirty seconds identifies a repeating watchdog
/// failure while leaving a single slow AppKit window build unremarkable.
const WATCHDOG_REPEAT_THRESHOLD: u32 = 3;
const WATCHDOG_REPEAT_WINDOW: Duration = Duration::from_secs(30);

/// Counts occurrences of one signature per key inside a sliding window and
/// reports only the crossing. Storms are defined by RATE: each individual
/// firing is unremarkable, hundreds in seconds is a total failure (#788).
pub(crate) struct KeyedRateWindow {
    threshold: u32,
    window: Duration,
    max_keys: usize,
    keys: Vec<(u64, VecDeque<Instant>)>,
}

impl KeyedRateWindow {
    const fn new(threshold: u32, window: Duration, max_keys: usize) -> Self {
        Self {
            threshold,
            window,
            max_keys,
            keys: Vec::new(),
        }
    }

    fn record(&mut self, key: u64, now: Instant) -> bool {
        let index = if let Some(index) = self.keys.iter().position(|(candidate, _)| *candidate == key)
        {
            index
        } else {
            if self.max_keys == 0 {
                return false;
            }
            if self.keys.len() == self.max_keys {
                let oldest = self
                    .keys
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, (_, occurrences))| occurrences.back().copied())
                    .map(|(index, _)| index)
                    .expect("a full keyed rate window has an eviction candidate");
                self.keys.swap_remove(oldest);
            }
            self.keys.push((key, VecDeque::new()));
            self.keys.len() - 1
        };

        let occurrences = &mut self.keys[index].1;
        while occurrences
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) > self.window)
        {
            occurrences.pop_front();
        }
        occurrences.push_back(now);
        if self.threshold > 0 && occurrences.len() >= self.threshold as usize {
            occurrences.clear();
            return true;
        }
        false
    }

    #[cfg(test)]
    fn reset(&mut self) {
        self.keys.clear();
    }
}

/// A publisher that drops frames continuously has stopped delivering media
/// entirely; a single dropped frame is normal. Trips only when BOTH a frame
/// count and an elapsed duration are exceeded with no successful push in
/// between, so a fast source cannot trip it on a sub-second hiccup (#788).
#[derive(Debug, Default)]
pub struct DropStreakDetector {
    streak: u32,
    started: Option<Instant>,
}

impl DropStreakDetector {
    pub(crate) fn record(&mut self, published: bool, now: Instant) -> bool {
        if published {
            self.streak = 0;
            self.started = None;
            return false;
        }

        self.streak = self.streak.saturating_add(1);
        let started = *self.started.get_or_insert(now);
        if self.streak >= PUBLISH_DROP_STREAK_FRAMES
            && now.saturating_duration_since(started) >= PUBLISH_DROP_STREAK_MIN_DURATION
        {
            self.streak = 0;
            self.started = Some(now);
            return true;
        }
        false
    }
}

static REPUBLISH_STORM_DETECTOR: OnceLock<Mutex<KeyedRateWindow>> = OnceLock::new();
static WATCHDOG_REPEAT_STORM_DETECTOR: OnceLock<Mutex<KeyedRateWindow>> = OnceLock::new();

fn note_republish_complete_at(window_id: u32, now: Instant) -> Option<SentryDiagnosticEvent> {
    let crossed = REPUBLISH_STORM_DETECTOR
        .get_or_init(|| {
            Mutex::new(KeyedRateWindow::new(
                REPUBLISH_STORM_THRESHOLD,
                REPUBLISH_STORM_WINDOW,
                32,
            ))
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .record(u64::from(window_id), now);
    crossed.then_some(SentryDiagnosticEvent::RepublishStorm(StormDiagnostic {
        role: DiagnosticRole::Sharer,
        scope: StormScopeTag::WindowShare,
    }))
}

/// Call once per completed republish of `window_id`.
pub fn note_republish_complete(window_id: u32) {
    if let Some(event) = note_republish_complete_at(window_id, Instant::now()) {
        capture_sentry_diagnostic(event);
    }
}

fn note_window_creation_watchdog_stall_at(
    window_id: u32,
    now: Instant,
) -> Option<SentryDiagnosticEvent> {
    let crossed = WATCHDOG_REPEAT_STORM_DETECTOR
        .get_or_init(|| {
            Mutex::new(KeyedRateWindow::new(
                WATCHDOG_REPEAT_THRESHOLD,
                WATCHDOG_REPEAT_WINDOW,
                32,
            ))
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .record(u64::from(window_id), now);
    crossed.then_some(SentryDiagnosticEvent::WatchdogRepeatStorm(
        StormDiagnostic {
            role: DiagnosticRole::Receiver,
            scope: StormScopeTag::RemoteWindow,
        },
    ))
}

/// Call once per remote-window creation-watchdog stall for `window_id`.
pub fn note_window_creation_watchdog_stall(window_id: u32) {
    if let Some(event) = note_window_creation_watchdog_stall_at(window_id, Instant::now()) {
        capture_sentry_diagnostic(event);
    }
}

/// Log lines that arrive in bursts of hundreds within seconds. Unsuppressed
/// they fill the 50-slot Sentry breadcrumb ring and evict the join-tail
/// context the eventual `error!` needs to be diagnosable (#788). Keeping one
/// per signature per interval leaves the storm visible without erasing
/// everything around it.
const BREADCRUMB_STORM_SIGNATURES: &[&str] = &[
    // #866: this literal MUST match transport/publisher.rs's
    // `resolve_camera_push_size` recovery-episode log line exactly (its
    // predecessor, "Dropping NV12 frame:", stopped existing when that fix
    // landed and this signature silently matched nothing for a while).
    "past the drop grace; recovering via",
    "creation watchdog fired",
    "republish complete (",
];
const BREADCRUMB_STORM_INTERVAL: Duration = Duration::from_secs(10);

/// #884: matches libwebrtc's decode-failure line ONLY when the status is
/// kCVReturnAllocationFailed (-6662, CoreVideo "allocation for a buffer or
/// buffer pool failed... lack of resources"). The literal must track
/// RTCVideoDecoderH264.mm's "Failed to decode frame. Status: <n>" shape --
/// the #866 lesson: a matcher pointed at a log line that stops existing
/// silently matches nothing. Other statuses (e.g. -12909 bad-data) are
/// ordinary stream errors and must NOT match.
fn decoder_allocation_failure_signature(message: &str) -> bool {
    message.contains("Failed to decode frame") && message.contains("-6662")
}

static BREADCRUMB_STORM_LAST_KEPT: Mutex<
    [Option<std::time::Instant>; BREADCRUMB_STORM_SIGNATURES.len()],
> = Mutex::new([None; BREADCRUMB_STORM_SIGNATURES.len()]);

fn breadcrumb_storm_allows(message: &str, now: std::time::Instant) -> bool {
    let Some(index) = BREADCRUMB_STORM_SIGNATURES
        .iter()
        .position(|signature| message.contains(signature))
    else {
        return true;
    };
    let mut last_kept = BREADCRUMB_STORM_LAST_KEPT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if last_kept[index].is_some_and(|last| now.duration_since(last) < BREADCRUMB_STORM_INTERVAL) {
        return false;
    }
    last_kept[index] = Some(now);
    true
}

#[derive(Default)]
struct DiagnosticRateLimiter {
    last_sent: [Option<std::time::Instant>; DIAGNOSTIC_EVENT_NAMES.len()],
    suppressed: [u32; DIAGNOSTIC_EVENT_NAMES.len()],
}

impl DiagnosticRateLimiter {
    fn allow(&mut self, event_name: &str, now: std::time::Instant) -> Option<&'static str> {
        let index = DIAGNOSTIC_EVENT_NAMES
            .iter()
            .position(|name| *name == event_name)?;
        if self.last_sent[index]
            .is_some_and(|last| now.duration_since(last) < SENTRY_DIAGNOSTIC_INTERVAL)
        {
            self.suppressed[index] = self.suppressed[index].saturating_add(1);
            return None;
        }
        self.last_sent[index] = Some(now);
        let bucket = match self.suppressed[index] {
            0 => "1",
            1..=8 => "2_9",
            9..=98 => "10_99",
            _ => "100_plus",
        };
        self.suppressed[index] = 0;
        Some(bucket)
    }
}

static SENTRY_DIAGNOSTIC_RATE_LIMITER: OnceLock<Mutex<DiagnosticRateLimiter>> = OnceLock::new();

/// Capture a schema-validated diagnostic event. This is inert when no DSN is
/// configured or the user disabled diagnostics, and emits at most one event
/// per class per bounded interval.
pub fn capture_sentry_diagnostic(event: SentryDiagnosticEvent) -> bool {
    capture_sentry_diagnostic_with_client(event, SENTRY_GUARD.get().is_some())
}

fn capture_sentry_diagnostic_with_client(
    event: SentryDiagnosticEvent,
    client_active: bool,
) -> bool {
    if !client_active || !SENTRY_ENABLED.load(Ordering::Relaxed) {
        return false;
    }
    let event_name = event.event_name();
    let mut limiter = SENTRY_DIAGNOSTIC_RATE_LIMITER
        .get_or_init(|| Mutex::new(DiagnosticRateLimiter::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(dedup_count_bucket) = limiter.allow(event_name, std::time::Instant::now()) else {
        return false;
    };
    drop(limiter);
    capture_sentry_diagnostic_on_current_hub(event, dedup_count_bucket);
    true
}

fn capture_sentry_diagnostic_on_current_hub(
    event: SentryDiagnosticEvent,
    dedup_count_bucket: &'static str,
) {
    // Diagnostics must not inherit arbitrary application scope. In
    // particular, attachments are appended to the envelope after
    // `before_send`, so rebuilding the Event there cannot remove them.
    sentry::with_scope(
        |scope| scope.clear(),
        || {
            sentry::capture_event(build_sentry_diagnostic_event(event, dedup_count_bucket));
        },
    );
}

impl SentryDiagnosticEvent {
    fn event_name(self) -> &'static str {
        match self {
            Self::CaptureLayoutInvalid(_) => "capture-layout-invalid",
            Self::CameraHealth(_) => "camera-health",
            Self::CameraSizeMismatchRecovery(_) => "camera-size-mismatch-recovery",
            Self::PlayoutDeviceRepointed(_) => "playout-device-repointed",
            Self::RepublishStorm(_) => "republish-storm",
            Self::PublishDropStreak(_) => "publish-drop-streak",
            Self::WatchdogRepeatStorm(_) => "watchdog-repeat-storm",
            Self::UpdateInstallFailed(_) => "update-install-failed",
            Self::ShareOverlayCursorCaptureCleared(_) => "share-overlay-cursor-capture-cleared",
            Self::WindowServerPortDead(_) => "winsrv-port-dead",
            Self::PreviousSessionVanished(_) => "previous-session-vanished",
            Self::WindowServerRestartDetected(_) => "window-server-restart-detected",
            Self::MemoryPressure(_) => "memory-pressure",
            Self::DecoderAllocationFailed(_) => "decoder-allocation-failed",
            Self::BrowserUrlExtractionFailed(_) => "browser-url-extraction-failed",
        }
    }
}

fn build_sentry_diagnostic_event(
    diagnostic: SentryDiagnosticEvent,
    dedup_count_bucket: &'static str,
) -> sentry::protocol::Event<'static> {
    let mut tags = sentry::protocol::Map::new();
    let mut insert = |key: &'static str, value: &'static str| {
        tags.insert(key.into(), value.into());
    };
    insert("event_name", diagnostic.event_name());
    insert("schema_version", SENTRY_DIAGNOSTIC_SCHEMA_VERSION);
    insert("build_version", env!("CARGO_PKG_VERSION"));
    insert("os_version", os_version_tag());
    insert(
        "architecture",
        match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x86_64",
            _ => "other",
        },
    );
    match diagnostic {
        SentryDiagnosticEvent::CaptureLayoutInvalid(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", value.source.tag());
            insert("capture_geometry", value.capture_geometry.tag());
            insert("configured_geometry", value.configured_geometry.tag());
            insert("pixel_format", value.pixel_format.tag());
            insert("scale_bucket", value.scale.tag());
            insert("encoder_implementation", value.encoder.tag());
            insert("stage_code", value.stage.tag());
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::CameraHealth(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", value.direction.tag());
            insert("capture_cadence", value.capture_cadence.tag());
            insert("encode_cadence", value.encode_cadence.tag());
            insert("queue_backpressure", value.queue_backpressure.tag());
            insert("decoder_render_health", value.decoder_render.tag());
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::CameraSizeMismatchRecovery(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", value.capture_geometry.tag());
            insert("configured_geometry", value.configured_geometry.tag());
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", value.direction.tag());
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", value.action.tag());
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::PlayoutDeviceRepointed(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", value.transition.tag());
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::RepublishStorm(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", value.scope.tag());
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::PublishDropStreak(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", value.scope.tag());
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::WatchdogRepeatStorm(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", value.scope.tag());
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::UpdateInstallFailed(value) => {
            insert("session_role", "not_applicable");
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", value.stage.tag());
            insert("install_failure_kind", value.kind.tag());
            insert("install_volume_boundary", value.boundary.tag());
            insert("install_destination_class", value.destination.tag());
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::ShareOverlayCursorCaptureCleared(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("overlay_clear_reason", value.reason.tag());
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::WindowServerPortDead(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::PreviousSessionVanished(value) => {
            insert("session_role", "not_applicable");
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", value.crash_report.tag());
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::WindowServerRestartDetected(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::MemoryPressure(value) => {
            insert("session_role", "not_applicable");
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", value.level.tag());
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::DecoderAllocationFailed(value) => {
            insert("session_role", value.role.tag());
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", "not_applicable");
        }
        SentryDiagnosticEvent::BrowserUrlExtractionFailed(value) => {
            insert("session_role", "not_applicable");
            insert("source_selection", "not_applicable");
            insert("capture_geometry", "not_applicable");
            insert("configured_geometry", "not_applicable");
            insert("pixel_format", "not_applicable");
            insert("scale_bucket", "not_applicable");
            insert("encoder_implementation", "not_applicable");
            insert("stage_code", "not_applicable");
            insert("camera_direction", "not_applicable");
            insert("capture_cadence", "not_applicable");
            insert("encode_cadence", "not_applicable");
            insert("queue_backpressure", "not_applicable");
            insert("decoder_render_health", "not_applicable");
            insert("recovery_action", "not_applicable");
            insert("playout_transition", "not_applicable");
            insert("storm_scope", "not_applicable");
            insert("install_failure_stage", "not_applicable");
            insert("install_failure_kind", "not_applicable");
            insert("install_volume_boundary", "not_applicable");
            insert("install_destination_class", "not_applicable");
            insert("overlay_clear_reason", "not_applicable");
            insert("crash_report_status", "not_applicable");
            insert("pressure_level", "not_applicable");
            insert("browser_url_extraction_cause", value.cause.tag());
        }
    }
    insert("dedup_count_bucket", dedup_count_bucket);
    let message = diagnostic_message(&tags);
    sentry::protocol::Event {
        message,
        tags,
        fingerprint: Cow::Owned(vec![diagnostic.event_name().into()]),
        ..Default::default()
    }
}

/// Tauri command backing Settings -> Diagnostics' "Send crash and error
/// reports to Sentry" toggle. Takes effect immediately: the very next event/
/// breadcrumb evaluated by `scrub_event_for_sentry`/`scrub_breadcrumb_for_sentry`
/// observes the new value.
#[tauri::command]
pub fn set_sentry_enabled(enabled: bool) {
    SENTRY_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Closed webview-to-native bridge for the gallery's remote-camera health
/// buckets. It rejects every value outside the small schema before it reaches
/// the existing Sentry event constructor.
#[tauri::command]
pub fn record_camera_receive_health(cadence: String, decoder_render: String) -> bool {
    let Some(event) = camera_receive_health_diagnostic(&cadence, &decoder_render) else {
        return false;
    };
    capture_sentry_diagnostic(event)
}

fn camera_receive_health_diagnostic(
    cadence: &str,
    decoder_render: &str,
) -> Option<SentryDiagnosticEvent> {
    let capture_cadence = match cadence {
        "reduced" => CadenceBucket::Reduced,
        "severe" => CadenceBucket::Severe,
        "stalled" => CadenceBucket::Stalled,
        _ => return None,
    };
    let decoder_render = match decoder_render {
        "decoder_degraded" => DecoderRenderHealth::DecoderDegraded,
        _ => return None,
    };
    Some(SentryDiagnosticEvent::CameraHealth(CameraHealthDiagnostic {
        role: DiagnosticRole::Receiver,
        direction: CameraDirection::Receive,
        capture_cadence,
        encode_cadence: CadenceBucket::NotApplicable,
        queue_backpressure: QueueBackpressureBucket::NotApplicable,
        decoder_render,
    }))
}

/// Resolve the platform log DIRECTORY, creating it if needed:
///
/// - macOS: `~/Library/Logs/Petal/`
/// - Windows: `%APPDATA%\Petal\logs\`
///
/// Falls back to the system temp directory if the platform base directory
/// cannot be resolved or created. This runs before the logger exists, so
/// `stderr` is the only available way to report that fallback.
fn resolve_log_dir() -> PathBuf {
    let dir = preferred_log_dir().unwrap_or_else(|| {
        eprintln!(
            "logging: could not resolve the platform log directory, falling back to a temp dir for petal.log"
        );
        std::env::temp_dir()
    });
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "logging: failed to create log directory {} ({e}), falling back to temp dir",
            dir.display()
        );
        return std::env::temp_dir();
    }
    dir
}

/// Today's (UTC) active log file: `<log_dir>/petal.log.<YYYY-MM-DD>` (#905
/// -- replaces the old fixed `petal.log`, now rolled mid-session by
/// `DailyLogSink` at the UTC date boundary with no restart needed). UTC, not
/// local time, to match `chrono_like_timestamp()`'s in-line timestamps
/// (also UTC by construction) -- local time here would make the filename's
/// date disagree with the timestamps inside the file.
fn resolve_log_path() -> PathBuf {
    resolve_log_dir().join(daily_log_file_name(&today_utc_string()))
}

/// Today's date in UTC, `YYYY-MM-DD` -- the single source of truth for
/// "which day's file is active" used by the live sink, the startup
/// gzip/prune sweep, and export's date-range filter alike.
fn today_utc_string() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// The per-day log file name for a given `YYYY-MM-DD` date string, e.g.
/// `petal.log.2026-09-02`.
fn daily_log_file_name(date: &str) -> String {
    format!("petal.log.{date}")
}

/// Extracts the `YYYY-MM-DD` date from a per-day log filename shape
/// (`petal.log.<date>` or its gzip'd `petal.log.<date>.gz`), or `None` for
/// anything else -- a legacy `petal.log`/`petal-*.log` file, or (the point
/// of the EXACT match below) a stray temp file.
///
/// #905 review (Finding 1, CRITICAL, flagged independently by two
/// reviewers): an earlier version of this function only checked the FIRST
/// 10 characters after the prefix, so a partial/orphaned compression temp
/// file (`petal.log.<date>.gz.tmp-<pid>`, left behind if the process was
/// killed mid-compress, or read while a background gzip is still writing
/// it) matched as an ordinary daily log. Because that name
/// does NOT end in `.gz`, the export path would then read the partially-
/// written/compressed bytes AS PLAINTEXT and feed them straight into
/// `redact_for_export` -- the exact privacy-boundary bypass this module
/// exists to prevent, just reintroduced through a temp file. This module
/// never legitimately produces any OTHER suffix on a `petal.log.<date>`
/// name (the same-day size backstop rotates to a distinct LEGACY-shaped
/// `petal-<timestamp>.log` instead, see `SAME_DAY_SIZE_BACKSTOP_BYTES`),
/// so accepting only an EXACT `<date>` or `<date>.gz` remainder -- not a
/// prefix match -- is not just tighter, it's the true grammar.
fn daily_log_date_from_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("petal.log.")?;
    let date = rest.strip_suffix(".gz").unwrap_or(rest);
    if date.len() != 10 {
        return None;
    }
    let bytes = date.as_bytes();
    let valid = bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    valid.then_some(date)
}

/// Whether `name` is any per-day log file (#905 shape): the active
/// plaintext file or a gzip'd completed day, i.e. exactly `petal.log.<date>`
/// or `petal.log.<date>.gz` (see `daily_log_date_from_name`'s doc comment
/// for why the match must be exact). A same-day size-backstop overflow
/// chunk is NOT this shape -- it reuses the legacy `petal-<timestamp>.log`
/// name instead (`SAME_DAY_SIZE_BACKSTOP_BYTES`). Deliberately excludes the
/// legacy bare `petal.log`/`petal.log.gz` too, which have no date at all.
fn is_daily_log_name(name: &str) -> bool {
    daily_log_date_from_name(name).is_some()
}

/// Whether `name` is a pre-#905 file this module must keep recognizing on
/// an upgraded install: the old single active `petal.log`, one of its
/// rotated `petal-<compact-timestamp>.log` siblings (also reused as the
/// same-day size-backstop's overflow chunk name, see
/// `SAME_DAY_SIZE_BACKSTOP_BYTES`), or either already gzip'd.
fn is_legacy_log_name(name: &str) -> bool {
    name == "petal.log"
        || name == "petal.log.gz"
        || (name.starts_with("petal-") && (name.ends_with(".log") || name.ends_with(".log.gz")))
}

/// Whether `name` is any log file this module ever creates, either naming
/// shape, compressed or not.
fn is_any_log_file_name(name: &str) -> bool {
    is_daily_log_name(name) || is_legacy_log_name(name)
}

/// The log file that best represents the PREVIOUS session's activity,
/// resolved by mtime across BOTH on-disk naming shapes AND both compressed
/// and plaintext: today's daily file if one exists, else the most recently
/// modified file of any recognized shape, else `None` on a first-ever
/// launch.
///
/// #905 review Finding 4 (both reviewers, partial disagreement -- resolved
/// in favor of the more defensive fix): an earlier version excluded `.gz`
/// files entirely, reasoning that a completed, compressed day is never the
/// "current" file for a LIVE session. That holds for the nominal case (this
/// resolver runs before this session's own gzip/prune sweep), but there is
/// a real narrow window where it doesn't: a full relaunch (single-instance
/// plugin briefly overlapping an outgoing process's teardown), a clock
/// revisit (see `GZIP_LOCK`'s doc comment), or simply an interrupted
/// previous run whose OWN sweep ran and gzip'd its predecessor's file right
/// before it died. In any of those, the newest EVIDENCE of prior activity
/// can legitimately be a `.gz`, and excluding it made both detectors below
/// return `None` (quietly going silent) instead of resolving it. Callers
/// that read content (`read_log_tail_lines`) must decompress as needed;
/// `previous_log_mtime` doesn't care either way (mtime is a filesystem
/// attribute, unaffected by compression).
///
/// This is NOT the same as `resolve_log_path()` (today's path, which may
/// not exist yet). It exists because the two previous-session detectors
/// (`report_previous_crashes` via `previous_log_mtime`,
/// `report_vanished_previous_session` via `read_log_tail_lines`) must keep
/// resolving the real most-recently-written file across a UTC date
/// boundary -- pointing them at a hardcoded `petal.log.<today>` would find
/// nothing on the first launch of a new day and silently go quiet (#905
/// trap; this exact "detector pointed at a file that stopped existing"
/// failure mode already burned this module once, see #866).
fn resolve_current_or_latest_log_file(log_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(log_dir).ok()?;
    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(is_any_log_file_name)
                .unwrap_or(false)
        })
        .filter_map(|p| {
            std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .map(|m| (p, m))
        })
        .collect();
    candidates.sort_by_key(|(_, mtime)| *mtime);
    candidates.pop().map(|(p, _)| p)
}

#[cfg(windows)]
fn preferred_log_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(windows_log_dir_from_appdata)
}

#[cfg(windows)]
fn windows_log_dir_from_appdata(appdata: PathBuf) -> PathBuf {
    appdata.join("Petal").join("logs")
}

/// Deliberately avoid pulling in the `dirs` crate for one lookup. `$HOME` is
/// available in real macOS user sessions, including GUI-launched apps.
#[cfg(not(windows))]
fn preferred_log_dir() -> Option<PathBuf> {
    dirs_home().map(|home| home.join("Library").join("Logs").join("Petal"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Specific known-noisy third-party crates, denylisted to `warn` by default
/// (see `resolve_log_filter`'s doc comment for why this must be a per-target
/// DENYLIST rather than a global cap with a single carve-out for this
/// crate).
const NOISY_THIRD_PARTY_CRATES: &[&str] = &[
    "webrtc_sys",
    "livekit",
    "livekit_api",
    "livekit_protocol",
    "livekit_runtime",
    "libwebrtc",
    "wry",
    "tao",
    "tungstenite",
    "tokio_tungstenite",
    "hyper",
    "hyper_util",
    "reqwest",
    "rustls",
];

/// Narrow carve-outs from `NOISY_THIRD_PARTY_CRATES`: third-party log targets
/// that are decisive for diagnosing a user-reported incident and cost only a
/// handful of lines per SESSION, not per frame (#788).
///
/// `livekit::platform_audio` is the vendored SDK's whole platform audio-device
/// module. At `info` it emits exactly the ADM lifecycle: acquire, enable
/// recording, enable playout, the recording/playout device counts, a
/// start/stop-recording pair, the audio-processing config, and the release on
/// drop -- nothing per frame (verified against
/// `apps/desktop/vendor/livekit/src/platform_audio/mod.rs`; pinned by
/// `decisive_third_party_targets_are_once_per_session_in_the_vendored_sdk`).
///
/// Those are the lines that say whether remote audio has a real playout device
/// at all. Without them a silent meeting (#787) looks byte-for-byte identical
/// in `petal.log` to a healthy one, and the only way to get them was asking the
/// user to relaunch with `RUST_LOG=info,livekit=info` mid-incident.
const DECISIVE_THIRD_PARTY_TARGETS: &[&str] = &["livekit::platform_audio"];

/// The result of resolving a `RUST_LOG` value into a real filter: the
/// `env_filter::Filter` that gates every log record, the ceiling passed to
/// `log::set_max_level`, and -- the point of #595 -- a warning when the raw
/// value didn't parse instead of a silent substitution.
struct ResolvedLogFilter {
    filter: env_filter::Filter,
    max_level: log::LevelFilter,
    /// `Some` only when `raw_rust_log` was present and failed to parse.
    /// Names the exact value received and the level actually applied
    /// (always `info`, the same default used when `RUST_LOG` is unset) --
    /// never left implicit, per #595's whole point: a value that silently
    /// fell back to `info` is indistinguishable from one that was never set.
    parse_warning: Option<String>,
}

/// Scans a `RUST_LOG`-style directive spec for a bare, unqualified level
/// token (e.g. `info` in `"info,desktop::remote_control=debug"`) -- the part
/// of `env_filter`'s directive grammar that sets the fallback level for any
/// target with no more specific directive. Mirrors (without depending on)
/// `env_filter`'s own parser: a comma-separated token with no `=` that
/// parses as a level becomes the global fallback, and the LAST such token
/// wins if more than one appears -- matching `env_filter::Builder`'s
/// directive-replacement semantics (`Builder::insert_directive`).
///
/// Used ONLY to decide whether `resolve_log_filter`'s noisy-third-party-
/// crate denylist below is warranted, deliberately independent of whether
/// the full spec ultimately parses cleanly: a spec with one malformed
/// per-module clause alongside a good bare level should still get the
/// denylist its bare level implies, since the fallback below re-applies that
/// same bare level anyway.
fn bare_global_level(spec: &str) -> Option<log::LevelFilter> {
    let mods = spec.split('/').next().unwrap_or(spec);
    let mut level = None;
    for token in mods.split(',') {
        let token = token.trim();
        if token.is_empty() || token.contains('=') {
            continue;
        }
        if let Ok(parsed) = token.parse::<log::LevelFilter>() {
            level = Some(parsed);
        }
    }
    level
}

/// Whether `spec` already carries a per-target directive that would apply to
/// `target`, i.e. one whose name is a prefix of it (`env_filter` matches
/// targets by `starts_with`, so `livekit=debug` applies to
/// `livekit::platform_audio`).
///
/// Used ONLY to decide whether `resolve_log_filter`'s
/// `DECISIVE_THIRD_PARTY_TARGETS` carve-out should be inserted at all. It must
/// not be, when the user has said something about that target themselves:
/// `env_filter` resolves a record against the LONGEST matching directive name
/// regardless of insertion order, so an unconditional
/// `livekit::platform_audio=info` entry would silently outrank -- rather than be
/// replaced by -- an explicit `RUST_LOG=livekit=trace`, breaking the
/// "your own directive for a denylisted crate wins" contract that the denylist
/// below already promises.
///
/// Mirrors (without depending on) `env_filter::Builder::parse`'s directive
/// grammar, same as `bare_global_level` above: split off any `/regex` tail,
/// then per comma-separated clause take the part before `=` as the target name,
/// skipping a bare token that parses as a level (that is the global fallback
/// directive, not a per-target one).
fn spec_has_directive_covering(spec: &str, target: &str) -> bool {
    let mods = spec.split('/').next().unwrap_or(spec);
    mods.split(',').any(|clause| {
        let clause = clause.trim();
        if clause.is_empty() {
            return false;
        }
        let name = clause.split('=').next().unwrap_or(clause).trim();
        if name.is_empty() {
            return false;
        }
        // `info` / `debug` / ... with no `=` is the global fallback level.
        if !clause.contains('=') && name.parse::<log::LevelFilter>().is_ok() {
            return false;
        }
        target.starts_with(name)
    })
}

/// Builds the real `env_filter::Filter` this app logs through, from an
/// optional raw `RUST_LOG` value (`None` when the env var is unset).
///
/// Accepts the standard `env_logger`/`RUST_LOG` comma-separated per-module
/// directive grammar (e.g. `info,desktop::remote_control=debug`), not just a
/// bare level word -- fixing #595, where a per-module spec failed
/// `str::parse::<log::LevelFilter>()` and silently fell back to `info` with
/// no warning at all (the exact failure class that burned #559/#561's three
/// P0 cycles on a phantom hang: a diagnostic launch's `RUST_LOG=warn` had
/// hidden the INFO markers a search was looking for). A value that fails to
/// parse here is reported via `parse_warning`, never silently substituted.
///
/// Kept as a pure function of its input (no env/fs/logger side effects) so
/// it's directly unit-testable -- `init()` itself can't be, since
/// `log::set_boxed_logger` only succeeds once per process.
///
/// IMPORTANT, learned the hard way during earlier work on this function (see
/// CLAUDE.md's logging section): the default ceiling must be applied as a
/// per-target DENYLIST of specific known-noisy third-party crates, NOT a
/// global cap with a single carve-out for `desktop_lib`. An earlier version
/// did `.level(Warn)` globally + `.level_for("desktop_lib", info)`, which
/// looks right but silently swallowed `log::info!` from ANY other crate in
/// the same process -- including this crate's own `examples/*.rs` probes
/// (each is a separate crate target, e.g. `join_log_probe`, not
/// `desktop_lib`) and would equally have swallowed info-level logs from
/// `main.rs` if it ever logged directly. Caught via a real probe run whose
/// "session: join_room(...) begin" line never appeared in the file.
/// Denylisting specific noisy third-party crates (rather than allowlisting
/// our own) means logging from ANY of this app's own code -- lib, examples,
/// or future binaries -- gets the real requested level by default, and only
/// genuinely verbose dependencies are turned down. The denylist is only
/// applied when the effective global level would otherwise be noisier than
/// `warn` -- if a user explicitly requests `warn` (or `error`) globally,
/// there's nothing to turn down further, and third-party crates keep
/// exactly the requested level like everything else.
///
/// The denylist has exactly one kind of hole in it, and it is deliberate:
/// `DECISIVE_THIRD_PARTY_TARGETS` names sub-targets whose `info` output is
/// once-per-session and diagnostically decisive, carved back out at `info`
/// (#788). Add to that list only after checking the target's real call sites
/// are not per-frame -- the denylist is what keeps `petal.log` readable.
fn resolve_log_filter(raw_rust_log: Option<&str>) -> ResolvedLogFilter {
    let effective_global_level = raw_rust_log
        .and_then(bare_global_level)
        .unwrap_or(log::LevelFilter::Info);

    let mut builder = env_filter::Builder::new();
    // Default when `RUST_LOG` is unset (or fails to parse below): `info` for
    // everything, matching this module's documented default.
    builder.filter_level(log::LevelFilter::Info);
    if effective_global_level > log::LevelFilter::Warn {
        for crate_name in NOISY_THIRD_PARTY_CRATES {
            builder.filter_module(crate_name, log::LevelFilter::Warn);
        }
        // Carve the decisive once-per-session targets back OUT of the denylist
        // (#788). `env_filter` resolves each record against the longest
        // directive name that prefixes its target, so this longer, more
        // specific entry beats the crate-root `livekit=warn` above no matter
        // what order they were inserted in.
        //
        // Deliberately pinned at `info`, not at `effective_global_level`: the
        // point is to restore the ADM lifecycle lines to a DEFAULT launch, not
        // to re-admit SDK internals under `RUST_LOG=debug`/`trace`. A developer
        // who does want more says so explicitly, and
        // `spec_has_directive_covering` then stands this carve-out down so
        // their directive is the one that applies.
        for target in DECISIVE_THIRD_PARTY_TARGETS {
            let user_owns_it =
                raw_rust_log.is_some_and(|raw| spec_has_directive_covering(raw, target));
            if !user_owns_it {
                builder.filter_module(target, log::LevelFilter::Info);
            }
        }
    }

    // Layered AFTER the denylist above, so a user's own explicit directive
    // for one of these crates (e.g. `RUST_LOG=info,livekit=debug`) replaces
    // our default rather than being clobbered by it -- `env_filter::Builder`
    // replaces same-named directives in place (`insert_directive`), so
    // whichever call inserts a name LAST wins.
    let mut parse_warning = None;
    if let Some(raw) = raw_rust_log {
        if let Err(err) = builder.try_parse(raw) {
            // `try_parse` leaves the builder untouched on error (confirmed
            // via `env_filter::Builder::try_parse`'s source: it early-returns
            // before mutating `self` on any parse error), so the pre-seeded
            // `info` default (+ denylist) above is exactly what applies --
            // the same defaults as if `RUST_LOG` had never been set. Report
            // that plainly instead of leaving it to be inferred.
            parse_warning = Some(format!(
                "RUST_LOG={raw:?} failed to parse ({err}) -- ignoring it and using the default level (info) instead"
            ));
        }
    }

    let filter = builder.build();
    let max_level = filter.filter();
    ResolvedLogFilter {
        filter,
        max_level,
        parse_warning,
    }
}

/// How long a single native-WebRTC log message may keep repeating verbatim
/// before `RepeatSuppressingLog` re-emits it (as a rolled-up summary,
/// counting how many times it fired in that window) rather than staying
/// silent forever. Bounds volume to at most one line per target per this
/// interval instead of one line per occurrence.
const NATIVE_WEBRTC_REPEAT_SUMMARY_INTERVAL: Duration = Duration::from_secs(30);

/// Identifies a repeating streak: target AND level AND message text must
/// all match for two records to count as "the same repeat" (#905 review
/// finding: keying on message text alone would conflate identical text
/// from two different targets or levels into one streak, and misattribute
/// the eventual rollup to whichever record happened to trigger it).
#[derive(Clone, PartialEq, Eq)]
struct NativeWebrtcRepeatKey {
    target: String,
    level: log::Level,
    message: String,
}

struct NativeWebrtcRepeatState {
    key: NativeWebrtcRepeatKey,
    repeat_count: u64,
    last_emitted: Instant,
}

/// Collapses IDENTICAL, CONSECUTIVE native-WebRTC log lines (#905): the
/// `RTCVideoEncoderH264.mm:614` frame-rate warning alone was 610,617 lines /
/// 34.5% of a real 263 MB field log, always the exact same text repeated
/// per-frame. The existing `NOISY_THIRD_PARTY_CRATES` denylist (this
/// module's `resolve_log_filter`) already pins this target at `warn`, and
/// this line IS a warn -- a level filter cannot help here, only
/// repeat-suppression can.
///
/// Wraps the OUTERMOST logger (before `SentryLogger`, before fern's own
/// `Dispatch`) rather than living inside `DailyLogSink`, for two reasons:
/// it must see the RAW pre-format record (fern's `.format()` stamps a
/// per-line timestamp, so comparing already-formatted lines would never
/// match twice), and stdout should benefit from the suppression too, not
/// just the file.
///
/// On a repeat: suppressed entirely, neither forwarded nor formatted,
/// unless `NATIVE_WEBRTC_REPEAT_SUMMARY_INTERVAL` has elapsed since the
/// last emitted line for this target, in which case ONE summary line
/// (carrying the repeat count) is emitted and the window resets. On a
/// change of message (or the very first occurrence): the new message is
/// always forwarded immediately, and if the PRIOR message had any
/// suppressed repeats not yet reported, a rollup for it is emitted first so
/// a streak's tail is never silently dropped.
struct RepeatSuppressingLog<L: log::Log> {
    inner: L,
    state: Mutex<Option<NativeWebrtcRepeatState>>,
}

impl<L: log::Log> RepeatSuppressingLog<L> {
    fn new(inner: L) -> Self {
        RepeatSuppressingLog {
            inner,
            state: Mutex::new(None),
        }
    }

    /// Re-emits `message` at `target`/`level` through `self.inner` -- used
    /// only for the synthetic "repeated Nx" summary lines. Takes target and
    /// level directly (not a borrowed `&log::Record`) because a rollup can
    /// be emitted well after the record that started the streak is gone
    /// (e.g. at `flush()`, or for the PRIOR streak when the message just
    /// changed) -- there is no live record to borrow location metadata
    /// from at that point, so this only carries target/level, not
    /// module_path/file/line (unused by this module's line format anyway).
    fn emit(&self, target: &str, level: log::Level, message: &str) {
        // Must stay ONE statement: the `Record` built below borrows the
        // `format_args!` temporary, which only lives to the end of the
        // statement that creates it -- splitting this into a `let` binding
        // used on a later line would drop that temporary out from under it.
        self.inner.log(
            &log::Record::builder()
                .args(format_args!("{message}"))
                .level(level)
                .target(target)
                .build(),
        );
    }
}

impl<L: log::Log> log::Log for RepeatSuppressingLog<L> {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        let target = record.target();
        let is_native_webrtc =
            target == NATIVE_WEBRTC_LOG_TARGET || target.starts_with(NATIVE_WEBRTC_LOG_TARGET_PREFIX);
        if !is_native_webrtc {
            self.inner.log(record);
            return;
        }
        let key = NativeWebrtcRepeatKey {
            target: target.to_string(),
            level: record.level(),
            message: record.args().to_string(),
        };
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(state) if state.key == key => {
                state.repeat_count += 1;
                if state.last_emitted.elapsed() >= NATIVE_WEBRTC_REPEAT_SUMMARY_INTERVAL {
                    let count = state.repeat_count;
                    state.repeat_count = 0;
                    state.last_emitted = Instant::now();
                    let NativeWebrtcRepeatKey { target, level, message } = key;
                    drop(guard);
                    self.emit(
                        &target,
                        level,
                        &format!(
                            "{message} [repeated {count}x in the last {}s, #905]",
                            NATIVE_WEBRTC_REPEAT_SUMMARY_INTERVAL.as_secs()
                        ),
                    );
                }
                // Otherwise: suppressed. Neither forwarded nor formatted.
            }
            Some(state) => {
                // The message just changed (or target/level did) -- flush a
                // rollup of the PRIOR key's suppressed repeats (if any),
                // using the PRIOR key's own target/level, not the new
                // record's, so the rollup isn't misattributed.
                let prior = (state.repeat_count > 0)
                    .then(|| (state.key.clone(), state.repeat_count));
                state.key = key;
                state.repeat_count = 0;
                state.last_emitted = Instant::now();
                drop(guard);
                if let Some((prior_key, prior_count)) = prior {
                    self.emit(
                        &prior_key.target,
                        prior_key.level,
                        &format!(
                            "{} [repeated {prior_count}x more before changing]",
                            prior_key.message
                        ),
                    );
                }
                self.inner.log(record);
            }
            None => {
                *guard = Some(NativeWebrtcRepeatState {
                    key,
                    repeat_count: 0,
                    last_emitted: Instant::now(),
                });
                drop(guard);
                self.inner.log(record);
            }
        }
    }

    fn flush(&self) {
        // #905 review (Finding 6): a streak still accumulating suppressed
        // repeats when the process shuts down would otherwise lose that
        // count entirely -- nothing else would ever trigger its rollup.
        // Emits (and resets the counter, not the whole tracked key) rather
        // than `take()`-ing the state outright, so this stays correct even
        // if `flush()` is called more than once.
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = guard.as_mut() {
            if state.repeat_count > 0 {
                let key = state.key.clone();
                let count = state.repeat_count;
                state.repeat_count = 0;
                state.last_emitted = Instant::now();
                drop(guard);
                self.emit(
                    &key.target,
                    key.level,
                    &format!("{} [repeated {count}x more, flushed]", key.message),
                );
                self.inner.flush();
                return;
            }
        }
        self.inner.flush();
    }
}

/// Install the file-based logging sink + panic hook. Must be called exactly
/// once, as the very first thing in `run()`, before any other startup code
/// (including `dotenvy::from_path`) so that even a failure in that early
/// code is captured. Returns the resolved log file path (also logged at
/// `info` level once the sink itself is live) so callers can report it.
///
/// Safe to call in any build config: honors `RUST_LOG` if set (matches
/// `env_logger`'s existing convention, so `RUST_LOG=debug npx tauri dev`
/// keeps working exactly as before), otherwise defaults to `info` for every
/// module in this crate and `warn` for third-party dependencies (so a noisy
/// dependency like `livekit`/`webrtc-sys` at `debug`/`trace` doesn't flood
/// the file with library-internal chatter by default).
pub fn init() -> PathBuf {
    // MUST be the first statement (#281 plan point 3) -- even a failure a
    // few lines below (e.g. `resolve_log_dir()`'s $HOME fallback) is then
    // covered. `init_sentry()` is a clean no-op when no DSN is compiled in,
    // which is every `cargo build`/`cargo test`/`tauri dev` run by default.
    init_sentry();

    let log_dir = resolve_log_dir();

    // Issue #13 (startup crash detection) / #878 (vanished-session
    // detection), updated for #905's per-day files: resolve whatever
    // plaintext file the PREVIOUS session actually wrote to -- today's
    // daily file if this is a same-day restart, yesterday's if this is the
    // first launch on a new UTC day, or a legacy `petal.log` on an install
    // that hasn't rolled once yet -- BEFORE the gzip/prune sweep below
    // touches anything. A hardcoded `petal.log.<today>` would find nothing
    // at every UTC midnight and silently make both detectors go quiet
    // (#905 trap; see `resolve_current_or_latest_log_file`'s doc comment).
    let previous_log_path = resolve_current_or_latest_log_file(&log_dir);

    // Capture the PREVIOUS run's log file mtime BEFORE the sweep below can
    // touch it -- the cheapest available proxy for "when the previous
    // session last logged anything," used after the sink is live to flag
    // DiagnosticReports crash files newer than that (i.e. crashes that
    // happened after the previous session went quiet). If there is no
    // previous log at all, fall back to the last 24h.
    let previous_log_mtime = previous_log_path
        .as_deref()
        .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60)
        });

    // Must read BEFORE the gzip/prune sweep below: this is the previous
    // session's own content, still sitting there uncompressed at this point
    // (#878 vanished-session detector).
    let previous_log_tail = previous_log_path
        .as_deref()
        .map(|p| read_log_tail_lines(p, VANISHED_SESSION_TAIL_LINES))
        .unwrap_or_default();

    // #905: gzip every completed day's file (today's is left alone so
    // `tail -f` keeps working) -- including legacy `petal.log`/`petal-*.log`
    // files an upgraded install still has lying around -- then delete
    // anything past `MAX_LOG_AGE_DAYS`. Replaces the old startup-only
    // size-based rotation, which only ever bounded the PREVIOUS run's
    // single file and never revisited the current one -- the root cause
    // of a real 263 MB, 6-day, single-launch log.
    roll_and_prune_daily_logs(&log_dir);

    let log_path = log_dir.join(daily_log_file_name(&today_utc_string()));

    let ResolvedLogFilter {
        filter,
        max_level,
        parse_warning,
    } = resolve_log_filter(std::env::var("RUST_LOG").ok().as_deref());
    if let Some(warning) = parse_warning.as_deref() {
        // The logger isn't installed yet, so `eprintln!` is the only channel
        // available this early -- re-emitted via `log::warn!` below once the
        // sink IS live so it also lands in petal.log, not just a terminal
        // that may not exist at all for a GUI launch (see this module's top
        // doc comment). Never silently substitute (#595).
        eprintln!("logging: {warning}");
    }

    // Eager open, purely to report a clean, immediate warning on failure
    // (matches the pre-#905 behavior) -- the actual writing goes through
    // `DailyLogSink` below, which reopens the same path itself. That's a
    // second (cheap, append-mode) open of the same file, not a conflict.
    let file_open_check = OpenOptions::new().create(true).append(true).open(&log_path);

    let mut dispatch = fern::Dispatch::new()
        .format(|out, message, record| {
            let message = redact_room_credentials(&message.to_string());
            out.finish(format_args!(
                "{} [{}] [{}] {}",
                chrono_like_timestamp(),
                record.level(),
                record.target(),
                message
            ))
        })
        // fern's own base level is left at the same ceiling as the real
        // filter (`max_level`, `env_filter::Filter::filter()`'s max across
        // every directive) so it never filters out anything the closure
        // below would otherwise allow. ALL real filtering -- the global
        // default, per-module `RUST_LOG` directives, and the noisy-third-
        // party-crate denylist -- happens in `.filter(...)`'s
        // `env_filter::Filter`, kept as the single source of truth instead
        // of two competing per-target systems (see `resolve_log_filter`'s
        // doc comment for why that split bit us before).
        .level(max_level)
        .filter(move |metadata| filter.enabled(metadata))
        // Always echo to stdout too -- still useful for `cargo run`/`tauri dev`
        // where a terminal IS attached; this is additive, not a replacement.
        .chain(std::io::stdout());

    match file_open_check {
        Ok(f) => {
            drop(f);
            dispatch =
                dispatch.chain(Box::new(DailyLogSink::new(log_dir.clone())) as Box<dyn log::Log>);
        }
        Err(e) => {
            eprintln!(
                "logging: failed to open log file {} ({e}) -- file logging DISABLED this run, stdout only",
                log_path.display()
            );
        }
    }

    // Chain `sentry-log`'s `SentryLogger` AROUND the fern dispatch rather
    // than calling `dispatch.apply()` directly (#281 point 7): `SentryLogger`
    // wraps a destination `log::Log` and forwards every record to it after
    // its own capture logic runs (see `sentry_log::logger::SentryLogger::
    // log()`), so fern remains the actual file/stdout sink underneath --
    // this is "chained alongside the existing file sink", not a second,
    // competing logger. A no-op (network-inert) when Sentry was never
    // initialized: `sentry_core::add_breadcrumb`/`capture_event` degrade to
    // no-ops with no active client.
    // Ignore fern's own computed ceiling here -- it just reflects the
    // `.level(max_level)` call above, which was already seeded from the
    // real `env_filter::Filter`'s own max (see `resolve_log_filter`); reuse
    // that binding directly below rather than re-deriving it from fern.
    let (_fern_level, fern_logger) = dispatch.into_log();
    // #905: collapse identical consecutive native-WebRTC lines BEFORE they
    // reach fern's format+fanout at all. Wrapped here (outside the whole
    // dispatch) rather than inside `DailyLogSink` so it sees the RAW
    // pre-format record (comparing formatted lines would never match twice,
    // since every line carries its own timestamp) and so stdout benefits
    // too, not just the file. See `RepeatSuppressingLog`'s doc comment.
    let fern_logger: Box<dyn log::Log> = Box::new(RepeatSuppressingLog::new(fern_logger));
    // `.filter(...)` overrides ONLY the two hook-internal targets (see their
    // constants' doc comments) to `Ignore` -- every other record keeps
    // `sentry_log`'s own `default_filter` (Error -> event, Warn/Info ->
    // breadcrumb, Debug/Trace -> ignored), unchanged from the plain
    // `with_dest` default. This does not affect what fern writes to
    // petal.log/stdout, only what the Sentry bridge does with these two
    // specific lines.
    let logger = sentry_log::SentryLogger::with_dest(fern_logger).filter(|metadata| {
        if metadata.target() == PANIC_HOOK_LOG_TARGET
            || metadata.target() == OBJC_EXCEPTION_HOOK_LOG_TARGET
        {
            sentry_log::LogFilter::Ignore
        } else if metadata.target() == NATIVE_WEBRTC_LOG_TARGET
            || metadata.target().starts_with(NATIVE_WEBRTC_LOG_TARGET_PREFIX)
        {
            // #787 mapped native WebRTC severities so its lines reach the log
            // at all -- but SentryLogger's capture branch runs on every record
            // `log::set_max_level` admits, BEFORE the fern denylist that caps
            // this target at `warn`. So each `RTC_LOG(LS_ERROR)` would open its
            // own Sentry issue, unrate-limited, and libwebrtc emits LS_ERROR
            // for routine non-fatal conditions (RTP demux, SRTP unprotect, ICE)
            // whose text carries ssrc/port numbers, so grouping fragments.
            //
            // The same day, transport/audio.rs downgraded its own watchdog from
            // error! to warn! with the note "error! would open a Sentry issue
            // per track" -- this channel has a far higher and, by construction,
            // entirely unmeasured line rate. Breadcrumb only: the lines stay in
            // petal.log and ride along on a real error, without becoming one.
            sentry_log::LogFilter::Breadcrumb
        } else {
            sentry_log::default_filter(metadata)
        }
    });
    if let Err(e) = log::set_boxed_logger(Box::new(logger)) {
        // `set_boxed_logger` only fails if a logger is already installed
        // (double-init) -- log it via stderr since the logger itself isn't
        // usable at this point, and this should never happen in normal
        // operation (this fn is called exactly once from `run()`).
        eprintln!("logging: log::set_boxed_logger() failed (logger already installed?): {e}");
    }
    log::set_max_level(max_level);

    install_panic_hook();
    #[cfg(target_os = "macos")]
    objc_exception::install();

    log::info!("logging: file sink initialized at {}", log_path.display());
    if let Some(warning) = parse_warning {
        // Re-emitted here (see the `eprintln!` above) now that the sink is
        // live, so it's captured in petal.log itself, not just a terminal
        // that may not exist for a GUI launch (#595).
        log::warn!("logging: {warning}");
    }

    let crash_report_found = report_previous_crashes(previous_log_mtime);
    report_vanished_previous_session(&previous_log_tail, crash_report_found);
    log_startup_hardware();

    log_path
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontendLogLevel {
    Info,
    Warn,
    Error,
}

/// Frontend-to-Rust log bridge for updater diagnostics (#43).
///
/// WKWebView console output is invisible in a normal `.app` launch, so the
/// updater calls this command for every meaningful step. Keep the command
/// narrowly named for now: it makes `petal.log` searchable without opening a
/// general-purpose frontend logging surface.
#[tauri::command]
pub fn log_updater_event(level: FrontendLogLevel, message: String) {
    let message = message.replace(['\r', '\n'], " ");
    match level {
        FrontendLogLevel::Info => log::info!("updater: {message}"),
        FrontendLogLevel::Warn => log::warn!("updater: {message}"),
        FrontendLogLevel::Error => log::error!("updater: {message}"),
    }
}

fn log_startup_hardware() {
    let arch = std::env::consts::ARCH;
    let hw_model =
        command_stdout("sysctl", &["-n", "hw.model"]).unwrap_or_else(|| "unknown".into());
    let macos = command_stdout("sw_vers", &["-productVersion"]).unwrap_or_else(|| "unknown".into());
    log::info!("startup: hardware model={hw_model} arch={arch} macOS={macos}");
    // #884: AGX GPU restart counter (IOAccelerator PerformanceStatistics
    // recoveryCount). A value that grows across sessions means the GPU has
    // been silently restarting -- the #878 H6 discriminator, readable
    // unprivileged. "unknown" is honest absence, never a fabricated 0.
    let gpu_restarts = command_stdout("/usr/sbin/ioreg", &["-r", "-c", "IOAccelerator", "-d", "1"])
        .and_then(|out| gpu_recovery_count_from_ioreg(&out))
        .map(|count| count.to_string())
        .unwrap_or_else(|| "unknown".into());
    log::info!("startup: gpu recoveryCount={gpu_restarts}");
}

/// Sum every accelerator's `"recoveryCount"=N` out of `ioreg -r -c
/// IOAccelerator -d 1` output (#884). Pure and unit-tested in both
/// directions -- a parser drift here must read as "unknown", never 0.
fn gpu_recovery_count_from_ioreg(ioreg_output: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut seen = false;
    for chunk in ioreg_output.split("\"recoveryCount\"=").skip(1) {
        let digits: String = chunk.chars().take_while(|c| c.is_ascii_digit()).collect();
        total += digits.parse::<u64>().ok()?;
        seen = true;
    }
    seen.then_some(total)
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Coarse OS version for the allowlisted `os_version` tag. Resolved once and
/// cached: `capture_sentry_diagnostic_on_current_hub` clears the Sentry scope
/// (to keep ambient attachments off diagnostics), which also wipes the
/// scope-level tag -- so the real value has to ride on the event itself (#788).
fn os_version_tag() -> &'static str {
    static OS_VERSION: OnceLock<String> = OnceLock::new();
    OS_VERSION.get_or_init(resolve_os_version).as_str()
}

fn resolve_os_version() -> String {
    let raw = if cfg!(target_os = "macos") {
        command_stdout("sw_vers", &["-productVersion"])
    } else if cfg!(target_os = "windows") {
        command_stdout("cmd", &["/c", "ver"])
            .as_deref()
            .and_then(parse_windows_ver)
    } else {
        None
    };
    raw.filter(|value| value != "unknown" && valid_diagnostic_tag("os_version", value))
        .unwrap_or_else(|| "unknown".into())
}

/// `cmd /c ver` prints `Microsoft Windows [Version 10.0.26100.4652]`. Keep
/// major.minor.build so the value stays inside the tag's 16-char closed schema.
fn parse_windows_ver(line: &str) -> Option<String> {
    let start = line.find("Version ")? + "Version ".len();
    let rest = &line[start..];
    let end = rest.find(']')?;
    let mut parts = rest[..end].split('.');
    let (major, minor, build) = (parts.next()?, parts.next()?, parts.next()?);
    Some(format!("{major}.{minor}.{build}"))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedLogs {
    pub archive_path: String,
    pub file_count: usize,
    pub revealed: bool,
}

/// Tauri command backing Settings -> Export logs (issue #121, date range
/// #905). It writes a local-only zip containing redacted text copies of the
/// selected log files (any mix of the current `petal.log.<date>`, gzip'd
/// completed days, or legacy `petal.log`/`petal-*.log` files), then
/// best-effort reveals the archive in the OS file manager (Finder on macOS,
/// Explorer on Windows).
///
/// `days` selects the date range: omitted (`None`) defaults to
/// `DEFAULT_EXPORT_LOG_DAYS` (the current day plus the previous one);
/// `Some(0)` means "all logs, no filtering"; `Some(n)` for `n >= 1` means
/// the current day plus the previous `n - 1`.
#[tauri::command]
pub async fn export_logs(days: Option<u32>) -> Result<ExportedLogs, String> {
    tokio::task::spawn_blocking(move || {
        let effective_days: Option<i64> = match days {
            None => Some(DEFAULT_EXPORT_LOG_DAYS),
            Some(0) => None,
            Some(n) => Some(n as i64),
        };
        let log_dir = resolve_log_dir();
        let archive_path = export_logs_archive(&log_dir, effective_days)?;
        let revealed = reveal_in_file_manager(&archive_path);
        let file_count = collect_log_files(&log_dir, effective_days).len();
        log::info!(
            "logging: exported {file_count} log file(s) to {}{}",
            archive_path.display(),
            if revealed {
                " and revealed it"
            } else {
                " (reveal unavailable)"
            }
        );
        Ok(ExportedLogs {
            archive_path: archive_path.display().to_string(),
            file_count,
            revealed,
        })
    })
    .await
    .map_err(|e| format!("log export task failed: {e}"))?
}

/// `days`: see `export_logs`'s doc comment -- `None` means no date
/// filtering (every matching file), `Some(n)` means the most recent `n`
/// days (by each file's `log_file_effective_date`).
pub fn export_logs_archive(log_dir: &Path, days: Option<i64>) -> Result<PathBuf, String> {
    let log_files = collect_log_files(log_dir, days);
    if log_files.is_empty() {
        return Err(format!("no Petal log files found in {}", log_dir.display()));
    }
    let archive_path = export_archive_path();
    write_logs_zip(&archive_path, &log_files)?;
    Ok(archive_path)
}

fn export_archive_path() -> PathBuf {
    // timestamp+pid alone collides when two exports happen within the same
    // second-resolution tick (real risk: two rapid-fire "Export logs"
    // clicks; observed in practice as flaky `cargo test --lib` runs once
    // #292 added a second concurrent caller of `export_logs_archive` --
    // two tests landing in the same tick clobbered each other's archive
    // file and read back the wrong content). A per-process monotonic
    // counter makes every call's path unique regardless of timing.
    static EXPORT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = EXPORT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "petal-logs-{}-{}-{seq}.zip",
        compact_timestamp_for_filename(),
        std::process::id()
    ))
}

/// Every log file in `log_dir` matching either on-disk naming shape
/// (#905's per-day `petal.log.<date>[.gz]`, or the legacy
/// `petal.log`/`petal-*.log[.gz]`), oldest first, optionally restricted to
/// the most recent `days` calendar days (`None` = every matching file).
///
/// Ordered by mtime, NOT by filename: raw string comparison is NOT safe
/// across the two shapes, since they diverge at the very byte that decides
/// lexicographic order (legacy `petal-` vs per-day `petal.log.`, and
/// `-` is 0x2D while `.` is 0x2E) -- a naive sort would put every legacy
/// file before every per-day file regardless of actual recency.
fn collect_log_files(log_dir: &Path, days: Option<i64>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return Vec::new();
    };
    let cutoff = days.map(|d| chrono::Utc::now().date_naive() - chrono::Duration::days(d.max(1) - 1));
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(is_any_log_file_name)
                    .unwrap_or(false)
        })
        .filter(|path| match cutoff {
            None => true,
            // A file whose date can't be determined is never silently
            // dropped from a range-restricted export -- the user might
            // specifically need it.
            Some(cutoff) => log_file_effective_date(path).is_none_or(|date| date >= cutoff),
        })
        .filter_map(|path| {
            std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .map(|mtime| (path, mtime))
        })
        .collect();
    files.sort_by(|(path_a, mtime_a), (path_b, mtime_b)| {
        mtime_a.cmp(mtime_b).then_with(|| path_a.cmp(path_b))
    });
    files.into_iter().map(|(path, _)| path).collect()
}

fn write_logs_zip(archive_path: &Path, log_files: &[PathBuf]) -> Result<(), String> {
    let entries = read_and_redact_log_files(log_files)?;
    let bytes = build_zip_bytes(LOCAL_EXPORT_README, &entries)?;
    std::fs::write(archive_path, &bytes)
        .map_err(|e| format!("could not write {}: {e}", archive_path.display()))?;
    Ok(())
}

/// Reads a single log file's content as plain bytes, transparently
/// decompressing if it's gzip'd (`.gz`, #905). This is the ONE shared
/// decompression boundary -- both `read_and_redact_log_files` (the local
/// export) and `build_feedback_attachment_zip_from` (the feedback
/// attachment) route through it (#905 review Finding 2: they previously
/// had two independent, inconsistent notions of "read a day's content" --
/// one that decompressed and one that silently didn't). Uses
/// `MultiGzDecoder`, not the single-member `GzDecoder`: `append_gz_member`
/// can concatenate more than one gzip member onto the same `.gz` file (the
/// rare clock-revisit case, see `GZIP_LOCK`'s doc comment), and only the
/// multi-member decoder decodes every member in sequence instead of
/// silently stopping after the first.
fn read_log_file_as_plaintext(path: &Path) -> std::io::Result<Vec<u8>> {
    let raw = std::fs::read(path)?;
    let is_gz = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|name| name.ends_with(".gz"))
        .unwrap_or(false);
    if !is_gz {
        return Ok(raw);
    }
    let mut decoder = flate2::read::MultiGzDecoder::new(&raw[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

/// Hard cap on the total DECOMPRESSED bytes a single "Export logs" zip-build
/// may accumulate in memory at once (#905 review Finding 8): every selected
/// `.gz` is fully decompressed and every resulting string retained at once
/// before being written to the in-memory zip buffer, with no cap unlike the
/// feedback-attachment path's own tail cap
/// (`FEEDBACK_ATTACHMENT_LOG_TAIL_BYTES`/`FEEDBACK_ATTACHMENT_MAX_ZIP_BYTES`).
/// A date range spanning a genuinely pathological, still-noisy multi-day
/// install could otherwise accumulate enough decompressed text to exhaust
/// memory. Fails closed (an `Err` asking for a shorter range) rather than
/// growing unboundedly -- generous enough (100 MiB) that it should never
/// trip for a normal install even over the full default retention window,
/// given this issue's own rate-limiting fixes.
const EXPORT_MAX_TOTAL_DECOMPRESSED_BYTES: u64 = 100 * 1024 * 1024;

/// Reads and redacts each log file into an in-memory `(entry_name,
/// redacted_text)` pair, shared by both the local "Export logs" zip and the
/// feedback-attachment zip below. This is the module's redaction policy
/// boundary (see the top-of-file doc comment) -- every file selection path
/// must route through here, never around it.
///
/// A gzip'd completed day is transparently decompressed (via
/// `read_log_file_as_plaintext`) before redaction and given back its
/// un-gzip'd entry name (e.g. `petal.log.2026-08-27.gz` on disk becomes the
/// `petal.log.2026-08-27` zip entry) -- redaction operates on TEXT, so
/// reading the compressed bytes as if they were already text would both
/// corrupt the export and let raw compressed bytes (never scanned by
/// `redact_for_export`) into an off-device zip unredacted.
fn read_and_redact_log_files(log_files: &[PathBuf]) -> Result<Vec<(String, String)>, String> {
    read_and_redact_log_files_with_cap(log_files, EXPORT_MAX_TOTAL_DECOMPRESSED_BYTES)
}

/// Real implementation behind `read_and_redact_log_files`, parameterized on
/// the total-bytes cap purely so a test can exercise Finding 8's guard
/// without actually allocating `EXPORT_MAX_TOTAL_DECOMPRESSED_BYTES` (100
/// MiB) of real data.
fn read_and_redact_log_files_with_cap(
    log_files: &[PathBuf],
    max_total_bytes: u64,
) -> Result<Vec<(String, String)>, String> {
    let mut entries = Vec::with_capacity(log_files.len());
    let mut total_bytes: u64 = 0;
    for path in log_files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("invalid log file name: {}", path.display()))?
            .to_string();
        let entry_name = name.strip_suffix(".gz").map(str::to_string).unwrap_or(name);
        let text_bytes = read_log_file_as_plaintext(path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        total_bytes = total_bytes.saturating_add(text_bytes.len() as u64);
        if total_bytes > max_total_bytes {
            return Err(format!(
                "selected logs are too large to export at once ({total_bytes} bytes > {max_total_bytes} byte cap) -- choose a shorter date range"
            ));
        }
        let redacted = redact_for_export(&String::from_utf8_lossy(&text_bytes));
        entries.push((entry_name, redacted));
    }
    Ok(entries)
}

/// Builds a zip file's raw bytes in memory: `README.txt` (fixed `readme`
/// text) followed by each `(name, text_content)` entry. The one shared
/// zip-writing core for both the local export (written straight to disk by
/// `write_logs_zip`) and the feedback-attachment path (`
/// build_feedback_attachment_zip`, kept fully in memory and returned as
/// bytes -- no temp file, no path ever handed to the frontend).
fn build_zip_bytes(readme: &[u8], entries: &[(String, String)]) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zip = zip::ZipWriter::new(cursor);
        let options = FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o600);

        zip.start_file("README.txt", options)
            .map_err(|e| format!("could not add README.txt to archive: {e}"))?;
        zip.write_all(readme)
            .map_err(|e| format!("could not write README.txt to archive: {e}"))?;

        for (name, content) in entries {
            zip.start_file(name.as_str(), options)
                .map_err(|e| format!("could not add {name} to archive: {e}"))?;
            zip.write_all(content.as_bytes())
                .map_err(|e| format!("could not write {name} to archive: {e}"))?;
        }

        zip.finish()
            .map_err(|e| format!("could not finalize zip: {e}"))?;
    }
    Ok(buf)
}

/// Builds the bounded, redacted diagnostic zip offered as an opt-in
/// UserDispatch feedback attachment (#292). Reuses the SAME
/// `redact_for_export()` pipeline the local "Export logs" button and the
/// Sentry `before_send`/`before_breadcrumb` hooks already trust for
/// off-device text (see this module's "Redaction boundary" doc comment
/// above) -- this is not a second, less-redacted export path. Differs from
/// `export_logs_archive` in three ways, all intentional for an off-device
/// attachment: (1) only the current daily file (plus the immediately
/// preceding day, see below) is included, never older rotated/gzip'd
/// files; (2) the combined content is tail-capped to
/// `FEEDBACK_ATTACHMENT_LOG_TAIL_BYTES` so a long-running session can't
/// balloon a single feedback submission; (3) the README states plainly
/// that this copy may leave the machine, unlike the local-only export's
/// wording. Returns bytes only -- no path, no temp file -- and fails
/// closed (an `Err`, never a truncated/partial payload) if the finished
/// zip would exceed `FEEDBACK_ATTACHMENT_MAX_ZIP_BYTES`.
pub(crate) fn build_feedback_attachment_zip() -> Result<Vec<u8>, String> {
    build_feedback_attachment_zip_from(&resolve_log_path())
}

/// The previous calendar day's daily log file, given today's (`primary`'s)
/// path -- or `None` if `primary` isn't in the per-day naming shape at all
/// (e.g. a legacy bare `petal.log`, which has no adjacent "yesterday" file
/// to speak of), or neither shape exists on disk.
///
/// #905 review Finding 2: an earlier version only ever constructed the
/// PLAINTEXT sibling name. In the real running app that file has almost
/// always ALREADY been gzip'd by the startup sweep (`roll_and_prune_daily_logs`
/// runs before the day is old enough to be "yesterday" from a later
/// session's point of view) by the time anyone asks for it, so this
/// silently returned a path that doesn't exist and the two-day span never
/// actually included yesterday outside of a test that hand-wrote a
/// plaintext fixture. Now resolves whichever of the plaintext OR gzip'd
/// name actually exists.
fn previous_daily_log_path(primary: &Path) -> Option<PathBuf> {
    let name = primary.file_name()?.to_str()?;
    let date = daily_log_date_from_name(name)?;
    let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let prev = parsed.pred_opt()?;
    let dir = primary.parent()?;
    let prev_name = daily_log_file_name(&prev.format("%Y-%m-%d").to_string());
    let plain = dir.join(&prev_name);
    if plain.exists() {
        return Some(plain);
    }
    let gz = dir.join(format!("{prev_name}.gz"));
    gz.exists().then_some(gz)
}

fn build_feedback_attachment_zip_from(log_path: &Path) -> Result<Vec<u8>, String> {
    let mut combined = Vec::new();
    // #905 trap: `log_path` (today's daily file) can be nearly empty just
    // after midnight. Prepend the previous day's file, if one exists, so a
    // submission moments after the UTC boundary still carries useful
    // recent context. A no-op for a legacy (pre-#905) bare `petal.log`
    // path, which has no such sibling. Routed through the SAME
    // `read_log_file_as_plaintext` decompression boundary the local export
    // uses (#905 review Finding 2) -- decompression happens BEFORE the
    // tail cap below, never after: capping raw (possibly compressed) bytes
    // would both misjudge the real content size and, worse, could slice a
    // gzip stream mid-member and feed garbage to `redact_for_export`.
    if let Some(prev) = previous_daily_log_path(log_path) {
        if let Ok(mut bytes) = read_log_file_as_plaintext(&prev) {
            combined.append(&mut bytes);
        }
    }
    let today_bytes = read_log_file_as_plaintext(log_path).map_err(|e| {
        format!(
            "no diagnostics available yet (could not read {}: {e})",
            log_path.display()
        )
    })?;
    combined.extend_from_slice(&today_bytes);
    let tail = tail_bytes(&combined, FEEDBACK_ATTACHMENT_LOG_TAIL_BYTES);
    let redacted = redact_for_export(&String::from_utf8_lossy(tail));
    let entries = vec![("petal.log".to_string(), redacted)];
    let bytes = build_zip_bytes(FEEDBACK_ATTACHMENT_README, &entries)?;
    if bytes.len() > FEEDBACK_ATTACHMENT_MAX_ZIP_BYTES {
        return Err(format!(
            "feedback diagnostics archive too large ({} bytes > {} byte cap) -- try again after the log rotates, or submit without diagnostics",
            bytes.len(),
            FEEDBACK_ATTACHMENT_MAX_ZIP_BYTES
        ));
    }
    Ok(bytes)
}

/// The last `max_len` bytes of `data`, cut at the nearest following newline
/// so the kept content starts at a whole log line rather than mid-line. A
/// cosmetic nicety only -- `redact_for_export` operates correctly on the cut
/// slice regardless of where the cut lands (via `String::from_utf8_lossy`,
/// which tolerates a slice that starts mid-UTF8-codepoint).
fn tail_bytes(data: &[u8], max_len: usize) -> &[u8] {
    if data.len() <= max_len {
        return data;
    }
    let start = data.len() - max_len;
    let cut = data[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|idx| start + idx + 1)
        .unwrap_or(start);
    &data[cut..]
}

fn reveal_in_file_manager(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        // `explorer.exe /select,<path>` spawns the shell and returns
        // immediately with an unreliable exit code, so a successful spawn is
        // treated as revealed (the standard pattern for reveal-in-Explorer).
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
            .is_ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = path;
        false
    }
}

/// The FAST half of rotation: rename `log_path` to a fresh legacy-shaped
/// `petal-<timestamp>.log` if it's over `max_bytes`. A single `metadata` +
/// (maybe) `rename` syscall -- deliberately kept separate from
/// `prune_rotated_logs` (a directory scan + N deletes) so a caller that
/// must do this synchronously under a lock (`daily_log_write`'s same-day
/// backstop -- the very next write needs the path to genuinely be free)
/// isn't also forced to do the slow part while holding it (#905 review
/// Finding 3). Returns whether a rotation happened, i.e. whether
/// `prune_rotated_logs` is now worth running.
fn rename_oversized_log(log_path: &Path, max_bytes: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(log_path) else {
        return false;
    };
    if metadata.len() <= max_bytes {
        return false;
    }
    let Some(parent) = log_path.parent() else {
        return false;
    };
    let rotated_name = format!("petal-{}.log", compact_timestamp_for_filename());
    let rotated_path = parent.join(rotated_name);
    if let Err(e) = std::fs::rename(log_path, &rotated_path) {
        eprintln!(
            "logging: failed to rotate oversized log {} -> {} ({e})",
            log_path.display(),
            rotated_path.display()
        );
        return false;
    }
    true
}

fn prune_rotated_logs(log_path: &Path, keep: usize) {
    let Some(parent) = log_path.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let mut rotated: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(|name| name.starts_with("petal-") && name.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();
    rotated.sort();
    let remove_count = rotated.len().saturating_sub(keep);
    for path in rotated.into_iter().take(remove_count) {
        let _ = std::fs::remove_file(path);
    }
}

fn compact_timestamp_for_filename() -> String {
    chrono_like_timestamp()
        .replace(['-', ':'], "")
        .replace(' ', "-")
        .replace('.', "")
}

// -- #905: per-day log rolling, gzip-on-roll, and age-based retention -----

/// In-memory state for the live per-day file sink: which day it currently
/// believes it's writing, the open handle for that day (`None` right after
/// a roll/backstop, until the next write reopens it), and how many bytes
/// have been written to it so far (used only for the same-day size
/// backstop -- NOT a byte-for-byte mirror of the file's real length, since
/// it's zeroed on every roll rather than re-measured).
struct DailyLogState {
    date: String,
    file: Option<File>,
    bytes_written: u64,
}

impl DailyLogState {
    fn new(dir: &Path) -> Self {
        let date = today_utc_string();
        let path = dir.join(daily_log_file_name(&date));
        let (file, bytes_written) = open_and_measure(&path);
        DailyLogState {
            date,
            file,
            bytes_written,
        }
    }
}

fn open_and_measure(path: &Path) -> (Option<File>, u64) {
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            (Some(f), len)
        }
        Err(e) => {
            eprintln!(
                "logging: failed to open log file {} ({e}) -- file logging DISABLED this run, stdout only",
                path.display()
            );
            (None, 0)
        }
    }
}

/// The actual roll/write decision, factored out of `DailyLogSink::log` so
/// it's directly unit-testable with a SYNTHETIC `today` instead of real
/// wall-clock time (#905's Definition of Done requires pinning "a session
/// spanning midnight produces two files without a restart" -- this is the
/// function that proves it).
///
/// Two things can trigger a roll: `today` no longer matching `state.date`
/// (a real UTC date change), or the same-day file already having crossed
/// `SAME_DAY_SIZE_BACKSTOP_BYTES` (bounding one pathological all-day
/// meeting). `on_roll` is called with the path of a file this function just
/// finished writing and closed, exactly once per DATE-CHANGE roll --
/// production gzips it on a background thread; tests just record the path.
///
/// The size-backstop path is different: it renames straight to a
/// `petal-<timestamp>.log` (same legacy shape the pre-#905 startup-only
/// rotation used) via `rename_oversized_log` SYNCHRONOUSLY, right here,
/// because it must -- the very next block below needs that path to
/// genuinely be free so it opens a truly fresh file, not the still-oversized
/// one. But `rename_oversized_log` is only the fast half of rotation; the
/// slow half (`prune_rotated_logs`'s directory scan + N deletes) is handed
/// to `on_backstop_prune` instead of also running inline (#905 review
/// Finding 3: an earlier version ran the FULL rename-then-prune sequence,
/// prune included, synchronously here -- since this function is always
/// called with the live sink's state lock held, that blocked every other
/// logging thread in the process, including a 45Hz pointer loop and every
/// capture thread, for as long as the prune's disk I/O took).
fn daily_log_write(
    dir: &Path,
    state: &mut DailyLogState,
    today: String,
    line: &str,
    mut on_roll: impl FnMut(PathBuf),
    mut on_backstop_prune: impl FnMut(PathBuf),
) {
    if state.date != today {
        let old_date = std::mem::replace(&mut state.date, today);
        if let Some(old_file) = state.file.take() {
            drop(old_file);
            on_roll(dir.join(daily_log_file_name(&old_date)));
        }
        state.bytes_written = 0;
    } else if state.bytes_written > SAME_DAY_SIZE_BACKSTOP_BYTES {
        if let Some(old_file) = state.file.take() {
            drop(old_file);
        }
        let path = dir.join(daily_log_file_name(&state.date));
        // `max_bytes = 0` forces the rename unconditionally -- the
        // threshold check already happened above under the live mutex, so
        // this is "rotate now," not "rotate if still oversized."
        if rename_oversized_log(&path, 0) {
            on_backstop_prune(path);
        }
        state.bytes_written = 0;
    }
    if state.file.is_none() {
        let path = dir.join(daily_log_file_name(&state.date));
        let (file, bytes_written) = open_and_measure(&path);
        state.file = file;
        state.bytes_written = bytes_written;
    }
    if let Some(file) = state.file.as_mut() {
        if let Err(e) = file.write_all(line.as_bytes()) {
            eprintln!("logging: failed to write to log file ({e})");
            return;
        }
        let _ = file.flush();
        state.bytes_written += line.len() as u64;
    }
}

/// Live per-day file sink (#905), chained into the `fern::Dispatch` exactly
/// like the old single `Box<dyn Write + Send>` file handle was, replacing
/// the fixed `petal.log` opened once at startup. Receives the ALREADY
/// fern-formatted line (timestamp/level/target prefix included) via
/// `record.args()`, since fern's own `Dispatch::log` rewrites the record's
/// args to the formatted text before fanning out to every chained output.
///
/// Deliberately NOT built on `fern::DateBased` (see Cargo.toml's comment on
/// the `fern` dependency for why): that type's real file handle/state lives
/// in a private submodule with no way to observe a roll or force a reopen
/// from outside the crate, and this sink needs both (gzip the file the
/// instant it rolls off, and force a reopen mid-day when the same-day size
/// backstop trips).
struct DailyLogSink {
    dir: PathBuf,
    state: Mutex<DailyLogState>,
}

impl DailyLogSink {
    fn new(dir: PathBuf) -> Self {
        let state = Mutex::new(DailyLogState::new(&dir));
        DailyLogSink { dir, state }
    }
}

impl log::Log for DailyLogSink {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let line = format!("{}\n", record.args());
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        daily_log_write(
            &self.dir,
            &mut state,
            today_utc_string(),
            &line,
            |old_path| spawn_gzip_and_remove(old_path),
            |rotated_log_path| spawn_prune_rotated_logs(rotated_log_path),
        );
    }

    fn flush(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = state.file.as_mut() {
            let _ = file.flush();
        }
    }
}

fn spawn_gzip_and_remove(path: PathBuf) {
    let result = std::thread::Builder::new()
        .name("petal-log-gzip".into())
        .spawn(move || gzip_and_remove(&path));
    if let Err(e) = result {
        eprintln!("logging: failed to spawn the background log-gzip thread ({e})");
    }
}

/// The slow half of the same-day size backstop (#905 review Finding 3):
/// `daily_log_write` already did the fast, must-be-synchronous rename; this
/// runs the directory-scan-and-prune off the hot logging path.
fn spawn_prune_rotated_logs(log_path: PathBuf) {
    let result = std::thread::Builder::new()
        .name("petal-log-prune".into())
        .spawn(move || prune_rotated_logs(&log_path, MAX_ROTATED_LOG_FILES));
    if let Err(e) = result {
        eprintln!("logging: failed to spawn the background log-prune thread ({e})");
    }
}

/// Process-wide serialization for every gzip operation (#905 review Finding
/// 5): the live roll-detection sink (background thread, once per date
/// change) and the startup sweep can both decide to compress the same
/// completed day. Without a single-owner guarantee, two encoders can race
/// to create/rename the SAME final `.gz` path, and -- the data-loss case --
/// a rare clock revisit back onto an already-gzip'd date reopens a fresh
/// plaintext file for it; when THAT later rolls off, the naive "the `.gz`
/// already exists, so just delete the plaintext" idempotency check would
/// silently discard the second visit's content instead of the harmless
/// duplicate it was designed for. A process-wide lock (gzip operations are
/// rare -- at most once per day per file, plus the startup sweep -- so
/// there is no real concurrency to lose) turns both races into a strict
/// ordering: whichever caller gets the lock first re-checks the source
/// file's existence FRESH under the lock, so a true duplicate race
/// self-resolves (the second caller finds the source already gone and is a
/// no-op), while a genuine second visit's content is APPENDED as an
/// additional gzip member rather than overwritten or dropped -- see
/// `append_gz_member`.
static GZIP_LOCK: Mutex<()> = Mutex::new(());

/// Monotonic counter for gzip temp-file names, so two concurrent gzip
/// operations (even within the same process, same PID) never share a temp
/// filename (#905 review Finding 5).
static GZIP_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Gzips `path` and removes the plaintext original on success -- but see
/// `GZIP_LOCK`'s doc comment for why "gzips" here means "appends a new
/// gzip member to `path`.gz" rather than "creates/overwrites it": a rare
/// clock revisit onto an already-completed day must not silently discard
/// either visit's content. Concatenated gzip streams are valid gzip input;
/// read back via `flate2::read::MultiGzDecoder` (not the single-member
/// `GzDecoder`), which decodes every member in sequence.
fn gzip_and_remove(path: &Path) {
    let _guard = GZIP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Re-check FRESH under the lock, not before it: a racing caller that
    // already compressed+removed this exact file while we waited for the
    // lock means there's nothing left for us to do (the common case is two
    // callers noticing the same completed day at nearly the same time).
    if !path.exists() {
        return;
    }
    let gz_path = PathBuf::from(format!("{}.gz", path.display()));
    match append_gz_member(path, &gz_path) {
        Ok(()) => {
            if let Err(e) = std::fs::remove_file(path) {
                eprintln!(
                    "logging: gzip'd {} to {} but failed to remove the original ({e})",
                    path.display(),
                    gz_path.display()
                );
            }
        }
        Err(e) => {
            eprintln!(
                "logging: failed to gzip {} ({e}) -- leaving it uncompressed",
                path.display()
            );
        }
    }
}

/// Compresses `src` into a fresh gzip member, VALIDATES that it decodes
/// before touching anything else (#905 review: never delete the only
/// plaintext copy before the compressed replacement is proven good), then
/// appends that member's bytes onto `dst` (creating it if absent). The temp
/// file used for the intermediate member lives alongside `dst` (same
/// volume, so the final append's read is a same-filesystem operation) but
/// under a hidden, uniquely-numbered name that never matches this module's
/// log-file naming grammar (`is_any_log_file_name`) even transiently --
/// #905 review Finding 1: an earlier version's temp name
/// (`<dst>.tmp-<pid>`) DID match a too-loose version of that grammar, so a
/// process killed mid-compress (or a read racing an in-progress write)
/// could see the partial/compressed bytes treated as plaintext and pushed
/// through export with no redaction at all.
fn append_gz_member(src: &Path, dst: &Path) -> std::io::Result<()> {
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let seq = GZIP_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_member = dir.join(format!(".petal-log-gzip-{}-{seq}.tmp", std::process::id()));

    let compress_result = (|| -> std::io::Result<()> {
        let mut input = File::open(src)?;
        let out = File::create(&tmp_member)?;
        let mut encoder = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?;
        Ok(())
    })();
    if let Err(e) = compress_result {
        let _ = std::fs::remove_file(&tmp_member);
        return Err(e);
    }

    if let Err(e) = validate_gz_member(&tmp_member) {
        let _ = std::fs::remove_file(&tmp_member);
        return Err(e);
    }

    let append_result = (|| -> std::io::Result<()> {
        let member_bytes = std::fs::read(&tmp_member)?;
        let mut out = OpenOptions::new().create(true).append(true).open(dst)?;
        out.write_all(&member_bytes)?;
        out.flush()
    })();
    let _ = std::fs::remove_file(&tmp_member);
    append_result
}

/// Decodes `path` as a single gzip member purely to prove it's well-formed
/// -- the output is discarded. Run BEFORE the member is appended to the
/// real `.gz` and before the plaintext source is removed, so a corrupt
/// compression (disk full mid-write, etc.) is caught while the only copy
/// of the data is still the untouched plaintext original.
fn validate_gz_member(path: &Path) -> std::io::Result<()> {
    let file = File::open(path)?;
    let mut decoder = flate2::read::GzDecoder::new(file);
    std::io::copy(&mut decoder, &mut std::io::sink())?;
    Ok(())
}

/// Startup sweep (#905), replacing the old startup-only size-based rotation
/// call: gzip every COMPLETED day's plaintext file -- including any
/// pre-existing legacy `petal.log`/`petal-*.log` files an upgraded install
/// still has lying around -- then delete anything, either naming shape,
/// compressed or not, older than `MAX_LOG_AGE_DAYS`.
fn roll_and_prune_daily_logs(log_dir: &Path) {
    gzip_completed_daily_logs(log_dir);
    prune_old_logs(log_dir, MAX_LOG_AGE_DAYS);
}

fn gzip_completed_daily_logs(log_dir: &Path) {
    let today = today_utc_string();
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".gz") {
            continue;
        }
        let is_completed_daily = daily_log_date_from_name(name)
            .map(|date| date != today)
            .unwrap_or(false);
        // A legacy `petal.log`/`petal-*.log` is NEVER "current" under the
        // new scheme -- nothing writes to it going forward, so it's always
        // safe to gzip on sight.
        if is_completed_daily || is_legacy_log_name(name) {
            gzip_and_remove(&path);
        }
    }
}

/// The calendar date a log file "belongs to," for retention/export
/// purposes: the parsed date for a per-day file, or the file's own mtime
/// date for a legacy file (which carries no date in its name at all).
fn log_file_effective_date(path: &Path) -> Option<chrono::NaiveDate> {
    let name = path.file_name().and_then(|n| n.to_str())?;
    let base = name.strip_suffix(".gz").unwrap_or(name);
    if let Some(date) = daily_log_date_from_name(base) {
        return chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok();
    }
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let datetime: chrono::DateTime<chrono::Utc> = mtime.into();
    Some(datetime.date_naive())
}

fn prune_old_logs(log_dir: &Path, max_age_days: i64) {
    let today = today_utc_string();
    let cutoff = chrono::Utc::now().date_naive() - chrono::Duration::days(max_age_days.max(0));
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_any_log_file_name(name) {
            continue;
        }
        let base = name.strip_suffix(".gz").unwrap_or(name);
        if daily_log_date_from_name(base) == Some(today.as_str()) {
            // Never prune today's active file.
            continue;
        }
        if let Some(date) = log_file_effective_date(&path) {
            if date < cutoff {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Strips `'`/`"` from a value about to be interpolated into one of our own
/// `'...'`-quoted log-line formats (`presence.rs`, `window_diag.rs`). A real
/// name or window title containing an apostrophe (e.g. "O'Brien") would
/// otherwise let the rest of the value escape `redact_after_marker`'s
/// quote-delimited scan in `redact_for_export` below -- both the name/title
/// itself AND anything after it on the line (e.g. a `(identity)`
/// parenthetical) would then leak in an exported diagnostic. Log-display
/// only: never apply this to the underlying value used for UI, presence
/// state, or anything other than a formatted log message.
pub(crate) fn log_safe_quoted(value: &str) -> std::borrow::Cow<'_, str> {
    if value.contains(['\'', '"']) {
        std::borrow::Cow::Owned(value.replace(['\'', '"'], "\u{2019}"))
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

pub fn redact_for_export(input: &str) -> String {
    let mut out = redact_room_credentials(input);
    out = redact_path_credential(&out, "/Users/", false);
    for marker in [
        "identity '",
        "identity \"",
        "room '",
        "room \"",
        "join_room('",
        // `window_diag.rs`'s `window-stack:` line logs every on-screen
        // window's owning app and title (e.g. a Mail/Gmail tab title
        // containing an email address, a document filename) at info level
        // whenever a share renders -- this is PII from OTHER apps, not
        // Petal's own state, so it must be redacted just as aggressively.
        "owner='",
        "name='",
    ] {
        out = redact_after_marker(&out, marker);
    }
    // `presence.rs` logs `'{name}' ({identity})` and bare room display
    // names in several message shapes -- redact the quoted value AND any
    // immediately-following `(identity)` parenthetical in one pass so the
    // LiveKit participant identity (itself a stable, re-identifiable
    // credential) never survives next to the name it belongs to.
    for marker in [
        "presence: '",
        "ParticipantConnected for '",
        "ParticipantDisconnected for '",
        "joined '",
        "disconnected from '",
        "roster for '",
        " in '",
    ] {
        out = redact_quoted_value_and_trailing_paren(&out, marker);
    }
    // Backstop: redact any bare email address wherever it appears (e.g.
    // inside another app's window title that a structural marker above
    // wouldn't match), not just ones following a known marker.
    redact_email_addresses(&out)
}

fn redact_room_credentials(input: &str) -> String {
    let out = redact_path_credential(input, "petal://join/", true);
    let out = redact_path_credential(&out, "/meeting/", false);
    redact_credential_suffixes(&out)
}

fn redact_path_credential(input: &str, prefix: &str, ascii_case_insensitive: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(idx) = find_prefix(rest, prefix, ascii_case_insensitive) {
        out.push_str(&rest[..idx + prefix.len()]);
        let after_prefix = &rest[idx + prefix.len()..];
        let credential_len = credential_path_segment_len(after_prefix);
        if credential_len == 0 {
            rest = after_prefix;
            continue;
        }
        let credential = &after_prefix[..credential_len];
        out.push_str(&redaction_label(credential));
        rest = &after_prefix[credential_len..];
    }
    out.push_str(rest);
    out
}

fn find_prefix(input: &str, prefix: &str, ascii_case_insensitive: bool) -> Option<usize> {
    if !ascii_case_insensitive {
        return input.find(prefix);
    }
    input
        .as_bytes()
        .windows(prefix.len())
        .position(|window| window.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn credential_path_segment_len(input: &str) -> usize {
    input
        .char_indices()
        .find_map(|(idx, ch)| {
            if credential_path_segment_delimiter(ch) {
                Some(idx)
            } else {
                None
            }
        })
        .unwrap_or(input.len())
}

fn credential_path_segment_delimiter(ch: char) -> bool {
    ch == '/'
        || ch == '?'
        || ch == '#'
        || ch.is_whitespace()
        || matches!(ch, '\'' | '"' | '`' | ')' | ']' | '}' | ',' | ';')
}

fn redact_credential_suffixes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' && is_credential_suffix_at(bytes, i) {
            out.push_str(&input[cursor..i + 1]);
            let suffix_end = i + 33;
            out.push_str(&redaction_label(&input[i + 1..suffix_end]));
            cursor = suffix_end;
            i = suffix_end;
        } else {
            i += 1;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

fn is_credential_suffix_at(bytes: &[u8], hyphen_idx: usize) -> bool {
    if hyphen_idx == 0 || hyphen_idx + 33 > bytes.len() {
        return false;
    }
    if !bytes[hyphen_idx - 1].is_ascii_alphanumeric() {
        return false;
    }
    let suffix = &bytes[hyphen_idx + 1..hyphen_idx + 33];
    if !suffix.iter().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    bytes
        .get(hyphen_idx + 33)
        .map(|b| !b.is_ascii_alphanumeric())
        .unwrap_or(true)
}

fn redact_after_marker(input: &str, marker: &str) -> String {
    let quote = marker.chars().last().unwrap_or('\'');
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(idx) = rest.find(marker) {
        let (before, after_before) = rest.split_at(idx + marker.len());
        out.push_str(before);
        let value_and_after = after_before;
        if let Some(end) = value_and_after.find(quote) {
            let value = &value_and_after[..end];
            out.push_str(&redaction_label(value));
            rest = &value_and_after[end..];
        } else {
            out.push_str("<redacted>");
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// Redacts a quoted value via `redact_after_marker`, then also redacts an
/// immediately-following `(identity)` parenthetical if present -- the
/// `'{name}' ({identity})` shape used throughout `presence.rs`. Without
/// this, redacting the name alone still leaves the LiveKit participant
/// identity (itself a stable, re-identifiable credential) sitting in plain
/// text right next to it.
fn redact_quoted_value_and_trailing_paren(input: &str, marker: &str) -> String {
    redact_parenthetical_immediately_after_label(&redact_after_marker(input, marker))
}

fn redact_parenthetical_immediately_after_label(input: &str) -> String {
    const LABEL_PREFIX: &str = "<redacted:";
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(mark_idx) = rest.find(LABEL_PREFIX) {
        let Some(close_rel) = rest[mark_idx..].find('>') else {
            out.push_str(rest);
            return out;
        };
        let label_end = mark_idx + close_rel + 1;
        out.push_str(&rest[..label_end]);
        let after_label = &rest[label_end..];
        // `redact_after_marker` doesn't consume the closing quote
        // delimiter, so it's still sitting right after the label here.
        let (quote_len, after_quote) = match after_label.chars().next() {
            Some(c @ ('\'' | '"')) => (c.len_utf8(), &after_label[c.len_utf8()..]),
            _ => (0, after_label),
        };
        if let Some(after_open) = after_quote.strip_prefix(" (") {
            if let Some(close_rel) = after_open.find(')') {
                out.push_str(&after_label[..quote_len]);
                out.push_str(" (");
                out.push_str(&redaction_label(&after_open[..close_rel]));
                out.push(')');
                rest = &after_open[close_rel + 1..];
                continue;
            }
        }
        rest = after_label;
    }
    out.push_str(rest);
    out
}

/// Redacts every bare email address in `input`, wherever it appears --
/// e.g. inside another app's window title captured by `window_diag.rs`'s
/// `window-stack:` line (a Mail/Gmail tab title, a document filename), not
/// just ones following a known structural marker. A conservative
/// local-part/domain scanner, not a full RFC 5322 validator -- purposely
/// permissive so it over-redacts rather than under-redacts.
fn redact_email_addresses(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            if let Some((start, end)) = email_span_at(input, i) {
                out.push_str(&input[cursor..start]);
                out.push_str(&redaction_label(&input[start..end]));
                cursor = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&input[cursor..]);
    out
}

fn email_span_at(input: &str, at_idx: usize) -> Option<(usize, usize)> {
    let local_start = input[..at_idx]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_email_local_char(c))
        .last()
        .map(|(idx, _)| idx)?;
    let after_at = &input[at_idx + 1..];
    let mut domain_end = 0usize;
    for (idx, c) in after_at.char_indices() {
        if is_email_domain_char(c) {
            domain_end = idx + c.len_utf8();
        } else {
            break;
        }
    }
    if domain_end == 0 {
        return None;
    }
    let domain = &after_at[..domain_end];
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return None;
    }
    Some((local_start, at_idx + 1 + domain_end))
}

fn is_email_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

fn is_email_domain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-')
}

fn redaction_label(value: &str) -> String {
    format!("<redacted:{:08x}>", fnv1a32(value.as_bytes()))
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c9dc5u32;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// Issue #13 (startup crash detection): scan `~/Library/Logs/
/// DiagnosticReports/` for `desktop-*.ips` crash reports newer than
/// `threshold` (the previous petal.log's mtime, or 24h ago if none) and log a
/// loud error-level pointer per file, so a previous session's silent SIGABRT
/// is visible at the top of the next session's log instead of only in a
/// directory nobody looks at. Cheap and non-fatal: any IO error just means no
/// report (crash detection must never itself break startup).
fn report_previous_crashes(threshold: std::time::SystemTime) -> bool {
    let Some(home) = dirs_home() else {
        return false;
    };
    let dir = home.join("Library").join("Logs").join("DiagnosticReports");
    let reports = crash_reports_since(&dir, threshold);
    for path in &reports {
        log::error!(
            "previous session appears to have CRASHED (see {}) -- crash report is newer than the previous petal.log",
            path.display()
        );
    }
    !reports.is_empty()
}

/// Most recent bytes of the previous `petal.log` considered when checking
/// for a vanished session (#878) -- bounded so a huge pre-rotation log never
/// costs an unbounded read at startup. Read from the END of the file (tail),
/// since the previous session's final activity is what decides the verdict.
const VANISHED_SESSION_TAIL_BYTES: u64 = 256 * 1024;
const VANISHED_SESSION_TAIL_LINES: usize = 300;

/// Read up to the last `max_lines` lines of `path`. Any IO error (missing
/// file, first-ever launch) yields an empty `Vec` -- detection must never
/// itself break startup.
///
/// A `.gz` path (#905 review Finding 4: `resolve_current_or_latest_log_file`
/// can now return one) is decompressed FULLY before taking the tail --
/// there's no meaningful way to seek to "near the end" of a compressed
/// stream by byte offset, and a completed day's file is small enough
/// (bounded further still by this issue's own rate-limiting fixes) that
/// this is cheap. A plaintext path keeps the original byte-offset seek, so
/// this remains a cheap tail read for the overwhelmingly common case (the
/// active, uncompressed file).
fn read_log_tail_lines(path: &Path, max_lines: usize) -> Vec<String> {
    let is_gz = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|name| name.ends_with(".gz"))
        .unwrap_or(false);
    let buf = if is_gz {
        let Ok(decompressed) = read_log_file_as_plaintext(path) else {
            return Vec::new();
        };
        String::from_utf8_lossy(&decompressed).into_owned()
    } else {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = File::open(path) else {
            return Vec::new();
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return Vec::new();
        };
        let start = len.saturating_sub(VANISHED_SESSION_TAIL_BYTES);
        if file.seek(SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            // Non-UTF8 boundary from seeking mid-file, or other IO error --
            // fail closed to "nothing to detect" rather than panicking.
            return Vec::new();
        }
        buf
    };
    let lines: Vec<String> = buf.lines().map(str::to_string).collect();
    let skip = lines.len().saturating_sub(max_lines);
    lines[skip..].to_vec()
}

/// Verdict for whether the previous session (as recorded in its own log
/// tail) ended cleanly or vanished mid-meeting (#878).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VanishedSessionVerdict {
    /// No join at all, or a join followed by a real shutdown marker.
    CleanShutdown,
    /// Joined a room with no later shutdown marker, and no crash report
    /// covers the gap -- the case this detector exists for.
    VanishedNoCrashReport,
    /// Joined a room with no later shutdown marker, but a crash report DOES
    /// cover the gap -- `report_previous_crashes` already explains this one;
    /// distinguished here only so the two cases don't look identical.
    VanishedWithCrashReport,
}

const VANISHED_SESSION_SHUTDOWN_MARKERS: &[&str] =
    &["left room", "quit: quit_app", "event journal loop stopped"];

/// Lines that only appear while a meeting is (or was moments ago) live.
/// The join line alone is NOT sufficient evidence (#882 review): the tail
/// is capped at `VANISHED_SESSION_TAIL_LINES` (~24 minutes of in-meeting
/// logging, measured on a real petal.log), so any meeting longer than that
/// scrolls its join line out of the tail and the original join-only
/// detector silently verdicted CleanShutdown -- including for two of the
/// three #878 field deaths it was built from (51min and 60+min sessions).
/// The periodic markers below recur every <=30s while in a meeting, so the
/// LAST one is always near the tail's end for a session that died mid-
/// meeting. Each literal must match a live log line: `session/room.rs`'s
/// "session: joined room '<name>'", `transport/subscriber.rs`'s
/// "compositor feed: ..." receiver health lines, `camera_session.rs`'s
/// "session: camera publish health -- ..." (the #866 lesson: a detector
/// pointed at a log line that stops existing silently matches nothing).
const VANISHED_SESSION_ACTIVITY_MARKERS: &[&str] = &[
    "session: joined room",
    "compositor feed:",
    "camera publish health",
];

/// Pure decision over one log tail: did the previous session's LAST
/// evidence of being in a meeting (a join line, or a periodic in-meeting
/// health line) have a shutdown marker after it? Isolated from any real
/// file so the fixture shapes (clean shutdown, truncated in-room, long
/// meeting whose join scrolled out of the tail, in-room-with-crash) are
/// unit-testable without touching disk (#878, tail fix per #882 review).
fn detect_vanished_session(lines: &[String], crash_report_found: bool) -> VanishedSessionVerdict {
    let Some(last_activity_index) = lines.iter().rposition(|line| {
        VANISHED_SESSION_ACTIVITY_MARKERS
            .iter()
            .any(|marker| line.contains(marker))
    }) else {
        return VanishedSessionVerdict::CleanShutdown;
    };
    let shut_down = lines[last_activity_index + 1..]
        .iter()
        .any(|line| VANISHED_SESSION_SHUTDOWN_MARKERS.iter().any(|marker| line.contains(marker)));
    if shut_down {
        VanishedSessionVerdict::CleanShutdown
    } else if crash_report_found {
        VanishedSessionVerdict::VanishedWithCrashReport
    } else {
        VanishedSessionVerdict::VanishedNoCrashReport
    }
}

/// Sibling to `report_previous_crashes`: a vanished session with NO crash
/// report is a stronger signal than a `.ips` gap alone, since it means the
/// process disappeared without even the OS's own crash reporter catching it
/// (#878's field cases -- WindowServer death takes Petal down with it,
/// leaving no `desktop-*.ips` at all).
fn report_vanished_previous_session(previous_log_tail: &[String], crash_report_found: bool) {
    match detect_vanished_session(previous_log_tail, crash_report_found) {
        VanishedSessionVerdict::CleanShutdown => {}
        VanishedSessionVerdict::VanishedNoCrashReport => {
            log::warn!(
                "previous session VANISHED mid-meeting (no shutdown marker, no crash report) -- see #878"
            );
            capture_sentry_diagnostic(SentryDiagnosticEvent::PreviousSessionVanished(
                PreviousSessionVanishedDiagnostic {
                    crash_report: VanishedSessionCrashReportTag::NotFound,
                },
            ));
        }
        VanishedSessionVerdict::VanishedWithCrashReport => {
            // Distinguishable from the no-report case: `report_previous_crashes`
            // already logged the loud error-level pointer to the .ips file, so
            // this is informational, not a fresh alarm.
            log::info!(
                "previous session ended mid-meeting, but a crash report was found for the same window -- not a silent vanish, see #878"
            );
        }
    }
}

/// Pure, unit-testable core of `report_previous_crashes`: every
/// `desktop-*.ips` file directly inside `dir` whose modification time is
/// strictly newer than `threshold`, sorted by path for deterministic output.
/// Keep this glob coupled to the Cargo crate/binary name `desktop`; if the
/// crate is renamed, update this crash-report scan in the same change.
/// IO errors (missing dir, unreadable entries) yield an empty/partial list
/// rather than an error -- see the caller's non-fatal requirement.
fn crash_reports_since(dir: &Path, threshold: std::time::SystemTime) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return false;
            };
            if !(name.starts_with("desktop-") && name.ends_with(".ips")) {
                return false;
            }
            entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|mtime| mtime > threshold)
                .unwrap_or(false)
        })
        .map(|entry| entry.path())
        .collect();
    out.sort();
    out
}

/// Minimal, dependency-free "good enough" UTC timestamp (`YYYY-MM-DD
/// HH:MM:SS.mmm`) for log lines. Deliberately not pulling in `chrono` or
/// `time` as a new dependency for a log-line prefix -- `std::time::SystemTime`
/// plus fixed-width integer division is enough precision for a debug log and
/// keeps this module's own dependency footprint minimal.
fn chrono_like_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let millis = now.subsec_millis();
    let days_since_epoch = total_secs / 86_400;
    let secs_of_day = total_secs % 86_400;
    let (h, m, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    // Civil-from-days conversion (Howard Hinnant's algorithm) -- avoids
    // pulling in a date/time crate just to turn a day count into y/m/d.
    let z = days_since_epoch as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m_num = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_final = if m_num <= 2 { y + 1 } else { y };

    format!("{y_final:04}-{m_num:02}-{d:02} {h:02}:{m:02}:{s:02}.{millis:03}")
}

/// Wire `std::panic::set_hook` so a panic anywhere in the app (any thread)
/// logs the message + file:line to the same file sink at `error` level
/// *before* the default hook runs -- so a crash is diagnosable from the log
/// alone, with no attached debugger and no crash-reporter access needed.
/// Chains to the previous hook (rather than replacing it outright) so the
/// default stderr panic message + backtrace-on-`RUST_BACKTRACE` behavior is
/// preserved for anyone who does have a terminal attached.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    let previous = Mutex::new(Some(previous));
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let message = panic_message(info);
        // `target:` set to PANIC_HOOK_LOG_TARGET so the Sentry log bridge
        // skips this line (see that constant's doc comment) -- the local
        // file/stdout sink is unaffected.
        log::error!(target: PANIC_HOOK_LOG_TARGET, "PANIC at {location}: {message}");

        forward_panic_to_sentry(info);

        if let Ok(mut guard) = previous.lock() {
            if let Some(hook) = guard.take() {
                hook(info);
                *guard = Some(hook);
            }
        }
    }));
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Resolve the Sentry DSN (#281 plan point 2). Compile-time embedding via
/// `option_env!` is the production path: a notarized `.app` launched via
/// `open`/Dock/Spotlight has no shell environment, so a runtime-only var
/// would never be set for a real user, and this is why the build must bake
/// it in (see `build.rs`'s `PETAL_SENTRY_DSN` handling and docs/
/// RELEASING.md). A *runtime* env var is checked FIRST, purely as a
/// local-testing convenience for pointing at a throwaway Sentry project
/// without recompiling -- production behavior must never depend on one
/// being set post-build, and it never is, because a shipped `.app` has no
/// mechanism to set it. Mirrors `transport::token::backend_base_url()`'s
/// exact runtime-then-compile-time pattern, including filtering an
/// empty-but-set value the same way (an empty compile-time bake, e.g. from
/// a CI run with `PETAL_SENTRY_DSN=`, must resolve to "absent", not "".
fn sentry_dsn() -> Option<String> {
    std::env::var("PETAL_SENTRY_DSN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            option_env!("PETAL_SENTRY_DSN")
                .map(str::to_string)
                .filter(|value| !value.trim().is_empty())
        })
        .map(|value| value.trim().to_string())
}

/// Initialize Sentry off-device crash/error reporting (#281). Called as the
/// very first statement of `init()`. A clean no-op -- no client, no network
/// attempt, `SENTRY_GUARD` stays unset -- whenever `sentry_dsn()` returns
/// `None`, which is every `cargo build`/`cargo test`/`tauri dev` run by
/// default (no DSN is ever baked in without an explicit `PETAL_SENTRY_DSN`
/// at build time).
fn init_sentry() {
    let Some(dsn_string) = sentry_dsn() else {
        return;
    };
    let dsn = match dsn_string.parse::<sentry::types::Dsn>() {
        Ok(dsn) => dsn,
        Err(e) => {
            eprintln!(
                "logging: PETAL_SENTRY_DSN is set but failed to parse ({e}) -- crash reporting disabled this run"
            );
            return;
        }
    };

    let guard = sentry::init(sentry::ClientOptions {
        dsn: Some(dsn),
        release: Some(env!("CARGO_PKG_VERSION").into()),
        environment: Some(if cfg!(debug_assertions) {
            "development".into()
        } else {
            "production".into()
        }),
        // Sampling/cost config, pinned explicitly (#281 point 9) rather than
        // left to defaults: no performance-tracing product is used (avoids
        // an unrelated PII surface -- transaction names can carry arbitrary
        // app data), errors are fully sampled, and the breadcrumb ring
        // buffer is capped well below the library default of 100.
        traces_sample_rate: 0.0,
        sample_rate: 1.0,
        max_breadcrumbs: 50,
        // Exactly one panic hook, and one ObjC uncaught-exception hook (no
        // second competing chain, #281 point 5): `default_integrations:
        // false` disables Sentry's own auto-installed `PanicIntegration`
        // (which would otherwise call `std::panic::set_hook` itself).
        // `install_panic_hook()`/`objc_exception`'s handler below instead
        // call `sentry_panic`'s event-building logic directly and forward
        // through `sentry::capture_event`, from EXISTING hooks this module
        // already owned before #281.
        default_integrations: false,
        // Allowlist-first PII policy (#281 point 8): never forward a raw
        // free-text log/panic/exception message verbatim off this machine.
        // Both hooks reuse `redact_for_export()` -- the SAME function the
        // manual "Export logs" path already calls -- as a scrub backstop,
        // and additionally strip every event field this policy has no fixed
        // allowlist entry for. See `scrub_event_for_sentry()`.
        before_send: Some(std::sync::Arc::new(scrub_event_for_sentry)),
        before_breadcrumb: Some(std::sync::Arc::new(scrub_breadcrumb_for_sentry)),
        // The separate structured-Logs/Metrics Sentry products are not used
        // (the `logs` cargo feature isn't even enabled, so these fields are
        // already inert) -- set explicitly so the intent doesn't silently
        // depend on a feature flag never flipping.
        enable_logs: false,
        enable_metrics: false,
        shutdown_timeout: SENTRY_FLUSH_TIMEOUT,
        ..Default::default()
    });

    if !guard.is_enabled() {
        eprintln!(
            "logging: sentry::init() did not produce an enabled client -- crash reporting disabled this run"
        );
        return;
    }

    // Fixed, allowlisted tag set (#281 point 8): build version + OS version,
    // set once here so they attach to every event/breadcrumb automatically.
    // The third allowlisted tag, `error_category`, is set per-event inside
    // `scrub_event_for_sentry()` since it depends on which path captured
    // the event (panic / ObjC exception / bridged log record).
    sentry::configure_scope(|scope| {
        scope.set_tag("build_version", env!("CARGO_PKG_VERSION"));
        scope.set_tag("os_version", os_version_tag());
    });

    // Held for the process lifetime -- see `SENTRY_GUARD`'s doc comment.
    // `set()` only fails if already set (double `init_sentry()` call, which
    // can't happen since `init()` runs exactly once from `run()`); on that
    // impossible path, log loudly and let this guard drop (disabling
    // reporting) rather than silently leaking two clients.
    if SENTRY_GUARD.set(guard).is_err() {
        eprintln!(
            "logging: Sentry guard already set (double logging::init()?) -- new client dropped"
        );
    }
}

/// Shared Sentry `before_send` hook (#281 point 8): the enforcement point
/// for the allowlist-first PII policy, run for EVERY event this process
/// ever sends to Sentry -- the manually-built panic/ObjC-exception events
/// (`forward_panic_to_sentry`/`forward_objc_exception_to_sentry` below) and
/// the `sentry-log` bridge's log-derived events alike, so there is exactly
/// one enforcement point rather than one per call site.
///
/// Policy: only a fixed, allowlisted set of tags survives
/// (`build_version`/`os_version`, set once via `configure_scope`, plus a
/// derived `error_category`); every other field this module has no fixed
/// allowlist entry for is dropped outright (`user`, `request`,
/// `server_name`, `contexts`, `extra`, `logger` -- none of these are ever
/// populated by this app's own event-building code, but `sentry-log`'s
/// bridge DOES populate `logger`/`contexts` from the raw `log::Record`, so
/// they must be actively stripped here, not just left unset). Whatever
/// message/exception text survives (the one thing this policy can't fully
/// eliminate without rewriting every `log::error!`/`log::warn!` call site
/// into a fixed template, which is out of scope for #281) is run through
/// `redact_for_export()` -- the SAME marker-based scrub the manual "Export
/// logs" feature already uses -- as a defense-in-depth backstop.
fn scrub_event_for_sentry(
    mut event: sentry::protocol::Event<'static>,
) -> Option<sentry::protocol::Event<'static>> {
    if !SENTRY_ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    if let Some(event_name) = event.tags.get("event_name").map(String::as_str) {
        if !DIAGNOSTIC_EVENT_NAMES.contains(&event_name) || !valid_sentry_diagnostic_event(&event) {
            return None;
        }
        let event_name = event_name.to_string();
        // Rebuild rather than remove fields one-by-one. This event class has
        // only its closed message and tags, with no exceptions, breadcrumbs,
        // contexts, or arbitrary data.
        return Some(sentry::protocol::Event {
            event_id: event.event_id,
            timestamp: event.timestamp,
            level: sentry::protocol::Level::Error,
            message: event.message,
            fingerprint: Cow::Owned(vec![event_name.into()]),
            tags: event.tags,
            release: Some(Cow::Borrowed(env!("CARGO_PKG_VERSION"))),
            ..Default::default()
        });
    }
    event.message = event.message.map(|m| redact_for_export(&m));
    for exception in event.exception.iter_mut() {
        exception.value = exception.value.take().map(|v| redact_for_export(&v));
    }
    for breadcrumb in event.breadcrumbs.iter_mut() {
        breadcrumb.message = breadcrumb.message.take().map(|m| redact_for_export(&m));
        breadcrumb.data.clear();
    }

    event.user = None;
    event.request = None;
    event.server_name = None;
    event.contexts.clear();
    event.extra.clear();
    event.logger = None;

    if !event.tags.contains_key("error_category") {
        let category = event
            .exception
            .first()
            .and_then(|exc| exc.mechanism.as_ref())
            .map(|m| m.ty.clone())
            .unwrap_or_else(|| "log_error".to_string());
        event.tags.insert("error_category".to_string(), category);
    }
    const ALLOWED_TAGS: &[&str] = &["build_version", "os_version", "error_category"];
    event
        .tags
        .retain(|key, _| ALLOWED_TAGS.contains(&key.as_str()));

    Some(event)
}

fn valid_sentry_diagnostic_event(event: &sentry::protocol::Event<'_>) -> bool {
    let event_name = event.tags.get("event_name").map(String::as_str);
    if event.tags.len() != DIAGNOSTIC_TAGS.len()
        || event
            .tags
            .keys()
            .any(|key| !DIAGNOSTIC_TAGS.contains(&key.as_str()))
        || event.fingerprint.len() != 1
        || event.fingerprint.first().map(|value| value.as_ref()) != event_name
        || event
            .release
            .as_deref()
            .is_some_and(|release| release != env!("CARGO_PKG_VERSION"))
        || event.message.as_deref() != diagnostic_message(&event.tags).as_deref()
        || event.logentry.is_some()
        || !event.exception.is_empty()
        || event.request.is_some()
        || event.logger.is_some()
        || event.culprit.is_some()
    {
        return false;
    }
    event
        .tags
        .iter()
        .all(|(key, value)| valid_diagnostic_tag(key, value))
}

fn valid_diagnostic_tag(key: &str, value: &str) -> bool {
    match key {
        "event_name" => DIAGNOSTIC_EVENT_NAMES.contains(&value),
        "schema_version" => value == SENTRY_DIAGNOSTIC_SCHEMA_VERSION,
        "build_version" => value == env!("CARGO_PKG_VERSION"),
        "os_version" => {
            value == "unknown"
                || (!value.is_empty()
                    && value.len() <= 16
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'.'))
        }
        "architecture" => matches!(value, "arm64" | "x86_64" | "other"),
        // `not_applicable` is reachable: `update-install-failed` fires outside
        // any meeting, so it has no sharer/receiver role to report (#871).
        "session_role" => matches!(value, "sharer" | "receiver" | "both" | "not_applicable"),
        "source_selection" => matches!(
            value,
            "window" | "display" | "system_picker" | "unknown" | "not_applicable"
        ),
        "capture_geometry" | "configured_geometry" => matches!(
            value,
            "tiny" | "small" | "medium" | "large" | "very_large" | "unknown" | "not_applicable"
        ),
        "pixel_format" => matches!(
            value,
            "bgra" | "nv12" | "other_supported" | "unknown" | "not_applicable"
        ),
        "scale_bucket" => matches!(
            value,
            "1x" | "2x" | "fractional" | "other" | "unknown" | "not_applicable"
        ),
        "encoder_implementation" => matches!(
            value,
            "hardware" | "software" | "unknown" | "not_applicable"
        ),
        "stage_code" => matches!(
            value,
            "validation"
                | "reconfiguration"
                | "first_frame"
                | "publish"
                | "unknown"
                | "not_applicable"
        ),
        "camera_direction" => matches!(value, "publish" | "receive" | "not_applicable"),
        "recovery_action" => matches!(value, "reanchor" | "letterbox" | "not_applicable"),
        "playout_transition" => {
            matches!(value, "repointed" | "unavailable" | "not_applicable")
        },
        "storm_scope" => matches!(
            value,
            "window_share" | "camera" | "remote_window" | "unknown" | "not_applicable"
        ),
        "install_failure_stage" => matches!(
            value,
            "resolve"
                | "stage"
                | "extract"
                | "backup"
                | "promote"
                | "rollback"
                | "privileged"
                | "not_applicable"
        ),
        "install_failure_kind" => matches!(
            value,
            "cross_device"
                | "permission_denied"
                | "read_only"
                | "no_space"
                | "not_found"
                | "other"
                | "not_applicable"
        ),
        "install_volume_boundary" => matches!(
            value,
            "same_volume" | "cross_volume" | "unknown" | "not_applicable"
        ),
        "install_destination_class" => matches!(
            value,
            "applications"
                | "user_applications"
                | "disk_image"
                | "removable_volume"
                | "other"
                | "not_applicable"
        ),
        "overlay_clear_reason" => matches!(
            value,
            "no_publication" | "retired" | "hide_pending" | "not_applicable"
        ),
        "crash_report_status" => matches!(value, "found" | "not_found" | "not_applicable"),
        "pressure_level" => matches!(value, "warn" | "critical" | "not_applicable"),
        "browser_url_extraction_cause" => matches!(
            value,
            "denied" | "timeout" | "ambiguous" | "no-match" | "spawn" | "failed" | "not_applicable"
        ),
        "capture_cadence" | "encode_cadence" => matches!(
            value,
            "healthy" | "reduced" | "severe" | "stalled" | "unknown" | "not_applicable"
        ),
        "queue_backpressure" => matches!(
            value,
            "none" | "low" | "high" | "saturated" | "unknown" | "not_applicable"
        ),
        "decoder_render_health" => matches!(
            value,
            "healthy"
                | "decoder_degraded"
                | "render_degraded"
                | "both_degraded"
                | "unknown"
                | "not_applicable"
        ),
        "dedup_count_bucket" => matches!(value, "1" | "2_9" | "10_99" | "100_plus"),
        _ => false,
    }
}

/// Shared Sentry `before_breadcrumb` hook -- same allowlist-first + scrub
/// backstop policy as `scrub_event_for_sentry()`, applied as breadcrumbs
/// are recorded (they're attached client-side to whatever event is
/// captured next, independent of `before_send`). `data` is always cleared:
/// this codebase does not use `log`'s structured key-value API anywhere
/// today (verified directly), so it is always empty in practice, but
/// allowlist-first means a future call site adding structured fields can't
/// silently start forwarding them off-device.
fn scrub_breadcrumb_for_sentry(
    mut breadcrumb: sentry::protocol::Breadcrumb,
) -> Option<sentry::protocol::Breadcrumb> {
    if !SENTRY_ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    if breadcrumb
        .message
        .as_deref()
        .is_some_and(decoder_allocation_failure_signature)
    {
        // #884: promote kCVReturnAllocationFailed decode failures from a
        // breadcrumb (which high-rate storms evict, #878) to a first-class,
        // rate-limited diagnostic event. The breadcrumb itself still passes
        // through below.
        capture_sentry_diagnostic(SentryDiagnosticEvent::DecoderAllocationFailed(
            DecoderAllocationFailedDiagnostic {
                role: DiagnosticRole::Receiver,
            },
        ));
    }
    if breadcrumb
        .message
        .as_deref()
        .is_some_and(|message| !breadcrumb_storm_allows(message, std::time::Instant::now()))
    {
        return None;
    }
    breadcrumb.message = breadcrumb.message.take().map(|m| redact_for_export(&m));
    breadcrumb.data.clear();
    Some(breadcrumb)
}

/// Forward a panic to Sentry (#281 points 5 and 6). Builds the event via
/// `sentry_panic`'s own conversion logic directly
/// (`PanicIntegration::event_from_panic_info`) rather than calling
/// `sentry_panic::panic_handler` -- that helper internally requires a
/// `PanicIntegration` to be REGISTERED on the client (via
/// `sentry_core::with_integration`), and registering one would also run its
/// `Integration::setup()`, which installs its OWN competing
/// `std::panic::set_hook` chain. We deliberately never register it; this
/// function is the only place its event-building logic runs, from inside
/// the pre-existing `install_panic_hook()` closure.
///
/// No-op if Sentry was never initialized (`SENTRY_GUARD` unset) -- capture/
/// flush would already be inert with no active client, but checking first
/// avoids doing any work (including a subprocess-free stacktrace walk) on
/// the hot panic path when the feature is compiled off.
fn forward_panic_to_sentry(info: &std::panic::PanicHookInfo<'_>) {
    if SENTRY_GUARD.get().is_none() {
        return;
    }
    let integration = sentry::integrations::panic::PanicIntegration::new();
    let event = integration.event_from_panic_info(info);
    sentry::capture_event(event);
    flush_sentry_before_death();
}

/// Forward an uncaught ObjC exception to Sentry (#281 points 5 and 6),
/// called from `objc_exception::petal_uncaught_objc_exception_handler`
/// below. No-op if Sentry was never initialized, same reasoning as
/// `forward_panic_to_sentry`.
fn forward_objc_exception_to_sentry(name: &str, reason: &str, call_stack_symbols: &str) {
    if SENTRY_GUARD.get().is_none() {
        return;
    }
    let event = sentry::protocol::Event {
        level: sentry::protocol::Level::Fatal,
        message: Some(format!("callStackSymbols: {call_stack_symbols}")),
        exception: vec![sentry::protocol::Exception {
            ty: "NSException".into(),
            value: Some(format!("{name}: {reason}")),
            mechanism: Some(sentry::protocol::Mechanism {
                ty: "objc_uncaught_exception".into(),
                handled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }]
        .into(),
        ..Default::default()
    };
    sentry::capture_event(event);
    flush_sentry_before_death();
}

/// Explicit flush-before-death (#281 point 6): MANDATORY, not assumed free.
/// Both the panic and ObjC-exception hooks run moments before the process
/// aborts/exits. Sentry's own `PanicIntegration` hook (which we deliberately
/// bypass, see above) does this internally for the panic path, but since
/// this module builds and captures both event kinds itself, both paths call
/// this explicitly rather than relying on undocumented internal behavior of
/// a mechanism this module doesn't use. Blocks the calling thread (the one
/// that panicked / hit the uncaught exception) for up to
/// `SENTRY_FLUSH_TIMEOUT` before the caller's default abort/terminate
/// proceeds -- without this, the event was captured into an in-process
/// queue but the background HTTP transport thread may never get to send it
/// before the process is gone, which is "the single most likely way this
/// integration ships broken while every test still passes" per #281's plan.
///
/// pub(crate) for the same reason from a third site (#882 review): the
/// `winsrv-port-dead` diagnostic (`platform/sls.rs`) is captured ~0.16s
/// before the window server's death SIGKILLs this process (the #878 field
/// timing) -- without an explicit flush that event nearly always dies in
/// the in-process queue, exactly when it matters most.
pub(crate) fn flush_sentry_before_death() {
    if let Some(client) = sentry::Hub::current().client() {
        client.flush(Some(SENTRY_FLUSH_TIMEOUT));
    }
}

/// Issue #13: ObjC uncaught-exception visibility -- the Rust panic hook's
/// Objective-C twin. `NSSetUncaughtExceptionHandler` registers a process-wide
/// handler that the ObjC runtime calls with the live `NSException` right
/// before the default terminate/abort path runs. This turns a future "silent"
/// SIGABRT (the exact class behind the compositor/share-border teardown
/// crashes: an ObjC exception during deferred NSPanel dealloc unwinding
/// through tao's run-loop observer) into a NAMED exception with a full
/// `callStackSymbols` backtrace in petal.log.
///
/// **Logging-only, by design:** the handler logs and returns -- it never
/// tries to swallow or recover; the runtime's default abort proceeds
/// unimpeded. Honest caveat: an ObjC exception that crosses a Rust
/// `catch_unwind` boundary (tao's `stop_app_on_panic`) aborts as a *foreign
/// exception* during unwinding, potentially before reaching the runtime's
/// top-level uncaught handler -- so this handler is guaranteed coverage for
/// genuinely-uncaught exceptions, and best-effort for the foreign-exception
/// path; the step-bracketing logs in `session.rs`/`share_border.rs` are the
/// complementary net for that case.
///
/// House FFI pattern (see `native_display.rs`): raw `extern "C"` + `objc2`
/// `msg_send!` on `AnyObject` -- NO new binding crates, no new ObjC class
/// metadata, so the hard-won link-cleanliness (CLAUDE.md M0 notes) is
/// untouched.
#[cfg(target_os = "macos")]
mod objc_exception {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use std::os::raw::c_char;

    #[link(name = "Foundation", kind = "framework")]
    extern "C" {
        fn NSSetUncaughtExceptionHandler(handler: Option<extern "C" fn(*mut AnyObject)>);
    }

    /// Read an `NSString*` (possibly nil) into a Rust `String` via
    /// `UTF8String` -- no NSString binding type needed.
    unsafe fn nsstring_to_string(obj: *mut AnyObject) -> String {
        if obj.is_null() {
            return "<nil>".to_string();
        }
        let utf8: *const c_char = msg_send![&*obj, UTF8String];
        if utf8.is_null() {
            return "<nil>".to_string();
        }
        std::ffi::CStr::from_ptr(utf8)
            .to_string_lossy()
            .into_owned()
    }

    /// The registered handler. Must never unwind (it's `extern "C"`, called
    /// by the ObjC runtime mid-crash), so every ObjC read is null-guarded and
    /// the whole body is straight-line logging.
    extern "C" fn petal_uncaught_objc_exception_handler(exception: *mut AnyObject) {
        let (name, reason, stack) = unsafe {
            if exception.is_null() {
                (
                    "<null NSException>".to_string(),
                    "<nil>".to_string(),
                    "<no stack>".to_string(),
                )
            } else {
                let name_obj: *mut AnyObject = msg_send![&*exception, name];
                let reason_obj: *mut AnyObject = msg_send![&*exception, reason];
                let symbols_obj: *mut AnyObject = msg_send![&*exception, callStackSymbols];
                let stack = if symbols_obj.is_null() {
                    "<no stack>".to_string()
                } else {
                    // `description` of the NSArray<NSString*> gives the whole
                    // backtrace in one string -- one message send instead of a
                    // count/objectAtIndex: loop, mid-crash.
                    let desc: *mut AnyObject = msg_send![&*symbols_obj, description];
                    nsstring_to_string(desc)
                };
                (
                    nsstring_to_string(name_obj),
                    nsstring_to_string(reason_obj),
                    stack,
                )
            }
        };
        // `target:` set to OBJC_EXCEPTION_HOOK_LOG_TARGET so the Sentry log
        // bridge skips this line (see that constant's doc comment) -- the
        // local file/stdout sink is unaffected.
        log::error!(
            target: super::OBJC_EXCEPTION_HOOK_LOG_TARGET,
            "UNCAUGHT ObjC EXCEPTION (default abort will proceed): name={name} reason={reason}\ncallStackSymbols: {stack}"
        );
        // #281: forward + explicitly flush BEFORE returning -- this is the
        // one path that does NOT get a flush for free (see
        // `flush_sentry_before_death`'s doc comment). Must happen before
        // this function returns and the runtime's default abort proceeds.
        super::forward_objc_exception_to_sentry(&name, &reason, &stack);
        // Logging-only: return and let the runtime's default handler
        // terminate the process as it normally would.
    }

    /// Register the handler. Called once from `init()`, right after the Rust
    /// panic hook, so both crash channels are wired before any other startup
    /// code runs.
    pub fn install() {
        unsafe {
            NSSetUncaughtExceptionHandler(Some(petal_uncaught_objc_exception_handler));
        }
        log::info!("logging: NSUncaughtExceptionHandler installed (ObjC exceptions will be named in this log before any abort)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log as _;
    use std::io::Read;
    use std::time::{Duration, SystemTime};
    use zip::ZipArchive;

    static STORM_DETECTOR_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn touch(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"{}").unwrap();
        path
    }

    fn set_mtime(path: &std::path::Path, time: SystemTime) {
        File::open(path).unwrap().set_modified(time).unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("petal-logging-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // #595: `RUST_LOG` must accept standard comma-separated per-module
    // directive syntax (not just a bare level word), and a value that fails
    // to parse must produce a visible warning instead of a silent fallback.

    #[test]
    fn resolve_log_filter_defaults_to_info_with_no_rust_log() {
        let resolved = resolve_log_filter(None);
        assert_eq!(resolved.max_level, log::LevelFilter::Info);
        assert!(resolved.parse_warning.is_none());
        assert!(enabled(&resolved.filter, log::Level::Info, "desktop_lib"));
        assert!(!enabled(&resolved.filter, log::Level::Debug, "desktop_lib"));
        // Noisy third-party crates are still denylisted to `warn` under the
        // `info` default -- unchanged from the pre-#595 behavior.
        assert!(enabled(&resolved.filter, log::Level::Warn, "livekit"));
        assert!(!enabled(&resolved.filter, log::Level::Info, "livekit"));
    }

    #[test]
    fn resolve_log_filter_accepts_bare_level_like_before() {
        let resolved = resolve_log_filter(Some("debug"));
        assert_eq!(resolved.max_level, log::LevelFilter::Debug);
        assert!(resolved.parse_warning.is_none());
        assert!(enabled(&resolved.filter, log::Level::Debug, "desktop_lib"));
        // `debug` is still noisier than `warn`, so the denylist still
        // applies to third-party crates that didn't get their own directive.
        assert!(!enabled(&resolved.filter, log::Level::Debug, "livekit"));
        assert!(enabled(&resolved.filter, log::Level::Warn, "livekit"));
    }

    #[test]
    fn resolve_log_filter_bare_warn_skips_the_denylist() {
        // Matches the pre-#595 behavior: a global level already at or below
        // `warn` has nothing left to turn down for third-party crates, so
        // they get exactly the requested level like everything else.
        let resolved = resolve_log_filter(Some("error"));
        assert_eq!(resolved.max_level, log::LevelFilter::Error);
        assert!(resolved.parse_warning.is_none());
        assert!(!enabled(&resolved.filter, log::Level::Warn, "livekit"));
        assert!(enabled(&resolved.filter, log::Level::Error, "livekit"));
    }

    #[test]
    fn resolve_log_filter_accepts_standard_per_module_directive_syntax() {
        // The exact value from #595's report.
        let resolved = resolve_log_filter(Some("info,desktop::remote_control=debug"));
        assert!(
            resolved.parse_warning.is_none(),
            "expected no parse warning, got {:?}",
            resolved.parse_warning
        );
        // The named module gets its own, more verbose level...
        assert!(enabled(
            &resolved.filter,
            log::Level::Debug,
            "desktop::remote_control"
        ));
        // ...prefix-matching submodules too, matching real env_logger/RUST_LOG
        // semantics (not just an exact-string match).
        assert!(enabled(
            &resolved.filter,
            log::Level::Debug,
            "desktop::remote_control::input"
        ));
        // ...while everything else stays at the global `info` level.
        assert!(enabled(&resolved.filter, log::Level::Info, "desktop::session"));
        assert!(!enabled(&resolved.filter, log::Level::Debug, "desktop::session"));
        // Noisy third-party crates are still denylisted under the `info` global.
        assert!(!enabled(&resolved.filter, log::Level::Info, "livekit"));
    }

    #[test]
    fn resolve_log_filter_lets_a_directive_override_the_denylist() {
        let resolved = resolve_log_filter(Some("info,livekit=debug"));
        assert!(resolved.parse_warning.is_none());
        // The user's own explicit directive for a denylisted crate wins over
        // our default -- it must not be clobbered by the denylist.
        assert!(enabled(&resolved.filter, log::Level::Debug, "livekit"));
    }

    #[test]
    fn resolve_log_filter_malformed_value_warns_instead_of_silently_falling_back() {
        // This is the exact bug #595 reports: a per-module directive with a
        // typo used to fail `str::parse::<log::LevelFilter>()` and silently
        // become `info` with no warning at all. Assert the warning path,
        // not just that SOME level got applied.
        let resolved =
            resolve_log_filter(Some("info,desktop::remote_control=notalevel"));
        let warning = resolved
            .parse_warning
            .as_deref()
            .expect("a malformed directive must produce a visible warning, not a silent fallback");
        assert!(
            warning.contains("info,desktop::remote_control=notalevel"),
            "warning should name the exact value received: {warning}"
        );
        assert!(
            warning.contains("info"),
            "warning should name the level actually applied: {warning}"
        );
        // The level actually applied must be the documented default, and it
        // must actually take effect (not just be claimed in the message).
        assert_eq!(resolved.max_level, log::LevelFilter::Info);
        assert!(enabled(&resolved.filter, log::Level::Info, "desktop_lib"));
        assert!(!enabled(&resolved.filter, log::Level::Debug, "desktop_lib"));
    }

    #[test]
    fn resolve_log_filter_multiple_equals_signs_warns() {
        // A second class of malformed spec (too many `=`s in one clause),
        // distinct from an unrecognized level word.
        let resolved = resolve_log_filter(Some("desktop::remote_control=warn=info"));
        assert!(
            resolved.parse_warning.is_some(),
            "expected a parse warning for a multi-`=` directive"
        );
        assert_eq!(resolved.max_level, log::LevelFilter::Info);
    }

    // #788: the ADM lifecycle lines from the vendored LiveKit SDK are the
    // single most decisive evidence for "is remote audio being decoded into a
    // real playout device, or into nothing" (#787) -- and the denylist above
    // hid all of them from a normal user's `petal.log`. These tests pin the
    // carve-out in BOTH directions, per CLAUDE.md's "test a gate in both
    // directions" rule: the decisive lines must APPEAR under a default launch,
    // and everything the denylist exists for must STILL be suppressed. A test
    // that only asserts the first half cannot tell a narrow carve-out from
    // having lifted the denylist wholesale.

    /// Representative real targets the denylist exists to silence. Each one is
    /// a target that genuinely does log at info/debug in a live session --
    /// `livekit::rtc_engine::rtc_session` (per-negotiation debug), `libwebrtc`
    /// (the whole native WebRTC log sink, bridged at debug by
    /// `vendor/libwebrtc/src/native/peer_connection_factory.rs`), plus the
    /// HTTP/TLS/webview stacks.
    const NOISY_TARGET_SAMPLES: &[&str] = &[
        "livekit",
        "livekit::room",
        "livekit::rtc_engine::rtc_session",
        "livekit_protocol",
        "webrtc_sys",
        "libwebrtc",
        "wry",
        "tao",
        "hyper",
        "rustls",
    ];

    const ADM_TARGET: &str = "livekit::platform_audio";

    #[test]
    fn resolve_log_filter_keeps_decisive_adm_lines_under_a_default_launch() {
        // Direction 1 -- the whole point of #788: no `RUST_LOG` at all, which
        // is what every real GUI launch has.
        let resolved = resolve_log_filter(None);
        assert!(
            enabled(&resolved.filter, log::Level::Info, ADM_TARGET),
            "the ADM acquire/enable/release lines must survive the default filter"
        );
        // The ceiling handed to `log::set_max_level` must still admit them --
        // an `info` record is dropped by the facade before the filter ever
        // sees it if `max_level` is lower.
        assert!(resolved.max_level >= log::LevelFilter::Info);

        // Direction 2 -- the denylist still does its job. If this carve-out
        // had been implemented by dropping `livekit`/`webrtc_sys` from
        // `NOISY_THIRD_PARTY_CRATES`, or by widening the global level, these
        // assertions are what would catch it.
        for noisy in NOISY_TARGET_SAMPLES {
            assert!(
                !enabled(&resolved.filter, log::Level::Info, noisy),
                "{noisy} must stay denylisted to warn under the default filter"
            );
            assert!(
                !enabled(&resolved.filter, log::Level::Debug, noisy),
                "{noisy} must stay denylisted to warn under the default filter"
            );
            assert!(
                enabled(&resolved.filter, log::Level::Warn, noisy),
                "{noisy} must still report warnings"
            );
        }
        // Not a blanket "anything under livekit:: is fine" hole either: a
        // sibling module of the carve-out stays suppressed.
        assert!(!enabled(
            &resolved.filter,
            log::Level::Info,
            "livekit::rtc_engine"
        ));
    }

    #[test]
    fn resolve_log_filter_carve_out_survives_a_noisier_global_level() {
        // `RUST_LOG=debug`/`trace` is where the denylist matters most, so the
        // carve-out has to keep working there -- and must NOT widen into the
        // SDK's debug chatter while doing it.
        for spec in ["debug", "trace"] {
            let resolved = resolve_log_filter(Some(spec));
            assert!(
                enabled(&resolved.filter, log::Level::Info, ADM_TARGET),
                "RUST_LOG={spec} must still show the ADM lifecycle lines"
            );
            assert!(
                !enabled(&resolved.filter, log::Level::Debug, ADM_TARGET),
                "RUST_LOG={spec} must not re-admit SDK debug chatter through the carve-out"
            );
            for noisy in NOISY_TARGET_SAMPLES {
                assert!(
                    !enabled(&resolved.filter, log::Level::Info, noisy),
                    "RUST_LOG={spec}: {noisy} must stay denylisted"
                );
            }
        }
    }

    #[test]
    fn resolve_log_filter_carve_out_stands_down_for_an_explicit_user_directive() {
        // A developer who asks for more than the carve-out grants must get it:
        // `env_filter` matches the LONGEST directive name, so an
        // unconditionally-inserted `livekit::platform_audio=info` would
        // outrank `livekit=trace` instead of deferring to it. This is the
        // regression that `spec_has_directive_covering` exists to prevent.
        let resolved = resolve_log_filter(Some("info,livekit=trace"));
        assert!(resolved.parse_warning.is_none());
        assert!(
            enabled(&resolved.filter, log::Level::Trace, ADM_TARGET),
            "an explicit `livekit=trace` must win over our `info` carve-out"
        );

        // ...including in the quieting direction: an explicit `off` for the
        // carved-out target itself must actually silence it.
        let resolved = resolve_log_filter(Some("info,livekit::platform_audio=off"));
        assert!(resolved.parse_warning.is_none());
        assert!(!enabled(&resolved.filter, log::Level::Error, ADM_TARGET));
    }

    #[test]
    fn resolve_log_filter_carve_out_is_skipped_when_the_denylist_is() {
        // A user who explicitly asks for `warn` globally asked for quiet. The
        // carve-out exists to poke a hole in the denylist, so it applies
        // exactly when the denylist does -- never louder than what was asked.
        let resolved = resolve_log_filter(Some("warn"));
        assert!(!enabled(&resolved.filter, log::Level::Info, ADM_TARGET));
        assert!(enabled(&resolved.filter, log::Level::Warn, ADM_TARGET));
    }

    #[test]
    fn spec_has_directive_covering_matches_env_filter_prefix_semantics() {
        // A bare level token is the global fallback, not a per-target
        // directive -- it must NOT stand the carve-out down.
        assert!(!spec_has_directive_covering("info", ADM_TARGET));
        assert!(!spec_has_directive_covering("", ADM_TARGET));
        assert!(!spec_has_directive_covering(
            "info,desktop::session=debug",
            ADM_TARGET
        ));
        // Prefix match, exactly like `env_filter`'s own target matching.
        assert!(spec_has_directive_covering(
            "info,livekit=debug",
            ADM_TARGET
        ));
        assert!(spec_has_directive_covering(
            "livekit::platform_audio=off",
            ADM_TARGET
        ));
        // `RUST_LOG=livekit` (no `=`) means livekit at max level -- still a
        // per-target directive, not a global one.
        assert!(spec_has_directive_covering("livekit", ADM_TARGET));
        // The `/regex` message-filter tail is not part of the directive list.
        assert!(!spec_has_directive_covering("info/livekit", ADM_TARGET));
    }

    #[test]
    fn decisive_third_party_targets_are_once_per_session_in_the_vendored_sdk() {
        // The carve-out is a TARGET-NAME match against a crate we vendor, so
        // it dies SILENTLY if a vendor bump renames the crate, moves the
        // module, or demotes these lines to `debug` -- exactly the rot a
        // `warn!` promotion inside `vendor/` would suffer in reverse. Pin the
        // three things the target string is derived from, so that bump fails
        // here instead of quietly deleting the only evidence #787 leaves
        // behind.
        // Tie the pin to the constant it protects: renaming the carve-out
        // without re-checking the vendored source must not pass silently.
        assert!(
            DECISIVE_THIRD_PARTY_TARGETS.contains(&ADM_TARGET),
            "this test pins `{ADM_TARGET}`; DECISIVE_THIRD_PARTY_TARGETS now says \
             {DECISIVE_THIRD_PARTY_TARGETS:?}"
        );

        let vendor = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vendor")
            .join("livekit");

        // 1. The crate name -- the first segment of the target.
        let manifest = std::fs::read_to_string(vendor.join("Cargo.toml"))
            .expect("vendored livekit Cargo.toml (pinned by [patch.crates-io])");
        assert!(
            manifest.contains("name = \"livekit\""),
            "DECISIVE_THIRD_PARTY_TARGETS assumes the vendored crate is named `livekit`"
        );

        // 2. The module path -- the rest of the target. `log::info!` defaults
        //    its target to `module_path!()`, so `src/platform_audio/mod.rs` in
        //    crate `livekit` is `livekit::platform_audio`.
        let module = vendor.join("src").join("platform_audio").join("mod.rs");
        let source = std::fs::read_to_string(&module).unwrap_or_else(|e| {
            panic!(
                "{} is the source of the `{ADM_TARGET}` log target carved out of \
                 NOISY_THIRD_PARTY_CRATES; could not read it ({e})",
                module.display()
            )
        });

        // 3. The decisive messages, still emitted at a level the carve-out
        //    admits. A message demoted to `debug` upstream would leave the
        //    directive in place and still produce an empty log.
        for needle in [
            "PlatformAudio: acquired Platform ADM",
            "PlatformAudio: enabled ADM playout for platform speakers",
            "recording devices, ",
            "PlatformAdmHandle: released Platform ADM",
        ] {
            let at = source
                .find(needle)
                .unwrap_or_else(|| panic!("{} no longer logs {needle:?}", module.display()));
            let macro_call = source[..at]
                .rfind("log::")
                .map(|start| &source[start..])
                .unwrap_or("");
            assert!(
                macro_call.starts_with("log::info!")
                    || macro_call.starts_with("log::warn!")
                    || macro_call.starts_with("log::error!"),
                "{needle:?} must still be emitted at info or louder for the \
                 `{ADM_TARGET}` carve-out to surface it; found {:?}",
                macro_call.split('(').next().unwrap_or(macro_call)
            );
        }

        // And the reason a whole-module carve-out is safe: nothing in it logs
        // per frame. `info!` call sites here are ADM lifecycle only, so a
        // future one that isn't should force a re-read of this decision.
        let info_sites = source.matches("log::info!").count();
        assert!(
            info_sites <= 10,
            "{} now has {info_sites} info-level log sites; re-check that none \
             are per-frame before leaving `{ADM_TARGET}` carved out of the \
             noisy-crate denylist (#788)",
            module.display()
        );
    }

    /// #787: `adm_proxy.cpp` logs its playout failures with
    /// `RTC_LOG(LS_ERROR)` -- the whole point being that a meeting nobody can
    /// hear should name its own mechanism in `petal.log` and in Sentry. Those
    /// lines reach Rust through exactly one place: the process-wide sink
    /// `PeerConnectionFactory::default()` installs. That sink used to throw
    /// the severity away and emit everything at `debug` on target
    /// `libwebrtc`, which the denylist above caps at `warn` -- so not one of
    /// them could ever appear. Measured, not assumed: a real 8.5 MB
    /// `petal.log` spanning many sessions contained zero `libwebrtc` records
    /// and zero `AdmProxy` lines.
    ///
    /// Three separable hops have to hold for that evidence to exist, so all
    /// three are asserted here.
    #[test]
    fn native_webrtc_error_lines_survive_the_default_filter() {
        use webrtc_sys::webrtc::ffi::LoggingSeverity;

        // Hop 1 -- the real mapping function the installed sink calls (not a
        // copy of it; re-exported from the vendored crate for this test).
        assert_eq!(
            livekit::webrtc::native::webrtc_log_level(LoggingSeverity::Error),
            log::Level::Error,
            "an RTC_LOG(LS_ERROR) must arrive at Rust as an error record -- \
             Sentry maps Error to an event and Warn only to a breadcrumb"
        );
        assert_eq!(
            livekit::webrtc::native::webrtc_log_level(LoggingSeverity::Warning),
            log::Level::Warn
        );
        // ...and not by simply making everything loud: the per-packet
        // severities must stay under the default filter.
        assert_eq!(
            livekit::webrtc::native::webrtc_log_level(LoggingSeverity::Info),
            log::Level::Debug
        );
        assert_eq!(
            livekit::webrtc::native::webrtc_log_level(LoggingSeverity::Verbose),
            log::Level::Trace
        );

        // Hop 2 -- the filter a real GUI launch actually runs with. `libwebrtc`
        // stays denylisted; the point is that `warn` admits `error`.
        let resolved = resolve_log_filter(None);
        assert!(
            NOISY_THIRD_PARTY_CRATES.contains(&"libwebrtc"),
            "this test assumes `libwebrtc` is denylisted to warn, which is what \
             makes hop 1 the load-bearing part"
        );
        assert!(
            enabled(&resolved.filter, log::Level::Error, "libwebrtc"),
            "native WebRTC errors must survive the default filter"
        );
        assert!(enabled(&resolved.filter, log::Level::Warn, "libwebrtc"));
        assert!(resolved.max_level >= log::LevelFilter::Warn);
        assert!(
            !enabled(&resolved.filter, log::Level::Debug, "libwebrtc"),
            "the denylist must still swallow native WebRTC's per-packet chatter"
        );

        // Hop 3 -- the two ends the assertions above cannot see: that the
        // installed sink is wired to that mapping at all, and that the C++ we
        // compile ourselves still emits #787's failure at LS_ERROR. Either
        // one silently reverting on a vendor bump puts the log back to empty.
        let vendor = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("vendor");

        let sink_source = vendor.join("libwebrtc").join("src").join("native").join("peer_connection_factory.rs");
        let sink = std::fs::read_to_string(&sink_source)
            .unwrap_or_else(|e| panic!("{} installs the only native WebRTC log sink; could not read it ({e})", sink_source.display()));
        assert!(
            sink.contains("new_log_sink(emit_webrtc_log)"),
            "{} must install the severity-mapping sink; upstream's form discards \
             the severity argument and logs everything at debug (#787)",
            sink_source.display()
        );
        assert!(
            !sink.contains("log::debug!(target: \"libwebrtc\""),
            "{} is back to logging every native line at debug (#787)",
            sink_source.display()
        );

        let adm_source = vendor.join("webrtc-sys").join("src").join("adm_proxy.cpp");
        let adm = std::fs::read_to_string(&adm_source)
            .unwrap_or_else(|e| panic!("{} is #787's evidence source; could not read it ({e})", adm_source.display()));
        for needle in [
            "platform playout could not be started",
            "platform StartPlayout failed",
        ] {
            let at = adm
                .find(needle)
                .unwrap_or_else(|| panic!("{} no longer logs {needle:?}", adm_source.display()));
            let macro_call = adm[..at].rfind("RTC_LOG(").map(|start| &adm[start..]).unwrap_or("");
            assert!(
                macro_call.starts_with("RTC_LOG(LS_ERROR)"),
                "{needle:?} must stay at LS_ERROR -- LS_WARNING is a Sentry \
                 breadcrumb, not an event; found {:?}",
                macro_call.split('<').next().unwrap_or(macro_call)
            );
        }
    }

    #[test]
    fn bare_global_level_ignores_module_scoped_tokens() {
        assert_eq!(
            bare_global_level("info,desktop::remote_control=debug"),
            Some(log::LevelFilter::Info)
        );
        assert_eq!(bare_global_level("desktop::remote_control=debug"), None);
        assert_eq!(bare_global_level("warn"), Some(log::LevelFilter::Warn));
        assert_eq!(bare_global_level(""), None);
    }

    /// Small helper so the tests above read as "is LEVEL enabled for TARGET"
    /// rather than constructing a `log::Metadata` by hand at every call site.
    fn enabled(filter: &env_filter::Filter, level: log::Level, target: &str) -> bool {
        filter.enabled(&log::Metadata::builder().level(level).target(target).build())
    }

    #[cfg(windows)]
    #[test]
    fn windows_log_dir_uses_appdata_petal_logs_layout() {
        let appdata = PathBuf::from(r"C:\Users\Alice\AppData\Roaming");
        assert_eq!(
            windows_log_dir_from_appdata(appdata.clone()),
            appdata.join("Petal").join("logs")
        );
    }

    #[test]
    fn crash_reports_since_finds_only_newer_desktop_ips_files() {
        let dir = temp_dir("newer");
        let threshold = SystemTime::now() - Duration::from_secs(60);
        let hit = touch(&dir, "desktop-2026-07-01-201927.ips");
        // Wrong process name, wrong extension -- both must be ignored even
        // though they're newer than the threshold.
        touch(&dir, "Safari-2026-07-01-201927.ips");
        touch(&dir, "desktop-2026-07-01-201927.crash");

        let found = crash_reports_since(&dir, threshold);
        assert_eq!(found, vec![hit]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_reports_since_ignores_files_older_than_threshold() {
        let dir = temp_dir("older");
        touch(&dir, "desktop-2026-06-30-000000.ips");
        // Threshold in the future -> every just-written file is "older".
        let threshold = SystemTime::now() + Duration::from_secs(3600);
        assert!(crash_reports_since(&dir, threshold).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_reports_since_is_empty_and_nonfatal_for_missing_dir() {
        let dir = std::env::temp_dir().join("petal-logging-test-definitely-missing-dir");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(crash_reports_since(&dir, SystemTime::UNIX_EPOCH).is_empty());
    }

    #[test]
    fn crash_reports_since_returns_sorted_paths() {
        let dir = temp_dir("sorted");
        let threshold = SystemTime::now() - Duration::from_secs(60);
        let b = touch(&dir, "desktop-b.ips");
        let a = touch(&dir, "desktop-a.ips");
        assert_eq!(crash_reports_since(&dir, threshold), vec![a, b]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // #878: vanished-session detector -- did the previous run's log tail
    // show a room join with no later shutdown marker?

    #[test]
    fn read_log_tail_lines_missing_file_is_empty() {
        let dir = temp_dir("tail-missing");
        assert!(read_log_tail_lines(&dir.join("nope.log"), 300).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_log_tail_lines_caps_at_max_lines() {
        let dir = temp_dir("tail-cap");
        let path = dir.join("petal.log");
        let content: String = (0..500).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, content).unwrap();
        let tail = read_log_tail_lines(&path, 300);
        assert_eq!(tail.len(), 300);
        assert_eq!(tail.first().unwrap(), "line 200");
        assert_eq!(tail.last().unwrap(), "line 499");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_log_tail_lines_decompresses_a_gz_path() {
        // #905 review Finding 4: `resolve_current_or_latest_log_file` can
        // now return a `.gz` -- its content must be decompressed before
        // taking the tail, not read as if it were already plaintext.
        let dir = temp_dir("tail-gz");
        let path = dir.join("petal.log.2026-08-20.gz");
        let content: String = (0..10).map(|n| format!("line {n}\n")).collect();
        let mut gz_bytes = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::default());
            encoder.write_all(content.as_bytes()).unwrap();
            encoder.finish().unwrap();
        }
        std::fs::write(&path, &gz_bytes).unwrap();

        let tail = read_log_tail_lines(&path, 300);
        assert_eq!(tail.len(), 10);
        assert_eq!(tail.first().unwrap(), "line 0");
        assert_eq!(tail.last().unwrap(), "line 9");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn vanished_session_verdict(lines: &[&str], crash_report_found: bool) -> VanishedSessionVerdict {
        let owned: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        detect_vanished_session(&owned, crash_report_found)
    }

    #[test]
    fn vanished_session_clean_shutdown_via_left_room() {
        let lines = [
            "session: joined room ops",
            "some other activity",
            "session: left room ops",
        ];
        assert_eq!(
            vanished_session_verdict(&lines, false),
            VanishedSessionVerdict::CleanShutdown
        );
    }

    #[test]
    fn vanished_session_clean_shutdown_via_quit_app() {
        let lines = ["session: joined room ops", "quit: quit_app invoked"];
        assert_eq!(
            vanished_session_verdict(&lines, false),
            VanishedSessionVerdict::CleanShutdown
        );
    }

    #[test]
    fn vanished_session_clean_shutdown_via_journal_loop_stopped() {
        let lines = ["session: joined room ops", "event journal loop stopped"];
        assert_eq!(
            vanished_session_verdict(&lines, false),
            VanishedSessionVerdict::CleanShutdown
        );
    }

    #[test]
    fn vanished_session_no_join_is_clean() {
        let lines = ["app started", "nothing happened"];
        assert_eq!(
            vanished_session_verdict(&lines, false),
            VanishedSessionVerdict::CleanShutdown
        );
    }

    #[test]
    fn vanished_session_truncated_in_room_with_no_crash_report() {
        let lines = [
            "session: joined room ops",
            "camera publish health -- capture_fps=30.0",
        ];
        assert_eq!(
            vanished_session_verdict(&lines, false),
            VanishedSessionVerdict::VanishedNoCrashReport
        );
    }

    #[test]
    fn vanished_session_truncated_in_room_with_crash_report_is_distinguishable() {
        let lines = [
            "session: joined room ops",
            "camera publish health -- capture_fps=30.0",
        ];
        let with_report = vanished_session_verdict(&lines, true);
        let without_report = vanished_session_verdict(&lines, false);
        assert_eq!(with_report, VanishedSessionVerdict::VanishedWithCrashReport);
        assert_ne!(
            with_report, without_report,
            "the crash-report and no-crash-report verdicts must differ"
        );
    }

    #[test]
    fn vanished_session_uses_the_last_join_not_the_first() {
        // Two joins: the first was left cleanly, the second (most recent)
        // was not -- the verdict must track the LAST join.
        let lines = [
            "session: joined room ops",
            "session: left room ops",
            "session: joined room ops",
        ];
        assert_eq!(
            vanished_session_verdict(&lines, false),
            VanishedSessionVerdict::VanishedNoCrashReport
        );
    }

    #[test]
    fn vanished_session_detected_when_the_join_scrolled_out_of_the_tail() {
        // #882 review: a >24min meeting pushes "session: joined room" out of
        // the 300-line tail; the periodic in-meeting health lines are then
        // the only evidence. Two of the three #878 field deaths look exactly
        // like this fixture -- the join-only detector verdicted them clean.
        let lines = [
            "compositor feed: window 1073741830 receiver frame health from 'peer' -- frames=100 compositor_fps=2.0 gap_since_last_frame_ms=1374 pixbufs=0",
            "session: camera publish health -- captured=120 pushed=118 dropped_push=1 overwritten_latest=2 capture_fps=30.0 encode_fps=29.4",
            "compositor feed: window 1073741830 receiver frame health from 'peer' -- frames=103 compositor_fps=0.3 gap_since_last_frame_ms=6431 pixbufs=0",
        ];
        assert_eq!(
            vanished_session_verdict(&lines, false),
            VanishedSessionVerdict::VanishedNoCrashReport
        );
    }

    #[test]
    fn vanished_session_clean_when_activity_precedes_a_leave_and_quit() {
        // In-meeting activity followed by a real leave + quit is a clean
        // shutdown even with the join line long out of the tail.
        let lines = [
            "compositor feed: window 42 receiver frame health from 'peer' -- frames=1 compositor_fps=30.0 gap_since_last_frame_ms=33 pixbufs=1",
            "session: left room 'ops' via user",
            "quit: quit_app command -- exiting(0)",
        ];
        assert_eq!(
            vanished_session_verdict(&lines, false),
            VanishedSessionVerdict::CleanShutdown
        );
    }

    #[test]
    fn oversized_log_rotates_and_prunes_old_rotations() {
        let dir = temp_dir("rotate");
        let log = dir.join("petal.log");
        std::fs::write(&log, b"0123456789").unwrap();
        std::fs::write(dir.join("petal-0001.log"), b"old").unwrap();
        std::fs::write(dir.join("petal-0002.log"), b"old").unwrap();

        // #905 review Finding 3 split this into two calls (fast rename,
        // then the slow prune) so a hot-path caller can run only the fast
        // half synchronously -- exercise both together here, matching the
        // pre-split `rotate_log_if_needed`'s net behavior exactly.
        rename_oversized_log(&log, 4);
        prune_rotated_logs(&log, 2);

        assert!(!log.exists(), "oversized active log should be renamed");
        let mut rotated: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("petal-") && name.ends_with(".log"))
            .collect();
        rotated.sort();
        assert_eq!(rotated.len(), 2, "retains only the newest rotated logs");
        assert!(!rotated.contains(&"petal-0001.log".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_log_is_not_rotated_but_old_rotations_are_pruned() {
        let dir = temp_dir("prune");
        let log = dir.join("petal.log");
        std::fs::write(&log, b"ok").unwrap();
        std::fs::write(dir.join("petal-0001.log"), b"old").unwrap();
        std::fs::write(dir.join("petal-0002.log"), b"old").unwrap();
        std::fs::write(dir.join("petal-0003.log"), b"old").unwrap();

        rename_oversized_log(&log, 4);
        prune_rotated_logs(&log, 2);

        assert!(log.exists(), "small active log should stay active");
        assert!(!dir.join("petal-0001.log").exists());
        assert!(dir.join("petal-0002.log").exists());
        assert!(dir.join("petal-0003.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- #905: per-day rolling, gzip-on-roll, same-day backstop, retention --

    #[test]
    fn daily_log_write_rolls_to_a_new_file_on_date_change_without_a_restart() {
        let dir = temp_dir("daily-roll");
        // Start already "on" the first synthetic date so the very first
        // write doesn't spuriously trigger a roll against whatever the
        // REAL today happens to be.
        let mut state = DailyLogState {
            date: "2026-08-31".to_string(),
            file: None,
            bytes_written: 0,
        };
        let mut rolled: Vec<PathBuf> = Vec::new();
        let mut pruned: Vec<PathBuf> = Vec::new();

        daily_log_write(
            &dir,
            &mut state,
            "2026-08-31".to_string(),
            "line one\n",
            |p| rolled.push(p),
            |p| pruned.push(p),
        );
        daily_log_write(
            &dir,
            &mut state,
            "2026-08-31".to_string(),
            "line two\n",
            |p| rolled.push(p),
            |p| pruned.push(p),
        );
        // The UTC date boundary crosses mid-session, with no restart.
        daily_log_write(
            &dir,
            &mut state,
            "2026-09-01".to_string(),
            "line three\n",
            |p| rolled.push(p),
            |p| pruned.push(p),
        );

        assert_eq!(rolled, vec![dir.join("petal.log.2026-08-31")]);
        assert!(pruned.is_empty(), "a date-change roll must never trigger the backstop's prune callback");
        assert_eq!(
            std::fs::read_to_string(dir.join("petal.log.2026-08-31")).unwrap(),
            "line one\nline two\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("petal.log.2026-09-01")).unwrap(),
            "line three\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daily_log_write_backstop_rotates_when_the_same_day_file_grows_too_large() {
        let dir = temp_dir("daily-backstop");
        let mut state = DailyLogState {
            date: "2026-08-27".to_string(),
            file: None,
            bytes_written: 0,
        };
        let mut rolled: Vec<PathBuf> = Vec::new();
        let mut pruned: Vec<PathBuf> = Vec::new();
        let big_line = format!("{}\n", "x".repeat((SAME_DAY_SIZE_BACKSTOP_BYTES as usize) + 1));

        daily_log_write(
            &dir,
            &mut state,
            "2026-08-27".to_string(),
            &big_line,
            |p| rolled.push(p),
            |p| pruned.push(p),
        );
        // The backstop trips on the NEXT write, once bytes_written already
        // exceeds the threshold.
        daily_log_write(
            &dir,
            &mut state,
            "2026-08-27".to_string(),
            "small\n",
            |p| rolled.push(p),
            |p| pruned.push(p),
        );

        // The same-day backstop reuses the legacy rotate mechanism for the
        // fast rename SYNCHRONOUSLY (unlike a real date-change roll, it
        // never calls `on_roll`) but hands the slow prune-by-count sweep to
        // `on_backstop_prune` instead of running it inline (#905 review
        // Finding 3) -- this test only proves the callback fires with the
        // right path; the sweep itself is `prune_rotated_logs`'s own
        // responsibility, covered by its existing tests.
        assert!(rolled.is_empty());
        assert_eq!(pruned, vec![dir.join("petal.log.2026-08-27")]);
        let legacy: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("petal-") && n.ends_with(".log"))
            .collect();
        assert_eq!(
            legacy.len(),
            1,
            "the oversized same-day file should be rotated to a legacy-shaped file: found {legacy:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("petal.log.2026-08-27")).unwrap(),
            "small\n",
            "a fresh file should pick up right after the backstop rotation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gzip_and_remove_compresses_content_and_deletes_the_original() {
        let dir = temp_dir("gzip");
        let path = dir.join("petal.log.2026-08-20");
        std::fs::write(&path, b"hello from a completed day\n").unwrap();

        gzip_and_remove(&path);

        assert!(
            !path.exists(),
            "original plaintext file must be removed after a successful gzip"
        );
        let gz_path = dir.join("petal.log.2026-08-20.gz");
        assert!(gz_path.exists());
        let compressed = std::fs::read(&gz_path).unwrap();
        let mut decoder = flate2::read::MultiGzDecoder::new(&compressed[..]);
        let mut out = String::new();
        decoder.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello from a completed day\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gzip_and_remove_is_a_safe_no_op_when_the_source_is_already_gone() {
        // Simulates the true-race case: a caller wins the `GZIP_LOCK`,
        // compresses+removes the source, and a second caller (which had
        // already decided to gzip the same path before either acquired the
        // lock) proceeds AFTER -- it must re-check under the lock and do
        // nothing, not error or duplicate content.
        let dir = temp_dir("gzip-race-gone");
        let path = dir.join("petal.log.2026-08-20");
        std::fs::write(&path, b"only copy\n").unwrap();

        gzip_and_remove(&path);
        assert!(!path.exists());
        let gz_path = dir.join("petal.log.2026-08-20.gz");
        let after_first = std::fs::read(&gz_path).unwrap();

        // The "second caller": the source is already gone.
        gzip_and_remove(&path);
        assert_eq!(
            std::fs::read(&gz_path).unwrap(),
            after_first,
            "a no-op second call must not touch the already-finished .gz"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gzip_and_remove_appends_rather_than_overwrites_on_a_clock_revisit() {
        // #905 review Finding 5: if the clock returns to a date that
        // already has a completed `.gz` (NTP correction, manual clock
        // change), a fresh plaintext file is opened for it and MUST NOT be
        // silently discarded when it later rolls off, just because a `.gz`
        // already happens to exist at that name.
        let dir = temp_dir("gzip-clock-revisit");
        let path = dir.join("petal.log.2026-08-20");
        let gz_path = dir.join("petal.log.2026-08-20.gz");

        std::fs::write(&path, b"first visit content\n").unwrap();
        gzip_and_remove(&path);
        assert!(!path.exists());
        assert!(gz_path.exists());

        // The clock revisits 2026-08-20: a fresh plaintext file is opened
        // and written under the SAME name.
        std::fs::write(&path, b"second visit content\n").unwrap();
        gzip_and_remove(&path);
        assert!(!path.exists());

        let compressed = std::fs::read(&gz_path).unwrap();
        let mut decoder = flate2::read::MultiGzDecoder::new(&compressed[..]);
        let mut out = String::new();
        decoder.read_to_string(&mut out).unwrap();
        assert_eq!(
            out, "first visit content\nsecond visit content\n",
            "neither visit's content may be lost: {out:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daily_log_date_from_name_extracts_a_valid_date_and_rejects_everything_else() {
        assert_eq!(daily_log_date_from_name("petal.log.2026-09-02"), Some("2026-09-02"));
        assert_eq!(
            daily_log_date_from_name("petal.log.2026-09-02.gz"),
            Some("2026-09-02")
        );
        assert_eq!(daily_log_date_from_name("petal.log"), None);
        assert_eq!(daily_log_date_from_name("petal.log.gz"), None);
        assert_eq!(daily_log_date_from_name("petal-20260902-154132676.log"), None);
        assert_eq!(daily_log_date_from_name("petal.log.notadate"), None);
        // #905 review Finding 1: a partial/orphaned compression temp file
        // (left behind by a killed process, or read mid-write by a racing
        // caller) must NEVER match -- a 10-char PREFIX match previously let
        // this slip through as an ordinary (mis-detected-as-plaintext)
        // daily log.
        assert_eq!(
            daily_log_date_from_name("petal.log.2026-09-02.gz.tmp-12345"),
            None
        );
        assert_eq!(
            daily_log_date_from_name("petal.log.2026-09-02.tmp-12345"),
            None
        );
        assert_eq!(daily_log_date_from_name("petal.log.2026-09-02extra"), None);
    }

    #[test]
    fn prune_old_logs_removes_past_the_age_window_but_keeps_recent_and_today() {
        let dir = temp_dir("prune-daily");
        let today = today_utc_string();
        let old_date = (chrono::Utc::now() - chrono::Duration::days(30))
            .format("%Y-%m-%d")
            .to_string();
        let recent_date = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();

        touch(&dir, &daily_log_file_name(&old_date));
        touch(&dir, &format!("{}.gz", daily_log_file_name(&old_date)));
        touch(&dir, &daily_log_file_name(&recent_date));
        touch(&dir, &daily_log_file_name(&today));
        let legacy_old = touch(&dir, "petal-old.log");
        set_mtime(
            &legacy_old,
            SystemTime::now() - Duration::from_secs(40 * 86_400),
        );

        prune_old_logs(&dir, MAX_LOG_AGE_DAYS);

        assert!(!dir.join(daily_log_file_name(&old_date)).exists());
        assert!(!dir.join(format!("{}.gz", daily_log_file_name(&old_date))).exists());
        assert!(dir.join(daily_log_file_name(&recent_date)).exists());
        assert!(
            dir.join(daily_log_file_name(&today)).exists(),
            "today's active file must never be pruned"
        );
        assert!(
            !legacy_old.exists(),
            "an ancient legacy file (judged by mtime) must be pruned too"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_current_or_latest_log_file_picks_the_newest_plaintext_when_it_is_newest() {
        let dir = temp_dir("resolve-latest");
        let base = SystemTime::now();
        let older_gz = touch(&dir, "petal.log.2026-08-30.gz");
        set_mtime(&older_gz, base - Duration::from_secs(100));
        let newer_plain = touch(&dir, "petal.log.2026-08-31");
        set_mtime(&newer_plain, base);
        assert_eq!(resolve_current_or_latest_log_file(&dir), Some(newer_plain));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_current_or_latest_log_file_considers_gz_files_when_they_are_newest() {
        // #905 review Finding 4: a `.gz` can legitimately be the best
        // available evidence of a previous session (e.g. a full relaunch
        // racing an outgoing process's teardown, or a session whose own
        // startup sweep gzip'd its predecessor's file right before it
        // died) -- it must not be unconditionally excluded.
        let dir = temp_dir("resolve-latest-gz");
        let base = SystemTime::now();
        let older_plain = touch(&dir, "petal.log.2026-08-30");
        set_mtime(&older_plain, base - Duration::from_secs(100));
        let newer_gz = touch(&dir, "petal.log.2026-08-31.gz");
        set_mtime(&newer_gz, base);
        assert_eq!(resolve_current_or_latest_log_file(&dir), Some(newer_gz));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_current_or_latest_log_file_falls_back_to_a_legacy_bare_petal_log() {
        let dir = temp_dir("resolve-legacy");
        let legacy = touch(&dir, "petal.log");
        assert_eq!(resolve_current_or_latest_log_file(&dir), Some(legacy));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_current_or_latest_log_file_is_none_on_a_first_ever_launch() {
        let dir = temp_dir("resolve-empty");
        assert_eq!(resolve_current_or_latest_log_file(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detectors_resolve_yesterdays_file_across_a_date_boundary_not_an_empty_new_file() {
        // #905 trap: pointing the previous-session detectors at a
        // hardcoded `petal.log.<today>` would find nothing on the first
        // launch of a new UTC day (that file doesn't exist yet) and
        // silently conclude a clean shutdown / 24h fallback every single
        // morning. They must resolve the newest EXISTING file instead.
        let dir = temp_dir("detector-boundary");
        std::fs::write(dir.join("petal.log.2026-08-31"), "session: joined room 'ops'\n").unwrap();
        // "Today" (2026-09-01) hasn't started writing yet -- no such file
        // exists at this point, exactly like right after a fresh UTC
        // midnight boot.

        let resolved =
            resolve_current_or_latest_log_file(&dir).expect("must resolve yesterday's file");
        assert_eq!(resolved, dir.join("petal.log.2026-08-31"));

        let tail = read_log_tail_lines(&resolved, VANISHED_SESSION_TAIL_LINES);
        assert_eq!(tail, vec!["session: joined room 'ops'".to_string()]);
        assert_eq!(
            detect_vanished_session(&tail, false),
            VanishedSessionVerdict::VanishedNoCrashReport,
            "a join with no shutdown marker on yesterday's file must still be detected as vanished"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn feedback_attachment_zip_spans_the_two_most_recent_daily_files() {
        let dir = temp_dir("feedback-span");
        std::fs::write(dir.join("petal.log.2026-09-01"), "yesterday's line\n").unwrap();
        let today_path = dir.join("petal.log.2026-09-02");
        std::fs::write(&today_path, "today's line\n").unwrap();

        let bytes = build_feedback_attachment_zip_from(&today_path).unwrap();
        let mut zip = ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut content = String::new();
        zip.by_name("petal.log")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(content.contains("yesterday's line"));
        assert!(content.contains("today's line"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- #905: native-WebRTC repeat suppression --------------------------

    #[derive(Clone)]
    struct RecordingLog {
        lines: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl RecordingLog {
        fn new() -> Self {
            RecordingLog {
                lines: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl log::Log for RecordingLog {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            self.lines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(record.args().to_string());
        }
        fn flush(&self) {}
    }

    fn log_via(wrapped: &RepeatSuppressingLog<RecordingLog>, target: &str, level: log::Level, msg: &str) {
        wrapped.log(
            &log::Record::builder()
                .args(format_args!("{msg}"))
                .level(level)
                .target(target)
                .build(),
        );
    }

    #[test]
    fn repeat_suppressing_log_collapses_identical_consecutive_native_webrtc_lines() {
        let recorder = RecordingLog::new();
        let lines = recorder.lines.clone();
        let wrapped = RepeatSuppressingLog::new(recorder);
        let msg = "frame rate setting 30 is larger than the maximal allowed frame rate 13/23";

        log_via(&wrapped, "libwebrtc::encoder", log::Level::Warn, msg);
        log_via(&wrapped, "libwebrtc::encoder", log::Level::Warn, msg);
        log_via(&wrapped, "libwebrtc::encoder", log::Level::Warn, msg);

        let recorded = lines.lock().unwrap();
        assert_eq!(
            recorded.as_slice(),
            &[msg.to_string()],
            "only the first occurrence should be forwarded while the repeats are suppressed: {recorded:?}"
        );
    }

    #[test]
    fn repeat_suppressing_log_flushes_a_rollup_when_the_message_changes() {
        let recorder = RecordingLog::new();
        let lines = recorder.lines.clone();
        let wrapped = RepeatSuppressingLog::new(recorder);

        log_via(&wrapped, "libwebrtc", log::Level::Warn, "A");
        log_via(&wrapped, "libwebrtc", log::Level::Warn, "A");
        log_via(&wrapped, "libwebrtc", log::Level::Warn, "A");
        log_via(&wrapped, "libwebrtc", log::Level::Warn, "B");

        let recorded = lines.lock().unwrap();
        assert_eq!(
            recorded.len(),
            3,
            "expected: first A, a rollup of A's suppressed repeats, then B: {recorded:?}"
        );
        assert_eq!(recorded[0], "A");
        assert!(
            recorded[1].contains('A') && recorded[1].contains("repeated 2x"),
            "the tail of a streak must not be silently dropped when the message changes: {:?}",
            recorded[1]
        );
        assert_eq!(recorded[2], "B");
    }

    #[test]
    fn repeat_suppressing_log_never_touches_non_native_webrtc_targets() {
        let recorder = RecordingLog::new();
        let lines = recorder.lines.clone();
        let wrapped = RepeatSuppressingLog::new(recorder);

        log_via(&wrapped, "desktop_lib::session", log::Level::Info, "same");
        log_via(&wrapped, "desktop_lib::session", log::Level::Info, "same");
        log_via(&wrapped, "desktop_lib::session", log::Level::Info, "same");

        assert_eq!(
            lines.lock().unwrap().len(),
            3,
            "targets outside libwebrtc must never be suppressed"
        );
    }

    #[test]
    fn repeat_suppressing_log_does_not_conflate_identical_text_from_different_targets_or_levels() {
        // #905 review: keying on message text alone would incorrectly
        // treat identical text from two different libwebrtc sub-targets --
        // or the same target at two different levels -- as one streak.
        let recorder = RecordingLog::new();
        let lines = recorder.lines.clone();
        let wrapped = RepeatSuppressingLog::new(recorder);

        log_via(&wrapped, "libwebrtc::a", log::Level::Warn, "same text");
        log_via(&wrapped, "libwebrtc::b", log::Level::Warn, "same text");
        log_via(&wrapped, "libwebrtc::a", log::Level::Info, "same text");

        assert_eq!(
            lines.lock().unwrap().len(),
            3,
            "different (target, level) pairs must never share a streak even with identical text"
        );
    }

    #[test]
    fn repeat_suppressing_log_flushes_a_pending_rollup_instead_of_losing_it() {
        let recorder = RecordingLog::new();
        let lines = recorder.lines.clone();
        let wrapped = RepeatSuppressingLog::new(recorder);

        log_via(&wrapped, "libwebrtc", log::Level::Warn, "A");
        log_via(&wrapped, "libwebrtc", log::Level::Warn, "A");
        log_via(&wrapped, "libwebrtc", log::Level::Warn, "A");
        // No message change and no summary interval elapsed -- without an
        // explicit flush, the 2 suppressed repeats would never be reported.
        wrapped.flush();

        let recorded = lines.lock().unwrap();
        assert_eq!(recorded.len(), 2, "expected: first A, then a flush rollup: {recorded:?}");
        assert_eq!(recorded[0], "A");
        assert!(
            recorded[1].contains('A') && recorded[1].contains("repeated 2x"),
            "flush must report the pending suppressed count instead of silently dropping it: {:?}",
            recorded[1]
        );
    }

    #[test]
    fn redact_for_export_masks_room_and_identity_values_stably() {
        let raw = "session: join_room('eng-sync') begin (identity 'alice@example.com')";
        let redacted = redact_for_export(raw);
        assert!(!redacted.contains("eng-sync"));
        assert!(!redacted.contains("alice@example.com"));
        assert!(redacted.contains("join_room('<redacted:"));
        assert!(redacted.contains("identity '<redacted:"));
        assert_eq!(redacted, redact_for_export(raw));
    }

    // -- #292 adversarial review: real leak classes found in a live
    // `petal.log` that the original marker set did not cover --------------

    #[test]
    fn redact_for_export_masks_window_titles_and_owners_from_window_stack_lines() {
        let raw = "window-stack: z=3 id=4675 owner='Google Chrome' name='Inbox - alice@example.com - Mail' layer=0 alpha=1.00 bounds=(0,0 1512x944)";
        let redacted = redact_for_export(raw);
        assert!(!redacted.contains("Google Chrome"));
        assert!(!redacted.contains("alice@example.com"));
        assert!(!redacted.contains("Inbox"));
        assert!(redacted.contains("owner='<redacted:"));
        assert!(redacted.contains("name='<redacted:"));
    }

    #[test]
    fn redact_for_export_masks_presence_names_and_identities_together() {
        let raw = "presence: 'Bob' (web-cd91512f-aaaa-bbbb-cccc-dddddddddddd) joined 'eng-standup'";
        let redacted = redact_for_export(raw);
        assert!(!redacted.contains("Bob"));
        assert!(!redacted.contains("web-cd91512f"));
        assert!(!redacted.contains("eng-standup"));
        assert!(redacted.contains("presence: '<redacted:"));
        assert!(redacted.contains(") joined '<redacted:"));
        // The identity in parens must be a DIFFERENT label than the name --
        // proves it was actually matched and redacted, not just dropped.
        let name_label_start =
            redacted.find("presence: '<redacted:").unwrap() + "presence: '".len();
        let identity_label_start = redacted.find(" (<redacted:").unwrap() + " (".len();
        assert_ne!(
            &redacted[name_label_start..name_label_start + 18],
            &redacted[identity_label_start..identity_label_start + 18]
        );
    }

    #[test]
    fn redact_for_export_masks_presence_connected_and_disconnected_for_variants() {
        let connected = "presence: ParticipantConnected for 'Alice' (web-1111) in 'eng-standup' but that identity is already in the roster -- duplicate identity join? (roster unchanged)";
        let redacted = redact_for_export(connected);
        assert!(!redacted.contains("Alice"));
        assert!(!redacted.contains("web-1111"));
        assert!(!redacted.contains("eng-standup"));

        let disconnected = "presence: ParticipantDisconnected for 'Carol' (web-2222) in 'eng-standup' but that identity was not in the roster (already removed, or never added -- roster unchanged)";
        let redacted2 = redact_for_export(disconnected);
        assert!(!redacted2.contains("Carol"));
        assert!(!redacted2.contains("web-2222"));
        assert!(!redacted2.contains("eng-standup"));
    }

    #[test]
    fn log_safe_quoted_replaces_ascii_quotes_and_leaves_everything_else_alone() {
        assert_eq!(log_safe_quoted("O'Brien"), "O\u{2019}Brien");
        assert_eq!(
            log_safe_quoted("Bob's tax return 2025.pdf"),
            "Bob\u{2019}s tax return 2025.pdf"
        );
        assert_eq!(log_safe_quoted("plain-name"), "plain-name");
        assert_eq!(log_safe_quoted("\"quoted\""), "\u{2019}quoted\u{2019}");
    }

    #[test]
    fn redact_for_export_masks_apostrophe_bearing_presence_name_and_identity() {
        // Fable review (#292, round 2): an unsanitized apostrophe in a
        // participant name let the value escape `redact_after_marker`'s
        // quote-delimited scan -- both the name's remainder AND the
        // following `(identity)` parenthetical leaked in clear. This
        // reproduces the exact failure shape with the fix (`log_safe_quoted`
        // applied at the presence.rs emission site) already in place: the
        // apostrophe never reaches the log line as an ASCII `'`, so the
        // scan's delimiter match is never fooled.
        let name = log_safe_quoted("O'Brien");
        let identity = log_safe_quoted("web-cd91512f-aaaa-bbbb-cccc-dddddddddddd");
        let room = log_safe_quoted("eng-standup");
        let raw = format!("presence: '{name}' ({identity}) joined '{room}'");
        let redacted = redact_for_export(&raw);
        assert!(
            !redacted.contains("Brien"),
            "the leaked remainder must not survive: {redacted}"
        );
        assert!(
            !redacted.contains("web-cd91512f"),
            "the identity that leaked via the broken parenthetical match must not survive: {redacted}"
        );
        assert!(!redacted.contains("eng-standup"));
        assert!(redacted.contains("presence: '<redacted:"));
    }

    #[test]
    fn redact_for_export_masks_apostrophe_bearing_window_title() {
        let owner = log_safe_quoted("Preview");
        let name = log_safe_quoted("Bob's tax return 2025.pdf");
        let raw = format!(
            "window-stack: z=3 id=4675 owner='{owner}' name='{name}' layer=0 alpha=1.00 bounds=(0,0 1512x944)"
        );
        let redacted = redact_for_export(&raw);
        assert!(
            !redacted.contains("tax return"),
            "leaked remainder: {redacted}"
        );
        assert!(redacted.contains("name='<redacted:"));
    }

    #[test]
    fn redact_for_export_masks_absolute_home_paths() {
        let raw = "logging: file sink initialized at /Users/till/Library/Logs/Petal/petal.log";
        let redacted = redact_for_export(raw);
        assert!(!redacted.contains("/Users/till/"));
        assert!(redacted.contains("/Users/<redacted:"));
        // Path structure after the username is preserved for debuggability.
        assert!(redacted.contains("/Library/Logs/Petal/petal.log"));
    }

    #[test]
    fn redact_for_export_masks_bare_email_addresses_anywhere() {
        let raw = "some unrelated window title mentions reach-me@example.co.uk in passing";
        let redacted = redact_for_export(raw);
        assert!(!redacted.contains("reach-me@example.co.uk"));
        assert!(redacted.contains("<redacted:"));
    }

    #[test]
    fn redact_for_export_does_not_over_match_at_sign_without_a_real_domain() {
        // A bare '@' with no plausible domain (no dot) must survive --
        // proves the email matcher isn't so loose it mangles unrelated text.
        let raw = "cc: @channel please review";
        assert_eq!(redact_for_export(raw), raw);
    }

    #[test]
    fn redact_for_export_removes_deep_link_credentials_by_structure() {
        let raw = concat!(
            "petal: launched with deep link(s): ",
            "[\"PETAL://Join/Eng-Sync-0123456789ABCDEF0123456789ABCDEF?utm=x\"]"
        );
        let redacted = redact_for_export(raw);

        assert!(!redacted.contains("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!redacted.contains("Eng-Sync-0123456789"));
        assert!(redacted.contains("PETAL://Join/<redacted:"));
        assert!(redacted.contains("?utm=x"));
    }

    #[test]
    fn redact_for_export_removes_meeting_route_credentials_by_structure() {
        let raw =
            "deep-link: navigated main webview to '/meeting/eng-sync-0123456789abcdef0123456789abcdef#ready'";
        let redacted = redact_for_export(raw);

        assert!(!redacted.contains("eng-sync-0123456789abcdef0123456789abcdef"));
        assert!(redacted.contains("/meeting/<redacted:"));
        assert!(redacted.contains("#ready"));
    }

    #[test]
    fn redact_for_export_removes_credential_suffixes_independent_of_phrase() {
        let raw = concat!(
            "backend request body room=eng-sync-0123456789abcdef0123456789abcdef ",
            "livekit=petal-room-design-review-fedcba98765432100123456789abcdef"
        );
        let redacted = redact_for_export(raw);

        assert!(!redacted.contains("0123456789abcdef0123456789abcdef"));
        assert!(!redacted.contains("fedcba98765432100123456789abcdef"));
        assert!(redacted.contains("room=eng-sync-<redacted:"));
        assert!(redacted.contains("livekit=petal-room-design-review-<redacted:"));
    }

    #[test]
    fn runtime_log_redaction_removes_room_credentials_without_identity_markers() {
        let raw = concat!(
            "deep-link: ignoring PETAL://join/eng-sync-0123456789abcdef0123456789abcdef ",
            "for identity 'alice@example.com'"
        );
        let redacted = redact_room_credentials(raw);

        assert!(!redacted.contains("0123456789abcdef0123456789abcdef"));
        assert!(redacted.contains("PETAL://join/<redacted:"));
        assert!(redacted.contains("identity 'alice@example.com'"));
    }

    #[test]
    fn collect_log_files_returns_all_matching_files_oldest_first_by_mtime() {
        let dir = temp_dir("collect");
        let base = SystemTime::now() - Duration::from_secs(3600);
        let active = touch(&dir, "petal.log");
        set_mtime(&active, base + Duration::from_secs(30));
        let rotated_b = touch(&dir, "petal-b.log");
        set_mtime(&rotated_b, base + Duration::from_secs(20));
        let rotated_a = touch(&dir, "petal-a.log");
        set_mtime(&rotated_a, base + Duration::from_secs(10));
        touch(&dir, "not-petal.log");
        touch(&dir, "petal-old.txt");

        assert_eq!(
            collect_log_files(&dir, None),
            vec![rotated_a, rotated_b, active]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_log_files_orders_by_mtime_not_by_name_across_the_two_naming_shapes() {
        // #905's own documented trap: '-' (0x2D) sorts before '.' (0x2E), so
        // a naive filename string sort would put every legacy
        // `petal-<...>.log` file before every per-day `petal.log.<date>`
        // file regardless of actual recency. Chosen so raw lexicographic
        // order DISAGREES with real chronological (mtime) order: the daily
        // file's name sorts textually AFTER the legacy file's name, even
        // though it is actually the OLDER of the two by mtime.
        let dir = temp_dir("collect-mixed-shape");
        let daily_older = touch(&dir, "petal.log.2020-01-01");
        let legacy_newer = touch(&dir, "petal-newer.log");
        let base = SystemTime::now();
        set_mtime(&daily_older, base - Duration::from_secs(100));
        set_mtime(&legacy_newer, base);

        assert_eq!(
            collect_log_files(&dir, None),
            vec![daily_older, legacy_newer],
            "must order by real mtime, not by filename"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_log_files_with_days_filters_to_the_requested_recent_window() {
        let dir = temp_dir("collect-range");
        let today = today_utc_string();
        let recent = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let old = (chrono::Utc::now() - chrono::Duration::days(10))
            .format("%Y-%m-%d")
            .to_string();
        touch(&dir, &daily_log_file_name(&today));
        touch(&dir, &daily_log_file_name(&recent));
        touch(&dir, &daily_log_file_name(&old));

        let names: Vec<String> = collect_log_files(&dir, Some(2))
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
            .collect();
        assert!(names.contains(&daily_log_file_name(&today)));
        assert!(names.contains(&daily_log_file_name(&recent)));
        assert!(
            !names.contains(&daily_log_file_name(&old)),
            "a 10-day-old file must be excluded from a last-2-days export: {names:?}"
        );

        assert_eq!(
            collect_log_files(&dir, None).len(),
            3,
            "None must mean no date filtering at all"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_logs_archive_writes_redacted_zip() {
        let dir = temp_dir("export");
        std::fs::write(
            dir.join("petal.log"),
            "session: join_room('eng-sync') begin\nidentity 'alice@example.com'\n",
        )
        .unwrap();
        std::fs::write(dir.join("petal-20260702.log"), "room \"ops\"\n").unwrap();

        let archive_path = export_logs_archive(&dir, None).unwrap();
        let file = File::open(&archive_path).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();

        let mut names = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            vec![
                "README.txt".to_string(),
                "petal-20260702.log".to_string(),
                "petal.log".to_string()
            ]
        );

        let mut active = String::new();
        zip.by_name("petal.log")
            .unwrap()
            .read_to_string(&mut active)
            .unwrap();
        assert!(!active.contains("eng-sync"));
        assert!(!active.contains("alice@example.com"));
        assert!(active.contains("<redacted:"));

        let _ = std::fs::remove_file(archive_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_logs_archive_decompresses_gzipped_completed_days_and_redacts_them() {
        // A completed day is gzip'd on disk by the time it's exported
        // (#905). The zip entry must be the DECOMPRESSED, redacted text --
        // not the raw compressed bytes reinterpreted as text (which would
        // both corrupt the export and skip redaction entirely).
        let dir = temp_dir("export-gz");
        let raw = "session: join_room('eng-sync') begin\nidentity 'alice@example.com'\n";
        let mut gz_bytes = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::default());
            encoder.write_all(raw.as_bytes()).unwrap();
            encoder.finish().unwrap();
        }
        std::fs::write(dir.join("petal.log.2026-08-27.gz"), &gz_bytes).unwrap();
        std::fs::write(dir.join("petal.log.2026-08-28"), "today, still plaintext\n").unwrap();

        let archive_path = export_logs_archive(&dir, None).unwrap();
        let file = File::open(&archive_path).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();

        let mut names = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            vec![
                "README.txt".to_string(),
                "petal.log.2026-08-27".to_string(),
                "petal.log.2026-08-28".to_string(),
            ],
            "the gz'd day's entry name must be un-gzip'd, not left as `....gz`"
        );

        let mut decompressed_entry = String::new();
        zip.by_name("petal.log.2026-08-27")
            .unwrap()
            .read_to_string(&mut decompressed_entry)
            .unwrap();
        assert!(!decompressed_entry.contains("eng-sync"));
        assert!(!decompressed_entry.contains("alice@example.com"));
        assert!(decompressed_entry.contains("<redacted:"));

        let _ = std::fs::remove_file(archive_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_and_redact_log_files_fails_closed_past_the_total_size_cap() {
        // #905 review Finding 8: an unbounded date-range export could
        // accumulate arbitrarily much decompressed text in memory at once.
        // Uses `_with_cap` with a tiny cap so the test doesn't need to
        // actually allocate `EXPORT_MAX_TOTAL_DECOMPRESSED_BYTES` (100 MiB)
        // of real data.
        let dir = temp_dir("export-cap");
        let a = touch(&dir, "petal.log.2026-08-01");
        std::fs::write(&a, "x".repeat(20)).unwrap();
        let b = touch(&dir, "petal.log.2026-08-02");
        std::fs::write(&b, "x".repeat(20)).unwrap();

        // Comfortably under the cap: both files fit.
        assert!(read_and_redact_log_files_with_cap(&[a.clone(), b.clone()], 100).is_ok());

        // Past the cap once both are summed: must fail closed with an
        // actionable message, not silently truncate or OOM.
        let result = read_and_redact_log_files_with_cap(&[a, b], 30);
        let err = result.expect_err("must fail closed once the combined size exceeds the cap");
        assert!(
            err.contains("too large") && err.contains("shorter date range"),
            "error should be actionable: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- #292: feedback-attachment diagnostic zip ------------------------

    #[test]
    fn feedback_attachment_zip_contains_only_current_log_redacted() {
        let dir = temp_dir("feedback-basic");
        let log_path = dir.join("petal.log");
        std::fs::write(
            &log_path,
            "session: join_room('eng-sync') begin\nidentity 'alice@example.com'\n",
        )
        .unwrap();
        // A rotated file must NOT show up in the feedback attachment (unlike
        // the local export, which includes rotated files too).
        std::fs::write(dir.join("petal-20260702.log"), "room \"ops\"\n").unwrap();

        let bytes = build_feedback_attachment_zip_from(&log_path).unwrap();
        let mut zip = ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut names = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            vec!["README.txt".to_string(), "petal.log".to_string()],
            "only README.txt + the current petal.log, never rotated files"
        );

        let mut active = String::new();
        zip.by_name("petal.log")
            .unwrap()
            .read_to_string(&mut active)
            .unwrap();
        assert!(!active.contains("eng-sync"));
        assert!(!active.contains("alice@example.com"));
        assert!(active.contains("<redacted:"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The feedback attachment's README must NOT repeat the local export's
    /// "no data was sent off this machine" claim -- attaching it to a
    /// feedback submission can genuinely send it off-device, and the two
    /// README texts must stay distinguishable (#292 point 4).
    #[test]
    fn feedback_attachment_readme_differs_from_local_export_readme() {
        let dir = temp_dir("feedback-readme");
        let log_path = dir.join("petal.log");
        std::fs::write(&log_path, "hello\n").unwrap();

        let bytes = build_feedback_attachment_zip_from(&log_path).unwrap();
        let mut zip = ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut readme = String::new();
        zip.by_name("README.txt")
            .unwrap()
            .read_to_string(&mut readme)
            .unwrap();

        assert!(
            !readme.contains("No data was sent off this machine"),
            "feedback attachment README must not claim the file stays local: {readme}"
        );
        assert!(
            readme
                .to_lowercase()
                .contains("may be sent off this machine"),
            "feedback attachment README must disclose it may leave the machine: {readme}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_export_readme_still_claims_no_data_left_the_machine() {
        let dir = temp_dir("local-readme");
        std::fs::write(dir.join("petal.log"), "hello\n").unwrap();

        let archive_path = export_logs_archive(&dir, None).unwrap();
        let file = File::open(&archive_path).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();
        let mut readme = String::new();
        zip.by_name("README.txt")
            .unwrap()
            .read_to_string(&mut readme)
            .unwrap();
        assert!(readme.contains("No data was sent off this machine"));

        let _ = std::fs::remove_file(archive_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn feedback_attachment_zip_tail_caps_a_large_log() {
        let dir = temp_dir("feedback-cap");
        let log_path = dir.join("petal.log");
        // Well over FEEDBACK_ATTACHMENT_LOG_TAIL_BYTES so the tail-cut must
        // engage; each line is tagged so the kept/dropped boundary is
        // directly assertable.
        let mut content = String::new();
        for i in 0..20_000 {
            content.push_str(&format!("line-{i:06}: padding padding padding\n"));
        }
        std::fs::write(&log_path, &content).unwrap();
        assert!(content.len() > FEEDBACK_ATTACHMENT_LOG_TAIL_BYTES);

        let bytes = build_feedback_attachment_zip_from(&log_path).unwrap();
        assert!(
            bytes.len() <= FEEDBACK_ATTACHMENT_MAX_ZIP_BYTES,
            "must stay within the hard zip cap"
        );
        let mut zip = ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut active = String::new();
        zip.by_name("petal.log")
            .unwrap()
            .read_to_string(&mut active)
            .unwrap();
        // The earliest lines must have been dropped; the very last line must
        // survive (tail is kept, not the head).
        assert!(!active.contains("line-000000:"));
        assert!(active.contains("line-019999:"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn feedback_attachment_zip_errors_closed_when_no_log_exists_yet() {
        let dir = temp_dir("feedback-missing");
        let missing = dir.join("petal.log");
        let result = build_feedback_attachment_zip_from(&missing);
        assert!(
            result.is_err(),
            "must fail closed, never return a partial/empty payload"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_bytes_cuts_at_the_nearest_newline_and_keeps_the_tail() {
        let data = b"aaaa\nbbbb\ncccc\ndddd\n";
        // max_len = 9 lands mid "cccc\ndddd\n" -- must snap forward to the
        // next newline rather than emit a partial line.
        let tail = tail_bytes(data, 9);
        assert_eq!(tail, b"dddd\n");
    }

    #[test]
    fn tail_bytes_returns_everything_when_under_the_cap() {
        let data = b"short\n";
        assert_eq!(tail_bytes(data, 1024), data);
    }

    // -- #281: Sentry PII allowlist/scrub + DSN-absence tests -----------

    /// The `before_send` hook must strip room names/participant identities
    /// (via the shared `redact_for_export` backstop, several different
    /// quoting/marker shapes at once) AND every field this app's PII policy
    /// has no fixed allowlist entry for -- `user`, `server_name`, `logger`,
    /// `extra`, `contexts`, and any tag beyond the fixed
    /// build_version/os_version/error_category set (e.g. a hypothetical
    /// `room_name` tag some future call site might add).
    #[test]
    fn scrub_event_for_sentry_strips_pii_and_disallowed_fields() {
        use sentry::protocol::{Breadcrumb, Event, Exception, Mechanism, User, Value};

        // Relies on SENTRY_ENABLED being true; take the same shared lock the
        // enable/disable-toggling tests use so this can't race a `false`
        // window and see `scrub_event_for_sentry` short-circuit to `None`.
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let mut event = Event {
            message: Some(
                "session: join_room('eng-sync') begin (identity 'alice@example.com')".to_string(),
            ),
            user: Some(User {
                id: Some("alice@example.com".into()),
                ..Default::default()
            }),
            server_name: Some("Tills-MacBook-Pro.local".into()),
            logger: Some("desktop_lib::session::room".into()),
            ..Default::default()
        };
        event.exception.values.push(Exception {
            ty: "panic".into(),
            value: Some(
                "panicked while in room 'ops-standup' for identity \"bob@example.com\"".to_string(),
            ),
            mechanism: Some(Mechanism {
                ty: "panic".into(),
                handled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        });
        event.breadcrumbs.values.push(Breadcrumb {
            message: Some(
                "deep-link: ignoring petal://join/eng-sync-0123456789abcdef0123456789abcdef"
                    .to_string(),
            ),
            data: {
                let mut data = sentry::protocol::Map::new();
                data.insert(
                    "participant_identity".into(),
                    Value::from("alice@example.com"),
                );
                data
            },
            ..Default::default()
        });
        event
            .tags
            .insert("build_version".to_string(), "0.6.4".to_string());
        event
            .tags
            .insert("os_version".to_string(), "15.5".to_string());
        // Not in the allowlist -- must be dropped even though it looks
        // innocuous; the whole point of allowlist-first is that we don't
        // trust a per-field judgment call at some future call site.
        event
            .tags
            .insert("room_name".to_string(), "eng-sync".to_string());
        event.extra.insert(
            "participant_identity".to_string(),
            Value::from("alice@example.com"),
        );

        let scrubbed =
            scrub_event_for_sentry(event).expect("before_send must not drop the event entirely");

        let message = scrubbed.message.clone().unwrap_or_default();
        assert!(!message.contains("eng-sync"));
        assert!(!message.contains("alice@example.com"));

        let exception_value = scrubbed.exception[0].value.clone().unwrap_or_default();
        assert!(!exception_value.contains("ops-standup"));
        assert!(!exception_value.contains("bob@example.com"));

        let breadcrumb_message = scrubbed.breadcrumbs[0].message.clone().unwrap_or_default();
        assert!(!breadcrumb_message.contains("eng-sync-0123456789abcdef0123456789abcdef"));
        assert!(
            scrubbed.breadcrumbs[0].data.is_empty(),
            "breadcrumb data must be dropped, not just scrubbed"
        );

        assert!(
            scrubbed.user.is_none(),
            "user must be dropped (not allowlisted)"
        );
        assert!(scrubbed.request.is_none());
        assert!(
            scrubbed.server_name.is_none(),
            "server_name (hostname) must be dropped -- often personally identifying"
        );
        assert!(scrubbed.logger.is_none());
        assert!(scrubbed.extra.is_empty(), "extra must be dropped entirely");
        assert!(
            scrubbed.contexts.is_empty(),
            "contexts must be dropped entirely"
        );

        assert_eq!(
            scrubbed.tags.get("build_version").map(String::as_str),
            Some("0.6.4")
        );
        assert_eq!(
            scrubbed.tags.get("os_version").map(String::as_str),
            Some("15.5")
        );
        assert_eq!(
            scrubbed.tags.get("error_category").map(String::as_str),
            Some("panic"),
            "error_category is derived from the exception mechanism when present"
        );
        assert!(
            !scrubbed.tags.contains_key("room_name"),
            "a non-allowlisted tag must never survive, even if it looks harmless"
        );
        assert_eq!(
            scrubbed.tags.len(),
            3,
            "exactly the fixed allowlisted tag set, nothing else"
        );
    }

    /// A log-derived event (no `exception` entries, e.g. from a bridged
    /// `log::error!`) has no mechanism to derive `error_category` from, and
    /// must fall back to a fixed "log_error" category rather than leaving
    /// the tag unset.
    #[test]
    fn scrub_event_for_sentry_defaults_error_category_for_log_derived_events() {
        use sentry::protocol::Event;

        // Relies on SENTRY_ENABLED being true; take the same shared lock the
        // enable/disable-toggling tests use so this can't race a `false`
        // window and see `scrub_event_for_sentry` short-circuit to `None`.
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let event = Event {
            message: Some("some error occurred".to_string()),
            ..Default::default()
        };
        let scrubbed = scrub_event_for_sentry(event).unwrap();
        assert_eq!(
            scrubbed.tags.get("error_category").map(String::as_str),
            Some("log_error")
        );
    }

    /// Several distinct PII quoting/marker shapes in one breadcrumb message
    /// must all be stripped by the `before_breadcrumb` hook, and `data`
    /// (arbitrary structured fields) must always be cleared.
    #[test]
    fn scrub_breadcrumb_for_sentry_strips_pii_shapes_and_data() {
        use sentry::protocol::{Breadcrumb, Value};

        // Relies on SENTRY_ENABLED being true; take the same shared lock the
        // enable/disable-toggling tests use so this can't race a `false`
        // window and see `scrub_breadcrumb_for_sentry` short-circuit to `None`.
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Three distinct marker/structural shapes `redact_for_export`
        // recognizes (same shapes its own existing tests above cover) -- a
        // bare `room=<slug>-<hex>` shape deliberately retains the
        // human-readable slug and only redacts the hex suffix (see
        // `redact_for_export_removes_credential_suffixes_independent_of_phrase`
        // above), so it's intentionally NOT included here.
        let breadcrumb = Breadcrumb {
            message: Some(
                concat!(
                    "session: join_room('eng-sync') begin ",
                    "(identity 'alice@example.com') ",
                    "deep-link: PETAL://Join/Eng-Sync-0123456789ABCDEF0123456789ABCDEF?utm=x"
                )
                .to_string(),
            ),
            data: {
                let mut data = sentry::protocol::Map::new();
                data.insert("room".into(), Value::from("eng-sync"));
                data
            },
            ..Default::default()
        };

        let scrubbed = scrub_breadcrumb_for_sentry(breadcrumb).unwrap();
        let message = scrubbed.message.unwrap_or_default();
        assert!(!message.contains("eng-sync"));
        assert!(!message.contains("Eng-Sync"));
        assert!(!message.contains("alice@example.com"));
        assert!(message.contains("<redacted:"));
        assert!(scrubbed.data.is_empty());
    }

    #[test]
    fn sentry_breadcrumb_storm_limiter_is_per_signature_and_interval() {
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *BREADCRUMB_STORM_LAST_KEPT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            [None; BREADCRUMB_STORM_SIGNATURES.len()];

        let start = std::time::Instant::now();
        for signature in BREADCRUMB_STORM_SIGNATURES {
            let message = format!("prefix {signature} details");
            assert!(breadcrumb_storm_allows(&message, start));
            assert!(!breadcrumb_storm_allows(
                &message,
                start + Duration::from_secs(1)
            ));
            assert!(breadcrumb_storm_allows(
                &message,
                start + BREADCRUMB_STORM_INTERVAL
            ));
        }
        assert!(breadcrumb_storm_allows(
            "session: joined room",
            start + Duration::from_secs(1)
        ));
    }

    #[test]
    fn sentry_breadcrumb_storm_suppression_preserves_prior_ring_context() {
        use sentry::protocol::{Breadcrumb, Event};

        let options = sentry::ClientOptions {
            max_breadcrumbs: 50,
            before_breadcrumb: Some(std::sync::Arc::new(scrub_breadcrumb_for_sentry)),
            before_send: Some(std::sync::Arc::new(scrub_event_for_sentry)),
            default_integrations: false,
            ..Default::default()
        };
        let envelopes = sentry::test::with_captured_envelopes_options(
            || {
                let _guard = SENTRY_ENABLED_TEST_LOCK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                set_sentry_enabled(true);
                *BREADCRUMB_STORM_LAST_KEPT
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    [None; BREADCRUMB_STORM_SIGNATURES.len()];

                sentry::add_breadcrumb(Breadcrumb {
                    message: Some("session: joined room".into()),
                    ..Default::default()
                });
                // Without signature suppression, these 200 entries evict all
                // 50 breadcrumb ring slots, including the join context above.
                for i in 0..200 {
                    sentry::add_breadcrumb(Breadcrumb {
                        message: Some(format!(
                            "publisher: camera frame size 1280x720 != published {i}x720 past the drop grace; recovering via Reanchor"
                        )),
                        ..Default::default()
                    });
                }
                sentry::capture_event(Event {
                    level: sentry::protocol::Level::Error,
                    message: Some("terminal capture failure".into()),
                    ..Default::default()
                });
            },
            options,
        );

        assert_eq!(envelopes.len(), 1);
        let event = envelopes[0].event().expect("captured error event");
        assert!(event
            .breadcrumbs
            .iter()
            .any(|breadcrumb| { breadcrumb.message.as_deref() == Some("session: joined room") }));
        let storm_count = event
            .breadcrumbs
            .iter()
            .filter(|breadcrumb| {
                breadcrumb
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains("past the drop grace; recovering via"))
            })
            .count();
        assert!(
            storm_count <= 2,
            "only a bounded sample of storm breadcrumbs may survive: {storm_count}"
        );
    }

    /// The compile-time DSN must be absent for a plain `cargo test` build
    /// (no `PETAL_SENTRY_DSN` is ever passed to this test binary's build),
    /// and a runtime override must also be absent unless explicitly set by
    /// the test itself -- so `init_sentry()` must be a clean no-op: no
    /// panic, no client installed, `SENTRY_GUARD` stays unset. This is the
    /// default state for every contributor build and CI run.
    #[test]
    fn sentry_is_silent_and_inert_without_a_dsn() {
        std::env::remove_var("PETAL_SENTRY_DSN");
        assert_eq!(
            sentry_dsn(),
            None,
            "compile-time DSN must be absent for a plain `cargo test` build"
        );

        init_sentry();
        assert!(
            SENTRY_GUARD.get().is_none(),
            "init_sentry() must not install a client when no DSN is available"
        );
    }

    /// `sentry_dsn()` must filter an empty-but-set runtime value the same
    /// way `transport::token::backend_base_url()` does -- `PETAL_SENTRY_DSN=`
    /// (set but empty) must resolve to "absent", not an empty DSN string
    /// that would fail to parse loudly on every startup.
    #[test]
    fn sentry_dsn_treats_empty_runtime_value_as_absent() {
        std::env::set_var("PETAL_SENTRY_DSN", "   ");
        assert_eq!(sentry_dsn(), None);
        std::env::remove_var("PETAL_SENTRY_DSN");
    }

    /// `SENTRY_ENABLED` is a process-wide static and `cargo test --lib` runs
    /// all tests in one process on multiple threads by default -- without
    /// this lock, `scrub_hooks_drop_everything_when_sentry_disabled` and
    /// `scrub_hooks_behave_normally_when_sentry_enabled` can interleave and
    /// observe each other's mid-test value, causing a flaky failure
    /// unrelated to either test's own logic (confirmed reproducible: passes
    /// every time under `--test-threads=1`, intermittently fails under the
    /// default parallel runner). Both tests must hold this for their full
    /// duration, not just around the `set_sentry_enabled` call.
    static SENTRY_ENABLED_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// `set_sentry_enabled(false)` must make the shared `before_send`/
    /// `before_breadcrumb` choke point drop every event/breadcrumb outright
    /// -- this is the ONLY enforcement point: no per-call-site check anywhere
    /// else. Resets `SENTRY_ENABLED` back to `true` at the end (its default)
    /// so this doesn't leak state into other tests in the same binary --
    /// `cargo test --lib` runs all tests in one process, and
    /// `SENTRY_ENABLED` is a process-wide static.
    #[test]
    fn scrub_hooks_drop_everything_when_sentry_disabled() {
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use sentry::protocol::{Breadcrumb, Event};

        set_sentry_enabled(false);

        let event = Event {
            message: Some("some error occurred".to_string()),
            ..Default::default()
        };
        assert!(
            scrub_event_for_sentry(event).is_none(),
            "before_send must drop the event entirely while disabled"
        );

        let breadcrumb = Breadcrumb {
            message: Some("some breadcrumb".to_string()),
            ..Default::default()
        };
        assert!(
            scrub_breadcrumb_for_sentry(breadcrumb).is_none(),
            "before_breadcrumb must drop the breadcrumb entirely while disabled"
        );

        set_sentry_enabled(true);
    }

    /// Sanity check that re-enabling restores the existing (already-tested
    /// above) scrub/redaction behavior rather than leaving the hooks
    /// permanently short-circuited -- i.e. `SENTRY_ENABLED` is a live gate,
    /// not a one-way kill switch. Also resets state at the end.
    #[test]
    fn scrub_hooks_behave_normally_when_sentry_enabled() {
        use sentry::protocol::{Breadcrumb, Event};

        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_sentry_enabled(true);

        let event = Event {
            message: Some("session: join_room('eng-sync') begin".to_string()),
            ..Default::default()
        };
        let scrubbed = scrub_event_for_sentry(event).expect("enabled: event must survive");
        assert!(!scrubbed.message.unwrap_or_default().contains("eng-sync"));

        let breadcrumb = Breadcrumb {
            message: Some("identity 'alice@example.com'".to_string()),
            ..Default::default()
        };
        let scrubbed_crumb =
            scrub_breadcrumb_for_sentry(breadcrumb).expect("enabled: breadcrumb must survive");
        assert!(!scrubbed_crumb
            .message
            .unwrap_or_default()
            .contains("alice@example.com"));
    }

    fn sample_capture_layout_diagnostic() -> SentryDiagnosticEvent {
        SentryDiagnosticEvent::CaptureLayoutInvalid(CaptureLayoutDiagnostic {
            role: DiagnosticRole::Sharer,
            source: SourceSelectionClass::Window,
            capture_geometry: GeometryBucket::Medium,
            configured_geometry: GeometryBucket::Large,
            pixel_format: PixelFormatClass::Bgra,
            scale: ScaleBucket::TwoX,
            encoder: EncoderImplementationClass::Hardware,
            stage: CaptureLayoutStage::Reconfiguration,
        })
    }

    fn sample_camera_health_diagnostic() -> SentryDiagnosticEvent {
        SentryDiagnosticEvent::CameraHealth(CameraHealthDiagnostic {
            role: DiagnosticRole::Receiver,
            direction: CameraDirection::Receive,
            capture_cadence: CadenceBucket::Severe,
            encode_cadence: CadenceBucket::NotApplicable,
            queue_backpressure: QueueBackpressureBucket::NotApplicable,
            decoder_render: DecoderRenderHealth::DecoderDegraded,
        })
    }

    #[test]
    fn camera_receive_ipc_accepts_only_closed_unhealthy_buckets() {
        let Some(SentryDiagnosticEvent::CameraHealth(event)) =
            camera_receive_health_diagnostic("severe", "decoder_degraded")
        else {
            panic!("valid receive health buckets must construct an event");
        };
        assert_eq!(event.role, DiagnosticRole::Receiver);
        assert_eq!(event.direction, CameraDirection::Receive);
        assert_eq!(event.capture_cadence, CadenceBucket::Severe);
        assert_eq!(event.encode_cadence, CadenceBucket::NotApplicable);
        assert_eq!(event.queue_backpressure, QueueBackpressureBucket::NotApplicable);
        assert_eq!(event.decoder_render, DecoderRenderHealth::DecoderDegraded);

        for forbidden in [
            "healthy",
            "unknown",
            "room-eng-sync",
            "alice@example.com",
            "https://example.test/private",
        ] {
            assert!(
                camera_receive_health_diagnostic(forbidden, "decoder_degraded").is_none(),
                "cadence must reject {forbidden:?}"
            );
            assert!(
                camera_receive_health_diagnostic("severe", forbidden).is_none(),
                "decoder/render must reject {forbidden:?}"
            );
        }
    }

    #[test]
    fn sentry_diagnostic_capture_layout_is_closed_schema_and_fixed_grouping() {
        let event = build_sentry_diagnostic_event(sample_capture_layout_diagnostic(), "1");
        let message = event.message.as_deref().expect("closed diagnostic message");
        assert!(message.starts_with("diagnostic: capture-layout-invalid"));
        assert!(message.contains("session_role=sharer"));
        assert!(message.contains("stage_code=reconfiguration"));
        assert!(event.exception.is_empty());
        assert!(event.breadcrumbs.is_empty());
        assert!(event.contexts.is_empty());
        assert!(event.extra.is_empty());
        assert_eq!(event.fingerprint.as_ref(), ["capture-layout-invalid"]);
        assert_eq!(event.tags.len(), DIAGNOSTIC_TAGS.len());
        assert_eq!(
            event.tags.get("event_name").map(String::as_str),
            Some("capture-layout-invalid")
        );
        assert_eq!(
            event.tags.get("schema_version").map(String::as_str),
            Some("1")
        );
        assert!(valid_sentry_diagnostic_event(&event));
    }

    #[test]
    fn sentry_diagnostic_messages_and_fingerprints_are_per_class() {
        let capture = build_sentry_diagnostic_event(sample_capture_layout_diagnostic(), "1");
        let camera = build_sentry_diagnostic_event(sample_camera_health_diagnostic(), "1");
        let capture_message = capture.message.as_deref().expect("capture message");
        let camera_message = camera.message.as_deref().expect("camera message");

        assert!(capture_message.contains("capture-layout-invalid"));
        assert!(camera_message.contains("camera-health"));
        assert_ne!(capture_message, camera_message);
        assert_eq!(capture.fingerprint.as_ref(), ["capture-layout-invalid"]);
        assert_eq!(camera.fingerprint.as_ref(), ["camera-health"]);
    }

    /// #867's playout re-point diagnostic is only useful if it actually
    /// SURVIVES `before_send`. `every_diagnostic_event_name_has_message_tags`
    /// proves it gets a title; it says nothing about whether
    /// `valid_sentry_diagnostic_event` accepts the event, and a rejected
    /// diagnostic is dropped silently by `scrub_event_for_sentry` -- so a
    /// forgotten `DIAGNOSTIC_TAGS` entry or `valid_diagnostic_tag` arm would
    /// mean the field never sees a single device flap. Round-trip it.
    #[test]
    fn playout_repoint_diagnostic_survives_before_send_with_a_real_title() {
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_sentry_enabled(true);

        for transition in [
            PlayoutTransitionTag::Repointed,
            PlayoutTransitionTag::Unavailable,
        ] {
            let event = build_sentry_diagnostic_event(
                SentryDiagnosticEvent::PlayoutDeviceRepointed(PlayoutDeviceDiagnostic {
                    role: DiagnosticRole::Both,
                    transition,
                }),
                "1",
            );
            assert_eq!(event.tags.len(), DIAGNOSTIC_TAGS.len());
            assert_eq!(event.fingerprint.as_ref(), ["playout-device-repointed"]);
            assert!(
                valid_sentry_diagnostic_event(&event),
                "the playout diagnostic must pass the validator, or before_send drops it"
            );

            let message = event
                .message
                .as_deref()
                .expect("playout diagnostic must be titled, not <unlabeled event>")
                .to_string();
            assert!(message.starts_with("diagnostic: playout-device-repointed"));
            assert!(message.contains(&format!("playout_transition={}", transition.tag())));

            let scrubbed = scrub_event_for_sentry(event)
                .expect("a valid playout diagnostic must survive before_send");
            assert_eq!(scrubbed.message.as_deref(), Some(message.as_str()));
        }
    }

    #[test]
    fn update_install_failed_diagnostic_round_trips_every_tag_combination() {
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_sentry_enabled(true);

        let stages = [
            InstallFailureStageTag::Resolve,
            InstallFailureStageTag::Stage,
            InstallFailureStageTag::Extract,
            InstallFailureStageTag::Backup,
            InstallFailureStageTag::Promote,
            InstallFailureStageTag::Rollback,
            InstallFailureStageTag::Privileged,
            InstallFailureStageTag::NotApplicable,
        ];
        let kinds = [
            InstallFailureKindTag::CrossDevice,
            InstallFailureKindTag::PermissionDenied,
            InstallFailureKindTag::ReadOnly,
            InstallFailureKindTag::NoSpace,
            InstallFailureKindTag::NotFound,
            InstallFailureKindTag::Other,
            InstallFailureKindTag::NotApplicable,
        ];
        let boundaries = [
            InstallVolumeBoundaryTag::SameVolume,
            InstallVolumeBoundaryTag::CrossVolume,
            InstallVolumeBoundaryTag::Unknown,
            InstallVolumeBoundaryTag::NotApplicable,
        ];
        let destinations = [
            InstallDestinationClassTag::Applications,
            InstallDestinationClassTag::UserApplications,
            InstallDestinationClassTag::DiskImage,
            InstallDestinationClassTag::RemovableVolume,
            InstallDestinationClassTag::Other,
            InstallDestinationClassTag::NotApplicable,
        ];

        for stage in stages {
            for kind in kinds {
                for boundary in boundaries {
                    for destination in destinations {
                        let event = build_sentry_diagnostic_event(
                            SentryDiagnosticEvent::UpdateInstallFailed(
                                UpdateInstallFailedDiagnostic {
                                    stage,
                                    kind,
                                    boundary,
                                    destination,
                                },
                            ),
                            "1",
                        );
                        assert_eq!(event.tags.len(), DIAGNOSTIC_TAGS.len());
                        assert_eq!(event.fingerprint.as_ref(), ["update-install-failed"]);
                        assert!(
                            valid_sentry_diagnostic_event(&event),
                            "update diagnostic failed validation: {event:?}"
                        );
                        let message = event
                            .message
                            .as_deref()
                            .expect("update install diagnostic must have a real title")
                            .to_string();
                        assert!(message.starts_with("diagnostic: update-install-failed"));
                        assert!(message.contains(&format!("install_failure_stage={}", stage.tag())));
                        let scrubbed = scrub_event_for_sentry(event)
                            .expect("a valid update install diagnostic must survive before_send");
                        assert_eq!(scrubbed.message.as_deref(), Some(message.as_str()));
                    }
                }
            }
        }
    }

    /// #866 shipped `camera-size-mismatch-recovery` with no arm in
    /// `diagnostic_message_tags`, so it reached Sentry as `<unlabeled event>`
    /// -- the very defect #788 fixed, reintroduced by the next event class.
    /// `valid_sentry_diagnostic_event` cannot catch this: it is fail-closed on
    /// tag count and fail-open on an absent message. Loop over the names so a
    /// class added later fails here instead of shipping untitled.
    #[test]
    fn every_diagnostic_event_name_has_message_tags() {
        for name in DIAGNOSTIC_EVENT_NAMES {
            let keys = diagnostic_message_tags(name).unwrap_or_else(|| {
                panic!(
                    "diagnostic event '{name}' has no diagnostic_message_tags arm; it would ship \
                     to Sentry with no title (#788/#866)"
                )
            });
            assert!(
                !keys.is_empty(),
                "diagnostic event '{name}' has an empty message-tag list; its title would carry \
                 no distinguishing facts"
            );
            for key in keys {
                assert!(
                    DIAGNOSTIC_TAGS.contains(key),
                    "diagnostic event '{name}' names message tag '{key}', which is not in \
                     DIAGNOSTIC_TAGS; diagnostic_message would return None and the event would \
                     ship untitled"
                );
            }
        }
    }

    /// Pins #915's new event by name rather than relying solely on the
    /// generic sweep above -- a future rename or removal of this specific
    /// arm should fail here with a message that names the event, not just
    /// "some diagnostic event's arm is missing".
    #[test]
    fn browser_url_extraction_failed_is_a_registered_diagnostic_event() {
        assert!(
            DIAGNOSTIC_EVENT_NAMES.contains(&"browser-url-extraction-failed"),
            "browser_url_extraction_failed must be in DIAGNOSTIC_EVENT_NAMES (#915)"
        );
        let keys = diagnostic_message_tags("browser-url-extraction-failed").unwrap_or_else(|| {
            panic!(
                "browser_url_extraction_failed has no diagnostic_message_tags arm; it would \
                 ship to Sentry with no title (#788/#915)"
            )
        });
        assert_eq!(keys, ["browser_url_extraction_cause"]);
    }

    /// Round-trips one `BrowserUrlExtractionFailed` event through the exact
    /// validator the real Sentry pipeline uses, the same way
    /// `playout_repoint_diagnostic_survives_before_send_with_a_real_title`
    /// does for an existing event -- passing `every_diagnostic_event_name_has_message_tags`
    /// proves the event gets a title, not that `valid_sentry_diagnostic_event`
    /// accepts it.
    #[test]
    fn browser_url_extraction_failed_diagnostic_is_a_valid_sentry_event() {
        for cause in [
            BrowserUrlExtractionCauseTag::Denied,
            BrowserUrlExtractionCauseTag::Timeout,
            BrowserUrlExtractionCauseTag::Ambiguous,
            BrowserUrlExtractionCauseTag::NoMatch,
            BrowserUrlExtractionCauseTag::Spawn,
            BrowserUrlExtractionCauseTag::Failed,
        ] {
            let event = build_sentry_diagnostic_event(
                SentryDiagnosticEvent::BrowserUrlExtractionFailed(
                    BrowserUrlExtractionFailedDiagnostic { cause },
                ),
                "1",
            );
            assert!(
                valid_sentry_diagnostic_event(&event),
                "cause {cause:?} produced an invalid Sentry diagnostic event"
            );
        }
    }

    #[test]
    fn sentry_diagnostic_scrubber_rejects_injected_message_and_preserves_valid_message() {
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_sentry_enabled(true);

        let mut injected = build_sentry_diagnostic_event(sample_capture_layout_diagnostic(), "1");
        injected.message = Some("room-eng-sync alice@example.com".into());
        assert!(scrub_event_for_sentry(injected).is_none());

        let valid = build_sentry_diagnostic_event(sample_capture_layout_diagnostic(), "1");
        let expected_message = valid.message.clone();
        let expected_tags = valid.tags.clone();
        let scrubbed = scrub_event_for_sentry(valid).expect("valid diagnostic must survive");
        assert_eq!(scrubbed.message, expected_message);
        assert_eq!(scrubbed.tags, expected_tags);
    }

    #[test]
    fn sentry_diagnostic_scrubber_drops_injected_sensitive_data_and_unknown_fields() {
        use sentry::protocol::{Breadcrumb, Value};
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prohibited = [
            "room-eng-sync-0123456789abcdef",
            "alice@example.com",
            "Window: payroll.xlsx",
            "https://example.test/private",
            "/Users/alice/secret",
            "typed clipboard text",
            "raw OS error",
            "frame-bytes",
            "audio-bytes",
        ];
        for value in prohibited {
            let mut event = build_sentry_diagnostic_event(sample_capture_layout_diagnostic(), "1");
            event.tags.insert("room_id".into(), value.into());
            event.extra.insert("injected".into(), Value::from(value));
            event.breadcrumbs.values.push(Breadcrumb {
                message: Some(value.into()),
                ..Default::default()
            });
            assert!(
                scrub_event_for_sentry(event).is_none(),
                "must drop injected value {value:?}"
            );
        }

        let mut known_key_injection =
            build_sentry_diagnostic_event(sample_capture_layout_diagnostic(), "1");
        known_key_injection
            .tags
            .insert("session_role".into(), "room-eng-sync".into());
        assert!(
            scrub_event_for_sentry(known_key_injection).is_none(),
            "known keys require closed enum values too"
        );

        let mut fingerprint_injection =
            build_sentry_diagnostic_event(sample_capture_layout_diagnostic(), "1");
        fingerprint_injection.fingerprint =
            Cow::Owned(vec!["room-eng-sync-0123456789abcdef".into()]);
        assert!(
            scrub_event_for_sentry(fingerprint_injection).is_none(),
            "fingerprints require the fixed diagnostic event name"
        );

        let mut release_injection =
            build_sentry_diagnostic_event(sample_capture_layout_diagnostic(), "1");
        release_injection.release = Some(Cow::Borrowed("alice@example.com"));
        assert!(
            scrub_event_for_sentry(release_injection).is_none(),
            "release requires the fixed build version"
        );
    }

    #[test]
    fn sentry_diagnostic_survives_real_client_scope_without_ambient_data() {
        use sentry::protocol::{Attachment, Breadcrumb, EnvelopeItem, User, Value};

        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_sentry_enabled(true);
        let options = sentry::ClientOptions {
            release: Some(env!("CARGO_PKG_VERSION").into()),
            environment: Some("private-environment".into()),
            server_name: Some("Alice-MacBook.local".into()),
            default_integrations: false,
            before_send: Some(std::sync::Arc::new(scrub_event_for_sentry)),
            ..Default::default()
        };
        let envelopes = sentry::test::with_captured_envelopes_options(
            || {
                *SENTRY_DIAGNOSTIC_RATE_LIMITER
                    .get_or_init(|| Mutex::new(DiagnosticRateLimiter::default()))
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    DiagnosticRateLimiter::default();
                sentry::configure_scope(|scope| {
                    scope.set_user(Some(User {
                        id: Some("alice@example.com".into()),
                        ..Default::default()
                    }));
                    scope.set_extra("window_title", Value::from("payroll.xlsx"));
                    scope.set_transaction(Some("private-room-transaction"));
                    scope.add_attachment(Attachment {
                        buffer: b"private-frame-bytes".to_vec(),
                        filename: "private-frame.bin".into(),
                        content_type: Some("application/octet-stream".into()),
                        ty: None,
                    });
                });
                sentry::add_breadcrumb(Breadcrumb {
                    message: Some("room-eng-sync-secret".into()),
                    ..Default::default()
                });
                assert!(capture_sentry_diagnostic_with_client(
                    sample_capture_layout_diagnostic(),
                    true,
                ));
                let mut second_path = match sample_capture_layout_diagnostic() {
                    SentryDiagnosticEvent::CaptureLayoutInvalid(value) => value,
                    _ => unreachable!(),
                };
                second_path.stage = CaptureLayoutStage::Publish;
                assert!(
                    !capture_sentry_diagnostic_with_client(
                        SentryDiagnosticEvent::CaptureLayoutInvalid(second_path),
                        true,
                    ),
                    "all capture-layout terminal paths share the global limiter"
                );
            },
            options,
        );

        assert_eq!(envelopes.len(), 1, "one diagnostic envelope must survive");
        assert_eq!(
            envelopes[0].items().count(),
            1,
            "the isolated diagnostic scope must not append attachments"
        );
        assert!(!envelopes[0]
            .items()
            .any(|item| matches!(item, EnvelopeItem::Attachment(_))));
        let event = envelopes[0].event().expect("diagnostic event item");
        assert!(valid_sentry_diagnostic_event(event));
        let message = event.message.as_deref().expect("diagnostic message");
        assert!(!message.is_empty());
        assert!(message.contains("capture-layout-invalid"));
        #[cfg(target_os = "macos")]
        assert_ne!(
            event.tags.get("os_version").map(String::as_str),
            Some("unknown")
        );
        assert!(event.breadcrumbs.is_empty());
        assert!(event.contexts.is_empty());
        assert!(event.extra.is_empty());
        assert!(event.user.is_none());
        assert!(event.transaction.is_none());
        assert!(event.server_name.is_none());
        assert!(event.environment.is_none());
        assert!(event.sdk.is_none());
    }

    #[test]
    fn sentry_diagnostic_rate_limiter_is_per_class_and_reports_bounded_suppression() {
        let start = std::time::Instant::now();
        let mut limiter = DiagnosticRateLimiter::default();
        assert_eq!(limiter.allow("capture-layout-invalid", start), Some("1"));
        assert_eq!(
            limiter.allow("capture-layout-invalid", start + Duration::from_secs(1)),
            None
        );
        assert_eq!(
            limiter.allow("camera-health", start + Duration::from_secs(1)),
            Some("1")
        );
        assert_eq!(
            limiter.allow("capture-layout-invalid", start + SENTRY_DIAGNOSTIC_INTERVAL),
            Some("2_9")
        );
    }

    fn reset_global_storm_detectors() {
        if let Some(detector) = REPUBLISH_STORM_DETECTOR.get() {
            detector
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .reset();
        }
        if let Some(detector) = WATCHDOG_REPEAT_STORM_DETECTOR.get() {
            detector
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .reset();
        }
    }

    #[test]
    fn republish_storm_pages_once_not_once_per_republish() {
        let _guard = STORM_DETECTOR_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_global_storm_detectors();
        let start = Instant::now();
        let cadence = Duration::from_millis(315);
        let mut limiter = DiagnosticRateLimiter::default();
        let mut detector_trips = 0;
        let mut sentry_bound = 0;

        for index in 0_u32..232 {
            let now = start + cadence * index;
            if let Some(event) = note_republish_complete_at(41, now) {
                detector_trips += 1;
                if now.saturating_duration_since(start) < SENTRY_DIAGNOSTIC_INTERVAL
                    && limiter.allow(event.event_name(), now).is_some()
                {
                    sentry_bound += 1;
                }
            }
        }

        assert!(
            detector_trips >= 15,
            "the field storm must repeatedly cross the detector, not merely be absent"
        );
        assert_eq!(sentry_bound, 1, "the first 60 s must page exactly once");
    }

    #[test]
    fn publish_drop_streak_pages_once_not_once_per_frame() {
        let start = Instant::now();
        let cadence = Duration::from_nanos(1_000_000_000 / 30);
        let mut detector = DropStreakDetector::default();
        let mut limiter = DiagnosticRateLimiter::default();
        let mut detector_trips = 0;
        let mut sentry_bound = 0;

        for index in 0_u32..2190 {
            let now = start + cadence * index;
            if detector.record(false, now) {
                detector_trips += 1;
                let event = SentryDiagnosticEvent::PublishDropStreak(StormDiagnostic {
                    role: DiagnosticRole::Sharer,
                    scope: StormScopeTag::Camera,
                });
                if now.saturating_duration_since(start) < SENTRY_DIAGNOSTIC_INTERVAL
                    && limiter.allow(event.event_name(), now).is_some()
                {
                    sentry_bound += 1;
                }
            }
        }

        assert!(
            detector_trips >= 15,
            "the field storm must repeatedly cross the detector, not merely be absent"
        );
        assert_eq!(sentry_bound, 1, "the first 60 s must page exactly once");
    }

    #[test]
    fn watchdog_repeat_storm_pages_once_not_once_per_fire() {
        let _guard = STORM_DETECTOR_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_global_storm_detectors();
        let start = Instant::now();
        let cadence = Duration::from_millis(2450);
        let mut limiter = DiagnosticRateLimiter::default();
        let mut detector_trips = 0;
        let mut sentry_bound = 0;

        for index in 0_u32..49 {
            let now = start + cadence * index;
            if let Some(event) = note_window_creation_watchdog_stall_at(73, now) {
                detector_trips += 1;
                if now.saturating_duration_since(start) < SENTRY_DIAGNOSTIC_INTERVAL
                    && limiter.allow(event.event_name(), now).is_some()
                {
                    sentry_bound += 1;
                }
            }
        }

        assert!(detector_trips > 1, "the detector must see a repeating storm");
        assert_eq!(sentry_bound, 1, "the first 60 s must page exactly once");
    }

    #[test]
    fn healthy_session_never_trips_any_storm_detector() {
        let _guard = STORM_DETECTOR_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_global_storm_detectors();
        let start = Instant::now();

        for offset in [0, 5, 10] {
            assert!(
                note_republish_complete_at(11, start + Duration::from_secs(offset)).is_none(),
                "three republishes over ten seconds are healthy"
            );
        }

        let mut drops = DropStreakDetector::default();
        let mut now = start;
        for _ in 0..100 {
            for _ in 0..5 {
                assert!(!drops.record(false, now));
                now += Duration::from_millis(33);
            }
            assert!(!drops.record(true, now));
            now += Duration::from_millis(33);
        }

        assert!(note_window_creation_watchdog_stall_at(22, start).is_none());
        assert!(note_window_creation_watchdog_stall_at(
            22,
            start + Duration::from_secs(20)
        )
        .is_none());
    }

    #[test]
    fn storm_detector_windows_are_per_key() {
        let _guard = STORM_DETECTOR_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_global_storm_detectors();
        let start = Instant::now();

        for occurrence in 0_u64..7 {
            for window_id in 0_u32..5 {
                assert!(
                    note_republish_complete_at(
                        window_id,
                        start + Duration::from_millis(occurrence * 100),
                    )
                    .is_none(),
                    "seven occurrences for each key must not combine into a global storm"
                );
            }
        }
    }

    #[test]
    fn storm_diagnostic_survives_before_send_with_a_real_title() {
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_sentry_enabled(true);
        let event = build_sentry_diagnostic_event(
            SentryDiagnosticEvent::RepublishStorm(StormDiagnostic {
                role: DiagnosticRole::Sharer,
                scope: StormScopeTag::WindowShare,
            }),
            "1",
        );

        assert_eq!(event.tags.len(), DIAGNOSTIC_TAGS.len());
        assert!(valid_sentry_diagnostic_event(&event));
        let message = event
            .message
            .as_deref()
            .expect("storm diagnostic must be titled, not <unlabeled event>")
            .to_string();
        assert!(message.starts_with("diagnostic: republish-storm"));
        assert!(message.contains("session_role=sharer"));
        assert!(message.contains("storm_scope=window_share"));

        let scrubbed = scrub_event_for_sentry(event)
            .expect("a valid storm diagnostic must survive before_send");
        assert_eq!(scrubbed.message.as_deref(), Some(message.as_str()));
    }

    #[test]
    fn sentry_diagnostic_capture_is_inert_without_client_or_when_disabled() {
        let _guard = SENTRY_ENABLED_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_sentry_enabled(false);
        assert!(!capture_sentry_diagnostic(
            sample_capture_layout_diagnostic()
        ));
        set_sentry_enabled(true);
        assert!(
            SENTRY_GUARD.get().is_none(),
            "test builds have no DSN/client"
        );
        assert!(!capture_sentry_diagnostic(
            sample_capture_layout_diagnostic()
        ));
    }
    // #884: the decoder-allocation signature must match the REAL libwebrtc
    // line and reject near misses -- both directions per CLAUDE.md rule 8.
    #[test]
    fn decoder_allocation_signature_matches_the_real_line_and_only_it() {
        assert!(decoder_allocation_failure_signature(
            "(RTCVideoDecoderH264.mm:61): Failed to decode frame. Status: -6662"
        ));
        assert!(
            !decoder_allocation_failure_signature(
                "(RTCVideoDecoderH264.mm:61): Failed to decode frame. Status: -12909"
            ),
            "bad-data decode errors are ordinary stream faults, not allocation pressure"
        );
        assert!(!decoder_allocation_failure_signature(
            "something else mentioning -6662 without a decode failure"
        ));
    }

    // #884: recoveryCount parser -- both directions. Sums multiple
    // accelerators; unparseable digits must read as unknown (None), never 0.
    #[test]
    fn gpu_recovery_count_parses_and_sums_and_fails_closed() {
        let two = "\"PerformanceStatistics\" = {\"recoveryCount\"=2,...}\n\"recoveryCount\"=3";
        assert_eq!(gpu_recovery_count_from_ioreg(two), Some(5));
        assert_eq!(gpu_recovery_count_from_ioreg("no counter here"), None);
        assert_eq!(
            gpu_recovery_count_from_ioreg("\"recoveryCount\"=notanumber"),
            None,
            "a drifted format must read as unknown, not zero"
        );
    }

}
