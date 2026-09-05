//! Closed PostHog product-event pipe (desktop host).
//!
//! Sentry stays the crash tool (`log::error!` → issue). These twelve events
//! answer “are users having a bad time?” as rates. The allowlist lives in
//! `docs/POSTHOG_EVENT_ALLOWLIST.md` — no new event without an
//! explicit add there. Local/CI builds are keyless and no-op; a release bakes
//! `PETAL_POSTHOG_KEY` the same way it bakes the Sentry DSN. The browser
//! client is a separate pipe (`web-harness/src/analytics.ts`) that tags
//! `client=web`; this module tags `client=native`.
//!
//! Never send room names, identities, window titles, device names, key
//! codes, clipboard, coordinates, or tokens.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::remote_control_core::{RemoteControlAction, RemoteControlMessage, RemoteControlType};
use crate::sync_ext::MutexExt;
use crate::transport::token::TokenError;

const DEFAULT_HOST: &str = "https://us.i.posthog.com";
const CAPTURE_PATH: &str = "/i/v0/e/";
const SEND_TIMEOUT: Duration = Duration::from_secs(2);
const QUEUE_CAP: usize = 64;
const TYPE_IDLE: Duration = Duration::from_secs(1);
const SCROLL_IDLE: Duration = Duration::from_millis(500);
const DISPLAY_RECONFIG_DEBOUNCE: Duration = Duration::from_secs(1);
const DISTINCT_ID_FILE: &str = "analytics-id";
// #908: one retry with a short backoff for a transient send failure. Bounded
// on purpose -- a dead network must not wedge the worker or grow memory. The
// channel stays capped at QUEUE_CAP regardless of how long sends take.
const MAX_SEND_RETRIES: u32 = 1;
const RETRY_BACKOFF: Duration = Duration::from_millis(500);
// #908: rate limit for the send-failure warn line. Never per-event (#905) --
// log the first failure in a streak immediately, then at most once per
// interval while it's still failing, then once more on recovery.
const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(60);
// #908: how long `flush()` will wait for the queue to drain at quit. Bounded
// so a dead network never delays shutdown beyond this.
pub(crate) const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

static DISTINCT_ID: OnceLock<String> = OnceLock::new();
static WORKER: OnceLock<tokio::sync::mpsc::Sender<serde_json::Value>> = OnceLock::new();
static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static MEETING: Mutex<Option<Meeting>> = Mutex::new(None);
static COALESCER: Mutex<InputCoalescer> = Mutex::new(InputCoalescer::new());
static LAST_DISPLAY_RECONFIG: Mutex<Option<Instant>> = Mutex::new(None);
static KEYLESS_LOGGED: AtomicBool = AtomicBool::new(false);
// #908: events discarded by `try_send` onto the full QUEUE_CAP channel.
// Read-and-cleared into the next captured event's `dropped_since_last`
// property (see `take_dropped_since_last`) so loss is visible in PostHog
// itself, not just locally. `Relaxed` + no allocation: `capture()` is on hot
// paths (remote-control input, video-stall watchdog).
static DROPPED: AtomicU64 = AtomicU64::new(0);
// #908: events accepted onto the channel but not yet fully handled by the
// worker (send attempt + retries settled). `flush()` polls this at quit so
// teardown events like `meeting_left` survive a clean exit.
static PENDING: AtomicU64 = AtomicU64::new(0);
// #908: tracks consecutive send failures so the worker can log a rate-limited
// `warn` line (visible in a shipped build's `info`-level file sink) instead
// of the previous `debug!` that never reached it.
static FAILURE_TRACKER: Mutex<FailureTracker> = Mutex::new(FailureTracker::new());

