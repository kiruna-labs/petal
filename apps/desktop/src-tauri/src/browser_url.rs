//! Browser URL extraction for shared browser windows.
//!
//! Petal sends the current browser tab URL as LiveKit participant metadata
//! alongside the existing shared-window title/scale metadata. This mirrors the
//! takt project's browser-context capture: use JavaScript for Automation
//! (JXA) against the shared window's OWNING PROCESS (by pid, never by bundle
//! id -- two instances of one browser share a bundle id and Apple Events by
//! bundle id reach the wrong one), keep the call timeout-bound, and return a typed
//! [`UrlExtraction`] outcome for every case -- including "not a recognised
//! browser" -- instead of guessing from titles or pixels.

use std::time::Duration;

/// Outcome of one URL-extraction attempt for a shared browser window.
///
/// A typed outcome (rather than `Option<String>`) is the whole point of
/// #915: every failure path used to collapse into `None`, indistinguishable
/// from "not a browser window" in the log, and nothing ever refreshed after
/// share start. See `extract_url_for_window`, `log_extraction_failure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlExtraction {
    /// A privacy-minimized `http(s)://` URL was extracted.
    Url(String),
    /// The script ran and found no matching window (or its match wasn't an
    /// openable http(s) URL).
    Empty,
    /// The target process no longer exists (quit between share start and
    /// this poll). Distinct from `Empty` so the log can tell "browser gone"
    /// from "browser up but wrong window".
    ProcessGone,
    /// The process is up but reports zero windows.
    NoWindows,
    /// The process is up and reports `n` windows, none of which matched the
    /// exact shared title (or the match was hidden/had no URL).
    TitleMismatch(u32),
    /// More than one on-screen window shared the target title, so the
    /// fail-closed match rule (#97) refused to guess. Carries the count.
    Ambiguous(u32),
    /// The script did not exit within the deadline and was killed.
    Timeout,
    /// `osascript` exited non-zero with a `-1743` ("not authorized to send
    /// Apple events") stderr -- Petal has no (or revoked) Automation consent
    /// for this bundle id. Terminal: callers should stop polling.
    Denied,
    /// `bundle_id` is not a recognised browser (or this isn't macOS).
    /// Terminal: callers should stop polling.
    Unsupported,
    /// The script exited non-zero for a reason other than a `-1743` denial.
    /// `stderr` is osascript's captured stderr, first line only -- this
    /// field must never carry a URL (see `log_extraction_failure`).
    Failed { status: i32, stderr: String },
    /// `osascript` itself could not be spawned or its exit/output could not
    /// be read.
    Spawn(String),
}

impl UrlExtraction {
    /// The extracted URL, if this outcome is a success. `None` for every
    /// other variant.
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Url(url) => Some(url.as_str()),
            _ => None,
        }
    }

    /// `true` when the caller should stop polling for the lifetime of the
    /// share/process rather than retry: an Automation denial won't fix
    /// itself on the next poll, and an unsupported bundle id never will.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Denied | Self::Unsupported)
    }

    /// A short, closed-set signature for logs and the `browser-url-extraction-failed`
    /// diagnostic tag. Stable strings -- do not rename without updating
    /// `logging.rs`'s `BrowserUrlExtractionCauseTag`.
    pub fn cause(&self) -> &'static str {
        match self {
            Self::Url(_) => "ok",
            Self::Empty => "no-match",
            Self::ProcessGone => "process-gone",
            Self::NoWindows => "no-windows",
            Self::TitleMismatch(_) => "title-mismatch",
            Self::Ambiguous(_) => "ambiguous",
            Self::Timeout => "timeout",
            Self::Denied => "denied",
            Self::Unsupported => "unsupported",
            Self::Failed { .. } => "failed",
            Self::Spawn(_) => "spawn",
        }
    }
}

/// Bound for the very first extraction attempt of a share. AppleScript's own
/// implicit `tell` timeout is 60s, so a wedged target self-terminates with
/// `-1712` rather than being killed early -- a shorter bound here would kill
/// a pending Automation-consent prompt mid-decision and just re-prompt on
/// the next poll (#915 plan step 2).
pub const FIRST_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound for every poll after the first attempt.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(3);
/// Cadence between polls after the first attempt.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Skip a poll's spawn entirely when the last extraction succeeded within
/// this long and the CGWindow title hasn't changed since.
pub const FRESH_URL_TTL: Duration = Duration::from_secs(15);

/// Run macOS URL extraction for one shared browser window, classifying every
/// outcome instead of collapsing failures into `None`. Non-macOS always
/// returns `Unsupported`.
pub fn extract_url_for_window(
    bundle_id: &str,
    owner_pid: i32,
    window_title: Option<&str>,
    timeout: Duration,
) -> UrlExtraction {
    #[cfg(target_os = "macos")]
    {
        if browser_family(bundle_id).is_none() {
            return UrlExtraction::Unsupported;
        }
        let Some(script) = script_for_bundle(bundle_id, owner_pid, window_title.unwrap_or(""))
        else {
            return UrlExtraction::Failed {
                status: -1,
                stderr: "invalid owner pid".to_string(),
            };
        };
        let outcome = crate::platform::osascript::run_osascript_javascript(&script, timeout);
        classify_osascript_outcome(outcome)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (bundle_id, owner_pid, window_title, timeout);
        UrlExtraction::Unsupported
    }
}

