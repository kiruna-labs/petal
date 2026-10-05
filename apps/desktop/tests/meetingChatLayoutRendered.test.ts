// The desktop meeting window with the chat open, at every gallery size (the
// window becomes the pill below GALLERY_BREAKPOINT, 520 px; tauri.conf.json
// minHeight is 360). The chat panel lives between the gallery's topbar and
// control bar: a 320 px column beside the tiles from 720 px, over the tiles
// below that. At every size the composer and Send are on screen and not
// covered, every control is on screen and not covered, and nothing scrolls
// sideways. When the control row would not fit (Chat plus the default-on
// Reactions button made it ~614 px), Invite and then Chat move into More,
// which carries the unread count, and they come back when the window grows.
// Chat text is selectable (app.css turns selection off everywhere else), and
// each sender's avatar and name carry their meeting color.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { chromium, type Browser, type Page } from 'playwright';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

type Layout = {
  scrollsX: boolean;
  covered: Record<string, string>;
  outOfView: string[];
  clusterFits: boolean;
  collapsed: string[];
  panel: { left: number; right: number; width: number } | null;
  tiles: { left: number; right: number };
};

/** Each named element: on screen and the top element at its centre. */
function measure(page: Page): Promise<Layout> {
  return page.evaluate(() => {
    const vw = innerWidth;
    const vh = innerHeight;
    const targets: Record<string, string> = {
      input: '[data-testid="chat-input"]',
      send: '[data-testid="chat-send"]',
      mic: '.controlbar [data-control="mic"], .controlbar .control-cell:first-child button',
      more: '.controlbar button[aria-label="More meeting controls"]',
      leave: '.controlbar button[aria-label="Leave meeting"]',
      react: '.controlbar .plugin-cell button',
      close: '.chat-close',
    };
    const covered: Record<string, string> = {};
    const outOfView: string[] = [];
    for (const [name, selector] of Object.entries(targets)) {
      const el = document.querySelector(selector) as HTMLElement | null;
      if (!el) {
        outOfView.push(`${name} (missing)`);
        continue;
      }
      const b = el.getBoundingClientRect();
      if (b.width < 1 || b.height < 1 || b.left < -0.5 || b.top < -0.5 || b.right > vw + 0.5 || b.bottom > vh + 0.5) {
        outOfView.push(`${name} [${Math.round(b.left)},${Math.round(b.top)},${Math.round(b.right)},${Math.round(b.bottom)}]`);
        continue;
      }
      const top = document.elementFromPoint(b.left + b.width / 2, b.top + b.height / 2);
      if (top && !el.contains(top) && !top.contains(el)) covered[name] = String((top as HTMLElement).className || top.tagName);
    }
    const bar = document.querySelector('.controlbar') as HTMLElement;
    const cluster = document.querySelector('.controls-cluster') as HTMLElement;
    const barStyle = getComputedStyle(bar);
    const available = bar.clientWidth - parseFloat(barStyle.paddingLeft) - parseFloat(barStyle.paddingRight);
    const panel = document.querySelector('[data-testid="chat-aside"]')?.getBoundingClientRect() ?? null;
    const tiles = (document.querySelector('.tiles') as HTMLElement).getBoundingClientRect();
    return {
      scrollsX: document.documentElement.scrollWidth > vw || document.body.scrollWidth > vw,
      covered,
      outOfView,
      clusterFits: cluster.scrollWidth <= available + 0.5,
      collapsed: [...document.querySelectorAll<HTMLElement>('.control-cell.collapsed')].map((el) => el.dataset.control ?? '?'),
      panel: panel ? { left: panel.left, right: panel.right, width: panel.width } : null,
      tiles: { left: tiles.left, right: tiles.right },
    };
  });
}

async function openFixture(browser: Browser, buildDir: string, width: number, height: number, setup = ''): Promise<Page> {
  const page = await browser.newPage({ viewport: { width, height } });
  if (setup) await page.addInitScript(setup);
  await page.goto(pathToFileURL(join(buildDir, 'meeting-chat.html')).href);
  await page.waitForFunction(() => document.body.dataset.fixtureReady === 'true' || Boolean(document.body.dataset.fixtureError));
  const error = await page.locator('body').getAttribute('data-fixture-error');
  assert.equal(error, null, `fixture failed: ${decodeURIComponent(error ?? '')}`);
  // Let the control-row ResizeObserver settle (one collapse per pass).
  await page.waitForTimeout(150);
  return page;
}

