import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const routeSource = readFileSync(
  fileURLToPath(new URL('../src/routes/share-border/+page.svelte', import.meta.url)),
  'utf8'
);
const tokensSource = readFileSync(
  fileURLToPath(new URL('../../../shared/ui/tokens.css', import.meta.url)),
  'utf8'
);
const rustSource = readFileSync(
  fileURLToPath(new URL('../src-tauri/src/share_border.rs', import.meta.url)),
  'utf8'
);

test('share border route draws a complete rounded SVG path without a legacy top-tab gap', () => {
  assert.match(routeSource, /function shareBorderPath/);
  assert.match(routeSource, /const r = Math\.max\(0, radius - s \/ 2\);/);
  assert.doesNotMatch(routeSource, /tabAnchorX|DEFAULT_TAB_ANCHOR_X|anchorX/);
  assert.match(routeSource, /<svg[\s\S]*class="share-border"[\s\S]*>/);
  assert.match(routeSource, /<path[\s\S]*class="share-border-path"[\s\S]*>/);
  assert.match(routeSource, /getTotalLength\(\)/);
  assert.match(routeSource, /new ResizeObserver/);
  assert.doesNotMatch(routeSource, /<div class="share-border"><\/div>/);
  assert.doesNotMatch(routeSource, /border:\s*var\(--share-border-stroke/);
});

test('share border reveal can run on mount or replay from native eval', () => {
  assert.match(routeSource, /const SHARE_BORDER_REVEAL_EVENT = 'petal-share-border-reveal';/);
  assert.match(routeSource, /const shouldAnimate = page\.url\.searchParams\.get\('animate'\) === '1';/);
  assert.match(routeSource, /window\.addEventListener\(SHARE_BORDER_REVEAL_EVENT, reveal\);/);
  assert.match(routeSource, /\{#key revealKey\}/);
  assert.match(routeSource, /data-reveal=\{revealState\}/);
  assert.match(
    routeSource,
    /animation: share-border-sweep var\(--share-border-sweep-duration, 420ms\)[\s\S]*var\(--ease-standard/
  );
});

test('share border sweep respects reduced motion tokens and local override', () => {
  assert.match(tokensSource, /--share-border-sweep-duration:\s*420ms;/);
  assert.match(
    tokensSource,
    /@media \(prefers-reduced-motion: reduce\)[\s\S]*--share-border-sweep-duration:\s*0ms;/
  );
  assert.match(
    routeSource,
    /@media \(prefers-reduced-motion: reduce\)[\s\S]*animation: none;[\s\S]*stroke-dashoffset: 0;/
  );
});

test('native share border threads ShowKind into URL and eval reveal routing', () => {
  assert.match(
    rustSource,
    /fn realize_share_border\(app_main: &AppHandle, border_id: u32, show_kind: ShowKind\)/
  );
  assert.match(rustSource, /realize_share_border\(&app_main, border_id, show_kind\)/);
  assert.match(rustSource, /fn share_border_url\(color: &str, frame: WindowFrame, animate: bool\)/);
  assert.match(rustSource, /fn should_reveal_via_url/);
  assert.match(rustSource, /fn should_reveal_via_eval/);
  assert.match(rustSource, /CustomEvent\(\{\:\?\}\)/);
});
