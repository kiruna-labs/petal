// MEASURES the meeting control bar in a real browser, with the real stylesheet,
// the real UI font and the built-in Reactions plugin's React button (#247):
// at phone widths no two buttons overlap, none is squashed or off the bar,
// Mic, Camera and Leave stay, and exactly the controls that left the bar are
// in the ⋯ menu, carrying their state. With room for everything there is no ⋯.
// A stand-in for #239's landscape rail proves the same along the bar's height.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import test, { after, before } from 'node:test';
import { build } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

const repoRoot = resolve(import.meta.dirname, '../..');
const webRoot = resolve(repoRoot, 'web-harness');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

type Browser = Awaited<ReturnType<typeof chromium.launch>>;
type Page = Awaited<ReturnType<Browser['newPage']>>;

/** Lowest priority first: the order the issue fixes for giving way. */
const COLLAPSE_PRIORITY = ['React', 'Draw', 'Invite', 'Chat', 'Share'];
const PINNED = ['Mic', 'Camera', 'Leave'];

// #239's landscape-phone rail, cut down to the rules that shape the bar: a
// column on the right edge whose height is the meeting's, icon-only, with a
// Full screen cell that only the rail shows. A stand-in until #239 lands; the
// query is its MEETING_RAIL_QUERY.
const RAIL_STAND_IN = `.control-cell.fullscreen-cell { display: none; }
@media (orientation: landscape) and (max-height: 500px) {
  .control-cell.fullscreen-cell { display: flex; }
  .meeting { display: grid; grid-template-columns: minmax(0, 1fr) auto; grid-template-rows: minmax(0, 1fr); }
  .meeting > .topbar, .meeting > .meeting-body { grid-area: 1 / 1; }
  .topbar { align-self: start; z-index: 7; }
  .dev-panel { position: absolute; top: 100%; left: 0; right: 0; }
  .controlbar { grid-area: 1 / 2; flex-direction: column; align-items: center; gap: 6px; min-height: 0;
    padding: 8px 4px; border-top: 0; border-left: 1px solid var(--hairline); }
  .controls-left { flex-direction: column; align-items: center; gap: 6px; min-height: 0; margin-bottom: auto; }
  .leave-cell { margin-left: 0; }
  .controlbar .meeting-control-label { display: none; }
  .controlbar .meeting-split { flex-direction: column; }
  .controlbar .meeting-split-options { flex: 0 0 22px; width: var(--control-size); min-width: 0; min-height: 0;
    border-left: 0; border-top: 1px solid var(--hairline); }
}`;

// #240's rule for a control hidden as unsupported (Share without
// getDisplayMedia). A stand-in until #240 lands.
const UNSUPPORTED_STAND_IN = '.control-cell[hidden] { display: none !important; }';

