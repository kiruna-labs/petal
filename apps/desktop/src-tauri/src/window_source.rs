//! Shareable-window enumeration for the window-tab picker (SPEC.md §4.2).
//!
//! Enumerates on-screen windows via ScreenCaptureKit's `SCShareableContent`
//! (crate: `screencapturekit`), NOT the older `CGWindowListCopyWindowInfo`
//! Quartz API. This is a deliberate choice, not an oversight: the same
//! `SCWindow` / `windowID` returned here is exactly what SPEC.md §4.1 later
//! hands to `SCContentFilter(desktopIndependentWindow:)` for the real capture
//! stream, so enumeration and capture share one window-identity source
//! instead of two APIs (`CGWindowList` vs `SCStream`) that can disagree about
//! what's on screen. A sibling project (takt) uses `CGWindowListCopyWindowInfo`
//! for a one-shot screenshot picker — that's a different use case (no later
//! `SCStream` handoff) and was explicitly ruled out here for that reason.
//!
//! ## Crate choice
//!
//! Uses the `screencapturekit` crate (high-level, synchronous
//! `SCShareableContent::get()` / `.windows()` / `.applications()` API, backed
//! by its own Swift FFI bridge) rather than hand-rolling ObjC message-sends or
//! using the lower-level `objc2-screen-capture-kit` bindings directly. It's
//! actively maintained (v8.0.0), has a clean, already-synchronous API (no
//! completion-handler plumbing needed on our side), and ships a
//! `snapshot()` batched-FFI path for cheap repeated polling later. App-icon
//! lookup (out of scope for that crate) uses `objc2-app-kit`'s
//! `NSRunningApplication` + `NSWorkspace`, which we need anyway for
//! `NSImage` → PNG.
//!
//! ## Permission handling
//!
//! `SCShareableContent::get()` can return `Ok` with an empty/truncated list
//! (no titles) when Screen Recording access hasn't been granted, rather than
//! a clean error — so we preflight with `CGPreflightScreenCaptureAccess()`
//! (raw C function from CoreGraphics.framework, linked directly, same
//! approach as the CoreGraphics interop in takt's `window_picker.rs`) and
//! surface a distinct "permission not granted" error variant *before* even
//! calling into ScreenCaptureKit, so the frontend can prompt properly instead
//! of rendering an empty tab strip.

use crate::sync_ext::MutexExt;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const LIST_CACHE_TTL: Duration = Duration::from_millis(2_500);
const THUMB_CACHE_TTL: Duration = Duration::from_millis(8_000);
const THUMB_PREWARM_LIMIT: usize = 8;

static LIST_CACHE: OnceLock<Mutex<Option<CachedList>>> = OnceLock::new();
static THUMB_CACHE: OnceLock<Mutex<HashMap<u32, CachedThumbnail>>> = OnceLock::new();
static THUMB_PREWARM_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct CachedList {
    captured_at: Instant,
    windows: Vec<ShareableWindow>,
}

#[derive(Clone)]
struct CachedThumbnail {
    captured_at: Instant,
    bytes: Vec<u8>,
}

/// One shareable window, ready to serialize to the frontend.
/// What a picker entry represents. Windows enumerates displays first as
/// "Screen N" cards; macOS mirrors that so the custom picker can offer
/// display sharing without relying on the system `SCContentSharingPicker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareableSourceKind {
    Window,
    Display,
}

/// Marker bit distinguishing a macOS display source id from a `CGWindowID`
/// (the same scheme the system-picker display path uses). Windows displays
/// use their own `windows_capture_target` token registry instead.
#[cfg(target_os = "macos")]
pub(crate) const DISPLAY_SOURCE_MARKER: u32 = 0x4000_0000;

/// Encode a `CGDirectDisplayID` as a picker/session source id.
#[cfg(target_os = "macos")]
pub(crate) fn display_source_id(display_id: u32) -> u32 {
    DISPLAY_SOURCE_MARKER | (display_id & 0x3fff_ffff)
}

/// Strip the display marker back to the raw `CGDirectDisplayID`.
#[cfg(target_os = "macos")]
pub(crate) fn display_id_from_source_id(source_id: u32) -> u32 {
    source_id & 0x3fff_ffff
}

/// Whether a source id refers to a display rather than a window.
#[cfg(target_os = "macos")]
pub(crate) fn is_display_source_id(source_id: u32) -> bool {
    source_id & DISPLAY_SOURCE_MARKER != 0
}

/// One shareable window, ready to serialize to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareableWindow {
    /// macOS: the stable `CGWindowID` / `SCWindow.windowID`, or a display
    /// source id (`DISPLAY_SOURCE_MARKER | CGDirectDisplayID`).
    /// Windows: a generated process-local token resolved through
    /// `windows_capture_target`; it is never the raw pointer-sized `HWND`.
    pub window_id: u32,
    /// Raw window title, untruncated. Truncation/display logic is a frontend
    /// concern (SPEC.md §4.2 tab strip). Displays use "Screen N".
    pub title: Option<String>,
    pub app_name: String,
    pub app_bundle_id: String,
    /// Owning process id, mostly useful for debugging / future filtering.
    /// Displays use 0.
    pub app_pid: i32,
    /// App icon as a `data:image/png;base64,...` string, or `None` if it
    /// couldn't be resolved (e.g. app already quit between enumeration and
    /// icon lookup). Transport-ready for an `<img src>` on the frontend.
    pub app_icon_base64: Option<String>,
    /// Whether the entry is a window or a display. Displays enumerate first
    /// as "Screen N" on both platforms; omitted -> the frontend defaults to
    /// `'window'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ShareableSourceKind>,
}

/// Error surface for [`list_shareable_windows`]. Kept distinct from a plain
/// `String` so the frontend can branch on `PermissionDenied` specifically
/// (SPEC.md §4.1's onboarding flow needs to detect this case, not just show a
/// generic error toast).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum WindowSourceError {
    /// Screen Recording TCC permission has not been granted (or a relaunch
    /// is needed after granting it — macOS doesn't always report this
    /// distinction, so the frontend's existing permission-flow polling,
    /// per SPEC.md §4.1, is the source of truth; this is just the fast
    /// preflight check before we bother calling ScreenCaptureKit at all).
    PermissionDenied(String),
    /// The platform has no complete implementation for this media operation.
    UnsupportedPlatform(String),
    /// Anything else — ScreenCaptureKit internal error, FFI failure, etc.
    Other(String),
}

impl std::fmt::Display for WindowSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionDenied(msg) | Self::UnsupportedPlatform(msg) | Self::Other(msg) => {
                write!(f, "{msg}")
            }
        }
    }
}

// =============================================================================
// macOS implementation
// =============================================================================

#[cfg(target_os = "macos")]
mod macos {
    use super::{display_source_id, ShareableSourceKind, ShareableWindow, WindowSourceError};
    use base64::Engine;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use screencapturekit::shareable_content::SCShareableContent;

