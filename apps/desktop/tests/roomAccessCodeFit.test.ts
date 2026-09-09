// #123 + CLAUDE.md's "UI text must NEVER truncate" hard rule: the room ID is
// now permanently visible on every main-menu row and on the live hero, which
// adds a line of text to a 400px-wide window that previously only showed it
// on hover. A source-text test cannot tell "fits" from "clipped", so this
// renders the REAL MainMenu (frameless -- the menu IS the window) with the
// real self-hosted fonts in headless Chromium at both documented main-window
// widths (400px default, 380px minimum, tauri.conf.json) and asserts
// scrollWidth <= clientWidth for every element in the window, plus opacity 1
// without any hover.
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
  assert.ok(browser, `room-ID fit test requires Chromium; checked: ${candidates.join(', ')}`);
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
    // `npm test` must work immediately after `npm ci`, before
    // `svelte-kit sync` has generated .svelte-kit/tsconfig.json.
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
        input: fileURLToPath(new URL('./room-access-code.html', fixtureRoot))
      }
    }
  });
}

type TextMeasurement = {
  selector: string;
  text: string | null;
  left: number;
  right: number;
  width: number;
  height: number;
  scrollWidth: number;
  clientWidth: number;
  opacity: string;
  visibility: string;
  display: string;
  textOverflow: string;
  fontFamily: string;
  fontSize: string;
};

type Measurement = {
  viewport: { width: number; deviceScaleFactor: number };
  fonts: { status: string; mono: boolean; display: boolean };
  menu: { width: number; scrollWidth: number; clientWidth: number };
  documentScrollWidth: number;
  hero: { height: number; scrollHeight: number; clientHeight: number; overflowY: string } | null;
  heroTitle: TextMeasurement | null;
  heroCode: TextMeasurement | null;
  codes: TextMeasurement[];
  names: TextMeasurement[];
  statuses: TextMeasurement[];
  overflow: Array<{ selector: string; text: string; scrollWidth: number; clientWidth: number }>;
};

const EXPECTED_CODES = ['tob-suna-rix', 'vel-mara-dun', 'wux-nomo-zek', 'qeb-tavu-hos'];
const HERO_CODE = 'kip-vera-mol';

async function renderMeasurement(
  browser: Awaited<ReturnType<typeof launchRenderedTestBrowser>>,
  fixtureUrl: string,
  width: number
): Promise<Measurement> {
  const height = 640;
  const { targetId } = await browser.call('Target.createTarget', { url: 'about:blank', width, height });
  const { sessionId } = await browser.call('Target.attachToTarget', { targetId, flatten: true });
  await browser.call(
    'Emulation.setDeviceMetricsOverride',
    { width, height, deviceScaleFactor: 1, mobile: false, screenWidth: width, screenHeight: height, dontSetVisibleSize: false },
    sessionId
  );
  await browser.call('Page.navigate', { url: fixtureUrl }, sessionId);

  const deadline = Date.now() + 15_000;
  let encoded: string | undefined;
  while (Date.now() < deadline) {
    const state = await browser.evaluate(
      sessionId,
      `({
        measurement: document.body?.dataset.roomCodeMeasurement ?? null,
        error: document.body?.dataset.roomCodeMeasurementError ?? null
      })`
    );
    if (state?.error) throw new Error(`room-ID fixture failed: ${decodeURIComponent(state.error)}`);
    if (state?.measurement) {
      encoded = state.measurement as string;
      break;
    }
    await new Promise((resolvePoll) => setTimeout(resolvePoll, 50));
  }
  if (!encoded) throw new Error(`room-ID fixture render timed out after 15000ms\n${browser.stderr()}`);
  const measurement = JSON.parse(decodeURIComponent(encoded)) as Measurement;
  await browser.call('Target.closeTarget', { targetId });
  return measurement;
}

