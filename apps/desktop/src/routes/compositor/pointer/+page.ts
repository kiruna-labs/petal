// Dedicated webview route for a remote compositor window's telepointer
// overlay (see src-tauri/src/compositor.rs's `create_pointer_overlay`). No
// dynamic server data — prerender so adapter-static can emit a static
// build/compositor/pointer.html for Tauri's WebviewUrl::App to point at,
// same pattern as hover-tab/share-border/menubar-popover.
export const ssr = false;
export const prerender = true;