/// Whether `bundle_id` is a browser `extract_url_for_window` can ever
/// succeed for -- the single source of truth for "is this a browser," so
/// callers deciding whether to even bother spawning a poller (`session/
/// share.rs`'s `spawn_share_url_refresh`) don't need their own duplicate
/// allowlist that can drift from `script_for_bundle`'s (#915: a prior
/// duplicate here excluded the Beta/Dev/Chromium/Opera entries `
/// script_for_bundle` already recognizes, and still included Firefox after
/// its script support was removed). Non-macOS always `false` (extraction
/// itself is macOS-only).
pub fn is_supported_bundle_id(bundle_id: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        browser_family(bundle_id).is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bundle_id;
        false
    }
}

/// Pure decision for `log_extraction_failure`: whether/how loudly to log
/// this outcome, and whether to also emit the Sentry diagnostic event.
/// `None` means "log nothing" (a success, or an unsupported bundle id --
/// there is no "browser share that will never work" signal worth telling
/// the field about). Split out from `log_extraction_failure` itself so the
/// warn-once/debug-thereafter and emit-once rules can be unit-tested
/// without a live logger or Sentry client.
fn extraction_log_plan(
    outcome: &UrlExtraction,
    first_for_share: bool,
) -> Option<(log::Level, bool)> {
    if matches!(outcome, UrlExtraction::Url(_) | UrlExtraction::Unsupported) {
        return None;
    }
    if first_for_share {
        Some((log::Level::Warn, true))
    } else {
        Some((log::Level::Debug, false))
    }
}

/// Maps a failure outcome to its Sentry diagnostic tag. `None` for the two
/// outcomes `log_extraction_failure` never reaches (`Url`, `Unsupported`) --
/// `extraction_log_plan` already filters those out, this is the second,
/// independent guard `capture_sentry_diagnostic` needs a concrete tag for.
#[cfg(target_os = "macos")]
fn browser_url_extraction_cause_tag(
    outcome: &UrlExtraction,
) -> Option<crate::logging::BrowserUrlExtractionCauseTag> {
    use crate::logging::BrowserUrlExtractionCauseTag as Tag;
    match outcome {
        UrlExtraction::Denied => Some(Tag::Denied),
        UrlExtraction::Timeout => Some(Tag::Timeout),
        UrlExtraction::Ambiguous(_) => Some(Tag::Ambiguous),
        UrlExtraction::Empty => Some(Tag::NoMatch),
        UrlExtraction::ProcessGone => Some(Tag::ProcessGone),
        UrlExtraction::NoWindows => Some(Tag::NoWindows),
        UrlExtraction::TitleMismatch(_) => Some(Tag::TitleMismatch),
        UrlExtraction::Spawn(_) => Some(Tag::Spawn),
        UrlExtraction::Failed { .. } => Some(Tag::Failed),
        UrlExtraction::Url(_) | UrlExtraction::Unsupported => None,
    }
}

