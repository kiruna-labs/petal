// #122, rendered: hovering a live room row shows WHO is in it, and the
// tooltip is FULLY VISIBLE while it does.
//
// Why this has to be a rendered test and not a source-text one:
//   - CSS `:hover` and `:active` cannot be driven by synthetic events. Real
//     pointer input is the only thing that puts the row in those states.
//   - The rows scroll inside `.room-list-scroll` (`overflow-y: auto`) under
//     `.main-menu { overflow: hidden }`. An absolutely-positioned tooltip on
//     the bottom row is CLIPPED, and `getBoundingClientRect()` reports the
//     clipped element's full, on-paper box regardless -- it cannot tell.
//     `document.elementFromPoint()` at the tooltip's centre AND four corners
//     can: a clipped pixel belongs to some other element.
//   - `.room-row-shell.clickable:active { transform: scale(...) }` makes the
//     PRESSED row the containing block for a `position: fixed` child, which
//     would snap the tooltip into row coordinates mid-press. Only a real
//     mouse-down reproduces it.
//   - CLAUDE.md's "UI text must NEVER truncate" rule: measured
//     (`scrollWidth <= clientWidth`) at both documented main-window widths,
//     with the real self-hosted fonts.
//
// It also owns the hero/re-sort regression: names arriving for a non-joined
// live room must NOT promote it into the LiveHero card or reorder the list.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import test from 'node:test';
import { chromium, type Page } from 'playwright';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

function fixturePath(name: string): string {
  return fileURLToPath(new URL(name, fixtureRoot));
}

const LIVE_ROOM = 'eng-sync';
const SECOND_LIVE_ROOM = 'design-review';
const EXPECTED_SUMMARY =
  'Alexandra Featherstonehaugh, Bartholomew Oyelaran-Whitfield, Chidinma Nwachukwu and 3 more in this room';

type TooltipReading = {
  found: boolean;
  text: string;
  opacity: string;
  visibility: string;
  scrollWidth: number;
  clientWidth: number;
  scrollHeight: number;
  clientHeight: number;
  textOverflow: string;
  whiteSpace: string;
  pointerEvents: string;
  rect: { left: number; top: number; right: number; bottom: number; width: number; height: number };
  /** elementFromPoint at the centre + four inset corners, as booleans. */
  hitsSelf: boolean[];
  insideViewport: boolean;
};

async function readTooltip(page: Page, room: string): Promise<TooltipReading> {
  return (await page.evaluate(`(() => {
    const row = [...document.querySelectorAll('.room-row-shell')]
      .find((shell) => shell.querySelector('.room-name')?.textContent?.trim() === ${JSON.stringify(room)});
    const tip = row?.querySelector('[data-testid="room-roster-tooltip"]');
    if (!tip) {
      return {
        found: false, text: '', opacity: '0', visibility: 'hidden',
        scrollWidth: 0, clientWidth: 0, scrollHeight: 0, clientHeight: 0,
        textOverflow: '', whiteSpace: '', pointerEvents: 'none',
        rect: { left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0 },
        hitsSelf: [], insideViewport: false
      };
    }
    const style = getComputedStyle(tip);
    const r = tip.getBoundingClientRect();
    // Inset past the corner radius (--radius-chip, 8px): a probe closer than
    // that to a corner falls OUTSIDE the rounded border box and misses the
    // tooltip even when nothing is clipping it.
    const inset = 12;
    const probes = [
      [r.left + r.width / 2, r.top + r.height / 2],
      [r.left + inset, r.top + inset],
      [r.right - inset, r.top + inset],
      [r.left + inset, r.bottom - inset],
      [r.right - inset, r.bottom - inset]
    ];
    // The tooltip ships pointer-events:none -- it must never swallow a click
    // meant for the row underneath it -- and that also removes it from hit
    // testing, so elementFromPoint would always answer 'something else'.
    // Opt it back in for the duration of the probe ONLY: the question being
    // asked is 'which element owns this pixel', and clipping (an ancestor's
    // overflow) and covering (a later-painted element) both still answer it
    // correctly. The resting value is asserted separately.
    const restingPointerEvents = style.pointerEvents;
    tip.style.pointerEvents = 'auto';
    void tip.offsetWidth;
    const hitsSelf = probes.map(([x, y]) => {
      const hit = document.elementFromPoint(x, y);
      return hit === tip || (hit instanceof Node && tip.contains(hit));
    });
    tip.style.pointerEvents = '';
    return {
      found: true,
      text: tip.textContent ?? '',
      opacity: style.opacity,
      visibility: style.visibility,
      scrollWidth: tip.scrollWidth,
      clientWidth: tip.clientWidth,
      scrollHeight: tip.scrollHeight,
      clientHeight: tip.clientHeight,
      textOverflow: style.textOverflow,
      whiteSpace: style.whiteSpace,
      pointerEvents: restingPointerEvents,
      rect: { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height },
      hitsSelf,
      insideViewport:
        r.left >= 0 && r.top >= 0 && r.right <= window.innerWidth && r.bottom <= window.innerHeight
    };
  })()`)) as TooltipReading;
}

