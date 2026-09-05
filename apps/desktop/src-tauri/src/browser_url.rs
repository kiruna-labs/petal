//! Browser URL extraction for shared browser windows.
//!
//! Petal sends the current browser tab URL as LiveKit participant metadata
//! alongside the existing shared-window title/scale metadata. This mirrors the
//! takt project's browser-context capture: use AppleScript for known browser
//! bundle IDs, keep the call timeout-bound, and return a typed
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
    window_title: Option<&str>,
    timeout: Duration,
) -> UrlExtraction {
    #[cfg(target_os = "macos")]
    {
        let Some(script) = script_for_bundle(bundle_id, window_title.unwrap_or("")) else {
            return UrlExtraction::Unsupported;
        };
        let outcome = crate::platform::osascript::run_osascript(&[script.as_str()], timeout);
        classify_osascript_outcome(outcome)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (bundle_id, window_title, timeout);
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
        script_for_bundle(bundle_id, "").is_some()
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
fn extraction_log_plan(outcome: &UrlExtraction, first_for_share: bool) -> Option<(log::Level, bool)> {
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
            if let Some(count) = trimmed.strip_prefix("AMBIGUOUS:") {
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

#[cfg(target_os = "macos")]
fn script_for_bundle(bundle_id: &str, window_title: &str) -> Option<String> {
    let target = applescript_string(window_title);
    match bundle_id {
        "com.apple.Safari" | "com.apple.SafariTechnologyPreview" => Some(format!(
            r#"set targetTitle to {target}
set matchesFound to 0
set matchedUrl to missing value
if application id "{bundle_id}" is not running then return ""
tell application id "{bundle_id}"
  repeat with w in windows
    try
      set winName to name of w as text
      set isHidden to false
      try
        set isHidden to miniaturized of w
      end try
      if targetTitle is not "" and winName is targetTitle and isHidden is false then
        set matchesFound to matchesFound + 1
        if matchesFound is 1 then
          set matchedUrl to URL of current tab of w
        end if
      end if
    end try
  end repeat
end tell
if matchesFound is 1 and matchedUrl is not missing value then return matchedUrl
if matchesFound > 1 then return "AMBIGUOUS:" & matchesFound
return """#
        )),
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
        | "com.operasoftware.Opera" => Some(format!(
            r#"set targetTitle to {target}
set matchesFound to 0
set matchedUrl to missing value
if application id "{bundle_id}" is not running then return ""
tell application id "{bundle_id}"
  repeat with w in windows
    try
      set winName to name of w as text
      set isHidden to false
      try
        set isHidden to minimized of w
      end try
      if targetTitle is not "" and winName is targetTitle and isHidden is false then
        set matchesFound to matchesFound + 1
        if matchesFound is 1 then
          set matchedUrl to URL of active tab of w
        end if
      end if
    end try
  end repeat
end tell
if matchesFound is 1 and matchedUrl is not missing value then return matchedUrl
if matchesFound > 1 then return "AMBIGUOUS:" & matchesFound
return """#
        )),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn applescript_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
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
            (UrlExtraction::Url("https://example.com".to_string()), "ok", false),
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
                script_for_bundle(bundle_id, "Petal").is_some(),
                "expected a script for {bundle_id}"
            );
        }
        assert!(
            script_for_bundle("org.mozilla.firefox", "Petal").is_none(),
            "Firefox has no tab model this AppleScript dictionary can address (#915) -- \
             it must stay unsupported, not silently fail every time"
        );
        assert!(script_for_bundle("com.example.not-a-browser", "Petal").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_scripts_do_not_use_fuzzy_or_front_window_fallback() {
        for bundle_id in ["com.apple.Safari", "com.google.Chrome"] {
            let script = script_for_bundle(bundle_id, "Petal").expect("supported browser");
            assert!(!script.contains("front window"));
            assert!(!script.contains(" contains "));
            assert!(script.contains("winName is targetTitle"));
            assert!(script.contains("matchesFound is 1"));
            assert!(script.contains("AMBIGUOUS:"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn chromium_script_filters_minimized_windows_and_safari_filters_miniaturized() {
        let chrome = script_for_bundle("com.google.Chrome", "Petal").expect("chrome script");
        assert!(chrome.contains("minimized of w"));
        assert!(chrome.contains("isHidden is false"));
        assert!(!chrome.contains("miniaturized"));

        let safari = script_for_bundle("com.apple.Safari", "Petal").expect("safari script");
        assert!(safari.contains("miniaturized of w"));
        assert!(safari.contains("isHidden is false"));
        assert!(!safari.contains(" minimized "));
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
            let script = script_for_bundle(bundle_id, "Petal").expect("supported browser");
            assert!(
                script.contains("set isHidden to false"),
                "script for {bundle_id} must default isHidden to false before reading the \
                 real (possibly-missing) property"
            );
            let bare_try_lines = script.lines().filter(|line| line.trim() == "try").count();
            assert!(
                bare_try_lines >= 2,
                "expected an outer per-window try plus a nested try around the hidden-property \
                 read in the {bundle_id} script, found {bare_try_lines} bare `try` lines: \
                 {script}"
            );
            assert!(
                script.contains("isHidden is false"),
                "the match condition must gate on the safely-read isHidden value, not the raw \
                 property read, in the {bundle_id} script"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scripts_refuse_to_relaunch_a_quit_browser() {
        // `tell application id "X"` launches a non-running app. The poller
        // runs every 3s for the life of a share, so without this guard,
        // quitting the shared browser mid-meeting relaunches it on the very
        // next poll.
        for bundle_id in [
            "com.apple.Safari",
            "com.apple.SafariTechnologyPreview",
            "com.google.Chrome",
            "org.chromium.Chromium",
        ] {
            let script = script_for_bundle(bundle_id, "Petal").expect("supported browser");
            let guard = format!(r#"if application id "{bundle_id}" is not running then return """#);
            assert!(
                script.contains(&guard),
                "expected a not-running guard in the {bundle_id} script before its `tell` block"
            );
            let guard_index = script.find(&guard).expect("guard present");
            let tell_index = script
                .find(&format!(r#"tell application id "{bundle_id}""#))
                .expect("tell block present");
            assert!(
                guard_index < tell_index,
                "the not-running guard must run before the `tell` block for {bundle_id}, or the \
                 `tell` itself launches the app first"
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn address_bar_candidates_require_browser_chrome_metadata() {
        assert!(address_bar_candidate_score("Address and search bar", "", "") > 0);
        assert!(address_bar_candidate_score("", "", "urlbar-input") > 0);
        assert!(address_bar_candidate_score("A page link", "", "") == 0);
    }

}
