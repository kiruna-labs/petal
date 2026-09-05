// #674: the yellow "hide" traffic-light dot (and friends) in a remote
// window's header were entirely covered by the NW/NE resize-grip hit zones
// (`.resize-zones`, z-index:3, apps/desktop/src/routes/compositor/surface/
// +page.svelte) because RemoteWindowHeader.svelte's `.traffic-lights` /
// `.right-cluster` had no z-index of their own. This test renders the real
// RemoteWindowHeader component next to a faithful reproduction of the
// resize-zones overlay (same DOM sibling relationship, same CSS) in real
// headless-Chromium layout and asserts `document.elementFromPoint` resolves
// to the header control, not the resize grip -- an event-level/unit test on
// the pure z-index values cannot distinguish "wins hit-testing" from
// "silently covered", per CLAUDE.md's native-window-lifecycle testing rule.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { accessSync, constants, readdirSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { homedir, tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

function executable(path: string): boolean {
  try {
    accessSync(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function cachedChromiumCandidates(): string[] {
  const cacheRoots = [
    join(homedir(), 'Library', 'Caches', 'ms-playwright'),
    join(homedir(), '.cache', 'ms-playwright'),
    join(homedir(), 'AppData', 'Local', 'ms-playwright')
  ];
  const platformDirs =
    process.platform === 'darwin'
      ? [process.arch === 'arm64' ? 'chrome-headless-shell-mac-arm64' : 'chrome-headless-shell-mac-x64']
      : process.platform === 'linux' && process.arch === 'x64'
        ? ['chrome-headless-shell-linux64']
        : process.platform === 'win32' && process.arch === 'x64'
          ? ['chrome-headless-shell-win64']
          : [];
  const executableName = process.platform === 'win32' ? 'chrome-headless-shell.exe' : 'chrome-headless-shell';
  const candidates: string[] = [];
  for (const root of cacheRoots) {
    let entries: string[] = [];
    try {
      entries = readdirSync(root).filter((entry) => entry.startsWith('chromium_headless_shell-'));
    } catch {
      continue;
    }
    for (const entry of entries.sort().reverse()) {
      for (const platformDir of platformDirs) {
        candidates.push(join(root, entry, platformDir, executableName));
      }
    }
  }
  return candidates;
}

function renderedTestBrowser(): string {
  const candidates = [
    process.env.PETAL_CHROME_BIN,
    ...cachedChromiumCandidates(),
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/usr/bin/google-chrome',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser'
  ].filter((candidate): candidate is string => Boolean(candidate));
  const browser = candidates.find(executable);
  assert.ok(browser, `resize-grip stacking test requires Chromium; checked: ${candidates.join(', ')}`);
  return browser;
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, label: string): Promise<T> {
  return new Promise((resolvePromise, reject) => {
    const timer = setTimeout(() => reject(new Error(`${label} timed out after ${timeoutMs}ms`)), timeoutMs);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolvePromise(value);
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      }
    );
  });
}

async function launchRenderedTestBrowser(profileDir: string) {
  const browserPath = renderedTestBrowser();
  const browserArgs = [
    '--headless',
    '--single-process',
    '--no-zygote',
    '--no-sandbox',
    '--disable-gpu',
    '--disable-software-rasterizer',
    '--disable-background-networking',
    '--disable-background-timer-throttling',
    '--disable-backgrounding-occluded-windows',
    '--disable-renderer-backgrounding',
    '--allow-file-access-from-files',
    '--force-device-scale-factor=1',
    '--no-first-run',
    '--no-default-browser-check',
    `--user-data-dir=${profileDir}`,
    '--remote-debugging-pipe',
    '--no-startup-window'
  ];
  const command = process.platform === 'darwin' && process.arch === 'arm64' && basename(browserPath) === 'Google Chrome'
    ? '/usr/bin/arch'
    : browserPath;
  const args = command === '/usr/bin/arch' ? ['-arm64', browserPath, ...browserArgs] : browserArgs;
  const child = spawn(command, args, { stdio: ['ignore', 'ignore', 'pipe', 'pipe', 'pipe'] });
  const browserExited = new Promise<void>((resolveExit) => {
    child.once('exit', () => resolveExit());
  });
  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr = `${stderr}${chunk}`.slice(-8000);
  });

  let nextId = 1;
  let buffer = Buffer.alloc(0);
  const pending = new Map<number, {
    resolve: (value: any) => void;
    reject: (error: Error) => void;
    timer: ReturnType<typeof setTimeout>;
  }>();

  function rejectPending(error: Error) {
    for (const waiter of pending.values()) {
      clearTimeout(waiter.timer);
      waiter.reject(error);
    }
    pending.clear();
  }

  child.once('error', (error) => rejectPending(error));
  child.once('exit', (code, signal) => {
    if (pending.size > 0) {
      rejectPending(new Error(`rendered-test browser exited before replying (code=${code}, signal=${signal})\n${stderr}`));
    }
  });

  const protocolInput = child.stdio[3];
  const protocolOutput = child.stdio[4];
  assert.ok(protocolInput && protocolOutput, 'Chromium did not expose its remote-debugging pipes');
  protocolOutput.on('data', (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      const delimiter = buffer.indexOf(0);
      if (delimiter < 0) break;
      const rawMessage = buffer.subarray(0, delimiter).toString();
      buffer = buffer.subarray(delimiter + 1);
      if (!rawMessage) continue;
      const message = JSON.parse(rawMessage);
      if (!message.id) continue;
      const waiter = pending.get(message.id);
      if (!waiter) continue;
      pending.delete(message.id);
      clearTimeout(waiter.timer);
      if (message.error) waiter.reject(new Error(message.error.message));
      else waiter.resolve(message.result);
    }
  });

  function call(method: string, params: Record<string, unknown> = {}, sessionId?: string): Promise<any> {
    const id = nextId++;
    const message: Record<string, unknown> = { id, method, params };
    if (sessionId) message.sessionId = sessionId;
    return new Promise((resolveCall, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`${method} timed out\n${stderr}`));
      }, 10_000);
      pending.set(id, { resolve: resolveCall, reject, timer });
      protocolInput.write(`${JSON.stringify(message)}\0`);
    });
  }

  async function evaluate(sessionId: string, expression: string): Promise<any> {
    const result = await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }, sessionId);
    if (result.exceptionDetails) {
      throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text ?? 'browser evaluation failed');
    }
    return result.result?.value;
  }

  return {
    call,
    evaluate,
    stderr: () => stderr,
    async close() {
      if (child.exitCode !== null || child.signalCode !== null) return;
      child.kill('SIGTERM');
      try {
        await withTimeout(browserExited, 3000, 'Chromium shutdown');
      } catch {
        child.kill('SIGKILL');
        await withTimeout(browserExited, 3000, 'forced Chromium shutdown');
      }
    }
  };
}

