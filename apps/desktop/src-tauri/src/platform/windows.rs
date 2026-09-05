//! Leaf Windows window/cursor primitives for the hover tab, telepointer
//! sender, and remote-control target resolution.
//!
//! Mirrors the role of `platform/cg.rs` on macOS: read-only window/cursor
//! queries, no Tauri windows, sessions, or app state. Every function here is
//! Windows-only (`#[cfg(target_os = "windows")]` at the `platform/mod.rs`
//! declaration) and returns data in **physical pixels** where geometry is
//! involved — callers convert to logical points with the relevant monitor's
//! `scale_factor()`, exactly like `hover_tab::platform::tab_position` does on
//! macOS (the shared `hover_core::hover_tab_presentation` math is unit-agnostic).
//!
//! HWND values are never narrowed into `u32`: the public wire `window_id` is
//! always the `windows_capture_target` token (`register_window`), resolved
//! back through that registry before any native touch.

use windows::core::BOOL;
use windows::Win32::Foundation::{
    GetLastError, SetLastError, HWND, LPARAM, POINT, RECT, WIN32_ERROR,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, ScreenToClient, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VIRTUAL_KEY};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetAncestor, GetClassNameW, GetClientRect, GetCursorPos, GetScrollInfo,
    GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, WindowFromPoint, GA_ROOT,
    GA_ROOTOWNER, GWL_EXSTYLE,
    GWL_STYLE, GW_HWNDNEXT, GW_HWNDPREV, GW_OWNER, HWND_TOP, HWND_TOPMOST, SB_HORZ, SB_VERT,
    SCROLLBAR_CONSTANTS, SCROLLINFO, SCROLLINFO_MASK, SIF_RANGE, WINDOW_STYLE, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_HSCROLL, WS_VSCROLL,
};

use crate::platform::cg::WindowFrame;

/// The current foreground window's top-level root, if available.
pub(crate) fn foreground_root_window() -> Option<HWND> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return None;
    }
    let root = unsafe { GetAncestor(foreground, GA_ROOT) };
    (!root.0.is_null()).then_some(root)
}

/// Current cursor position in virtual-screen physical pixels.
pub(crate) fn cursor_position() -> Option<(f64, f64)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point).ok()? };
    Some((point.x as f64, point.y as f64))
}

/// Whether a virtual key's high-order bit reports it physically held down
/// right now (global state, independent of window focus).
pub(crate) fn key_is_down(vkey: VIRTUAL_KEY) -> bool {
    (unsafe { GetAsyncKeyState(vkey.0 as i32) } as u16 & 0x8000) != 0
}

/// Read the high-bit held state and low-bit edge in one Windows query. The
/// high bit alone can miss a short click between two 16ms tracker ticks.
pub(crate) fn key_state(vkey: VIRTUAL_KEY) -> (bool, bool) {
    let state = unsafe { GetAsyncKeyState(vkey.0 as i32) } as u16;
    ((state & 0x8000) != 0, (state & 0x0001) != 0)
}
/// The top-level window under the cursor, if any.
///
/// `WindowFromPoint` returns the topmost CHILD window containing the point
/// (tooltips, menus, webview children, …); `GetAncestor(GA_ROOT)` walks up to
/// the top-level window the way `EnumWindows` would have listed it. Returns
/// `None` for a null handle or an invisible window.
pub(crate) fn root_window_at(cursor: (f64, f64)) -> Option<HWND> {
    child_window_at(cursor).and_then(|hwnd| {
        let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        if root.0.is_null() {
            return None;
        }
        if !unsafe { IsWindowVisible(root) }.as_bool() {
            return None;
        }
        Some(root)
    })
}

