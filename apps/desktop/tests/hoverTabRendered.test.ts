import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { chromium } from 'playwright';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';
import test from 'node:test';

const fixtureRoot = new URL('./fixtures/', import.meta.url);

interface CornerRadii {
  topLeft: string;
  topRight: string;
  bottomRight: string;
  bottomLeft: string;
}

interface HoverTabMeasurement {
  host: { width: number; height: number; scrollWidth: number; scrollHeight: number };
  pill: { width: number; height: number; scrollWidth: number; scrollHeight: number; radii: CornerRadii };
  button: { width: number; height: number; borderColor: string; ariaLabel: string | null; title: string | null; ariaBusy: string | null; ariaKeyshortcuts: string | null; nativeTooltipAllowed: boolean; radii: CornerRadii };
  actionCount: number;
  menuCount: number;
  shared: boolean;
}

test('rendered hover-tab fixture stays fixed and separates primary action from the native menu', async () => {
  // `os.tmpdir()`, not TEMP/TMP with a '.' fallback: those are unset on macOS
  // and Linux, so the fallback made `buildDir` RELATIVE -- and vite resolves a
  // relative `outDir` against `root` (the fixture dir) while the goto below
  // resolves it against the CWD. The build succeeded and the test then looked
  // for the html somewhere it was never written, failing everywhere but
  // Windows and leaving a stray build dir in the repo.
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-hover-tab-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: {
        tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } })
      },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: { alias: { '@petal/shared': resolve(fileURLToPath(new URL('../../../shared', import.meta.url))) } },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fileURLToPath(new URL('./hover-tab-rendered.html', fixtureRoot)) }
      }
    });

    browser = await chromium.launch({
      headless: true,
      args: ['--allow-file-access-from-files', '--disable-gpu']
    });
    for (const userAgent of [
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605.1.15 Version/17.0 Safari/605.1.15',
      'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36'
    ]) {
      const page = await browser.newPage({ viewport: { width: 320, height: 140 }, userAgent });
      const nativeTooltipExpected = /Windows NT/i.test(userAgent);
      await page.goto(pathToFileURL(join(buildDir, 'hover-tab-rendered.html')).href);
      await page.waitForFunction(() => document.body.dataset.hoverTabReady === 'true');

      const measure = () => page.evaluate(() => {
        const host = document.querySelector<HTMLElement>('.hover-tab-host')!;
        const pill = document.querySelector<HTMLElement>('.pill')!;
        const button = document.querySelector<HTMLButtonElement>('.hover-tab-action')!;
        const pillStyle = getComputedStyle(pill);
        const buttonStyle = getComputedStyle(button);
        const rect = button.getBoundingClientRect();
        const fixture = (window as any).hoverTabFixture;
        return {
          host: { width: host.clientWidth, height: host.clientHeight, scrollWidth: host.scrollWidth, scrollHeight: host.scrollHeight },
          pill: { width: pill.getBoundingClientRect().width, height: pill.getBoundingClientRect().height, scrollWidth: pill.scrollWidth, scrollHeight: pill.scrollHeight, radii: { topLeft: pillStyle.borderTopLeftRadius, topRight: pillStyle.borderTopRightRadius, bottomRight: pillStyle.borderBottomRightRadius, bottomLeft: pillStyle.borderBottomLeftRadius } },
          button: { width: rect.width, height: rect.height, borderColor: buttonStyle.borderTopColor, ariaLabel: button.getAttribute('aria-label'), title: button.getAttribute('title'), ariaBusy: button.getAttribute('aria-busy'), ariaKeyshortcuts: button.getAttribute('aria-keyshortcuts'), nativeTooltipAllowed: button.hasAttribute('data-allow-native-tooltip'), radii: { topLeft: buttonStyle.borderTopLeftRadius, topRight: buttonStyle.borderTopRightRadius, bottomRight: buttonStyle.borderBottomRightRadius, bottomLeft: buttonStyle.borderBottomLeftRadius } },
          actionCount: fixture.getShareClicks(),
          menuCount: fixture.getMenuOpens(),
          shared: fixture.getShared()
        } satisfies HoverTabMeasurement;
      });

      const outsidePillRadii = { topLeft: '0px', topRight: '12px', bottomRight: '12px', bottomLeft: '0px' };
      const outsideButtonRadii = { topLeft: '0px', topRight: '10px', bottomRight: '10px', bottomLeft: '0px' };
      const insetPillRadii = { topLeft: '12px', topRight: '0px', bottomRight: '0px', bottomLeft: '12px' };
      const insetButtonRadii = { topLeft: '10px', topRight: '0px', bottomRight: '0px', bottomLeft: '10px' };

      const initial = await measure();
      assert.deepEqual(initial.host, { width: 40, height: 40, scrollWidth: 40, scrollHeight: 40 });
      assert.equal(initial.pill.width, 40);
      assert.equal(initial.pill.height, 40);
      assert.deepEqual(initial.pill.radii, outsidePillRadii);
      assert.equal(initial.button.width, 40);
      assert.equal(initial.button.height, 40);
      assert.equal(initial.button.borderColor, 'rgb(127, 240, 163)');
      assert.deepEqual(initial.button.radii, outsideButtonRadii);
      assert.equal(initial.button.ariaLabel, 'Share this window. Drag vertically to move; right-click for options');
      assert.equal(
        initial.button.title,
        nativeTooltipExpected ? 'Share this window — drag to move; right-click for options' : null
      );
      assert.equal(initial.button.nativeTooltipAllowed, nativeTooltipExpected);
      assert.equal(initial.button.ariaBusy, 'false');
      assert.equal(initial.button.ariaKeyshortcuts, 'Shift+F10,ContextMenu');
      assert.equal(await page.locator('.hover-tab-action').count(), 1);
      assert.equal(await page.locator('.hover-tab-options').count(), 0);
      assert.equal(await page.locator('.hover-tab-trigger').count(), 1);
      assert.equal(await page.locator('.hover-tab-tray').count(), 0);
      assert.ok(initial.host.scrollWidth <= 40);
      assert.ok(initial.host.scrollHeight <= 40);

      await page.mouse.move(20, 20);
      await page.waitForTimeout(220);
      const hoveredOutside = await measure();
      assert.deepEqual(hoveredOutside.host, initial.host, 'stationary hover must not change geometry');
      assert.deepEqual(hoveredOutside.pill.radii, outsidePillRadii, 'outside shell keeps sharp target-facing corners on hover');
      assert.deepEqual(hoveredOutside.button.radii, outsideButtonRadii, 'outside action keeps sharp target-facing corners on hover');
      assert.equal(await page.evaluate(() => document.activeElement?.classList.contains('hover-tab-action')), false, 'passive hover must not steal focus');

      await page.evaluate(() => (window as any).hoverTabFixture.setInset(true));
      await page.waitForFunction(() => document.querySelector<HTMLElement>('.hover-tab-host')?.classList.contains('inset') === true);
      let insetState = await measure();
      assert.deepEqual(insetState.pill.radii, insetPillRadii);
      assert.deepEqual(insetState.button.radii, insetButtonRadii);
      await page.mouse.move(20, 20);
      await page.waitForTimeout(220);
      insetState = await measure();
      assert.deepEqual(insetState.pill.radii, insetPillRadii, 'inset shell keeps sharp target-facing corners on hover');
      assert.deepEqual(insetState.button.radii, insetButtonRadii, 'inset action keeps sharp target-facing corners on hover');
      await page.evaluate(() => (window as any).hoverTabFixture.setInset(false));
      await page.waitForFunction(() => document.querySelector<HTMLElement>('.hover-tab-host')?.classList.contains('inset') === false);

      const action = page.locator('.hover-tab-action');
      await action.click();
      await page.waitForFunction(() => (window as any).hoverTabFixture.getShared() === true);
      let state = await measure();
      assert.equal(state.actionCount, 1);
      assert.equal(state.menuCount, 0);
      assert.equal(state.button.ariaLabel, 'Stop sharing. Drag vertically to move; right-click for options');
      assert.equal(
        state.button.title,
        nativeTooltipExpected ? 'Stop sharing — drag to move; right-click for options' : null
      );
      assert.equal(state.button.nativeTooltipAllowed, nativeTooltipExpected);
      assert.equal(state.shared, true);
      assert.notEqual(state.button.borderColor, initial.button.borderColor, 'shared tabs do not use the unshared live border');
      assert.equal(state.host.width, 40);
      assert.equal(state.host.height, 40);
      assert.deepEqual(state.button.radii, outsideButtonRadii);
      assert.equal(await page.locator('.hover-tab-live-dot').count(), 1);

      await action.click();
      await page.waitForFunction(() => (window as any).hoverTabFixture.getShared() === false);
      state = await measure();
      assert.equal(state.actionCount, 2);
      assert.equal(state.button.ariaLabel, 'Share this window. Drag vertically to move; right-click for options');
      assert.equal(state.button.borderColor, initial.button.borderColor, 'unshared tabs use the bright live border');
      assert.equal(state.host.width, 40);
      assert.equal(state.host.height, 40);

      await action.focus();
      await action.press('Enter');
      await page.waitForFunction(() => (window as any).hoverTabFixture.getShared() === true);
      await action.press('Space');
      await page.waitForFunction(() => (window as any).hoverTabFixture.getShared() === false);
      state = await measure();
      assert.equal(state.actionCount, 4, 'Enter and Space each perform one direct action');
      assert.equal(state.menuCount, 0, 'primary keyboard activation must not open the menu');

      await action.click({ button: 'right' });
      await page.waitForFunction(() => (window as any).hoverTabFixture.getMenuOpens() === 1);
      state = await measure();
      assert.equal(state.actionCount, 4, 'right-click must not toggle sharing');
      assert.equal(state.menuCount, 1);
      await action.focus();
      await page.evaluate(() => {
        const button = document.querySelector<HTMLButtonElement>('.hover-tab-action')!;
        button.dispatchEvent(new KeyboardEvent('keydown', {
          key: 'F10',
          code: 'F10',
          shiftKey: true,
          bubbles: true,
          cancelable: true
        }));
      });
      await page.waitForFunction(() => (window as any).hoverTabFixture.getMenuOpens() === 2);
      await page.evaluate(() => {
        const button = document.querySelector<HTMLButtonElement>('.hover-tab-action')!;
        button.dispatchEvent(new KeyboardEvent('keydown', {
          key: 'ContextMenu',
          code: 'ContextMenu',
          bubbles: true,
          cancelable: true
        }));
      });
      await page.waitForFunction(() => (window as any).hoverTabFixture.getMenuOpens() === 3);
      state = await measure();
      assert.equal(state.actionCount, 4, 'keyboard menu shortcuts must not toggle sharing');
      assert.equal(state.menuCount, 3);
      assert.equal(await page.evaluate(() => (window as any).hoverTabFixture.getLastMenuInvocation()), 'keyboard');
      assert.equal(state.host.width, 40);
      assert.equal(state.host.height, 40);

      await page.evaluate(() => (window as any).hoverTabFixture.setShared(true));
      await page.waitForFunction(() => document.querySelector<HTMLButtonElement>('.hover-tab-action')?.getAttribute('aria-label') === 'Stop sharing. Drag vertically to move; right-click for options');
      assert.equal((await measure()).button.ariaLabel, 'Stop sharing. Drag vertically to move; right-click for options');
      await page.close();
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