async function buildFixture(buildDir: string) {
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
        '$app/environment': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
        '@petal/shared': fileURLToPath(new URL('../../shared', desktopRoot))
      }
    },
    build: {
      outDir: buildDir,
      emptyOutDir: true,
      rollupOptions: {
        input: fileURLToPath(new URL('./resize-grip-header.html', fixtureRoot))
      }
    }
  });
}

type Probe = {
  point: { x: number; y: number };
  rect: { left: number; top: number; right: number; bottom: number; width: number; height: number };
  hitSelector: string | null;
  hitIsSelf: boolean;
  hitIsResizeNw: boolean;
  hitIsResizeNe: boolean;
} | null;

type Measurement = {
  viewport: { width: number };
  trafficHide: Probe;
  trafficFit: Probe;
  overflowBtn: Probe;
  winMin: Probe;
  resizeNwRect: { left: number; top: number; right: number; bottom: number; width: number; height: number };
  resizeNeRect: { left: number; top: number; right: number; bottom: number; width: number; height: number };
};

async function renderMeasurement(
  browser: Awaited<ReturnType<typeof launchRenderedTestBrowser>>,
  fixtureUrl: string,
  width: number,
  userAgent?: string
): Promise<Measurement> {
  const { targetId } = await browser.call('Target.createTarget', { url: 'about:blank', width, height: 200 });
  const { sessionId } = await browser.call('Target.attachToTarget', { targetId, flatten: true });
  await browser.call(
    'Emulation.setDeviceMetricsOverride',
    { width, height: 200, deviceScaleFactor: 1, mobile: false, screenWidth: width, screenHeight: 200, dontSetVisibleSize: false },
    sessionId
  );
  if (userAgent) {
    await browser.call('Emulation.setUserAgentOverride', { userAgent }, sessionId);
  }
  await browser.call('Page.navigate', { url: fixtureUrl }, sessionId);

  const deadline = Date.now() + 10_000;
  let encoded: string | undefined;
  while (Date.now() < deadline) {
    const state = await browser.evaluate(
      sessionId,
      `({
        measurement: document.body?.dataset.resizeGripMeasurement ?? null,
        error: document.body?.dataset.resizeGripMeasurementError ?? null
      })`
    );
    if (state?.error) throw new Error(`resize-grip fixture failed: ${decodeURIComponent(state.error)}`);
    if (state?.measurement) {
      encoded = state.measurement as string;
      break;
    }
    await new Promise((resolvePoll) => setTimeout(resolvePoll, 50));
  }
  if (!encoded) throw new Error(`resize-grip fixture render timed out after 10000ms\n${browser.stderr()}`);
  const measurement = JSON.parse(decodeURIComponent(encoded)) as Measurement;
  await browser.call('Target.closeTarget', { targetId });
  return measurement;
}

