// Regression: the WEB client's meeting chat list must scroll with real wheel
// input. Drives Playwright's mouse wheel (synthetic WheelEvents never scroll)
// over a 60-message history in the real web-harness meeting DOM and CSS, in
// chromium and webkit. WebKit routes wheel input by paint order and ignores the
// pointer-events:none of the full-viewport plugin overlay frame, so a drawer
// painted below that overlay could not be wheel-scrolled (fixed by painting the
// drawer above it, see ChatDrawer.svelte).
import assert from 'node:assert/strict';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer, type Server } from 'node:http';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { build } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

const repoRoot = resolve(import.meta.dirname, '../..');
const webRoot = resolve(repoRoot, 'web-harness');
const require = createRequire(import.meta.url);
const playwright = require(resolve(repoRoot, 'apps/desktop/node_modules/playwright')) as {
  chromium: BrowserType;
  webkit: BrowserType;
};
const RESULT_PATH = process.env.CHAT_WHEEL_PROBE_OUT ?? join(tmpdir(), 'chat-wheel-web-probe.json');

type BrowserType = { launch(options: object): Promise<Browser> };
type Browser = { newPage(options: object): Promise<Page>; close(): Promise<void> };
type Page = {
  addInitScript(script: string): Promise<void>;
  evaluate<R = unknown>(script: string): Promise<R>;
  goto(url: string, options?: object): Promise<unknown>;
  waitForFunction(script: string, arg?: unknown, options?: object): Promise<unknown>;
  waitForTimeout(ms: number): Promise<void>;
  setViewportSize(size: { width: number; height: number }): Promise<void>;
  mouse: { move(x: number, y: number): Promise<void>; wheel(dx: number, dy: number): Promise<void> };
  locator(selector: string): { click(): Promise<void>; boundingBox(): Promise<{ x: number; y: number; width: number; height: number } | null> };
  close(): Promise<void>;
};

// Runs in the page before any app script. Strings only: a tsx/esbuild
// keepNames wrapper (__name) would be missing in the page.
const INIT = `(() => {
  const desc = (t) => {
    if (t === window) return 'window';
    if (!(t instanceof Element)) return String(t && t.nodeName);
    const cls = typeof t.className === 'string' ? t.className.trim().split(/\\s+/).filter(Boolean).slice(0, 2).join('.') : '';
    return t.nodeName.toLowerCase() + (t.id ? '#' + t.id : '') + (cls ? '.' + cls : '');
  };
  window.__wheelReg = [];
  const origAdd = EventTarget.prototype.addEventListener;
  EventTarget.prototype.addEventListener = function (type, listener, options) {
    if (type === 'wheel') {
      const stack = (new Error().stack || '').split('\\n').slice(2, 5).map((s) => s.trim()).join(' | ');
      window.__wheelReg.push({
        target: desc(this),
        passive: typeof options === 'object' && options !== null ? options.passive : 'unset',
        capture: typeof options === 'object' && options !== null ? !!options.capture : !!options,
        stack,
      });
    }
    return origAdd.call(this, type, listener, options);
  };
  window.__probe = {
    desc,
    chain(el) {
      const out = [];
      for (let n = el; n; n = n.parentElement) {
        const cs = getComputedStyle(n);
        out.push({ d: desc(n), st: Math.round(n.scrollTop), sh: n.scrollHeight, ch: n.clientHeight, oy: cs.overflowY });
      }
      return out;
    },
    armWheel() {
      window.__wheelLog = [];
      window.addEventListener('wheel', (e) => {
        const rec = { target: desc(e.target), dpAtDispatch: e.defaultPrevented, dp: null };
        window.__wheelLog.push(rec);
        setTimeout(() => { rec.dp = e.defaultPrevented; }, 0);
      }, { capture: true });
    },
  };
})();`;

const MEASURE_LIST = `(() => {
  const list = document.querySelector('[data-testid="chat-list"]');
  if (!list) return null;
  const r = list.getBoundingClientRect();
  return { top: list.scrollTop, sh: list.scrollHeight, ch: list.clientHeight, rect: [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height)], chain: window.__probe.chain(list), winY: window.scrollY, html: getComputedStyle(document.documentElement).overflow, body: getComputedStyle(document.body).overflow };
})()`;

const SEED_MESSAGES = `(async () => {
  const chat = window.__petalHarness.chat;
  const enc = new TextEncoder();
  for (let i = 0; i < 60; i++) {
    const identity = 'peer-' + (i % 3);
    const text = i % 5 === 4 ? 'Message ' + i + ': a longer one that wraps onto a second line in the chat column' : 'Message ' + i;
    const wire = { v: 1, type: 'msg', id: 'm-wheel-' + String(i).padStart(4, '0'), text, t: Date.now() - (60 - i) * 1000 };
    chat.onData(enc.encode(JSON.stringify(wire)), { identity, name: ['Mira', 'Theo', 'Ada'][i % 3] }, identity);
  }
  // Svelte flushes on a microtask/frame: wait for the drawer to render the rows.
  await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
  return document.querySelectorAll('[data-testid="chat-msg"]').length;
})()`;

