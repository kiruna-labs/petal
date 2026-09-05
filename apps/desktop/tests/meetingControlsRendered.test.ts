import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import test from 'node:test';
import { chromium } from 'playwright';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

function fixturePath(name: string): string {
  return fileURLToPath(new URL(name, fixtureRoot));
}

test('expanded gallery device selectors stay open and restore focus', { timeout: 30_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-meeting-controls-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: {
        tsconfigRaw: JSON.stringify({
          compilerOptions: { target: 'ES2022', useDefineForClassFields: true }
        })
      },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          '$app/environment': fixturePath('sveltekit-environment.ts'),
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot)))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fixturePath('meeting-controls.html') }
      }
    });

    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files']
    });
    const page = await browser.newPage({ viewport: { width: 840, height: 560 } });
    await page.goto(pathToFileURL(join(buildDir, 'meeting-controls.html')).href, {
      waitUntil: 'load'
    });
    await page.waitForFunction(
      () => document.body.dataset.fixtureReady === 'true' || !!document.body.dataset.fixtureError,
      { timeout: 10_000 }
    );
    const fixtureError = await page.locator('body').getAttribute('data-fixture-error');
    assert.equal(fixtureError, null, fixtureError ? decodeURIComponent(fixtureError) : '');

    const largeStage = page.locator('.large-stage');
    const mic = largeStage.getByRole('button', { name: 'Microphone options' });
    const camera = largeStage.getByRole('button', { name: 'Camera options' });
    const share = largeStage.getByRole('button', { name: 'Share a window' });
    const micSplit = mic.locator('..');
    const cameraSplit = camera.locator('..');
    const micPrimary = micSplit.locator('.control-button').first();
    const regressionFailures: string[] = [];
    const micIdleBackground = await micSplit.evaluate((element) => getComputedStyle(element).backgroundColor);
    const cameraIdleBackground = await cameraSplit.evaluate((element) => getComputedStyle(element).backgroundColor);
    const adjacentIdleBackground = await largeStage
      .locator('.control-cell')
      .nth(2)
      .locator('.control-button')
      .evaluate((element) => getComputedStyle(element).backgroundColor);
    if (micIdleBackground !== adjacentIdleBackground) {
      regressionFailures.push(`mic idle ${micIdleBackground} !== adjacent ${adjacentIdleBackground}`);
    }
    if (cameraIdleBackground !== adjacentIdleBackground) {
      regressionFailures.push(`camera idle ${cameraIdleBackground} !== adjacent ${adjacentIdleBackground}`);
    }
    if (
      (await micPrimary.evaluate((element) => getComputedStyle(element).backgroundColor)) !==
      'rgba(0, 0, 0, 0)'
    ) {
      regressionFailures.push('mic primary segment is highlighted at rest');
    }
    if (
      (await mic.evaluate((element) => getComputedStyle(element).backgroundColor)) !==
      'rgba(0, 0, 0, 0)'
    ) {
      regressionFailures.push('mic options segment is highlighted at rest');
    }
    await micPrimary.hover();
    await page.waitForTimeout(160);
    const hoverBackground = await micPrimary.evaluate((element) => getComputedStyle(element).backgroundColor);
    assert.notEqual(hoverBackground, 'rgba(0, 0, 0, 0)');
    await page.mouse.move(0, 0);
    await page.setViewportSize({ width: 520, height: 360 });
    await page.waitForTimeout(100);
    await mic.click();
    await page.waitForTimeout(160);
    const openBackground = await mic.evaluate((element) => getComputedStyle(element).backgroundColor);
    assert.notEqual(openBackground, 'rgba(0, 0, 0, 0)');
    await page.locator('.devices-menu.placed').waitFor({ timeout: 2_000 });
    assert.equal(await mic.getAttribute('aria-expanded'), 'true');
    assert.equal(await camera.getAttribute('aria-expanded'), 'false');
    assert.equal(await page.locator('.devices-menu.placed').count(), 1);

    const narrowGeometry = await page.evaluate(() => {
      const menu = document.querySelector('.devices-menu');
      const picker = document.querySelector('.device-picker');
      const trigger = document.querySelector('.large-stage [aria-label="Microphone options"]');
      const controlbar = document.querySelector('.large-stage .controlbar');
      if (!(menu instanceof HTMLElement) || !(picker instanceof HTMLElement) ||
          !(trigger instanceof HTMLElement) || !(controlbar instanceof HTMLElement)) return null;
      const menuRect = menu.getBoundingClientRect();
      const triggerRect = trigger.getBoundingClientRect();
      const controlbarRect = controlbar.getBoundingClientRect();
      const pickerRect = picker.getBoundingClientRect();
      const lowerCorner = document.elementFromPoint(
        Math.min(innerWidth - 1, Math.max(0, menuRect.right - 3)),
        Math.min(innerHeight - 1, Math.max(0, menuRect.bottom - 3))
      );
      const verticalGap = menuRect.bottom <= controlbarRect.top
        ? controlbarRect.top - menuRect.bottom
        : menuRect.top - controlbarRect.bottom;
      return {
        viewport: { width: innerWidth, height: innerHeight },
        menu: { top: menuRect.top, right: menuRect.right, bottom: menuRect.bottom },
        picker: { bottom: pickerRect.bottom, maxHeight: getComputedStyle(picker).maxHeight },
        trigger: { top: triggerRect.top, bottom: triggerRect.bottom },
        controlbar: { top: controlbarRect.top, bottom: controlbarRect.bottom },
        verticalGap,
        overlapsControlbar: menuRect.bottom > controlbarRect.top && menuRect.top < controlbarRect.bottom,
        pickerScrollable:
          picker.scrollHeight > picker.clientHeight && getComputedStyle(picker).overflowY === 'auto',
        lowerCornerClass: lowerCorner?.className ?? lowerCorner?.tagName ?? null
      };
    });
    if (!narrowGeometry) {
      regressionFailures.push('expected rendered picker geometry');
    } else {
      if (narrowGeometry.menu.top < 0) {
        regressionFailures.push(`picker top is outside viewport: ${JSON.stringify(narrowGeometry)}`);
      }
      if (narrowGeometry.menu.bottom > narrowGeometry.viewport.height) {
        regressionFailures.push(`picker bottom is outside viewport: ${JSON.stringify(narrowGeometry)}`);
      }
      if (narrowGeometry.overlapsControlbar) {
        regressionFailures.push(`picker overlaps action bar: ${JSON.stringify(narrowGeometry)}`);
      }
      if (Math.round(narrowGeometry.verticalGap) !== 8) {
        regressionFailures.push(`picker anchor gap drifted: ${JSON.stringify(narrowGeometry)}`);
      }
      if (!narrowGeometry.pickerScrollable) {
        regressionFailures.push(`device picker is not scrollable: ${JSON.stringify(narrowGeometry)}`);
      }
      if (!/device-picker|devices-menu/.test(String(narrowGeometry.lowerCornerClass))) {
        regressionFailures.push(`picker lower corner hit ${String(narrowGeometry.lowerCornerClass)}`);
      }
    }
    assert.deepEqual(regressionFailures, [], `meeting-control regressions:\n${regressionFailures.join('\n')}`);

    // Wheel input over a device row must chain to the outer picker after the
    // former inner list reaches its boundary. The old nested `.device-list`
    // scroller used `overscroll-behavior: contain`, so the second wheel was
    // trapped over the row while the same wheel over the heading moved the
    // outer picker.
    const deviceRow = page.locator('.device-row').first();
    const readDeviceScroll = () =>
      page.evaluate(() => {
        const picker = document.querySelector<HTMLElement>('.device-picker');
        const list = document.querySelector<HTMLElement>('.device-list');
        return {
          pickerTop: picker?.scrollTop ?? 0,
          pickerMax: Math.max(0, (picker?.scrollHeight ?? 0) - (picker?.clientHeight ?? 0)),
          listTop: list?.scrollTop ?? 0,
          listMax: Math.max(0, (list?.scrollHeight ?? 0) - (list?.clientHeight ?? 0))
        };
      });
    await page.evaluate(() => {
      const picker = document.querySelector<HTMLElement>('.device-picker');
      const list = document.querySelector<HTMLElement>('.device-list');
      if (picker) picker.scrollTop = 0;
      if (list) list.scrollTop = 0;
    });
    await deviceRow.hover();
    await page.mouse.wheel(0, 180);
    await page.waitForTimeout(50);
    const atFormerInnerBoundary = await readDeviceScroll();
    await page.mouse.wheel(0, 120);
    await page.waitForTimeout(50);
    const afterRowWheel = await readDeviceScroll();
    assert.equal(atFormerInnerBoundary.listTop, atFormerInnerBoundary.listMax);
    assert.ok(
      afterRowWheel.pickerTop > atFormerInnerBoundary.pickerTop,
      `wheel over a device row was trapped at the inner boundary: ${JSON.stringify({ atFormerInnerBoundary, afterRowWheel })}`
    );
    assert.equal(afterRowWheel.listMax, 0, `device rows must not own a nested scrollbar: ${JSON.stringify(afterRowWheel)}`);

    // Outside action clicks must dismiss the picker without consuming the
    // clicked action. This is the red regression for the reported bug: the
    // old full-viewport backdrop sat below the action bar, so this click
    // invoked Share while leaving the device menu mounted.
    await share.click();
    // #887: wait for the dismissal to LAND, don't sleep past it. The menu
    // leaves via `restrainedSurfaceExitTransition` (MOTION_EXIT_MS=120) plus
    // the dismissible layer's rAF hop -- measured 190-195ms from pointerdown
    // to DOM removal. A fixed 160ms sleep sat a few ms inside that window, so
    // any small scheduling change flipped this to red (5bc57b2f did) while
    // the product behavior was correct the whole time. The bounded wait still
    // fails loudly if dismissal genuinely stops happening.
    await page.locator('.devices-menu').waitFor({ state: 'detached', timeout: 2_000 });
    assert.equal(await page.locator('.devices-menu').count(), 0);
    assert.deepEqual(
      await page.evaluate(() => (window as typeof window & { __meetingControlActions?: string[] }).__meetingControlActions),
      ['screenshare']
    );
    assert.equal(
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.getAttribute('aria-label')),
      'Share a window'
    );

    await mic.click();
    await page.locator('.devices-menu.placed').waitFor({ timeout: 2_000 });
    await page.keyboard.press('Escape');
    // #887: wait for the dismissal to LAND, don't sleep past it. The menu
    // leaves via `restrainedSurfaceExitTransition` (MOTION_EXIT_MS=120) plus
    // the dismissible layer's rAF hop -- measured 190-195ms from pointerdown
    // to DOM removal. A fixed 160ms sleep sat a few ms inside that window, so
    // any small scheduling change flipped this to red (5bc57b2f did) while
    // the product behavior was correct the whole time. The bounded wait still
    // fails loudly if dismissal genuinely stops happening.
    await page.locator('.devices-menu').waitFor({ state: 'detached', timeout: 2_000 });
    assert.equal(await page.locator('.devices-menu').count(), 0);
    assert.equal(
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.getAttribute('aria-label')),
      'Microphone options'
    );

    await camera.click();
    await page.locator('.devices-menu.placed').waitFor({ timeout: 2_000 });
    assert.equal(await camera.getAttribute('aria-expanded'), 'true');
    assert.equal(await mic.getAttribute('aria-expanded'), 'false');
    assert.equal(await page.locator('.devices-menu.placed').count(), 1);

    await page.mouse.click(1, 1);
    // #887: wait for the dismissal to LAND, don't sleep past it. The menu
    // leaves via `restrainedSurfaceExitTransition` (MOTION_EXIT_MS=120) plus
    // the dismissible layer's rAF hop -- measured 190-195ms from pointerdown
    // to DOM removal. A fixed 160ms sleep sat a few ms inside that window, so
    // any small scheduling change flipped this to red (5bc57b2f did) while
    // the product behavior was correct the whole time. The bounded wait still
    // fails loudly if dismissal genuinely stops happening.
    await page.locator('.devices-menu').waitFor({ state: 'detached', timeout: 2_000 });
    assert.equal(await page.locator('.devices-menu').count(), 0);
    assert.equal(
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.getAttribute('aria-label')),
      'Camera options'
    );

    await mic.click();
    await page.locator('.devices-menu.placed').waitFor({ timeout: 2_000 });
    await mic.click();
    // #887: wait for the dismissal to LAND, don't sleep past it. The menu
    // leaves via `restrainedSurfaceExitTransition` (MOTION_EXIT_MS=120) plus
    // the dismissible layer's rAF hop -- measured 190-195ms from pointerdown
    // to DOM removal. A fixed 160ms sleep sat a few ms inside that window, so
    // any small scheduling change flipped this to red (5bc57b2f did) while
    // the product behavior was correct the whole time. The bounded wait still
    // fails loudly if dismissal genuinely stops happening.
    await page.locator('.devices-menu').waitFor({ state: 'detached', timeout: 2_000 });
    assert.equal(await page.locator('.devices-menu').count(), 0);
    assert.equal(await mic.getAttribute('aria-expanded'), 'false');

    await page.emulateMedia({ reducedMotion: 'reduce' });
    const reducedMotion = await page.evaluate(() => {
      const root = getComputedStyle(document.documentElement);
      const stage = document.querySelector('.large-stage');
      return {
        feedback: root.getPropertyValue('--motion-feedback').trim(),
        enter: root.getPropertyValue('--motion-enter').trim(),
        distance: root.getPropertyValue('--motion-distance').trim(),
        pressScale: root.getPropertyValue('--press-scale').trim(),
        stageTransition: stage ? getComputedStyle(stage).transitionDuration : null
      };
    });
    assert.deepEqual(reducedMotion, {
      feedback: '0ms',
      enter: '0ms',
      distance: '0px',
      pressScale: '1',
      stageTransition: '0s'
    });
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
