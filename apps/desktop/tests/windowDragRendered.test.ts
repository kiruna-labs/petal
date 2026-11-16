// PETAL-WORKAROUND(tao-drag-window): real mouse input through the real listener
// order: Tauri's own drag.js (tests/fixtures/tauri-drag.js, pinned to the
// resolved crate by window_drag.rs) as an init script, then the app bundle
// running the real installWindowDragWorkaround() on a macOS user agent.
// Every IPC call lands on one stub, so the test sees which drag each press starts.
// Chromium only: DOM dispatch order is the same in WebKit, and headless WebKit
// wedges when the screen is locked (see chatWheelScrollRendered.test.ts).
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { build } from 'esbuild';
import { chromium, type Page } from 'playwright';

const MAC_UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)';
const ORIGIN = 'http://petal-drag.test';

const tauriDragJs = readFileSync(new URL('./fixtures/tauri-drag.js', import.meta.url), 'utf8').replace(
  '__TEMPLATE_os_name__',
  "'macos'"
);

const PAGE = `<!doctype html><html><body style="margin:0">
<div id="bar" data-tauri-drag-region style="display:flex;gap:20px;height:40px;width:600px;padding:0 0 0 60px">
  <span id="label" data-tauri-drag-region>Title</span>
  <span id="plain-child">child</span>
  <button id="button">Button</button>
  <span id="stopper" data-tauri-drag-region>Dot</span>
</div>
<div id="deep" data-tauri-drag-region="deep" style="height:40px;width:600px;display:flex;gap:20px">
  <span id="deep-child">deep child</span>
  <span data-tauri-drag-region="false"><span id="no-drag">no drag</span></span>
</div>
<div id="content" style="height:40px;width:600px">content</div>
<script>
  window.__appDocMouseDowns = 0;
  document.addEventListener('mousedown', () => window.__appDocMouseDowns++);
  document.getElementById('stopper').addEventListener('mousedown', (e) => e.stopPropagation());
</script>
<script src="/bundle.js"></script>
</body></html>`;

async function bundle(install: boolean): Promise<string> {
  const entry = install
    ? `import { installWindowDragWorkaround } from './src/lib/windowDrag.ts';
       window.__uninstall = installWindowDragWorkaround();`
    : '';
  const out = await build({
    stdin: { contents: entry, resolveDir: new URL('..', import.meta.url).pathname, loader: 'ts' },
    bundle: true,
    format: 'iife',
    write: false,
    logLevel: 'silent',
    tsconfigRaw: '{}',
    alias: { '@petal/shared': new URL('../../../shared', import.meta.url).pathname }
  });
  return out.outputFiles[0]!.text;
}

async function openPage(install: boolean): Promise<{ page: Page; close: () => Promise<void> }> {
  const js = await bundle(install);
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({ userAgent: MAC_UA, viewport: { width: 800, height: 300 } });
    // A string, not a function: tsx would wrap a function's inner arrows in its
    // `__name` helper, which doesn't exist in the page.
    await page.addInitScript(`
      window.__ipc = [];
      window.__TAURI_INTERNALS__ = {
        invoke(cmd) { window.__ipc.push(cmd); return Promise.resolve(null); },
        transformCallback() { return 0; }
      };`);
    await page.addInitScript(tauriDragJs);
    await page.route(`${ORIGIN}/**`, (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === '/bundle.js') return route.fulfill({ contentType: 'text/javascript', body: js });
      return route.fulfill({ contentType: 'text/html', body: PAGE });
    });
    await page.goto(`${ORIGIN}/`);
    return { page, close: () => browser.close() };
  } catch (err) {
    await browser.close();
    throw err;
  }
}

async function pressAndTakeIpc(page: Page, selector: string, how: 'click' | 'dblclick' | 'right'): Promise<string[]> {
  await page.evaluate(() => {
    (window as unknown as { __ipc: string[] }).__ipc.length = 0;
  });
  const box = (await page.locator(selector).boundingBox())!;
  // Bare regions drag only on a direct press, so aim at the element's own
  // padding/edge rather than a child.
  const x = box.x + Math.min(4, box.width / 2);
  const y = box.y + box.height / 2;
  if (how === 'dblclick') await page.mouse.dblclick(x, y);
  else await page.mouse.click(x, y, { button: how === 'right' ? 'right' : 'left' });
  await page.waitForTimeout(50);
  return page.evaluate(() => [...(window as unknown as { __ipc: string[] }).__ipc]);
}

test('without the workaround Tauri drag.js starts its own drag (the fixture is live)', { timeout: 60_000 }, async () => {
  const { page, close } = await openPage(false);
  try {
    assert.deepEqual(await pressAndTakeIpc(page, '#bar', 'click'), ['plugin:window|start_dragging']);
  } finally {
    await close();
  }
});

test('on macOS real presses reach start_window_drag and never Tauri start_dragging', { timeout: 60_000 }, async () => {
  const { page, close } = await openPage(true);
  try {
    const DRAG = ['start_window_drag'];
    assert.deepEqual(await pressAndTakeIpc(page, '#bar', 'click'), DRAG, 'bare region, direct press');
    assert.deepEqual(await pressAndTakeIpc(page, '#label', 'click'), DRAG, 'bare region child that is itself a region');
    assert.deepEqual(await pressAndTakeIpc(page, '#plain-child', 'click'), [], 'plain child of a bare region');
    assert.deepEqual(await pressAndTakeIpc(page, '#button', 'click'), [], 'button inside a region');
    assert.deepEqual(await pressAndTakeIpc(page, '#stopper', 'click'), [], 'element handler that stops mousedown');
    assert.deepEqual(await pressAndTakeIpc(page, '#deep-child', 'click'), DRAG, 'child of a deep region');
    assert.deepEqual(await pressAndTakeIpc(page, '#no-drag', 'click'), [], 'inside data-tauri-drag-region="false"');
    assert.deepEqual(await pressAndTakeIpc(page, '#content', 'click'), [], 'no region');
    assert.deepEqual(await pressAndTakeIpc(page, '#bar', 'right'), [], 'right button');
    // Double click: the first press is ours, the second reaches Tauri, which
    // toggles maximize on mouseup.
    assert.deepEqual(
      await pressAndTakeIpc(page, '#bar', 'dblclick'),
      ['start_window_drag', 'plugin:window|internal_toggle_maximize'],
      'double click still zooms'
    );
  } finally {
    await close();
  }
});

test('app document mousedown listeners see the same presses as under Tauri drag.js', { timeout: 60_000 }, async () => {
  // region-window/+page.svelte's document `beginDrag` relies on this: Tauri's
  // stopImmediatePropagation already hid drag-region presses from it.
  for (const install of [false, true]) {
    const { page, close } = await openPage(install);
    try {
      const count = () => page.evaluate(() => (window as unknown as { __appDocMouseDowns: number }).__appDocMouseDowns);
      await pressAndTakeIpc(page, '#bar', 'click');
      assert.equal(await count(), 0, `drag-region press hidden (workaround ${install})`);
      await pressAndTakeIpc(page, '#content', 'click');
      assert.equal(await count(), 1, `plain press seen (workaround ${install})`);
    } finally {
      await close();
    }
  }
});