interface Box {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

interface BarState {
  vertical: boolean;
  bar: Box;
  /** Cell labels in the bar, in bar order. */
  shown: string[];
  /** Cell labels that are not rendered, in bar order. */
  hidden: string[];
  overlaps: string[];
  outside: string[];
  squashed: string[];
  moreShown: boolean;
  dot: boolean;
  dotColor: string;
  moreLabel: string | null;
}

interface MenuRow {
  label: string;
  ariaLabel: string | null;
  role: string | null;
  checked: string | null;
  disabled: string | null;
  haspopup: string | null;
  note: string | null;
  state: string | null;
  stateBackground: string | null;
}

let buildDir = '';
let browser: Browser | undefined;

before(async () => {
  buildDir = await mkdtemp(join(tmpdir(), 'petal-browser-control-overflow-build-'));
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
  browser = await chromium.launch({
    headless: true,
    args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files']
  });
}, { timeout: 120_000 });

after(async () => {
  await browser?.close();
  if (buildDir) await rm(buildDir, { recursive: true, force: true });
});

/** Two frames: the bar re-fits on the frame after a resize. */
async function settle(page: Page): Promise<void> {
  await page.evaluate(() => new Promise<void>((done) => requestAnimationFrame(() => requestAnimationFrame(() => done()))));
}

async function openMeeting(
  viewport: { width: number; height: number },
  css: string[] = [],
  device: { hasTouch?: boolean; isMobile?: boolean } = {}
): Promise<{ page: Page; errors: string[]; close: () => Promise<void> }> {
  const context = await browser!.newContext({ viewport, ...device });
  // A "ResizeObserver loop completed with undelivered notifications" is only
  // an `error` event on the window: neither a page error nor a console line.
  await context.addInitScript({
    content: "window.__windowErrors = []; addEventListener('error', (event) => window.__windowErrors.push(String(event.message)));",
  });
  const page = await context.newPage();
  const errors: string[] = [];
  page.on('pageerror', (err: Error) => errors.push(err.message));
  page.on('console', (message: { type(): string; text(): string }) => {
    if (message.type() === 'error') errors.push(message.text());
  });
  await page.goto(pathToFileURL(join(buildDir, 'index.html')).href, { waitUntil: 'load' });
  await page.waitForSelector('#ctl-more', { state: 'attached' });
  // The built-in Reactions plugin's React cell: the bar a user really gets.
  await page.waitForSelector('.plugin-control-cell', { state: 'attached' });
  for (const content of css) await page.addStyleTag({ content });
  await page.evaluate(async () => {
    document.querySelector('#meeting-screen')!.classList.remove('hidden');
    document.querySelector('#join-screen')!.classList.add('hidden');
    // Labels measured in a fallback font would understate the bar.
    await document.fonts.ready;
  });
  assert.equal(await page.evaluate(() => document.fonts.check('600 11px "Albert Sans"')), true, 'the real UI font loaded');
  await settle(page);
  return { page, errors, close: () => context.close() };
}

// Page functions below use no named inner functions: tsx compiles this file
// with `keepNames`, whose `__name` helper does not exist inside the page.
function readBar(): BarState {
  const bar = document.querySelector<HTMLElement>('.controlbar')!;
  const barBox = bar.getBoundingClientRect();
  const cells = Array.from(bar.querySelectorAll('.control-cell'))
    .filter((cell) => !cell.classList.contains('overflow-cell'))
    .map((cell) => ({
      label: cell.querySelector('.meeting-control-label')?.textContent?.trim() ?? '?',
      rendered: cell.getClientRects().length > 0,
    }));
  const buttons = Array.from(bar.querySelectorAll<HTMLButtonElement>('button'))
    .filter((button) => button.getClientRects().length > 0)
    .map((button) => ({
      name: `${button.closest('.control-cell')?.querySelector('.meeting-control-label')?.textContent?.trim()}${
        button.classList.contains('control-button') ? '' : ' options'
      }`,
      box: button.getBoundingClientRect(),
      control: button.classList.contains('control-button'),
    }));
  const overlaps: string[] = [];
  for (let i = 0; i < buttons.length; i++) {
    for (let j = i + 1; j < buttons.length; j++) {
      const a = buttons[i].box;
      const b = buttons[j].box;
      const x = Math.min(a.right, b.right) - Math.max(a.left, b.left);
      const y = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
      if (x > 0.5 && y > 0.5) overlaps.push(`${buttons[i].name} x ${buttons[j].name}`);
    }
  }
  const outside = buttons
    .filter(
      ({ box }) =>
        box.left < Math.max(0, barBox.left) - 0.5 ||
        box.top < Math.max(0, barBox.top) - 0.5 ||
        box.right > Math.min(innerWidth, barBox.right) + 0.5 ||
        box.bottom > Math.min(innerHeight, barBox.bottom) + 0.5
    )
    .map(({ name }) => name);
  const squashed = buttons
    .filter(({ box, control }) => control && (box.width < 43.5 || box.height < 43.5))
    .map(({ name }) => name);
  const more = document.querySelector<HTMLElement>('#ctl-more')!;
  return {
    vertical: getComputedStyle(bar).flexDirection === 'column',
    bar: { left: barBox.left, top: barBox.top, right: barBox.right, bottom: barBox.bottom },
    shown: cells.filter((cell) => cell.rendered).map((cell) => cell.label),
    hidden: cells.filter((cell) => !cell.rendered).map((cell) => cell.label),
    overlaps,
    outside,
    squashed,
    moreShown: more.getClientRects().length > 0,
    dot: document.querySelector('.overflow-dot')!.getClientRects().length > 0,
    dotColor: getComputedStyle(document.querySelector('.overflow-dot')!).backgroundColor,
    moreLabel: more.getAttribute('aria-label'),
  };
}

function readMenu(): MenuState {
  const menu = document.querySelector<HTMLElement>('#overflow-menu')!;
  const rows = Array.from(menu.querySelectorAll<HTMLElement>('.overflow-menu-row')).map((row) => {
    const state = row.querySelector<HTMLElement>('.overflow-menu-state');
    return {
      label: row.querySelector('.meeting-menu-row-copy')?.firstChild?.textContent?.trim() ?? '',
      ariaLabel: row.getAttribute('aria-label'),
      role: row.getAttribute('role'),
      checked: row.getAttribute('aria-checked'),
      disabled: row.getAttribute('aria-disabled'),
      haspopup: row.getAttribute('aria-haspopup'),
      note: row.querySelector('.overflow-menu-note')?.textContent?.trim() ?? null,
      state: state?.textContent?.trim() ?? null,
      stateBackground: state ? getComputedStyle(state).backgroundColor : null,
    };
  });
  const r = menu.hidden ? null : menu.getBoundingClientRect();
  return {
    rows,
    box: r && { left: r.left, top: r.top, right: r.right, bottom: r.bottom },
    placed: menu.classList.contains('placed'),
  };
}

interface MenuState {
  rows: MenuRow[];
  box: Box | null;
  placed: boolean;
}

const barOf = (page: Page): Promise<BarState> => page.evaluate(readBar);
const menuOf = (page: Page): Promise<MenuState> => page.evaluate(readMenu);

async function openMenu(page: Page): Promise<MenuState> {
  await page.click('#ctl-more');
  await page.waitForSelector('#overflow-menu.placed');
  return menuOf(page);
}

function assertFits(state: BarState, where: string): void {
  assert.deepEqual(state.overlaps, [], `${where}: buttons overlap`);
  assert.deepEqual(state.outside, [], `${where}: buttons outside the bar or the viewport`);
  assert.deepEqual(state.squashed, [], `${where}: buttons squashed below 44px`);
  for (const label of PINNED) assert.ok(state.shown.includes(label), `${where}: ${label} must stay in the bar (${JSON.stringify(state)})`);
}

function assertInside(box: Box | null, viewport: { width: number; height: number }, where: string): asserts box is Box {
  assert.ok(box, `${where}: menu not open`);
  assert.ok(box.left >= 0 && box.top >= 0 && box.right <= viewport.width && box.bottom <= viewport.height, `${where}: menu off screen ${JSON.stringify(box)}`);
}

test('at 320, 360 and 412 px the bar fits: no overlap, no squash, and exactly the hidden controls are in the ⋯ menu', { timeout: 60_000 }, async () => {
  const expected: Record<number, string[]> = {
    // Lowest priority first, and only as many as the width needs.
    320: ['React', 'Draw', 'Invite', 'Chat', 'Share'],
    360: ['React', 'Draw', 'Invite', 'Chat'],
    412: ['React', 'Draw', 'Invite'],
  };
  for (const viewport of [{ width: 320, height: 568 }, { width: 360, height: 780 }, { width: 412, height: 915 }]) {
    const where = `${viewport.width}px`;
    const { page, errors, close } = await openMeeting(viewport);
    try {
      const state = await barOf(page);
      assert.equal(state.vertical, false);
      assertFits(state, where);
      assert.equal(state.moreShown, true, `${where}: ⋯ must show`);
      assert.deepEqual(
        [...state.hidden].sort((a, b) => COLLAPSE_PRIORITY.indexOf(a) - COLLAPSE_PRIORITY.indexOf(b)),
        expected[viewport.width],
        `${where}: wrong controls hidden`
      );
      const menu = await openMenu(page);
      // Every hidden control, in bar order, and nothing else.
      assert.deepEqual(menu.rows.map((row) => row.label), state.hidden, `${where}: menu rows`);
      assertInside(menu.box, viewport, where);
      assert.ok(menu.box.bottom <= state.bar.top + 0.5, `${where}: the menu opens above the bar, not over its buttons`);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  }
});

test('menu rows follow the order the bar shows, not the DOM (#239 lifts Chat with CSS `order`)', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 360, height: 780 }, [
    '.controls-left > .control-cell:has(> .meeting-split) { order: -2; } .controls-left > .chat-cell { order: -1; }',
  ]);
  try {
    const state = await barOf(page);
    assertFits(state, 'reordered');
    // What collapses is still decided by priority, not position.
    assert.deepEqual(state.hidden, ['Invite', 'Draw', 'Chat', 'React']);
    assert.deepEqual((await openMenu(page)).rows.map((row) => row.label), ['Chat', 'Invite', 'Draw', 'React']);
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('with room for every control there is no ⋯: at 1280, and at 500 -- measured, not a phone breakpoint', { timeout: 60_000 }, async () => {
  for (const viewport of [{ width: 1280, height: 800 }, { width: 500, height: 800 }]) {
    const { page, errors, close } = await openMeeting(viewport);
    try {
      const state = await barOf(page);
      assertFits(state, `${viewport.width}px`);
      assert.equal(state.moreShown, false, `${viewport.width}px: no ⋯ when everything fits`);
      assert.deepEqual(state.hidden, []);
      assert.deepEqual(state.shown, ['Mic', 'Camera', 'Share', 'Invite', 'Draw', 'Chat', 'React', 'Leave']);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  }
});

test('the menu carries each hidden control\'s state, and a dot on ⋯ flags a hidden one that needs attention', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 320, height: 568 });
  try {
    let state = await barOf(page);
    assert.equal(state.dot, false);
    assert.equal(state.moreLabel, 'More controls');

    // What uiHelpers.ts's setShareControl writes for a live share: a share
    // that went into the menu must still show in the bar.
    await page.evaluate(() => {
      const share = document.querySelector<HTMLElement>('#ctl-share')!;
      share.style.setProperty('--control-live-bg', 'rgb(90, 140, 250)');
      share.style.setProperty('--control-live-fg', 'rgb(10, 10, 10)');
      share.classList.add('live');
      share.setAttribute('aria-pressed', 'true');
      share.setAttribute('aria-label', 'Stop sharing your screen');
    });
    await settle(page);
    state = await barOf(page);
    assert.equal(state.dot, true, 'a live share in the menu lights the dot');
    assert.equal(state.dotColor, 'rgb(90, 140, 250)', "in the sharer's identity color");
    assert.equal(state.moreLabel, 'More controls, sharing your screen');

    // What chat/setupChat.svelte.ts's renderControl writes for 3 unread.
    await page.evaluate(() => {
      const badge = document.querySelector<HTMLElement>('#ctl-chat-badge')!;
      badge.hidden = false;
      badge.textContent = '3';
      document.querySelector('#ctl-chat')!.setAttribute('aria-label', 'Open chat, 3 unread');
    });
    await settle(page);
    state = await barOf(page);
    assert.equal(state.moreLabel, 'More controls, sharing your screen, new activity');

    let menu = await openMenu(page);
    const row = (label: string) => menu.rows.find((r) => r.label === label)!;
    assert.deepEqual(
      { ...row('Chat'), stateBackground: undefined },
      { label: 'Chat', ariaLabel: 'Open chat, 3 unread', role: 'menuitemcheckbox', checked: 'false', disabled: null, haspopup: null, note: null, state: '3', stateBackground: undefined }
    );
    assert.deepEqual(
      { ...row('Share'), stateBackground: undefined },
      // Led by the feature's name: "Stop sharing your screen" does not say "Share".
      { label: 'Share', ariaLabel: 'Share, Stop sharing your screen', role: 'menuitemcheckbox', checked: 'true', disabled: null, haspopup: null, note: null, state: 'Live', stateBackground: undefined }
    );
    assert.equal(row('Share').stateBackground, 'rgb(90, 140, 250)', 'live in the sharer\'s identity color');
    assert.equal(row('Draw').disabled, 'true');
    assert.equal(row('Draw').note, 'Nothing to draw on', 'Draw says why it is unavailable');
    assert.equal(row('Draw').ariaLabel, 'Draw, Nothing to draw on', 'and its name says which control it is');
    assert.equal(row('Invite').role, 'menuitem');
    assert.equal(row('Invite').ariaLabel, 'Copy invite link');
    assert.equal(row('React').role, 'menuitem');
    assert.equal(row('React').haspopup, 'dialog', "the plugin button's popover, announced on its row");

    // State that changes while the menu is open shows up in it.
    await page.evaluate(() => {
      const badge = document.querySelector<HTMLElement>('.plugin-control-cell .plugin-control-badge')!;
      badge.hidden = false;
      badge.textContent = '2';
      const chatBadge = document.querySelector<HTMLElement>('#ctl-chat-badge')!;
      chatBadge.hidden = true;
    });
    await settle(page);
    menu = await menuOf(page);
    assert.equal(menu.rows.find((r) => r.label === 'React')!.state, '2');
    assert.equal(menu.rows.find((r) => r.label === 'Chat')!.state, null);
    assert.equal((await barOf(page)).moreLabel, 'More controls, sharing your screen, new activity', 'the plugin badge is new activity too');
    await page.evaluate(() => {
      document.querySelector<HTMLElement>('.plugin-control-cell .plugin-control-badge')!.hidden = true;
      const share = document.querySelector<HTMLElement>('#ctl-share')!;
      share.classList.remove('live');
      share.setAttribute('aria-pressed', 'false');
      share.setAttribute('aria-label', 'Share your screen');
    });
    await settle(page);
    state = await barOf(page);
    assert.equal(state.dot, false, 'nothing left needing attention');
    assert.equal(state.moreLabel, 'More controls');

    // A disabled row does nothing, and the menu stays.
    // `force`: Playwright itself will not click an aria-disabled element.
    await page.click('#overflow-menu .overflow-menu-row[aria-disabled="true"]', { force: true });
    assert.equal(await page.evaluate(() => document.querySelector<HTMLButtonElement>('#ctl-draw')!.getAttribute('aria-pressed')), 'false');
    assert.ok((await menuOf(page)).box, 'the menu stays open');
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }

  // A badge on a control still in the bar is its own signal: no dot.
  const wide = await openMeeting({ width: 412, height: 915 });
  try {
    await wide.page.evaluate(() => {
      const badge = document.querySelector<HTMLElement>('#ctl-chat-badge')!;
      badge.hidden = false;
      badge.textContent = '1';
    });
    await settle(wide.page);
    const state = await barOf(wide.page);
    assert.ok(state.shown.includes('Chat'));
    assert.equal(state.dot, false);
  } finally {
    await wide.close();
  }
});

