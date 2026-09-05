// Dedicated webview route for the always-present, hidden sharer-side
// remote-control CONSENT prompt (src-tauri/src/control_consent.rs). No
// dynamic server data -- prerender so adapter-static emits a static
// build/control-consent/index.html for Tauri's WebviewUrl::App (same
// reasoning as share-notice/+page.ts).
export const ssr = false;
export const prerender = true;
