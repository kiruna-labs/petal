import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { after, before, test } from 'node:test';
import { build } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { DEV_SHEET_QUERY, MEETING_RAIL_QUERY } from '../src/meetingViewport.ts';

// #239: the phone meeting layout, checked on REAL rendered geometry (the
// node tests have no layout engine, so a collapsed hero or an off-screen
// control would pass them vacuously). One build of the real client, then
// synthetic tiles in #tiles driven through the real layout picker.

const repoRoot = resolve(import.meta.dirname, '../..');
const webRoot = resolve(repoRoot, 'web-harness');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

type Browser = Awaited<ReturnType<typeof chromium.launch>>;
type Page = Awaited<ReturnType<Browser['newPage']>>;

let buildDir = '';
let browser: Browser | undefined;

before(async () => {
  buildDir = await mkdtemp(join(tmpdir(), 'petal-meeting-layout-build-'));
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

const LANDSCAPE_PHONE = { viewport: { width: 863, height: 360 }, isMobile: true, hasTouch: true, deviceScaleFactor: 1 };
const PORTRAIT_PHONE = { viewport: { width: 412, height: 839 }, isMobile: true, hasTouch: true, deviceScaleFactor: 1 };
const DESKTOP = { viewport: { width: 1280, height: 800 } };

interface MeetingOptions {
  /** Query string for the page URL, e.g. 'dev=1'. */
  query?: string;
  /** Give tile 0 a shared window's docked header and, if sized, a live
   * video of that size (a canvas stream, so it reports real dimensions). */
  share?: { header: boolean; video?: [width: number, height: number] };
  setup?: (page: Page) => Promise<void>;
}

/** Opens the meeting screen with `count` synthetic tiles (tile 0 is a share). */
async function openMeeting(context: Record<string, unknown>, count: number, options: MeetingOptions = {}): Promise<Page> {
  const ctx = await browser!.newContext(context);
  const page = await ctx.newPage();
  if (options.setup) await options.setup(page);
  const url = pathToFileURL(join(buildDir, 'index.html')).href + (options.query ? `?${options.query}` : '');
  await page.goto(url, { waitUntil: 'load' });
  await page.waitForSelector('.layout-mode-button', { state: 'attached' });
  await page.evaluate(
    ([tileCount, share]: readonly [number, MeetingOptions['share'] | null]) => {
      document.querySelector('#join-screen')?.classList.add('hidden');
      document.querySelector('#meeting-screen')?.classList.remove('hidden');
      const tiles = document.querySelector('#tiles')!;
      const w = window as typeof window & { __tileClicks?: number };
      w.__tileClicks = 0;
      for (let index = 0; index < tileCount; index += 1) {
        const tile = document.createElement('div');
        tile.id = `tile-${index}`;
        tile.className = index === 0 ? 'tile share-tile' : 'tile camera-off';
        tile.dataset.owner = `peer-${index}`;
        tile.innerHTML = `<span class="initials">P${index}</span><div class="name-chip"><span class="name-chip-label">Peer ${index}</span></div>`;
        tile.addEventListener('click', () => {
          w.__tileClicks = (w.__tileClicks ?? 0) + 1;
        });
        tiles.appendChild(tile);
      }
      const shareTile = document.querySelector('#tile-0');
      if (!shareTile || !share) return;
      if (share.header) {
        // The real header's structure and classes (remoteWindowHeader.ts).
        shareTile.classList.add('has-remote-window-header');
        shareTile.insertAdjacentHTML(
          'beforeend',
          `<div class="remote-window-header"><div class="remote-window-header__left">` +
            `<div class="remote-window-header__window-actions"><button class="control-button remote-window-header__icon-control">+</button></div>` +
            `<div class="remote-window-header__title-cluster"><span class="remote-window-header__title">` +
            `<span class="remote-window-header__source-label">Notes</span> <span class="remote-window-header__owner-label">by Alice Chen</span>` +
            `</span></div></div><div class="remote-window-header__right">` +
            `<button class="remote-window-header__header-btn remote-window-header__ai-chat">AI chat</button>` +
            `<div class="remote-window-header__mode-switcher"><button class="remote-window-header__segment">View</button>` +
            `<button class="remote-window-header__segment">Control</button><button class="remote-window-header__segment">Draw</button></div>` +
            `<button class="remote-window-header__header-btn remote-window-header__overflow-button">...</button></div></div>`
        );
      }
      if (share.video) {
        const canvas = document.createElement('canvas');
        [canvas.width, canvas.height] = share.video;
        const paint = canvas.getContext('2d')!;
        setInterval(() => {
          paint.fillStyle = `hsl(${Date.now() % 360} 60% 40%)`;
          paint.fillRect(0, 0, canvas.width, canvas.height);
        }, 50);
        const video = document.createElement('video');
        video.className = 'share-video';
        video.muted = true;
        video.autoplay = true;
        video.playsInline = true;
        video.srcObject = canvas.captureStream(20);
        shareTile.prepend(video);
      }
    },
    [count, options.share ?? null] as const
  );
  return page;
}

async function setLayout(page: Page, label: 'Grid view' | 'Spotlight view') {
  await page.evaluate((wanted: string) => {
    document.querySelector<HTMLButtonElement>(`.layout-mode-button[aria-label="${wanted}"]`)!.click();
  }, label);
  await page.waitForTimeout(350); // FLIP settles
}

function rect(page: Page, selector: string) {
  return page.evaluate((sel: string) => {
    const r = document.querySelector(sel)!.getBoundingClientRect();
    return { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height };
  }, selector);
}

/** Whether the document itself moved: a wheel notch, a script scroll. */
async function documentScrolls(page: Page): Promise<boolean> {
  await page.mouse.move(200, 200).catch(() => {});
  await page.mouse.wheel(0, 400).catch(() => {});
  await page.waitForTimeout(150);
  return page.evaluate(() => {
    window.scrollBy(0, 400);
    return window.scrollY !== 0 || document.scrollingElement!.scrollTop !== 0;
  });
}

test('#239 the landscape rail CSS and the top bar idle clock answer to the same media query', async () => {
  // meetingViewport.ts only runs the fade clock while MEETING_RAIL_QUERY
  // matches; if style.css's block drifted, the bar would fade in a layout
  // where it is not an overlay (or never fade where it is).
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  assert.ok(css.includes(`@media ${MEETING_RAIL_QUERY} {`), `style.css has no @media ${MEETING_RAIL_QUERY} block`);
});

test('#239 x #247 the developer sheet CSS and the ⋯ menu\'s developer row answer to the same media query', async () => {
  // main.ts offers the ⋯ row only while DEV_SHEET_QUERY matches; if
  // style.css's sheet block drifted, the row would show where the drawer is
  // a visible row (a ⋯ for nothing), or be missing where it is parked.
  const css = (await readFile(new URL('../src/style.css', import.meta.url), 'utf8')).replace(/\s+/g, ' ');
  assert.ok(css.includes(`@media ${DEV_SHEET_QUERY} {`), `style.css has no @media ${DEV_SHEET_QUERY} block`);
});

test('#239 the spotlight hero fills its track beside the strip instead of collapsing to its border', { timeout: 60_000 }, async () => {
  const page = await openMeeting(DESKTOP, 4);
  try {
    await setLayout(page, 'Spotlight view');
    const hero = await rect(page, '.tile.is-spotlight');
    const strip = await rect(page, '.spotlight-strip');
    // 0.9.27-0.9.29 rendered this hero 2px tall: only the strip was visible.
    assert.ok(hero.height > 400, `hero is ${hero.width}x${hero.height}`);
    assert.ok(Math.abs(hero.width / hero.height - 16 / 9) < 0.02, 'a camera-off hero keeps a 16:9 box');
    assert.ok(strip.left >= hero.right, 'the strip sits beside the hero on a wide surface');
    const thumbnails = await page.evaluate(() =>
      [...document.querySelectorAll('.spotlight-strip > .tile')].map((tile) => {
        const r = tile.getBoundingClientRect();
        return `${Math.round(r.width)}x${Math.round(r.height)}`;
      })
    );
    assert.equal(thumbnails.length, 3);
    assert.equal(new Set(thumbnails).size, 1, `every thumbnail one size: ${thumbnails.join(', ')}`);
  } finally {
    await page.context().close();
  }
});

test('#239 the meeting page never scrolls on any device, and desktop keeps its developer row', { timeout: 60_000 }, async () => {
  // Review blocker: with the drawer hung below a 100dvh meeting, the page
  // scrolled ~33px -- a hand-join on a Pixel 8 left the top bar and Mic cut
  // off, a swipe or one wheel notch exposed the developer row.
  for (const [label, device] of [
    ['desktop', DESKTOP],
    ['portrait phone', PORTRAIT_PHONE],
    ['landscape phone', LANDSCAPE_PHONE],
  ] as const) {
    const page = await openMeeting(device, 4);
    try {
      await setLayout(page, 'Grid view');
      assert.equal(await documentScrolls(page), false, `${label}: the document scrolled`);
      const { innerHeight } = await page.evaluate(() => ({ innerHeight }));
      const devPanel = await rect(page, '#dev-panel');
      if (label === 'desktop') {
        // Testers' row, as on main: on screen, under the control bar.
        const controlbar = await rect(page, '.controlbar');
        assert.ok(devPanel.top >= controlbar.bottom - 1 && devPanel.bottom <= innerHeight + 1, 'the developer row sits under the controls');
      } else {
        assert.ok(devPanel.top >= innerHeight - 1, `${label}: the developer sheet is parked below the screen`);
      }
      const topbar = await rect(page, '.topbar');
      assert.ok(topbar.top >= 0, `${label}: the top bar is on screen`);
    } finally {
      await page.context().close();
    }
  }
});

test('#239 on a phone ?dev=1 slides the developer sheet up, and closing it parks it again', { timeout: 60_000 }, async () => {
  const page = await openMeeting(PORTRAIT_PHONE, 2, { query: 'dev=1' });
  try {
    await page.waitForTimeout(400); // the sheet's slide
    const open = await rect(page, '#dev-panel');
    assert.ok(open.top < 839 && open.bottom <= 839 + 1, `the sheet is on screen (${open.top}-${open.bottom})`);
    assert.ok(open.height <= 839 * 0.45 + 1, 'at most 45% of the screen');
    assert.equal(await page.evaluate(() => (document.querySelector('#dev-panel') as HTMLDetailsElement).open), true);
    // Automation clicks #share-btn by script whether or not the sheet is up.
    assert.equal(await page.locator('#share-btn').count(), 1);
    await page.locator('#dev-panel > summary').click();
    await page.waitForTimeout(400);
    assert.ok((await rect(page, '#dev-panel')).top >= 839 - 1, 'closed, it is parked below the screen');
  } finally {
    await page.context().close();
  }
});

test('#239 x #247 on a phone the ⋯ menu slides the developer sheet up; where the drawer is a row it offers none', { timeout: 60_000 }, async () => {
  for (const [label, device] of [
    ['portrait phone', PORTRAIT_PHONE],
    ['landscape phone', LANDSCAPE_PHONE],
  ] as const) {
    const page = await openMeeting(device, 2);
    try {
      const { height } = device.viewport;
      assert.ok((await rect(page, '#dev-panel')).top >= height - 1, `${label}: the sheet starts parked`);
      await page.click('#ctl-more');
      await page.waitForSelector('#overflow-menu.placed');
      const rows = await page.evaluate(() =>
        [...document.querySelectorAll('#overflow-menu .overflow-menu-row')].map((row) => row.textContent!.trim())
      );
      assert.equal(rows.at(-1), 'Developer & test tools', `${label}: the menu ends with the developer row (${rows.join(', ')})`);
      await page.click('#overflow-menu .overflow-menu-row >> text=Developer & test tools');
      await page.waitForTimeout(400); // the sheet's slide
      const sheet = await rect(page, '#dev-panel');
      assert.ok(sheet.top < height && sheet.bottom <= height + 1, `${label}: the sheet is up (${sheet.top}-${sheet.bottom})`);
      assert.equal(await page.evaluate(() => (document.querySelector('#dev-panel') as HTMLDetailsElement).open), true);
      assert.equal(await page.evaluate(() => document.querySelector<HTMLElement>('#overflow-menu')!.hidden), true, `${label}: the menu closed`);
    } finally {
      await page.context().close();
    }
  }

  // Desktop: the drawer is the meeting's bottom row, so no ⋯ at all at
  // 1280; a short desktop window (the rail, with a mouse) keeps its row too,
  // and its ⋯ holds only controls.
  const desktop = await openMeeting(DESKTOP, 2);
  try {
    assert.equal(await desktop.evaluate(() => document.querySelector('#ctl-more')!.getClientRects().length), 0, 'desktop: no ⋯');
  } finally {
    await desktop.context().close();
  }
  const short = await openMeeting({ viewport: { width: 1280, height: 480 } }, 2);
  try {
    const rows = await short.evaluate(() => {
      document.querySelector<HTMLButtonElement>('#ctl-more')!.click();
      return [...document.querySelectorAll('#overflow-menu .overflow-menu-row')].map((row) => row.textContent!.trim());
    });
    assert.ok(!rows.includes('Developer & test tools'), `short desktop window: no developer row (${rows.join(', ')})`);
  } finally {
    await short.context().close();
  }
});

test('#239 a landscape phone gets a slim control rail and two tiles covering over 55% of the screen', { timeout: 60_000 }, async () => {
  const page = await openMeeting(LANDSCAPE_PHONE, 2);
  try {
    await setLayout(page, 'Grid view');
    const rail = await rect(page, '.controlbar');
    assert.ok(rail.right >= 863 - 1 && rail.top <= 0 && rail.bottom >= 360 - 1, 'a full-height rail on the right edge');
    assert.ok(rail.width <= 64, `the rail is one button wide (${rail.width}px)`);
    // Mic, Camera and Leave never leave the rail, and ⋯ (#247) holds what
    // does not fit its height: every other control is in the rail on
    // screen, or in the ⋯ menu.
    for (const id of ['#ctl-audio', '#ctl-video', '#ctl-leave', '#ctl-more']) {
      const r = await rect(page, id);
      assert.ok(r.width > 0 && r.top >= 0 && r.bottom <= 360 && r.left >= rail.left, `${id} is in the rail, on screen`);
    }
    const unreachable = await page.evaluate(() =>
      [...document.querySelectorAll<HTMLElement>('.controlbar .control-cell')]
        .filter((cell) => !cell.hidden && !cell.classList.contains('overflowed'))
        .filter((cell) => {
          const r = cell.getBoundingClientRect();
          // window.*: a bare `innerHeight` here makes tsx rename another test's
          // `const { innerHeight }`, inside page code that then cannot find it.
          return !(r.height > 0 && r.top >= 0 && r.bottom <= window.innerHeight && r.right <= window.innerWidth);
        })
        .map((cell) => cell.querySelector('button')?.id ?? cell.className)
    );
    assert.deepEqual(unreachable, [], 'every control is in the rail on screen or in the ⋯ menu');
    const labelsShown = await page.evaluate(
      () => [...document.querySelectorAll('.controlbar .meeting-control-label')].filter((el) => getComputedStyle(el).display !== 'none').length
    );
    assert.equal(labelsShown, 0, 'icon-only: the names live in aria-label');
    const topbarFullscreen = await page.evaluate(() => getComputedStyle(document.querySelector('#topbar-fullscreen')!).display);
    assert.equal(topbarFullscreen, 'none', 'the rail, not the top bar, holds full screen in landscape');

    const tileArea = await page.evaluate(() =>
      [...document.querySelectorAll('#tiles > .tile')].reduce((sum, tile) => {
        const r = tile.getBoundingClientRect();
        return sum + r.width * r.height;
      }, 0)
    );
    const fraction = tileArea / (863 * 360);
    // #239 definition of done: 55% (it was ~25% with the bottom bar, top bar and dev row).
    assert.ok(fraction >= 0.55, `two tiles cover ${(fraction * 100).toFixed(1)}% of the viewport`);
  } finally {
    await page.context().close();
  }
});

test('#239 landscape spotlight strip is a side column that scrolls to every participant', { timeout: 60_000 }, async () => {
  const page = await openMeeting(LANDSCAPE_PHONE, 8);
  try {
    await setLayout(page, 'Spotlight view');
    const hero = await rect(page, '.tile.is-spotlight');
    const strip = await rect(page, '.spotlight-strip');
    assert.ok(hero.height >= 340, `the hero takes the height (${hero.height}px)`);
    assert.ok(strip.left >= hero.right && strip.height >= 340, 'a full-height column beside the hero');
    const scroll = await page.evaluate(() => {
      const el = document.querySelector<HTMLElement>('.spotlight-strip')!;
      const style = getComputedStyle(el);
      const last = el.lastElementChild!;
      el.scrollTop = el.scrollHeight;
      const r = last.getBoundingClientRect();
      const box = el.getBoundingClientRect();
      return {
        overflowY: style.overflowY,
        overscroll: style.overscrollBehaviorY,
        scrolls: el.scrollHeight > el.clientHeight,
        lastReachable: r.top >= box.top - 1 && r.bottom <= box.bottom + 1,
        thumbnailHeight: r.height,
      };
    });
    assert.equal(scroll.overflowY, 'auto');
    assert.equal(scroll.overscroll, 'contain');
    assert.ok(scroll.scrolls, '7 thumbnails do not fit: the strip scrolls rather than shrinking them');
    assert.ok(scroll.lastReachable, 'the last participant is reachable');
    assert.ok(scroll.thumbnailHeight >= 60, `thumbnails keep a floor (${scroll.thumbnailHeight}px)`);
  } finally {
    await page.context().close();
  }
});

test('#239 a shared window hero takes the window\'s own shape and is the biggest picture on screen', { timeout: 60_000 }, async () => {
  // Review case: a 16:9 hero BOX gave a 4:3 window less video than a
  // thumbnail on a portrait phone. The hero now fits the video (plus its
  // 44px header), and re-fits when the video reports its size.
  for (const [label, device] of [
    ['portrait phone', PORTRAIT_PHONE],
    ['landscape phone', LANDSCAPE_PHONE],
  ] as const) {
    const page = await openMeeting(device, 5, { share: { header: true, video: [800, 600] } });
    try {
      await page.waitForFunction(() => (document.querySelector('#tile-0 video') as HTMLVideoElement).videoWidth > 0);
      await setLayout(page, 'Spotlight view');
      const shape = await page.evaluate(() => {
        const hero = document.querySelector('.tile.is-spotlight')!.getBoundingClientRect();
        const video = document.querySelector('.tile.is-spotlight video')!.getBoundingClientRect();
        const thumbnails = [...document.querySelectorAll('.spotlight-strip > .tile')].map((tile) => {
          const r = tile.getBoundingClientRect();
          return r.width * r.height;
        });
        return { hero: [hero.width, hero.height], video: video.width * video.height, largestThumbnail: Math.max(...thumbnails) };
      });
      const [width, height] = shape.hero;
      assert.ok(Math.abs(width / (height - 44) - 4 / 3) < 0.03, `${label}: hero ${width}x${height} is a 4:3 window under a 44px header`);
      assert.ok(shape.video >= 2 * shape.largestThumbnail, `${label}: video ${shape.video} vs thumbnail ${shape.largestThumbnail}`);
    } finally {
      await page.context().close();
    }
  }
});

test('#239 a shared window\'s title stays readable in a landscape phone\'s spotlight', { timeout: 60_000 }, async () => {
  // Review case: at a 480-630px hero the segmented switcher still showed and
  // squeezed the title into one letter per line ("by Alic e Che n").
  const page = await openMeeting({ ...LANDSCAPE_PHONE, viewport: { width: 667, height: 375 } }, 4, { share: { header: true } });
  try {
    await setLayout(page, 'Spotlight view');
    const title = await rect(page, '.tile.is-spotlight .remote-window-header__title');
    const hero = await rect(page, '.tile.is-spotlight');
    assert.ok(hero.width < 640, `a phone-sized hero (${hero.width}px)`);
    assert.ok(title.width >= 120, `the title has room (${title.width}px)`);
    assert.ok(title.height <= 36, `at most two lines (${title.height}px)`);
  } finally {
    await page.context().close();
  }
});

test('#239 the landscape top bar fades when idle, and the tap that brings it back pins nothing', { timeout: 60_000 }, async () => {
  const page = await openMeeting(LANDSCAPE_PHONE, 2, { setup: (p) => p.clock.install() });
  try {
    await setLayout(page, 'Grid view');
    assert.equal(await page.locator('#meeting-screen.chrome-idle').count(), 0, 'visible on arrival');
    await page.clock.runFor(3500);
    assert.equal(await page.locator('#meeting-screen.chrome-idle').count(), 1, 'faded after ~3s idle');
    const pickerTaps = () => page.evaluate(() => getComputedStyle(document.querySelector('.layout-mode-button')!).pointerEvents);
    assert.equal(await pickerTaps(), 'none', 'a faded bar takes no taps');

    const tile = await rect(page, '#tile-1');
    await page.touchscreen.tap(tile.left + tile.width / 2, tile.top + tile.height / 2);
    assert.equal(await page.locator('#meeting-screen.chrome-idle').count(), 0, 'a tap brings it back');
    assert.equal(await pickerTaps(), 'auto');
    assert.equal(await page.evaluate(() => (window as typeof window & { __tileClicks?: number }).__tileClicks), 0, 'and does not reach the tile');
    await page.touchscreen.tap(tile.left + tile.width / 2, tile.top + tile.height / 2);
    assert.equal(await page.evaluate(() => (window as typeof window & { __tileClicks?: number }).__tileClicks), 1, 'the next tap does');
    // Only the bar's controls take taps: the gradient between them passes
    // taps to the share header underneath.
    assert.equal(await page.evaluate(() => getComputedStyle(document.querySelector('.topbar-right')!).pointerEvents), 'none');
  } finally {
    await page.context().close();
  }
});

test('#239 a late tap-to-play prompt brings the faded top bar back', { timeout: 60_000 }, async () => {
  const page = await openMeeting(LANDSCAPE_PHONE, 2, { setup: (p) => p.clock.install() });
  try {
    await page.clock.runFor(3500);
    assert.equal(await page.locator('#meeting-screen.chrome-idle').count(), 1);
    // connection.ts prepends this when the browser refuses autoplay.
    await page.evaluate(() => {
      const prompt = document.createElement('button');
      prompt.className = 'audio-playback-prompt';
      prompt.textContent = 'Enable audio';
      document.querySelector('.topbar-right')!.prepend(prompt);
    });
    await page.waitForFunction(() => !document.querySelector('#meeting-screen')!.classList.contains('chrome-idle'));
    await page.clock.runFor(10_000);
    assert.equal(await page.locator('#meeting-screen.chrome-idle').count(), 0, 'and keeps it while the prompt waits');
  } finally {
    await page.context().close();
  }
});

test('#239 a portrait phone with its keyboard up keeps its bottom bar (a short viewport is not landscape)', { timeout: 60_000 }, async () => {
  // interactive-widget=resizes-content: a 290px keyboard leaves a 360x294
  // "landscape" viewport. Without the aspect clause the bar jumped into the
  // rail and the room name stacked one letter per line.
  const page = await openMeeting({ ...PORTRAIT_PHONE, viewport: { width: 360, height: 584 } }, 4);
  try {
    await page.evaluate(() => (document.querySelector('#ctl-chat') as HTMLButtonElement).click());
    await page.setViewportSize({ width: 360, height: 294 });
    await page.waitForTimeout(300);
    const bar = await rect(page, '.controlbar');
    assert.ok(bar.width >= 359 && bar.bottom >= 294 - 1, `the control bar stays along the bottom (${JSON.stringify(bar)})`);
    assert.equal(await page.evaluate(() => getComputedStyle(document.querySelector('#meeting-screen')!).display), 'flex');
    // And a landscape phone with ITS keyboard up (863x180) stays the rail.
    await page.setViewportSize({ width: 863, height: 180 });
    await page.waitForTimeout(300);
    assert.equal(await page.evaluate(() => getComputedStyle(document.querySelector('#meeting-screen')!).display), 'grid');
  } finally {
    await page.context().close();
  }
});

test('#239 the portrait top bar stays one slim row, with fingertip-sized hit areas', { timeout: 60_000 }, async () => {
  const page = await openMeeting({ ...PORTRAIT_PHONE, viewport: { width: 360, height: 780 } }, 2);
  try {
    await page.evaluate(() => {
      document.querySelector('#room-name')!.textContent = 'Petal meeting';
      document.querySelector('#feedback-meeting-trigger')!.removeAttribute('hidden');
    });
    await page.waitForTimeout(100);
    const topbar = await rect(page, '.topbar');
    assert.ok(topbar.height <= 52, `the top bar is ${topbar.height}px tall`);
    const name = await page.evaluate(() => {
      const el = document.querySelector('#room-name')!;
      return { height: el.getBoundingClientRect().height, line: parseFloat(getComputedStyle(el).lineHeight) };
    });
    assert.ok(name.height < name.line * 1.5, `the room name is one line (${name.height}px)`);
    // 44px to a fingertip without 44px of chrome: taps just outside the
    // visible full-screen button and picker still land on them.
    // (No named inner functions in page code: tsx's keepNames would inject
    // an `__name` helper that does not exist inside the page.)
    const hits = await page.evaluate(() => {
      const fullscreen = document.querySelector('#topbar-fullscreen')!;
      const [grid, spotlight] = [...document.querySelectorAll('.layout-mode-button')];
      const probes: Array<[string, Element, number, number]> = [
        ['fullscreenBelow', fullscreen, 0, 5],
        ['fullscreenLeft', fullscreen, -5, 0],
        ['gridBelow', grid, 0, 8],
        ['gridLeft', grid, -6, 0],
        ['spotlightAbove', spotlight, 0, -8],
      ];
      return {
        ...Object.fromEntries(
          probes.map(([name, el, dx, dy]) => {
            const r = el.getBoundingClientRect();
            const x = dx < 0 ? r.left + dx : dx > 0 ? r.right + dx : r.left + r.width / 2;
            const y = dy < 0 ? r.top + dy : dy > 0 ? r.bottom + dy : r.top + r.height / 2;
            return [name, document.elementFromPoint(x, y)?.closest('button') === el];
          })
        ),
        visibleSize: fullscreen.getBoundingClientRect().height,
      };
    });
    assert.deepEqual(hits, {
      fullscreenBelow: true,
      fullscreenLeft: true,
      gridBelow: true,
      gridLeft: true,
      spotlightAbove: true,
      visibleSize: 32,
    });
  } finally {
    await page.context().close();
  }
});

test('#239 a short desktop window gets the rail but keeps its top bar and developer row', { timeout: 60_000 }, async () => {
  const page = await openMeeting({ viewport: { width: 1280, height: 480 } }, 4, { setup: (p) => p.clock.install() });
  try {
    await setLayout(page, 'Grid view');
    assert.equal(await page.evaluate(() => getComputedStyle(document.querySelector('#meeting-screen')!).display), 'grid', 'the rail');
    await page.clock.runFor(3500);
    assert.equal(await page.locator('#meeting-screen.chrome-idle').count(), 0, 'no fade with a mouse');
    const devPanel = await rect(page, '#dev-panel');
    assert.ok(devPanel.bottom <= 480 + 1 && devPanel.top < 480 && devPanel.width >= 1279, 'the developer row, full width, on screen');
    const rail = await rect(page, '.controlbar');
    assert.ok(rail.bottom <= devPanel.top + 1, 'the rail ends above the developer row');
  } finally {
    await page.context().close();
  }
});

test('#239 a narrow landscape phone with the chat open gives the drawer its own header', { timeout: 60_000 }, async () => {
  const page = await openMeeting({ ...LANDSCAPE_PHONE, viewport: { width: 640, height: 360 } }, 2);
  try {
    assert.notEqual(await page.evaluate(() => getComputedStyle(document.querySelector('.topbar')!).display), 'none');
    await page.evaluate(() => (document.querySelector('#ctl-chat') as HTMLButtonElement).click());
    await page.waitForTimeout(200);
    assert.equal(await page.evaluate(() => getComputedStyle(document.querySelector('.topbar')!).display), 'none');
  } finally {
    await page.context().close();
  }
});

test('#239 portrait phone: strip under the hero, controls side by side without overlap', { timeout: 60_000 }, async () => {
  const page = await openMeeting(PORTRAIT_PHONE, 4);
  try {
    await setLayout(page, 'Spotlight view');
    const hero = await rect(page, '.tile.is-spotlight');
    const strip = await rect(page, '.spotlight-strip');
    assert.ok(strip.top >= hero.bottom, 'the strip sits under the hero on a tall surface');
    assert.ok(hero.width >= 380, `the hero takes the width (${hero.width}px)`);

    // What does not fit is in the ⋯ menu (#247): every cell in the bar is
    // whole and on its own.
    const overlaps = await page.evaluate(() => {
      const shown = [...document.querySelectorAll<HTMLElement>('.controlbar .control-cell')].flatMap((cell) => {
        const r = cell.getBoundingClientRect();
        return getComputedStyle(cell).display === 'none' || r.width === 0 ? [] : [{ id: cell.textContent!.trim(), r }];
      });
      const found: string[] = [];
      for (let i = 0; i < shown.length; i += 1) {
        for (let j = i + 1; j < shown.length; j += 1) {
          const a = shown[i].r;
          const b = shown[j].r;
          const x = Math.min(a.right, b.right) - Math.max(a.left, b.left);
          const y = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
          if (x > 1 && y > 1) found.push(`${shown[i].id} / ${shown[j].id}`);
        }
      }
      return found;
    });
    assert.deepEqual(overlaps, [], 'no control cell overlaps another');
    for (const id of ['#ctl-audio', '#ctl-video', '#ctl-leave', '#ctl-more']) {
      const r = await rect(page, id);
      assert.ok(r.width > 0 && r.left >= 0 && r.right <= 412 && r.bottom <= 839, `${id} on screen`);
    }
    assert.ok((await rect(page, '#topbar-fullscreen')).width > 0, 'full screen lives in the top bar in portrait');
  } finally {
    await page.context().close();
  }
});

test('#239 the rail full-screen button enters and leaves real full screen, and follows the document', { timeout: 60_000 }, async () => {
  // A landscape phone with the height for it in the rail (a shorter one puts
  // it in the ⋯ menu, #247: the next test).
  const page = await openMeeting({ ...LANDSCAPE_PHONE, viewport: { width: 932, height: 430 } }, 1);
  try {
    const button = page.locator('#ctl-fullscreen');
    const labelIs = (label: string) =>
      page.waitForFunction((want: string) => document.querySelector('#ctl-fullscreen')?.getAttribute('aria-label') === want, label);
    await button.click();
    await labelIs('Exit full screen');
    assert.equal(await button.getAttribute('aria-pressed'), 'true');
    // Leaving by any other route (back gesture, Esc) resets the button too.
    await page.evaluate(() => document.exitFullscreen());
    await labelIs('Enter full screen');
    assert.equal(await page.locator('#topbar-fullscreen').getAttribute('aria-pressed'), 'false');
  } finally {
    await page.context().close();
  }
});

test('#239 x #247 full screen is the last control to leave the rail, and its ⋯ row enters real full screen', { timeout: 60_000 }, async () => {
  const page = await openMeeting(LANDSCAPE_PHONE, 1);
  try {
    // A phone browser's landscape height (863x360): Chat gives way first.
    await page.waitForFunction(() => document.querySelector('#ctl-chat')?.closest('.control-cell')?.classList.contains('overflowed'));
    assert.equal(await page.locator('.fullscreen-cell.overflowed').count(), 0, 'full screen stays in an 863x360 rail');
    assert.ok((await page.locator('#ctl-fullscreen').boundingBox())!.width > 0);
    // Shorter still, it goes to the ⋯ menu too.
    await page.setViewportSize({ width: 863, height: 290 });
    await page.waitForFunction(() => document.querySelector('.fullscreen-cell')?.classList.contains('overflowed'));
    await page.click('#ctl-more');
    await page.waitForSelector('#overflow-menu.placed');
    // The row clicks the rail button inside the user's own tap, so
    // requestFullscreen keeps the user activation it needs.
    await page.click('#overflow-menu .overflow-menu-row >> text=Full screen');
    await page.waitForFunction(() => document.fullscreenElement !== null);
    // The buttons follow `fullscreenchange`, which lands just after.
    await page.waitForFunction(() => document.querySelector('#ctl-fullscreen')?.getAttribute('aria-label') === 'Exit full screen');
    await page.evaluate(() => document.exitFullscreen());
    await page.waitForFunction(() => document.querySelector('#ctl-fullscreen')?.getAttribute('aria-label') === 'Enter full screen');
  } finally {
    await page.context().close();
  }
});

test('#239 a toast with an action keeps its width on a phone instead of wrapping a few letters per line', { timeout: 60_000 }, async () => {
  const page = await openMeeting(PORTRAIT_PHONE, 1);
  try {
    // The shared Toast pill's layout (icon, a message that may shrink, an
    // action that may not), in the real #toast host and stylesheet.
    const box = await page.evaluate(() => {
      const host = document.querySelector<HTMLElement>('#toast')!;
      host.innerHTML =
        '<div style="display:inline-flex;align-items:center;gap:10px;padding:9px 16px;box-sizing:border-box">' +
        '<span style="width:16px;height:16px;flex-shrink:0"></span>' +
        '<span class="probe-message" style="flex:1 1 auto;min-width:0;font:500 13px/1.35 sans-serif">Alice Chen is sharing a window</span>' +
        '<button style="flex-shrink:0;white-space:nowrap;font:600 13px sans-serif;padding:4px 10px">Bring to front</button></div>';
      host.classList.remove('hidden');
      const message = host.querySelector<HTMLElement>('.probe-message')!;
      const lineHeight = parseFloat(getComputedStyle(message).lineHeight);
      const r = host.getBoundingClientRect();
      return { width: r.width, left: r.left, right: r.right, lines: Math.round(message.getBoundingClientRect().height / lineHeight) };
    });
    assert.ok(box.lines <= 2, `the message takes ${box.lines} lines`);
    assert.ok(box.width > 412 / 2, `the toast is ${Math.round(box.width)}px wide, more than half the screen`);
    assert.ok(box.left >= 0 && box.right <= 412, 'on screen');
    assert.ok(Math.abs((box.left + box.right) / 2 - 206) <= 1, 'centred');
  } finally {
    await page.context().close();
  }
});

test('#239 full screen is one tap, hides the navigation UI, and is absent where unsupported', { timeout: 60_000 }, async () => {
  const page = await openMeeting(PORTRAIT_PHONE, 1, {
    setup: (p) =>
      p.addInitScript(() => {
        const calls: unknown[] = [];
        (window as typeof window & { __fullscreenCalls?: unknown[] }).__fullscreenCalls = calls;
        Element.prototype.requestFullscreen = function (options?: FullscreenOptions) {
          calls.push(options);
          return Promise.resolve();
        };
      }),
  });
  try {
    const button = page.locator('#topbar-fullscreen');
    assert.equal(await button.getAttribute('aria-label'), 'Enter full screen');
    await button.click();
    assert.deepEqual(await page.evaluate(() => (window as typeof window & { __fullscreenCalls?: unknown[] }).__fullscreenCalls), [
      { navigationUI: 'hide' }
    ]);
  } finally {
    await page.context().close();
  }

  // iPhone Safari: no element full screen, so no button anywhere. (A string,
  // not a function: tsx would name the getter with a helper the page lacks.)
  const iphone = await openMeeting(PORTRAIT_PHONE, 1, {
    setup: (p) => p.addInitScript({ content: "Object.defineProperty(Document.prototype, 'fullscreenEnabled', { value: false });" }),
  });
  try {
    assert.equal(await iphone.locator('#topbar-fullscreen, #ctl-fullscreen').count(), 0);
  } finally {
    await iphone.context().close();
  }
});
