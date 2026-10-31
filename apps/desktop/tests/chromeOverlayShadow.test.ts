import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// #406: on macOS 26 a transparent window that keeps tao's default shadow gets
// a black outline around its opaque content -- the status banner and the
// telepointers on every remote window. Both overlays are built by
// `create_chrome_webview`, so the shadow must be disabled there.

const compositor = readFileSync(
  new URL('../src-tauri/src/compositor.rs', import.meta.url),
  'utf8'
);

function chromeWebviewBuilder(): string {
  const start = compositor.indexOf('fn create_chrome_webview(');
  assert.notEqual(start, -1, 'could not locate create_chrome_webview');
  const end = compositor.indexOf('.build()', start);
  assert.notEqual(end, -1, 'could not locate the builder call in create_chrome_webview');
  return compositor.slice(start, end);
}

test('#406: the remote-window chrome overlays are transparent and shadowless', () => {
  const body = chromeWebviewBuilder();
  assert.match(body, /\.transparent\(true\)/);
  assert.equal((body.match(/\.shadow\(false\)/g) ?? []).length, 1);
  assert.doesNotMatch(body, /\.shadow\(true\)/);
});

test('#406: both overlays still route through create_chrome_webview', () => {
  for (const role of ['"control overlay"', '"pointer overlay"']) {
    assert.match(compositor, new RegExp(`create_chrome_webview\\([\\s\\S]{0,200}role: ${role}`));
  }
});
