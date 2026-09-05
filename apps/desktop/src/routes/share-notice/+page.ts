// Dedicated webview route for the always-present, hidden top-center
// "<Name> is sharing a window" notice pill (#679; see
// src-tauri/src/share_notice.rs). No dynamic server data — prerender so
// adapter-static can emit a static build/share-notice/index.html for
// Tauri's WebviewUrl::App to point at (same reasoning as hover-tab/+page.ts).
export const ssr = false;
export const prerender = true;
