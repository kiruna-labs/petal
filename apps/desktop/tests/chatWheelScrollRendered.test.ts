// Real wheel input over a long chat history in the desktop meeting chrome.
// Synthetic WheelEvents never scroll, so this drives Playwright's mouse wheel.
import assert from 'node:assert/strict';
import { createServer, type Server } from 'node:http';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { chromium, webkit, type BrowserType, type Page } from 'playwright';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

const listState = (page: Page) =>
  page.evaluate(() => {
    const el = document.querySelector('[data-testid="chat-list"]') as HTMLElement;
    const chain: string[] = [];
    for (let n: HTMLElement | null = el; n; n = n.parentElement) {
      chain.push(`${n.tagName.toLowerCase()}.${String(n.className).split(' ')[0]} sh=${n.scrollHeight} ch=${n.clientHeight} st=${Math.round(n.scrollTop)} ov=${getComputedStyle(n).overflowY}`);
    }
    return { top: el.scrollTop, sh: el.scrollHeight, ch: el.clientHeight, chain };
  });

async function wheelOver(page: Page, trackpad = false): Promise<{ before: Awaited<ReturnType<typeof listState>>; after: Awaited<ReturnType<typeof listState>>; later: Awaited<ReturnType<typeof listState>> }> {
  const box = (await page.locator('[data-testid="chat-list"]').boundingBox())!;
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  const before = await listState(page);
  if (trackpad) {
    // A trackpad flick: many small deltas, one every frame.
    for (let i = 0; i < 40; i++) {
      await page.mouse.wheel(0, -8);
      await page.waitForTimeout(16);
    }
  } else {
    await page.mouse.wheel(0, -300);
  }
  await page.waitForTimeout(400);
  const after = await listState(page);
  await page.waitForTimeout(600);
  const later = await listState(page);
  return { before, after, later };
}

for (const [name, type] of [['chromium', chromium], ['webkit', webkit]] as Array<[string, BrowserType]>) {
  test(`chat history scrolls with the mouse wheel (${name})`, { timeout: 240_000 }, async () => {
    const buildDir = await mkdtemp(join(tmpdir(), 'petal-chat-wheel-'));
    const browser = await type.launch({ headless: true });
    let server: Server | undefined;
    try {
      await build({
        root: fileURLToPath(fixtureRoot),
        configFile: false,
        logLevel: 'silent',
        base: './',
        esbuild: { tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } }) },
        plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
        resolve: {
          alias: {
            $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
            '$app/environment': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
            '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot))),
          },
        },
        build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./meeting-chat.html', fixtureRoot)) } },
      });
      // Served over http: module scripts do not load from file:// in WebKit.
      const types: Record<string, string> = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' };
      server = createServer((req, res) => {
        const path = join(buildDir, decodeURIComponent((req.url ?? '/').split('?')[0]!).replace(/\.\.+/g, ''));
        readFile(path).then(
          (body) => { res.writeHead(200, { 'content-type': types[path.slice(path.lastIndexOf('.'))] ?? 'application/octet-stream' }); res.end(body); },
          () => { res.writeHead(404); res.end(); },
        );
      });
      await new Promise<void>((ok) => server!.listen(0, '127.0.0.1', ok));
      const origin = `http://127.0.0.1:${(server.address() as { port: number }).port}`;
      for (const [width, height] of [[900, 600], [1440, 900], [640, 600], [900, 360]] as Array<[number, number]>) {
        const page = await browser.newPage({ viewport: { width, height } });
        await page.addInitScript('window.__chatCount = 60;');
        await page.goto(`${origin}/meeting-chat.html`);
        await page.waitForFunction(() => document.body.dataset.fixtureReady === 'true' || Boolean(document.body.dataset.fixtureError));
        await page.waitForTimeout(300);
        const r = await wheelOver(page);
        assert.ok(r.before.sh > r.before.ch + 100, `${width}x${height}: the history overflows the list (sh ${r.before.sh}, ch ${r.before.ch})`);
        assert.ok(r.after.top < r.before.top - 50, `${name} ${width}x${height}: wheel up scrolled the list (${r.before.top} -> ${r.after.top})`);
        assert.ok(Math.abs(r.later.top - r.after.top) < 2, `${name} ${width}x${height}: the list stays where the reader scrolled (${r.after.top} -> ${r.later.top})`);
        await page.close();
      }
      // The history built up live: every line sent from the composer while pinned.
      {
        const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
        await page.goto(`${origin}/meeting-chat.html`);
        await page.waitForFunction(() => document.body.dataset.fixtureReady === 'true' || Boolean(document.body.dataset.fixtureError));
        for (let i = 0; i < 60; i++) {
          await page.fill('[data-testid="chat-input"]', `live line ${i} with enough words to wrap onto a second line in the column`);
          await page.press('[data-testid="chat-input"]', 'Enter');
        }
        await page.waitForTimeout(300);
        const r = await wheelOver(page, true);
        assert.ok(r.before.sh > r.before.ch + 100, `live: the history overflows the list`);
        assert.ok(r.after.top < r.before.top - 50, `${name} live: wheel up scrolled the list (${r.before.top} -> ${r.after.top})`);
        assert.ok(Math.abs(r.later.top - r.after.top) < 2, `${name} live: the list stays where the reader scrolled (${r.after.top} -> ${r.later.top})`);
        await page.close();
      }
    } finally {
      await browser.close();
      await new Promise<void>((ok) => (server ? server.close(() => ok()) : ok()));
      await rm(buildDir, { recursive: true, force: true });
    }
  });
}
