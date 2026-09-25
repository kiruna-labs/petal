// The shared ChatDrawer at the width both clients give it (320 px, and the
// 60%-of-400px column the desktop's minimum window leaves), and in the
// desktop's shortest window (360 px tall, tauri.conf.json minHeight; #246):
// long names ellipsize, unbroken tokens wrap, nothing scrolls horizontally,
// consecutive lines from one sender group under one name, the composer is one
// row with Send beside the input, Enter sends and Shift+Enter does not,
// clicking Send keeps focus in the input, the empty/over-limit composer
// cannot send, going over the limit is announced once (and typing near it is
// not), Escape closes, and at least four lines of message text stay
// readable. The browser's landscape-phone layout is
// web-harness/tests/chatDrawerLayoutRendered.test.ts.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { chromium, type Page } from 'playwright';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

type Box = { left: number; right: number; top: number; bottom: number };

/** Input, Send and (when shown) the character count, as page boxes. */
async function composerBoxes(page: Page): Promise<{ input: Box; send: Box; count: Box | null }> {
  const [input, send, count] = await page.evaluate(() =>
    ['[data-testid="chat-input"]', '[data-testid="chat-send"]', '.chat-count'].map((selector) => {
      const el = document.querySelector(selector);
      if (!el || el.getClientRects().length === 0) return null;
      const { left, right, top, bottom } = el.getBoundingClientRect();
      return { left, right, top, bottom };
    }),
  );
  return { input: input!, send: send!, count };
}

/** The over-limit status as the accessibility tree has it: null when it is
 * not in the tree (display: none, say), so a screen reader would not be
 * tracking it when it first speaks. */
async function limitStatus(page: Page): Promise<{ role: string; live: string; text: string } | null> {
  const cdp = await page.context().newCDPSession(page);
  try {
    const { root } = await cdp.send('DOM.getDocument', { depth: 0 });
    const { nodeId } = await cdp.send('DOM.querySelector', { nodeId: root.nodeId, selector: '.chat-limit-status' });
    if (!nodeId) return null;
    const { nodes } = await cdp.send('Accessibility.getPartialAXTree', { nodeId, fetchRelatives: false });
    const node = nodes[0];
    if (!node || node.ignored) return null;
    const live = node.properties?.find((property) => property.name === 'live')?.value.value;
    const text = await page.locator('.chat-limit-status').textContent();
    return { role: String(node.role?.value ?? ''), live: String(live ?? ''), text: text ?? '' };
  } finally {
    await cdp.detach();
  }
}

/** Send sits to the right of the input and overlaps it vertically: one row. */
function assertSendBesideInput({ input, send }: { input: Box; send: Box }, label: string): void {
  assert.ok(send.left >= input.right - 1, `${label}: Send (left ${send.left}) is right of the input (right ${input.right})`);
  assert.ok(send.top < input.bottom && send.bottom > input.top, `${label}: Send (${send.top}-${send.bottom}) shares the input's row (${input.top}-${input.bottom})`);
}

/** Lines of message text (not name lines) wholly inside the list's viewport. */
function visibleTextLines(page: Page): Promise<number> {
  return page.evaluate(() => {
    const list = (document.querySelector('[data-testid="chat-list"]') as HTMLElement).getBoundingClientRect();
    let lines = 0;
    for (const el of document.querySelectorAll('.chat-text')) {
      const range = document.createRange();
      range.selectNodeContents(el);
      const tops = [...range.getClientRects()]
        .filter((r) => r.height > 4 && r.top >= list.top - 1 && r.bottom <= list.bottom + 1)
        .map((r) => r.top)
        .sort((a, b) => a - b);
      let last = -Infinity;
      for (const top of tops) {
        if (top - last > 8) lines++;
        last = top;
      }
    }
    return lines;
  });
}

