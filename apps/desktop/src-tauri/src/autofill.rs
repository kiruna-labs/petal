//! Disable WebView2 autofill (browser-style "saved name/address" dropdowns and
//! password-save prompts) in a desktop app that has no business looking like a
//! browser. WebView2 has no per-field opt-out that sticks for every future
//! input, so kill it at the engine. In webview2-com 0.38 those switches live on
//! `ICoreWebView2Profile6` (profile-level `IsGeneralAutofillEnabled` /
//! `IsPasswordAutosaveEnabled`, which override the environment defaults and
//! persist for the profile), reached via the window's WebView2 controller.
//! Best-effort: a failure logs and never panics.
#![cfg(target_os = "windows")]

use tauri::Manager;
use windows_core::Interface;

pub(crate) fn disable_autofill(app: &tauri::AppHandle) {
    for (label, window) in app.webview_windows() {
        let label_owned = label.clone();
        // `with_webview` queues the closure on the main-thread event loop,
        // where it receives tauri's `PlatformWebview` (the wry WebView2
        // handle). That is the ONLY public path to the WebView2 controller in
        // tauri 2.11, and running there keeps the COM calls on the thread the
        // controller was created on. `controller()` is an inherent
        // `#[cfg(windows)]` method on `PlatformWebview`.
        let result = window.with_webview(move |webview| {
            let label = &label_owned;
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                let controller = webview.controller();
                let core = unsafe { controller.CoreWebView2()? };
                // `Profile()` is exposed on ICoreWebView2_13 in webview2-com
                // 0.38's bindings; the runtime object implements it.
                let core_13 = core
                    .cast::<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_13>()?;
                let profile = unsafe { core_13.Profile()? };
                let profile6 = profile.cast::<
                    webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Profile6,
                >()?;
                unsafe {
                    profile6.SetIsGeneralAutofillEnabled(false)?;
                    profile6.SetIsPasswordAutosaveEnabled(false)?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => log::info!("petal: autofill disabled on webview '{label}'"),
                Err(e) => log::warn!("petal: failed to disable autofill on webview '{label}': {e}"),
            }
        });
        if let Err(e) = result {
            log::warn!("petal: could not schedule autofill disable on webview '{label}': {e}");
        }
    }
}
