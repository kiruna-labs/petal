// #37: a plugin frame's own `<meta>` CSP cannot stop the frame navigating
// ITSELF, so the leak has to be closed from outside the frame. Both halves are
// exercised here in a real browser, over HTTP so the destination is genuinely
// another origin:
//
//   1. No embedder CSP -> the navigation SUCCEEDS and takes the roster with it
//      (the vulnerability, reproduced). The host must notice the second `load`,
//      unload the plugin, and post nothing more into that window.
//   2. Embedder `frame-src 'none'` (what tauri.conf.json and vercel.json now
//      ship) -> the navigation never happens at all, and plugin srcdoc frames
//      still boot under that policy.
//
// The negative assertion is synchronised against a POSITIVE control -- the
// well-behaved `hello` plugin logging the same broadcast -- so "no envelope
// arrived" cannot pass merely because nothing was sent yet.
import assert from 'node:assert/strict';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { extname, join, normalize, resolve } from 'node:path';
import test from 'node:test';
import { build } from 'vite';

const repoRoot = resolve(import.meta.dirname, '../..');
const fixtureRoot = resolve(repoRoot, 'web-harness/tests/fixtures/plugins');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
};

/** Serves the built fixture plus `/leak.html` (the attacker page) from one port. */
async function startServer(buildDir: string): Promise<{ port: number; close: () => Promise<void> }> {
  const leakPage = await readFile(join(fixtureRoot, 'leak.html'));
  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    const path = new URL(req.url ?? '/', 'http://localhost').pathname;
    if (path === '/leak.html') {
      res.writeHead(200, { 'content-type': MIME['.html']! });
      res.end(leakPage);
      return;
    }
    const file = join(buildDir, normalize(path).replace(/^(\.\.[/\\])+/, ''));
    readFile(file).then(
      (body) => {
        res.writeHead(200, { 'content-type': MIME[extname(file)] ?? 'application/octet-stream' });
        res.end(body);
      },
      () => {
        res.writeHead(404);
        res.end('not found');
      },
    );
  });
  await new Promise<void>((done) => server.listen(0, '127.0.0.1', done));
  const address = server.address();
  if (address === null || typeof address === 'string') throw new Error('server did not bind a port');
  return {
    port: address.port,
    close: () => new Promise<void>((done) => server.close(() => done())),
  };
}

interface Peer {
  identity: string;
  name: string;
  isLocal: boolean;
  speaking: boolean;
  micMuted: boolean;
}
const ALICE: Peer = { identity: 'alice', name: 'Alice Anderson', isLocal: false, speaking: false, micMuted: false };
const BOB: Peer = { identity: 'bob', name: 'Bob Secret', isLocal: false, speaking: false, micMuted: false };

