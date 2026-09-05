// Dev-only telepointer harness route (see src-tauri/src/dev_telepointer.rs).
// Prerender so adapter-static actually emits build/dev/telepointer.html —
// without this the file was never emitted, so the dev-telepointer panel would
// 404 on a genuinely missing file even after the reroute hook maps
// /dev/telepointer.html → /dev/telepointer.
export const ssr = false;
export const prerender = true;
