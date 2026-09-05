// Dedicated webview route for the floating hover "share" pill (see
// src-tauri/src/hover_tab.rs). No dynamic server data — prerender so
// adapter-static can emit a static build/hover-tab/index.html for Tauri's
// WebviewUrl::App to point at.
export const ssr = false;
export const prerender = true;
