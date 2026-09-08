// Dev-only plugin-boot probe route (see src-tauri/src/test_cockpit/plugin_boot.rs).
// Prerender so adapter-static emits build/dev/plugin-boot.html — the Test
// Cockpit opens it as `WebviewUrl::App("dev/plugin-boot.html")`, which resolves
// against the embedded asset table, and an un-emitted file 404s there.
export const ssr = false;
export const prerender = true;
