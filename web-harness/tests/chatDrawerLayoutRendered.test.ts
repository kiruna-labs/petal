// The browser meeting's chat drawer on a landscape phone (#246): the drawer
// column takes the full height beside the tiles (no tile under it) and #239's
// control rail, with Send beside the input and at least four lines of message
// text; #239's floating top bar stops short of it. With the soft keyboard up
// (#239's interactive-widget=resizes-content shrinks the page to what is left
// above it) the input stays in view and the rail, Leave included, stays too.
// Portrait and wide windows keep the drawer between the top bar and the
// control bar. On a touch screen, opening chat does not focus the input (that
// would raise the keyboard over the messages). Where the rail has no room for
// Chat, it is opened from the ⋯ menu (#247).
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
  controlbarShown: boolean;
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

/** Chat's own button -- or, on a rail too short for it (#247's ⋯ overflow on
 * #239's landscape rail), its row in the ⋯ menu. */
async function toggleChat(page: Page): Promise<void> {
  const inMenu = await page.evaluate(() => document.querySelector('#ctl-chat')!.closest('.control-cell')!.classList.contains('overflowed'));
  if (inMenu) {
    await page.locator('#ctl-more').click();
    await page.locator('#overflow-menu .overflow-menu-row >> text=Chat').click();
  } else {
    await page.locator('#ctl-chat').click();
  }
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
    // Chat's button, or ⋯ while Chat is in its menu (#247).
    const chatControl = document.querySelector('#ctl-chat')!.closest('.control-cell')!.classList.contains('overflowed') ? '#ctl-more' : '#ctl-chat';
    const [aside, topbar, controlbar, list, input, send, chatButton] = [
      '#chat-drawer',
      '.topbar',
      '.controlbar',
      '[data-testid="chat-list"]',
      '[data-testid="chat-input"]',
      '[data-testid="chat-send"]',
      chatControl
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
      controlbarShown: getComputedStyle(document.querySelector('.controlbar')!).display !== 'none',
      input,
      send,
      tiles,
      textLines,
      chatButtonClear: !!document.elementFromPoint((chatButton.left + chatButton.right) / 2, (chatButton.top + chatButton.bottom) / 2)?.closest(chatControl),
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
    await toggleChat(page);
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

    // Landscape, keyboard closed (#239's layout): the drawer column runs from
    // the top as far as the control rail beside it does -- the whole height
    // on a phone (below); this mouse-driven window keeps its developer row
    // under both -- between the tiles and the rail, with the floating top bar
    // stopping short of it.
    const phone = await layout(page);
    assert.equal(phone.aside.top, 0, 'the drawer starts at the top of the screen');
    assertNear(phone.aside.bottom, phone.controlbar.bottom, 'the drawer runs as far down as the rail');
    assertNear(phone.aside.right, phone.controlbar.left, 'the drawer sits beside the rail');
    assert.ok(phone.controlbar.bottom - phone.controlbar.top >= phone.aside.bottom - phone.aside.top - 0.5, 'the rail is full height');
    assert.equal(phone.aside.width, 320);
    assert.ok(phone.topbar.right <= phone.aside.left + 0.5, `the top bar (right ${phone.topbar.right}) stops short of the drawer (left ${phone.aside.left})`);
    assert.equal(phone.tiles.length, 4);
    for (const [i, tile] of phone.tiles.entries()) {
      assert.ok(tile.right <= phone.aside.left + 0.5, `tile ${i} (right ${tile.right}) is not under the drawer (left ${phone.aside.left})`);
    }
    assert.equal(phone.chatButtonClear, true, 'the Chat control stays reachable');
    assert.equal(phone.inputClear, true, 'nothing covers the input');
    assertBeside(phone, '800x360');
    assert.ok(phone.textLines >= 4, `at least four lines of message text visible, got ${phone.textLines}`);
    assert.equal(phone.pageScrollsX, false);

    // Soft keyboard up: the page shrinks to what is left above it. The rail
    // stays (Leave with it) and the input stays in view beside it.
    await page.setViewportSize({ width: 800, height: 172 });
    const keyboard = await layout(page);
    assert.ok(keyboard.input.top >= 0 && keyboard.input.bottom <= 172, `input in view with the keyboard up (${keyboard.input.top}-${keyboard.input.bottom})`);
    assert.equal(keyboard.controlbarShown, true, 'the rail stays with the keyboard up');
    assertNear(keyboard.aside.right, keyboard.controlbar.left, 'keyboard up: the drawer sits beside the rail');
    for (const [i, tile] of keyboard.tiles.entries()) {
      assert.ok(tile.right <= keyboard.aside.left + 0.5, `keyboard up: tile ${i} is not under the drawer`);
    }
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

    // Closing chat: the top bar spans the tiles again.
    await page.setViewportSize({ width: 800, height: 360 });
    await page.locator('.chat-close').click();
    assert.equal(await page.evaluate(() => (document.querySelector('#chat-drawer') as HTMLElement).hidden), true);
    assert.equal(await page.evaluate(() => getComputedStyle(document.querySelector('.topbar')!).display !== 'none'), true, 'the top bar is back');

    // The "Enable audio" prompt (connection.ts puts it in the top bar when
    // the browser blocks playback) stays on screen and tappable with chat open.
    await page.evaluate(() => {
      const prompt = document.createElement('button');
      prompt.className = 'audio-playback-prompt';
      prompt.textContent = 'Enable audio';
      document.querySelector('.topbar-right')!.prepend(prompt);
    });
    await toggleChat(page);
    const withPrompt = await layout(page);
    assert.equal(withPrompt.topbarShown, true, 'the top bar stays while it holds the Enable audio prompt');
    assert.ok(withPrompt.topbar.right <= withPrompt.aside.left + 0.5, 'and stays clear of the drawer');
    assert.equal(
      await page.evaluate(() => {
        const r = document.querySelector('.audio-playback-prompt')!.getBoundingClientRect();
        return document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2)?.closest('.audio-playback-prompt') !== null;
      }),
      true,
      'the Enable audio prompt is tappable with chat open'
    );
    await page.evaluate(() => document.querySelector('.audio-playback-prompt')!.remove());

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

    // A touch screen: opening chat leaves the keyboard down, and on a phone
    // (developer tools parked below the screen, #239) the drawer and the
    // rail both take the whole height.
    const phoneContext = await browser.newContext({ viewport: { width: 800, height: 360 }, hasTouch: true, isMobile: true });
    const touchPage: Page = await phoneContext.newPage();
    await openMeeting(touchPage, url);
    assert.equal(await touchPage.evaluate(() => matchMedia('(pointer: coarse)').matches), true);
    await toggleChat(touchPage);
    await touchPage.waitForFunction(() => !!document.querySelector('[data-testid="chat-input"]'));
    assert.equal(await composerFocusCalls(touchPage), 0, 'with a coarse pointer, opening chat does not focus the input');
    const touch = await layout(touchPage);
    assert.equal(touch.aside.top, 0, 'phone: the drawer starts at the top');
    assertNear(touch.aside.bottom, 360, 'phone: the drawer runs to the bottom of the screen');
    assertNear(touch.controlbar.bottom, 360, 'phone: so does the rail');
    assertNear(touch.aside.right, touch.controlbar.left, 'phone: the drawer sits beside the rail');
    for (const [i, tile] of touch.tiles.entries()) {
      assert.ok(tile.right <= touch.aside.left + 0.5, `phone: tile ${i} is not under the drawer`);
    }
    await phoneContext.close();
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
