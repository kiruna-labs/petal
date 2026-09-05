import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import test from 'node:test';

const __dirname = dirname(fileURLToPath(import.meta.url));
const read = (p: string) => readFileSync(resolve(__dirname, p), 'utf8');
// Assertions below are about CODE, not prose: these files deliberately explain
// the traps in comments, and a naive grep would match the explanation.
const stripComments = (src: string) =>
  src
    .split('\n')
    .filter((line) => !/^\s*(\/\/|\*|\/\*)/.test(line))
    .join('\n');

const tauriConfig = JSON.parse(read('../src-tauri/tauri.conf.json'));
const lib = read('../src-tauri/src/lib.rs');
const layout = read('../src/routes/+layout.svelte');
const deepLink = read('../src-tauri/src/deep_link.rs');
const mainWindowRs = read('../src-tauri/src/main_window.rs');

// #636: the main window used to be created visible, so the user watched an
// opaque WKWebView underlay with SQUARE corners (the 24px radius is CSS the
// page had not run yet) until hydration finished. These pin the launch
// contract that fixes it. None of this failed loudly when it broke, which is
// why it survived so long.

test('the main window is created invisible so its pre-paint state is never shown', () => {
  const mainWindow = tauriConfig.app.windows[0];
  assert.equal(mainWindow.visible, false);
  // The two properties that make the pre-paint state ugly rather than benign:
  // transparent means WKWebView's opaque underlay shows through until the page
  // paints, and decorations:false means macOS contributes no corner rounding.
  assert.equal(mainWindow.transparent, true);
  assert.equal(mainWindow.decorations, false);
});

test('setup() does not show the main window directly -- only the reveal gate does', () => {
  assert.doesNotMatch(
    lib,
    /show_and_activate_main_window\(&handle, "startup"\)/,
    'startup must go through reveal_main_window, not show the window before first paint'
  );
  assert.match(lib, /fn reveal_main_window/);
  assert.match(lib, /reveal_main_window\(&app, "frontend-ready"\)/);
});

test('a frontend that never reports first paint still gets a window', () => {
  // Gating the ONLY reveal on a frontend signal means any failure that stops
  // that signal leaves the user with no window at all. An unstyled window
  // beats an invisible app.
  assert.match(lib, /MAIN_WINDOW_REVEAL_FALLBACK/);
  assert.match(lib, /reveal_main_window\(&handle, "startup-fallback"\)/);
});

test('the reveal happens once, so later triggers cannot re-arm activation', () => {
  assert.match(lib, /MAIN_WINDOW_REVEALED\s*\.?\s*\n?\s*\.swap\(true/);
});

test('a repeat reveal is a no-op, not a show that steals focus', () => {
  // `frontend_ready` fires on EVERY webview mount, so a mid-session reload of
  // the main webview (deep link, updater relaunch of the view, any full
  // navigation) reaches reveal_main_window again. If that path re-ran
  // show+activate it would yank the window in front of whatever the user was
  // doing. The early return must therefore be bare.
  const body = lib.slice(lib.indexOf('fn reveal_main_window'));
  const earlyReturn = stripComments(body.slice(0, body.indexOf('log::info!')));
  assert.doesNotMatch(
    earlyReturn,
    /show_and_activate_main_window/,
    'the already-revealed branch must not show or activate the window'
  );
  // An explicit second launch DOES mean "foreground this now", so that caller
  // asks for it separately.
  assert.match(lib, /reveal_main_window\(app, "single-instance"\);\s*\n\s*show_and_activate_main_window\(app, "single-instance"\)/);
});

test('the ready signal does NOT wait on requestAnimationFrame', () => {
  // The circular dependency that nearly shipped: every window reporting here
  // is hidden at that moment (main is `visible: false`; overlays are hidden at
  // creation). WebKit throttles or suspends rAF for a hidden document, so
  // waiting for a "presented frame" before asking to be shown can never
  // resolve -- the window cannot paint until shown, and is not shown until it
  // paints. Every launch would have fallen through to the timeout.
  const fn = layout.slice(layout.indexOf('async function reportFrontendReady'));
  const body = stripComments(fn.slice(0, fn.indexOf('\n  }')));
  assert.doesNotMatch(
    body,
    /requestAnimationFrame/,
    'reportFrontendReady must not gate on rAF: hidden windows do not get animation frames'
  );
});

test('the main route is prerendered, so launch does not fall back to the SPA shell', () => {
  const pageTs = resolve(__dirname, '../src/routes/main/+page.ts');
  assert.ok(existsSync(pageTs), 'src/routes/main/+page.ts must exist');
  const source = readFileSync(pageTs, 'utf8');
  assert.match(source, /export const prerender = true/);
  // The window opens this exact URL; if the route stops emitting it, SvelteKit
  // silently serves index.html instead of failing.
  assert.equal(tauriConfig.app.windows[0].url, 'main.html');
});

test('the reveal safety net is armed on every platform, not just macOS', () => {
  // `visible: false` is platform-agnostic. A macOS-only fallback would leave
  // Windows with no window at all if the frontend never reports ready --
  // unrecoverable, since the only other reveal is a second launch.
  assert.match(lib, /fn arm_main_window_reveal_fallback/);
  assert.equal(
    (lib.match(/arm_main_window_reveal_fallback\(/g) ?? []).length,
    3,
    'expected the definition plus one call per platform setup()'
  );
});

test('transparency is re-applied at reveal, as the compositor does before show', () => {
  // The setup()-time treatment runs while the window is hidden and has only one
  // retry; if it did not stick, WKWebView's opaque underlay IS the black box.
  const fn = lib.slice(lib.indexOf('fn reveal_main_window'));
  const body = fn.slice(0, fn.indexOf('\n}'));
  assert.match(body, /webview_transparency::apply_or_retry/);
});

test('navigation paths do not show the main window behind the gate', () => {
  // Both poll for the window during a COLD launch (deep link at ~200ms), so a
  // bare show() there puts the unpainted window on screen -- the reported bug,
  // on the coldest path there is. The navigation itself triggers a mount whose
  // frontend_ready reveals it with content.
  assert.doesNotMatch(stripComments(deepLink), /window\.show\(\)/);
  assert.doesNotMatch(stripComments(mainWindowRs), /window\.show\(\)/);
});