#[cfg(test)]
thread_local! {
    static TEST_SINK: std::cell::RefCell<Option<Vec<CapturedEvent>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
const COMMON_KEYS: [&str; 5] = ["build_version", "os", "os_version", "arch", "client"];
#[cfg(test)]
const EVENT_NAMES: [&str; 13] = [
    "meeting_joined",
    "meeting_left",
    "join_failed",
    "share_started",
    "share_stopped",
    "remote_audio_silent",
    "remote_video_stalled",
    "capture_restarted",
    "reconnect",
    "permission_denied",
    "remote_control_input",
    "device_changed",
    // #872: nothing recorded whether annotation/drawing was ON. That flag is
    // the ONLY thing that makes the share overlay capture the cursor, so when a
    // user reported an unclickable desktop we could not tell from telemetry
    // whether they had it enabled. Bucketed on/off only -- no strokes, no
    // coordinates, no window titles.
    "annotation_toggled",
];

#[derive(Clone, Copy)]
struct Meeting {
    joined_at: Instant,
    reconnects: u32,
}

/// #908: decides WHEN a send-failure/recovery is worth a log line, never
/// whether one happened -- every attempt still updates the tracker, but only
/// a state transition (first failure, recovery) or a periodic summary while
/// still failing produces a message. This is what keeps a total outage
/// visible without becoming a per-event line (see #905's 263 MB log).
struct FailureTracker {
    consecutive_failures: u64,
    last_logged_at: Option<Instant>,
}

impl FailureTracker {
    const fn new() -> Self {
        Self {
            consecutive_failures: 0,
            last_logged_at: None,
        }
    }

    /// Call once per settled send attempt (after retries are exhausted or it
    /// succeeded). Returns `Some(message)` exactly when a log line should be
    /// emitted at `warn`; `None` otherwise.
    ///
    /// #908 review blocker 3: a single `last_logged_at` cooldown gates BOTH
    /// failure and recovery lines -- it is never cleared on recovery. The
    /// original version cleared it on every success, so an alternating
    /// fail/succeed/fail/... sequence logged on every single settlement
    /// (failure -> warn, recovery -> warn, next failure -> warn again),
    /// which is exactly the per-event storm #905 exists to prevent. With one
    /// shared clock, at most one line total can be emitted per
    /// `FAILURE_LOG_INTERVAL`, regardless of how the outcomes alternate.
    fn note_result(&mut self, ok: bool, now: Instant, error: Option<&str>) -> Option<String> {
        let due = match self.last_logged_at {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= FAILURE_LOG_INTERVAL,
        };
        if ok {
            let had_failures = self.consecutive_failures > 0;
            self.consecutive_failures = 0;
            if !had_failures || !due {
                return None;
            }
            self.last_logged_at = Some(now);
            return Some("analytics: capture send recovered".to_string());
        }
        self.consecutive_failures += 1;
        if !due {
            return None;
        }
        self.last_logged_at = Some(now);
        let detail = error.unwrap_or("unknown");
        Some(format!(
            "analytics: capture send failing ({} consecutive failure(s), last error class: {detail})",
            self.consecutive_failures
        ))
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedEvent {
    pub name: String,
    pub properties: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DurationBucket {
    ZeroToTen,
    TenToThirty,
    ThirtyToOneTwenty,
    OneTwentyPlus,
}

impl DurationBucket {
    fn as_str(self) -> &'static str {
        match self {
            Self::ZeroToTen => "0_10s",
            Self::TenToThirty => "10_30s",
            Self::ThirtyToOneTwenty => "30_120s",
            Self::OneTwentyPlus => "120s_plus",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReconnectCountBucket {
    Zero,
    One,
    TwoToFour,
    FivePlus,
}

impl ReconnectCountBucket {
    fn as_str(self) -> &'static str {
        match self {
            Self::Zero => "0",
            Self::One => "1",
            Self::TwoToFour => "2_4",
            Self::FivePlus => "5_plus",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinFailedReason {
    Network,
    NoBackend,
    Token,
    Timeout,
}

impl JoinFailedReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::NoBackend => "no_backend",
            Self::Token => "token",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShareStartedSource {
    Window,
    Display,
    Picker,
}

impl ShareStartedSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Display => "display",
            Self::Picker => "picker",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShareStoppedReason {
    User,
    WindowGone,
    CaptureFailed,
}

impl ShareStoppedReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::WindowGone => "window_gone",
            Self::CaptureFailed => "capture_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoStallSource {
    Stats,
    Gallery,
    Native,
}

impl VideoStallSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stats => "stats",
            Self::Gallery => "gallery",
            Self::Native => "native",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestartOutcome {
    Recovered,
    Failed,
}

impl RestartOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Recovered => "recovered",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionKind {
    Screen,
    Mic,
    Camera,
}

impl PermissionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Mic => "mic",
            Self::Camera => "camera",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteControlInputKind {
    Click,
    Type,
    Paste,
    Scroll,
}

impl RemoteControlInputKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Type => "type",
            Self::Paste => "paste",
            Self::Scroll => "scroll",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceKind {
    Display,
    Camera,
    Mic,
}

impl DeviceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Camera => "camera",
            Self::Mic => "mic",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnnotationState {
    On,
    Off,
}

impl AnnotationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceChange {
    Switched,
    Failed,
    Reconfigured,
    Sleep,
    Wake,
}

impl DeviceChange {
    fn as_str(self) -> &'static str {
        match self {
            Self::Switched => "switched",
            Self::Failed => "failed",
            Self::Reconfigured => "reconfigured",
            Self::Sleep => "sleep",
            Self::Wake => "wake",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    MeetingJoined,
    MeetingLeft {
        duration_bucket: DurationBucket,
        reconnect_count_bucket: ReconnectCountBucket,
    },
    JoinFailed {
        reason: JoinFailedReason,
    },
    ShareStarted {
        source: ShareStartedSource,
    },
    ShareStopped {
        reason: ShareStoppedReason,
    },
    RemoteAudioSilent {
        duration_bucket: DurationBucket,
    },
    // NO duration_bucket: the emit site (diagnostics.rs) is an edge-triggered
    // state transition with no duration in hand. It previously hardcoded
    // ZeroToTen on EVERY stall, so the dashboard read 0_10s for all of them --
    // a fabricated dimension. Missing data is hidden, never guessed
    // (CLAUDE.md data honesty). Plumb a real duration before adding it back.
    RemoteVideoStalled {
        source: VideoStallSource,
    },
    CaptureRestarted {
        outcome: RestartOutcome,
    },
    Reconnect {
        outcome: RestartOutcome,
    },
    PermissionDenied {
        kind: PermissionKind,
    },
    RemoteControlInput {
        kind: RemoteControlInputKind,
    },
    DeviceChanged {
        kind: DeviceKind,
        change: DeviceChange,
    },
    AnnotationToggled {
        state: AnnotationState,
    },
}

impl Event {
    fn name(&self) -> &'static str {
        match self {
            Self::MeetingJoined => "meeting_joined",
            Self::MeetingLeft { .. } => "meeting_left",
            Self::JoinFailed { .. } => "join_failed",
            Self::ShareStarted { .. } => "share_started",
            Self::ShareStopped { .. } => "share_stopped",
            Self::RemoteAudioSilent { .. } => "remote_audio_silent",
            Self::RemoteVideoStalled { .. } => "remote_video_stalled",
            Self::CaptureRestarted { .. } => "capture_restarted",
            Self::Reconnect { .. } => "reconnect",
            Self::PermissionDenied { .. } => "permission_denied",
            Self::RemoteControlInput { .. } => "remote_control_input",
            Self::DeviceChanged { .. } => "device_changed",
            Self::AnnotationToggled { .. } => "annotation_toggled",
        }
    }

    fn extras(&self) -> BTreeMap<&'static str, &'static str> {
        let mut extras = BTreeMap::new();
        match self {
            Self::MeetingJoined => {}
            Self::MeetingLeft {
                duration_bucket,
                reconnect_count_bucket,
            } => {
                extras.insert("duration_bucket", duration_bucket.as_str());
                extras.insert("reconnect_count_bucket", reconnect_count_bucket.as_str());
            }
            Self::JoinFailed { reason } => {
                extras.insert("reason", reason.as_str());
            }
            Self::ShareStarted { source } => {
                extras.insert("source", source.as_str());
            }
            Self::ShareStopped { reason } => {
                extras.insert("reason", reason.as_str());
            }
            Self::RemoteAudioSilent { duration_bucket } => {
                extras.insert("duration_bucket", duration_bucket.as_str());
            }
            Self::RemoteVideoStalled { source } => {
                extras.insert("source", source.as_str());
            }
            Self::CaptureRestarted { outcome } | Self::Reconnect { outcome } => {
                extras.insert("outcome", outcome.as_str());
            }
            Self::PermissionDenied { kind } => {
                extras.insert("kind", kind.as_str());
            }
            Self::RemoteControlInput { kind } => {
                extras.insert("kind", kind.as_str());
            }
            Self::DeviceChanged { kind, change } => {
                extras.insert("kind", kind.as_str());
                extras.insert("change", change.as_str());
            }
            Self::AnnotationToggled { state } => {
                extras.insert("state", state.as_str());
            }
        }
        extras
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClassifiedInput {
    Click,
    PointerDown,
    PointerUp,
    Type,
    Paste,
    Scroll,
}

struct InputCoalescer {
    pointer_down: bool,
    last_type: Option<Instant>,
    last_scroll: Option<Instant>,
}

impl InputCoalescer {
    const fn new() -> Self {
        Self {
            pointer_down: false,
            last_type: None,
            last_scroll: None,
        }
    }

    fn note(&mut self, classified: ClassifiedInput, now: Instant) -> Option<RemoteControlInputKind> {
        match classified {
            ClassifiedInput::Click => {
                self.pointer_down = false;
                Some(RemoteControlInputKind::Click)
            }
            ClassifiedInput::PointerDown => {
                self.pointer_down = true;
                None
            }
            ClassifiedInput::PointerUp => {
                if self.pointer_down {
                    self.pointer_down = false;
                    Some(RemoteControlInputKind::Click)
                } else {
                    None
                }
            }
            ClassifiedInput::Type => burst(&mut self.last_type, now, TYPE_IDLE)
                .then_some(RemoteControlInputKind::Type),
            ClassifiedInput::Scroll => burst(&mut self.last_scroll, now, SCROLL_IDLE)
                .then_some(RemoteControlInputKind::Scroll),
            ClassifiedInput::Paste => Some(RemoteControlInputKind::Paste),
        }
    }
}

fn burst(last: &mut Option<Instant>, now: Instant, idle: Duration) -> bool {
    let emit = match *last {
        None => true,
        Some(previous) => now.saturating_duration_since(previous) >= idle,
    };
    *last = Some(now);
    emit
}

pub(crate) fn duration_bucket(duration: Duration) -> DurationBucket {
    if duration < Duration::from_secs(10) {
        DurationBucket::ZeroToTen
    } else if duration < Duration::from_secs(30) {
        DurationBucket::TenToThirty
    } else if duration < Duration::from_secs(120) {
        DurationBucket::ThirtyToOneTwenty
    } else {
        DurationBucket::OneTwentyPlus
    }
}

fn reconnect_count_bucket(count: u32) -> ReconnectCountBucket {
    match count {
        0 => ReconnectCountBucket::Zero,
        1 => ReconnectCountBucket::One,
        2..=4 => ReconnectCountBucket::TwoToFour,
        _ => ReconnectCountBucket::FivePlus,
    }
}

fn classify_remote_control(message: &RemoteControlMessage) -> Option<ClassifiedInput> {
    match message.message_type {
        RemoteControlType::Pointer => match message.action {
            Some(RemoteControlAction::Click) => Some(ClassifiedInput::Click),
            Some(RemoteControlAction::Down) => Some(ClassifiedInput::PointerDown),
            Some(RemoteControlAction::Up) => Some(ClassifiedInput::PointerUp),
            Some(RemoteControlAction::Move) | Some(RemoteControlAction::Unknown) | None => None,
        },
        RemoteControlType::Key => Some(ClassifiedInput::Type),
        RemoteControlType::Text => Some(ClassifiedInput::Paste),
        RemoteControlType::Wheel => Some(ClassifiedInput::Scroll),
        RemoteControlType::Request
        | RemoteControlType::Release
        | RemoteControlType::Status
        | RemoteControlType::Result
        | RemoteControlType::Unknown => None,
    }
}

pub(crate) fn video_stall_source(source: &str) -> VideoStallSource {
    if source.contains("stats-frame-starvation") || source.starts_with("stats-") {
        VideoStallSource::Stats
    } else if source.contains("gallery") || source.contains("livekit-js") {
        VideoStallSource::Gallery
    } else {
        VideoStallSource::Native
    }
}

fn api_key() -> Option<String> {
    std::env::var("PETAL_POSTHOG_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            option_env!("PETAL_POSTHOG_KEY")
                .map(str::to_string)
                .filter(|value| !value.trim().is_empty())
        })
        .map(|value| value.trim().to_string())
        .filter(|value| value.starts_with("phc_"))
}

fn host() -> String {
    std::env::var("PETAL_POSTHOG_HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            option_env!("PETAL_POSTHOG_HOST")
                .map(str::to_string)
                .filter(|value| !value.trim().is_empty())
        })
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_HOST.to_string())
}

fn arch_label() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

fn os_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        "linux" => "linux",
        other => other,
    }
}

fn os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        command_stdout("sw_vers", &["-productVersion"]).unwrap_or_else(|| "unknown".into())
    }
    #[cfg(target_os = "windows")]
    {
        windows_os_version().unwrap_or_else(|| "unknown".into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "unknown".into()
    }
}

/// Windows OS version WITHOUT spawning a console process. `cmd /C ver` used
/// to run here, but on default-terminal hosts (Windows 11's Windows
/// Terminal) every spawn flashed a terminal window at each analytics event,
/// and it ran even when PostHog was disabled (018A/B). `RtlGetVersion`
/// (ntdll) reports the real version in-process.
#[cfg(target_os = "windows")]
fn windows_os_version() -> Option<String> {
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    let status = unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut info) };
    (status == windows::Win32::Foundation::NTSTATUS(0)).then(|| {
        format!(
            "{}.{}.{}",
            info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
        )
    })
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn common_properties() -> BTreeMap<String, String> {
    let mut properties = BTreeMap::new();
    properties.insert(
        "build_version".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    properties.insert("os".to_string(), os_label().to_string());
    properties.insert("os_version".to_string(), os_version());
    properties.insert("arch".to_string(), arch_label().to_string());
    properties.insert("client".to_string(), "native".to_string());
    properties
}

fn new_distinct_id() -> String {
    let mut bytes = [0u8; 16];
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    bytes[..8].copy_from_slice(&nanos.to_le_bytes());
    bytes[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    #[cfg(unix)]
    {
        if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            let _ = file.read_exact(&mut bytes);
        }
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn load_or_create_distinct_id(app_data_dir: &Path) -> String {
    let path = app_data_dir.join(DISTINCT_ID_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if trimmed.len() == 32 && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return trimmed.to_string();
        }
    }
    let id = new_distinct_id();
    if let Err(error) = std::fs::create_dir_all(app_data_dir) {
        log::warn!("analytics: could not create app data dir for anonymous id: {error}");
        return id;
    }
    if let Err(error) = std::fs::write(&path, &id) {
        log::warn!("analytics: could not persist anonymous id: {error}");
    }
    id
}

/// #908 review blocker 4: a `reqwest::Error`'s `Display` can embed the
/// request URL, and the configured host can carry userinfo, query
/// parameters, or an internal IP -- neither is safe to log verbatim under
/// `logging.rs`'s redaction policy. Classify into this fixed, secret-free
/// set instead; `.as_str()` is the only thing that ever reaches a log line.
///
/// The classification doubles as the should-fix from the same review: only
/// a transient condition is worth retrying. A 401 (bad/rotated key) or any
/// other non-transient 4xx will never succeed on retry, so retrying it just
/// holds the single-consumer worker for `RETRY_BACKOFF` + a second
/// `SEND_TIMEOUT` for nothing -- exactly the throughput hit ("~0.5 ->
/// ~0.22 events/sec") that makes a real outage overflow the queue faster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SendOutcome {
    Timeout,
    Connect,
    Http401,
    Http408,
    Http429,
    Http5xx,
    HttpOther,
    TransportOther,
}

impl SendOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Http401 => "http_401",
            Self::Http408 => "http_408",
            Self::Http429 => "http_429",
            Self::Http5xx => "http_5xx",
            Self::HttpOther => "http_other",
            Self::TransportOther => "transport_other",
        }
    }

    /// Worth a retry: a blip that a second attempt might sail through.
    /// Everything else (401, other 4xx) is permanent -- the payload or
    /// credential is wrong and trying again changes nothing.
    fn is_transient(self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Connect | Self::Http408 | Self::Http429 | Self::Http5xx
        )
    }
}