test('the ⋯ menu works from the keyboard like the desktop\'s: first row focused, arrows wrap, Escape returns focus', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 360, height: 780 });
  const focused = () =>
    page.evaluate(() => {
      const el = document.activeElement as HTMLElement | null;
      return el?.id || el?.querySelector('.meeting-menu-row-copy')?.firstChild?.textContent?.trim() || el?.tagName || null;
    });
  try {
    await page.focus('#ctl-more');
    await page.keyboard.press('Enter');
    await page.waitForSelector('#overflow-menu.placed');
    assert.equal(await page.getAttribute('#ctl-more', 'aria-expanded'), 'true');
    // 360: Invite, Draw, Chat, React, in bar order.
    assert.equal(await focused(), 'Invite');
    await page.keyboard.press('ArrowDown');
    assert.equal(await focused(), 'Draw', 'a disabled row is still reachable');
    await page.keyboard.press('ArrowUp');
    await page.keyboard.press('ArrowUp');
    assert.equal(await focused(), 'React', 'ArrowUp wraps to the last row');
    await page.keyboard.press('ArrowDown');
    assert.equal(await focused(), 'Invite', 'ArrowDown wraps to the first row');
    await page.keyboard.press('End');
    assert.equal(await focused(), 'React');
    await page.keyboard.press('Home');
    assert.equal(await focused(), 'Invite');
    await page.keyboard.press('Escape');
    assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true);
    assert.equal(await focused(), 'ctl-more', 'Escape returns focus to ⋯');
    assert.equal(await page.getAttribute('#ctl-more', 'aria-expanded'), 'false');

    // A row does what the hidden control does: Chat opens the chat drawer.
    await page.keyboard.press('Enter');
    await page.waitForSelector('#overflow-menu.placed');
    await page.keyboard.press('ArrowDown');
    await page.keyboard.press('ArrowDown');
    assert.equal(await focused(), 'Chat');
    await page.keyboard.press('Enter');
    assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true);
    assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#chat-drawer')!.hidden), false);
    assert.equal(await page.getAttribute('#ctl-chat', 'aria-pressed'), 'true');

    // Tabbing out of the menu closes it; so does a click outside.
    await page.focus('#ctl-more');
    await page.keyboard.press('Enter');
    await page.waitForSelector('#overflow-menu.placed');
    await page.keyboard.press('End');
    await page.keyboard.press('Tab');
    assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true);
    await page.click('#ctl-more');
    await page.waitForSelector('#overflow-menu.placed');
    await page.mouse.click(180, 200);
    assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true);
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('a plugin popover opened from the menu anchors to ⋯, not to its hidden button', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 360, height: 780 });
  try {
    await openMenu(page);
    await page.click('#overflow-menu .overflow-menu-row >> text=React');
    await page.waitForSelector('.petal-plugin-popover');
    const [popover, more] = await page.evaluate(() =>
      ['.petal-plugin-popover', '#ctl-more'].map((selector) => {
        const r = document.querySelector(selector)!.getBoundingClientRect();
        return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
      })
    );
    // placePopover: above the anchor, 8px clear of it.
    assert.ok(Math.abs(more.top - 8 - popover.bottom) <= 1, `popover ${JSON.stringify(popover)} not above ⋯ ${JSON.stringify(more)}`);
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('a menu row clicks its control inside the user\'s own click, so the user activation getDisplayMedia and full screen need carries over', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 320, height: 568 });
  try {
    await page.evaluate(() => {
      const w = window as unknown as { inRowClick: boolean; shareClicks: Array<{ active: boolean; inRowClick: boolean }> };
      w.inRowClick = false;
      w.shareClicks = [];
      // True only while a row's own click is being dispatched: set on the
      // way down, cleared on the way back up.
      document.addEventListener('click', (event) => {
        if ((event.target as Element).closest('.overflow-menu-row')) w.inRowClick = true;
      }, true);
      document.addEventListener('click', () => {
        w.inRowClick = false;
      });
      document.querySelector('#ctl-share')!.addEventListener('click', () => {
        w.shareClicks.push({ active: navigator.userActivation.isActive, inRowClick: w.inRowClick });
      }, true);
    });
    await openMenu(page);
    await page.click('#overflow-menu .overflow-menu-row >> text=Share');
    assert.deepEqual(
      await page.evaluate(() => (window as unknown as { shareClicks: unknown[] }).shareClicks),
      [{ active: true, inRowClick: true }]
    );
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('focus never falls to the page: a control leaving the bar hands it to ⋯, and an emptied menu to the control back in the bar', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 1280, height: 800 });
  const focused = () => page.evaluate(() => document.activeElement?.id || document.activeElement?.tagName);
  try {
    await page.focus('#ctl-invite');
    await page.setViewportSize({ width: 360, height: 780 });
    await settle(page);
    assert.ok((await barOf(page)).hidden.includes('Invite'));
    assert.equal(await focused(), 'ctl-more', 'Invite went into the menu: ⋯ holds focus');

    // 360: Invite, Draw, Chat, React. With Invite's row focused, widen until
    // everything fits and the menu has nothing left.
    await page.keyboard.press('Enter');
    await page.waitForSelector('#overflow-menu.placed');
    await page.setViewportSize({ width: 1280, height: 800 });
    await settle(page);
    assert.equal((await barOf(page)).moreShown, false);
    assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true);
    assert.equal(await focused(), 'ctl-invite', "the emptied menu hands focus to Invite's own button");

    // Draw's row: its button is disabled and cannot take focus, so the first
    // control of the bar does.
    await page.setViewportSize({ width: 360, height: 780 });
    await settle(page);
    await page.focus('#ctl-more');
    await page.keyboard.press('Enter');
    await page.waitForSelector('#overflow-menu.placed');
    await page.keyboard.press('ArrowDown');
    await page.setViewportSize({ width: 1280, height: 800 });
    await settle(page);
    assert.equal(await focused(), 'ctl-audio');
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('the menu closes with the meeting screen', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 360, height: 780 });
  try {
    await openMenu(page);
    await page.evaluate(() => {
      document.querySelector('#meeting-screen')!.classList.add('hidden');
      document.querySelector('#join-screen')!.classList.remove('hidden');
    });
    await settle(page);
    assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true);
    assert.equal(await page.getAttribute('#ctl-more', 'aria-expanded'), 'false');
    // Back in a meeting: fitted again, the menu still closed.
    await page.evaluate(() => {
      document.querySelector('#meeting-screen')!.classList.remove('hidden');
      document.querySelector('#join-screen')!.classList.add('hidden');
    });
    await settle(page);
    const state = await barOf(page);
    assertFits(state, 'back in the meeting');
    assert.equal(state.moreShown, true);
    assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true);
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('on a touch screen the tap that dismisses the menu lands nowhere else (not on Leave), and rows are 44px', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 360, height: 780 }, [], { hasTouch: true, isMobile: true });
  try {
    assert.equal(await page.evaluate(() => matchMedia('(pointer: coarse)').matches), true, 'the emulation is a coarse pointer');
    await page.evaluate(() => {
      const w = window as unknown as { leaveClicks: number };
      w.leaveClicks = 0;
      document.querySelector('#ctl-leave')!.addEventListener('click', () => {
        w.leaveClicks += 1;
      }, true);
    });
    await page.tap('#ctl-more');
    await page.waitForSelector('#overflow-menu.placed');
    const heights: number[] = await page.evaluate(() =>
      Array.from(document.querySelectorAll('#overflow-menu .overflow-menu-row'), (row) => row.getBoundingClientRect().height)
    );
    assert.ok(heights.length > 0 && heights.every((h) => h >= 44), `rows ${JSON.stringify(heights)} under 44px`);
    const leave = await page.evaluate(() => {
      const r = document.querySelector('#ctl-leave')!.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    });
    await page.touchscreen.tap(leave.x, leave.y);
    await settle(page);
    assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true, 'the tap closed the menu');
    assert.equal(await page.evaluate(() => (window as unknown as { leaveClicks: number }).leaveClicks), 0, 'and did not also press Leave');
    // With the menu closed, the same tap is Leave's again.
    await page.touchscreen.tap(leave.x, leave.y);
    assert.equal(await page.evaluate(() => (window as unknown as { leaveClicks: number }).leaveClicks), 1);
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('a window resize is fitted before the frame is drawn, and same-value attribute rewrites cost no re-fit', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 1280, height: 800 });
  try {
    // A resize listener added after the page's own sees the bar as that
    // frame will be painted.
    await page.evaluate(() => {
      const w = window as unknown as { atResize: { more: boolean; overlaps: number } | null };
      w.atResize = null;
      addEventListener('resize', () => {
        const boxes = Array.from(document.querySelectorAll('.controlbar button'))
          .filter((button) => button.getClientRects().length > 0)
          .map((button) => button.getBoundingClientRect());
        let overlaps = 0;
        for (let i = 0; i < boxes.length; i++) {
          for (let j = i + 1; j < boxes.length; j++) {
            const x = Math.min(boxes[i].right, boxes[j].right) - Math.max(boxes[i].left, boxes[j].left);
            const y = Math.min(boxes[i].bottom, boxes[j].bottom) - Math.max(boxes[i].top, boxes[j].top);
            if (x > 0.5 && y > 0.5) overlaps += 1;
          }
        }
        w.atResize = { more: document.querySelector('#ctl-more')!.getClientRects().length > 0, overlaps };
      });
    });
    await page.setViewportSize({ width: 360, height: 780 });
    await settle(page);
    assert.deepEqual(await page.evaluate(() => (window as unknown as { atResize: unknown }).atResize), { more: true, overlaps: 0 });
    // Nothing else may re-fit while counting: a font the new layout needs.
    await page.evaluate(() => document.fonts.ready.then(() => undefined));
    await settle(page);

    // Re-fits show as the collapsed cells' class flipping off and on again.
    await page.evaluate(() => {
      const w = window as unknown as { refits: number };
      w.refits = 0;
      new MutationObserver((records) => {
        w.refits += records.filter((record) => (record.target as Element).classList.contains('control-cell')).length;
      }).observe(document.querySelector('.controlbar')!, { subtree: true, attributes: true, attributeFilter: ['class'] });
    });
    // What plugins' re-render and Draw's copy do all the time: the same values again.
    await page.evaluate(() => {
      const draw = document.querySelector<HTMLButtonElement>('#ctl-draw')!;
      for (let i = 0; i < 20; i++) {
        draw.setAttribute('aria-label', draw.getAttribute('aria-label')!);
        draw.title = draw.title;
        draw.disabled = draw.disabled;
        draw.classList.toggle('draw-active', false);
      }
    });
    await settle(page);
    assert.equal(await page.evaluate(() => (window as unknown as { refits: number }).refits), 0, 'same-value rewrites re-fit nothing');
    // A real change still does (the count is not vacuous).
    await page.evaluate(() => document.querySelector('#ctl-draw')!.setAttribute('aria-label', 'Draw on a shared window'));
    await settle(page);
    assert.ok(await page.evaluate(() => (window as unknown as { refits: number }).refits > 0), 'a real change re-fits');
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('the bar re-fits when cells come and go and when a label changes', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 620, height: 800 });
  try {
    assert.equal((await barOf(page)).moreShown, false);
    // Three more plugin buttons, built the way plugins/setupPlugins.ts builds them.
    await page.evaluate(() => {
      const left = document.querySelector('.controls-left')!;
      for (const name of ['Poll', 'Timer', 'Notes']) {
        const cell = document.createElement('div');
        cell.className = 'control-cell plugin-control-cell';
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'control-button plugin-control-button';
        button.setAttribute('aria-label', name);
        const label = document.createElement('span');
        label.className = 'meeting-control-label';
        label.textContent = name;
        cell.append(button, label);
        left.insertBefore(cell, left.querySelector(':scope > .overflow-cell'));
      }
    });
    await settle(page);
    let state = await barOf(page);
    assertFits(state, 'three more plugin buttons');
    assert.equal(state.moreShown, true);
    assert.deepEqual(state.hidden, ['Timer', 'Notes'], 'plugin buttons give way first, the one furthest along first');

    // A longer label widens its cell: Share's grows to the label's 92px cap.
    await page.evaluate(() => {
      document.querySelector('#ctl-share-label')!.textContent = 'Share your whole screen';
    });
    await settle(page);
    state = await barOf(page);
    assertFits(state, 'longer label');
    assert.deepEqual(state.hidden, ['Poll', 'Timer', 'Notes']);

    await page.evaluate(() => {
      for (const cell of Array.from(document.querySelectorAll('.plugin-control-cell')).slice(1)) cell.remove();
      document.querySelector('#ctl-share-label')!.textContent = 'Share';
    });
    await settle(page);
    state = await barOf(page);
    assert.equal(state.moreShown, false, 'everything fits again: ⋯ goes');
    assert.deepEqual(state.hidden, []);

    // Styles that widen the cells but not the bar (as a media query on the
    // other axis would): no mutation in the bar and no change to its size,
    // so only watching the cells themselves catches it.
    await page.addStyleTag({ content: '.control-cell { padding-inline: 10px; }' });
    await settle(page);
    state = await barOf(page);
    assertFits(state, 'cells widened by a stylesheet');
    assert.equal(state.moreShown, true);
    assert.deepEqual(state.hidden, ['Draw', 'React']);
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('re-fitting after a size change never trips a ResizeObserver loop, even when the collapse changes the bar\'s height', { timeout: 60_000 }, async () => {
  const { page, errors, close } = await openMeeting({ width: 620, height: 800 });
  try {
    // A plugin button whose label wraps to three lines makes the bar taller
    // while it is shown, and shorter again once it collapses.
    await page.evaluate(() => {
      const left = document.querySelector('.controls-left')!;
      const cell = document.createElement('div');
      cell.className = 'control-cell plugin-control-cell';
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'control-button plugin-control-button';
      const label = document.createElement('span');
      label.className = 'meeting-control-label';
      label.textContent = 'Live poll results for everyone';
      cell.append(button, label);
      left.insertBefore(cell, left.querySelector(':scope > .overflow-cell'));
    });
    await settle(page);
    const tall = await barOf(page);
    assert.deepEqual(tall.hidden, [], 'the tall cell fits at 620');
    // Narrow the bar without resizing the window (a window resize is fitted
    // in its own event): only the ResizeObserver sees this one.
    await page.addStyleTag({ content: '.controlbar { padding-inline: 40px; }' });
    await settle(page);
    const state = await barOf(page);
    assertFits(state, 'narrowed bar');
    assert.deepEqual(state.hidden, ['Live poll results for everyone']);
    assert.ok(state.bar.bottom - state.bar.top < tall.bar.bottom - tall.bar.top, 'the bar got shorter as the tall cell left');
    assert.deepEqual(errors, []);
    assert.deepEqual(
      await page.evaluate(() => (window as unknown as { __windowErrors: string[] }).__windowErrors),
      [],
      'no ResizeObserver loop error'
    );
  } finally {
    await close();
  }
});

test('a row the menu offers of its own (#239\'s "Developer & test tools") brings ⋯ even when every control fits', { timeout: 60_000 }, async () => {
  for (const viewport of [{ width: 1280, height: 800 }, { width: 500, height: 800 }]) {
    const where = `${viewport.width}px`;
    const { page, errors, close } = await openMeeting(viewport);
    try {
      const before = await barOf(page);
      // What main.ts registers for the dev drawer, through the automation
      // hook. A source string: the item's methods would otherwise get tsx's
      // `__name` helper, which the page does not have.
      await page.evaluate(`(() => {
        window.itemOn = false;
        window.itemRuns = 0;
        window.__petalHarness.controlOverflow.addMenuItem({
          label: 'Test tools',
          icon: '<svg viewBox="0 0 24 24"><path d="M4 4h16v16H4z"></path></svg>',
          available: () => window.itemOn,
          run: () => { window.itemRuns += 1; },
        });
      })()`);
      assert.deepEqual(await barOf(page), before, `${where}: an unavailable item changes nothing`);

      await page.evaluate(() => {
        const w = window as unknown as { __petalHarness: { controlOverflow: { update(): void } }; itemOn: boolean };
        w.itemOn = true;
        w.__petalHarness.controlOverflow.update();
      });
      const state = await barOf(page);
      assertFits(state, where);
      assert.equal(state.moreShown, true, `${where}: the item alone brings ⋯`);
      const menu = await openMenu(page);
      assert.deepEqual(
        menu.rows.map((row) => row.label),
        [...state.hidden, 'Test tools'],
        `${where}: the item follows the hidden controls`
      );
      assert.equal(
        await page.evaluate(() => document.querySelectorAll('#overflow-menu .overflow-menu-divider').length),
        state.hidden.length > 0 ? 1 : 0
      );
      await page.keyboard.press('End');
      await page.keyboard.press('Enter');
      assert.equal(await page.evaluate(() => (window as unknown as { itemRuns: number }).itemRuns), 1);
      assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true);
      assert.equal(await page.evaluate(() => document.activeElement?.id), 'ctl-more');
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  }
});

test('a control hidden as unsupported (Share on phones, #240) is in neither the bar nor the menu, and its room is reused', { timeout: 60_000 }, async () => {
  const expected: Record<number, string[]> = {
    320: ['Invite', 'Draw', 'Chat', 'React'],
    // Share's room lets Invite back in.
    412: ['Draw', 'React'],
  };
  for (const viewport of [{ width: 320, height: 568 }, { width: 412, height: 915 }]) {
    const where = `${viewport.width}px`;
    const { page, errors, close } = await openMeeting(viewport, [UNSUPPORTED_STAND_IN]);
    try {
      await page.evaluate(() => {
        document.querySelector('#ctl-share')!.closest<HTMLElement>('.control-cell')!.hidden = true;
      });
      await settle(page);
      const state = await barOf(page);
      assertFits(state, where);
      assert.ok(!state.shown.includes('Share'), `${where}: Share must not be in the bar`);
      const menu = await openMenu(page);
      assert.deepEqual(menu.rows.map((row) => row.label), expected[viewport.width], `${where}: menu rows`);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  }
});

test('on a vertical rail (a stand-in for #239\'s landscape layout) it measures the height, and re-fits on rotation', { timeout: 60_000 }, async () => {
  const landscape = { width: 915, height: 412 };
  const { page, errors, close } = await openMeeting(landscape, [RAIL_STAND_IN]);
  try {
    // #239's Full screen cell, placed as meetingViewport.ts places it: in the
    // bar itself, just before Leave.
    await page.evaluate(() => {
      const bar = document.querySelector('.controlbar')!;
      const cell = document.createElement('div');
      cell.className = 'control-cell fullscreen-cell';
      const button = document.createElement('button');
      button.type = 'button';
      button.id = 'ctl-fullscreen';
      button.className = 'control-button';
      button.setAttribute('aria-label', 'Enter full screen');
      button.setAttribute('aria-pressed', 'false');
      const label = document.createElement('span');
      label.className = 'meeting-control-label';
      label.textContent = 'Full screen';
      cell.append(button, label);
      bar.insertBefore(cell, bar.querySelector('.leave-cell'));
    });
    await settle(page);
    let state = await barOf(page);
    assert.equal(state.vertical, true, 'the stand-in lays the bar out as a column');
    assertFits(state, 'rail');
    // A 915px-wide bottom bar would fit everything: only the height hides
    // these. Full screen outlasts Draw and Invite; Share and Chat stay too.
    assert.deepEqual(state.hidden, ['Invite', 'Draw', 'React']);
    assert.equal(state.moreShown, true);
    for (const label of ['Share', 'Chat', 'Full screen']) assert.ok(state.shown.includes(label), `${label} stays in the rail`);

    const menu = await openMenu(page);
    assert.deepEqual(menu.rows.map((row) => row.label), ['Invite', 'Draw', 'React']);
    assertInside(menu.box, landscape, 'rail');
    assert.ok(menu.box.right <= state.bar.left + 0.5, 'the menu opens beside the rail, not over it');
    await page.keyboard.press('ArrowDown');
    await page.keyboard.press('Escape');
    assert.equal(await page.evaluate(() => document.activeElement?.id), 'ctl-more');

    // Rotate to portrait: a bottom bar again, 412px wide.
    await page.setViewportSize({ width: 412, height: 915 });
    await settle(page);
    state = await barOf(page);
    assert.equal(state.vertical, false);
    assertFits(state, 'rotated to portrait');
    assert.deepEqual(state.hidden, ['Invite', 'Draw', 'React', 'Full screen']);
    // Full screen is not part of this layout at all: not offered in the menu.
    assert.deepEqual((await openMenu(page)).rows.map((row) => row.label), ['Invite', 'Draw', 'React']);
    await page.keyboard.press('Escape');

    await page.setViewportSize(landscape);
    await settle(page);
    state = await barOf(page);
    assert.equal(state.vertical, true);
    assertFits(state, 'rotated back');
    assert.deepEqual(state.hidden, ['Invite', 'Draw', 'React']);

    // A shorter phone (iPhone SE landscape): Full screen goes too, and stays
    // gone although the rail's own rule shows that cell.
    await page.setViewportSize({ width: 667, height: 375 });
    await settle(page);
    state = await barOf(page);
    assertFits(state, '375px rail');
    assert.deepEqual(state.hidden, ['Invite', 'Draw', 'React', 'Full screen']);
    assert.deepEqual((await openMenu(page)).rows.map((row) => row.label), ['Invite', 'Draw', 'React', 'Full screen']);
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});
