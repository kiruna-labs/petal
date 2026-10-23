// #406: transparent overlays and card panels must not carry a native window
// shadow (AppKit outlines their alpha, a black hairline on macOS 26). Card
// panels keep their CSS shadow instead, inside a transparent gutter that the
// native side sizes and the page pads by. This file keeps the gutter
// extents, tokens.css, the Rust constants and the panel wiring in lockstep,
// and unit-tests the transparent-builder lint.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  SHADOW_GUTTERS,
  shadowGutterFromSearch,
  shadowGutterPadding,
  type ShadowGutter,
} from '../src/lib/shadowGutter.ts';
import { findViolations } from '../scripts/check-transparent-shadow.mjs';

const read = (rel: string) => readFileSync(new URL(rel, import.meta.url), 'utf8');
const tokens = read('../../../shared/ui/tokens.css');
const gutterRs = read('../src-tauri/src/shadow_gutter.rs');

function shadowExtent(token: string): ShadowGutter {
  const match = tokens.match(new RegExp(`--${token}:\\s*0 (\\d+)px (\\d+)px (-?\\d+)px`));
  assert.ok(match, `tokens.css must define --${token} as "0 <y> <blur> <spread>"`);
  const [, y, blur, spread] = match.map(Number);
  const side = blur + spread;
  return { x: side, top: Math.max(0, side - y), bottom: side + y };
}

function rustGutter(name: string): ShadowGutter {
  const match = gutterRs.match(
    new RegExp(`pub const ${name}: ShadowGutter = ShadowGutter \\{\\s*x: ([\\d.]+),\\s*top: ([\\d.]+),\\s*bottom: ([\\d.]+),`),
  );
  assert.ok(match, `shadow_gutter.rs must define ${name}`);
  return { x: Number(match[1]), top: Number(match[2]), bottom: Number(match[3]) };
}

test('gutters cover the CSS shadow extents and match the Rust constants', () => {
  assert.deepEqual(SHADOW_GUTTERS.float, shadowExtent('shadow-float'));
  assert.deepEqual(SHADOW_GUTTERS.panel, shadowExtent('shadow-panel'));
  assert.deepEqual(rustGutter('FLOAT'), SHADOW_GUTTERS.float);
  assert.deepEqual(rustGutter('PANEL'), SHADOW_GUTTERS.panel);
});

test('the page pads only when the native side sized a gutter', () => {
  assert.equal(shadowGutterFromSearch(''), null);
  assert.equal(shadowGutterFromSearch('?windowId=3'), null);
  assert.equal(shadowGutterFromSearch('?shadowGutter=bogus'), null);
  assert.deepEqual(shadowGutterFromSearch('?windowId=3&shadowGutter=float'), SHADOW_GUTTERS.float);
  assert.equal(shadowGutterPadding(null), '');
  assert.equal(shadowGutterPadding(SHADOW_GUTTERS.float), 'padding: 8px 26px 44px;');
});

const cardPanels = [
  { rs: '../src-tauri/src/control_consent.rs', route: '../src/routes/control-consent/+page.svelte' },
  { rs: '../src-tauri/src/share_notice.rs', route: '../src/routes/share-notice/+page.svelte' },
  { rs: '../src-tauri/src/ai_chat/panel.rs', route: '../src/routes/ai-chat-panel/+page.svelte' },
  { rs: '../src-tauri/src/menubar.rs', route: '../src/routes/menubar-popover/+page.svelte' },
  {
    rs: '../src-tauri/src/compositor.rs',
    route: '../src/routes/compositor/ai-chat/+page.svelte',
    from: 'const AI_CHAT_OVERLAY_SHADOW_GUTTER',
    to: 'failed to create ai-chat overlay',
  },
] as { rs: string; route: string; from?: string; to?: string }[];

test('every macOS card panel: native shadow off, gutter sized, gutter click-through, page padded', () => {
  for (const { rs, route, from, to } of cardPanels) {
    const whole = read(rs);
    // compositor.rs also builds the remote window panel itself, a real window
    // that keeps its shadow; check only the ai-chat overlay builder there.
    const source = from && to ? whole.slice(whole.indexOf(from), whole.indexOf(to)) : whole;
    assert.ok(source.length > 0, `${rs}: builder not found`);
    assert.doesNotMatch(source, /has_shadow\(true\)/, `${rs} must not turn the native shadow on`);
    assert.match(source, /has_shadow\(false\)/, `${rs} must turn the native shadow off`);
    assert.match(source, /shadow_gutter::route_with_gutter\(/, `${rs} must tell the page its gutter`);
    assert.match(source, /shadow_gutter::track\(/, `${rs} must make its gutter click-through`);
    assert.match(source, /_SHADOW_GUTTER\.window_size\(/, `${rs} must size the window by the gutter`);
    const page = read(route);
    assert.match(page, /shadowGutterPadding\(shadowGutterFromSearch\(\)\)/, `${route} must pad its card`);
    assert.match(page, /<div class="shadow-gutter" style=\{gutterPadding\}>/, `${route} must wrap its card`);
  }
});

test('the control and pointer overlays are built without a native shadow', () => {
  const compositor = read('../src-tauri/src/compositor.rs');
  const builder = compositor.slice(
    compositor.indexOf('fn create_chrome_webview('),
    compositor.indexOf('.focused(false);', compositor.indexOf('fn create_chrome_webview(')),
  );
  assert.match(builder, /\.transparent\(true\)[\s\S]*\.shadow\(false\)/);
});

test('lint flags a transparent builder with no explicit shadow setting', () => {
  const bare = `fn a() {\n    let w = WebviewWindowBuilder::new(app, "x", url)\n        .decorations(false)\n        .transparent(true)\n        .build();\n}\n`;
  assert.deepEqual(
    findViolations(bare).map((v: { line: number }) => v.line),
    [4],
  );
  assert.deepEqual(findViolations(bare.replace('.build()', '.shadow(false)\n        .build()')), []);
  assert.deepEqual(findViolations(bare.replace('.build()', '.shadow(true)\n        .build()')), []);
  assert.deepEqual(
    findViolations(bare.replace('.build()', '// shadow-lint: allow opaque content fills the window\n        .build()')),
    [],
  );
  // A bare `// shadow-lint: allow` with no reason does not count.
  assert.equal(findViolations(bare.replace('.build()', '// shadow-lint: allow\n        .build()')).length, 1);
});

test('lint reads a panel builder chain, including its with_window closure, as one statement', () => {
  const panel = `fn p() {\n    match PanelBuilder::new(app, LABEL)\n        .has_shadow(false)\n        .transparent(true)\n        .with_window(|w| w.decorations(false).transparent(true))\n        .build()\n    {\n        Ok(_) => {}\n        Err(_) => {}\n    }\n}\n`;
  assert.deepEqual(findViolations(panel), []);
  // A shadow call in a DIFFERENT statement does not cover this builder, and
  // a commented-out one does not count.
  const elsewhere = `fn q() {\n    let a = other().shadow(false);\n    let b = Builder::new()\n        // .shadow(false)\n        .transparent(true)\n        .build();\n}\n`;
  assert.equal(findViolations(elsewhere).length, 1);
});