/// One send attempt. A non-2xx response counts as a failure just like a
/// transport error (#908) -- the old code only ever checked `Result::Err`
/// from `send()`, so a persistent 4xx/5xx (bad key, malformed payload,
/// PostHog-side rejection) looked identical to success. Never calls
/// `.to_string()` on the `reqwest::Error` or touches the URL -- see
/// `SendOutcome`'s doc comment.
async fn attempt_send(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<(), SendOutcome> {
    match client.post(url).json(body).send().await {
        Ok(response) if response.status().is_success() => Ok(()),
        Ok(response) => Err(match response.status().as_u16() {
            401 => SendOutcome::Http401,
            408 => SendOutcome::Http408,
            429 => SendOutcome::Http429,
            code if (500..600).contains(&code) => SendOutcome::Http5xx,
            _ => SendOutcome::HttpOther,
        }),
        Err(error) if error.is_timeout() => Err(SendOutcome::Timeout),
        Err(error) if error.is_connect() => Err(SendOutcome::Connect),
        Err(_) => Err(SendOutcome::TransportOther),
    }
}

/// Bounded retry: at most `MAX_SEND_RETRIES` extra attempts, each after a
/// fixed short backoff, and ONLY for a transient outcome (see
/// `SendOutcome::is_transient`) -- a permanent failure (401, other 4xx)
/// returns immediately instead of wasting a retry cycle on something that
/// cannot succeed. Cannot wedge the worker (the loop always terminates) and
/// cannot grow memory (no buffering beyond the one in-flight `body`).
async fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<(), SendOutcome> {
    let mut attempt = 0u32;
    loop {
        match attempt_send(client, url, body).await {
            Ok(()) => return Ok(()),
            Err(outcome) => {
                attempt += 1;
                if !outcome.is_transient() || attempt > MAX_SEND_RETRIES {
                    return Err(outcome);
                }
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
        }
    }
}

fn start_worker() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<serde_json::Value>(QUEUE_CAP);
    if WORKER.set(tx).is_err() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let Some(api_key) = api_key() else {
            return;
        };
        let host = host();
        let url = format!("{host}{CAPTURE_PATH}");
        // #908: the old fallback (`unwrap_or_else(|_| reqwest::Client::new())`)
        // built a client with NO timeout on a builder failure. A blackholed
        // network would then hang `attempt_send` forever -- the worker never
        // reaches `PENDING.fetch_sub`, so PENDING never returns to zero and
        // every later `flush()` burns its full timeout for the rest of the
        // process's life. A builder failure is effectively never expected in
        // practice (TLS backend init), so treat it as fatal for the worker
        // instead of silently downgrading its safety guarantee.
        let client = match reqwest::Client::builder().timeout(SEND_TIMEOUT).build() {
            Ok(client) => HTTP_CLIENT.get_or_init(|| client),
            Err(_) => {
                log::warn!(
                    "analytics: could not build HTTP client -- product events disabled this run"
                );
                return;
            }
        };
        // #908: a successful start was completely silent before this --
        // "did analytics even initialize?" needed a debugger. One line, no
        // key material and no raw host (see `logging.rs`'s redaction policy
        // and #908 review blocker 4 -- a configurable host can carry
        // userinfo, query parameters, or an internal IP).
        log::info!("analytics: worker started");
        while let Some(mut body) = rx.recv().await {
            body["api_key"] = serde_json::Value::String(api_key.clone());
            let result = send_with_retry(client, &url, &body).await;
            let message = FAILURE_TRACKER.lock_unpoisoned().note_result(
                result.is_ok(),
                Instant::now(),
                result.err().map(SendOutcome::as_str),
            );
            if let Some(message) = message {
                log::warn!("{message}");
            }
            if result.is_err() {
                // #908 review blocker 2: this event may have been carrying
                // an earlier `dropped_since_last` count (see
                // `take_dropped_since_last`/`build_body`). That count was
                // only ever restored when `try_send` itself failed --  if
                // the event reached the queue but then failed HTTP delivery
                // (exactly the outage scenario where drops are most likely),
                // the count silently vanished. Read it back out of the body
                // and restore it so it still reaches a later event.
                restore_carried_drop_count(&body);
            }
            PENDING.fetch_sub(1, Ordering::Relaxed);
        }
    });
}

