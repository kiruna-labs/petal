// The one test that runs plugins in a REAL browser: proves the sandbox
// (no Tauri or host globals leak into a plugin frame, no network), the boot
// sequence (init -> activated), host-drawn toolbar buttons, toasts, and the
// reactions popover -> overlay path via the logic<->surface channel.
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

test('plugin frames are sandboxed, boot, draw buttons, toast, and route popover picks to the overlay', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-plugin-sandbox-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  try {
    await build({
      root: fixtureRoot,
      configFile: false,
      logLevel: 'silent',
      base: './',
      server: { fs: { allow: [repoRoot] } },
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: resolve(fixtureRoot, 'sandbox.html') } },
    });

    browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files'] });
    const page = await browser.newPage({ viewport: { width: 400, height: 700 } });
    const requests: string[] = [];
    page.on('request', (r: { url(): string }) => requests.push(r.url()));
    await page.goto(pathToFileURL(join(buildDir, 'sandbox.html')).href);
    await page.waitForFunction(() => document.body.dataset.ready === 'true');

    // Both plugins activate.
    await page.waitForFunction(() => {
      const p = (window as any).__probe;
      return p.frameEvents.includes('petal.test-hello:activated') && p.frameEvents.includes('petal.reactions:activated');
    });
    const probe1 = await page.evaluate(() => (window as any).__probe);
    assert.deepEqual(probe1.errors, [], `plugin errors: ${probe1.errors.join('\n')}`);

    // Sandbox: the hello plugin reports which host globals it can see.
    const probeLine = probe1.logs.find((l: string) => l.includes('sandbox-probe'));
    assert.ok(probeLine, 'hello plugin logged its sandbox probe');
    const report = JSON.parse(probeLine.slice(probeLine.indexOf('{')));
    assert.deepEqual(report.leaks, [], 'no Tauri/host globals inside the frame');
    assert.equal(report.origin, 'null', 'frame runs on an opaque origin');
    const frameCsp = await page.evaluate(() => {
      const f = document.querySelector('iframe[data-plugin-frame="logic"]') as HTMLIFrameElement;
      return { sandbox: f.getAttribute('sandbox'), hasCsp: f.srcdoc.includes("connect-src 'none'") };
    });
    assert.deepEqual(frameCsp, { sandbox: 'allow-scripts', hasCsp: true });

    // Host-drawn toolbar buttons exist for both plugins; clicking hello's toasts.
    assert.deepEqual(
      probe1.buttons.map((b: any) => `${b.pluginId}/${b.buttonId}/${b.label}`).sort(),
      ['petal.reactions/react/React', 'petal.test-hello/hello/Hello'],
    );
    await page.click('#btn-petal\\.test-hello-hello');
    await page.waitForFunction(() => (window as any).__probe.toasts.length > 0);
    assert.deepEqual(await page.evaluate(() => (window as any).__probe.toasts), ['Hello from a plugin']);

    // Reactions: overlay frame mounted on load; clicking React opens a popover frame.
    assert.equal(await page.locator('iframe.petal-plugin-surface-overlay').count(), 1);
    await page.click('#btn-petal\\.reactions-react');
    const popover = page.frameLocator('iframe.petal-plugin-surface-popover');
    await popover.locator('button[aria-label="React with 👍"]').waitFor({ timeout: 10_000 });
    await popover.locator('button[aria-label="React with 👍"]').click();
    // The pick travels picker -> logic (channel) -> publish (adapter) and -> overlay (channel).
    await page.waitForFunction(() => (window as any).__probe.publishes.length > 0);
    const publishes = await page.evaluate(() => (window as any).__probe.publishes);
    assert.match(publishes[0], /^petal\.reactions:emoji:\{"e":"👍","t":\d+\}$/);
    const overlay = page.frameLocator('iframe.petal-plugin-surface-overlay');
    await overlay.locator('.r .e').first().waitFor({ timeout: 10_000 });
    assert.equal(await overlay.locator('.r .e').first().textContent(), '👍');
    assert.equal(await overlay.locator('.r .n').first().textContent(), 'Me', 'sender first name from the host, not the payload');

    // Escape closes the popover.
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => document.querySelector('iframe.petal-plugin-surface-popover') === null);

    // No plugin frame made a network request (file: page; only our own assets).
    const foreign = requests.filter((u) => !u.startsWith('file:') && !u.startsWith('about:') && !u.startsWith('data:'));
    assert.deepEqual(foreign, []);
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
