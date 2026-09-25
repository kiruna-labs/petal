// The browser meeting's chat drawer on a landscape phone (#246): the body row
// between the top bar, the control bar and the dev tools is too short to read
// in, so while chat is open those step out and the drawer column takes the
// full height beside the tiles (no tile under it), with Send beside the input
// and at least four lines of message text. With the soft keyboard up (#239's
// interactive-widget=resizes-content shrinks the page to what is left above
// it) the control bar steps out too and the input stays in view. Closing chat
// brings everything back; portrait and wide windows keep today's layout. On a
// touch screen, opening chat does not focus the input (that would raise the
// keyboard over the messages).
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import test from 'node:test';
import { build } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

const repoRoot = resolve(import.meta.dirname, '../..');
const webRoot = resolve(repoRoot, 'web-harness');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

type Box = { left: number; right: number; top: number; bottom: number; width: number };
type Layout = {
  aside: Box;
  topbar: Box;
  topbarShown: boolean;
  controlbar: Box;
  input: Box;
  send: Box;
  tiles: Box[];
  textLines: number;
  chatButtonClear: boolean;
  inputClear: boolean;
  pageScrollsX: boolean;
};
type Page = {
  evaluate<R>(fn: () => R | Promise<R>): Promise<R>;
  goto(url: string, options?: object): Promise<unknown>;
  waitForFunction(fn: () => unknown): Promise<unknown>;
  setViewportSize(size: { width: number; height: number }): Promise<void>;
  locator(selector: string): { click(): Promise<void> };
};

/** Open the meeting screen with four tiles and a focus spy on the chat input. */
async function openMeeting(page: Page, url: string): Promise<void> {
  await page.goto(url, { waitUntil: 'load' });
  await page.waitForFunction(() => (window as unknown as { __petalHarness?: { chat?: unknown } }).__petalHarness?.chat);
  await page.evaluate(() => {
    document.querySelector('#meeting-screen')?.classList.remove('hidden');
    document.querySelector('#join-screen')?.classList.add('hidden');
    const tiles = document.querySelector('#tiles')!;
    for (let i = 0; i < 4; i++) {
      const tile = document.createElement('div');
      tile.className = 'tile';
      tiles.append(tile);
    }
    const w = window as unknown as { __composerFocus: number };
    w.__composerFocus = 0;
    const focus = HTMLTextAreaElement.prototype.focus;
    HTMLTextAreaElement.prototype.focus = function (this: HTMLTextAreaElement, options?: FocusOptions) {
      if (this.dataset.testid === 'chat-input') w.__composerFocus++;
      return focus.call(this, options);
    };
  });
}

function composerFocusCalls(page: Page): Promise<number> {
  return page.evaluate(() => (window as unknown as { __composerFocus: number }).__composerFocus);
}

function layout(page: Page): Promise<Layout> {
  return page.evaluate(async () => {
    // Let a resize settle (the list re-pins to the newest message on the
    // next frame) and the web fonts finish, so line counts are stable.
    await document.fonts.ready;
    await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
    // No named helpers in here: tsx would wrap them in a __name() the page lacks.
    const [aside, topbar, controlbar, list, input, send, chatButton] = [
      '#chat-drawer',
      '.topbar',
      '.controlbar',
      '[data-testid="chat-list"]',
      '[data-testid="chat-input"]',
      '[data-testid="chat-send"]',
      '#ctl-chat'
    ].map((selector) => {
      const { left, right, top, bottom, width } = document.querySelector(selector)!.getBoundingClientRect();
      return { left, right, top, bottom, width };
    });
    const tiles = [...document.querySelectorAll('.tiles > .tile')].map((tile) => {
      const { left, right, top, bottom, width } = tile.getBoundingClientRect();
      return { left, right, top, bottom, width };
    });
    let textLines = 0;
    for (const el of document.querySelectorAll('.chat-text')) {
      const range = document.createRange();
      range.selectNodeContents(el);
      const tops = [...range.getClientRects()]
        .filter((r) => r.height > 4 && r.top >= list.top - 1 && r.bottom <= list.bottom + 1)
        .map((r) => r.top)
        .sort((a, b) => a - b);
      let last = -Infinity;
      for (const top of tops) {
        if (top - last > 8) textLines++;
        last = top;
      }
    }
    return {
      aside,
      topbar,
      topbarShown: getComputedStyle(document.querySelector('.topbar')!).display !== 'none',
      controlbar,
      input,
      send,
      tiles,
      textLines,
      chatButtonClear: !!document.elementFromPoint((chatButton.left + chatButton.right) / 2, (chatButton.top + chatButton.bottom) / 2)?.closest('#ctl-chat'),
      inputClear: document.elementFromPoint(input.left + 12, (input.top + input.bottom) / 2) === document.querySelector('[data-testid="chat-input"]'),
      pageScrollsX: document.scrollingElement!.scrollWidth > window.innerWidth + 1
    };
  });
}