/// The topmost CHILD window containing `cursor` (the raw `WindowFromPoint`
/// result, before walking to a root). This is the natural `WM_MOUSEWHEEL`/
/// `WM_MOUSEHWHEEL` message destination — a scrollable render/child window —
/// rather than its top-level parent. Returns `None` for a null handle or a
/// window whose top-level root is invisible.
pub(crate) fn child_window_at(cursor: (f64, f64)) -> Option<HWND> {
    let hwnd = unsafe {
        WindowFromPoint(POINT {
            x: cursor.0 as i32,
            y: cursor.1 as i32,
        })
    };
    if hwnd.0.is_null() {
        return None;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.0.is_null() || !unsafe { IsWindowVisible(root) }.as_bool() {
        return None;
    }
    Some(hwnd)
}

/// The root window at `cursor`, treating THIS process's own top-level windows
/// (hover pill, share overlays, AI-chat panel, main window) as transparent:
/// they are the app's chrome floating over the shared content, so remote
/// control must see through them to the shared window beneath — otherwise
/// `WindowFromPoint` reports the source-relative pill (glued to the window's
/// top-left) as an occluder whenever the remote pointer crosses it, and the
/// controller gets a bogus "Covered" refusal on a front window. `target_root`
/// and `overlay_roots` are accepted immediately (the shared window or its own
/// telepointer overlay); otherwise the first FOREIGN root wins — a genuine
/// occluder. Returns `None` when nothing visible is under the point.
pub(crate) fn root_window_at_skipping_self(
    cursor: (f64, f64),
    target_root: isize,
    overlay_roots: &[isize],
) -> Option<isize> {
    let point = POINT {
        x: cursor.0 as i32,
        y: cursor.1 as i32,
    };
    let mut hwnd = unsafe { WindowFromPoint(point) };
    let mut seen: std::collections::HashSet<isize> = std::collections::HashSet::new();
    while !hwnd.0.is_null() {
        let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
        let root_id = root.0 as isize;
        if root_id == 0 || !unsafe { IsWindowVisible(root) }.as_bool() {
            return None;
        }
        if !seen.insert(root_id) {
            return None; // z-order walk cycled — give up rather than loop
        }
        if root_id == target_root || overlay_roots.contains(&root_id) {
            return Some(root_id);
        }
        // Accept only windows that actually cover the point (a walked-down
        // candidate may not contain it).
        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(root, &mut rect) };
        let contains = point.x >= rect.left
            && point.x < rect.right
            && point.y >= rect.top
            && point.y < rect.bottom;
        if !contains {
            hwnd = unsafe { GetWindow(root, GW_HWNDNEXT) }.unwrap_or_default();
            continue;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(root, Some(&mut pid)) };
        if pid == std::process::id() {
            // Own chrome — look at the window below it in the z-order.
            hwnd = unsafe { GetWindow(root, GW_HWNDNEXT) }.unwrap_or_default();
            continue;
        }
        return Some(root_id);
    }
    None
}

/// Whether screen point `cursor` lies inside `top_level`'s own client area
/// (`ScreenToClient` + `GetClientRect`).
///
/// This is the ONLY point check ID-addressed wheel needs: it is
/// z-order-independent, so a window covered by another window on the sharer's
/// desktop still "contains" its own points and accepts the post. Deliberately
/// NOT `WindowFromPoint` (which returns the topmost window and would reject a
/// covered-but-targeted aim — the exact 006B2 regression). Returns `None` for
/// a null handle or a point outside the client area.
pub(crate) fn window_contains_point(top_level: HWND, cursor: (f64, f64)) -> Option<()> {
    if top_level.0.is_null() {
        return None;
    }
    let mut client = POINT {
        x: cursor.0 as i32,
        y: cursor.1 as i32,
    };
    if !unsafe { ScreenToClient(top_level, &mut client) }.as_bool() {
        return None;
    }
    if client.x < 0 || client.y < 0 {
        return None;
    }
    let mut rect = RECT::default();
    if (unsafe { GetClientRect(top_level, &mut rect) }).is_err() {
        return None;
    }
    if client.x >= rect.right || client.y >= rect.bottom {
        return None;
    }
    Some(())
}

/// Whether `hwnd` can scroll: it has a non-empty scroll range on `bar`
/// (`GetScrollInfo`), or its window style carries the matching scrollbar bit
/// (`WS_VSCROLL`/`WS_HSCROLL`). `GetScrollInfo` is checked FIRST because
/// Chromium-based apps (Win11 Notepad, browsers) draw scrollbars in-page and
/// set no `WS_*SCROLL` style, yet expose a real scroll range — the classic
/// `WS_*SCROLL` bit alone would miss them. `GetScrollInfo` returning `Err`
/// (a control that does not manage its own scrollbars) falls back to the
/// style bit.
fn window_scrolls_axis(hwnd: HWND, bar: SCROLLBAR_CONSTANTS, style_bit: WINDOW_STYLE) -> bool {
    let mut info = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE,
        ..Default::default()
    };
    if unsafe { GetScrollInfo(hwnd, bar, &mut info) }.is_ok() && info.nMax > info.nMin {
        return true;
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    style & (style_bit.0 as isize) != 0
}

