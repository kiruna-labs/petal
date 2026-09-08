// #37: a plugin frame's own `<meta>` CSP cannot stop the frame navigating
// ITSELF, so the desktop webview must refuse the navigation from outside. The
// app shipped `csp: null` (no policy at all) until this landed.
//
// The two fields belong together and neither is cosmetic:
//   - `frame-src 'none'` is the whole policy. Nothing else is restricted, so
//     it cannot break the app's own scripts, styles, or connections.
//   - Tauri would otherwise ADD `script-src 'self' 'nonce-…' 'sha256-…'` and a
//     `style-src` to whatever policy is configured (tauri::manager::set_csp).
//     A srcdoc document INHERITS its embedder's policy, so that injected
//     script-src would reach inside every plugin frame and block the inline
//     runtime and plugin module -- i.e. plugins would stop running on desktop
//     while every test that renders them in Chromium still passed.
// Behaviour inside a real WKWebView is covered by the Test Cockpit's
// PLUGIN-BOOT scenario (src-tauri/src/test_cockpit/plugin_boot.rs), which runs
// on the self-hosted Mac in nightly-loopback.yml and requires the built-in
// Reactions plugin's srcdoc frame to report ready under whatever policy the
// two fields below produce. It is a live gate, not something this file can run.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const config = JSON.parse(readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
const security = config.app.security;

test('the desktop webview forbids plugin frames from navigating themselves anywhere', () => {
  assert.equal(typeof security.csp, 'string', 'csp: null means no policy at all (#37)');
  assert.match(security.csp, /frame-src 'none'/);
  // Anything beyond frame-src would also be inherited by the plugin srcdoc
  // frames, so keep the policy to the one directive this is for.
  assert.deepEqual(
    security.csp
      .split(';')
      .map((d: string) => d.trim())
      .filter(Boolean),
    ["frame-src 'none'"],
  );
  assert.deepEqual(security.dangerousDisableAssetCspModification, ['script-src', 'style-src']);
});
