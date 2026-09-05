// Dedicated webview route for the persistent colored share-border overlay
// (see src-tauri/src/share_border.rs). No dynamic server data — prerender so
// adapter-static can emit a static build/share-border/index.html for Tauri's
// WebviewUrl::App to point at. Each panel instance passes its color via a
// `?color=` query param, read client-side (no server rendering involved).
export const ssr = false;
export const prerender = true;