async function openMeeting(page: Page): Promise<void> {
  await page.waitForFunction('!!(window.__petalHarness && window.__petalHarness.chat)');
  await page.evaluate(`(() => {
    document.querySelector('#meeting-screen')?.classList.remove('hidden');
    document.querySelector('#join-screen')?.classList.add('hidden');
  })()`);
}

async function openChat(page: Page): Promise<void> {
  const collapsed = await page.evaluate<boolean>(
    "document.querySelector('#ctl-chat').getClientRects().length === 0",
  );
  if (!collapsed) {
    await page.locator('#ctl-chat').click();
    return;
  }
  await page.locator('#ctl-more').click();
  await page.evaluate(`(() => {
    const row = [...document.querySelectorAll('#overflow-menu .overflow-menu-row')].find((el) => el.textContent && el.textContent.includes('Chat'));
    row?.click();
  })()`);
}

type ListSnap = {
  top: number;
  sh: number;
  ch: number;
  rect: number[];
  chain: Array<{ d: string; st: number; sh: number; ch: number; oy: string }>;
  winY: number;
  html: string;
  body: string;
};

async function measureList(page: Page): Promise<ListSnap> {
  const snap = await page.evaluate<ListSnap | null>(MEASURE_LIST);
  assert.ok(snap, 'the chat list is in the DOM');
  return snap;
}

type Row = {
  engine: string;
  viewport: string;
  listRect: number[];
  overflowY: string;
  sh: number;
  ch: number;
  topBefore: number;
  topAfter: number;
  delta: number;
  scrolled: boolean;
  ancestorsChanged: string[];
  winYBefore: number;
  winYAfter: number;
  htmlOverflow: string;
  bodyOverflow: string;
  pointerUnder: string;
  pointerInsideList: boolean;
  wheelTargets: string[];
  defaultPreventedAtDispatch: boolean[];
  defaultPreventedAfterDispatch: Array<boolean | null>;
};

const VIEWPORTS: Array<[string, number, number]> = [
  ['1280x800', 1280, 800],
  ['900x600', 900, 600],
  ['390x844', 390, 844],
];

const ENGINES: Array<[string, BrowserType]> = [
  ['chromium', playwright.chromium],
  ['webkit', playwright.webkit],
];