/// Wait (up to `timeout`) for every event already accepted onto the queue to
/// be sent (or given up on after retries). Returns promptly once the queue
/// drains -- never delays a healthy-network quit -- and is hard-bounded by
/// `timeout` so a dead network cannot block shutdown.
pub(crate) async fn flush(timeout: Duration) {
    if WORKER.get().is_none() {
        return;
    }
    wait_for_pending_to_drain(timeout).await;
}

/// The polling loop behind `flush()`, split out so tests can drive `PENDING`
/// directly without needing a real worker/key (`flush()` itself is a no-op
/// in every test since `WORKER` is never initialized -- no test bakes a key).
async fn wait_for_pending_to_drain(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while PENDING.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Load the anonymous distinct id and start the capture worker when a key is
/// present. Keyless runs (every local/CI build) stay a no-op.
pub(crate) fn init(app_data_dir: &Path) {
    let _ = DISTINCT_ID.set(load_or_create_distinct_id(app_data_dir));
    if api_key().is_none() {
        if KEYLESS_LOGGED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            log::info!("analytics: PostHog key absent -- product events disabled this run");
        }
        return;
    }
    start_worker();
}

fn in_meeting() -> bool {
    MEETING.lock_unpoisoned().is_some()
}

fn event_properties(event: &Event) -> BTreeMap<String, String> {
    let mut properties = common_properties();
    for (key, value) in event.extras() {
        properties.insert(key.to_string(), value.to_string());
    }
    properties
}

/// Snapshot-and-clear the drop counter for embedding in the next event about
/// to be enqueued. If that event itself fails to enqueue, the caller must add
/// this value back (see `capture()`) so a drop is never silently absorbed --
/// only ever reported later than it happened.
fn take_dropped_since_last() -> u64 {
    DROPPED.swap(0, Ordering::Relaxed)
}

/// Put back the `dropped_since_last` count carried by an event whose send
/// finally failed, so it can ride out on a later one (#908 review blocker 2).
///
/// This exists as a named function, rather than inline in the worker loop,
/// so the regression test can drive the REAL implementation. An earlier
/// version of that test inlined a copy of this logic and asserted on its own
/// copy -- it passed with the production restore deleted, which is no test at
/// all. Keep the worker and the test calling this one function.
fn restore_carried_drop_count(body: &serde_json::Value) {
    if let Some(dropped) = body["properties"]["dropped_since_last"].as_u64() {
        DROPPED.fetch_add(dropped, Ordering::Relaxed);
    }
}

fn build_body(event: &Event, distinct_id: &str, dropped_since_last: u64) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert("$geoip_disable".into(), serde_json::Value::Bool(true));
    props.insert("$ip".into(), serde_json::Value::Null);
    for (key, value) in event_properties(event) {
        props.insert(key, serde_json::Value::String(value));
    }
    if dropped_since_last > 0 {
        // #908: `internal/docs/POSTHOG_EVENT_ALLOWLIST.md` documents this
        // property. It surfaces queue overflow (QUEUE_CAP = 64) in PostHog
        // itself rather than only in a local counter nobody looks at.
        props.insert(
            "dropped_since_last".into(),
            serde_json::Value::from(dropped_since_last),
        );
    }
    serde_json::json!({
        "event": event.name(),
        "distinct_id": distinct_id,
        "properties": props,
    })
}