test('chat drawer fits its column, groups senders, and sends on Enter', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-chat-drawer-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot))),
        },
      },
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./chat-drawer.html', fixtureRoot)) } },
    });

    browser = await chromium.launch({ headless: true, args: ['--allow-file-access-from-files', '--disable-gpu'] });

    for (const { width, height } of [{ width: 400, height: 600 }, { width: 720, height: 600 }, { width: 800, height: 360 }]) {
      const page = await browser.newPage({ viewport: { width, height } });
      await page.goto(pathToFileURL(join(buildDir, 'chat-drawer.html')).href);
      await page.waitForFunction(() => document.body.dataset.ready === 'true');

      const fit = await page.evaluate(() => {
        const aside = document.querySelector('[data-testid="chat-aside"]') as HTMLElement;
        const list = document.querySelector('[data-testid="chat-list"]') as HTMLElement;
        const overflowing = [...document.querySelectorAll<HTMLElement>('.chat-drawer *')].filter(
          (el) => el.scrollWidth > el.clientWidth + 1 && getComputedStyle(el).overflowX !== 'hidden' && !el.matches('textarea'),
        );
        return {
          asideWidth: aside.getBoundingClientRect().width,
          listScrollsX: list.scrollWidth > list.clientWidth + 1,
          overflowing: overflowing.map((el) => el.className),
          names: [...document.querySelectorAll<HTMLElement>('.chat-name')].map((el) => ({
            text: el.textContent,
            ellipsized: el.scrollWidth > el.clientWidth,
          })),
          messages: document.querySelectorAll('[data-testid="chat-msg"]').length,
          namedLines: document.querySelectorAll('.chat-meta').length,
          relayed: document.querySelectorAll('[data-testid="chat-msg"][data-relayed="true"]').length,
          times: [...document.querySelectorAll<HTMLElement>('.chat-time')].map((el) => el.textContent),
        };
      });
      assert.ok(fit.asideWidth <= 320 && fit.asideWidth >= 200, `${width}: aside ${fit.asideWidth}`);
      assert.equal(fit.listScrollsX, false, `${width}: the list must never scroll horizontally`);
      assert.deepEqual(fit.overflowing, [], `${width}: no element overflows its box`);
      assert.equal(fit.messages, 4);
      assert.equal(fit.namedLines, 3, 'Theo\'s two consecutive lines share one name line');
      assert.equal(fit.relayed, 1);
      assert.equal(fit.names[0].ellipsized, true, 'a very long name ellipsizes instead of wrapping or pushing the time out');
      assert.deepEqual(fit.names.map((n) => n.text), ['Mira Aleksandra Konstantinopoulou-Whitfield', 'Theo', 'You']);
      assert.ok(fit.times.every((t) => /\d/.test(t ?? '')), `times render: ${fit.times.join(', ')}`);

      // Composer: one row, Send beside the input and no count far from the
      // limit; empty cannot send; Shift+Enter inserts a line (Send stays
      // beside the grown input); Enter sends the normalized text and clears;
      // the new message appears at the bottom.
      const input = page.locator('[data-testid="chat-input"]');
      const send = page.locator('[data-testid="chat-send"]');
      const idle = await composerBoxes(page);
      assertSendBesideInput(idle, `${width}x${height}`);
      assert.equal(idle.count, null, `${width}x${height}: no character count far from the limit`);
      assert.equal(await send.isDisabled(), true);
      await input.click();
      await page.keyboard.type('  first line');
      await page.keyboard.press('Shift+Enter');
      await page.keyboard.type('second line  ');
      assert.equal(await send.isDisabled(), false);
      assert.equal(await input.inputValue(), '  first line\nsecond line  ');
      assertSendBesideInput(await composerBoxes(page), `${width}x${height} typing two lines`);
      await page.keyboard.press('Enter');
      const sent = await page.evaluate(() => (window as unknown as { __chat: { sent: string[] } }).__chat.sent);
      assert.deepEqual(sent, ['first line\nsecond line']);
      assert.equal(await input.inputValue(), '', 'composer clears after send');
      assert.equal(await page.locator('[data-testid="chat-msg"]').count(), 5);
      const last = await page.locator('[data-testid="chat-msg"]').last().locator('.chat-text').textContent();
      assert.equal(last, 'first line\nsecond line');
      const pinned = await page.evaluate(() => {
        const list = document.querySelector('[data-testid="chat-list"]') as HTMLElement;
        return list.scrollHeight - list.scrollTop - list.clientHeight < 2;
      });
      assert.equal(pinned, true, 'the list follows the newest message');

      // Screen readers: the count is not a live region (it would read out
      // every keystroke); a status region, in the tree before it ever
      // speaks, says only that the draft went over the limit.
      assert.equal(await page.locator('.chat-count').getAttribute('aria-live'), null, 'the count itself is not announced');
      assert.deepEqual(await limitStatus(page), { role: 'status', live: 'polite', text: '' }, `${width}x${height}: a silent status region is in the tree`);
      await input.fill('x'.repeat(1850));
      assert.equal(await page.locator('.chat-count').textContent(), '150', 'near the limit the count shows');
      assert.equal((await limitStatus(page))?.text, '', 'near the limit nothing is announced');

      // Over the limit: the counter warns (above Send, beside the input),
      // Send is disabled and the status speaks, once: another character over
      // leaves its text as it was. Escape closes.
      await input.fill('x'.repeat(2001));
      assert.equal(await send.isDisabled(), true);
      assert.equal(await page.locator('.chat-count.over').textContent(), '-1');
      const overStatus = await limitStatus(page);
      assert.match(overStatus?.text ?? '', /too long to send: 2000 characters at most/i);
      await input.fill('x'.repeat(2002));
      assert.equal(await page.locator('.chat-count.over').textContent(), '-2');
      assert.deepEqual(await limitStatus(page), overStatus, 'a second character over is not announced again');
      const over = await composerBoxes(page);
      assertSendBesideInput(over, `${width}x${height} over the limit`);
      assert.ok(over.count && over.count.bottom <= over.send.top && over.count.left >= over.input.right - 1, `${width}x${height}: the count sits above Send`);
      await page.keyboard.press('Escape');
      assert.equal(await page.evaluate(() => (window as unknown as { __closed: () => number }).__closed()), 1);

      // A busy chat: at least four lines of message text stay readable above
      // the composer, in the shortest window too (#246).
      await input.fill('');
      await page.evaluate(() => {
        const chat = (window as unknown as { __chat: { add: (m: unknown) => void } }).__chat;
        for (let i = 0; i < 12; i++) {
          chat.add({ id: `m-busy-${String(i).padStart(4, '0')}`, text: `Busy line ${i}`, t: Date.UTC(2026, 8, 14, 12, 3, i), sender: { identity: `peer-${i % 2}`, name: i % 2 ? 'Theo' : 'Mira' }, self: false, relayed: false });
        }
      });
      await page.waitForFunction(() => document.querySelectorAll('[data-testid="chat-msg"]').length === 17);
      const lines = await visibleTextLines(page);
      assert.ok(lines >= 4, `${width}x${height}: ${lines} lines of message text visible`);

      // A second line grows the input and shrinks the list (as the soft
      // keyboard does): the newest message stays in view.
      await input.click();
      await page.keyboard.type('one');
      await page.keyboard.press('Shift+Enter');
      await page.keyboard.type('two');
      const stillPinned = await page.evaluate(async () => {
        await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
        const list = document.querySelector('[data-testid="chat-list"]') as HTMLElement;
        return list.scrollHeight - list.scrollTop - list.clientHeight < 2;
      });
      assert.equal(stillPinned, true, `${width}x${height}: the list stays on the newest message while the input grows`);

      // Clicking Send submits without taking focus from the input (on a
      // phone, a blur would drop the soft keyboard between messages).
      await page.evaluate(() => {
        const w = window as unknown as { __blurs: number };
        w.__blurs = 0;
        document.querySelector('[data-testid="chat-input"]')!.addEventListener('blur', () => w.__blurs++);
      });
      await send.click();
      assert.deepEqual(await page.evaluate(() => (window as unknown as { __chat: { sent: string[] } }).__chat.sent), ['first line\nsecond line', 'one\ntwo']);
      assert.equal(await page.evaluate(() => (window as unknown as { __blurs: number }).__blurs), 0, `${width}x${height}: Send click keeps focus in the input`);
      assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('data-testid')), 'chat-input');
      await page.close();
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