    // Not a "normal app window" layer (menu bar, Dock, status items, etc.).
    // ScreenCaptureKit's `windowLayer` mirrors the same Quartz window-layer
    // values `CGWindowListCopyWindowInfo` uses: 0 is the normal app-window
    // layer, non-zero is menu bar / Dock / status items / overlays.
    const MIN_WINDOW_SIDE: f64 = crate::share_target::MIN_WINDOW_SIDE as f64;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        // Returns true if the current process already has Screen Recording
        // access; does NOT prompt. `CGRequestScreenCaptureAccess` (which does
        // prompt) is intentionally not called here — SPEC.md §4.1 wants a
        // frontloaded, explained onboarding flow to drive the actual OS
        // prompt, not a silent/incidental one triggered by enumeration.
        fn CGPreflightScreenCaptureAccess() -> bool;
    }

    /// Cheap check for whether we can expect real enumeration results.
    /// Exposed as its own Tauri command too (see `lib.rs`) so the frontend
    /// can check permission state without paying for a full enumeration.
    pub fn has_screen_recording_access() -> bool {
        unsafe { CGPreflightScreenCaptureAccess() }
    }

    fn classify_source_window(
        window_layer: i32,
        width: f64,
        height: f64,
        owner_pid: i32,
        self_pid: i32,
        owner_bundle_id: &str,
        region_selector: bool,
    ) -> crate::share_target::ShareTargetDecision {
        crate::share_target::classify(&crate::share_target::mac_window_facts(
            i64::from(window_layer),
            width,
            height,
            i64::from(owner_pid),
            i64::from(self_pid),
            Some(owner_bundle_id),
            region_selector,
            owner_pid == self_pid && !region_selector,
        ))
    }

    pub fn list() -> Result<Vec<ShareableWindow>, WindowSourceError> {
        if !has_screen_recording_access() {
            log::warn!("window_source: list() refused -- Screen Recording permission DENIED");
            return Err(WindowSourceError::PermissionDenied(
                "Screen Recording permission has not been granted to Petal. Grant it in \
                 System Settings \u{2192} Privacy & Security \u{2192} Screen Recording, then \
                 relaunch Petal."
                    .to_string(),
            ));
        }

        // `with_on_screen_windows_only` + `with_exclude_desktop_windows`
        // trims out minimized/offscreen windows and desktop-picture/icon
        // layer noise before it ever reaches us — cheaper than filtering
        // client-side, and correctness-equivalent (SPEC.md §4.2 only wants
        // currently shareable windows in the tab strip).
        let content = SCShareableContent::create()
            .with_on_screen_windows_only(true)
            .with_exclude_desktop_windows(true)
            .get()
            .map_err(|e| WindowSourceError::Other(e.to_string()))?;

        let mut out = Vec::new();
        // Displays first (mirrors the Windows picker's "Screen N" cards), so
        // the custom picker offers display sharing without the system picker.
        let display_count = content.displays().len();
        for (index, display) in content.displays().iter().enumerate() {
            out.push(ShareableWindow {
                window_id: display_source_id(display.display_id()),
                title: Some(format!("Screen {}", index + 1)),
                app_name: "Display".to_string(),
                app_bundle_id: String::new(),
                app_pid: 0,
                app_icon_base64: None,
                kind: Some(ShareableSourceKind::Display),
            });
        }
        for window in content.windows() {
            let frame = window.frame();
            let Some(owning_app) = window.owning_application() else {
                continue; // no owning app -> nothing to show/label a tab with
            };
            let app_bundle_id = owning_app.bundle_identifier();
            let title = window.title();
            let self_pid = std::process::id() as i32;
            let is_region = frame.size.width >= MIN_WINDOW_SIDE
                && frame.size.height >= MIN_WINDOW_SIDE
                && crate::region_window::is_owned_region_window(
                    title.as_deref().unwrap_or_default(),
                    owning_app.process_id(),
                    self_pid,
                );
            let decision = classify_source_window(
                window.window_layer(),
                frame.size.width,
                frame.size.height,
                owning_app.process_id(),
                self_pid,
                &app_bundle_id,
                is_region,
            );
            if !decision.is_eligible() {
                continue; // central policy rejects menu/Dock/sliver/Petal surfaces
            }
            if matches!(
                decision.kind(),
                Some(crate::share_target::ShareTargetKind::RegisteredRegion)
            ) {
                // Picker selection must take the display-region capture path,
                // not capture the hollow selector itself. Registering during
                // enumeration also makes the source immediately eligible for
                // the hover-tab share path.
                crate::region_window::register(crate::region_window::RegionWindowSource::new(
                    window.window_id(),
                    owning_app.process_id(),
                    title.clone().unwrap_or_else(|| {
                        crate::region_window::REGION_WINDOW_TITLE_PREFIX.to_string()
                    }),
                    crate::region_window::RegionRect::new(
                        frame.origin.x,
                        frame.origin.y,
                        frame.size.width,
                        frame.size.height,
                    ),
                ));
            }

            out.push(ShareableWindow {
                window_id: window.window_id(),
                title,
                app_name: owning_app.application_name(),
                app_bundle_id,
                app_pid: owning_app.process_id(),
                app_icon_base64: app_icon_png_base64(owning_app.process_id()),
                kind: Some(ShareableSourceKind::Window),
            });
        }

        log::info!(
            "window_source: list() enumerated {display_count} display(s) and {} shareable window(s)",
            out.len().saturating_sub(display_count)
        );
        Ok(out)
    }

    /// Resolve an app's icon by PID via `NSRunningApplication` and encode it
    /// as a PNG data-URL-ready base64 string. `SCRunningApplication` (from
    /// `screencapturekit`) doesn't expose the icon itself — it's a thin
    /// ScreenCaptureKit wrapper, not an AppKit one — so we look the process
    /// up again through AppKit's `NSRunningApplication`, which does carry
    /// `.icon()`.
    /// #889: EVERY ObjC allocation in here must be drained by the local
    /// autorelease pool. `list()` runs on pool-less Rust/tokio threads (it is
    /// called from `session::share::start_share` via `source_info_for_window`,
    /// among others), and `TIFFRepresentation()` returns an AUTORELEASED
    /// `NSData` holding every representation of the app icon uncompressed --
    /// measured at **73,957,376 bytes (70.5MB) per icon**, allocated through
    /// `NSAllocateMemoryPages`/`vm_allocate`. With no pool on the thread that
    /// autorelease is never balanced, so each enumeration leaked ~70MB per
    /// app: `malloc_history` on a live 1.9GB session attributed 21 calls /
    /// 1,553,104,896 bytes to exactly this stack, and the owner measured
    /// 500MB-1GB lost per share/unshare cycle (one enumeration each).
    /// Do not remove the pool, and do not hoist any ObjC value out of it.
    fn app_icon_png_base64(pid: i32) -> Option<String> {
        use objc2::rc::autoreleasepool;
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSRunningApplication};
        use objc2_foundation::{NSDictionary, NSString};

        autoreleasepool(|_| {
            let running_app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
            let icon = running_app.icon()?;

            // NSImage -> PNG: go through NSBitmapImageRep, which is the standard
            // Cocoa recipe for rasterizing an NSImage (which may be vector/PDF-
            // backed for app icons) to a concrete bitmap we can encode.
            let tiff_data = icon.TIFFRepresentation()?;
            let bitmap = NSBitmapImageRep::imageRepWithData(&tiff_data)?;
            let properties: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
            // SAFETY: `properties` is an empty dictionary, which is a valid
            // (if minimal) properties argument for PNG representation — PNG
            // encoding doesn't require any of the optional keys (e.g. JPEG
            // compression factor) that this argument exists to carry.
            let png_data = unsafe {
                bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties)
            }?;

            // `to_vec()` copies into Rust-owned memory BEFORE the pool drains,
            // so nothing below this line depends on an ObjC allocation.
            let bytes = png_data.to_vec();
            // Return a full `data:` URL, not raw base64 — this field is documented
            // as "data-URL-ready for an <img src>" and the frontend picker binds it
            // straight into `<img src={appIconBase64}>`. Raw base64 (no `data:`
            // prefix) is an invalid img src and renders as a broken-image icon,
            // which is exactly the "broken picker images" bug.
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            Some(format!("data:image/png;base64,{encoded}"))
        })
    }

    #[cfg(test)]
    mod tests {
        use crate::share_target::{
            classify, mac_window_facts, ShareTargetDecision, ShareTargetKind, ShareTargetRejection,
        };

        #[test]
        fn own_process_windows_are_excluded_from_share_source_enumeration() {
            let facts = mac_window_facts(
                0,
                40.0,
                40.0,
                42,
                42,
                Some("com.example.Source"),
                false,
                true,
            );
            assert_eq!(
                classify(&facts),
                ShareTargetDecision::Rejected(ShareTargetRejection::PetalChrome)
            );
        }

        #[test]
        fn external_normal_windows_remain_share_source_candidates() {
            let facts = mac_window_facts(
                0,
                40.0,
                40.0,
                99,
                42,
                Some("com.example.Source"),
                false,
                false,
            );
            assert_eq!(
                classify(&facts),
                ShareTargetDecision::Eligible(ShareTargetKind::Window)
            );
        }

        #[test]
        fn denylisted_bundle_windows_are_excluded_from_share_source_enumeration() {
            let facts = mac_window_facts(
                0,
                40.0,
                40.0,
                99,
                42,
                Some("com.apple.controlcenter"),
                false,
                false,
            );
            assert_eq!(
                classify(&facts),
                ShareTargetDecision::Rejected(ShareTargetRejection::DenylistedBundle)
            );
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::{has_screen_recording_access, list};

// =============================================================================
// Windows implementation: unified display + window enumeration
// =============================================================================
//
// Displays enumerate FIRST ("Screen N" entries), then windows. Both kinds are
// registered through `windows_capture_target`'s single unified token counter
// (kind disambiguates), so a picker refresh never renumbers displays and a
// display token can never collide with a window token.

#[cfg(target_os = "windows")]
mod windows {
    use super::{ShareableSourceKind, ShareableWindow, WindowSourceError};
    use crate::windows_capture_target::{self};
    use std::path::Path;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors,
        GetMonitorInfoW, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC,
        HGDIOBJ, HMONITOR, MONITORINFOEXW,
    };
    use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
    use windows::Win32::UI::WindowsAndMessaging::{
        DestroyIcon, DrawIconEx, EnumWindows, DI_NORMAL, HICON,
    };

    /// WGC has no consent gate on Windows: `CreateForWindow`/`CreateForMonitor`
    /// capture without any screen-recording permission prompt.
    pub fn has_screen_recording_access() -> bool {
        true
    }

    pub fn list() -> Result<Vec<ShareableWindow>, WindowSourceError> {
        let self_pid = std::process::id();
        let mut out = Vec::new();

        // --- Displays first: EnumDisplayMonitors → "Screen N" ---
        let mut monitors: Vec<HMONITOR> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(
                None,
                None,
                Some(enum_display_monitors_proc),
                LPARAM(&mut monitors as *mut Vec<HMONITOR> as isize),
            );
        }
        for hmonitor in monitors.iter().copied() {
            let token =
                windows_capture_target::register_display(hmonitor.0 as usize).map_err(|error| {
                    WindowSourceError::Other(format!("display registration failed: {error}"))
                })?;
            let ordinal = windows_capture_target::resolve(token)
                .ok()
                .and_then(|target| target.display_ordinal())
                .unwrap_or_default();
            out.push(ShareableWindow {
                window_id: token,
                title: Some(format!("Screen {ordinal}")),
                app_name: "Display".to_string(),
                app_bundle_id: String::new(),
                app_pid: 0,
                app_icon_base64: None,
                kind: Some(ShareableSourceKind::Display),
            });
        }

        // --- Windows: EnumWindows + filters ---
        let mut hwnds: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(enum_windows_proc),
                LPARAM(&mut hwnds as *mut Vec<HWND> as isize),
            );
        }
        for hwnd in hwnds {
            let Some(inspection) = crate::platform::windows::inspect_window(hwnd, self_pid) else {
                continue;
            };
            let decision = crate::share_target::classify(&inspection.facts);
            if !decision.is_eligible() {
                continue;
            }
            let Some(frame) = inspection.frame else {
                continue;
            };
            let pid = inspection.facts.owner_pid;
            let title = inspection.title.clone();
            let is_region = matches!(
                decision.kind(),
                Some(crate::share_target::ShareTargetKind::RegisteredRegion)
            );
            let token =
                windows_capture_target::register(hwnd.0 as usize, pid).map_err(|error| {
                    WindowSourceError::Other(format!("window registration failed: {error}"))
                })?;
            if is_region {
                crate::region_window::register(crate::region_window::RegionWindowSource::new(
                    token,
                    pid as i32,
                    title.clone().unwrap_or_default(),
                    crate::region_window::RegionRect::new(
                        frame.x as f64,
                        frame.y as f64,
                        frame.width as f64,
                        frame.height as f64,
                    ),
                ));
            }
            let exe_path = crate::platform::windows::process_exe_path(pid);
            let app_name = exe_path
                .as_deref()
                .and_then(|path| Path::new(path).file_stem())
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Unknown".to_string());
            let app_icon_base64 = exe_path.as_deref().and_then(app_icon_png_base64);
            out.push(ShareableWindow {
                window_id: token,
                title,
                app_name: app_name.clone(),
                app_bundle_id: app_name,
                app_pid: pid as i32,
                app_icon_base64,
                kind: Some(ShareableSourceKind::Window),
            });
        }

        log::info!(
            "window_source: list() enumerated {} display(s) and {} window(s)",
            monitors.len(),
            out.len().saturating_sub(monitors.len())
        );
        Ok(out)
    }

    unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let windows = &mut *(lparam.0 as *mut Vec<HWND>);
        windows.push(hwnd);
        BOOL(1)
    }

    unsafe extern "system" fn enum_display_monitors_proc(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        // Include every monitor whose rcMonitor area is non-zero (skip
        // phantom/off/zero-area entries).
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let include = unsafe { GetMonitorInfoW(hmonitor, &mut info.monitorInfo) }.as_bool()
            && info.monitorInfo.rcMonitor.right > info.monitorInfo.rcMonitor.left
            && info.monitorInfo.rcMonitor.bottom > info.monitorInfo.rcMonitor.top;
        if include {
            let monitors = &mut *(lparam.0 as *mut Vec<HMONITOR>);
            monitors.push(hmonitor);
        }
        BOOL(1)
    }

    /// App icon as a `data:image/png;base64,...` string via `SHGetFileInfoW`
    /// (large icon) drawn into a 32×32 top-down BGRA DIB, then PNG-encoded.
    fn app_icon_png_base64(exe_path: &str) -> Option<String> {
        use base64::Engine;
        let wide: Vec<u16> = exe_path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sfi = SHFILEINFOW::default();
        let result = unsafe {
            SHGetFileInfoW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            )
        };
        if result == 0 || sfi.hIcon.is_invalid() {
            return None;
        }
        let encoded = icon_to_png_base64(sfi.hIcon);
        unsafe {
            let _ = DestroyIcon(sfi.hIcon);
        }
        encoded
    }

    fn icon_to_png_base64(hicon: HICON) -> Option<String> {
        use base64::Engine;
        let hdc = unsafe { CreateCompatibleDC(None) };
        if hdc.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 32,
                biHeight: -32, // top-down: row 0 first
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [Default::default()],
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = unsafe { CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
            .ok()?;
        let old = unsafe { SelectObject(hdc, HGDIOBJ(dib.0)) };
        let drawn = unsafe { DrawIconEx(hdc, 0, 0, hicon, 32, 32, 0, None, DI_NORMAL) }.is_ok();

        let mut png_bytes = None;
        if drawn && !bits.is_null() {
            let pixels = unsafe { std::slice::from_raw_parts(bits as *const u32, 32 * 32) };
            let mut rgba = Vec::with_capacity(32 * 32 * 4);
            for &pixel in pixels {
                // 32bpp DIB memory order is BGRA (little-endian u32).
                let b = (pixel & 0xff) as u8;
                let g = ((pixel >> 8) & 0xff) as u8;
                let r = ((pixel >> 16) & 0xff) as u8;
                let a = ((pixel >> 24) & 0xff) as u8;
                rgba.extend_from_slice(&[r, g, b, a]);
            }
            if let Some(img) = image::RgbaImage::from_raw(32, 32, rgba) {
                let mut bytes = Vec::new();
                {
                    use std::io::Cursor;
                    let mut cursor = Cursor::new(&mut bytes);
                    if image::DynamicImage::ImageRgba8(img)
                        .write_to(&mut cursor, image::ImageFormat::Png)
                        .is_ok()
                    {
                        png_bytes = Some(bytes);
                    }
                }
            }
        }

        unsafe {
            let _ = SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(hdc);
        }
        let bytes = png_bytes?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Some(format!("data:image/png;base64,{encoded}"))
    }

    #[cfg(test)]
    mod tests {
        use crate::share_target::{
            classify, ShareTargetDecision, ShareTargetFacts, ShareTargetKind, ShareTargetRejection,
        };

        fn ordinary(owner_pid: u32, self_pid: u32) -> ShareTargetFacts {
            ShareTargetFacts {
                owner_pid,
                self_pid,
                visible: true,
                minimized: false,
                tool_window: false,
                app_window: true,
                cloaked: false,
                owner_present: false,
                root_owner_differs: false,
                width: 800,
                height: 600,
                layer: 0,
                region_selector: false,
                petal_chrome: false,
                system_surface: false,
                bundle_id: None,
                class_name: Some("ApplicationFrameWindow".to_string()),
                process_name: Some("example.exe".to_string()),
            }
        }

        #[test]
        fn picker_policy_is_the_central_classifier() {
            assert_eq!(
                classify(&ordinary(99, 42)),
                ShareTargetDecision::Eligible(ShareTargetKind::Window)
            );
        }

        #[test]
        fn own_process_windows_are_excluded() {
            let mut facts = ordinary(42, 42);
            facts.petal_chrome = true;
            assert_eq!(
                classify(&facts),
                ShareTargetDecision::Rejected(ShareTargetRejection::PetalChrome)
            );
        }

        #[test]
        fn hidden_minimized_tool_cloaked_owned_and_sliver_windows_are_excluded() {
            let cases: &[(&str, fn(&mut ShareTargetFacts), ShareTargetRejection)] = &[
                (
                    "hidden",
                    |f| f.visible = false,
                    ShareTargetRejection::Hidden,
                ),
                (
                    "minimized",
                    |f| f.minimized = true,
                    ShareTargetRejection::Minimized,
                ),
                (
                    "tool",
                    |f| f.tool_window = true,
                    ShareTargetRejection::ToolWindow,
                ),
                (
                    "cloaked",
                    |f| f.cloaked = true,
                    ShareTargetRejection::Cloaked,
                ),
                (
                    "owned",
                    |f| f.owner_present = true,
                    ShareTargetRejection::OwnedOrTransient,
                ),
                (
                    "sliver",
                    |f| f.width = crate::share_target::MIN_WINDOW_SIDE - 1,
                    ShareTargetRejection::TooSmall,
                ),
            ];
            for (name, mutate, expected) in cases {
                let mut facts = ordinary(99, 42);
                mutate(&mut facts);
                assert_eq!(
                    classify(&facts),
                    ShareTargetDecision::Rejected(expected.clone()),
                    "{name}"
                );
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows::{has_screen_recording_access, list};

#[cfg(target_os = "windows")]
pub(crate) use crate::platform::windows::process_exe_path;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod other {
    use super::{ShareableWindow, WindowSourceError};

    pub fn has_screen_recording_access() -> bool {
        false
    }

    pub fn list() -> Result<Vec<ShareableWindow>, WindowSourceError> {
        Err(WindowSourceError::UnsupportedPlatform(
            "Window enumeration is not implemented on this platform yet.".to_string(),
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use other::{has_screen_recording_access, list};

fn list_cache() -> &'static Mutex<Option<CachedList>> {
    LIST_CACHE.get_or_init(|| Mutex::new(None))
}

fn thumb_cache() -> &'static Mutex<HashMap<u32, CachedThumbnail>> {
    THUMB_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_list(now: Instant) -> Option<Vec<ShareableWindow>> {
    let guard = list_cache().lock_unpoisoned();
    let cached = guard.as_ref()?;
    if now.duration_since(cached.captured_at) <= LIST_CACHE_TTL {
        Some(cached.windows.clone())
    } else {
        None
    }
}

fn store_list(windows: &[ShareableWindow], now: Instant) {
    let mut guard = list_cache().lock_unpoisoned();
    *guard = Some(CachedList {
        captured_at: now,
        windows: windows.to_vec(),
    });
}

/// Drop any cached window enumeration so the next `list_cached()` re-runs the
/// real enumeration. Called by the window-change watcher
/// (`window_change_watcher`, Windows-only) when a desktop event (window
/// created/closed/minimized/restored) makes the 2.5s TTL stale — the picker's
/// event-driven refresh must see the CURRENT window set, not the pre-event
/// one. The thumbnail cache is intentionally NOT cleared: unchanged windows
/// keep their still-fresh thumbnails, and new/restored windows re-capture on
/// their own. `allow(dead_code)` on non-Windows: the only production caller
/// is the Windows watcher; macOS builds legitimately never call it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn invalidate_list_cache() {
    let mut guard = list_cache().lock_unpoisoned();
    *guard = None;
}

fn cached_thumbnail(window_id: u32, now: Instant) -> Option<Vec<u8>> {
    let guard = thumb_cache().lock_unpoisoned();
    let cached = guard.get(&window_id)?;
    if now.duration_since(cached.captured_at) <= THUMB_CACHE_TTL {
        Some(cached.bytes.clone())
    } else {
        None
    }
}

fn store_thumbnail(window_id: u32, bytes: &[u8], now: Instant) {
    let mut guard = thumb_cache().lock_unpoisoned();
    guard.insert(
        window_id,
        CachedThumbnail {
            captured_at: now,
            bytes: bytes.to_vec(),
        },
    );
    // #684: THUMB_CACHE was insert-only -- THUMB_CACHE_TTL was only ever a
    // read-side staleness check (`cached_thumbnail`), so an entry for a
    // window that has since closed (or simply hasn't been re-thumbnailed)
    // stayed resident for the process lifetime. Every store is a natural,
    // already-existing hook to sweep entries past the same TTL, with no new
    // timer/thread and no window-close event wiring needed.
    guard.retain(|_, cached| now.duration_since(cached.captured_at) <= THUMB_CACHE_TTL);
}

/// Cached shareable-window enumeration for UI surfaces that may open and
/// close repeatedly. This still calls the real ScreenCaptureKit path when the
/// short TTL expires; callers must run it off the main thread.
pub fn list_cached() -> Result<Vec<ShareableWindow>, WindowSourceError> {
    let now = Instant::now();
    if let Some(windows) = cached_list(now) {
        log::debug!(
            "window_source: list_cached() served {} window(s) from cache",
            windows.len()
        );
        prewarm_thumbnails(&windows);
        return Ok(windows);
    }

    let windows = list()?;
    store_list(&windows, now);
    prewarm_thumbnails(&windows);
    Ok(windows)
}

fn prewarm_thumbnails(windows: &[ShareableWindow]) {
    let ids: Vec<u32> = windows
        .iter()
        .take(THUMB_PREWARM_LIMIT)
        .map(|w| w.window_id)
        .collect();
    if ids.is_empty() {
        return;
    }
    if THUMB_PREWARM_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    std::thread::spawn(move || {
        for window_id in ids {
            let now = Instant::now();
            if cached_thumbnail(window_id, now).is_some() {
                continue;
            }
            match capture_window_thumbnail_uncached(window_id, THUMBNAIL_MAX_LONG_EDGE) {
                Ok(bytes) => store_thumbnail(window_id, &bytes, Instant::now()),
                Err(e) => log::debug!(
                    "window_source: thumbnail prewarm failed for window {window_id}: {e}"
                ),
            }
        }
        THUMB_PREWARM_IN_FLIGHT.store(false, Ordering::Release);
    });
}

// =============================================================================
// Thumbnail capture (cheap periodic preview, separate from the real SCStream
// capture path). Reuses takt's `capture_window_by_id` technique verbatim:
// `screencapture -x -o -l<id> -t jpg` to a temp file, read back as bytes.
// This is deliberately NOT ScreenCaptureKit — it's a lightweight, infrequent
// snapshot for the tab strip's preview thumbnail, not the realtime capture
// stream (SPEC.md §4.1), which is a separate, much heavier `SCStream` path.
// =============================================================================

/// Long edge cap for the picker's own preview thumbnails — small on purpose,
/// the card only ever displays ~284px wide.
const THUMBNAIL_MAX_LONG_EDGE: u32 = 320;

/// Capture a single window's current contents as a JPEG, by `CGWindowID`,
/// via the system `screencapture` CLI. Returns the raw JPEG bytes.
///
/// macOS-only (the `screencapture` binary and window ids are macOS
/// concepts); on other platforms this always errors.
pub fn capture_window_thumbnail(window_id: u32) -> Result<Vec<u8>, String> {
    capture_window_thumbnail_inner(window_id, false)
}

/// Capture a window thumbnail, optionally bypassing the short TTL cache.
/// `force` is used by the picker's explicit refresh: a window that stayed on
/// the desktop must re-capture even though its 8s cache entry is still valid.
pub fn capture_window_thumbnail_force(window_id: u32) -> Result<Vec<u8>, String> {
    capture_window_thumbnail_inner(window_id, true)
}

fn capture_window_thumbnail_inner(window_id: u32, force: bool) -> Result<Vec<u8>, String> {
    let now = Instant::now();
    if !force {
        if let Some(bytes) = cached_thumbnail(window_id, now) {
            return Ok(bytes);
        }
    }

    let bytes = capture_window_thumbnail_uncached(window_id, THUMBNAIL_MAX_LONG_EDGE)?;
    store_thumbnail(window_id, &bytes, Instant::now());
    Ok(bytes)
}

/// Uncached variant. AI chat's frame pump (#656) uses this directly with its
/// OWN, larger `max_long_edge` — the cache's TTL would feed the model a
/// stale frame, and a stale frame is worse than a slightly more expensive
/// capture when the model is describing what the user is looking at right
/// now. `max_long_edge` is NOT the picker's fixed 320px: a vision model
/// reading text/UI in the shared window needs meaningfully more detail than
/// a preview card does (#656's original plan called for ≤1280px; see
/// `ai_chat::session::AI_CHAT_FRAME_MAX_LONG_EDGE`) — this was a real gap
/// found while auditing against #656's own plan (this function used to
/// hardcode the picker's 320px for AI chat too, which is small enough to
/// make on-screen text illegible to the model).
pub(crate) fn capture_window_thumbnail_uncached(
    window_id: u32,
    max_long_edge: u32,
) -> Result<Vec<u8>, String> {
    #[cfg(target_os = "macos")]
    {
        // In-process ScreenCaptureKit screenshot first (fast, downscaled, no
        // subprocess); `screencapture` remains the fallback for every SCK
        // failure — including the all-zero-content case of a VMware guest
        // (#247), which is exactly why the SCK path validates content.
        match capture_window_thumbnail_sck(window_id, max_long_edge) {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                log::debug!(
                    "window_source: SCK thumbnail failed for window {window_id}: {e}; falling back to screencapture"
                );
                capture_window_thumbnail_macos(window_id, max_long_edge)
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // WGC one-shot capture works for window AND display tokens (the
        // module resolves the registry kind). Encode the BGRA frame as PNG
        // and return the raw bytes; `capture_window_thumbnail` (and the
        // frontend's `loadThumbnail`) wrap/consume them as a data URL.
        let frame = crate::windows_screen_capture::capture_one_shot(
            window_id,
            std::time::Duration::from_secs(3),
        )?;
        // The one-shot captures at the window's native resolution; the
        // picker card displays ~284px-wide thumbnails, so downscale before
        // PNG-encoding to save bytes + encode CPU. Pass through unchanged
        // for small windows.
        let (bgra, bytes_per_row, width, height) = match downscale_bgra(
            &frame.bgra,
            frame.bytes_per_row,
            frame.width,
            frame.height,
            max_long_edge,
        ) {
            Some(scaled) => scaled,
            None => (frame.bgra, frame.bytes_per_row, frame.width, frame.height),
        };
        encode_bgra_png(&bgra, bytes_per_row, width, height)
            .ok_or_else(|| "failed to encode thumbnail PNG".to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = window_id;
        let _ = max_long_edge;
        Err("Window thumbnail capture is not implemented on this platform yet.".to_string())
    }
}

/// Convert tightly-packed NV12 (Y + interleaved U/V) to tightly-packed BGRA
/// (alpha 0xff), using the BT.601 video-range profile — the capture default
/// for windows. Returns None when any plane is undersized or dims are zero.
/// (No existing equivalent: `video_color::convert_i420_to_bgra` needs I420
/// planes and `transport/publisher.rs::convert_nv12_to_i420` is private.)
fn nv12_to_bgra(
    y: &[u8],
    y_stride: u32,
    uv: &[u8],
    uv_stride: u32,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    use crate::video_color::{ycbcr_to_rgb_8bit, VideoColorProfile, YCbCr8};

    if width == 0 || height == 0 {
        return None;
    }
    let y_stride = y_stride as usize;
    let uv_stride = uv_stride as usize;
    let chroma_width = (width as usize + 1) / 2;
    let chroma_height = (height as usize + 1) / 2;
    if y_stride < width as usize
        || uv_stride < chroma_width
        || y.len() < y_stride.saturating_mul(height as usize)
        || uv.len() < uv_stride.saturating_mul(chroma_height)
    {
        return None;
    }

    let bytes_per_row = width as usize * 4;
    let mut bgra = vec![0u8; bytes_per_row * height as usize];
    let profile = VideoColorProfile::BT601_VIDEO;
    for py in 0..height as usize {
        let cy = py / 2;
        let y_row = py * y_stride;
        let uv_row = cy * uv_stride;
        let out_row = py * bytes_per_row;
        for px in 0..width as usize {
            // NV12 chroma is one interleaved U/V pair per 2x2 luma block; odd
            // widths reuse the final pair.
            let cx = (px / 2).min(chroma_width.saturating_sub(1));
            let uv_base = uv_row + cx * 2;
            let (Some(u), Some(v)) = (uv.get(uv_base), uv.get(uv_base + 1)) else {
                return None;
            };
            let rgb = ycbcr_to_rgb_8bit(
                YCbCr8 {
                    y: *y.get(y_row + px)?,
                    cb: *u,
                    cr: *v,
                },
                profile,
            );
            let out = out_row + px * 4;
            bgra[out] = rgb.b;
            bgra[out + 1] = rgb.g;
            bgra[out + 2] = rgb.r;
            bgra[out + 3] = 0xff;
        }
    }
    Some(bgra)
}

/// Sample a handful of Y and UV bytes across the frame; all-zero content is
/// the #247 signature (a VMware guest's SCK delivers Y=0/U=0/V=0 buffers
/// while `screencapture` works) and must route the thumbnail to the fallback.
fn screenshot_is_all_zero(y: &[u8], y_stride: u32, uv: &[u8], uv_stride: u32, height: u32) -> bool {
    let y_stride = y_stride as usize;
    let uv_stride = uv_stride as usize;
    let height = height as usize;
    let rows = [0usize, height / 2, height.saturating_sub(1)];
    let mut all_zero = true;
    for row in rows {
        let offset = row.saturating_mul(y_stride);
        let Some(slice) = y.get(offset..(offset.saturating_add(32)).min(y.len())) else {
            continue;
        };
        if slice.iter().any(|&b| b != 0) {
            all_zero = false;
            break;
        }
    }
    if !all_zero {
        return false;
    }
    // Luma was zero everywhere sampled; confirm the interleaved chroma too.
    let chroma_height = (height + 1) / 2;
    let chroma_rows = [0usize, chroma_height / 2, chroma_height.saturating_sub(1)];
    for row in chroma_rows {
        let offset = row.saturating_mul(uv_stride);
        let Some(slice) = uv.get(offset..(offset.saturating_add(32)).min(uv.len())) else {
            continue;
        };
        if slice.iter().any(|&b| b != 0) {
            return false;
        }
    }
    true
}

/// Scale `(width, height)` so the long edge is at most `max_long_edge`,
/// preserving aspect, then even-align both dims (thumbnails are NV12-sourced
/// and the picker cards are ~284px — 320 mirrors the Windows thumbnail path).
fn thumbnail_output_size(width: f64, height: f64, max_long_edge: u32) -> (u32, u32) {
    let long = width.max(height);
    let (w, h) = if long <= max_long_edge as f64 {
        (width.max(1.0), height.max(1.0))
    } else {
        let ratio = max_long_edge as f64 / long;
        (width * ratio, height * ratio)
    };
    (
        ((w.round() as u32) & !1).max(2),
        ((h.round() as u32) & !1).max(2),
    )
}

/// Encode a tightly-packed BGRA raster as JPEG bytes (thumbnail wire format —
/// the frontend labels raw thumbnail bytes `data:image/jpeg`).
#[cfg(target_os = "macos")]
fn encode_bgra_jpeg(bgra: &[u8], bytes_per_row: usize, width: u32, height: u32) -> Option<Vec<u8>> {
    let expected = usize::try_from(width)
        .ok()?
        .checked_mul(4)
        .and_then(|stride| stride.checked_mul(usize::try_from(height).ok()?))?;
    if bytes_per_row < usize::try_from(width).ok()? * 4 || bgra.len() < expected {
        return None;
    }
    let mut rgba = Vec::with_capacity(expected);
    for row in 0..height as usize {
        let start = row * bytes_per_row;
        for col in 0..width as usize {
            let offset = start + col * 4;
            rgba.extend_from_slice(&[bgra[offset + 2], bgra[offset + 1], bgra[offset], 0xff]);
        }
    }
    let img = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut bytes = Vec::new();
    {
        use std::io::Cursor;
        let mut cursor = Cursor::new(&mut bytes);
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut cursor, image::ImageFormat::Jpeg)
            .ok()?;
    }
    Some(bytes)
}

/// `SCScreenshotManager` (and the `SCContentSharingPicker`) require macOS 14;
/// older macOS routes every thumbnail through the `screencapture` fallback.
#[cfg(target_os = "macos")]
fn macos_at_least_14() -> bool {
    use objc2_foundation::{NSOperatingSystemVersion, NSProcessInfo};
    let version: NSOperatingSystemVersion = NSProcessInfo::processInfo().operatingSystemVersion();
    (
        version.majorVersion,
        version.minorVersion,
        version.patchVersion,
    ) >= (14, 0, 0)
}

/// In-process ScreenCaptureKit thumbnail for one window: `SCScreenshotManager`
/// captures the window at a small output size (long edge ≤ `max_long_edge`,
/// even dims) directly, avoiding the `screencapture` subprocess, the temp
/// file, and the full-resolution copy of the legacy path. Every failure
/// returns `Err` so the caller can fall back to `screencapture` — including
/// the all-zero-content case (#247: a VMware guest's SCK delivers empty
/// buffers while `screencapture` works).
#[cfg(target_os = "macos")]
fn capture_window_thumbnail_sck(window_id: u32, max_long_edge: u32) -> Result<Vec<u8>, String> {
    // `image_buffer()` is on `CMSampleBufferExt` (the generic CoreMedia
    // accessors), NOT `CMSampleBufferSCExt` (which only carries the
    // SCStreamFrameInfo attachments like frame_status/dirty_rects).
    use screencapturekit::cm::CMSampleBufferExt;
    use screencapturekit::screenshot_manager::SCScreenshotManager;
    use screencapturekit::shareable_content::SCShareableContent;
    use screencapturekit::stream::configuration::{PixelFormat, SCStreamConfiguration};
    use screencapturekit::stream::content_filter::SCContentFilter;

    if !macos_at_least_14() {
        return Err("SCScreenshotManager requires macOS 14".to_string());
    }

    // Same enumeration + filter construction the real capture path uses, so
    // the thumbnail and the eventual share agree on source identity. Display
    // source ids (DISPLAY_SOURCE_MARKER | CGDirectDisplayID) build a display
    // filter instead of a window filter.
    let content = SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|e| format!("SCShareableContent enumeration failed: {e}"))?;
    let (filter, frame) = if is_display_source_id(window_id) {
        let display_id = display_id_from_source_id(window_id);
        let display = content
            .displays()
            .into_iter()
            .find(|d| d.display_id() == display_id)
            .ok_or_else(|| format!("display {display_id} not found in SCShareableContent"))?;
        let frame = display.frame();
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();
        (filter, frame)
    } else {
        let window = content
            .windows()
            .into_iter()
            .find(|w| w.window_id() == window_id)
            .ok_or_else(|| format!("window {window_id} not found in SCShareableContent"))?;
        let frame = window.frame();
        let filter = SCContentFilter::create().with_window(&window).build();
        (filter, frame)
    };
    // `frame` is in POINTS (SCWindow::frame()/SCDisplay::frame() are CGRects
    // in the window-coordinate space), but SCStreamConfiguration's width/
    // height are PIXELS -- the real capture path already carries this
    // distinction (capture.rs's `capture_pixel_size`). Missing it here meant
    // a Retina window under `max_long_edge` POINTS (extremely common --
    // e.g. a half-screen window on a 14" MacBook is ~756pt / 1512px) was
    // requested at 1x, silently capping AI chat's frames well under the
    // 1280px budget this fix exists to deliver. `point_pixel_scale()` is the
    // SAME scale the real filter would use if it opened a stream, not a
    // separate NSScreen/CGDisplay lookup that could disagree with it.
    let scale = f64::from(filter.point_pixel_scale()).max(1.0);
    let (output_w, output_h) = thumbnail_output_size(
        frame.size.width * scale,
        frame.size.height * scale,
        max_long_edge,
    );

    let config = SCStreamConfiguration::new()
        .with_width(output_w)
        .with_height(output_h)
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_shows_cursor(false);

    let sample = SCScreenshotManager::capture_sample_buffer(&filter, &config)
        .map_err(|e| format!("SCScreenshotManager capture failed: {e}"))?;
    let pixel_buffer = sample
        .image_buffer()
        .ok_or_else(|| "SCK screenshot sample has no image buffer".to_string())?;

    let width = pixel_buffer.width() as u32;
    let height = pixel_buffer.height() as u32;
    let payload = crate::capture::copy_nv12_payload(&pixel_buffer, None)
        .map_err(|e| format!("SCK thumbnail NV12 copy failed: {e}"))?;
    let crate::capture::CapturedFramePayload::Nv12 {
        y,
        y_stride,
        uv,
        uv_stride,
        ..
    } = payload
    else {
        return Err("SCK thumbnail payload was not NV12".to_string());
    };

    if screenshot_is_all_zero(&y, y_stride, &uv, uv_stride, height) {
        return Err("SCK screenshot returned all-zero content (empty backing store)".to_string());
    }

    let bgra = nv12_to_bgra(&y, y_stride, &uv, uv_stride, width, height)
        .ok_or_else(|| "NV12 thumbnail conversion failed".to_string())?;
    let bytes = encode_bgra_jpeg(&bgra, width as usize * 4, width, height)
        .ok_or_else(|| "failed to encode thumbnail JPEG".to_string())?;
    log::info!(
        "window_source: thumbnail for window {window_id} captured via SCK ({width}x{height})"
    );
    Ok(bytes)
}

/// Scale a tightly-packed BGRA raster so the longer side is at most `max_dim`
/// (preserving aspect), returning the scaled tight-packed raster + new dims.
/// Nearest-neighbor: thumbnails are small and the picker card downscales
/// anyway, so quality is sufficient and it avoids box-filter cost. Returns
/// `None` only when `width` or `height` is 0; the caller treats that as a
/// pass-through (unreachable in practice, since `capture_one_shot` rejects
/// zero-size targets).
#[cfg(target_os = "windows")]
fn downscale_bgra(
    bgra: &[u8],
    bytes_per_row: usize,
    width: u32,
    height: u32,
    max_dim: u32,
) -> Option<(Vec<u8>, usize, u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    if width <= max_dim && height <= max_dim {
        return Some((bgra.to_vec(), bytes_per_row, width, height));
    }
    let scale = max_dim as f64 / width.max(height) as f64;
    let new_width = ((width as f64 * scale).round() as u32).max(1);
    let new_height = ((height as f64 * scale).round() as u32).max(1);
    let mut out = vec![0u8; new_width as usize * new_height as usize * 4];
    for y in 0..new_height {
        let src_y = ((y as u64 * height as u64) / new_height as u64) as usize;
        let src_row = src_y * bytes_per_row;
        let dst_row = y as usize * new_width as usize * 4;
        for x in 0..new_width {
            let src_x = ((x as u64 * width as u64) / new_width as u64) as usize;
            let src = src_x * 4;
            let dst = dst_row + x as usize * 4;
            out[dst..dst + 4].copy_from_slice(&bgra[src_row + src..src_row + src + 4]);
        }
    }
    Some((out, new_width as usize * 4, new_width, new_height))
}

/// Encode a tightly-packed BGRA raster as PNG bytes (thumbnail wire format).
#[cfg(target_os = "windows")]
fn encode_bgra_png(bgra: &[u8], bytes_per_row: usize, width: u32, height: u32) -> Option<Vec<u8>> {
    let expected = usize::try_from(width)
        .ok()?
        .checked_mul(4)
        .and_then(|stride| stride.checked_mul(usize::try_from(height).ok()?))?;
    if bytes_per_row < usize::try_from(width).ok()? * 4 || bgra.len() < expected {
        return None;
    }
    let mut rgba = Vec::with_capacity(expected);
    for row in 0..height as usize {
        let start = row * bytes_per_row;
        for col in 0..width as usize {
            let offset = start + col * 4;
            rgba.extend_from_slice(&[bgra[offset + 2], bgra[offset + 1], bgra[offset], 0xff]);
        }
    }
    let img = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut bytes = Vec::new();
    {
        use std::io::Cursor;
        let mut cursor = Cursor::new(&mut bytes);
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .ok()?;
    }
    Some(bytes)
}

#[cfg(target_os = "macos")]
fn capture_window_thumbnail_macos(source_id: u32, max_long_edge: u32) -> Result<Vec<u8>, String> {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "petal-window-thumb-{source_id}-{}.jpg",
        std::process::id()
    ));

    // Same flags takt's `capture_window_by_id` uses (window_picker.rs
    // ~line 450): `-x` no sound, `-o` omit the window shadow, `-l<id>`
    // capture that exact window by CGWindowID (works even if occluded or
    // moved), `-t jpg` output format. Display source ids capture by
    // `CGDirectDisplayID` with `-D<id>` instead.
    let mut command = std::process::Command::new("screencapture");
    command.arg("-x").arg("-o").arg("-t").arg("jpg");
    let target_flag = if is_display_source_id(source_id) {
        format!("-D{}", display_id_from_source_id(source_id))
    } else {
        format!("-l{source_id}")
    };
    let output = command
        .arg(&target_flag)
        .arg(&path)
        .output()
        .map_err(|e| format!("failed to launch screencapture: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "screencapture {target_flag} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let bytes =
        std::fs::read(&path).map_err(|e| format!("failed to read captured thumbnail: {e}"))?;
    let _ = std::fs::remove_file(&path); // best-effort cleanup of the temp file
                                         // Unlike the SCK path, `screencapture` has no output-size flag -- it
                                         // always captures at the window's native resolution. Every macOS <14
                                         // caller (and every SCK failure) hits this path, so without a downscale
                                         // step here too, that population would send full-resolution frames
                                         // straight through -- exactly the bug this pass fixes for the common
                                         // (SCK) path, just reachable a different way. Best-effort: a resize
                                         // failure returns the native-resolution bytes rather than erroring the
                                         // whole capture over an optimization.
    Ok(downscale_jpeg(&bytes, max_long_edge).unwrap_or(bytes))
}

/// Decode a JPEG, scale it down (preserving aspect) if its long edge exceeds
/// `max_long_edge`, and re-encode. `None` on any decode/encode failure, or
/// when the image is already within bounds (nothing to do) -- the caller
/// falls back to the original bytes either way, so this never needs to be
/// infallible.
#[cfg(target_os = "macos")]
fn downscale_jpeg(jpeg: &[u8], max_long_edge: u32) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?;
    let long_edge = img.width().max(img.height());
    if long_edge <= max_long_edge {
        return None;
    }
    let scaled = img.resize(
        max_long_edge,
        max_long_edge,
        image::imageops::FilterType::Triangle,
    );
    let mut out = Vec::new();
    {
        use std::io::Cursor;
        scaled
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_output_size_caps_the_long_edge_and_preserves_aspect() {
        // A window bigger than the cap must scale down.
        assert_eq!(thumbnail_output_size(1920.0, 1080.0, 320), (320, 180));
        // Already-small input passes through unscaled (still even-aligned).
        assert_eq!(thumbnail_output_size(200.0, 150.0, 320), (200, 150));
    }

    /// Regression for the Retina points-vs-pixels bug: `SCWindow::frame()`/
    /// `SCDisplay::frame()` report POINTS, but `SCStreamConfiguration`'s
    /// width/height are PIXELS. A caller that forgets to scale by the
    /// display's `point_pixel_scale()` before calling this function silently
    /// requests a 1x capture for any window under `max_long_edge` POINTS --
    /// e.g. a 756x945-point half-screen window on a 2x display should
    /// deliver its real 1512x1890-pixel content capped at 1280, not a
    /// pre-scale 756x945 that never even reaches the cap. This test exists
    /// to prove `thumbnail_output_size` itself does the right thing once
    /// GIVEN pixel-scaled input -- the actual scaling happens at the SCK
    /// call site (`capture_window_thumbnail_sck`), which cannot be unit
    /// tested without a real ScreenCaptureKit session.
    #[test]
    fn thumbnail_output_size_caps_a_retina_scaled_window_correctly() {
        let points_w = 756.0;
        let points_h = 945.0;
        let scale = 2.0;
        assert_eq!(
            thumbnail_output_size(points_w * scale, points_h * scale, 1280),
            (1024, 1280),
            "a Retina window's real pixel size must be what gets capped, not its point size"
        );
    }

    #[test]
    fn window_source_error_display_includes_message() {
        let err = WindowSourceError::PermissionDenied("nope".to_string());
        assert_eq!(err.to_string(), "nope");
        let err = WindowSourceError::Other("boom".to_string());
        assert_eq!(err.to_string(), "boom");
    }

    // `downscale_jpeg` is the fallback (`screencapture` CLI) path's downscale
    // step -- macOS-only, same reason `downscale_bgra`'s tests are gated to
    // Windows: the function does not exist on other platforms, so an ungated
    // test fails to BUILD there (not just fails to run), blocking the
    // pre-push hook everywhere.
    #[cfg(target_os = "macos")]
    fn solid_jpeg(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([80, 140, 200]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        bytes
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn downscale_jpeg_caps_the_long_edge_and_preserves_aspect() {
        let jpeg = solid_jpeg(1920, 1080);
        let scaled =
            downscale_jpeg(&jpeg, 1280).expect("a 1920-wide image over the cap must scale");
        let decoded =
            image::load_from_memory_with_format(&scaled, image::ImageFormat::Jpeg).unwrap();
        assert_eq!(decoded.width(), 1280, "long edge must be capped exactly");
        // 1920x1080 is a 16:9 frame; 1280 long edge -> 720 short edge.
        assert_eq!(decoded.height(), 720, "aspect ratio must be preserved");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn downscale_jpeg_passes_through_images_already_within_bounds() {
        let jpeg = solid_jpeg(800, 600);
        assert!(
            downscale_jpeg(&jpeg, 1280).is_none(),
            "an image already under the cap must not be touched -- the caller falls back to \
             the original bytes on None"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn downscale_jpeg_rejects_garbage_input() {
        assert!(downscale_jpeg(b"not a jpeg", 1280).is_none());
    }

    // `downscale_bgra` is `#[cfg(target_os = "windows")]`, so these three tests
    // must carry the same gate. Without it they still compile on macOS, where
    // the function does not exist, and `cargo test --lib` fails to BUILD at all
    // ("cannot find function `downscale_bgra`") -- which also blocks the
    // pre-push hook. They still run on Windows, the only platform the function
    // exists on.
    #[cfg(target_os = "windows")]
    #[test]
    fn downscale_bgra_caps_long_side_and_preserves_aspect() {
        let width = 1920u32;
        let height = 1080u32;
        let bytes_per_row = width as usize * 4;
        let mut bgra = vec![0u8; bytes_per_row * height as usize];
        // Paint each pixel with a distinct value so sampling is verifiable.
        for y in 0..height {
            for x in 0..width {
                let i = y as usize * bytes_per_row + x as usize * 4;
                bgra[i] = (x % 256) as u8;
                bgra[i + 1] = (y % 256) as u8;
                bgra[i + 2] = 0;
                bgra[i + 3] = 255;
            }
        }

        let (scaled, out_stride, out_w, out_h) =
            downscale_bgra(&bgra, bytes_per_row, width, height, 320).unwrap();

        assert_eq!(out_w, 320);
        assert_eq!(out_h, 180);
        assert_eq!(out_stride, out_w as usize * 4);
        assert_eq!(scaled.len(), out_stride * out_h as usize);
        // Nearest-neighbor sampling: the top-left output pixel must equal the
        // source pixel at (0, 0).
        assert_eq!(scaled[0], bgra[0]);
        assert_eq!(scaled[1], bgra[1]);
        // The last output pixel maps to a source coordinate near the
        // bottom-right corner (never out of bounds).
        let last = (out_h as usize - 1) * out_stride + (out_w as usize - 1) * 4;
        let src_x = ((out_w as u64 - 1) * width as u64 / out_w as u64) as usize;
        let src_y = ((out_h as u64 - 1) * height as u64 / out_h as u64) as usize;
        let src = src_y * bytes_per_row + src_x * 4;
        assert_eq!(scaled[last], bgra[src]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn downscale_bgra_passes_through_small_windows() {
        let bgra = vec![0u8; 4];
        let (scaled, stride, w, h) = downscale_bgra(&bgra, 4, 1, 1, 320).unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(stride, 4);
        assert_eq!(scaled, bgra);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn downscale_bgra_rejects_zero_size() {
        assert!(downscale_bgra(&[], 0, 0, 0, 320).is_none());
    }

    /// Thumbnail cache contract the picker's forced refresh depends on:
    /// within the TTL a seeded entry is served; a re-capture (what
    /// `capture_window_thumbnail_force` does) overwrites the entry so the
    /// next read returns the NEW bytes, not the stale ones.
    #[test]
    fn thumbnail_cache_serves_within_ttl_and_overwrites_on_recapture() {
        let now = Instant::now();
        store_thumbnail(4242, b"stale", now);
        assert_eq!(cached_thumbnail(4242, now), Some(b"stale".to_vec()));
        // A forced re-capture stores fresh bytes over the stale entry.
        store_thumbnail(4242, b"fresh", now);
        assert_eq!(cached_thumbnail(4242, now), Some(b"fresh".to_vec()));
        // TTL expiry evicts.
        let later = now + THUMB_CACHE_TTL + Duration::from_millis(1);
        assert_eq!(cached_thumbnail(4242, later), None);
    }

    /// #684: `THUMB_CACHE` was insert-only -- `THUMB_CACHE_TTL` was applied
    /// only on read (`cached_thumbnail`), so a window that closed (or simply
    /// was never re-thumbnailed) left its `CachedThumbnail` JPEG resident in
    /// the map for the process lifetime. `store_thumbnail` now sweeps
    /// expired entries against the existing TTL on every store -- the
    /// picker's own prewarm/force-refresh calls already hit this path, so no
    /// new timer or window-close event wiring is needed. This seeds an
    /// artificially-expired entry directly (simulating a since-closed
    /// window's stale thumbnail) and asserts a later store evicts it from
    /// the underlying map, not just from what a TTL-gated read would return.
    #[test]
    fn store_thumbnail_evicts_expired_entries_from_a_since_closed_window() {
        let now = Instant::now();
        let expired_at = now
            .checked_sub(THUMB_CACHE_TTL + Duration::from_millis(1))
            .expect("test host must have >8s of monotonic uptime");

        // Use window ids unique to this test so it can't collide with the
        // shared-process-static THUMB_CACHE used by sibling tests.
        let closed_window_id = 684_001;
        let live_window_id = 684_002;

        {
            let mut guard = thumb_cache().lock_unpoisoned();
            guard.insert(
                closed_window_id,
                CachedThumbnail {
                    captured_at: expired_at,
                    bytes: b"stale-closed-window".to_vec(),
                },
            );
        }

        // Any new store is the natural sweep trigger -- no dedicated janitor
        // thread required.
        store_thumbnail(live_window_id, b"fresh", now);

        let guard = thumb_cache().lock_unpoisoned();
        assert!(
            !guard.contains_key(&closed_window_id),
            "expired entry for a since-closed window must be evicted from the map, not just \
             hidden behind the read-side TTL check"
        );
        assert!(
            guard.contains_key(&live_window_id),
            "the entry just stored must survive its own sweep"
        );
    }

    /// The watcher's cache-bust contract: after `invalidate_list_cache()`, a
    /// seeded list entry is gone so the next `list_cached()` re-enumerates —
    /// the 2.5s TTL must not serve a pre-event window set to the picker's
    /// event-driven refresh.
    #[test]
    fn invalidate_list_cache_drops_seeded_list() {
        let now = Instant::now();
        store_list(&[], now);
        assert!(cached_list(now).is_some());
        invalidate_list_cache();
        assert!(cached_list(now).is_none());
    }
}