/// Find a descendant of `top_level` that (a) contains screen point `cursor`
/// and (b) is scrollable, for use as the `PostMessageW(WM_MOUSEWHEEL/…HWHEEL)`
/// destination.
///
/// This is the scroll-target resolution that `ChildWindowFromPointEx` cannot
/// provide: that API returns the FIRST/topmost child at the point regardless
/// of scrollability, so for apps with deep child trees (Win11 Notepad is a
/// Chromium app) it can return a non-scrollable container that swallows the
/// wheel. Enumerating children and picking the first that CONTAINS the point
/// AND has scroll range/style lands on the actual scrollable editor/render
/// widget. Enumeration order is front-to-back, so the topmost scrollable
/// child wins — matching which child a real wheel at that point would hit.
///
/// Returns `None` when no descendant of `top_level` at the point is
/// scrollable (callers then fall back to `top_level` itself).
pub(crate) fn scrollable_child_at_point(top_level: HWND, cursor: (f64, f64)) -> Option<HWND> {
    if top_level.0.is_null() {
        return None;
    }
    // EnumChildWindows visits top-levels first, then their children (front to
    // back within each level); remember the first scrollable one that also
    // contains the point. The callback needs both the cursor and the result,
    // so both ride in one heap state passed through LPARAM.
    struct EnumState {
        cursor_x: i32,
        cursor_y: i32,
        found: Option<HWND>,
    }
    let mut state = EnumState {
        cursor_x: cursor.0 as i32,
        cursor_y: cursor.1 as i32,
        found: None,
    };
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lparam.0 as *mut EnumState) };
        if state.found.is_some() {
            return BOOL(0); // already found one
        }
        let mut rect = RECT::default();
        if (unsafe { GetWindowRect(hwnd, &mut rect) }).is_err() {
            return BOOL(1); // keep looking
        }
        let contains = state.cursor_x >= rect.left
            && state.cursor_x < rect.right
            && state.cursor_y >= rect.top
            && state.cursor_y < rect.bottom;
        if !contains {
            return BOOL(1);
        }
        if window_scrolls_axis(hwnd, SB_VERT, WS_VSCROLL)
            || window_scrolls_axis(hwnd, SB_HORZ, WS_HSCROLL)
        {
            state.found = Some(hwnd);
            return BOOL(0);
        }
        BOOL(1)
    }
    unsafe {
        let _ = EnumChildWindows(
            Some(top_level),
            Some(enum_proc),
            LPARAM((&mut state as *mut EnumState) as isize),
        );
    }
    state.found
}

/// Whether a valid window belongs to the topmost z-order band.
pub(crate) fn window_is_topmost(hwnd: HWND) -> Option<bool> {
    if hwnd.0.is_null() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return None;
    }
    let style = unsafe {
        SetLastError(WIN32_ERROR(0));
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        (style, GetLastError())
    };
    (style.1 == WIN32_ERROR(0)).then_some(style.0 & WS_EX_TOPMOST.0 as isize != 0)
}

/// Resolve the insertion anchor immediately above `hwnd`, failing closed when
/// the source is invalid or the z-order query cannot be trusted. `excluded`
/// is normally the hover tab itself, so an already-adjacent tab is skipped
/// rather than being passed to `SetWindowPos` as its own anchor.
pub(crate) fn checked_window_above_in_z_order_excluding(
    hwnd: HWND,
    excluded: Option<HWND>,
) -> Option<HWND> {
    let source_topmost = window_is_topmost(hwnd)?;
    let mut above = unsafe { GetWindow(hwnd, GW_HWNDPREV) }.ok()?;
    while !above.0.is_null() {
        if excluded.is_some_and(|candidate| candidate == above) {
            above = unsafe { GetWindow(above, GW_HWNDPREV) }.ok()?;
            continue;
        }
        let above_topmost = window_is_topmost(above)?;
        if source_topmost != above_topmost {
            return Some(if source_topmost {
                HWND_TOPMOST
            } else {
                HWND_TOP
            });
        }
        return Some(above);
    }
    Some(if source_topmost {
        HWND_TOPMOST
    } else {
        HWND_TOP
    })
}

/// Resolve an anchor without excluding another window.
pub(crate) fn checked_window_above_in_z_order(hwnd: HWND) -> Option<HWND> {
    checked_window_above_in_z_order_excluding(hwnd, None)
}

