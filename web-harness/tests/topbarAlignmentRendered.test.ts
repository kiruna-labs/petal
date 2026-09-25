// #241, rendered: the meeting top bar's name, time, title actions and every
// control in its right cluster share one control height and one centre line.
// Reading the CSS cannot show that -- the centre line falls out of flex
// alignment, line-heights and each control's padding and border together --
// so measure the real stylesheet in Chromium at a desktop, a tablet and a
// phone width, and on a touch screen.
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

interface Box {
  part: string;
  height: number;
  centre: number;
}

interface TopbarReading {
  controlHeight: number;
  hoverNone: boolean;
  timeOpacity: string;
  title: Box[];
  controls: Box[];
}

test('web meeting top bar: title row and every top-bar control share one height and one centre line (#241)', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-browser-topbar-alignment-build-'));
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

    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files']
    });

    for (const { width, touch } of [
      { width: 1280, touch: false },
      { width: 760, touch: false },
      { width: 400, touch: false },
      { width: 400, touch: true }
    ]) {
      const context = await browser.newContext({ viewport: { width, height: 800 }, hasTouch: touch, isMobile: touch });
      const page = await context.newPage();
      await page.goto(pathToFileURL(join(buildDir, 'index.html')).href, { waitUntil: 'load' });
      // tileLayout.ts inserts the pill at startup.
      await page.waitForSelector('.layout-picker', { state: 'attached' });
      const reading: TopbarReading = await page.evaluate(async () => {
        document.querySelector('#meeting-screen')?.classList.remove('hidden');
        document.querySelector('#join-screen')?.classList.add('hidden');
        document.querySelector('#room-name')!.textContent = 'Petal meeting';
        // A keyed build reveals the bug-report trigger; this one has no
        // UserDispatch key.
        document.querySelector<HTMLButtonElement>('#feedback-meeting-trigger')!.hidden = false;
        // The autoplay-unlock prompt, as connection.ts's
        // ensureAudioPlaybackPrompt prepends it, so it is measured too.
        const prompt = document.createElement('button');
        prompt.type = 'button';
        prompt.className = 'audio-playback-prompt';
        prompt.textContent = 'Enable audio';
        document.querySelector('.topbar-right')!.prepend(prompt);
        await document.fonts.ready;

        // Anonymous callbacks only: tsx names a `const f = () => ...` with a
        // `__name` helper that does not exist in the page.
        const [title, controls] = [
          ['#room-name', '#elapsed', '#room-copy'].map((selector) => document.querySelector<HTMLElement>(selector)!),
          // Every rendered, in-flow child of the right cluster, so a control
          // added there later is held to the same height and centre line.
          Array.from(document.querySelector('.topbar-right')!.children).filter((element) => {
            const style = getComputedStyle(element);
            const rect = element.getBoundingClientRect();
            return style.display !== 'none' && !['absolute', 'fixed'].includes(style.position) && rect.height > 0;
          }) as HTMLElement[]
        ].map((elements) =>
          elements.map((element) => {
            const rect = element.getBoundingClientRect();
            return { part: element.id || element.className, height: rect.height, centre: rect.top + rect.height / 2 };
          })
        );
        return {
          controlHeight: parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--topbar-control-height')),
          hoverNone: matchMedia('(hover: none)').matches,
          timeOpacity: getComputedStyle(document.querySelector('#elapsed')!).opacity,
          title,
          controls
        };
      });
      const where = `${width}px${touch ? ' touch' : ''}: ${JSON.stringify(reading)}`;

      assert.ok(reading.controlHeight > 0, `--topbar-control-height did not resolve at ${where}`);
      const parts = reading.controls.map((box) => box.part);
      for (const expected of ['audio-playback-prompt', 'layout-picker', 'feedback-meeting-cell']) {
        assert.ok(parts.includes(expected), `${expected} was not measured at ${where}`);
      }
      for (const control of reading.controls) {
        assert.equal(control.height, reading.controlHeight, `${control.part} height at ${where}`);
      }
      const centres = [...reading.title, ...reading.controls].map((box) => box.centre);
      assert.ok(
        Math.max(...centres) - Math.min(...centres) <= 1,
        `the title row and top-bar controls are off one centre line at ${where}`
      );

      // A mouse can hover, so there the time keeps its hover reveal; a touch
      // screen cannot, so there it shows at rest.
      assert.equal(reading.hoverNone, touch, `(hover: none) at ${where}`);
      assert.equal(reading.timeOpacity, touch ? '1' : '0', `time visibility at rest at ${where}`);
      await context.close();
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