/// Log one extraction failure. `warn` (plus one diagnostic event) the first
/// time a share fails, `debug` for every later poll of the same share --
/// mirroring `#788`'s per-episode-not-per-sample Sentry volume rule. Never
/// called for a success or an unsupported bundle id (callers should not
/// bother; `extraction_log_plan` also refuses to emit for them
/// defensively). Never logs the URL, at any level.
pub fn log_extraction_failure(window_id: u32, outcome: &UrlExtraction, first_for_share: bool) {
    let Some((level, emit_diagnostic)) = extraction_log_plan(outcome, first_for_share) else {
        return;
    };
    let cause = outcome.cause();
    match outcome {
        UrlExtraction::Failed { status, stderr } => log::log!(
            level,
            "browser url extraction failed for window {window_id}: cause={cause} status={status} stderr={stderr}"
        ),
        _ => log::log!(
            level,
            "browser url extraction failed for window {window_id}: cause={cause}"
        ),
    }
    if emit_diagnostic {
        #[cfg(target_os = "macos")]
        if let Some(tag) = browser_url_extraction_cause_tag(outcome) {
            crate::logging::capture_sentry_diagnostic(
                crate::logging::SentryDiagnosticEvent::BrowserUrlExtractionFailed(
                    crate::logging::BrowserUrlExtractionFailedDiagnostic { cause: tag },
                ),
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn classify_osascript_outcome(
    outcome: crate::platform::osascript::OsascriptOutcome,
) -> UrlExtraction {
    use crate::platform::osascript::OsascriptOutcome;
    match outcome {
        OsascriptOutcome::Spawn(error) => UrlExtraction::Spawn(error),
        OsascriptOutcome::Timeout => UrlExtraction::Timeout,
        OsascriptOutcome::Failed { status, stderr } => {
            if stderr.contains("-1743") {
                UrlExtraction::Denied
            } else {
                let first_line = stderr.lines().next().unwrap_or("").to_string();
                UrlExtraction::Failed {
                    status,
                    stderr: first_line,
                }
            }
        }
        OsascriptOutcome::Ok(stdout) => {
            let trimmed = stdout.trim();
            if trimmed == "PETAL_GONE" {
                UrlExtraction::ProcessGone
            } else if trimmed == "PETAL_NOWINDOWS" {
                UrlExtraction::NoWindows
            } else if let Some(count) = trimmed.strip_prefix("PETAL_NOMATCH:") {
                match count.trim().parse::<u32>() {
                    Ok(n) => UrlExtraction::TitleMismatch(n),
                    Err(_) => UrlExtraction::Empty,
                }
            } else if let Some(count) = trimmed.strip_prefix("AMBIGUOUS:") {
                match count.trim().parse::<u32>() {
                    Ok(n) => UrlExtraction::Ambiguous(n),
                    Err(_) => UrlExtraction::Empty,
                }
            } else if trimmed.is_empty() {
                UrlExtraction::Empty
            } else {
                match privacy_minimized_openable_url(trimmed) {
                    Some(url) => UrlExtraction::Url(url),
                    None => UrlExtraction::Empty,
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) async fn windows_target_supports_url_extraction(
    target: crate::windows_capture_target::WindowsCaptureTarget,
) -> bool {
    if target.kind() != crate::windows_capture_target::TargetKind::Window {
        return false;
    }

    let pid = target.owner_process_id();
    tauri::async_runtime::spawn_blocking(move || {
        crate::window_source::process_exe_path(pid)
            .is_some_and(|path| is_supported_windows_browser_executable(&path))
    })
    .await
    .unwrap_or(false)
}

#[cfg(target_os = "windows")]
pub(crate) async fn url_for_windows_target(
    target: crate::windows_capture_target::WindowsCaptureTarget,
) -> Option<String> {
    if target.kind() != crate::windows_capture_target::TargetKind::Window {
        return None;
    }

    let pid = target.owner_process_id();
    let raw_handle = target.raw_handle();
    tauri::async_runtime::spawn_blocking(move || {
        let executable_path = crate::window_source::process_exe_path(pid)?;
        if !is_supported_windows_browser_executable(&executable_path) {
            return None;
        }
        let _com = initialize_com().ok()?;
        let hwnd = windows::Win32::Foundation::HWND(raw_handle as *mut core::ffi::c_void);
        if hwnd.0.is_null() {
            return None;
        }
        // This is deliberately target-based rather than cursor-based: the
        // picker selected this HWND, and a moved cursor must not disclose a
        // URL from another window.
        unsafe { try_to_get_url_from_underlying_window(hwnd) }
    })
    .await
    .ok()
    .flatten()
}

pub fn is_openable_url(url: &str) -> bool {
    let trimmed = url.trim();
    trimmed.starts_with("http://") || trimmed.starts_with("https://")
}

pub fn privacy_minimized_openable_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if !is_openable_url(trimmed) {
        return None;
    }
    let end = trimmed.find(['?', '#']).unwrap_or(trimmed.len());
    Some(trimmed[..end].to_string())
}

fn is_supported_windows_browser_executable(executable_path: &str) -> bool {
    let executable = executable_path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(executable_path);
    matches!(
        executable.to_ascii_lowercase().as_str(),
        "chrome.exe" | "brave.exe" | "msedge.exe" | "firefox.exe" | "vivaldi.exe" | "arc.exe"
    )
}

#[cfg(target_os = "windows")]
struct ComApartment;

#[cfg(target_os = "windows")]
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}

#[cfg(target_os = "windows")]
fn initialize_com() -> windows::core::Result<ComApartment> {
    let hr = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        )
    };
    if hr.is_err() {
        return Err(hr.into());
    }
    Ok(ComApartment)
}

#[cfg(target_os = "windows")]
unsafe fn try_to_get_url_from_underlying_window(
    hwnd: windows::Win32::Foundation::HWND,
) -> Option<String> {
    get_url_with_ui_automation(hwnd).or_else(|| get_browser_url_from_hwnd(hwnd))
}

#[cfg(target_os = "windows")]
fn address_bar_candidate_score(name: &str, class_name: &str, automation_id: &str) -> u8 {
    let name = name.to_ascii_lowercase();
    let class_name = class_name.to_ascii_lowercase();
    let automation_id = automation_id.to_ascii_lowercase();
    if automation_id.contains("urlbar") || class_name.contains("omnibox") {
        return 3;
    }
    if name.contains("address bar")
        || name.contains("address and search")
        || name.contains("location bar")
        || name.contains("omnibox")
    {
        return 2;
    }
    0
}

#[cfg(target_os = "windows")]
unsafe fn get_url_with_ui_automation(hwnd: windows::Win32::Foundation::HWND) -> Option<String> {
    use windows::core::Interface;
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationValuePattern,
        TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_EditControlTypeId,
        UIA_ValuePatternId,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let automation: IUIAutomation =
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
    let mut rect = RECT::default();
    let window_rect = GetWindowRect(hwnd, &mut rect).ok().map(|_| rect);
    let candidate = |element: &IUIAutomationElement| -> Option<(u8, String)> {
        let value = element
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()
            .and_then(|pattern| pattern.CurrentValue().ok())
            .map(|value| value.to_string());
        let name = element
            .CurrentName()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let class_name = element
            .CurrentClassName()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let automation_id = element
            .CurrentAutomationId()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let metadata_score = address_bar_candidate_score(&name, &class_name, &automation_id);
        let geometry_score = window_rect
            .zip(element.CurrentBoundingRectangle().ok())
            .map(|(window, rect)| {
                let width = rect.right - rect.left;
                u8::from(width >= 200 && rect.top >= window.top && rect.top <= window.top + 180)
            })
            .unwrap_or(0);
        let score = metadata_score.max(geometry_score);
        let url = value
            .as_deref()
            .and_then(privacy_minimized_openable_url)
            // Chrome's hit-tested omnibox may expose its URL as CurrentName
            // rather than through ValuePattern. The metadata/geometry gate
            // above keeps this from becoming a generic name scan.
            .or_else(|| privacy_minimized_openable_url(&name))?;
        (score > 0).then_some((score, url))
    };

    // Chrome's omnibox is reliably returned when asking UI Automation for the
    // element at a point inside the browser chrome, even when Chrome omits all
    // useful metadata from the omnibox element. Probe the top band rather than
    // the cursor: sharing is target-HWND based and the cursor may be elsewhere.
    if let (Some(window), Ok(walker)) = (window_rect, automation.ControlViewWalker()) {
        let width = window.right - window.left;
        for x_quarter in [1, 2, 3] {
            for y_offset in (24..=180).step_by(8) {
                let point = POINT {
                    x: window.left + width * x_quarter / 4,
                    y: window.top + y_offset,
                };
                let Ok(mut element) = automation.ElementFromPoint(point) else {
                    continue;
                };
                for _ in 0..8 {
                    if let Some((score, url)) = candidate(&element) {
                        if score >= 1 {
                            return Some(url);
                        }
                    }
                    let Ok(parent) = walker.GetParentElement(&element) else {
                        break;
                    };
                    element = parent;
                }
            }
        }
    }

    // Keep the broader Edit-control search as a fallback for browsers whose
    // accessibility tree exposes the omnibox but not hit-testing.
    let root = automation.ElementFromHandle(hwnd).ok()?;
    let condition = automation
        .CreatePropertyCondition(
            UIA_ControlTypePropertyId,
            &VARIANT::from(UIA_EditControlTypeId.0),
        )
        .ok()?;
    let edits = root.FindAll(TreeScope_Descendants, &condition).ok()?;
    let mut best: Option<(u8, String)> = None;
    for index in 0..edits.Length().ok()?.max(0) {
        let Ok(element) = edits.GetElement(index) else {
            continue;
        };
        let Some((score, url)) = candidate(&element) else {
            continue;
        };
        match &best {
            Some((best_score, best_url)) if *best_score > score => {}
            Some((best_score, best_url)) if *best_score == score && best_url != &url => {
                return None;
            }
            _ => best = Some((score, url)),
        }
    }
    best.map(|(_, url)| url)
}

#[cfg(target_os = "windows")]
unsafe fn get_browser_url_from_hwnd(hwnd: windows::Win32::Foundation::HWND) -> Option<String> {
    use std::ffi::c_void;
    use std::ptr;
    use windows::core::Interface;
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::IAccessible;

    #[link(name = "OleAcc")]
    extern "system" {
        fn AccessibleObjectFromWindow(
            hwnd: windows::Win32::Foundation::HWND,
            object_id: u32,
            interface_id: *const windows::core::GUID,
            object: *mut *mut c_void,
        ) -> windows::core::HRESULT;
    }

    const OBJID_CLIENT: u32 = 0xFFFFFFFC;
    const CHILDID_SELF: i32 = 0;
    let mut accessible = ptr::null_mut();
    let result = AccessibleObjectFromWindow(hwnd, OBJID_CLIENT, &IAccessible::IID, &mut accessible);
    if result.is_err() || accessible.is_null() {
        return None;
    }

    let accessible = IAccessible::from_raw(accessible);
    let mut pending = vec![(accessible, VARIANT::from(CHILDID_SELF))];
    let mut visited = 0usize;
    let mut found = None;

    while let Some((accessible, child)) = pending.pop() {
        visited += 1;
        if visited > 10_000 {
            return None;
        }

        let name = accessible
            .get_accName(&child)
            .ok()
            .map(|value| value.to_string())
            .unwrap_or_default();
        if address_bar_candidate_score(&name, "", "") != 0 {
            if let Some(value) = accessible.get_accValue(&child).ok() {
                if let Some(url) = privacy_minimized_openable_url(&value.to_string()) {
                    if found
                        .as_deref()
                        .is_some_and(|existing| existing != url.as_str())
                    {
                        return None;
                    }
                    found = Some(url);
                }
            }
        }

        let child_count = accessible.accChildCount().unwrap_or(0);
        for index in (1..=child_count).rev() {
            let child = VARIANT::from(index);
            match accessible.get_accChild(&child) {
                Ok(dispatch) => {
                    if let Ok(child_accessible) = dispatch.cast::<IAccessible>() {
                        pending.push((child_accessible, VARIANT::from(CHILDID_SELF)));
                    }
                }
                Err(_) => pending.push((accessible.clone(), child)),
            }
        }
    }
    found
}

/// Browser scripting families `script_for_bundle` knows. The bundle id is
/// only the "is this a supported browser, and which dictionary" check -- the
/// script itself targets a pid (see `script_for_bundle`).
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserFamily {
    Safari,
    Chromium,
}

#[cfg(target_os = "macos")]
fn browser_family(bundle_id: &str) -> Option<BrowserFamily> {
    match bundle_id {
        "com.apple.Safari" | "com.apple.SafariTechnologyPreview" => Some(BrowserFamily::Safari),
        "com.google.Chrome"
        | "com.google.Chrome.canary"
        | "com.google.Chrome.beta"
        | "com.google.Chrome.dev"
        | "com.brave.Browser"
        | "com.microsoft.edgemac"
        | "com.microsoft.edgemac.Beta"
        | "com.microsoft.edgemac.Dev"
        | "com.microsoft.edgemac.Canary"
        | "com.vivaldi.Vivaldi"
        | "company.thebrowser.Browser"
        | "org.chromium.Chromium"
        | "com.operasoftware.Opera" => Some(BrowserFamily::Chromium),
        _ => None,
    }
}

/// Build the JXA (`osascript -l JavaScript`) script for one shared window.
///
/// Sends RAW Apple Events through the ObjC bridge to
/// `NSAppleEventDescriptor.descriptorWithProcessIdentifier(<pid>)` -- the
/// exact process that owns the shared window. Neither `application id "..."`
/// (AppleScript) nor JXA `Application(<pid>)` is pid-bound (both resolve to
/// whichever instance of the bundle LaunchServices picks), so with two
/// processes sharing a bundle id (the user's Chrome plus an automation
/// Chrome) they reached the wrong one: no window with the shared title,
/// `cause=no-match` forever. `pid` is a validated positive integer
/// interpolated as a number, never text. A pid-addressed event cannot launch
/// an app, so no is-running guard is needed.
///
/// Windows are read one index at a time (1..N until `-1719` invalid index),
/// since `every window` is not answered. Four-char codes come from each
/// browser's sdef: window `cwin`, name `pnam`, minimized/miniaturized `pmnd`;
/// Chromium tab `acTa` + URL `URL `, Safari tab `cTab` + URL `pURL`.
///
/// Output contract (parsed by `classify_osascript_outcome`): the URL for
/// exactly one visible exact-title match; `AMBIGUOUS:<n>` for several;
/// `PETAL_GONE` when the process no longer exists; `PETAL_NOWINDOWS` for
/// zero windows; otherwise `PETAL_NOMATCH:<window count>`. Any other error
/// (e.g. `-1743` Automation denial) is rethrown with its code in
/// parentheses so it reaches stderr.
#[cfg(target_os = "macos")]
fn script_for_bundle(bundle_id: &str, pid: i32, window_title: &str) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let family = browser_family(bundle_id)?;
    let target = js_string(window_title);
    let (tab_code, url_code) = match family {
        BrowserFamily::Safari => ("cTab", "pURL"),
        BrowserFamily::Chromium => ("acTa", "URL "),
    };
    Some(format!(
        r#"(function () {{
  ObjC.import('Foundation');
  var targetTitle = {target};
  function fourcc(s) {{
    return (s.charCodeAt(0) << 24) | (s.charCodeAt(1) << 16) | (s.charCodeAt(2) << 8) | s.charCodeAt(3);
  }}
  function aeError(code, detail) {{
    code = Number(code);
    var e = new Error(detail + " (" + code + ")");
    e.aeCode = code;
    return e;
  }}
  function typeDesc(code) {{ return $.NSAppleEventDescriptor.descriptorWithTypeCode(fourcc(code)); }}
  function enumDesc(code) {{ return $.NSAppleEventDescriptor.descriptorWithEnumCode(fourcc(code)); }}
  function objSpec(wantType, form, seld, from) {{
    var r = $.NSAppleEventDescriptor.recordDescriptor;
    r.setDescriptorForKeyword(typeDesc(wantType), fourcc('want'));
    r.setDescriptorForKeyword(enumDesc(form), fourcc('form'));
    r.setDescriptorForKeyword(seld, fourcc('seld'));
    r.setDescriptorForKeyword(from || $.NSAppleEventDescriptor.nullDescriptor, fourcc('from'));
    return r.coerceToDescriptorType(fourcc('obj '));
  }}
  function prop(code, from) {{ return objSpec('prop', 'prop', typeDesc(code), from); }}
  function send(spec) {{
    var targetDesc = $.NSAppleEventDescriptor.descriptorWithProcessIdentifier({pid});
    var ev = $.NSAppleEventDescriptor.appleEventWithEventClassEventIDTargetDescriptorReturnIDTransactionID(fourcc('core'), fourcc('getd'), targetDesc, -1, 0);
    ev.setParamDescriptorForKeyword(spec, fourcc('----'));
    var err = $();
    var reply = ev.sendEventWithOptionsTimeoutError(3, 10, err);
    if (reply.isNil()) {{
      throw aeError(err.code ? err.code : -1, "send failed");
    }}
    var en = reply.paramDescriptorForKeyword(fourcc('errn'));
    if (en && !en.isNil() && en.int32Value) {{
      throw aeError(en.int32Value, "AE error");
    }}
    return reply.paramDescriptorForKeyword(fourcc('----'));
  }}
  var matchesFound = 0;
  var matchedUrl = null;
  var total = 0;
  try {{
    for (var i = 1; i <= 200; i++) {{
      var w = objSpec('cwin', 'indx', $.NSAppleEventDescriptor.descriptorWithInt32(i), null);
      var winName;
      try {{
        winName = String(send(prop('pnam', w)).stringValue.js);
      }} catch (e) {{
        if (e.aeCode === -1719) {{ break; }}
        if (e.aeCode === -1743 || e.aeCode === -600 || e.aeCode === -609 || e.aeCode === -1712) {{ throw e; }}
        total = total + 1;
        continue;
      }}
      total = total + 1;
      try {{
        var isHidden = false;
        try {{
          isHidden = send(prop('pmnd', w)).booleanValue === true;
        }} catch (e) {{}}
        if (targetTitle !== "" && winName === targetTitle && isHidden === false) {{
          matchesFound = matchesFound + 1;
          if (matchesFound === 1) {{
            matchedUrl = String(send(prop('{url_code}', prop('{tab_code}', w))).stringValue.js);
          }}
        }}
      }} catch (e) {{}}
    }}
  }} catch (e) {{
    if (e.aeCode === -600 || e.aeCode === -609) {{
      return "PETAL_GONE";
    }}
    throw new Error(String(e.message));
  }}
  if (matchesFound === 1 && matchedUrl !== null && matchedUrl !== undefined) {{ return matchedUrl; }}
  if (matchesFound > 1) {{ return "AMBIGUOUS:" + matchesFound; }}
  if (total === 0) {{ return "PETAL_NOWINDOWS"; }}
  return "PETAL_NOMATCH:" + total;
}})()"#
    ))
}

