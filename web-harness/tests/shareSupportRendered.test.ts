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

type Browser = Awaited<ReturnType<typeof chromium.launch>>;

interface RenderedControls {
  apiPresent: boolean;
  shareCellDisplay: string;
  shareButtonWidth: number;
  shareFocusable: boolean;
  micButtonWidth: number;
  testPatternShareWidth: number;
  pageErrors: string[];
}

const PHONE_PORTRAIT = { viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true };
const PHONE_LANDSCAPE = { viewport: { width: 844, height: 390 }, isMobile: true, hasTouch: true };
const DESKTOP = { viewport: { width: 1280, height: 800 } };

async function buildHarness(buildDir: string) {
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
}

// Chrome for Android and iPhone Safari have no getDisplayMedia, but headless
// Chromium keeps it even under phone emulation -- so a phone is emulated by
// deleting it before the app's own scripts run.
async function renderControls(
  browser: Browser,
  buildDir: string,
  contextOptions: typeof PHONE_PORTRAIT | typeof DESKTOP,
  { dropGetDisplayMedia }: { dropGetDisplayMedia: boolean }
): Promise<RenderedControls> {
  const context = await browser.newContext(contextOptions);
  try {
    const page = await context.newPage();
    const pageErrors: string[] = [];
    page.on('pageerror', (err: Error) => pageErrors.push(err.message));
    if (dropGetDisplayMedia) {
      await page.addInitScript(() => {
        delete (MediaDevices.prototype as Partial<MediaDevices>).getDisplayMedia;
      });
    }
    await page.goto(pathToFileURL(join(buildDir, 'index.html')).href, { waitUntil: 'load' });
    await page.waitForSelector('#ctl-share', { state: 'attached' });
    await page.evaluate(() => {
      document.querySelector('#meeting-screen')?.classList.remove('hidden');
      document.querySelector('#join-screen')?.classList.add('hidden');
      document.querySelector<HTMLDetailsElement>('#dev-panel')!.open = true;
    });
    const rendered = await page.evaluate(() => {
      const share = document.querySelector<HTMLElement>('#ctl-share')!;
      // A keyboard user must not be able to tab onto a control that is gone.
      share.focus();
      const shareFocusable = document.activeElement === share;
      share.blur();
      return {
        apiPresent: typeof navigator.mediaDevices?.getDisplayMedia === 'function',
        shareCellDisplay: getComputedStyle(share.closest('.control-cell')!).display,
        shareButtonWidth: share.getBoundingClientRect().width,
        shareFocusable,
        micButtonWidth: document.querySelector('#ctl-audio')!.getBoundingClientRect().width,
        testPatternShareWidth: document.querySelector('#share-btn')!.getBoundingClientRect().width,
      };
    });
    return { ...rendered, pageErrors };
  } finally {
    await context.close();
  }
}

test('the Share control is hidden where getDisplayMedia is missing and shown where it exists', { timeout: 120_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-browser-share-support-build-'));
  let browser: Browser | undefined;

  try {
    await buildHarness(buildDir);
    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files']
    });

    // Both phone orientations: they fall into different media queries, and a
    // rule in any of them that re-declares `display` on a cell undoes the hide.
    for (const [name, device] of [['portrait', PHONE_PORTRAIT], ['landscape', PHONE_LANDSCAPE]] as const) {
      const phone = await renderControls(browser, buildDir, device, { dropGetDisplayMedia: true });
      assert.deepEqual(phone.pageErrors, [], `phone ${name}: the app must boot cleanly`);
      assert.equal(phone.apiPresent, false, `phone ${name}: the emulation must actually remove getDisplayMedia`);
      assert.equal(phone.shareCellDisplay, 'none', `phone ${name}: the Share cell must not render`);
      assert.equal(phone.shareButtonWidth, 0, `phone ${name}: the Share button must take no space`);
      assert.equal(phone.shareFocusable, false, `phone ${name}: the hidden Share button must not take focus`);
      assert.ok(phone.micButtonWidth > 0, `phone ${name}: only Share goes, not the rest of the bar`);
      // The dev panel's test-pattern share is canvas capture, not display
      // capture, and automation drives it on every platform.
      assert.ok(phone.testPatternShareWidth > 0, `phone ${name}: the test-pattern share must stay`);
    }

    const desktop = await renderControls(browser, buildDir, DESKTOP, { dropGetDisplayMedia: false });
    assert.deepEqual(desktop.pageErrors, []);
    assert.equal(desktop.apiPresent, true);
    assert.notEqual(desktop.shareCellDisplay, 'none');
    assert.equal(desktop.shareFocusable, true, 'desktop: the Share button must take focus');
    assert.ok(desktop.shareButtonWidth > 0, 'desktop: the Share button must render');
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
