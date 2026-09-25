// #243, rendered: the home screen fills a phone in both orientations (one
// column in portrait, title beside the fields in landscape), the name card
// keeps the colour bubble and Save on one row and saves on Enter, and phones
// get no desktop download. Layout is measured in a real browser: whether the
// card fills the screen, where the free space goes and whether a first visit
// fits a landscape phone cannot be read off the CSS source.
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { extname, join, normalize, resolve } from 'node:path';
import test from 'node:test';
import { build } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

const repoRoot = resolve(import.meta.dirname, '../..');
const webRoot = resolve(repoRoot, 'web-harness');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
};

// Served from http://127.0.0.1/ as in production: from a file:// path the
// client reads the path as an invite link and shows an "Invite link problem"
// line a real first visit never has.
async function serve(dir: string): Promise<{ origin: string; close: () => Promise<void> }> {
  const server = createServer((req, res) => {
    const path = new URL(req.url ?? '/', 'http://127.0.0.1').pathname;
    const file = join(dir, normalize(path === '/' ? '/index.html' : path).replace(/^(\.\.[/\\])+/, ''));
    readFile(file).then(
      (body) => {
        res.writeHead(200, { 'content-type': MIME[extname(file)] ?? 'application/octet-stream' });
        res.end(body);
      },
      () => {
        res.writeHead(404);
        res.end();
      }
    );
  });
  await new Promise<void>((done) => server.listen(0, '127.0.0.1', done));
  const address = server.address();
  if (address === null || typeof address === 'string') throw new Error('server did not bind a port');
  return { origin: `http://127.0.0.1:${address.port}`, close: () => new Promise<void>((done) => server.close(() => done())) };
}

const ANDROID_UA =
  'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Mobile Safari/537.36';

type Box = { left: number; top: number; right: number; bottom: number; width: number; height: number };

type Boxes = Record<'card' | 'topbar' | 'hero' | 'body' | 'footer' | 'name' | 'bubble' | 'save', Box> & {
  scrollWidth: number;
  scrollHeight: number;
  downloadDisplay: string;
  canvasColor: string;
};

// A string, not a function: tsx's keepNames would wrap a function's inner
// helpers in a `__name` call that does not exist in the page.
async function boxes(page: { evaluate(script: string): Promise<unknown> }): Promise<Boxes> {
  return (await page.evaluate(`(() => {
    const box = (selector) => {
      const r = document.querySelector(selector).getBoundingClientRect();
      return { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height };
    };
    return {
      scrollWidth: document.documentElement.scrollWidth,
      scrollHeight: document.scrollingElement.scrollHeight,
      card: box('.join-card'),
      topbar: box('.home-topbar'),
      hero: box('.hero-quiet'),
      body: box('.home-body'),
      footer: box('#build-version'),
      name: box('#display-name'),
      bubble: box('#profile-color-bubble'),
      save: box('#profile-onboarding-done'),
      downloadDisplay: getComputedStyle(document.querySelector('#desktop-download')).display,
      canvasColor: getComputedStyle(document.documentElement).backgroundColor
    };
  })()`)) as Boxes;
}

const centerY = (box: Box) => box.top + box.height / 2;