/// `try_send` onto the bounded channel, counting a miss instead of silently
/// discarding it (#908). No allocation, no blocking -- safe for `capture()`'s
/// hot paths.
///
/// `PENDING` is incremented BEFORE `try_send`, not after. The instant
/// `try_send` returns `Ok`, the item is visible to the worker task on
/// another thread, which can `recv()` and decrement `PENDING` before this
/// function would otherwise get around to incrementing it -- underflowing
/// the counter to `u64::MAX` and making every later `flush()` burn its full
/// timeout, or making a concurrent `flush()` observe a false zero while the
/// item is still queued. Incrementing first (and rolling back on failure,
/// which happens synchronously before the worker ever sees the item) makes
/// both races impossible.
fn enqueue(tx: &tokio::sync::mpsc::Sender<serde_json::Value>, body: serde_json::Value) -> bool {
    PENDING.fetch_add(1, Ordering::Relaxed);
    match tx.try_send(body) {
        Ok(()) => true,
        Err(_) => {
            PENDING.fetch_sub(1, Ordering::Relaxed);
            DROPPED.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

fn capture(event: Event) {
    // Gate BEFORE collecting properties: `common_properties` on Windows runs
    // `os_version` (RtlGetVersion — previously `cmd /C ver`, a console-process
    // spawn). With no PostHog key the events are disabled, and the old spawn
    // flashed a Windows Terminal window at every join/leave/share/unshare on
    // default-terminal hosts (018A/B). The test sink is also gated here
    // first: tests have no key.
    #[cfg(test)]
    {
        if TEST_SINK.with(|sink| {
            if let Some(events) = sink.borrow_mut().as_mut() {
                events.push(CapturedEvent {
                    name: event.name().to_string(),
                    properties: event_properties(&event),
                });
                true
            } else {
                false
            }
        }) {
            return;
        }
    }
    let Some(_key) = api_key() else {
        return;
    };
    let Some(tx) = WORKER.get() else {
        return;
    };
    let distinct_id = DISTINCT_ID.get().cloned().unwrap_or_else(new_distinct_id);
    let dropped = take_dropped_since_last();
    let body = build_body(&event, &distinct_id, dropped);
    if !enqueue(tx, body) {
        // This event's own send was dropped too, so the count it was
        // carrying never reached PostHog. Put it back (plus the drop
        // `enqueue` just counted for this event) for the next attempt.
        DROPPED.fetch_add(dropped, Ordering::Relaxed);
    }
}

pub(crate) fn meeting_joined() {
    *MEETING.lock_unpoisoned() = Some(Meeting {
        joined_at: Instant::now(),
        reconnects: 0,
    });
    capture(Event::MeetingJoined);
}

pub(crate) fn meeting_left() {
    let Some(meeting) = MEETING.lock_unpoisoned().take() else {
        return;
    };
    capture(Event::MeetingLeft {
        duration_bucket: duration_bucket(meeting.joined_at.elapsed()),
        reconnect_count_bucket: reconnect_count_bucket(meeting.reconnects),
    });
}

pub(crate) fn join_failed(reason: JoinFailedReason) {
    capture(Event::JoinFailed { reason });
}

pub(crate) fn join_failed_from_token_error(error: &TokenError) {
    let reason = match error {
        TokenError::MissingEnv(_) => JoinFailedReason::NoBackend,
        TokenError::Timeout => JoinFailedReason::Timeout,
        TokenError::Connect
        | TokenError::Transport
        | TokenError::Backend(_)
        | TokenError::HttpStatus(_) => JoinFailedReason::Network,
        TokenError::InvalidBackendUrl(_) | TokenError::Decode => JoinFailedReason::Token,
        #[cfg(any(test, debug_assertions))]
        TokenError::Jwt(_) => JoinFailedReason::Token,
    };
    join_failed(reason);
}

pub(crate) fn join_failed_from_connect_timeout() {
    join_failed(JoinFailedReason::Timeout);
}

pub(crate) fn join_failed_from_connect_network() {
    join_failed(JoinFailedReason::Network);
}

pub(crate) fn share_started(source: ShareStartedSource) {
    capture(Event::ShareStarted { source });
}

pub(crate) fn share_stopped(reason: ShareStoppedReason) {
    capture(Event::ShareStopped { reason });
}

pub(crate) fn remote_audio_silent(duration: Duration) {
    capture(Event::RemoteAudioSilent {
        duration_bucket: duration_bucket(duration),
    });
}

pub(crate) fn remote_video_stalled(source: &str) {
    capture(Event::RemoteVideoStalled {
        source: video_stall_source(source),
    });
}

pub(crate) fn capture_restarted(outcome: RestartOutcome) {
    capture(Event::CaptureRestarted { outcome });
}

pub(crate) fn reconnect_recovered() {
    if let Some(meeting) = MEETING.lock_unpoisoned().as_mut() {
        meeting.reconnects = meeting.reconnects.saturating_add(1);
    }
    capture(Event::Reconnect {
        outcome: RestartOutcome::Recovered,
    });
}

pub(crate) fn reconnect_failed() {
    capture(Event::Reconnect {
        outcome: RestartOutcome::Failed,
    });
}

pub(crate) fn permission_denied(kind: PermissionKind) {
    capture(Event::PermissionDenied { kind });
}

pub(crate) fn note_remote_control_applied(message: &RemoteControlMessage) {
    note_remote_control_applied_in(&COALESCER, Instant::now(), message);
}

/// Split out so tests drive coalescing with their OWN state and an explicit
/// clock. `COALESCER` is process-global and `remote_control.rs`'s replay
/// worker writes to it from background threads that outlive the test that
/// started them, so a test racing the global silently loses a coalesced
/// event (#868). Never reintroduce `Instant::now()` or `COALESCER` here.
fn note_remote_control_applied_in(
    coalescer: &Mutex<InputCoalescer>,
    now: Instant,
    message: &RemoteControlMessage,
) {
    let Some(classified) = classify_remote_control(message) else {
        return;
    };
    // The coalescer lock must not be held across `capture`.
    let kind = coalescer.lock_unpoisoned().note(classified, now);
    let Some(kind) = kind else {
        return;
    };
    capture(Event::RemoteControlInput { kind });
}

pub(crate) fn device_changed(kind: DeviceKind, change: DeviceChange) {
    if !in_meeting() {
        return;
    }
    if kind == DeviceKind::Display && change == DeviceChange::Reconfigured {
        let mut last = LAST_DISPLAY_RECONFIG.lock_unpoisoned();
        if last.is_some_and(|previous| previous.elapsed() < DISPLAY_RECONFIG_DEBOUNCE) {
            return;
        }
        *last = Some(Instant::now());
    }
    capture(Event::DeviceChanged { kind, change });
}

/// #872: the sharer turned annotation/drawing on or off. That flag is what makes
/// the share overlay capture the cursor, so without this a report of "I cannot
/// click anything" is undiagnosable from telemetry.
pub(crate) fn annotation_toggled(active: bool) {
    capture(Event::AnnotationToggled {
        state: if active { AnnotationState::On } else { AnnotationState::Off },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    // Meeting / coalescer / display-debounce state is process-wide (correct
    // for the running app). Tests that touch it must not overlap.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_sink<R>(run: impl FnOnce() -> R) -> (R, Vec<CapturedEvent>) {
        let _guard = TEST_LOCK.lock_unpoisoned();
        TEST_SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
        *MEETING.lock_unpoisoned() = None;
        *COALESCER.lock_unpoisoned() = InputCoalescer::new();
        *LAST_DISPLAY_RECONFIG.lock_unpoisoned() = None;
        let result = run();
        let events = TEST_SINK.with(|sink| sink.borrow_mut().take().unwrap_or_default());
        *MEETING.lock_unpoisoned() = None;
        *COALESCER.lock_unpoisoned() = InputCoalescer::new();
        *LAST_DISPLAY_RECONFIG.lock_unpoisoned() = None;
        (result, events)
    }

    fn names(events: &[CapturedEvent]) -> Vec<&str> {
        events.iter().map(|event| event.name.as_str()).collect()
    }

    fn extra<'a>(event: &'a CapturedEvent, key: &str) -> &'a str {
        event.properties.get(key).map(String::as_str).unwrap_or("")
    }

    fn fixture(kind: &str, action: Option<&str>) -> RemoteControlMessage {
        let mut value = json!({
            "v": 1,
            "kind": kind,
            "targetUserId": "host",
            "controllerId": "peer",
            "windowId": 1,
            "seq": 1u64,
        });
        if let Some(action) = action {
            value["action"] = json!(action);
        }
        serde_json::from_value(value).expect("remote-control fixture")
    }

    #[test]
    fn allowlist_event_names_are_closed() {
        let names: Vec<_> = [
            Event::MeetingJoined,
            Event::MeetingLeft {
                duration_bucket: DurationBucket::ZeroToTen,
                reconnect_count_bucket: ReconnectCountBucket::Zero,
            },
            Event::JoinFailed {
                reason: JoinFailedReason::Network,
            },
            Event::ShareStarted {
                source: ShareStartedSource::Window,
            },
            Event::ShareStopped {
                reason: ShareStoppedReason::User,
            },
            Event::RemoteAudioSilent {
                duration_bucket: DurationBucket::ZeroToTen,
            },
            Event::RemoteVideoStalled {
                source: VideoStallSource::Native,
            },
            Event::CaptureRestarted {
                outcome: RestartOutcome::Recovered,
            },
            Event::Reconnect {
                outcome: RestartOutcome::Failed,
            },
            Event::PermissionDenied {
                kind: PermissionKind::Mic,
            },
            Event::RemoteControlInput {
                kind: RemoteControlInputKind::Click,
            },
            Event::DeviceChanged {
                kind: DeviceKind::Display,
                change: DeviceChange::Wake,
            },
            Event::AnnotationToggled {
                state: AnnotationState::On,
            },
        ]
        .iter()
        .map(Event::name)
        .collect();
        assert_eq!(names, EVENT_NAMES);
    }

    #[test]
    fn payloads_only_carry_allowlisted_property_keys() {
        let events = [
            Event::MeetingJoined,
            Event::MeetingLeft {
                duration_bucket: DurationBucket::TenToThirty,
                reconnect_count_bucket: ReconnectCountBucket::TwoToFour,
            },
            Event::JoinFailed {
                reason: JoinFailedReason::Timeout,
            },
            Event::ShareStarted {
                source: ShareStartedSource::Picker,
            },
            Event::ShareStopped {
                reason: ShareStoppedReason::WindowGone,
            },
            Event::RemoteAudioSilent {
                duration_bucket: DurationBucket::TenToThirty,
            },
            Event::RemoteVideoStalled {
                source: VideoStallSource::Gallery,
            },
            Event::CaptureRestarted {
                outcome: RestartOutcome::Failed,
            },
            Event::Reconnect {
                outcome: RestartOutcome::Recovered,
            },
            Event::PermissionDenied {
                kind: PermissionKind::Screen,
            },
            Event::RemoteControlInput {
                kind: RemoteControlInputKind::Scroll,
            },
            Event::DeviceChanged {
                kind: DeviceKind::Mic,
                change: DeviceChange::Switched,
            },
        ];
        for event in events {
            let properties = event_properties(&event);
            for key in properties.keys() {
                assert!(
                    COMMON_KEYS.contains(&key.as_str())
                        || event.extras().contains_key(key.as_str()),
                    "event {} leaked property {key}",
                    event.name()
                );
            }
            for forbidden in [
                "room",
                "room_name",
                "identity",
                "window_title",
                "sid",
                "ip",
                "dsn",
                "token",
                "device_name",
                "display_id",
                "key",
                "text",
                "x",
                "y",
            ] {
                assert!(
                    !properties.contains_key(forbidden),
                    "event {} must not include {forbidden}",
                    event.name()
                );
            }
            assert_eq!(
                properties.get("client").map(String::as_str),
                Some("native"),
                "event {} must tag client=native",
                event.name()
            );
        }
    }

    #[test]
    fn duration_and_reconnect_buckets_match_allowlist() {
        assert_eq!(duration_bucket(Duration::from_secs(9)), DurationBucket::ZeroToTen);
        assert_eq!(
            duration_bucket(Duration::from_secs(10)),
            DurationBucket::TenToThirty
        );
        assert_eq!(
            duration_bucket(Duration::from_secs(29)),
            DurationBucket::TenToThirty
        );
        assert_eq!(
            duration_bucket(Duration::from_secs(30)),
            DurationBucket::ThirtyToOneTwenty
        );
        assert_eq!(
            duration_bucket(Duration::from_secs(120)),
            DurationBucket::OneTwentyPlus
        );
        assert_eq!(reconnect_count_bucket(0).as_str(), "0");
        assert_eq!(reconnect_count_bucket(1).as_str(), "1");
        assert_eq!(reconnect_count_bucket(4).as_str(), "2_4");
        assert_eq!(reconnect_count_bucket(5).as_str(), "5_plus");
    }

    #[test]
    fn token_errors_map_onto_join_failed_reasons() {
        assert_eq!(
            match &TokenError::MissingEnv("PETAL_BACKEND_URL") {
                TokenError::MissingEnv(_) => JoinFailedReason::NoBackend,
                _ => unreachable!(),
            },
            JoinFailedReason::NoBackend
        );
        let (_, events) = with_sink(|| {
            join_failed_from_token_error(&TokenError::Timeout);
            join_failed_from_token_error(&TokenError::Connect);
            join_failed_from_token_error(&TokenError::Decode);
        });
        assert_eq!(
            events
                .iter()
                .map(|event| extra(event, "reason"))
                .collect::<Vec<_>>(),
            ["timeout", "network", "token"]
        );
    }

    #[test]
    fn two_clicks_are_two_events_keys_coalesce_wheels_coalesce() {
        let t0 = Instant::now();
        let mut coalescer = InputCoalescer::new();
        assert_eq!(
            coalescer.note(ClassifiedInput::Click, t0),
            Some(RemoteControlInputKind::Click)
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::Click, t0 + Duration::from_millis(20)),
            Some(RemoteControlInputKind::Click)
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::Type, t0),
            Some(RemoteControlInputKind::Type)
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::Type, t0 + Duration::from_millis(200)),
            None
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::Type, t0 + Duration::from_millis(1300)),
            Some(RemoteControlInputKind::Type)
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::Scroll, t0),
            Some(RemoteControlInputKind::Scroll)
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::Scroll, t0 + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::Scroll, t0 + Duration::from_millis(600)),
            Some(RemoteControlInputKind::Scroll)
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::PointerDown, t0),
            None
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::PointerUp, t0 + Duration::from_millis(30)),
            Some(RemoteControlInputKind::Click)
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::Click, t0),
            Some(RemoteControlInputKind::Click)
        );
        assert_eq!(
            coalescer.note(ClassifiedInput::PointerUp, t0 + Duration::from_millis(10)),
            None
        );
    }

    #[test]
    fn classify_ignores_moves_and_never_reads_key_text() {
        assert_eq!(
            classify_remote_control(&fixture("pointer", Some("move"))),
            None
        );
        assert_eq!(
            classify_remote_control(&fixture("pointer", Some("click"))),
            Some(ClassifiedInput::Click)
        );
        assert_eq!(
            classify_remote_control(&fixture("key", None)),
            Some(ClassifiedInput::Type)
        );
        assert_eq!(
            classify_remote_control(&fixture("text", None)),
            Some(ClassifiedInput::Paste)
        );
        assert_eq!(
            classify_remote_control(&fixture("wheel", None)),
            Some(ClassifiedInput::Scroll)
        );
    }

    #[test]
    fn meeting_leave_uses_reconnect_count_and_device_changes_require_a_meeting() {
        let (_, events) = with_sink(|| {
            device_changed(DeviceKind::Mic, DeviceChange::Switched);
            meeting_joined();
            reconnect_recovered();
            device_changed(DeviceKind::Mic, DeviceChange::Switched);
            meeting_left();
        });
        assert_eq!(
            names(&events),
            [
                "meeting_joined",
                "reconnect",
                "device_changed",
                "meeting_left"
            ]
        );
        assert_eq!(extra(&events[3], "reconnect_count_bucket"), "1");
        assert_eq!(extra(&events[2], "kind"), "mic");
        assert_eq!(extra(&events[2], "change"), "switched");
    }

    #[test]
    fn display_reconfigure_debounces_inside_one_second() {
        let (_, events) = with_sink(|| {
            meeting_joined();
            device_changed(DeviceKind::Display, DeviceChange::Reconfigured);
            device_changed(DeviceKind::Display, DeviceChange::Reconfigured);
            device_changed(DeviceKind::Display, DeviceChange::Sleep);
        });
        let device: Vec<_> = events
            .iter()
            .filter(|event| event.name == "device_changed")
            .map(|event| extra(event, "change"))
            .collect();
        assert_eq!(device, ["reconfigured", "sleep"]);
    }

    #[test]
    fn video_stall_source_maps_watchdog_strings() {
        assert_eq!(
            video_stall_source("stats-frame-starvation"),
            VideoStallSource::Stats
        );
        assert_eq!(
            video_stall_source("gallery-bridge-freeze-watchdog"),
            VideoStallSource::Gallery
        );
        assert_eq!(
            video_stall_source("livekit-js-stream-state"),
            VideoStallSource::Gallery
        );
        assert_eq!(
            video_stall_source("native-no-frame-watchdog"),
            VideoStallSource::Native
        );
        assert_eq!(
            video_stall_source("livekit-rust-track-muted"),
            VideoStallSource::Native
        );
    }

    #[test]
    fn persist_anonymous_id_is_stable_and_hex() {
        let dir = std::env::temp_dir().join(format!(
            "petal-analytics-id-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = load_or_create_distinct_id(&dir);
        let second = load_or_create_distinct_id(&dir);
        assert_eq!(first, second);
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capture_without_a_key_does_not_need_a_worker() {
        assert!(WORKER.get().is_none());
        let (_, events) = with_sink(|| {
            share_started(ShareStartedSource::Window);
        });
        assert_eq!(names(&events), ["share_started"]);
        assert!(WORKER.get().is_none());
    }

    #[test]
    fn note_remote_control_applied_uses_the_coalescer() {
        // Deterministic clock + a coalescer local to this test: see #868.
        let coalescer = Mutex::new(InputCoalescer::new());
        let t0 = Instant::now();
        let (_, events) = with_sink(|| {
            let at = |now: Instant, kind: &str, action: Option<&str>| {
                note_remote_control_applied_in(&coalescer, now, &fixture(kind, action));
            };
            at(t0, "pointer", Some("move"));
            at(t0, "pointer", Some("click"));
            at(t0, "pointer", Some("click"));
            at(t0, "key", None);
            // Inside TYPE_IDLE: coalesced into the burst above.
            at(t0 + TYPE_IDLE - Duration::from_millis(1), "key", None);
            // Idle window elapsed. It is measured from the PREVIOUS key, not
            // from the start of the burst, so this must clear the key above.
            at(t0 + TYPE_IDLE * 2, "key", None);
        });
        assert_eq!(
            events
                .iter()
                .map(|event| extra(event, "kind"))
                .collect::<Vec<_>>(),
            ["click", "click", "type", "type"]
        );
    }

    #[test]
    fn a_concurrent_global_coalescer_writer_cannot_perturb_a_local_one() {
        // `remote_control.rs`'s replay worker calls the global entry point
        // from background threads that outlive their own test (#868). Drive
        // the global hard while a local coalescer runs the same sequence: the
        // local result must be unaffected. The writer's own `capture` calls
        // land in ITS thread's unset `TEST_SINK`, so only the shared
        // `COALESCER` was ever the hazard.
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    note_remote_control_applied(&fixture("key", None));
                }
            })
        };

        let coalescer = Mutex::new(InputCoalescer::new());
        let t0 = Instant::now();
        let (_, events) = with_sink(|| {
            note_remote_control_applied_in(&coalescer, t0, &fixture("key", None));
        });

        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        assert_eq!(
            events
                .iter()
                .map(|event| extra(event, "kind"))
                .collect::<Vec<_>>(),
            ["type"]
        );
    }

    // ---- #908: drop counter, rate-limited failure logging, bounded retry,
    // and bounded flush. ----

    #[test]
    fn enqueue_counts_a_drop_when_the_queue_is_full() {
        let _guard = TEST_LOCK.lock_unpoisoned();
        DROPPED.store(0, Ordering::Relaxed);
        PENDING.store(0, Ordering::Relaxed);
        let (tx, _rx) = tokio::sync::mpsc::channel::<serde_json::Value>(1);
        assert!(enqueue(&tx, json!({"n": 0})));
        assert_eq!(PENDING.load(Ordering::Relaxed), 1);
        assert_eq!(DROPPED.load(Ordering::Relaxed), 0);
        // Capacity 1, already holds one unread item: this one overflows,
        // matching #908's real trigger (a `remote_video_stalled` burst
        // against QUEUE_CAP).
        assert!(!enqueue(&tx, json!({"n": 1})));
        assert_eq!(
            DROPPED.load(Ordering::Relaxed),
            1,
            "a discarded try_send must be counted, not silently swallowed"
        );
        assert_eq!(
            PENDING.load(Ordering::Relaxed),
            1,
            "a dropped send is never pending"
        );
        DROPPED.store(0, Ordering::Relaxed);
        PENDING.store(0, Ordering::Relaxed);
    }

    #[test]
    fn pending_never_underflows_under_concurrent_enqueue_and_drain() {
        // Regression for the send-before-increment race a reviewer caught
        // (#908): PENDING must be incremented BEFORE the item becomes
        // visible to a consumer via `try_send`, or a consumer racing ahead
        // of the producer's increment can decrement first and underflow
        // PENDING to u64::MAX -- which makes every later `flush()` burn its
        // full timeout forever. Drive a real producer/consumer race (not
        // just a single-threaded call) so the ordering actually matters.
        let _guard = TEST_LOCK.lock_unpoisoned();
        PENDING.store(0, Ordering::Relaxed);
        DROPPED.store(0, Ordering::Relaxed);
        static SAW_UNDERFLOW: AtomicBool = AtomicBool::new(false);
        SAW_UNDERFLOW.store(false, Ordering::Relaxed);
        const N: usize = 2000;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<serde_json::Value>(8);
        let drainer = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("build current-thread runtime");
            rt.block_on(async {
                let mut received = 0usize;
                while received < N {
                    if rx.recv().await.is_some() {
                        // The actual defect, caught where it happens rather
                        // than in the final total: an item is receivable, so
                        // a correct producer has ALREADY incremented and
                        // PENDING must be >= 1. Observing 0 here means the
                        // increment came after `try_send` and this decrement
                        // is about to wrap PENDING to u64::MAX. Asserting only
                        // the post-drain total cannot see this -- the wrap
                        // adds back to zero, so the sum is conserved either
                        // way and the test would pass against the bug.
                        if PENDING.load(Ordering::Relaxed) == 0 {
                            SAW_UNDERFLOW.store(true, Ordering::Relaxed);
                        }
                        PENDING.fetch_sub(1, Ordering::Relaxed);
                        received += 1;
                    } else {
                        break;
                    }
                }
            });
        });
        for i in 0..N {
            // Retry on a full queue like a real producer would eventually
            // succeed -- capacity is small (8) on purpose, to force the
            // producer and drainer to race hard against each other.
            while !enqueue(&tx, json!({"n": i})) {
                std::thread::yield_now();
            }
        }
        drainer.join().expect("drainer thread panicked");
        assert!(
            !SAW_UNDERFLOW.load(Ordering::Relaxed),
            "a consumer saw PENDING == 0 while holding a received item -- the \
             increment is racing behind `try_send` and PENDING wrapped to \
             u64::MAX, which makes every later flush() burn its full timeout"
        );
        assert_eq!(
            PENDING.load(Ordering::Relaxed),
            0,
            "PENDING must land exactly at zero once every item is drained -- \
             any underflow would show up as a huge number here, not zero"
        );
        DROPPED.store(0, Ordering::Relaxed);
    }

    #[test]
    fn take_dropped_since_last_reads_and_clears_the_counter() {
        let _guard = TEST_LOCK.lock_unpoisoned();
        DROPPED.store(5, Ordering::Relaxed);
        assert_eq!(take_dropped_since_last(), 5);
        assert_eq!(DROPPED.load(Ordering::Relaxed), 0);
        assert_eq!(take_dropped_since_last(), 0);
    }

    #[test]
    fn dropped_since_last_property_appears_only_when_positive() {
        let clean = build_body(&Event::MeetingJoined, "id", 0);
        assert!(
            clean["properties"].get("dropped_since_last").is_none(),
            "must not add a zero-value property to every event"
        );
        let dirty = build_body(&Event::MeetingJoined, "id", 7);
        assert_eq!(dirty["properties"]["dropped_since_last"], json!(7));
    }

    #[test]
    fn failure_tracker_logs_first_failure_then_rate_limits_failure_and_recovery_alike() {
        // #908 review blocker 3: failure and recovery lines now share ONE
        // cooldown clock. A recovery inside the cooldown window is silent
        // (though it still resets the streak), and the timestamp is never
        // cleared on recovery -- so a failure immediately afterward, still
        // inside the same window, stays silent too.
        let mut tracker = FailureTracker::new();
        let t0 = Instant::now();

        // First failure in a clean streak: log immediately.
        let first = tracker.note_result(false, t0, Some("boom"));
        assert!(first.is_some(), "the first failure must be visible");
        assert!(first.unwrap().contains("1 consecutive"));

        // Every failure inside the rate-limit window is silent -- this is
        // what keeps it from becoming a per-event line (#905).
        for i in 0..50 {
            let at = t0 + Duration::from_millis(100 * i);
            assert!(
                tracker.note_result(false, at, Some("boom")).is_none(),
                "failure #{i} inside the rate-limit window must not log"
            );
        }

        // A recovery inside the SAME window is silent too -- it must not
        // reset the cooldown clock (that was the bug: it used to clear
        // last_logged_at, making the very next failure log immediately).
        let quiet_recovery = tracker.note_result(true, t0 + Duration::from_secs(5), None);
        assert!(
            quiet_recovery.is_none(),
            "a recovery inside the cooldown window must stay silent"
        );

        // A failure right after that quiet recovery must ALSO stay silent --
        // this is exactly the alternation the old code turned into a
        // per-event storm.
        assert!(
            tracker
                .note_result(false, t0 + Duration::from_secs(6), Some("boom"))
                .is_none(),
            "a failure immediately after a silent recovery must not log"
        );

        // Once the interval has elapsed while still failing: one summary
        // line. The streak was reset by the quiet recovery above, so the
        // count reflects only the failures since then.
        let summary = tracker.note_result(
            false,
            t0 + FAILURE_LOG_INTERVAL + Duration::from_secs(1),
            Some("boom"),
        );
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert!(summary.contains("consecutive failure"));

        // A recovery once the window has elapsed again DOES log.
        let recovered = tracker.note_result(
            true,
            t0 + 2 * FAILURE_LOG_INTERVAL + Duration::from_secs(2),
            None,
        );
        assert!(recovered.is_some());
        assert!(recovered.unwrap().contains("recovered"));

        // A success with no prior failures never logs -- success is never a
        // per-event line either.
        assert!(tracker.note_result(true, t0, None).is_none());
    }

    #[test]
    fn alternating_failures_and_recoveries_never_exceed_one_log_line_per_interval() {
        // #908 review blocker 3 (Sol): "add an alternating failure/success
        // stress test asserting a hard maximum number of log lines per
        // minute." Drive 200 alternating outcomes, all inside one
        // FAILURE_LOG_INTERVAL, and assert the total line count stays at
        // the hard cap of 1 (the very first failure) -- not 200.
        let mut tracker = FailureTracker::new();
        let t0 = Instant::now();
        let mut lines = 0u32;
        for i in 0..200u64 {
            let ok = i % 2 == 1; // fail, recover, fail, recover, ...
            let at = t0 + Duration::from_millis(i * 100); // all within 20s
            if tracker.note_result(ok, at, Some("boom")).is_some() {
                lines += 1;
            }
        }
        assert_eq!(
            lines, 1,
            "200 alternating outcomes inside one cooldown window must produce \
             exactly one log line, not one per transition"
        );

        // Spanning several intervals, the cap is one line PER interval, not
        // one forever -- a real multi-hour outage must still surface.
        let mut lines_across_many_intervals = 0u32;
        let mut tracker = FailureTracker::new();
        for i in 0..20u64 {
            let ok = i % 2 == 1;
            let at = t0 + Duration::from_secs(i * 90); // 90s apart: > FAILURE_LOG_INTERVAL
            if tracker.note_result(ok, at, Some("boom")).is_some() {
                lines_across_many_intervals += 1;
            }
        }
        assert!(
            lines_across_many_intervals >= 10,
            "spaced-out alternation must still surface roughly one line per \
             interval, not be permanently silenced: got {lines_across_many_intervals}"
        );
    }

    #[tokio::test]
    async fn wait_for_pending_drains_promptly_once_the_queue_empties() {
        let _guard = TEST_LOCK.lock_unpoisoned();
        PENDING.store(1, Ordering::Relaxed);
        let drainer = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            PENDING.fetch_sub(1, Ordering::Relaxed);
        });
        let start = Instant::now();
        wait_for_pending_to_drain(Duration::from_millis(500)).await;
        let elapsed = start.elapsed();
        drainer.await.unwrap();
        assert_eq!(PENDING.load(Ordering::Relaxed), 0);
        assert!(
            elapsed < Duration::from_millis(300),
            "flush must return as soon as the queue drains on a healthy \
             network, not wait out the full bound: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_pending_is_bounded_when_the_queue_never_drains() {
        let _guard = TEST_LOCK.lock_unpoisoned();
        PENDING.store(1, Ordering::Relaxed);
        let start = Instant::now();
        wait_for_pending_to_drain(Duration::from_millis(100)).await;
        let elapsed = start.elapsed();
        PENDING.store(0, Ordering::Relaxed);
        assert!(
            elapsed >= Duration::from_millis(90),
            "must actually wait for the bound, not return instantly: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "a dead network must not delay quit past the bound: {elapsed:?}"
        );
    }

    /// Minimal single-request-at-a-time HTTP stub for exercising
    /// `send_with_retry` without any real network dependency. `responses` is
    /// consumed one per accepted connection, written back verbatim as the
    /// full HTTP response (status line + headers + body).
    fn spawn_fake_server(responses: Vec<&'static str>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/")
    }

    fn closed_port_url() -> String {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        // Nothing is listening now. How that manifests is PLATFORM-SPECIFIC:
        // macOS/Linux loopback sends RST and the connect fails immediately,
        // while on Windows CI the SYN is dropped and each attempt instead
        // burns the full `SEND_TIMEOUT`. Tests must therefore bound
        // themselves by the retry constants, never by "a refusal is fast".
        drop(listener);
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn send_with_retry_recovers_from_one_transient_failure() {
        let url = spawn_fake_server(vec![
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n",
        ]);
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .unwrap();
        let body = json!({"event": "test"});
        let result = send_with_retry(&client, &url, &body).await;
        assert!(result.is_ok(), "one retry must recover a transient failure: {result:?}");
    }

    #[tokio::test]
    async fn send_with_retry_gives_up_after_max_retries_within_a_bounded_time() {
        let url = closed_port_url();
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .unwrap();
        let body = json!({"event": "test"});
        // The worst case is every attempt burning its full timeout, plus one
        // backoff between them. Derive it from the constants so the test
        // states the actual guarantee -- "at most MAX_SEND_RETRIES extra
        // attempts" -- instead of a number that happens to hold on one OS.
        let worst_case = SEND_TIMEOUT * (MAX_SEND_RETRIES + 1) + RETRY_BACKOFF * MAX_SEND_RETRIES;
        let start = Instant::now();
        let result = tokio::time::timeout(worst_case * 2, send_with_retry(&client, &url, &body))
            .await
            .expect("send_with_retry must terminate, never wedge the worker");
        let elapsed = start.elapsed();
        assert!(result.is_err(), "a fully refused connection must end in failure");
        assert!(
            elapsed >= RETRY_BACKOFF,
            "must actually back off before the retry: {elapsed:?}"
        );
        assert!(
            elapsed < worst_case + Duration::from_secs(1),
            "retry must stay bounded by {MAX_SEND_RETRIES} extra attempt(s) \
             (worst case {worst_case:?}), never loop: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_non_success_status_counts_as_a_failure_not_a_silent_success() {
        // #908: the old code only checked transport-level `Result::Err`, so a
        // persistent 4xx/5xx looked identical to success. Only ONE response
        // is queued (not two): a 401 is permanent and must NOT be retried
        // (see the next test), so the server must never see a second
        // connection.
        let url = spawn_fake_server(vec!["HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n"]);
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .unwrap();
        let body = json!({"event": "test"});
        let result = send_with_retry(&client, &url, &body).await;
        assert_eq!(
            result,
            Err(SendOutcome::Http401),
            "a rejected request must not be treated as sent"
        );
    }

    #[tokio::test]
    async fn a_permanent_401_is_not_retried() {
        // #908 review should-fix (Sol): retrying a permanent failure (bad/
        // rotated key) only holds the single-consumer worker hostage for
        // RETRY_BACKOFF + a second SEND_TIMEOUT while a real outage
        // overflows the queue faster. Assert it returns near-instantly --
        // if it were retrying, RETRY_BACKOFF (500ms) alone would blow this
        // bound.
        let url = spawn_fake_server(vec!["HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n"]);
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .unwrap();
        let body = json!({"event": "test"});
        let start = Instant::now();
        let result = send_with_retry(&client, &url, &body).await;
        let elapsed = start.elapsed();
        assert_eq!(result, Err(SendOutcome::Http401));
        assert!(
            elapsed < Duration::from_millis(300),
            "a permanent failure must return immediately, not after a retry backoff: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_transient_5xx_is_retried_and_a_permanent_4xx_is_not() {
        // Direct coverage of `SendOutcome::is_transient`'s classification,
        // the thing the retry-scoping should-fix and blocker-4 privacy fix
        // both hang off of.
        assert!(SendOutcome::Http5xx.is_transient());
        assert!(SendOutcome::Http408.is_transient());
        assert!(SendOutcome::Http429.is_transient());
        assert!(SendOutcome::Timeout.is_transient());
        assert!(SendOutcome::Connect.is_transient());
        assert!(!SendOutcome::Http401.is_transient());
        assert!(!SendOutcome::HttpOther.is_transient());
        assert!(!SendOutcome::TransportOther.is_transient());

        // None of the classified strings can carry a URL, host, or key --
        // they're a fixed, hardcoded set (#908 review blocker 4).
        for outcome in [
            SendOutcome::Timeout,
            SendOutcome::Connect,
            SendOutcome::Http401,
            SendOutcome::Http408,
            SendOutcome::Http429,
            SendOutcome::Http5xx,
            SendOutcome::HttpOther,
            SendOutcome::TransportOther,
        ] {
            let s = outcome.as_str();
            assert!(!s.contains("://"), "must never look like a URL: {s}");
            assert!(!s.contains('.'), "must never look like a hostname: {s}");
        }
    }

    #[tokio::test]
    async fn a_dropped_count_carried_by_an_event_that_fails_http_delivery_is_restored() {
        // #908 review blocker 2 (both reviewers): `take_dropped_since_last`
        // was only ever restored when `try_send` itself failed. If the
        // event reached the queue but then failed HTTP delivery -- the
        // outage scenario where drops are most likely -- the count in its
        // body vanished. This exercises the real worker loop end-to-end
        // (not just the helper) against a server that always 500s, and
        // checks DROPPED ends up holding the count back.
        let _guard = TEST_LOCK.lock_unpoisoned();
        DROPPED.store(0, Ordering::Relaxed);
        let dropped_before = take_dropped_since_last();
        assert_eq!(dropped_before, 0);
        DROPPED.store(9, Ordering::Relaxed);
        let carried = take_dropped_since_last();
        assert_eq!(carried, 9);
        assert_eq!(DROPPED.load(Ordering::Relaxed), 0);

        let body = build_body(&Event::MeetingJoined, "id", carried);
        assert_eq!(body["properties"]["dropped_since_last"], json!(9));

        // Simulate the worker's failure path: a server that always fails,
        // exhausting both attempts.
        let url = spawn_fake_server(vec![
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n",
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n",
        ]);
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .unwrap();
        let result = send_with_retry(&client, &url, &body).await;
        assert!(result.is_err(), "both attempts were configured to fail");

        // Call the SAME function the worker's failure branch calls. Do not
        // inline a copy of it here: a previous version of this test did, and
        // passed with the production restore deleted.
        if result.is_err() {
            restore_carried_drop_count(&body);
        }
        assert_eq!(
            DROPPED.load(Ordering::Relaxed),
            9,
            "the count carried by an event that failed HTTP delivery must be \
             restored, not silently lost"
        );
        DROPPED.store(0, Ordering::Relaxed);
    }
}