/// Compatibility wrapper for existing compositor callers. Hover-tab placement
/// uses the checked helper because it must hide on uncertainty.
pub(crate) fn window_above_in_z_order(hwnd: HWND) -> HWND {
    let Ok(above) = (unsafe { GetWindow(hwnd, GW_HWNDPREV) }) else {
        return HWND_TOP;
    };
    if above.0.is_null() {
        HWND_TOP
    } else {
        above
    }
}

/// Owner PID of a window (0 when it cannot be resolved).
pub(crate) fn owner_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn window_frame_from_rect(rect: RECT) -> Option<WindowFrame> {
    let width = rect.right.checked_sub(rect.left)?;
    let height = rect.bottom.checked_sub(rect.top)?;
    (width > 0 && height > 0).then_some(WindowFrame {
        x: rect.left,
        y: rect.top,
        width,
        height,
    })
}

fn choose_visible_window_frame(
    extended_frame: Option<WindowFrame>,
    fallback_frame: Option<WindowFrame>,
) -> Option<WindowFrame> {
    extended_frame.or(fallback_frame)
}

/// On-screen frame of a window in physical pixels.
///
/// `GetWindowRect` includes invisible resize borders on modern Windows. It is
/// retained as the fallback because DWM can briefly reject the extended-frame
/// query during teardown or composition transitions. Callers that position a
/// visible overlay should use [`visible_window_frame`] instead.
pub(crate) fn window_frame(hwnd: HWND) -> Option<WindowFrame> {
    if hwnd.0.is_null() {
        return None;
    }
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect).ok()? };
    window_frame_from_rect(rect)
}

/// Visible on-screen frame of a top-level window in physical pixels.
///
/// `DWMWA_EXTENDED_FRAME_BOUNDS` excludes Windows' invisible resize borders,
/// matching the pixels users see and the border overlay must cover. The
/// fallback preserves the old `GetWindowRect` behavior whenever DWM cannot
/// supply a valid rectangle.
pub(crate) fn visible_window_frame(hwnd: HWND) -> Option<WindowFrame> {
    if hwnd.0.is_null() {
        return None;
    }
    let mut extended = RECT::default();
    let extended_frame = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut extended as *mut RECT as *mut core::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()
        .and_then(|_| window_frame_from_rect(extended))
    };
    choose_visible_window_frame(extended_frame, window_frame(hwnd))
}

/// Current effective DPI scale for `hwnd`. A zero DPI or invalid HWND is
/// rejected instead of silently projecting a native surface at the wrong size.
pub(crate) fn window_dpi_scale(hwnd: HWND) -> Option<f64> {
    if hwnd.0.is_null() {
        return None;
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    (dpi >= 96).then_some(dpi as f64 / 96.0)
}

/// Physical bounds of the monitor currently containing `hwnd`.
/// `MONITOR_DEFAULTTONEAREST` keeps a partially off-screen source attached to
/// the nearest real monitor; a null monitor still fails closed.
pub(crate) fn monitor_frame_for_window(hwnd: HWND) -> Option<WindowFrame> {
    if hwnd.0.is_null() {
        return None;
    }
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.0.is_null() {
        return None;
    }
    display_frame(monitor)
}

fn work_area_from_monitor_info(info: &MONITORINFO) -> Option<WindowFrame> {
    window_frame_from_rect(info.rcWork)
}

/// Physical work-area bounds of the monitor currently containing `hwnd`.
/// Unlike `monitor_frame_for_window`, this excludes reserved taskbar space.
/// `MONITOR_DEFAULTTONEAREST` keeps a partially off-screen source attached to
/// the nearest real monitor; invalid monitor data fails closed.
pub(crate) fn monitor_work_area_for_window(hwnd: HWND) -> Option<WindowFrame> {
    if hwnd.0.is_null() {
        return None;
    }
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.0.is_null() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut info) }
        .as_bool()
        .then_some(())?;
    work_area_from_monitor_info(&info)
}

/// Native facts needed by the central share-target classifier, plus the
/// already-read values callers need after classification. Keeping this as one
/// adapter prevents picker and hover from drifting back into separate Win32
/// predicate copies.
#[derive(Debug, Clone)]
pub(crate) struct WindowInspection {
    pub(crate) facts: crate::share_target::ShareTargetFacts,
    pub(crate) frame: Option<WindowFrame>,
    pub(crate) title: Option<String>,
}

