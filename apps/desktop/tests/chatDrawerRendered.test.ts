// The shared ChatDrawer at the width both clients give it (320 px, and the
// 60%-of-400px column the desktop's minimum window leaves): long names
// ellipsize, unbroken tokens wrap, nothing scrolls horizontally, consecutive
// lines from one sender group under one name, Enter sends and Shift+Enter
// does not, the empty/over-limit composer cannot send, Escape closes.
// Plugins (I-7b): a post shows "via <plugin>" on its own line and never
// groups with the person's typed lines, a private answer says only you can
// see it, `/` lists commands with the owning plugin, Tab/Enter complete,
// Enter runs, a refused or unknown command keeps the draft and says why,
// and `//` sends a message that starts with `/`.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { chromium } from 'playwright';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

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

    for (const width of [400, 720]) {
      const page = await browser.newPage({ viewport: { width, height: 600 } });
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
          vias: [...document.querySelectorAll<HTMLElement>('[data-testid="chat-via"]')].map((el) => ({
            text: el.textContent?.replace(/\s+/g, ' ').trim(),
            title: el.getAttribute('title'),
            clipped: el.scrollWidth > el.clientWidth + 1,
          })),
          times: [...document.querySelectorAll<HTMLElement>('.chat-time')].map((el) => el.textContent),
        };
      });
      assert.ok(fit.asideWidth <= 320 && fit.asideWidth >= 200, `${width}: aside ${fit.asideWidth}`);
      assert.equal(fit.listScrollsX, false, `${width}: the list must never scroll horizontally`);
      assert.deepEqual(fit.overflowing, [], `${width}: no element overflows its box`);
      assert.equal(fit.messages, 6);
      assert.equal(fit.namedLines, 5, "Theo's two typed lines share a name line; his plugin post does not");
      assert.equal(fit.relayed, 1);
      assert.equal(fit.names[0].ellipsized, true, 'a very long name ellipsizes instead of wrapping or pushing the time out');
      assert.deepEqual(fit.names.map((n) => n.text), ['Mira Aleksandra Konstantinopoulou-Whitfield', 'Theo', 'You', 'Theo', 'Timer']);
      assert.deepEqual(
        fit.vias.map((v) => [v.text, v.clipped]),
        [['via Timer', false], ['Only you can see this', false]],
        'a plugin post is never shown as the person alone, and the label never clips',
      );
      assert.equal(fit.vias[0].title, 'Posted by the Timer plugin (petal.timer) for Theo');
      assert.ok(fit.times.every((t) => /\d/.test(t ?? '')), `times render: ${fit.times.join(', ')}`);

      // Composer: empty cannot send; Shift+Enter inserts a line; Enter sends
      // the normalized text and clears; the new message appears at the bottom.
      const input = page.locator('[data-testid="chat-input"]');
      const send = page.locator('[data-testid="chat-send"]');
      assert.equal(await send.isDisabled(), true);
      await input.click();
      await page.keyboard.type('  first line');
      await page.keyboard.press('Shift+Enter');
      await page.keyboard.type('second line  ');
      assert.equal(await send.isDisabled(), false);
      assert.equal(await input.inputValue(), '  first line\nsecond line  ');
      await page.keyboard.press('Enter');
      const sent = await page.evaluate(() => (window as unknown as { __chat: { sent: string[] } }).__chat.sent);
      assert.deepEqual(sent, ['first line\nsecond line']);
      assert.equal(await input.inputValue(), '', 'composer clears after send');
      assert.equal(await page.locator('[data-testid="chat-msg"]').count(), 7);
      const last = await page.locator('[data-testid="chat-msg"]').last().locator('.chat-text').textContent();
      assert.equal(last, 'first line\nsecond line');
      const pinned = await page.evaluate(() => {
        const list = document.querySelector('[data-testid="chat-list"]') as HTMLElement;
        return list.scrollHeight - list.scrollTop - list.clientHeight < 2;
      });
      assert.equal(pinned, true, 'the list follows the newest message');

      // Commands: `/` lists every command with its plugin; the list fits.
      const chat = () => page.evaluate(() => (window as unknown as { __chat: { sent: string[]; ran: string[] } }).__chat);
      await input.fill('/');
      const list = page.locator('[data-testid="chat-suggestions"]');
      await list.waitFor();
      const rows = await page.locator('[data-testid="chat-suggestion"]').evaluateAll((els) =>
        els.map((el) => ({ text: el.textContent?.replace(/\s+/g, ' ').trim(), overflowing: el.scrollWidth > el.clientWidth + 1 })),
      );
      assert.deepEqual(rows.map((r) => r.text), [
        '/tally Count hands for a quick decision in the meeting Tally for Teams Pro',
        '/timer 5m [label] | list | cancel Start a countdown everyone can see Timer',
      ]);
      assert.ok(rows.every((r) => !r.overflowing), `${width}: suggestion rows wrap, never overflow`);
      // Narrowing filters; Tab completes the highlighted one; Enter on the exact name runs it.
      await page.keyboard.type('ti');
      assert.equal(await page.locator('[data-testid="chat-suggestion"]').count(), 1);
      await page.keyboard.press('Tab');
      assert.equal(await input.inputValue(), '/timer ');
      assert.equal(await list.count(), 0, 'the list closes once arguments start');
      await page.keyboard.type('5m standup');
      await page.keyboard.press('Enter');
      assert.deepEqual((await chat()).ran, ['timer|5m standup']);
      assert.equal(await input.inputValue(), '', 'a command that ran clears the draft');
      assert.deepEqual((await chat()).sent, ['first line\nsecond line'], 'a command is never sent as a message');

      // A command the host refuses keeps the draft and says why.
      await input.fill('/tally');
      await page.keyboard.press('Enter');
      assert.deepEqual((await chat()).ran, ['timer|5m standup', 'tally|']);
      assert.equal(await input.inputValue(), '/tally');
      assert.equal(await page.locator('[data-testid="chat-error"]').textContent(), 'Tally for Teams Pro is still starting. Try again in a moment.');
      await page.keyboard.type(' ');
      assert.equal(await page.locator('[data-testid="chat-error"]').count(), 0, 'editing clears the refusal');

      // Unknown or malformed command: refused locally, never sent; // escapes.
      await input.fill('/usr/local/bin');
      await page.keyboard.press('Enter');
      assert.match((await page.locator('[data-testid="chat-error"]').textContent()) ?? '', /^\/usr\/local\/bin is not a command\. To send a message that starts with \/, type \/\/ first\.$/);
      const errorBox = await page.locator('[data-testid="chat-error"]').evaluate((el) => el.scrollWidth <= el.clientWidth + 1);
      assert.ok(errorBox, `${width}: the refusal wraps inside the drawer`);
      await input.fill('//usr/local/bin');
      await page.keyboard.press('Enter');
      assert.deepEqual((await chat()).sent, ['first line\nsecond line', '/usr/local/bin']);

      // Escape closes the suggestion list first, then the drawer.
      await input.fill('/t');
      await list.waitFor();
      await page.keyboard.press('Escape');
      assert.equal(await list.count(), 0);
      assert.equal(await page.evaluate(() => (window as unknown as { __closed: () => number }).__closed()), 0);

      // Over the limit: the counter warns and Send is disabled; Escape closes.
      await input.fill('x'.repeat(2001));
      assert.equal(await send.isDisabled(), true);
      assert.equal(await page.locator('.chat-count.over').textContent(), '-1');
      await page.keyboard.press('Escape');
      assert.equal(await page.evaluate(() => (window as unknown as { __closed: () => number }).__closed()), 1);
      await page.close();
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