test('remote window header controls win hit-testing over the resize-grip corners (#674)', async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-resize-grip-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-resize-grip-chrome-'));
  let browser: Awaited<ReturnType<typeof launchRenderedTestBrowser>> | undefined;

  try {
    await buildFixture(buildDir);
    browser = await launchRenderedTestBrowser(profileDir);
    const fixtureUrl = pathToFileURL(join(buildDir, 'resize-grip-header.html')).href;

    // macOS-style header at a realistic remote-window width (480px, above
    // the #497 470px breakpoint so the full mode-switcher renders and
    // .overflow-btn stays display:none -- exercised separately below):
    // traffic-hide dot sits fully inside the NW resize corner, traffic-fit
    // sits just outside it.
    const macUserAgent =
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36';
    const macMeasurement = await renderMeasurement(browser, fixtureUrl, 480, macUserAgent);

    assert.ok(macMeasurement.trafficHide, 'traffic-hide dot did not render');
    assert.equal(
      macMeasurement.trafficHide!.hitIsSelf,
      true,
      `yellow "hide" dot at (${macMeasurement.trafficHide!.point.x}, ${macMeasurement.trafficHide!.point.y}) resolved to ${macMeasurement.trafficHide!.hitSelector} instead of itself`
    );
    assert.equal(macMeasurement.trafficHide!.hitIsResizeNw, false, 'yellow "hide" dot is still covered by .resize-nw');

    assert.ok(macMeasurement.trafficFit, 'traffic-fit dot did not render');
    assert.equal(
      macMeasurement.trafficFit!.hitIsSelf,
      true,
      `green "fit" dot at (${macMeasurement.trafficFit!.point.x}, ${macMeasurement.trafficFit!.point.y}) resolved to ${macMeasurement.trafficFit!.hitSelector} instead of itself`
    );
    assert.equal(macMeasurement.trafficFit!.hitIsResizeNw, false, 'green "fit" dot is still covered by .resize-nw');

    // Narrow header (<=470px, #497's breakpoint): the labelled mode-switcher
    // is replaced by .overflow-btn, which sits in the header's top-right --
    // exactly the corner .resize-ne covers.
    const narrowMeasurement = await renderMeasurement(browser, fixtureUrl, 420, macUserAgent);

    assert.ok(narrowMeasurement.overflowBtn, 'overflow button did not render at 420px');
    assert.equal(
      narrowMeasurement.overflowBtn!.hitIsSelf,
      true,
      `overflow button at (${narrowMeasurement.overflowBtn!.point.x}, ${narrowMeasurement.overflowBtn!.point.y}) resolved to ${narrowMeasurement.overflowBtn!.hitSelector} instead of itself`
    );
    assert.equal(narrowMeasurement.overflowBtn!.hitIsResizeNe, false, 'overflow button is still covered by .resize-ne');

    // Windows header: same fixture, forced Windows user agent so
    // RemoteWindowHeader.svelte renders its .win-ctl/.win-min branch instead
    // of the macOS traffic dots -- both branches live in the SAME
    // `.traffic-lights` container this fix raises, so this proves the fix is
    // platform-agnostic rather than assuming symmetry.
    const winMeasurement = await renderMeasurement(
      browser,
      fixtureUrl,
      480,
      'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'
    );

    assert.ok(winMeasurement.winMin, 'Windows win-min button did not render under a Windows user agent');
    assert.equal(
      winMeasurement.winMin!.hitIsSelf,
      true,
      `Windows minimize button at (${winMeasurement.winMin!.point.x}, ${winMeasurement.winMin!.point.y}) resolved to ${winMeasurement.winMin!.hitSelector} instead of itself`
    );
    assert.equal(winMeasurement.winMin!.hitIsResizeNw, false, 'Windows minimize button is still covered by .resize-nw');
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([rm(buildDir, { recursive: true, force: true }), rm(profileDir, { recursive: true, force: true })]);
    }
  }
});
