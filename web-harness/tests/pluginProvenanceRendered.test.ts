// MEASURES the host's provenance caption in a real browser, with the real
// stylesheet and the real UI font, for the worst case `validateManifest`
// accepts: the longest allowed plugin name (24 chars) in a popover whose
// declared width is the adversarial floor (1 px). Reading the CSS cannot tell
// "fits" from "clipped" -- a plugin-declared width used to size the popover
// clipped the host's own "· plugin" caption out of view
// (kiruna-labs/petal#71 review, finding 1). It also asserts the caption names
// the plugin's SOURCE, which is the host's own record, not plugin-authored
// text (finding 3a).
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import test from 'node:test';
import { build } from 'vite';

const repoRoot = resolve(import.meta.dirname, '../..');
const fixtureRoot = resolve(repoRoot, 'web-harness/tests/fixtures/plugins');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

interface CaptionMeasurement {
  text: string;
  title: string | null;
  scrollWidth: number;
  clientWidth: number;
  scrollHeight: number;
  clientHeight: number;
  spanScrollWidth: number;
  spanClientWidth: number;
  popoverWidth: number;
  captionRight: number;
  captionBottom: number;
  popoverRight: number;
  popoverBottom: number;
}

test('the plugin popover caption stays fully visible at the worst width a manifest may declare', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-plugin-provenance-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  try {
    await build({
      root: fixtureRoot,
      configFile: false,
      logLevel: 'silent',
      base: './',
      server: { fs: { allow: [repoRoot] } },
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: resolve(fixtureRoot, 'provenance.html') } },
    });

    browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files'] });
    // The real main-window width; the popover layer is the full viewport.
    const page = await browser.newPage({ viewport: { width: 400, height: 600 } });
    await page.goto(pathToFileURL(join(buildDir, 'provenance.html')).href);
    await page.waitForFunction(() => document.body.dataset.ready === 'true');
    assert.deepEqual(await page.evaluate(() => (window as any).__probe.errors), []);
    // Albert Sans, not a fallback: a narrower fallback would understate the caption.
    const fontLoaded = await page.evaluate(async () => {
      await document.fonts.load('700 10px "Albert Sans"');
      await document.fonts.ready;
      return document.fonts.check('700 10px "Albert Sans"');
    });
    assert.equal(fontLoaded, true, 'the real UI font loaded');

    const measure = async (pluginId: string): Promise<CaptionMeasurement> => {
      await page.click(`#btn-${pluginId.replace(/\./g, '\\.')}-open`);
      await page.locator('.petal-plugin-popover .petal-plugin-caption').waitFor();
      const m = await page.evaluate(() => {
        const caption = document.querySelector<HTMLElement>('.petal-plugin-popover .petal-plugin-caption')!;
        const span = caption.querySelector<HTMLElement>('span')!;
        const popover = caption.parentElement as HTMLElement;
        const cr = caption.getBoundingClientRect();
        const pr = popover.getBoundingClientRect();
        return {
          text: (caption.textContent ?? '').trim(),
          title: caption.getAttribute('title'),
          scrollWidth: caption.scrollWidth,
          clientWidth: caption.clientWidth,
          scrollHeight: caption.scrollHeight,
          clientHeight: caption.clientHeight,
          spanScrollWidth: span.scrollWidth,
          spanClientWidth: span.clientWidth,
          popoverWidth: Math.round(pr.width),
          captionRight: cr.right,
          captionBottom: cr.bottom,
          popoverRight: pr.right,
          popoverBottom: pr.bottom,
        };
      });
      await page.keyboard.press('Escape');
      await page.waitForFunction(() => document.querySelector('.petal-plugin-popover') === null);
      return m as CaptionMeasurement;
    };

    const fits = (m: CaptionMeasurement, where: string) => {
      assert.ok(m.scrollWidth <= m.clientWidth + 0.5, `${where}: caption clipped horizontally (scrollWidth ${m.scrollWidth} > clientWidth ${m.clientWidth}) -- ${JSON.stringify(m)}`);
      assert.ok(m.spanScrollWidth <= m.spanClientWidth + 0.5, `${where}: caption text clipped (scrollWidth ${m.spanScrollWidth} > clientWidth ${m.spanClientWidth}) -- ${JSON.stringify(m)}`);
      assert.ok(m.scrollHeight <= m.clientHeight + 0.5, `${where}: caption clipped vertically (scrollHeight ${m.scrollHeight} > clientHeight ${m.clientHeight}) -- ${JSON.stringify(m)}`);
      // The popover sets `overflow: hidden`, so "inside its own box" is part of "visible".
      assert.ok(m.captionRight <= m.popoverRight + 0.5, `${where}: caption escapes the popover (${m.captionRight} > ${m.popoverRight})`);
      assert.ok(m.captionBottom <= m.popoverBottom + 0.5, `${where}: caption clipped by the popover (${m.captionBottom} > ${m.popoverBottom})`);
    };

    // The adversarial case: 24 'W's -- the longest name the validator allows,
    // in the widest glyph -- in a popover that declared width: 1.
    const hostile = await measure('acme.hostile');
    fits(hostile, 'hostile width');
    assert.equal(hostile.text, `${'W'.repeat(24)} · installed plugin`);
    assert.equal(hostile.title, `${'W'.repeat(24)} · installed plugin`);

    // A declared width is still honoured when it is already caption-safe.
    const builtin = await measure('petal.reactions');
    fits(builtin, 'builtin');
    assert.equal(builtin.text, 'Reactions · built-in plugin');
    assert.equal(builtin.popoverWidth, 296, 'a caption-safe declared width is used as declared');

    // The source is the HOST's record (LoadedPlugin.source), not manifest text:
    // a sideloaded plugin calling itself "Reactions" cannot claim "built-in".
    const dev = await measure('dev.local-plugin');
    fits(dev, 'dev');
    assert.equal(dev.text, 'Local Build · dev plugin');
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