function assertBeside({ input, send }: Layout, label: string): void {
  assert.ok(send.left >= input.right - 1, `${label}: Send is right of the input`);
  assert.ok(send.top < input.bottom && send.bottom > input.top, `${label}: Send shares the input's row`);
}

function assertNear(actual: number, expected: number, label: string): void {
  assert.ok(Math.abs(actual - expected) < 0.5, `${label}: ${actual} vs ${expected}`);
}

test('browser chat drawer takes the full height beside the tiles on a landscape phone', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-browser-chat-layout-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;

  try {
    await build({
      root: webRoot,
      configFile: false,
      logLevel: 'silent',
      base: './',
      plugins: [svelte()],
      define: {
        __PETAL_BUILD_INFO__: JSON.stringify({ version: 'test', commit: 'test', buildDate: '2099-01-01' }),
        'import.meta.env.VITE_SENTRY_DSN': JSON.stringify('')
      },
      resolve: { alias: { '@petal/shared': resolve(repoRoot, 'shared') } },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: resolve(webRoot, 'index.html') }
      }
    });
    const url = pathToFileURL(join(buildDir, 'index.html')).href;

    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files']
    });
    const page: Page = await browser.newPage({ viewport: { width: 800, height: 360 } });
    await openMeeting(page, url);

    // Open chat (a mouse user gets the caret) and fill it the way a peer would.
    await page.locator('#ctl-chat').click();
    assert.equal(await composerFocusCalls(page), 1, 'with a fine pointer, opening chat focuses the input');
    await page.evaluate(() => {
      type Hook = { chat: { onData(payload: Uint8Array, participant: unknown, identity: string): void } };
      const { chat } = (window as unknown as { __petalHarness: Hook }).__petalHarness;
      const encoder = new TextEncoder();
      for (let i = 0; i < 20; i++) {
        const text = i % 5 === 4 ? `Message ${i}: a longer one that wraps onto a second line in the drawer column` : `Message ${i}`;
        const wire = { v: 1, type: 'msg', id: `m-layout-${String(i).padStart(4, '0')}`, text, t: Date.now() - (20 - i) * 1000 };
        chat.onData(encoder.encode(JSON.stringify(wire)), { identity: `peer-${i % 3}`, name: ['Mira', 'Theo', 'Ada'][i % 3] }, `peer-${i % 3}`);
      }
    });
    await page.waitForFunction(() => document.querySelectorAll('[data-testid="chat-msg"]').length === 20);

    // Landscape phone, keyboard closed: top bar and dev tools out, the
    // drawer column runs from the top to the control bar, beside the tiles.
    const phone = await layout(page);
    assert.equal(phone.topbarShown, false, 'the top bar steps out while chat is open');
    assert.equal(phone.aside.top, 0);
    assertNear(phone.aside.bottom, phone.controlbar.top, 'the drawer stops at the control bar');
    assert.equal(phone.aside.right, 800);
    assert.equal(phone.aside.width, 320);
    assert.equal(phone.tiles.length, 4);
    for (const [i, tile] of phone.tiles.entries()) {
      assert.ok(tile.right <= phone.aside.left + 0.5, `tile ${i} (right ${tile.right}) is not under the drawer (left ${phone.aside.left})`);
    }
    assert.equal(phone.chatButtonClear, true, 'the Chat control stays reachable');
    assert.equal(phone.inputClear, true, 'nothing covers the input');
    assertBeside(phone, '800x360');
    assert.ok(phone.textLines >= 4, `at least four lines of message text visible, got ${phone.textLines}`);
    assert.equal(phone.pageScrollsX, false);

    // Soft keyboard up: the page shrinks to what is left above it.
    await page.setViewportSize({ width: 800, height: 172 });
    const keyboard = await layout(page);
    assert.ok(keyboard.input.top >= 0 && keyboard.input.bottom <= 172, `input in view with the keyboard up (${keyboard.input.top}-${keyboard.input.bottom})`);
    assert.ok(keyboard.send.bottom <= 172);
    assert.equal(keyboard.inputClear, true, 'nothing covers the input');
    assertBeside(keyboard, 'keyboard up');
    assert.ok(keyboard.textLines >= 2, `message text still readable above the input, got ${keyboard.textLines}`);
    // A two-line draft shows both lines whole, even this short.
    const draft = await page.evaluate(async () => {
      const input = document.querySelector('[data-testid="chat-input"]') as HTMLTextAreaElement;
      input.value = 'first line\nsecond line';
      await new Promise((done) => requestAnimationFrame(done));
      const clipped = input.scrollHeight > input.clientHeight + 1;
      input.value = '';
      return { clipped, bottom: input.getBoundingClientRect().bottom };
    });
    assert.equal(draft.clipped, false, 'a two-line draft is not clipped with the keyboard up');

    // Closing chat brings the top bar back.
    await page.setViewportSize({ width: 800, height: 360 });
    await page.locator('.chat-close').click();
    assert.equal(await page.evaluate(() => (document.querySelector('#chat-drawer') as HTMLElement).hidden), true);
    assert.equal(await page.evaluate(() => getComputedStyle(document.querySelector('.topbar')!).display !== 'none'), true, 'the top bar is back');

    // The top bar stays while it holds the "Enable audio" prompt
    // (connection.ts puts it there when the browser blocks playback).
    await page.evaluate(() => {
      const prompt = document.createElement('button');
      prompt.className = 'audio-playback-prompt';
      prompt.textContent = 'Enable audio';
      document.querySelector('.topbar-right')!.prepend(prompt);
    });
    await page.locator('#ctl-chat').click();
    assert.equal((await layout(page)).topbarShown, true, 'the top bar stays while it holds the Enable audio prompt');
    await page.evaluate(() => document.querySelector('.audio-playback-prompt')!.remove());
    assert.equal((await layout(page)).topbarShown, false);

    // Portrait (keyboard up) and wide windows: unchanged, the drawer sits
    // between the top bar and the control bar.
    for (const [width, height] of [[375, 400], [1280, 800]]) {
      await page.setViewportSize({ width, height });
      const other = await layout(page);
      assert.equal(other.topbarShown, true, `${width}x${height}: top bar stays`);
      assertNear(other.aside.top, other.topbar.bottom, `${width}x${height}: drawer starts under the top bar`);
      assertNear(other.aside.bottom, other.controlbar.top, `${width}x${height}: drawer stops at the control bar`);
      assert.equal(other.aside.width, width > 640 ? 320 : width);
      assert.equal(other.inputClear, true, `${width}x${height}: nothing covers the input`);
      assertBeside(other, `${width}x${height}`);
    }

    // A touch screen: opening chat leaves the keyboard down.
    const phoneContext = await browser.newContext({ viewport: { width: 800, height: 360 }, hasTouch: true, isMobile: true });
    const touchPage: Page = await phoneContext.newPage();
    await openMeeting(touchPage, url);
    assert.equal(await touchPage.evaluate(() => matchMedia('(pointer: coarse)').matches), true);
    await touchPage.locator('#ctl-chat').click();
    await touchPage.waitForFunction(() => !!document.querySelector('[data-testid="chat-input"]'));
    assert.equal(await composerFocusCalls(touchPage), 0, 'with a coarse pointer, opening chat does not focus the input');

    // A message that arrives while chat is closed shows a toast; opening chat
    // shows that message in the drawer, so its toast goes instead of sitting
    // on the composer.
    await touchPage.locator('#ctl-chat').click();
    await touchPage.waitForFunction(() => !document.querySelector('[data-testid="chat-input"]'));
    await touchPage.evaluate(() => {
      const hook = (window as unknown as { __petalHarness: { chat: { onData(p: Uint8Array, participant: unknown, identity: string): void } } }).__petalHarness.chat;
      const wire = { v: 1, type: 'msg', id: 'a'.repeat(32), text: 'Can we zoom into the timeline table?', t: Date.now() };
      hook.onData(new TextEncoder().encode(JSON.stringify(wire)), { identity: 'bob', name: 'Bob Okafor' }, 'bob');
    });
    await touchPage.waitForFunction(() => !document.querySelector('#toast')!.classList.contains('hidden'));
    assert.match(await touchPage.evaluate(() => document.querySelector('#toast')!.textContent ?? ''), /Bob Okafor: Can we zoom/);
    await touchPage.locator('#ctl-chat').click();
    await touchPage.waitForFunction(() => !!document.querySelector('[data-testid="chat-input"]'));
    assert.equal(await touchPage.evaluate(() => document.querySelector('#toast')!.classList.contains('hidden')), true, 'opening chat takes the message toast down');
    await phoneContext.close();
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