/// JSON-encode `s` as a JavaScript string literal. JSON string syntax is a
/// subset of JS, so quotes, backslashes, newlines and control characters are
/// all escaped; U+2028/U+2029 are escaped too for pre-ES2019 engines.
#[cfg(target_os = "macos")]
fn js_string(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".to_string())
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_urls_are_openable() {
        assert!(is_openable_url("https://example.com"));
        assert!(is_openable_url("http://localhost:1420"));
        assert!(!is_openable_url("file:///tmp/x"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url(""));
    }

    #[test]
    fn supported_windows_browser_executable_names_are_case_insensitive() {
        for path in [
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files\BraveSoftware\Brave-Browser\Application\BRAVE.EXE",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Mozilla Firefox\firefox.exe",
            r"C:\Users\user\AppData\Local\Vivaldi\vivaldi.exe",
            r"C:\Users\user\AppData\Local\Arc\Arc.exe",
        ] {
            assert!(
                is_supported_windows_browser_executable(path),
                "expected supported browser path: {path}"
            );
        }
        for path in [
            r"C:\Windows\explorer.exe",
            r"C:\Windows\System32\notepad.exe",
            "",
            r"C:\Apps\custom-browser.exe",
        ] {
            assert!(
                !is_supported_windows_browser_executable(path),
                "expected rejected executable path: {path}"
            );
        }
    }

    #[test]
    fn browser_urls_are_privacy_minimized() {
        assert_eq!(
            privacy_minimized_openable_url(" https://example.com/docs?token=secret#section "),
            Some("https://example.com/docs".to_string())
        );
        assert_eq!(
            privacy_minimized_openable_url("http://localhost:1420/#/meeting/room"),
            Some("http://localhost:1420/".to_string())
        );
        assert_eq!(privacy_minimized_openable_url("file:///tmp/x?secret"), None);
        assert_eq!(privacy_minimized_openable_url("   "), None);
    }

    #[test]
    fn is_supported_bundle_id_rejects_non_browsers() {
        // True on every platform: a non-browser bundle id must never be
        // treated as supported, macOS or not.
        assert!(!is_supported_bundle_id("com.apple.finder"));
        assert!(!is_supported_bundle_id("org.mozilla.firefox"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn is_supported_bundle_id_recognizes_the_full_chromium_family() {
        // #915: the standalone allowlist this replaced had drifted from
        // `script_for_bundle`'s -- Beta/Dev/Chromium/Opera were missing.
        assert!(is_supported_bundle_id("com.google.Chrome.beta"));
    }

    #[test]
    fn cause_and_is_terminal_mapping() {
        let cases: &[(UrlExtraction, &str, bool)] = &[
            (
                UrlExtraction::Url("https://example.com".to_string()),
                "ok",
                false,
            ),
            (UrlExtraction::Empty, "no-match", false),
            (UrlExtraction::Ambiguous(2), "ambiguous", false),
            (UrlExtraction::Timeout, "timeout", false),
            (UrlExtraction::Denied, "denied", true),
            (UrlExtraction::Unsupported, "unsupported", true),
            (
                UrlExtraction::Failed {
                    status: 1,
                    stderr: "boom".to_string(),
                },
                "failed",
                false,
            ),
            (UrlExtraction::Spawn("nope".to_string()), "spawn", false),
        ];
        for (outcome, expected_cause, expected_terminal) in cases {
            assert_eq!(outcome.cause(), *expected_cause, "outcome: {outcome:?}");
            assert_eq!(
                outcome.is_terminal(),
                *expected_terminal,
                "outcome: {outcome:?}"
            );
        }
    }

    #[test]
    fn url_only_returns_some_for_the_url_variant() {
        assert_eq!(
            UrlExtraction::Url("https://example.com".to_string()).url(),
            Some("https://example.com")
        );
        for outcome in [
            UrlExtraction::Empty,
            UrlExtraction::Ambiguous(2),
            UrlExtraction::Timeout,
            UrlExtraction::Denied,
            UrlExtraction::Unsupported,
            UrlExtraction::Failed {
                status: 1,
                stderr: "boom".to_string(),
            },
            UrlExtraction::Spawn("nope".to_string()),
        ] {
            assert_eq!(outcome.url(), None, "outcome: {outcome:?}");
        }
    }

    #[test]
    fn extraction_log_plan_warns_and_emits_only_on_the_first_failure_for_a_share() {
        let outcome = UrlExtraction::Timeout;
        assert_eq!(
            extraction_log_plan(&outcome, true),
            Some((log::Level::Warn, true)),
            "the first failure for a share must warn and emit exactly one diagnostic event"
        );
        assert_eq!(
            extraction_log_plan(&outcome, false),
            Some((log::Level::Debug, false)),
            "every later poll of the same share must stay at debug and emit nothing further"
        );
    }

    #[test]
    fn extraction_log_plan_never_logs_a_success_or_an_unsupported_bundle() {
        for first_for_share in [true, false] {
            assert_eq!(
                extraction_log_plan(
                    &UrlExtraction::Url("https://example.com".to_string()),
                    first_for_share
                ),
                None
            );
            assert_eq!(
                extraction_log_plan(&UrlExtraction::Unsupported, first_for_share),
                None
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_url_extraction_cause_tag_covers_every_loggable_outcome() {
        use crate::logging::BrowserUrlExtractionCauseTag as Tag;
        let cases = [
            (UrlExtraction::Denied, Some(Tag::Denied)),
            (UrlExtraction::Timeout, Some(Tag::Timeout)),
            (UrlExtraction::Ambiguous(3), Some(Tag::Ambiguous)),
            (UrlExtraction::Empty, Some(Tag::NoMatch)),
            (UrlExtraction::Spawn("nope".to_string()), Some(Tag::Spawn)),
            (
                UrlExtraction::Failed {
                    status: 1,
                    stderr: "boom".to_string(),
                },
                Some(Tag::Failed),
            ),
            (UrlExtraction::Url("https://example.com".to_string()), None),
            (UrlExtraction::Unsupported, None),
        ];
        for (outcome, expected) in cases {
            assert_eq!(
                browser_url_extraction_cause_tag(&outcome),
                expected,
                "outcome: {outcome:?}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classify_maps_a_stderr_1743_line_to_denied_regardless_of_status() {
        use crate::platform::osascript::OsascriptOutcome;
        let outcome = classify_osascript_outcome(OsascriptOutcome::Failed {
            status: 1,
            stderr: "execution error: Not authorized to send Apple events (-1743)".to_string(),
        });
        assert_eq!(outcome, UrlExtraction::Denied);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classify_maps_a_non_1743_failure_to_failed_with_first_stderr_line_only() {
        use crate::platform::osascript::OsascriptOutcome;
        let outcome = classify_osascript_outcome(OsascriptOutcome::Failed {
            status: 1,
            stderr: "first line\nsecond line".to_string(),
        });
        assert_eq!(
            outcome,
            UrlExtraction::Failed {
                status: 1,
                stderr: "first line".to_string(),
            }
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classify_maps_ambiguous_marker_to_ambiguous_with_count() {
        use crate::platform::osascript::OsascriptOutcome;
        assert_eq!(
            classify_osascript_outcome(OsascriptOutcome::Ok("AMBIGUOUS:2".to_string())),
            UrlExtraction::Ambiguous(2)
        );
        // A malformed count fails closed to Empty rather than panicking or
        // silently treating it as a match.
        assert_eq!(
            classify_osascript_outcome(OsascriptOutcome::Ok("AMBIGUOUS:oops".to_string())),
            UrlExtraction::Empty
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classify_maps_empty_stdout_to_empty_and_timeout_passes_through() {
        use crate::platform::osascript::OsascriptOutcome;
        assert_eq!(
            classify_osascript_outcome(OsascriptOutcome::Ok(String::new())),
            UrlExtraction::Empty
        );
        assert_eq!(
            classify_osascript_outcome(OsascriptOutcome::Ok("   \n".to_string())),
            UrlExtraction::Empty
        );
        assert_eq!(
            classify_osascript_outcome(OsascriptOutcome::Timeout),
            UrlExtraction::Timeout
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classify_maps_a_non_http_result_to_empty_not_url() {
        use crate::platform::osascript::OsascriptOutcome;
        assert_eq!(
            classify_osascript_outcome(OsascriptOutcome::Ok("file:///tmp/x\n".to_string())),
            UrlExtraction::Empty
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn script_for_bundle_covers_the_fixed_allowlist_and_drops_firefox() {
        for bundle_id in [
            "com.apple.Safari",
            "com.apple.SafariTechnologyPreview",
            "com.google.Chrome",
            "com.google.Chrome.canary",
            "com.google.Chrome.beta",
            "com.google.Chrome.dev",
            "com.brave.Browser",
            "com.microsoft.edgemac",
            "com.microsoft.edgemac.Beta",
            "com.microsoft.edgemac.Dev",
            "com.microsoft.edgemac.Canary",
            "com.vivaldi.Vivaldi",
            "company.thebrowser.Browser",
            "org.chromium.Chromium",
            "com.operasoftware.Opera",
        ] {
            assert!(
                script_for_bundle(bundle_id, 4242, "Petal").is_some(),
                "expected a script for {bundle_id}"
            );
        }
        assert!(
            script_for_bundle("org.mozilla.firefox", 4242, "Petal").is_none(),
            "Firefox has no tab model this AppleScript dictionary can address (#915) -- \
             it must stay unsupported, not silently fail every time"
        );
        assert!(script_for_bundle("com.example.not-a-browser", 4242, "Petal").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_scripts_do_not_use_fuzzy_or_front_window_fallback() {
        for bundle_id in ["com.apple.Safari", "com.google.Chrome"] {
            let script = script_for_bundle(bundle_id, 4242, "Petal").expect("supported browser");
            assert!(!script.contains("front window"));
            assert!(!script.contains(" contains "));
            assert!(script.contains("winName === targetTitle"));
            assert!(script.contains("matchesFound === 1"));
            assert!(script.contains("AMBIGUOUS:"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn chromium_script_filters_minimized_windows_and_safari_filters_miniaturized() {
        let chrome = script_for_bundle("com.google.Chrome", 4242, "Petal").expect("chrome script");
        // Both families read the sdef `minimized`/`miniaturized` property
        // code `pmnd`, defaulting to "not hidden" if the read fails.
        assert!(chrome.contains("prop('pmnd', w)"));
        assert!(chrome.contains("isHidden === false"));
        assert!(!chrome.contains("miniaturized"));

        let safari = script_for_bundle("com.apple.Safari", 4242, "Petal").expect("safari script");
        assert!(safari.contains("prop('pmnd', w)"));
        assert!(safari.contains("isHidden === false"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hidden_property_read_is_wrapped_in_its_own_nested_try_and_defaults_to_false() {
        // Arc and Opera's exact `sdef` for `minimized`/`miniaturized` on the
        // `window` class are unverified; a browser whose dictionary lacks the
        // property must not blow up the outer per-window `try` and silently
        // drop that window from matching -- it must default to "not hidden"
        // instead, via its own inner try/end try.
        for bundle_id in ["com.apple.Safari", "com.google.Chrome"] {
            let script = script_for_bundle(bundle_id, 4242, "Petal").expect("supported browser");
            assert!(
                script.contains("var isHidden = false;"),
                "script for {bundle_id} must default isHidden to false before reading the \
                 real (possibly-missing) property"
            );
            let try_lines = script.lines().filter(|line| line.trim() == "try {").count();
            assert!(
                try_lines >= 3,
                "expected the outer app try, a per-window try, and a nested try around the \
                 hidden-property read in the {bundle_id} script, found {try_lines} `try {{` \
                 lines: {script}"
            );
            assert!(
                script.contains("isHidden === false"),
                "the match condition must gate on the safely-read isHidden value, not the raw \
                 property read, in the {bundle_id} script"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scripts_target_the_owner_pid_never_the_bundle_id() {
        // Two processes sharing a bundle id (a regular plus an automation
        // instance) made bundle-id AND JXA `Application(<pid>)` events reach
        // the wrong one: `cause=no-match` forever. Only raw events addressed
        // with `descriptorWithProcessIdentifier` are pid-bound (measured live).
        for bundle_id in [
            "com.apple.Safari",
            "com.apple.SafariTechnologyPreview",
            "com.google.Chrome",
            "org.chromium.Chromium",
            "com.operasoftware.Opera",
        ] {
            let script = script_for_bundle(bundle_id, 54935, "Petal").expect("supported browser");
            assert!(
                script.contains("descriptorWithProcessIdentifier(54935)"),
                "script for {bundle_id} must send raw events to the pid: {script}"
            );
            assert!(
                !script.contains("application id"),
                "script for {bundle_id} must not address the app by bundle id: {script}"
            );
            assert!(
                !script.contains("Application("),
                "JXA Application(...) is not pid-bound; {bundle_id} script must not use it: {script}"
            );
            assert!(
                !script.contains(bundle_id),
                "the bundle id must not appear in the {bundle_id} script at all: {script}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scripts_use_the_dictionary_of_their_browser_family() {
        let chrome = script_for_bundle("com.google.Chrome", 1, "T").expect("chrome");
        assert!(chrome.contains("prop('URL ', prop('acTa', w))"));
        assert!(!chrome.contains("cTab"));
        let safari = script_for_bundle("com.apple.Safari", 1, "T").expect("safari");
        assert!(safari.contains("prop('pURL', prop('cTab', w))"));
        assert!(!safari.contains("acTa"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn script_for_bundle_rejects_a_non_positive_pid() {
        assert!(script_for_bundle("com.google.Chrome", 0, "T").is_none());
        assert!(script_for_bundle("com.google.Chrome", -5, "T").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hostile_titles_stay_inside_a_json_string_literal() {
        let title = "a\"); doShellScript(\"rm -rf ~\") //\nline2\\ \u{2028}";
        let literal = js_string(title);
        assert_eq!(
            literal,
            "\"a\\\"); doShellScript(\\\"rm -rf ~\\\") //\\nline2\\\\ \\u2028\""
        );
        for bundle_id in ["com.google.Chrome", "com.apple.Safari"] {
            let script = script_for_bundle(bundle_id, 7, title).expect("supported browser");
            assert!(
                script.contains(&format!("var targetTitle = {literal};")),
                "title must appear only as the escaped literal in the {bundle_id} script"
            );
            // The raw payload (unescaped quote followed by `);`) must not
            // appear anywhere: every `"` from the title is backslash-escaped.
            assert!(!script.contains("a\"); doShellScript"));
            assert!(!script.contains('\u{2028}'));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classify_maps_the_diagnostic_sentinels_to_distinct_outcomes() {
        use crate::platform::osascript::OsascriptOutcome;
        let classify =
            |out: &str| classify_osascript_outcome(OsascriptOutcome::Ok(out.to_string()));
        assert_eq!(classify("PETAL_GONE\n"), UrlExtraction::ProcessGone);
        assert_eq!(classify("PETAL_NOWINDOWS\n"), UrlExtraction::NoWindows);
        assert_eq!(
            classify("PETAL_NOMATCH:3\n"),
            UrlExtraction::TitleMismatch(3)
        );
        assert_eq!(classify("PETAL_NOMATCH:oops\n"), UrlExtraction::Empty);
        assert_eq!(
            classify("https://example.com/a?b=1\n"),
            UrlExtraction::Url("https://example.com/a".to_string())
        );
    }

    #[test]
    fn diagnostic_outcomes_have_distinct_non_terminal_causes() {
        let cases: &[(UrlExtraction, &str)] = &[
            (UrlExtraction::ProcessGone, "process-gone"),
            (UrlExtraction::NoWindows, "no-windows"),
            (UrlExtraction::TitleMismatch(2), "title-mismatch"),
        ];
        for (outcome, cause) in cases {
            assert_eq!(outcome.cause(), *cause);
            assert!(!outcome.is_terminal());
            assert_eq!(outcome.url(), None);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn diagnostic_outcomes_map_to_their_own_sentry_tags() {
        use crate::logging::BrowserUrlExtractionCauseTag as Tag;
        assert_eq!(
            browser_url_extraction_cause_tag(&UrlExtraction::ProcessGone),
            Some(Tag::ProcessGone)
        );
        assert_eq!(
            browser_url_extraction_cause_tag(&UrlExtraction::NoWindows),
            Some(Tag::NoWindows)
        );
        assert_eq!(
            browser_url_extraction_cause_tag(&UrlExtraction::TitleMismatch(1)),
            Some(Tag::TitleMismatch)
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn address_bar_candidates_require_browser_chrome_metadata() {
        assert!(address_bar_candidate_score("Address and search bar", "", "") > 0);
        assert!(address_bar_candidate_score("", "", "urlbar-input") > 0);
        assert!(address_bar_candidate_score("A page link", "", "") == 0);
    }
}
