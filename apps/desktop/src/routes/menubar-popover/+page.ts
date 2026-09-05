// Dedicated webview route for the menubar pill's popover (see
// src-tauri/src/menubar.rs). No dynamic server data — prerender so
// adapter-static can emit a static build/menubar-popover/index.html for
// Tauri's WebviewUrl::App to point at. Same pattern as hover-tab/share-border.
export const ssr = false;
export const prerender = true;
