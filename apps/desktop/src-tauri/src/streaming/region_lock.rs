//! Petal View while live: when the stream is bound to a Petal View region,
//! the region is snapped to the canvas shape (16:9, or 9:16 in portrait) and
//! keeps that shape while the sharer resizes it, so the layout inside always
//! fills the fixed canvas with no bars. Released when the stream stops or
//! moves to another share.
//!
//! Windows holds the shape in `WM_SIZING`; macOS sets the `NSWindow`'s own
//! aspect ratio, which AppKit's live resize keeps. The geometry below is pure
//! and platform-neutral. All rects are `(left, top, right, bottom)`: physical
//! pixels on Windows, AppKit points on macOS (bottom-up, which the geometry
//! doesn't mind).
// `constrain_sizing` and the `WMSZ_*` edges serve `WM_SIZING`, so outside
// Windows only the tests reach them.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// `WM_SIZING` edges (`WMSZ_*`).
const LEFT: u32 = 1;
const RIGHT: u32 = 2;
const TOP: u32 = 3;
const TOP_LEFT: u32 = 4;
const TOP_RIGHT: u32 = 5;
const BOTTOM: u32 = 6;
const BOTTOM_LEFT: u32 = 7;

type Rect = (i32, i32, i32, i32);

/// The region at `aspect` (width:height): same centre and about the same
/// area, shrunk if needed to fit inside `work` (the display's work area).
pub(crate) fn snap_to_aspect(rect: Rect, aspect: (u32, u32), work: Rect) -> Rect {
    let (w, h) = (f64::from(rect.2 - rect.0), f64::from(rect.3 - rect.1));
    let (work_w, work_h) = (f64::from(work.2 - work.0), f64::from(work.3 - work.1));
    if w <= 0.0 || h <= 0.0 || work_w <= 0.0 || work_h <= 0.0 || aspect.0 == 0 || aspect.1 == 0 {
        return rect;
    }
    let ratio = f64::from(aspect.0) / f64::from(aspect.1);
    let mut new_w = (w * h * ratio).sqrt();
    let mut new_h = new_w / ratio;
    let fit = (work_w / new_w).min(work_h / new_h).min(1.0);
    new_w *= fit;
    new_h *= fit;
    let (new_w, new_h) = (new_w.round() as i32, new_h.round() as i32);
    let centre = (rect.0 + (rect.2 - rect.0) / 2, rect.1 + (rect.3 - rect.1) / 2);
    let x = (centre.0 - new_w / 2).clamp(work.0, (work.2 - new_w).max(work.0));
    let y = (centre.1 - new_h / 2).clamp(work.1, (work.3 - new_h).max(work.1));
    (x, y, x + new_w, y + new_h)
}

/// A resize drag's proposed rect, held to `aspect`: a side edge drives the
/// other dimension, a corner is width-driven, and the edge opposite the one
/// being dragged stays put. The short side never drops below `min_short`.
pub(crate) fn constrain_sizing(edge: u32, rect: Rect, aspect: (u32, u32), min_short: i32) -> Rect {
    if aspect.0 == 0 || aspect.1 == 0 {
        return rect;
    }
    let (aw, ah) = (i64::from(aspect.0), i64::from(aspect.1));
    let (mut w, mut h) = (i64::from(rect.2 - rect.0), i64::from(rect.3 - rect.1));
    if matches!(edge, TOP | BOTTOM) {
        w = h * aw / ah;
    } else {
        h = w * ah / aw;
    }
    let min_short = i64::from(min_short.max(1));
    if aw >= ah && h < min_short {
        h = min_short;
        w = h * aw / ah;
    } else if aw < ah && w < min_short {
        w = min_short;
        h = w * ah / aw;
    }
    let (w, h) = (w as i32, h as i32);
    let (left, right) = if matches!(edge, LEFT | TOP_LEFT | BOTTOM_LEFT) {
        (rect.2 - w, rect.2)
    } else {
        (rect.0, rect.0 + w)
    };
    let (top, bottom) = if matches!(edge, TOP | TOP_LEFT | TOP_RIGHT) {
        (rect.3 - h, rect.3)
    } else {
        (rect.1, rect.1 + h)
    };
    (left, top, right, bottom)
}

/// The smallest aspect ratio equal to `canvas` (1920x1080 -> 16:9).
fn reduced(canvas: (u32, u32)) -> (u32, u32) {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }
    let divisor = gcd(canvas.0, canvas.1).max(1);
    (canvas.0 / divisor, canvas.1 / divisor)
}