test('#243: the home screen fills a phone in both orientations', { timeout: 120_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-home-phone-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  let server: Awaited<ReturnType<typeof serve>> | undefined;

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

    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu']
    });
    server = await serve(buildDir);
    const url = `${server.origin}/`;
    // Nothing leaves the machine.
    const localOnly = (route: { request: () => { url: () => string }; abort: () => Promise<void>; continue: () => Promise<void> }) =>
      route.request().url().startsWith(server!.origin) ? route.continue() : route.abort();

    // Pixel 8 with the browser toolbar showing, portrait then landscape; then
    // the smallest two-column landscape, an iPhone SE's 667x375.
    for (const viewport of [{ width: 412, height: 839 }, { width: 863, height: 360 }, { width: 667, height: 375 }]) {
      const landscape = viewport.width > viewport.height;
      const at = (message: string) => `${viewport.width}x${viewport.height}: ${message}`;
      const context = await browser.newContext({ viewport, userAgent: ANDROID_UA, isMobile: true, hasTouch: true });
      await context.route('**/*', localOnly);
      const page = await context.newPage();
      await page.goto(url, { waitUntil: 'load' });
      // A first visit opens the name and colour card by itself.
      await page.waitForSelector('#profile-onboarding:not(.hidden)');
      await page.evaluate(() => document.fonts.ready);

      assert.equal(await page.textContent('#conn-error'), '', at('a first visit shows an error'));
      const first = await boxes(page);
      assert.ok(Math.abs(centerY(first.bubble) - centerY(first.save)) <= 1, at('the colour bubble and Save are not on one row'));
      assert.ok(first.bubble.right < first.save.left, at('the colour bubble must sit left of Save'));
      assert.ok(first.name.bottom <= Math.min(first.bubble.top, first.save.top), at('the name field must sit above that row'));
      assert.ok(first.card.height >= viewport.height - 1, at('the card with the name card open is shorter than the screen'));
      assert.ok(first.body.bottom <= first.footer.top + 1, at('the fields overlap the footer'));
      if (landscape) {
        // Two columns: title left, name card and fields right, and all of it
        // on screen -- Create/Join is not below the fold on a first visit.
        assert.ok(first.hero.right <= first.body.left, at('the title and the fields must sit side by side'));
        assert.ok(first.scrollHeight <= viewport.height + 1, at(`the first visit scrolls (${first.scrollHeight}px of content)`));
      }

      // Enter does nothing until there is a name to save.
      await page.focus('#display-name');
      await page.keyboard.press('Enter');
      assert.equal(await page.locator('#profile-onboarding.hidden').count(), 0, at('Enter closed the card with no name'));

      await page.fill('#display-name', 'Ada');
      await page.click('#profile-color-bubble');
      const popover = await page.locator('#profile-color-options').boundingBox();
      assert.ok(popover && popover.width >= 200, at(`the colour popover is squeezed (${popover?.width}px)`));
      assert.ok(popover.x >= 0 && popover.x + popover.width <= viewport.width, at('the colour popover leaves the screen'));
      await page.keyboard.press('Escape');
      if (landscape) {
        await page.click('#profile-onboarding-done');
      } else {
        // The keyboard's "done" key saves, as the Save button does.
        assert.equal(await page.getAttribute('#display-name', 'enterkeyhint'), 'done');
        await page.focus('#display-name');
        await page.keyboard.press('Enter');
      }
      await page.waitForSelector('#profile-onboarding.hidden', { state: 'attached', timeout: 5_000 });
      assert.equal(await page.evaluate(() => localStorage.getItem('petal-harness-name')), 'Ada', at('the name was not saved'));

      const home = await boxes(page);
      assert.deepEqual(
        [home.card.left, home.card.top, home.card.width, home.card.height].map(Math.round),
        [0, 0, viewport.width, viewport.height],
        at('the card must fill the screen exactly')
      );
      assert.equal(Math.round(home.topbar.top), 0, at('the top bar must sit at the top'));
      assert.equal(Math.round(home.footer.bottom), viewport.height, at('the footer must sit at the bottom'));
      if (landscape) {
        assert.ok(Math.abs(centerY(home.hero) - centerY(home.body)) <= 1, at('the title and the fields must share a centre line'));
      } else {
        // A third of the free space above the title, two thirds below the
        // fields: the title at the optical centre, the fields clear of the
        // keyboard.
        const above = home.hero.top - home.topbar.bottom;
        const below = home.footer.top - home.body.bottom;
        assert.ok(above > 0 && Math.abs(below - 2 * above) <= 3, at(`free space is not split 1:2 (${above}px above, ${below}px below)`));
      }
      assert.ok(home.scrollWidth <= viewport.width, at('the page scrolls sideways'));
      assert.equal(home.downloadDisplay, 'none', at('a phone was offered the desktop download'));
      // Rubber-banding past the top shows the backdrop's top colour, not a
      // darker band.
      assert.equal(home.canvasColor, 'rgb(22, 24, 27)', at('the overscroll canvas does not match the backdrop'));
      await context.close();
    }

    // Desktop keeps the floating 380px card and the download link.
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    await page.route('**/*', localOnly);
    await page.goto(url, { waitUntil: 'load' });
    await page.waitForSelector('#profile-onboarding:not(.hidden)');
    const desktop = await boxes(page);
    assert.equal(desktop.card.width, 380);
    assert.ok(desktop.card.left > 0 && desktop.card.top > 0, 'the desktop card must float, not fill the window');
    assert.notEqual(desktop.downloadDisplay, 'none', 'desktop browsers keep the desktop download');
    assert.ok(Math.abs(centerY(desktop.bubble) - centerY(desktop.save)) <= 1, 'desktop: bubble and Save share a row');
  } finally {
    await browser?.close();
    await server?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
