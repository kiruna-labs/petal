// Dedicated webview route for the main window (tauri.conf.json's
// `"url": "main.html"`). Prerender so adapter-static emits build/main.html at
// the exact URL the window opens.
//
// Without this the file did not exist at all: the window fell back to
// adapter-static's `index.html` and then had to run the `/main.html` → `/main`
// reroute in hooks.ts before it could even start rendering the right route.
// Every other native-window route already opts in (hover-tab, share-border,
// menubar-popover, network-cockpit, window-picker); main was the one that
// silently did not, because nothing FAILS when a prerendered page is missing —
// SvelteKit just serves the SPA fallback, so no gate ever caught it.
//
// Note what this does and does not buy. `ssr = false` is required app-wide
// (Tauri has no Node server), so the emitted HTML is an entry shell, NOT
// prerendered markup — the first pixel still waits on the JS bundle. This only
// removes the fallback-and-reroute hop. What actually stops the user seeing a
// black unstyled box is the reveal gate: the window is created
// `visible: false` and shown on the real first-paint signal (#636, see
// `lib.rs`'s `reveal_main_window`).
export const ssr = false;
export const prerender = true;
