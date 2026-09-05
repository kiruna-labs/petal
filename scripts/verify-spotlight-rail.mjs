#!/usr/bin/env node
/**
 * #676: prove -- by REAL RENDERED GEOMETRY, not a regex over the source --
 * that spotlight-view rail thumbnails never overlap and hold a true 16:9,
 * and that camera-off names/name-chips are scaled down for the rail rather
 * than rendering at the fixed base size meant for a full-size grid tile.
 *
 * Why this can't be a plain `apps/desktop/tests/*.test.ts` unit test: those
 * run under `node --test` with no DOM and no layout engine
 * (`apps/desktop/package.json`'s `test` script). `getBoundingClientRect`
 * there returns all zeros, so an overlap assertion would pass vacuously
 * whether or not the bug is present -- exactly the class of test that let
 * 0738c91f regress this silently in the first place (`grep -n spotlight
 * apps/desktop/tests/*.ts` returned nothing before this file existed).
 *
 * This drives real headless Chromium (the same Playwright tier as
 * scripts/verify-no-black-frame.mjs) against the REAL component CSS --
 * Gallery.svelte's and ParticipantTile.svelte's own `<style>` blocks, pulled
 * out verbatim and only stripped of Svelte's `:global(...)` wrapper syntax
 * (which is not valid CSS outside the Svelte compiler; the selectors
 * underneath are unchanged) -- plus the real design tokens, at two window
 * sizes: the app's actual default (400x640, where the stale
 * `@media (max-width: 620px)` override caused the overlap) and a wide
 * window (900x700, where unconstrained `grid-auto-columns: auto` tracks
 * over-stretched and left ~83px gaps).
 *
 * Usage: node scripts/verify-spotlight-rail.mjs
 */

import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const repoRoot = resolve(import.meta.dirname, '..');
const require = createRequire(import.meta.url);

let chromium;
try {
  const playwrightModule =
    process.env.PETAL_PLAYWRIGHT_MODULE ?? resolve(repoRoot, 'apps/desktop/node_modules/playwright');
  ({ chromium } = require(playwrightModule));
} catch (error) {
  console.error(
    `Requires Playwright. Install apps/desktop dependencies or set PETAL_PLAYWRIGHT_MODULE. ${
      error instanceof Error ? error.message : String(error)
    }`
  );
  process.exit(2);
}

/** Extracts a Svelte SFC's <style> block content verbatim. */
function extractStyle(source, label) {
  const match = source.match(/<style[^>]*>([\s\S]*?)<\/style>/);
  if (!match) throw new Error(`No <style> block found in ${label}`);
  return match[1];
}

/**
 * Strips Svelte's `:global(selector)` wrapper down to just `selector`.
 * Not a simple regex swap: some usages nest parens inside the wrapped
 * selector itself (e.g. `:global(.control-button.size-compact:not(:disabled))`),
 * so this walks the string tracking paren depth to find each `:global(`'s
 * real matching close-paren rather than the first `)` encountered.
 */
function stripGlobal(css) {
  let out = '';
  let i = 0;
  const marker = ':global(';
  for (;;) {
    const idx = css.indexOf(marker, i);
    if (idx === -1) {
      out += css.slice(i);
      break;
    }
    out += css.slice(i, idx);
    let depth = 1;
    let j = idx + marker.length;
    while (j < css.length && depth > 0) {
      if (css[j] === '(') depth++;
      else if (css[j] === ')') depth--;
      j++;
    }
    out += css.slice(idx + marker.length, j - 1);
    i = j;
  }
  return out;
}

const tokensCss = readFileSync(resolve(repoRoot, 'apps/desktop/src/styles/tokens.css'), 'utf8');
const gallerySource = readFileSync(
  resolve(repoRoot, 'apps/desktop/src/lib/components/Gallery.svelte'),
  'utf8'
);
const participantTileSource = readFileSync(
  resolve(repoRoot, 'apps/desktop/src/lib/components/ParticipantTile.svelte'),
  'utf8'
);
const galleryCss = stripGlobal(extractStyle(gallerySource, 'Gallery.svelte'));
const participantTileCss = stripGlobal(extractStyle(participantTileSource, 'ParticipantTile.svelte'));
const productionCss = [tokensCss, participantTileCss, galleryCss].join('\n');