test('web meeting chat list scrolls with real mouse wheel input', { timeout: 600_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-chat-wheel-web-'));
  let server: Server | undefined;
  const rows: Row[] = [];
  const controls: Array<{ engine: string; scrollTop: number; wheelTargets: string[] }> = [];
  const registrations: Record<string, unknown> = {};
  try {
    await build({
      root: webRoot,
      configFile: false,
      logLevel: 'silent',
      base: './',
      plugins: [svelte()],
      define: {
        __PETAL_BUILD_INFO__: JSON.stringify({ version: 'test', commit: 'test', buildDate: '2099-01-01' }),
        'import.meta.env.VITE_SENTRY_DSN': JSON.stringify(''),
      },
      resolve: { alias: { '@petal/shared': resolve(repoRoot, 'shared') } },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        minify: false,
        rollupOptions: { input: resolve(webRoot, 'index.html') },
      },
    });

    // Served over http: module scripts do not load from file:// in WebKit.
    const types: Record<string, string> = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.woff2': 'font/woff2' };
    server = createServer((req, res) => {
      const path = join(buildDir, decodeURIComponent((req.url ?? '/').split('?')[0]!).replace(/\.\.+/g, ''));
      readFile(path).then(
        (body) => {
          res.writeHead(200, { 'content-type': types[path.slice(path.lastIndexOf('.'))] ?? 'application/octet-stream' });
          res.end(body);
        },
        () => {
          res.writeHead(404);
          res.end();
        },
      );
    });
    await new Promise<void>((ok) => server!.listen(0, '127.0.0.1', ok));
    const origin = `http://127.0.0.1:${(server.address() as { port: number }).port}`;

    for (const [engine, type] of ENGINES) {
      const browser = await type.launch({ headless: true });
      try {
        for (const [label, width, height] of VIEWPORTS) {
          const page = await browser.newPage({ viewport: { width, height } });
          try {
            await page.addInitScript(INIT);
            await page.goto(`${origin}/index.html`, { waitUntil: 'load' });
            await openMeeting(page);
            await page.waitForTimeout(200);
            await openChat(page);
            await page.waitForTimeout(200);
            const count = await page.evaluate<number>(SEED_MESSAGES);
            assert.equal(count, 60, `${engine} ${label}: 60 messages rendered`);
            await page.waitForTimeout(400);

            const box = await page.locator('[data-testid="chat-list"]').boundingBox();
            assert.ok(box, `${engine} ${label}: the chat list has a box (chat is visible)`);
            const cx = box.x + box.width / 2;
            const cy = box.y + box.height / 2;
            await page.mouse.move(cx, cy);
            await page.waitForTimeout(100);
            const pointerUnder = await page.evaluate<string>(
              `(() => { const el = document.elementFromPoint(${cx}, ${cy}); return el ? window.__probe.desc(el) + (el.closest('[data-testid="chat-list"]') ? ' [inside chat-list]' : ' [NOT inside chat-list]') : 'null'; })()`,
            );

            await page.evaluate('window.__probe.armWheel()');
            const before = await measureList(page);
            assert.ok(before.sh > before.ch + 100, `${engine} ${label}: the history overflows the list (sh ${before.sh}, ch ${before.ch})`);

            await page.mouse.wheel(0, -300);
            await page.waitForTimeout(600);
            const after = await measureList(page);
            const log = await page.evaluate<Array<{ target: string; dpAtDispatch: boolean; dp: boolean | null }>>('window.__wheelLog');

            const ancestorsChanged: string[] = [];
            for (let i = 1; i < before.chain.length; i++) {
              const b = before.chain[i]!;
              const a = after.chain[i];
              if (a && a.st !== b.st) ancestorsChanged.push(`${b.d} ${b.st}->${a.st}`);
            }
            if (after.winY !== before.winY) ancestorsChanged.push(`window.scrollY ${before.winY}->${after.winY}`);

            const registrationsOnPage = await page.evaluate<unknown[]>('window.__wheelReg');
            registrations[`${engine}-${label}`] = registrationsOnPage;

            rows.push({
              engine,
              viewport: label,
              listRect: before.rect,
              overflowY: before.chain[0]!.oy,
              sh: before.sh,
              ch: before.ch,
              topBefore: before.top,
              topAfter: after.top,
              delta: after.top - before.top,
              scrolled: after.top < before.top - 50,
              ancestorsChanged,
              winYBefore: before.winY,
              winYAfter: after.winY,
              htmlOverflow: before.html,
              bodyOverflow: before.body,
              pointerUnder,
              pointerInsideList: pointerUnder.includes('[inside chat-list]'),
              wheelTargets: log.map((r) => r.target),
              defaultPreventedAtDispatch: log.map((r) => r.dpAtDispatch),
              defaultPreventedAfterDispatch: log.map((r) => r.dp),
            });
          } finally {
            await page.close();
          }
        }
        // Control: a plain scroll box on a bare page, same engine and wheel call.
        // If this does not scroll either, the engine's wheel path is the cause.
        const control = await browser.newPage({ viewport: { width: 900, height: 600 } });
        try {
          await control.setContent('<div id="c" style="height:300px;overflow:auto"><div style="height:3000px">x</div></div>');
          await control.evaluate(INIT);
          await control.evaluate('window.__probe.armWheel()');
          await control.mouse.move(100, 100);
          await control.waitForTimeout(100);
          await control.mouse.wheel(0, 300);
          await control.waitForTimeout(600);
          const c = await control.evaluate<{ top: number; wheel: string[] }>(
            "({ top: document.getElementById('c').scrollTop, wheel: window.__wheelLog.map((r) => r.target) })",
          );
          controls.push({ engine, scrollTop: c.top, wheelTargets: c.wheel });
        } finally {
          await control.close();
        }
      } finally {
        await browser.close();
      }
    }
  } finally {
    await mkdir(join(RESULT_PATH, '..'), { recursive: true });
    await writeFile(RESULT_PATH, JSON.stringify({ rows, controls, registrations }, null, 2));
    if (server) await new Promise<void>((ok) => server!.close(() => ok()));
    await rm(buildDir, { recursive: true, force: true });
  }

  console.log('\nengine   viewport   overflowY  sh    ch   top before -> after  delta  scrolled  wheel-target(s)  preventedAtDispatch / afterTick  pointer');
  for (const r of rows) {
    console.log(
      `${r.engine.padEnd(8)} ${r.viewport.padEnd(10)} ${r.overflowY.padEnd(10)} ${String(r.sh).padEnd(5)} ${String(r.ch).padEnd(4)} ${r.topBefore} -> ${r.topAfter}  ${r.delta}  ${r.scrolled}  ${r.wheelTargets.join(',')}  ${r.defaultPreventedAtDispatch.join(',')} / ${r.defaultPreventedAfterDispatch.join(',')}  ${r.pointerUnder}`,
    );
    if (r.ancestorsChanged.length) console.log(`   ancestors changed: ${r.ancestorsChanged.join('; ')}`);
  }
  for (const c of controls) console.log(`control ${c.engine}: plain scroll box scrollTop=${c.scrollTop} wheelSeenBy=${c.wheelTargets.join(',') || 'none'}`);
  console.log(`results written to ${RESULT_PATH}`);
  assert.equal(rows.length, ENGINES.length * VIEWPORTS.length, 'every engine and viewport was measured');
  for (const r of rows) {
    assert.ok(r.wheelTargets.length >= 1, `${r.engine} ${r.viewport}: the wheel event reached the page`);
    assert.ok(r.pointerInsideList, `${r.engine} ${r.viewport}: the chat list is under the pointer (${r.pointerUnder})`);
    assert.ok(r.scrolled, `${r.engine} ${r.viewport}: wheel up scrolled the chat list (${r.topBefore} -> ${r.topAfter})`);
  }
  for (const c of controls) assert.ok(c.scrollTop > 0, `${c.engine}: the bare-page control scroll box scrolls (engine wheel path works)`);
});