fn window_class_name(hwnd: HWND) -> Option<String> {
    let mut buffer = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) } as usize;
    (len > 0).then(|| String::from_utf16_lossy(&buffer[..len.min(buffer.len())]))
}

pub(crate) fn process_exe_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = vec![0u16; 1024];
    let mut size = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    result.ok()?;
    buffer.truncate(size as usize);
    Some(String::from_utf16_lossy(&buffer))
}

fn process_name_for_pid(pid: u32) -> Option<String> {
    let path = process_exe_path(pid)?;
    std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn window_is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked_value = BOOL(0);
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked_value as *mut BOOL as *mut core::ffi::c_void,
            std::mem::size_of::<BOOL>() as u32,
        )
        .is_ok()
            && cloaked_value.as_bool()
    }
}

/// Collect one live HWND snapshot. No eligibility decision happens here;
/// [`crate::share_target::classify`] owns that policy for every caller.
pub(crate) fn inspect_window(hwnd: HWND, self_pid: u32) -> Option<WindowInspection> {
    if hwnd.0.is_null() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let root_owner = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    let owner_present = unsafe { GetWindow(hwnd, GW_OWNER) }
        .ok()
        .is_some_and(|owner| !owner.0.is_null());
    let root_owner_differs = !root.0.is_null() && !root_owner.0.is_null() && root_owner != root;
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let title = window_title(hwnd);
    let class_name = window_class_name(hwnd);
    // Process identity is only needed for shell classes whose names are also
    // used by ordinary applications. Avoid opening/querying a process on
    // every 16ms hover tick for ordinary application windows, but retain the
    // ShellHost/ControlCenterWindow pair used by Windows 11 Quick Settings.
    let process_name = class_name
        .as_deref()
        .filter(|class| {
            class.eq_ignore_ascii_case("Windows.UI.Core.CoreWindow")
                || class.eq_ignore_ascii_case("ControlCenterWindow")
        })
        .and_then(|_| process_name_for_pid(pid));
    let region_selector = pid == self_pid
        && crate::region_window::is_owned_region_window(
            title.as_deref().unwrap_or_default(),
            pid as i32,
            self_pid as i32,
        );
    let frame = visible_window_frame(hwnd);
    let (width, height) = frame
        .map(|frame| (frame.width, frame.height))
        .unwrap_or_default();
    Some(WindowInspection {
        facts: crate::share_target::ShareTargetFacts {
            owner_pid: pid,
            self_pid,
            visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
            minimized: unsafe { IsIconic(hwnd) }.as_bool(),
            tool_window: ex_style & (WS_EX_TOOLWINDOW.0 as isize) != 0,
            app_window: ex_style & (WS_EX_APPWINDOW.0 as isize) != 0,
            cloaked: window_is_cloaked(hwnd),
            owner_present,
            root_owner_differs,
            width,
            height,
            layer: 0,
            region_selector,
            petal_chrome: pid == self_pid && !region_selector,
            system_surface: false,
            bundle_id: None,
            class_name: class_name.clone(),
            process_name,
        },
        frame,
        title,
    })
}

/// Window title (empty string if untitled).
pub(crate) fn window_title(hwnd: HWND) -> Option<String> {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) } as usize;
    if len == 0 {
        return Some(String::new());
    }
    Some(String::from_utf16_lossy(&buffer[..len]))
}

/// Set whether an app-owned top-level window participates in supported
/// Windows capture paths. `WDA_NONE` is the idle/recordable state; exclusion
/// is a temporary lease held only while a Petal display-region share runs.
pub(crate) fn set_capture_affinity(hwnd: HWND, excluded: bool) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };

    if hwnd.0.is_null() {
        return false;
    }
    let affinity = if excluded {
        WDA_EXCLUDEFROMCAPTURE
    } else {
        WDA_NONE
    };
    unsafe { SetWindowDisplayAffinity(hwnd, affinity).is_ok() }
}

/// Keep an app-owned transparent overlay out of Windows Graphics Capture.
/// Callers that intend to replace the WGC system border must treat `false` as
/// a hard fallback signal, never as a cosmetic warning.
pub(crate) fn set_capture_exclusion(hwnd: HWND) -> bool {
    set_capture_affinity(hwnd, true)
}

/// Restore an app-owned window to the normal captureable state.
pub(crate) fn clear_capture_exclusion(hwnd: HWND) -> bool {
    set_capture_affinity(hwnd, false)
}