const NAMES = ['Till (you)', 'Jason Thomas', 'Priya Patel', 'Marco Diaz', 'Sana Wu'];

/** Mirrors the real DOM Gallery.svelte's `participantTile` snippet renders,
 * scoped to exactly what the rail's CSS selectors key off (`.tile-wrap`,
 * `.spotlight-thumb`, `.tile`, `.camera-off-name`, `.name-chip`,
 * `.muted-chip`). */
function thumbHtml(name, { camerasOff, muted }) {
  return `
    <div class="tile-wrap spotlight-thumb" role="button" tabindex="0">
      <div class="tile">
        <span class="off-fill active" aria-hidden="true"></span>
        <span class="camera-off-name active" aria-label="${name}">${name}</span>
        <span class="camera-off-name camera-off-name-measure" aria-hidden="true">${name}</span>
        ${
          camerasOff
            ? ''
            : `<div class="name-chip"><span class="name-chip-visible">${name}</span><span class="name-chip-measure" aria-hidden="true">${name}</span></div>`
        }
        ${muted ? '<div class="muted-chip" title="Muted"></div>' : ''}
      </div>
    </div>`;
}

function pageHtml() {
  const thumbs = NAMES.map((name, index) =>
    thumbHtml(name, { camerasOff: index % 2 === 0, muted: index === 0 })
  ).join('\n');
  return `<!doctype html>
<html>
<body style="margin:0;background:#0a0a0b">
  <div class="tiles spotlight" style="width:100vw;height:100vh;box-sizing:border-box;">
    <div class="spotlight-layout">
      <div class="tile-wrap spotlight-main">
        <div class="tile"><span class="camera-off-name active">Hero</span></div>
      </div>
      <div class="spotlight-rail" aria-label="Other gallery feeds">
        ${thumbs}
      </div>
    </div>
  </div>
</body>
</html>`;
}

const failures = [];
function check(condition, message) {
  if (condition) console.log(`  ok   ${message}`);
  else {
    console.log(`  FAIL ${message}`);
    failures.push(message);
  }
}

function rectsOverlap(a, b) {
  return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
}

async function measureRail(page) {
  return page.evaluate(() => {
    const thumbs = Array.from(document.querySelectorAll('.spotlight-rail .tile-wrap.spotlight-thumb'));
    return thumbs.map((el) => {
      const rect = el.getBoundingClientRect();
      const nameEl = el.querySelector('.camera-off-name.active');
      const nameFontSize = nameEl ? parseFloat(getComputedStyle(nameEl).fontSize) : null;
      return {
        left: rect.left,
        right: rect.right,
        top: rect.top,
        bottom: rect.bottom,
        width: rect.width,
        height: rect.height,
        nameFontSize
      };
    });
  });
}

