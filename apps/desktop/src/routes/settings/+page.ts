// Dedicated webview route for the standalone Settings window
// (src-tauri/src/settings_window.rs opens `settings.html`). Prerender so
// adapter-static emits build/settings.html at the exact URL the window
// opens, same as every other native-window route (see main/+page.ts for why
// a missing prerendered page fails silently).
export const prerender = true;