#[cfg(target_os = "windows")]
static LOCKED: std::sync::Mutex<Option<(isize, String)>> = std::sync::Mutex::new(None);
/// The held selector's label.
#[cfg(target_os = "macos")]
static LOCKED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Follow the stream's binding: `Some((window_id, canvas))` locks that share
/// if it is a Petal View region (releasing any other); `None` releases.
pub fn bind(app: &tauri::AppHandle, binding: Option<(u32, (u32, u32))>) {
    #[cfg(target_os = "windows")]
    {
        use crate::sync_ext::MutexExt;
        let target = binding.and_then(|(window_id, canvas)| selector(app, window_id).map(|s| (s, canvas)));
        let mut locked = LOCKED.lock_unpoisoned();
        let target_hwnd = target.as_ref().map(|((_, hwnd), _)| *hwnd);
        if let Some((hwnd, label)) = locked.take() {
            if Some(hwnd) != target_hwnd {
                let _ = app.run_on_main_thread(move || native::unlock(hwnd));
                log::info!("streaming: Petal View '{label}' shape released");
            }
        }
        if let Some(((label, hwnd), canvas)) = target {
            let aspect = reduced(canvas);
            *locked = Some((hwnd, label.clone()));
            let _ = app.run_on_main_thread(move || match native::lock(hwnd, aspect) {
                Some((before, after)) => log::info!(
                    "streaming: Petal View '{label}' held at {}:{} ({}x{} -> {}x{})",
                    aspect.0,
                    aspect.1,
                    before.2 - before.0,
                    before.3 - before.1,
                    after.2 - after.0,
                    after.3 - after.1
                ),
                None => log::warn!("streaming: couldn't hold Petal View '{label}' at {}:{}", aspect.0, aspect.1),
            });
        }
    }
    #[cfg(target_os = "macos")]
    {
        use crate::sync_ext::MutexExt;
        let target = binding.and_then(|(window_id, canvas)| selector_label(window_id).map(|label| (label, canvas)));
        let mut locked = LOCKED.lock_unpoisoned();
        if let Some(label) = locked.take() {
            if target.as_ref().map(|(target, _)| target) != Some(&label) {
                let main_app = app.clone();
                let _ = app.run_on_main_thread(move || {
                    if let Some(window) = tauri::Manager::get_webview_window(&main_app, &label) {
                        native::unlock(&window);
                    }
                    log::info!("streaming: Petal View '{label}' shape released");
                });
            }
        }
        if let Some((label, canvas)) = target {
            let aspect = reduced(canvas);
            *locked = Some(label.clone());
            let main_app = app.clone();
            let _ = app.run_on_main_thread(move || {
                let held = tauri::Manager::get_webview_window(&main_app, &label)
                    .and_then(|window| native::lock(&window, aspect));
                match held {
                    Some((before, after)) => log::info!(
                        "streaming: Petal View '{label}' held at {}:{} ({}x{} -> {}x{} pt)",
                        aspect.0,
                        aspect.1,
                        before.2 - before.0,
                        before.3 - before.1,
                        after.2 - after.0,
                        after.3 - after.1
                    ),
                    None => log::warn!("streaming: couldn't hold Petal View '{label}' at {}:{}", aspect.0, aspect.1),
                }
            });
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = (app, binding);
}

/// The Petal View selector behind a share, if it is one: its label.
#[cfg(target_os = "macos")]
fn selector_label(window_id: u32) -> Option<String> {
    let source = crate::region_window::resolve(window_id)?;
    crate::region_window::selector_label_from_title(&source.title)
}

#[cfg(target_os = "macos")]
mod native {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    use super::Rect;

    fn to_rect(frame: NSRect) -> Rect {
        let (x, y) = (frame.origin.x.round() as i32, frame.origin.y.round() as i32);
        (x, y, x + frame.size.width.round() as i32, y + frame.size.height.round() as i32)
    }

    fn to_frame(rect: Rect) -> NSRect {
        NSRect::new(
            NSPoint::new(f64::from(rect.0), f64::from(rect.1)),
            NSSize::new(f64::from(rect.2 - rect.0), f64::from(rect.3 - rect.1)),
        )
    }

    /// On the main thread: snap to `aspect` inside the screen's visible
    /// frame, then have AppKit hold it while the sharer resizes. Returns the
    /// frame before and after the snap.
    pub(super) fn lock(window: &tauri::WebviewWindow, aspect: (u32, u32)) -> Option<(Rect, Rect)> {
        let ns_window = window.ns_window().ok()?.cast::<AnyObject>();
        // SAFETY: Tauri's live `NSWindow*`, messaged on the main thread.
        unsafe {
            let screen: *mut AnyObject = msg_send![ns_window, screen];
            if screen.is_null() {
                return None;
            }
            let frame: NSRect = msg_send![ns_window, frame];
            let visible: NSRect = msg_send![screen, visibleFrame];
            let before = to_rect(frame);
            let after = super::snap_to_aspect(before, aspect, to_rect(visible));
            if after != before {
                let _: () = msg_send![ns_window, setFrame: to_frame(after), display: true];
            }
            let ratio = NSSize::new(f64::from(aspect.0), f64::from(aspect.1));
            let _: () = msg_send![ns_window, setAspectRatio: ratio];
            Some((before, after))
        }
    }

    /// Unit resize increments clear the aspect ratio (`NSWindow.aspectRatio`).
    pub(super) fn unlock(window: &tauri::WebviewWindow) {
        let Ok(ns_window) = window.ns_window() else { return };
        // SAFETY: as in `lock`.
        unsafe {
            let _: () = msg_send![ns_window.cast::<AnyObject>(), setResizeIncrements: NSSize::new(1.0, 1.0)];
        }
    }
}

/// The Petal View selector behind a share, if it is one: (label, HWND).
#[cfg(target_os = "windows")]
fn selector(app: &tauri::AppHandle, window_id: u32) -> Option<(String, isize)> {
    use tauri::Manager;
    let source = crate::region_window::resolve(window_id)?;
    let label = crate::region_window::selector_label_from_title(&source.title)?;
    let hwnd = app.get_webview_window(&label)?.hwnd().ok()?.0 as isize;
    Some((label, hwnd))
}

#[cfg(target_os = "windows")]
mod native {
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER, WM_NCDESTROY, WM_SIZING,
    };

    use super::Rect;

    const SUBCLASS_ID: usize = 0x5045_5441;
    /// Petal View's minimum height, logical pixels (`region_window.rs`).
    const MIN_SHORT_LOGICAL: u32 = 120;

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut core::ffi::c_void)
    }

    fn pack(aspect: (u32, u32)) -> usize {
        ((aspect.0 as usize & 0xffff) << 16) | (aspect.1 as usize & 0xffff)
    }

    fn unpack(data: usize) -> (u32, u32) {
        (((data >> 16) & 0xffff) as u32, (data & 0xffff) as u32)
    }

    unsafe extern "system" fn keep_aspect(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        id: usize,
        data: usize,
    ) -> LRESULT {
        match msg {
            WM_SIZING if lparam.0 != 0 => {
                // SAFETY: WM_SIZING's lParam is the drag's RECT, ours to edit.
                let rect = unsafe { &mut *(lparam.0 as *mut RECT) };
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
                let min_short = (MIN_SHORT_LOGICAL * dpi / 96) as i32;
                let (left, top, right, bottom) = super::constrain_sizing(
                    wparam.0 as u32,
                    (rect.left, rect.top, rect.right, rect.bottom),
                    unpack(data),
                    min_short,
                );
                *rect = RECT { left, top, right, bottom };
                return LRESULT(1);
            }
            WM_NCDESTROY => {
                let _ = unsafe { RemoveWindowSubclass(hwnd, Some(keep_aspect), id) };
            }
            _ => {}
        }
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }

    /// On the window's own thread: snap to `aspect`, then hold it while the
    /// sharer resizes. Returns the frame before and after the snap.
    pub(super) fn lock(raw: isize, aspect: (u32, u32)) -> Option<(Rect, Rect)> {
        let hwnd = hwnd(raw);
        let mut rect = RECT::default();
        // SAFETY: plain Win32 calls on a live top-level window we own.
        unsafe {
            GetWindowRect(hwnd, &mut rect).ok()?;
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
                return None;
            }
            let before = (rect.left, rect.top, rect.right, rect.bottom);
            let work = info.rcWork;
            let after = super::snap_to_aspect(before, aspect, (work.left, work.top, work.right, work.bottom));
            if after != before {
                let (w, h) = (after.2 - after.0, after.3 - after.1);
                SetWindowPos(hwnd, None, after.0, after.1, w, h, SWP_NOZORDER | SWP_NOACTIVATE).ok()?;
            }
            SetWindowSubclass(hwnd, Some(keep_aspect), SUBCLASS_ID, pack(aspect))
                .as_bool()
                .then_some((before, after))
        }
    }

    pub(super) fn unlock(raw: isize) {
        // SAFETY: removing our own subclass; a no-op if it isn't installed.
        let _ = unsafe { RemoveWindowSubclass(hwnd(raw), Some(keep_aspect), SUBCLASS_ID) };
    }

    /// A real (hidden) window: the snap moves it, and a resize drag, as the
    /// system sends it, comes back held to the shape. Opt-in: it creates a
    /// desktop window. `cargo test -- --ignored`.
    #[test]
    #[ignore = "creates a desktop window"]
    fn holds_a_real_window_to_the_canvas_shape() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, SendMessageW, WINDOW_EX_STYLE, WMSZ_RIGHT, WS_POPUP,
        };
        // SAFETY: a window this thread creates, uses and destroys.
        unsafe {
            let window =
                CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("lock"), WS_POPUP, 100, 100, 1000, 700, None, None, None, None)
                    .unwrap();
            let raw = window.0 as isize;
            let (_, after) = lock(raw, (16, 9)).expect("locked");
            let mut rect = RECT::default();
            GetWindowRect(window, &mut rect).unwrap();
            assert_eq!((rect.left, rect.top, rect.right, rect.bottom), after);
            let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
            assert!((w * 9 - h * 16).abs() <= 16, "snapped to 16:9: {w}x{h}");
            // Drag the right edge out to 1600 wide: the height follows.
            let mut drag = RECT { left: rect.left, top: rect.top, right: rect.left + 1600, bottom: rect.top + 300 };
            SendMessageW(window, WM_SIZING, Some(WPARAM(WMSZ_RIGHT as usize)), Some(LPARAM(&mut drag as *mut RECT as isize)));
            assert_eq!(drag.bottom - drag.top, 900, "{drag:?}");
            // Released: the system's rect is left alone.
            unlock(raw);
            let mut drag = RECT { left: 0, top: 0, right: 1600, bottom: 300 };
            SendMessageW(window, WM_SIZING, Some(WPARAM(WMSZ_RIGHT as usize)), Some(LPARAM(&mut drag as *mut RECT as isize)));
            assert_eq!(drag.bottom, 300);
            DestroyWindow(window).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(rect: Rect) -> (i32, i32) {
        (rect.2 - rect.0, rect.3 - rect.1)
    }

    fn is_16_9(rect: Rect) -> bool {
        let (w, h) = size(rect);
        (w * 9 - h * 16).abs() <= 16
    }

    #[test]
    fn snaps_to_the_canvas_shape_keeping_centre_and_area() {
        let work = (0, 0, 1920, 1040);
        let snapped = snap_to_aspect((100, 100, 1100, 800), (16, 9), work);
        assert!(is_16_9(snapped), "{snapped:?}");
        let (w, h) = size(snapped);
        assert!((w * h - 700_000).abs() < 2_000, "about the same area: {w}x{h}");
        assert_eq!(((snapped.0 + snapped.2) / 2, (snapped.1 + snapped.3) / 2), (600, 450));
    }

    #[test]
    fn a_snap_stays_on_the_display() {
        let work = (0, 0, 1920, 1040);
        // Bigger than the display allows at 16:9: shrunk to fit.
        let full = snap_to_aspect(work, (16, 9), work);
        assert!(is_16_9(full) && size(full).1 == 1040 && full.0 >= 0 && full.2 <= 1920, "{full:?}");
        // Near an edge: moved back inside.
        let corner = snap_to_aspect((1700, 900, 1900, 1030), (16, 9), work);
        assert!(corner.2 <= 1920 && corner.3 <= 1040, "{corner:?}");
        // Portrait canvas.
        let tall = snap_to_aspect((100, 100, 900, 600), (9, 16), work);
        let (w, h) = size(tall);
        assert!((w * 16 - h * 9).abs() <= 16, "{tall:?}");
    }

    #[test]
    fn resizing_holds_the_shape_from_the_edge_being_dragged() {
        let wide = (16, 9);
        // A side edge drives the other dimension; top-left stays put.
        assert_eq!(constrain_sizing(RIGHT, (0, 0, 1600, 500), wide, 120), (0, 0, 1600, 900));
        assert_eq!(constrain_sizing(BOTTOM, (0, 100, 1000, 1000), wide, 120), (0, 100, 1600, 1000));
        // Dragging the top-left corner: the bottom-right corner stays put.
        assert_eq!(constrain_sizing(TOP_LEFT, (0, 0, 1600, 500), wide, 120), (0, -400, 1600, 500));
        assert_eq!(constrain_sizing(LEFT, (300, 0, 1900, 500), wide, 120), (300, 0, 1900, 900));
        // Never smaller than the minimum.
        assert_eq!(constrain_sizing(RIGHT, (0, 0, 100, 50), wide, 120), (0, 0, 213, 120));
        // Portrait.
        assert_eq!(constrain_sizing(RIGHT, (0, 0, 900, 100), (9, 16), 120), (0, 0, 900, 1600));
    }

    #[test]
    fn canvas_sizes_reduce_to_their_shape() {
        assert_eq!(reduced((1920, 1080)), (16, 9));
        assert_eq!(reduced((1280, 720)), (16, 9));
        assert_eq!(reduced((3840, 2160)), (16, 9));
        assert_eq!(reduced((1080, 1920)), (9, 16));
    }
}
