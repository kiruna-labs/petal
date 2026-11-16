// #487 + CONTRIBUTING.md "Native window lifecycle changes need a test that
// drives the real event path": the go-live dialog's page half. Mounts the
// real page (`src/routes/go-live/+page.svelte`) with Tauri's IPC and events
// mocked and drives its own handlers: a refused go-live keeps the dialog
// open, Cancel closes the window, the `go-live-target` event a reused
// (macOS) dialog gets on reopen leaves nothing of the last open behind, and
// a go-live that succeeds retires the dialog. The native half (the command
// and its close-request handler) is
// `streaming::commands::tests::the_go_live_dialog_opens_closes_and_reopens_through_its_handlers`.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';
import { chromium } from 'playwright';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);
const CLOSE = 'plugin:window|close';

type Call = { command: string; windowId: number | null };
type Steps = {
  opened: { calls: Call[] };
  failedGoLive: { calls: string[]; error: string | null; key: string | null };
  cancel: { calls: string[]; key: string | null };
  reopened: { calls: Call[]; error: string | null; key: string | null };
  goLive: { calls: Call[]; key: string | null; error: string | null };
};

test('the go-live dialog closes, reopens clean and retires after going live (#487)', { timeout: 120_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-go-live-lifecycle-'));
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
      resolve: {
        alias: {
          $lib: fileURLToPath(new URL('./src/lib', desktopRoot)),
          '$app/env': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
          '@petal/shared': fileURLToPath(new URL('../../shared', desktopRoot))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fileURLToPath(new URL('./go-live-lifecycle.html', fixtureRoot)) }
      }
    });
    browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files'] });
    const page = await browser.newPage({ viewport: { width: 460, height: 660 } });
    await page.goto(pathToFileURL(join(buildDir, 'go-live-lifecycle.html')).href);
    await page.waitForFunction(() => !!document.body.dataset.lifecycle || !!document.body.dataset.lifecycleError, undefined, {
      timeout: 30_000
    });
    const state = (await page.evaluate(`(() => ({
      steps: document.body.dataset.lifecycle ?? null,
      error: document.body.dataset.lifecycleError ?? null
    }))()`)) as { steps: string | null; error: string | null };
    if (state.error) throw new Error(`go-live lifecycle fixture failed: ${decodeURIComponent(state.error)}`);
    const steps = JSON.parse(decodeURIComponent(state.steps!)) as Steps;

    // Opened: the page asks which share it is for and previews that share.
    const openedCommands = steps.opened.calls.map((c) => c.command);
    assert.ok(openedCommands.includes('go_live_target'), `opened: ${openedCommands}`);
    assert.ok(steps.opened.calls.some((c) => c.command === 'stream_preview' && c.windowId === 7), 'previews share 7');

    // Refused: the error shows and the dialog stays open with the key.
    assert.ok(steps.failedGoLive.calls.includes('stream_go_live'), `refused: ${steps.failedGoLive.calls}`);
    assert.ok(!steps.failedGoLive.calls.includes(CLOSE), 'a refused go-live keeps the dialog open');
    assert.match(steps.failedGoLive.error ?? '', /refused the stream key/);
    assert.equal(steps.failedGoLive.key, 'secret-key');

    // Cancel: the page closes its own window and drops the key first.
    assert.deepEqual(steps.cancel.calls, [CLOSE], 'Cancel closes the window, nothing else');
    assert.equal(steps.cancel.key, '', 'the key is dropped before the window goes');

    // Reopened for share 9: the old error is gone, no key, and the page
    // reloads destinations and previews the new share.
    assert.equal(steps.reopened.error, null, 'the last open\'s error does not linger');
    assert.equal(steps.reopened.key, '', 'no key from the last open');
    const reopened = steps.reopened.calls.map((c) => c.command);
    assert.ok(reopened.includes('stream_destinations_list'), `reopened: ${reopened}`);
    assert.ok(steps.reopened.calls.some((c) => c.command === 'stream_preview' && c.windowId === 9), 'previews share 9');
    assert.ok(!steps.reopened.calls.some((c) => c.windowId === 7), 'nothing for the old share');

    // Go live: one request, for share 9, then the dialog retires itself.
    const goLive = steps.goLive.calls.map((c) => c.command);
    const request = steps.goLive.calls.find((c) => c.command === 'stream_go_live');
    assert.equal(request?.windowId, 9, 'goes live on the share it was reopened for');
    assert.equal(goLive.filter((c) => c === 'stream_go_live').length, 1);
    assert.ok(goLive.indexOf(CLOSE) > goLive.indexOf('stream_go_live'), `retires after stream_go_live answers: ${goLive}`);
    assert.equal(steps.goLive.key, '', 'the key is dropped before the window goes');
    assert.equal(steps.goLive.error, null);
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