async function runScenario(browser, { label, width, height }) {
  console.log(`\nscenario: ${label} (${width}x${height})`);
  const page = await browser.newPage({ viewport: { width, height } });
  try {
    await page.setContent(pageHtml());
    await page.addStyleTag({ content: productionCss });
    // Let fonts settle / layout stabilize before measuring.
    await page.waitForTimeout(50);

    const thumbs = await measureRail(page);
    check(thumbs.length === NAMES.length, `all ${NAMES.length} thumbnails rendered (got ${thumbs.length})`);

    // (a) no rect intersection between adjacent thumbs.
    let overlapFound = false;
    for (let i = 0; i < thumbs.length; i++) {
      for (let j = i + 1; j < thumbs.length; j++) {
        if (rectsOverlap(thumbs[i], thumbs[j])) {
          overlapFound = true;
          console.log(
            `    overlap: thumb[${i}] (${thumbs[i].left.toFixed(1)}-${thumbs[i].right.toFixed(1)}) vs ` +
              `thumb[${j}] (${thumbs[j].left.toFixed(1)}-${thumbs[j].right.toFixed(1)})`
          );
        }
      }
    }
    check(!overlapFound, 'no two rail thumbnails overlap');

    // (b) true 16:9 within tolerance, and all thumbs share the same height
    // (they all sit in one flex row stretched to the rail height).
    const ASPECT_TOLERANCE = 0.06; // relative
    let aspectOk = true;
    for (const [i, t] of thumbs.entries()) {
      const aspect = t.width / t.height;
      const expected = 16 / 9;
      const relError = Math.abs(aspect - expected) / expected;
      if (relError > ASPECT_TOLERANCE) {
        aspectOk = false;
        console.log(`    thumb[${i}] aspect ${aspect.toFixed(3)} vs expected ${expected.toFixed(3)} (${t.width.toFixed(1)}x${t.height.toFixed(1)})`);
      }
    }
    check(aspectOk, `all thumbnails hold 16:9 within ${(ASPECT_TOLERANCE * 100).toFixed(0)}%`);

    // Gaps: adjacent thumbs (sorted by left) should be separated by
    // something close to the rail's 12px gap, not by a stretch artifact
    // (the wide-window ~83px dead-space bug) and not by a negative gap
    // (overlap, already checked above but this catches near-misses too).
    const sorted = [...thumbs].sort((a, b) => a.left - b.left);
    let gapsOk = true;
    for (let i = 1; i < sorted.length; i++) {
      const gap = sorted[i].left - sorted[i - 1].right;
      if (gap < 6 || gap > 30) {
        gapsOk = false;
        console.log(`    gap between thumb ${i - 1} and ${i}: ${gap.toFixed(1)}px (expected ~12px)`);
      }
    }
    check(gapsOk, 'adjacent thumbnails are separated by ~12px, not overlapping or stretched apart');

    // (c) name font-size capped for the thumbnail size. The pre-fix value
    // was a flat 26px regardless of tile size; the fix clamps to <=20px.
    let fontOk = true;
    for (const [i, t] of thumbs.entries()) {
      if (t.nameFontSize === null || t.nameFontSize > 20.5 || t.nameFontSize < 10) {
        fontOk = false;
        console.log(`    thumb[${i}] camera-off-name font-size: ${t.nameFontSize}`);
      }
    }
    check(fontOk, 'camera-off-name font-size is capped (<=~20px), not the unscaled 26px base');

    return { thumbs };
  } finally {
    await page.close();
  }
}

const browser = await chromium.launch({ headless: true });
try {
  console.log('#676 spotlight rail: no overlap, true 16:9, scaled typography\n');

  // Window height held at the app's own default (640px) for every scenario
  // below, so only window WIDTH varies -- these three exist to isolate the
  // width-driven bugs (#676's overlap + wide-window stretch). A window
  // SHORT enough to engage the separate max-height rail/hero guard (added
  // alongside this fix, `@media (max-height: 560px)`) legitimately lets the
  // rail's `min-width: 132px` floor win over the aspect-ratio-derived width
  // there (matching web-harness's own accepted tradeoff at extreme sizes),
  // so it's deliberately not exercised by this width-focused script -- see
  // the human test plan in the issue for that axis.
  await runScenario(browser, { label: 'default app window', width: 400, height: 640 });
  // Wide: rail height stays near its 104px floor (~131px here, since the
  // >620px width means the narrower breakpoint's own padding/floor don't
  // apply) while the window is wide enough (1800px) to leave several
  // hundred px of free space in the rail after packing 5 thumbs -- exactly
  // the condition that made `grid-auto-columns: auto` stretch tracks and
  // open ~83px gaps between correctly-sized thumbs.
  await runScenario(browser, { label: 'wide window (auto-track over-stretch case)', width: 1800, height: 640 });
  await runScenario(browser, { label: 'near min window width', width: 380, height: 640 });

  console.log('');
  if (failures.length > 0) {
    console.error(`spotlight-rail verification FAILED (${failures.length}):`);
    for (const failure of failures) console.error(`  - ${failure}`);
    process.exit(1);
  }
  console.log('spotlight-rail verification PASSED');
} finally {
  await browser.close();
}
