// Dedicated webview route for a remote compositor window's PANEL itself
// (see src-tauri/src/compositor.rs's `ensure_window`). The panel's real
// content is a native `AVSampleBufferDisplayLayer` sublayer added directly
// to the panel's NSView layer (native_display.rs) -- this webview exists
// only because Tauri/tauri_nspanel's `PanelBuilder` requires a webview URL
// to construct a window at all; it renders nothing (fully transparent,
// zero-size content) and sits BEHIND the native video layer. No dynamic
// server data — prerender so adapter-static can emit a static
// build/compositor/surface.html for Tauri's WebviewUrl::App to point at.
export const ssr = false;
export const prerender = true;