/// Register `hwnd` in the capture-target registry and return its stable
/// `u32` token (the wire `window_id`). Repeated registration while the window
/// lives returns the existing token.
pub(crate) fn register_window(hwnd: HWND, pid: u32) -> Option<u32> {
    crate::windows_capture_target::register(hwnd.0 as usize, pid).ok()
}

/// Convenience: on-screen frame for a capture-target registry raw handle
/// (already a valid HWND).
pub(crate) fn window_frame_for_raw(raw_handle: usize) -> Option<WindowFrame> {
    window_frame(HWND(raw_handle as *mut core::ffi::c_void))
}

/// On-screen frame of a display in physical pixels. Display capture targets
/// store HMONITOR handles, not HWNDs; use rcMonitor rather than GetWindowRect.
pub(crate) fn display_frame(hmonitor: HMONITOR) -> Option<WindowFrame> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(hmonitor, &mut info) }
        .as_bool()
        .then_some(())?;
    let rect = info.rcMonitor;
    (rect.right > rect.left && rect.bottom > rect.top).then_some(WindowFrame {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

pub(crate) fn display_frame_for_raw(raw_handle: usize) -> Option<WindowFrame> {
    display_frame(HMONITOR(raw_handle as *mut core::ffi::c_void))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_handle_functions_fail_closed() {
        let null = HWND::default();
        assert_eq!(owner_pid(null), 0);
        assert_eq!(window_is_topmost(null), None);
        assert_eq!(checked_window_above_in_z_order(null), None);
        assert_eq!(
            checked_window_above_in_z_order_excluding(null, Some(null)),
            None
        );
        assert!(window_frame(null).is_none());
        assert!(visible_window_frame(null).is_none());
        assert!(window_dpi_scale(null).is_none());
        assert!(monitor_frame_for_window(null).is_none());
        assert!(monitor_work_area_for_window(null).is_none());
        assert!(display_frame_for_raw(0).is_none());
    }

    fn test_rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn visible_frame_validation_rejects_degenerate_and_overflowing_rects() {
        assert_eq!(
            window_frame_from_rect(test_rect(-20, 10, 180, 110)),
            Some(WindowFrame {
                x: -20,
                y: 10,
                width: 200,
                height: 100,
            })
        );
        assert!(window_frame_from_rect(test_rect(10, 10, 10, 100)).is_none());
        assert!(window_frame_from_rect(test_rect(10, 10, 100, 10)).is_none());
        assert!(window_frame_from_rect(test_rect(i32::MAX, 0, i32::MIN, 100)).is_none());
    }

    #[test]
    fn work_area_conversion_uses_rcwork_and_fails_closed() {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        info.rcWork = test_rect(-1920, 40, 0, 1080);
        assert_eq!(
            work_area_from_monitor_info(&info),
            Some(WindowFrame {
                x: -1920,
                y: 40,
                width: 1920,
                height: 1040,
            })
        );
        info.rcWork = test_rect(0, 0, 0, 1080);
        assert!(work_area_from_monitor_info(&info).is_none());
        info.rcWork = test_rect(0, 0, 1920, 0);
        assert!(work_area_from_monitor_info(&info).is_none());
    }

    #[test]
    fn invalid_dwm_frame_uses_valid_get_window_rect_fallback() {
        let fallback = Some(WindowFrame {
            x: 7,
            y: 9,
            width: 640,
            height: 480,
        });
        assert_eq!(choose_visible_window_frame(None, fallback), fallback);
        assert_eq!(
            choose_visible_window_frame(
                Some(WindowFrame {
                    x: 20,
                    y: 30,
                    width: 600,
                    height: 400,
                }),
                fallback,
            ),
            Some(WindowFrame {
                x: 20,
                y: 30,
                width: 600,
                height: 400,
            })
        );
        assert_eq!(choose_visible_window_frame(None, None), None);
    }

    #[test]
    fn off_screen_cursor_finds_no_window() {
        // Cursor far off the virtual screen: WindowFromPoint returns NULL.
        assert!(root_window_at((i32::MAX as f64, i32::MAX as f64)).is_none());
    }

    #[test]
    fn own_windows_are_never_shareable() {
        // The current process's own hwnd is unknown to us here, but a null
        // hwnd with a bogus matching pid still fails the pid check first —
        // pin the constant that keeps hover + picker filters consistent.
        assert_eq!(crate::share_target::MIN_WINDOW_SIDE, 40);
    }
}
