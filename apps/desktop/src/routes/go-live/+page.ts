// Dedicated webview route for the go-live dialog
// (src-tauri/src/streaming/commands.rs opens `go-live.html`). Prerendered so
// adapter-static emits build/go-live.html at the exact URL the window opens.
export const prerender = true;
