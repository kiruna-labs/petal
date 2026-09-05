//! Shared WebView2 command-line arguments for Windows.
//!
//! WebView2 (Chromium/Edge) GPU compositing is on by default, but hosts with a
//! GPU-blocklisted adapter (VMs, older GPUs, some remote-desktop sessions) fall
//! back to SwiftShader software rendering. These switches force the GPU
//! compositing path on for every Petal window on Windows.
//!
//! NOTE: `--disable-software-rasterizer` is deliberately NOT included —
//! SwiftShader is kept as the safety net so that if GPU compositing still
//! fails at runtime (driver crash, exotic adapter) the window does not render
//! blank. The flags below request the GPU path; SwiftShader only engages when
//! that path is genuinely unavailable.
//!
//! IMPORTANT (wry 0.55.1): setting `additionalBrowserArgs` REPLACES wry's
//! default arguments (`--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection`),
//! so they are re-included below. The switches are applied at WebView2
//! environment creation (`CreateCoreWebView2EnvironmentWithOptions`) — they
//! cannot be changed for an already-created environment, which is why the
//! config-created `main` window gets them via `additionalBrowserArgs` in
//! `tauri.conf.json` instead of a builder call.
//!
//! This module is compiled only on Windows (`additional_browser_args` is
//! unsupported on macOS/Linux — see `tauri::WebviewWindowBuilder`).

pub(crate) const WEBVIEW2_ACCEL_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
--ignore-gpu-blocklist \
--enable-gpu \
--enable-gpu-compositing \
--enable-zero-copy \
--enable-accelerated-video-decode";