test('meeting chat keeps composer and controls usable at every gallery size', { timeout: 180_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-meeting-chat-build-'));
  let browser: Browser | undefined;
  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: { tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } }) },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          '$app/environment': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot))),
        },
      },
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./meeting-chat.html', fixtureRoot)) } },
    });
    browser = await chromium.launch({ headless: true, args: ['--allow-file-access-from-files', '--disable-gpu'] });

    // 1. Every gallery size, chat open.
    const sizes: Array<[number, number]> = [
      [520, 600], [560, 600], [614, 600], [640, 600], [719, 600], [720, 600],
      [900, 600], [1000, 700], [1440, 900], [520, 360], [900, 360],
    ];
    for (const [width, height] of sizes) {
      const label = `${width}x${height}`;
      const page = await openFixture(browser, buildDir, width, height);
      const layout = await measure(page);
      assert.equal(layout.scrollsX, false, `${label}: the page scrolls sideways`);
      assert.deepEqual(layout.outOfView, [], `${label}: off screen: ${layout.outOfView.join(', ')}`);
      assert.deepEqual(layout.covered, {}, `${label}: covered: ${JSON.stringify(layout.covered)}`);
      assert.ok(layout.clusterFits, `${label}: the control row overflows the control bar`);
      assert.ok(layout.panel, `${label}: the chat panel is open`);
      if (width >= 720) {
        assert.equal(Math.round(layout.panel.width), 320, `${label}: a 320 px column`);
        assert.ok(layout.tiles.right <= layout.panel.left + 0.5, `${label}: tiles sit beside the panel`);
      } else {
        // Over the tiles, never the controls: the whole width, or -- in a
        // short window, whose controls stand in a rail at the right edge --
        // the width left of the rail.
        assert.ok(
          layout.panel.left <= layout.tiles.left + 0.5 && layout.panel.right >= layout.tiles.right - 0.5,
          `${label}: the panel covers the tiles: ${JSON.stringify({ panel: layout.panel, tiles: layout.tiles })}`
        );
        assert.equal(Math.round(layout.panel.width), height < 420 ? Math.round(layout.tiles.right - layout.tiles.left) : width, `${label}: the panel's width`);
      }
      // Chat stays reachable: its own control, or a More row.
      if (layout.collapsed.includes('chat')) {
        await page.click('.controlbar button[aria-label="More meeting controls"]');
        assert.ok(await page.locator('[data-testid="more-chat"]').isVisible(), `${label}: Chat is in More`);
        assert.equal(await page.locator('[data-testid="more-chat"]').textContent().then((t) => t?.includes('Close chat')), true, `${label}: More says Close chat while open`);
      } else {
        assert.ok(await page.locator('.controlbar [data-control="chat"]').isVisible(), `${label}: Chat control visible`);
      }
      await page.close();
    }

    // 2. Narrowest gallery, chat closed with unread messages: Chat is in
    // More, More carries the count, the More row opens chat, and growing the
    // window brings Invite and Chat back.
    {
      const page = await openFixture(browser, buildDir, 520, 600, 'window.__chatOpen = false; window.__chatUnread = 3;');
      const layout = await measure(page);
      assert.deepEqual(layout.collapsed, ['invite', 'chat'], '520 px: Invite then Chat collapse into More');
      assert.equal(layout.panel, null);
      assert.equal(await page.locator('[data-testid="more-chat-badge"]').textContent(), '3', 'More carries the unread count');
      await page.click('.controlbar button[aria-label="More meeting controls"]');
      assert.match((await page.locator('[data-testid="more-chat"]').textContent()) ?? '', /Chat\s*3 unread/);
      assert.ok(await page.locator('[data-testid="more-invite"]').isVisible(), 'Invite is in More');
      await page.click('[data-testid="more-chat"]');
      assert.deepEqual(await page.evaluate(() => (window as unknown as { __controls: string[] }).__controls), ['chat']);

      await page.setViewportSize({ width: 900, height: 600 });
      await page.waitForFunction(() => document.querySelectorAll('.control-cell.collapsed').length === 0, null, { timeout: 5_000 });
      assert.equal(await page.locator('[data-testid="more-chat-badge"]').count(), 0, 'no More badge once Chat is back');
      await page.setViewportSize({ width: 560, height: 600 });
      await page.waitForFunction(() => document.querySelectorAll('.control-cell.collapsed').length === 1, null, { timeout: 5_000 });
      assert.deepEqual((await measure(page)).collapsed, ['invite'], '560 px: only Invite collapses');
      await page.close();
    }

    // 3. Chat text is selectable (whole messages or part of one) and each
    // sender's avatar and name carry their meeting color.
    {
      const page = await openFixture(browser, buildDir, 900, 600);
      assert.equal(await page.locator('[data-testid="chat-list"]').evaluate((el) => getComputedStyle(el).userSelect), 'text');
      assert.equal(await page.locator('.tiles').evaluate((el) => getComputedStyle(el).userSelect), 'none', 'the rest of the app stays unselectable');
      const texts = page.locator('.chat-text');
      const count = await texts.count();
      const first = await texts.nth(count - 4).boundingBox();
      const last = await texts.nth(count - 3).boundingBox();
      assert.ok(first && last);
      // Press inside the first line (not on the gap between wrapped lines).
      await page.mouse.move(first.x + 2, first.y + 8);
      await page.mouse.down();
      await page.mouse.move(last.x + last.width - 2, last.y + last.height / 2, { steps: 8 });
      await page.mouse.up();
      const selected = await page.evaluate(() => window.getSelection()?.toString() ?? '');
      const expectedEnd = (await texts.nth(count - 3).textContent())!.trim().slice(-6);
      assert.ok(selected.startsWith('A longer line') && selected.includes(expectedEnd), `drag selects across messages (got ${JSON.stringify(selected)})`);
      await page.evaluate(() => window.getSelection()?.removeAllRanges());
      await page.mouse.move(first.x + 2, first.y + 8);
      await page.mouse.down();
      await page.mouse.move(first.x + 120, first.y + 8, { steps: 6 });
      await page.mouse.up();
      const part = await page.evaluate(() => window.getSelection()?.toString() ?? '');
      assert.ok(part.length > 4 && 'A longer line so the list'.startsWith(part.trimEnd()), `part of one message selects (got ${JSON.stringify(part)})`);

      const people = await page.evaluate(() =>
        [...document.querySelectorAll<HTMLElement>('[data-testid="chat-msg"]')]
          .filter((m) => m.querySelector('[data-testid="chat-avatar"]'))
          .map((m) => {
            const avatar = m.querySelector('[data-testid="chat-avatar"]') as HTMLElement;
            return {
              name: m.querySelector('.chat-name')?.textContent,
              local: m.dataset.local === 'true',
              initial: avatar.textContent?.trim(),
              avatarColor: getComputedStyle(avatar).color,
              ring: getComputedStyle(avatar, '::after').borderTopColor,
              nameColor: getComputedStyle(m.querySelector('.chat-name')!).color,
              puzzle: Boolean(avatar.querySelector('svg')),
            };
          }),
      );
      const mira = people.find((p) => p.name === 'Mira')!;
      assert.deepEqual([mira.initial, mira.avatarColor, mira.ring, mira.nameColor], ['M', 'rgb(240, 108, 201)', 'rgb(240, 108, 201)', 'rgb(240, 108, 201)']);
      const theo = people.find((p) => p.name === 'Theo')!;
      assert.deepEqual([theo.initial, theo.avatarColor, theo.nameColor], ['T', 'rgb(110, 139, 255)', 'rgb(110, 139, 255)']);
      const notice = people.find((p) => p.local)!;
      assert.equal(notice.name, 'Timer (plugin)');
      assert.ok(notice.puzzle, 'a private plugin answer shows the puzzle, not a person');
      assert.notEqual(notice.nameColor, 'rgb(127, 240, 163)', 'and not my color');

      // Continuation lines align with the first line's text, not the avatar.
      const aligned = await page.evaluate(() => {
        const msgs = [...document.querySelectorAll<HTMLElement>('[data-testid="chat-msg"]')];
        const cont = msgs.find((m) => m.classList.contains('continues'));
        const lead = msgs.find((m) => !m.classList.contains('continues'));
        if (!cont || !lead) return null;
        return [cont.querySelector('.chat-text')!.getBoundingClientRect().left, lead.querySelector('.chat-text')!.getBoundingClientRect().left];
      });
      if (aligned) assert.ok(Math.abs(aligned[0] - aligned[1]) < 0.5, `continuation text aligns (${aligned})`);
      await page.close();
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
