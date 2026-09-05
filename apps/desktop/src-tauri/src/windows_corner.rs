//! Native DWM corner radius for Petal's rectangular windows on Windows.
//!
//! Tauri's `transparent: true` (tauri.conf.json, shared with macOS) is
//! implemented on Windows as `DwmEnableBlurBehindWindow` with an empty region
//! (tao 0.35.3) — frosted-glass transparency, which DWM will not round.
//! Gallery mode therefore runs the main window as a real opaque window with
//! `DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND` so Windows 11 draws the
//! native corners; the collapsed meeting pill still needs the transparent
//! window around the capsule, so `set_main_pill_mode` toggles between the two
//! states. Windows 10 ignores the corner attribute (square corners) — that is
//! native for Windows 10.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmEnableBlurBehindWindow, DwmSetWindowAttribute, DWM_BLURBEHIND,
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    DWMWCP_ROUND, DWM_BB_BLURREGION, DWM_BB_ENABLE,
};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject};

/// `round = true`: opaque window with DWM-drawn native corners (gallery mode).
/// `round = false`: the tao-style transparent blur-behind window (pill mode).
pub(crate) fn set_window_native_mode(hwnd: HWND, round: bool) -> Result<(), String> {
    unsafe {
        if round {
            // `DWM_BB_ENABLE` must be set even when disabling: it marks the
            // `fEnable` member as initialized, and DWM rejects the struct
            // with E_INVALIDARG when dwFlags is 0.
            let blur = DWM_BLURBEHIND {
                dwFlags: DWM_BB_ENABLE,
                fEnable: false.into(),
                hRgnBlur: Default::default(),
                fTransitionOnMaximized: false.into(),
            };
            DwmEnableBlurBehindWindow(hwnd, &blur)
                .map_err(|e| format!("DwmEnableBlurBehindWindow(off): {e}"))?;
            let pref: u32 = DWMWCP_ROUND.0 as u32;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const u32 as *const core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
            .map_err(|e| format!("DwmSetWindowAttribute(round): {e}"))?;
        } else {
            // Same empty-region blur-behind tao applies for `transparent(true)`
            // (tao-0.35.3 platform_impl/windows/window.rs, "making the window
            // transparent").
            let region = CreateRectRgn(0, 0, -1, -1);
            let blur = DWM_BLURBEHIND {
                dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
                fEnable: true.into(),
                hRgnBlur: region,
                fTransitionOnMaximized: false.into(),
            };
            let result = DwmEnableBlurBehindWindow(hwnd, &blur);
            let _ = DeleteObject(region.into());
            result.map_err(|e| format!("DwmEnableBlurBehindWindow(on): {e}"))?;
            let pref: u32 = DWMWCP_DONOTROUND.0 as u32;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const u32 as *const core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
            .map_err(|e| format!("DwmSetWindowAttribute(donotround): {e}"))?;
        }
    }
    Ok(())
}

/// Flip a just-built rectangular webview window to opaque with DWM-native
/// rounded corners (gallery mode). Idempotent; failure is logged and the
/// window keeps its old transparent+square look.
pub(crate) fn make_native_rounded(window: &tauri::WebviewWindow) {
    let result = window
        .hwnd()
        .map_err(|e| format!("no hwnd: {e}"))
        .and_then(|hwnd| set_window_native_mode(hwnd, true));
    match result {
        Ok(()) => log::info!("petal: native corner radius applied to '{}'", window.label()),
        Err(e) => log::warn!("petal: failed to apply native corner radius to '{}': {e}", window.label()),
    }
}

/// Wire contract: `{ active: boolean }`, returns unit. `active = true` is pill
/// mode (transparent window); `active = false` restores the opaque
/// native-rounded gallery window. Registered ONLY in the Windows
/// invoke_handler; the macOS frontend never calls it (isWindows() gate).
#[tauri::command]
pub fn set_main_pill_mode(app: tauri::AppHandle, active: bool) -> Result<(), String> {
    use tauri::Manager;
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "set_main_pill_mode: main window not found".to_string())?;
    let hwnd = window.hwnd().map_err(|e| format!("set_main_pill_mode: no hwnd: {e}"))?;
    set_window_native_mode(hwnd, !active)
}