test('the always-visible room ID fits the real main window without truncating anything (#123)', async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-room-code-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-room-code-chrome-'));
  let browser: Awaited<ReturnType<typeof launchRenderedTestBrowser>> | undefined;

  try {
    await buildFixture(buildDir);
    browser = await launchRenderedTestBrowser(profileDir);
    const fixtureUrl = pathToFileURL(join(buildDir, 'room-access-code.html')).href;

    // 400px is tauri.conf.json's main-window width; 380px its minWidth.
    for (const width of [400, 380]) {
      const measurement = await renderMeasurement(browser, fixtureUrl, width);

      assert.equal(measurement.viewport.width, width, `${width}px browser viewport drifted`);
      assert.equal(measurement.viewport.deviceScaleFactor, 1, 'pixel test must use CSS-pixel scale 1');
      assert.equal(measurement.fonts.status, 'loaded', 'document fonts did not finish loading');
      assert.equal(measurement.fonts.mono, true, 'JetBrains Mono (the room-ID face) did not load');
      assert.equal(measurement.fonts.display, true, 'Manrope (the room-name face) did not load');

      // The IDs are rendered at all, in full, with the real mono face.
      assert.equal(measurement.codes.length, EXPECTED_CODES.length, `${width}px: not every row rendered its room ID`);
      assert.deepEqual(
        measurement.codes.map((code) => code.text),
        EXPECTED_CODES,
        `${width}px: room IDs are missing or altered`
      );
      assert.ok(measurement.heroCode, `${width}px: the live hero rendered no room ID`);
      assert.equal(measurement.heroCode!.text, HERO_CODE);

      for (const code of [...measurement.codes, measurement.heroCode!]) {
        // Visible without any hover: this is the #123 behaviour change, and
        // the fixture never dispatches a pointer event.
        assert.equal(code.opacity, '1', `${width}px: ${code.selector} is not visible without hover (opacity ${code.opacity})`);
        assert.notEqual(code.visibility, 'hidden', `${width}px: ${code.selector} is visibility:hidden`);
        assert.notEqual(code.display, 'none', `${width}px: ${code.selector} is display:none`);
        assert.ok(code.width > 0 && code.height > 0, `${width}px: ${code.selector} has no rendered box`);
        assert.match(code.fontFamily, /JetBrains Mono/, `${width}px: ${code.selector} is not using the mono face`);

        // The hard rule: fully visible, never clipped or ellipsized.
        assert.ok(
          code.scrollWidth <= code.clientWidth,
          `${width}px: room ID "${code.text}" is truncated (scrollWidth ${code.scrollWidth} > clientWidth ${code.clientWidth})`
        );
        assert.notEqual(code.textOverflow, 'ellipsis', `${width}px: ${code.selector} would ellipsize`);
        assert.ok(code.left >= 0, `${width}px: room ID "${code.text}" starts off the left edge (${code.left})`);
        assert.ok(
          code.right <= width,
          `${width}px: room ID "${code.text}" runs past the window edge (right ${code.right} > ${width})`
        );
      }

      // The ID shares its row with the name (and, on a live row, the status).
      // Those must still fit too -- the regression this rule really guards.
      assert.ok(measurement.names.length > 0, `${width}px: no room names rendered`);
      for (const text of [...measurement.names, ...measurement.statuses, measurement.heroTitle!]) {
        assert.ok(
          text.scrollWidth <= text.clientWidth,
          `${width}px: "${text.text}" is truncated (scrollWidth ${text.scrollWidth} > clientWidth ${text.clientWidth})`
        );
        assert.ok(text.right <= width + 0.5, `${width}px: "${text.text}" runs past the window edge (right ${text.right})`);
      }

      // Nothing anywhere in the window overflows horizontally, and the window
      // itself never gains a horizontal scrollbar.
      assert.deepEqual(measurement.overflow, [], `${width}px: elements overflow horizontally`);
      assert.ok(
        measurement.documentScrollWidth <= width,
        `${width}px: the window scrolls horizontally (documentScrollWidth ${measurement.documentScrollWidth})`
      );

      // The hero is `overflow: hidden`, so its extra ID line has to grow the
      // panel rather than be clipped by it.
      assert.ok(measurement.hero, `${width}px: the live hero did not render`);
      assert.ok(
        measurement.hero!.scrollHeight <= measurement.hero!.clientHeight,
        `${width}px: the live hero clips its content vertically (scrollHeight ${measurement.hero!.scrollHeight} > clientHeight ${measurement.hero!.clientHeight})`
      );
    }
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([rm(buildDir, { recursive: true, force: true }), rm(profileDir, { recursive: true, force: true })]);
    }
  }
});