test('a plugin frame that navigates itself is cut off, and an embedder frame-src blocks it outright', { timeout: 120_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-plugin-selfnav-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  let server: Awaited<ReturnType<typeof startServer>> | undefined;
  try {
    await build({
      root: fixtureRoot,
      configFile: false,
      logLevel: 'silent',
      base: './',
      server: { fs: { allow: [repoRoot] } },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: [resolve(fixtureRoot, 'selfnav.html'), resolve(fixtureRoot, 'selfnav-csp.html')] },
      },
    });
    server = await startServer(buildDir);
    // localhost and 127.0.0.1 are different origins, so the plugin frame really
    // leaves the host page's origin even though one server answers both.
    const leakUrl = `http://127.0.0.1:${server.port}/leak.html`;
    const pageUrl = (name: string) => `http://localhost:${server!.port}/${name}?leak=${encodeURIComponent(leakUrl)}`;

    browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-gpu'] });

    // ---- 1. No embedder CSP: the frame gets out, and must then be cut off.
    {
      const page = await browser.newPage({ viewport: { width: 400, height: 700 } });
      const requests: string[] = [];
      page.on('request', (r: { url(): string }) => requests.push(r.url()));
      await page.goto(pageUrl('selfnav.html'));
      await page.waitForFunction(() => document.body.dataset.ready === 'true');
      await page.waitForFunction(() => {
        const p = (window as any).__probe;
        return p.frameEvents.includes('petal.test-hello:activated') && p.frameEvents.includes('petal.test-escape:activated');
      });
      assert.deepEqual(await page.evaluate(() => (window as any).__probe.errors), []);

      // One meeting event is enough for the plugin to learn a name and leave.
      await page.evaluate((alice: Peer) => (window as any).__host.broadcast('meeting.participant-joined', alice), ALICE);
      await page.waitForFunction(() => (window as any).__probe.leaks.some((l: string) => l.startsWith('landed:')));

      const landed = (await page.evaluate(() => (window as any).__probe.leaks)) as string[];
      const roster = decodeURIComponent(landed.find((l) => l.startsWith('landed:'))!);
      assert.match(roster, /alice=Alice Anderson/, `the roster left the sandbox in the URL: ${roster}`);
      assert.ok(
        requests.some((u) => u.startsWith(leakUrl) && decodeURIComponent(u).includes('Alice Anderson')),
        `the frame really requested another origin: ${requests.join('\n')}`,
      );

      // The host must notice the second `load` and unload the plugin.
      await page.waitForFunction(() => (window as any).__host.isLoaded('petal.test-escape') === false);
      const warns = (await page.evaluate(() => (window as any).__probe.logs)) as string[];
      assert.ok(
        warns.some((l) => l.startsWith('warn:') && l.includes('petal.test-escape') && l.includes('navigated away')),
        `the host said why it unloaded the plugin: ${warns.join('\n')}`,
      );

      // Now the real question: does anything else reach that window? Send a new
      // participant and wait for the UNCOMPROMISED plugin to report it, so the
      // broadcast is known to have happened before we assert the negative.
      await page.evaluate((bob: Peer) => (window as any).__host.broadcast('meeting.participant-joined', bob), BOB);
      await page.waitForFunction(() =>
        (window as any).__probe.logs.some((l: string) => l.includes('petal.test-hello:info:participant-joined Bob Secret')),
      );
      const leaks = (await page.evaluate(() => (window as any).__probe.leaks)) as string[];
      assert.deepEqual(
        leaks.filter((l) => l.startsWith('received:')),
        [],
        `no envelope may be delivered to a navigated frame: ${leaks.join('\n')}`,
      );
      assert.ok(!leaks.some((l) => l.includes('Bob Secret')), `nothing about Bob reached the attacker page: ${leaks.join('\n')}`);
      await page.close();
    }

    // ---- 2. Embedder `frame-src 'none'`: plugins still boot, nothing escapes.
    {
      const page = await browser.newPage({ viewport: { width: 400, height: 700 } });
      const requests: string[] = [];
      const console_: string[] = [];
      page.on('request', (r: { url(): string }) => requests.push(r.url()));
      page.on('console', (m: { text(): string }) => console_.push(m.text()));
      await page.goto(pageUrl('selfnav-csp.html'));
      await page.waitForFunction(() => document.body.dataset.ready === 'true');
      // srcdoc frames are NOT blocked by frame-src (they are not fetched); if a
      // browser ever changed that, this is where the policy would show up as
      // "plugins do not run at all".
      await page.waitForFunction(() => {
        const p = (window as any).__probe;
        return p.frameEvents.includes('petal.test-hello:activated') && p.frameEvents.includes('petal.test-escape:activated');
      });

      await page.evaluate((alice: Peer) => (window as any).__host.broadcast('meeting.participant-joined', alice), ALICE);
      await page.waitForFunction(() =>
        (window as any).__probe.logs.some((l: string) => l.includes('petal.test-escape:info:escaping with')),
      );
      await page.evaluate((bob: Peer) => (window as any).__host.broadcast('meeting.participant-joined', bob), BOB);
      await page.waitForFunction(() =>
        (window as any).__probe.logs.some((l: string) => l.includes('petal.test-hello:info:participant-joined Bob Secret')),
      );

      assert.deepEqual(await page.evaluate(() => (window as any).__probe.leaks), [], 'nothing was navigated to');
      assert.deepEqual(
        requests.filter((u) => u.startsWith(leakUrl)),
        [],
        `no request left for the attacker origin: ${requests.join('\n')}`,
      );
      // Say WHY it did not leave, so a future change that merely breaks the
      // fixture's navigation cannot pass as "the policy worked".
      assert.ok(
        console_.some((t) => t.includes('violates the following Content Security Policy directive: "frame-src \'none\'"')),
        `the embedder policy is what refused it: ${console_.join('\n')}`,
      );
      // Chromium commits an error page in the frame after refusing, so the
      // gate in host.ts fires too and the plugin ends up unloaded. Both
      // defences engaging is the intended outcome; the difference that matters
      // is that nothing reached the network.
      await page.waitForFunction(() => (window as any).__host.isLoaded('petal.test-escape') === false);
      await page.close();
    }
  } finally {
    await browser?.close();
    await server?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});

test('meet.petal.live sends the same frame-src the rendered test proves out', async () => {
  const config = JSON.parse(await readFile(resolve(repoRoot, 'web-harness/vercel.json'), 'utf8'));
  const values = (config.headers ?? []).flatMap((rule: { source: string; headers: { key: string; value: string }[] }) =>
    rule.source === '/(.*)' ? rule.headers.filter((h) => h.key.toLowerCase() === 'content-security-policy').map((h) => h.value) : [],
  );
  assert.equal(values.length, 1, `exactly one site-wide CSP header: ${JSON.stringify(config.headers)}`);
  // Only frame-src: a srcdoc plugin frame inherits its embedder's policy, so
  // anything else here would also apply inside every plugin (#37).
  assert.deepEqual(
    values[0]!.split(';').map((d: string) => d.trim()).filter(Boolean),
    ["frame-src 'none'"],
  );
});