/**
 * Poll a tooltip reading until it settles instead of sleeping a guessed
 * interval past it (the #75 pattern). Returns the last sample either way, so
 * the caller's assertion still reports the real value on timeout.
 */
async function pollTooltip(
  page: Page,
  room: string,
  settled: (reading: TooltipReading) => boolean,
  timeoutMs = 4_000
): Promise<TooltipReading> {
  const deadline = Date.now() + timeoutMs;
  let reading = await readTooltip(page, room);
  while (!settled(reading) && Date.now() < deadline) {
    await page.waitForTimeout(50);
    reading = await readTooltip(page, room);
  }
  return reading;
}

function rowLocator(page: Page, room: string) {
  return page.locator('.room-row-shell').filter({ has: page.locator('.room-name', { hasText: room }) });
}

test('#122: the room-roster tooltip shows on hover and focus, never clipped, never truncated', { timeout: 120_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-room-roster-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      // `npm test` must work immediately after `npm ci`, before
      // `svelte-kit sync` has generated .svelte-kit/tsconfig.json.
      esbuild: {
        tsconfigRaw: JSON.stringify({
          compilerOptions: { target: 'ES2022', useDefineForClassFields: true }
        })
      },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          '$app/environment': fixturePath('sveltekit-environment.ts'),
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot)))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fixturePath('room-roster-tooltip.html') }
      }
    });

    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files']
    });

    // 400px is tauri.conf.json's main-window width; 380px its minWidth.
    for (const width of [400, 380]) {
      const page = await browser.newPage({
        viewport: { width, height: 580 },
        deviceScaleFactor: 1
      });
      await page.goto(pathToFileURL(join(buildDir, 'room-roster-tooltip.html')).href, { waitUntil: 'load' });
      await page.waitForFunction(
        () => document.body.dataset.fixtureReady === 'true' || !!document.body.dataset.fixtureError,
        { timeout: 20_000 }
      );
      const fixtureError = await page.locator('body').getAttribute('data-fixture-error');
      assert.equal(fixtureError, null, fixtureError ? decodeURIComponent(fixtureError) : '');

      const at = (message: string) => `${width}px: ${message}`;

      // --- The hero/re-sort regression, checked BEFORE any hover. -----------
      // Names arrived for two non-joined live rooms. Nothing may move.
      assert.equal(await page.locator('.hero--live').count(), 0, at('a roster promoted a room into the LiveHero card'));
      const order = await page.locator('.room-list .room-row-shell .room-name').allTextContents();
      assert.deepEqual(
        order.map((name) => name.trim()),
        [
          'design-review', 'standup', 'infra-sync', 'release-train', 'oncall-handoff', 'bug-triage',
          'growth-weekly', 'security-review', 'platform-guild', 'hiring-loop', 'roadmap-review', 'eng-sync'
        ],
        at('the room list re-sorted when names arrived')
      );
      // The dot/avatar branch is driven by `participants`, which the status
      // lookup does not feed -- so a roster row still shows the plain dot.
      assert.equal(await page.locator('.avatar-stack').count(), 0, at('a roster rendered an avatar stack'));

      // A room with no roster has no tooltip element at all.
      const quiet = await readTooltip(page, 'standup');
      assert.equal(quiet.found, false, at('a room with no roster must render no tooltip'));

      // --- Hover the BOTTOM row of the scrolled list. -----------------------
      const liveRow = rowLocator(page, LIVE_ROOM);
      await liveRow.hover();
      const hovered = await pollTooltip(page, LIVE_ROOM, (r) => r.opacity === '1');
      assert.equal(hovered.found, true, at('the live row rendered no tooltip'));
      assert.equal(hovered.text, EXPECTED_SUMMARY, at('the tooltip text is wrong'));
      assert.equal(hovered.opacity, '1', at(`the tooltip did not reveal on hover (opacity ${hovered.opacity})`));
      assert.notEqual(hovered.visibility, 'hidden', at('the tooltip is visibility:hidden'));
      assert.ok(hovered.rect.width > 0 && hovered.rect.height > 0, at('the tooltip has no rendered box'));

      // Fully on screen, and every probe lands on the tooltip itself -- the
      // reading that a clipped-by-the-scroller tooltip cannot pass.
      assert.equal(hovered.insideViewport, true, at(`the tooltip left the window: ${JSON.stringify(hovered.rect)}`));
      assert.deepEqual(
        hovered.hitsSelf,
        [true, true, true, true, true],
        at(`the tooltip is clipped or covered (centre + 4 corners: ${JSON.stringify(hovered.hitsSelf)} rect=${JSON.stringify(hovered.rect)})`)
      );

      // The no-truncation rule, measured.
      assert.ok(
        hovered.scrollWidth <= hovered.clientWidth,
        at(`the tooltip text is truncated (scrollWidth ${hovered.scrollWidth} > clientWidth ${hovered.clientWidth})`)
      );
      // Vertical too: a fixed height with `overflow: hidden` clips the second
      // and third wrapped lines, which the width check cannot see.
      assert.ok(
        hovered.scrollHeight <= hovered.clientHeight,
        at(`the tooltip clips its wrapped lines (scrollHeight ${hovered.scrollHeight} > clientHeight ${hovered.clientHeight})`)
      );
      assert.notEqual(hovered.textOverflow, 'ellipsis', at('the tooltip would ellipsize'));
      assert.notEqual(hovered.whiteSpace, 'nowrap', at('the tooltip must wrap, not run off'));
      assert.equal(
        hovered.pointerEvents,
        'none',
        at('the tooltip overlays the rows below it and must never swallow their clicks')
      );

      // --- Press and HOLD: the `:active` containing-block trap. -------------
      const box = await liveRow.boundingBox();
      assert.ok(box, at('the live row has no box'));
      await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
      await page.mouse.down();
      const pressed = await pollTooltip(page, LIVE_ROOM, (r) => r.opacity === '0');
      assert.equal(
        pressed.opacity,
        '0',
        at('the tooltip stayed visible while the row was pressed -- the scale() transform reparents it')
      );
      await page.mouse.up();
      await page.mouse.move(0, 0);
      await page.waitForTimeout(120);

      // --- Keyboard focus reveals it too. -----------------------------------
      // Real Tab navigation, not a programmatic .focus(): `:focus-visible` is
      // the whole point, and it is only asserted for keyboard focus. Tab from
      // the top of the document until the live ROW SHELL itself is the active
      // element -- the row's own inner controls (copy / favorite / remove) sit
      // between the rows in the tab order.
      await page.evaluate(`(() => { document.body.focus(); })()`);
      let landedOnRow = false;
      for (let tabs = 0; tabs < 160 && !landedOnRow; tabs++) {
        await page.keyboard.press('Tab');
        landedOnRow = (await page.evaluate(`(() => {
          const active = document.activeElement;
          return !!(active
            && active.classList.contains('room-row-shell')
            && active.querySelector('.room-name')?.textContent?.trim() === ${JSON.stringify(LIVE_ROOM)});
        })()`)) as boolean;
      }
      assert.equal(landedOnRow, true, at('Tab never reached the live room row'));
      const focused = await pollTooltip(page, LIVE_ROOM, (r) => r.opacity === '1');
      assert.equal(focused.opacity, '1', at(`keyboard focus did not reveal the tooltip (opacity ${focused.opacity})`));
      assert.equal(focused.insideViewport, true, at('the focused tooltip left the window'));
      assert.ok(
        focused.scrollWidth <= focused.clientWidth,
        at(`the focused tooltip is truncated (scrollWidth ${focused.scrollWidth} > clientWidth ${focused.clientWidth})`)
      );

      // The accessible name carries the same names the tooltip shows.
      const ariaLabel = await liveRow.getAttribute('aria-label');
      assert.ok(
        ariaLabel?.includes(EXPECTED_SUMMARY),
        at(`the row's accessible name does not carry the roster: ${ariaLabel}`)
      );

      // --- A short roster at the TOP of the list still fits and hits. -------
      await page.evaluate(`(() => { document.querySelector('.room-list-scroll').scrollTop = 0; })()`);
      await page.waitForTimeout(80);
      await rowLocator(page, SECOND_LIVE_ROOM).hover();
      const top = await pollTooltip(page, SECOND_LIVE_ROOM, (r) => r.opacity === '1');
      assert.equal(top.opacity, '1', at('the top row tooltip did not reveal'));
      assert.equal(top.text, 'Ada and Bruno in this room', at('the short roster reads wrong'));
      assert.deepEqual(top.hitsSelf, [true, true, true, true, true], at('the top row tooltip is clipped or covered'));
      assert.ok(top.scrollWidth <= top.clientWidth, at('the top row tooltip is truncated'));

      await page.close();
    }
  } finally {
    try {
      await browser?.close();
    } finally {
      await rm(buildDir, { recursive: true, force: true });
    }
  }
});
