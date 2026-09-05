import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { accessSync, constants, readFileSync, readdirSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { homedir, tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';

const toastSource = readFileSync(
  new URL('../../../shared/ui/components/Toast.svelte', import.meta.url),
  'utf8'
);
const gallerySource = readFileSync(
  new URL('../src/lib/components/Gallery.svelte', import.meta.url),
  'utf8'
);
const pillSource = readFileSync(new URL('../../../shared/ui/components/Pill.svelte', import.meta.url), 'utf8');
const menubarSource = readFileSync(
  new URL('../src/routes/menubar-popover/+page.svelte', import.meta.url),
  'utf8'
);

function cssBlock(source: string, selector: string): string {
  const marker = `${selector} {`;
  const start = source.indexOf(marker);
  assert.notEqual(start, -1, `missing CSS block for ${selector}`);
  const bodyStart = start + marker.length;
  const end = source.indexOf('}', bodyStart);
  assert.notEqual(end, -1, `unterminated CSS block for ${selector}`);
  return source.slice(bodyStart, end);
}

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);
const UPDATE_VERSION = '2.0.0-beta.20260712.123456';
const UPDATE_MESSAGE = `Update ${UPDATE_VERSION} ready — restart to install`;
const VIEWPORT_MARGIN_PX = 24;

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
  assert.ok(
    browser,
    `rendered update-toast test requires Chromium; checked: ${candidates.join(', ')}`
  );
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

async function removeTempPath(path: string): Promise<void> {
  for (let attempt = 0; attempt < 12; attempt += 1) {
    try {
      await rm(path, { recursive: true, force: true });
      return;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'EBUSY' || attempt === 11) throw error;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
}

async function launchRenderedTestBrowser(profileDir: string) {
  const browserPath = renderedTestBrowser();
  const browserArgs = [
    '--headless',
    // Keep Chromium multi-process: headless shell on Windows exits with a
    // GPU virtualization fatal error when Target.createTarget runs under
    // --single-process, even with GPU rendering disabled.
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
  // Register this before any shutdown signal so even an immediate process
  // exit cannot race past close()'s waiter.
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
      rejectPending(
        new Error(
          `rendered-test browser exited before replying (code=${code}, signal=${signal})\n${stderr}`
        )
      );
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
    const result = await call(
      'Runtime.evaluate',
      { expression, awaitPromise: true, returnByValue: true },
      sessionId
    );
    if (result.exceptionDetails) {
      throw new Error(
        result.exceptionDetails.exception?.description ??
          result.exceptionDetails.text ??
          'browser evaluation failed'
      );
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

test('desktop transient toasts wrap copied invite links instead of truncating', () => {
  const toastMessageStyles = cssBlock(toastSource, '.message');
  const pillAutoHeightStyles = cssBlock(pillSource, '.pill.auto-height');

  assert.match(toastSource, /<Pill padded autoHeight>/);
  assert.match(pillSource, /class:auto-height=\{autoHeight\}/);
  assert.match(pillAutoHeightStyles, /height:\s*auto;/);
  assert.match(pillAutoHeightStyles, /min-height:\s*46px;/);
  assert.match(toastMessageStyles, /min-width:\s*0;/);
  assert.match(toastMessageStyles, /max-width:\s*min\(360px, calc\(100vw - 64px\)\);/);
  assert.match(toastMessageStyles, /overflow-wrap:\s*anywhere;/);
  assert.match(toastMessageStyles, /white-space:\s*pre-line;/);
  assert.doesNotMatch(toastMessageStyles, /overflow:\s*hidden;/);
  assert.doesNotMatch(toastMessageStyles, /text-overflow:\s*ellipsis;/);
  assert.doesNotMatch(toastMessageStyles, /white-space:\s*nowrap;/);
});

test('available update toast fits the documented main-window widths in rendered pixels', async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-update-toast-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-update-toast-chrome-'));
  let browser: Awaited<ReturnType<typeof launchRenderedTestBrowser>> | undefined;

  try {
    // Build a standalone entry instead of booting SvelteKit's root layout:
    // this keeps unrelated Tauri startup IPC out of the test while compiling
    // and mounting the real ToastHost -> Toast -> Pill component chain.
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      // `npm test` must work immediately after `npm ci`, before
      // `svelte-kit sync` has generated .svelte-kit/tsconfig.json.
      esbuild: {
        // Vite only skips parent tsconfig discovery for the string form.
        tsconfigRaw: JSON.stringify({
          compilerOptions: { target: 'ES2022', useDefineForClassFields: true }
        })
      },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          // This standalone Vite build intentionally skips SvelteKit config,
          // so provide the browser-only virtual module used by session.svelte.
          '$app/environment': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
          // ...and the shared-package alias (SvelteKit's kit.alias injects it
          // in the real build; this bare Vite instance needs it manually).
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot)))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: {
          input: fileURLToPath(new URL('./update-toast.html', fixtureRoot))
        }
      }
    });

    browser = await launchRenderedTestBrowser(profileDir);
    const fixtureUrl = pathToFileURL(join(buildDir, 'update-toast.html')).href;

    for (const width of [380, 400]) {
      const { targetId } = await browser.call('Target.createTarget', {
        // The target window bounds alone are not a CSS viewport contract in
        // headless Chromium: it can retain its 500px default viewport. Create
        // a blank target, explicitly emulate the viewport, then navigate so
        // the fixture measures the requested width from its first layout.
        url: 'about:blank',
        width,
        height: 700
      });
      const { sessionId } = await browser.call('Target.attachToTarget', {
        targetId,
        flatten: true
      });
      await browser.call(
        'Emulation.setDeviceMetricsOverride',
        {
          width,
          height: 700,
          deviceScaleFactor: 1,
          mobile: false,
          screenWidth: width,
          screenHeight: 700,
          dontSetVisibleSize: false
        },
        sessionId
      );
      await browser.call('Page.navigate', { url: fixtureUrl }, sessionId);

      const renderDeadline = Date.now() + 10_000;
      let encodedMeasurement: string | undefined;
      while (Date.now() < renderDeadline) {
        const state = await browser.evaluate(
          sessionId,
          `({
            measurement: document.body?.dataset.toastMeasurement ?? null,
            error: document.body?.dataset.toastMeasurementError ?? null
          })`
        );
        if (state?.error) {
          throw new Error(`rendered update-toast fixture failed: ${decodeURIComponent(state.error)}`);
        }
        if (state?.measurement) {
          encodedMeasurement = state.measurement as string;
          break;
        }
        const remainingMs = renderDeadline - Date.now();
        if (remainingMs > 0) {
          await new Promise((resolvePoll) => setTimeout(resolvePoll, Math.min(50, remainingMs)));
        }
      }
      if (!encodedMeasurement) {
        throw new Error(
          `${width}px update-toast render timed out after 10000ms\n${browser.stderr()}`
        );
      }
      const measurement = JSON.parse(decodeURIComponent(encodedMeasurement));

      assert.equal(measurement.viewport.width, width, `${width}px browser viewport drifted`);
      assert.equal(measurement.viewport.deviceScaleFactor, 1, 'pixel test must use CSS-pixel scale 1');
      assert.equal(measurement.fonts.status, 'loaded', 'document fonts did not finish loading');
      assert.equal(measurement.fonts.message, true, 'Albert Sans 500 did not load');
      assert.equal(measurement.fonts.action, true, 'Albert Sans 600 did not load');
      assert.match(measurement.fonts.computedMessageFamily, /Albert Sans/, 'message uses the wrong font');
      assert.match(measurement.fonts.computedActionFamily, /Albert Sans/, 'action uses the wrong font');

      assert.equal(measurement.icon.present, true, 'available toast info icon is missing');
      assert.ok(measurement.icon.width > 0 && measurement.icon.height > 0, 'info icon is not rendered');
      assert.equal(measurement.message.text, UPDATE_MESSAGE, 'versioned update message is not rendered');
      assert.ok(measurement.message.width > 0 && measurement.message.height > 0, 'message is not visible');
      assert.equal(measurement.action.text, 'Restart now', 'update action is not rendered');
      assert.ok(measurement.action.width > 0 && measurement.action.height > 0, 'update action is not visible');
      assert.equal(measurement.dismiss.label, 'Dismiss', 'dismiss button is not rendered');
      assert.ok(
        measurement.dismiss.width > 0 && measurement.dismiss.height > 0,
        'dismiss button is not visible'
      );
      assert.ok(
        measurement.dismiss.icon.width > 0 && measurement.dismiss.icon.height > 0,
        'dismiss icon is not visible'
      );
      assert.ok(
        measurement.dismiss.icon.scrollWidth <= measurement.dismiss.icon.clientWidth,
        `dismiss icon overflows: ${measurement.dismiss.icon.scrollWidth}px > ${measurement.dismiss.icon.clientWidth}px`
      );

      const rightLimit = width - VIEWPORT_MARGIN_PX + 0.5;
      assert.ok(
        measurement.action.right <= rightLimit,
        `action right edge ${measurement.action.right}px exceeds ${rightLimit}px at ${width}px`
      );
      assert.ok(
        measurement.dismiss.right <= rightLimit,
        `dismiss right edge ${measurement.dismiss.right}px exceeds ${rightLimit}px at ${width}px`
      );
      assert.deepEqual(
        measurement.overflow,
        [],
        `${width}px update toast contains scroll overflow: ${JSON.stringify(measurement.overflow)}`
      );

      await browser.call('Target.closeTarget', { targetId });
    }
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([
        removeTempPath(buildDir),
        removeTempPath(profileDir)
      ]);
    }
  }
});

test('desktop gallery topbar tooltips wrap instead of truncating', () => {
  const topbarTooltipStyles = cssBlock(gallerySource, '.topbar-tooltip');

  assert.match(topbarTooltipStyles, /max-width:\s*148px;/);
  assert.match(topbarTooltipStyles, /overflow-wrap:\s*anywhere;/);
  assert.match(topbarTooltipStyles, /text-wrap:\s*pretty;/);
  assert.match(topbarTooltipStyles, /white-space:\s*normal;/);
  assert.doesNotMatch(topbarTooltipStyles, /overflow:\s*hidden;/);
  assert.doesNotMatch(topbarTooltipStyles, /text-overflow:\s*ellipsis;/);
  assert.doesNotMatch(topbarTooltipStyles, /white-space:\s*nowrap;/);
});

// #786 inserted a third control cell (the bug-report button) into the gallery
// topbar's right cluster, widening it by roughly one control. The test above
// pins the tooltip's CSS, and the #786 placement tests scan the markup -- but
// neither renders anything, so neither can see the thing that actually
// matters: whether the wider cluster clips a control at the narrowest real
// window. Measure it.
test('gallery topbar controls stay unclipped at the real 380px and 400px window widths (#786)', async () => {
  // One build and one browser for BOTH widths: each rendered case otherwise
  // costs a full Vite build plus a Chrome launch, and the extra concurrent
  // CDP targets make the other rendered suites flake with "Target position can
  // only be set for new windows" under load.
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-gallery-topbar-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-gallery-topbar-chrome-'));
  let browser: Awaited<ReturnType<typeof launchRenderedTestBrowser>> | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: { tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } }) },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: { alias: { $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))), '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot))) } },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fileURLToPath(new URL('./gallery-topbar.html', fixtureRoot)) }
      }
    });

    browser = await launchRenderedTestBrowser(profileDir);
    const fixtureUrl = pathToFileURL(join(buildDir, 'gallery-topbar.html')).href;

    for (const width of [380, 400]) {
      const { targetId } = await browser.call('Target.createTarget', { url: 'about:blank', width, height: 700 });
      const { sessionId } = await browser.call('Target.attachToTarget', { targetId, flatten: true });
      await browser.call('Emulation.setDeviceMetricsOverride', {
        width,
        height: 700,
        deviceScaleFactor: 1,
        mobile: false,
        screenWidth: width,
        screenHeight: 700,
        dontSetVisibleSize: false
      }, sessionId);
      await browser.call('Page.navigate', { url: fixtureUrl }, sessionId);

      const deadline = Date.now() + 15_000;
      let encoded: string | undefined;
      while (Date.now() < deadline) {
        const state = await browser.evaluate(sessionId, `({
          measurement: document.body?.dataset.galleryTopbarMeasurement ?? null,
          error: document.body?.dataset.galleryTopbarMeasurementError ?? null
        })`);
        if (state?.error) throw new Error(`gallery topbar fixture failed: ${decodeURIComponent(state.error)}`);
        if (state?.measurement) {
          encoded = state.measurement as string;
          break;
        }
        await new Promise((resolvePoll) => setTimeout(resolvePoll, 50));
      }
      assert.ok(encoded, `${width}px gallery topbar render timed out\n${browser.stderr()}`);
      const measurement = JSON.parse(decodeURIComponent(encoded));

      assert.equal(measurement.viewport, width);

      // The #786 control is present, has real size, and both edges are inside
      // the viewport -- i.e. genuinely reachable, not merely in the DOM.
      assert.ok(
        measurement.reportBug.visible,
        `#786 bug-report control is not fully on screen at ${width}px: ${JSON.stringify(measurement.reportBug)}`
      );

      // Nothing in the topbar clips its own content. "Clips" means the box
      // actually hides overflow (or ellipsizes) -- a bare scrollWidth check
      // false-positives on `.topbar-tooltip`, which is absolutely positioned
      // and designed to exceed its 32px cell while staying fully readable.
      assert.deepEqual(
        measurement.clipped,
        [],
        `gallery topbar clips content at ${width}px: ${JSON.stringify(measurement.clipped)}`
      );

      // An in-flow cluster grown too wide pushes the DOCUMENT past the
      // viewport and is cut off by the window itself -- invisible to any
      // per-element check.
      assert.ok(
        measurement.document.scrollWidth <= measurement.document.clientWidth + 1,
        `gallery chrome overflows the ${width}px window: ` +
          `${measurement.document.scrollWidth}px > ${measurement.document.clientWidth}px`
      );
      assert.ok(
        measurement.rightCluster.right <= width + 1,
        `right cluster runs past the ${width}px window edge: ${JSON.stringify(measurement.rightCluster)}`
      );

      // The room title yields space to the right cluster and wraps rather
      // than truncating. It must keep a usable width, which is what an
      // over-wide cluster could otherwise steal.
      assert.ok(
        measurement.roomName && measurement.roomName.width > 0,
        `room name was squeezed to nothing at ${width}px: ${JSON.stringify(measurement.roomName)}`
      );
      assert.equal(
        measurement.roomName.ellipsized,
        false,
        `room name still overflows at ${width}px: ${JSON.stringify(measurement.roomName)}`
      );

      await browser.call('Target.closeTarget', { targetId });
    }
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([removeTempPath(buildDir), removeTempPath(profileDir)]);
    }
  }
});

test('desktop gallery and spotlight morph persistent tiles, retarget rapidly, and honor reduced motion', async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-gallery-motion-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-gallery-motion-chrome-'));
  let browser: Awaited<ReturnType<typeof launchRenderedTestBrowser>> | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: { tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } }) },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: { alias: { $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))), '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot))) } },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fileURLToPath(new URL('./gallery-topbar.html', fixtureRoot)) }
      }
    });

    browser = await launchRenderedTestBrowser(profileDir);
    const { targetId } = await browser.call('Target.createTarget', { url: 'about:blank', width: 900, height: 700 });
    const { sessionId } = await browser.call('Target.attachToTarget', { targetId, flatten: true });
    await browser.call('Emulation.setDeviceMetricsOverride', {
      width: 900,
      height: 700,
      deviceScaleFactor: 1,
      mobile: false,
      screenWidth: 900,
      screenHeight: 700,
      dontSetVisibleSize: false
    }, sessionId);
    await browser.call('Emulation.setEmulatedMedia', {
      features: [{ name: 'prefers-reduced-motion', value: 'no-preference' }]
    }, sessionId);
    await browser.call('Page.navigate', {
      url: `${pathToFileURL(join(buildDir, 'gallery-topbar.html')).href}#motion`
    }, sessionId);

    const deadline = Date.now() + 10_000;
    while (Date.now() < deadline) {
      const ready = await browser.evaluate(
        sessionId,
        `document.querySelectorAll('.tile-wrap').length === 3 && !!document.querySelector('.layout-toggle')`
      );
      if (ready) break;
      await new Promise((resolvePoll) => setTimeout(resolvePoll, 50));
    }
    const initial = await browser.evaluate(sessionId, `({
      count: document.querySelectorAll('.tile-wrap').length,
      spotlight: !!document.querySelector('.tiles.spotlight')
    })`);
    assert.deepEqual(initial, { count: 3, spotlight: false });
    // Let the fixture's initial participant intros settle before measuring the
    // mode-change transition; otherwise the first layout request competes with
    // the mount transition and hides the very morph this regression checks.
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 240));

    const initialVideoState = await browser.evaluate(sessionId, `(() => {
      const videos = Array.from(document.querySelectorAll('video.video-el'));
      window.__galleryVideoNodes = Object.fromEntries(
        Array.from(document.querySelectorAll('[data-participant-key]')).map((tile) => [
          tile.dataset.participantKey,
          tile.querySelector('video.video-el')
        ])
      );
      window.__galleryVideoStreams = Object.fromEntries(
        Object.entries(window.__galleryVideoNodes).map(([key, video]) => [key, video?.srcObject])
      );
      return {
        count: videos.length,
        ready: videos.every((video) => video.classList.contains('ready')),
        visible: videos.every((video) => getComputedStyle(video).opacity === '1')
      };
    })()`);
    assert.deepEqual(initialVideoState, { count: 3, ready: false, visible: false });
    await browser.evaluate(sessionId, `Array.from(document.querySelectorAll('video.video-el')).forEach((video) => video.dispatchEvent(new Event('loadeddata')))`);
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 240));
    const paintedVideoState = await browser.evaluate(sessionId, `({
      ready: Array.from(document.querySelectorAll('video.video-el')).every((video) => video.classList.contains('ready')),
      visible: Array.from(document.querySelectorAll('video.video-el')).every((video) => getComputedStyle(video).opacity === '1')
    })`);
    assert.deepEqual(paintedVideoState, { ready: true, visible: true });

    await browser.evaluate(sessionId, `document.querySelector('.layout-toggle')?.click()`);
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 60));
    const gridToSpotlightVideoState = await browser.evaluate(sessionId, `(() => {
      const prior = Object.values(window.__galleryVideoNodes ?? {});
      const current = Array.from(document.querySelectorAll('video.video-el'));
      const streams = window.__galleryVideoStreams ?? {};
      return {
        priorConnected: prior.filter((video) => video?.isConnected).length,
        reused: current.filter((video) => prior.includes(video)).length,
        streamPreserved: Object.entries(window.__galleryVideoNodes ?? {}).every(([key, video]) => video?.srcObject === streams[key]),
        ready: current.filter((video) => video.classList.contains('ready')).length,
        visible: current.filter((video) => getComputedStyle(video).opacity === '1').length
      };
    })()`);
    assert.deepEqual(
      gridToSpotlightVideoState,
      { priorConnected: 3, reused: 3, streamPreserved: true, ready: 3, visible: 3 },
      `grid → spotlight replaced or blanked painted camera video elements: ${JSON.stringify(gridToSpotlightVideoState)}`
    );
    const midGridToSpotlight = await browser.evaluate(sessionId, `({
      count: document.querySelectorAll('.tile-wrap').length,
      spotlight: !!document.querySelector('.tiles.spotlight'),
      moving: document.getAnimations().some((animation) => animation.playState === 'running')
    })`);
    assert.equal(midGridToSpotlight.count, 3);
    assert.equal(midGridToSpotlight.spotlight, true);
    assert.equal(midGridToSpotlight.moving, true, 'grid → spotlight should be visibly in motion');
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 280));

    const settledSpotlight = await browser.evaluate(sessionId, `({
      count: document.querySelectorAll('.tile-wrap').length,
      main: document.querySelectorAll('.spotlight-main').length,
      thumbs: document.querySelectorAll('.spotlight-thumb').length
    })`);
    assert.deepEqual(settledSpotlight, { count: 3, main: 1, thumbs: 2 });
    const spotlightGeometry = await browser.evaluate(sessionId, `(() => {
      const rail = document.querySelector('.spotlight-rail')?.getBoundingClientRect();
      const mainElement = document.querySelector('.spotlight-main');
      const main = mainElement?.getBoundingClientRect();
      const thumbs = Array.from(document.querySelectorAll('.spotlight-thumb')).map((tile) => tile.getBoundingClientRect());
      const railElement = document.querySelector('.spotlight-rail');
      return {
        rail: rail ? { left: rail.left, right: rail.right, top: rail.top, bottom: rail.bottom } : null,
        main: main ? { left: main.left, right: main.right, top: main.top, bottom: main.bottom, width: main.width, height: main.height } : null,
        mainPosition: mainElement ? getComputedStyle(mainElement).position : null,
        thumbs: thumbs.map((rect) => ({ left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom, width: rect.width, height: rect.height })),
        scrollWidth: railElement?.scrollWidth ?? 0,
        clientWidth: railElement?.clientWidth ?? 0
      };
    })()`);
    assert.ok(spotlightGeometry.rail && spotlightGeometry.main, `spotlight geometry did not render: ${JSON.stringify(spotlightGeometry)}`);
    assert.ok(spotlightGeometry.main.width > 0 && spotlightGeometry.main.height > 0, `spotlight hero collapsed: ${JSON.stringify(spotlightGeometry)}`);
    assert.equal(spotlightGeometry.mainPosition, 'sticky');
    assert.ok(spotlightGeometry.main.left >= spotlightGeometry.rail.left - 1);
    assert.ok(spotlightGeometry.main.right <= spotlightGeometry.rail.right + 1);
    assert.ok(spotlightGeometry.thumbs.every((thumb) => thumb.top >= spotlightGeometry.main.bottom - 1 && thumb.bottom <= spotlightGeometry.rail.bottom + 1));
    assert.ok(spotlightGeometry.thumbs[1].left >= spotlightGeometry.thumbs[0].right + 11, `spotlight thumbnails overlap: ${JSON.stringify(spotlightGeometry)}`);

    // Selecting a different hero while the spotlight branch stays mounted
    // exercises the keyed hero block and the hero↔rail FLIP pair.
    await browser.evaluate(sessionId, `document.querySelector('.spotlight-thumb')?.click()`);
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 60));
    const heroSwapVideoState = await browser.evaluate(sessionId, `(() => {
      const prior = Object.values(window.__galleryVideoNodes ?? {});
      const current = Array.from(document.querySelectorAll('video.video-el'));
      const streams = window.__galleryVideoStreams ?? {};
      return {
        priorConnected: prior.filter((video) => video?.isConnected).length,
        reused: current.filter((video) => prior.includes(video)).length,
        streamPreserved: Object.entries(window.__galleryVideoNodes ?? {}).every(([key, video]) => video?.srcObject === streams[key]),
        ready: current.filter((video) => video.classList.contains('ready')).length,
        visible: current.filter((video) => getComputedStyle(video).opacity === '1').length
      };
    })()`);
    assert.deepEqual(
      heroSwapVideoState,
      { priorConnected: 3, reused: 3, streamPreserved: true, ready: 3, visible: 3 },
      `spotlight hero swap replaced or blanked painted camera video elements: ${JSON.stringify(heroSwapVideoState)}`
    );
    const midHeroSwap = await browser.evaluate(sessionId, `Array.from(document.querySelectorAll('.tile-wrap')).some((tile) => {
      const style = getComputedStyle(tile);
      return style.transform !== 'none' || style.opacity !== '1';
    })`);
    assert.equal(midHeroSwap, true, 'spotlight hero swap should be visibly in motion');
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 280));
    assert.deepEqual(
      await browser.evaluate(sessionId, `({
        count: document.querySelectorAll('.tile-wrap').length,
        main: document.querySelectorAll('.spotlight-main').length,
        thumbs: document.querySelectorAll('.spotlight-thumb').length
      })`),
      { count: 3, main: 1, thumbs: 2 }
    );

    // Retarget before the first mode change finishes. The last request wins,
    // with no duplicate outgoing tile tree left to block pointer input.
    await browser.evaluate(sessionId, `(() => {
      const toggle = document.querySelector('.layout-toggle');
      toggle?.click();
      setTimeout(() => document.querySelector('.layout-toggle')?.click(), 20);
    })()`);
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 360));
    assert.deepEqual(
      await browser.evaluate(sessionId, `({
        spotlight: !!document.querySelector('.tiles.spotlight'),
        count: document.querySelectorAll('.tile-wrap').length
      })`),
      { spotlight: true, count: 3 }
    );

    await browser.call('Emulation.setEmulatedMedia', {
      features: [{ name: 'prefers-reduced-motion', value: 'reduce' }]
    }, sessionId);
    await browser.evaluate(sessionId, `document.querySelector('.layout-toggle')?.click()`);
    await browser.evaluate(sessionId, `document.querySelector('.layout-toggle')?.click()`);
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 40));
    assert.deepEqual(
      await browser.evaluate(sessionId, `({
        spotlight: !!document.querySelector('.tiles.spotlight'),
        count: document.querySelectorAll('.tile-wrap').length
      })`),
      { spotlight: true, count: 3 }
    );
    const reducedVideoState = await browser.evaluate(sessionId, `(() => {
      const prior = Object.values(window.__galleryVideoNodes ?? {});
      const current = Array.from(document.querySelectorAll('video.video-el'));
      const streams = window.__galleryVideoStreams ?? {};
      return {
        priorConnected: prior.filter((video) => video?.isConnected).length,
        reused: current.filter((video) => prior.includes(video)).length,
        streamPreserved: Object.entries(window.__galleryVideoNodes ?? {}).every(([key, video]) => video?.srcObject === streams[key]),
        ready: current.filter((video) => video.classList.contains('ready')).length,
        visible: current.filter((video) => getComputedStyle(video).opacity === '1').length
      };
    })()`);
    assert.deepEqual(
      reducedVideoState,
      { priorConnected: 3, reused: 3, streamPreserved: true, ready: 3, visible: 3 },
      `reduced-motion layout change replaced or blanked painted camera video elements: ${JSON.stringify(reducedVideoState)}`
    );

    await browser.call('Target.closeTarget', { targetId });
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([removeTempPath(buildDir), removeTempPath(profileDir)]);
    }
  }
});

test('Accessibility repair instructions fit in measured 400px onboarding width', async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-accessibility-repair-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-accessibility-repair-chrome-'));
  let browser: Awaited<ReturnType<typeof launchRenderedTestBrowser>> | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: { tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } }) },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: { alias: { $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))), '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot))) } },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fileURLToPath(new URL('./accessibility-repair.html', fixtureRoot)) }
      }
    });

    browser = await launchRenderedTestBrowser(profileDir);
    const { targetId } = await browser.call('Target.createTarget', { url: 'about:blank', width: 400, height: 800 });
    const { sessionId } = await browser.call('Target.attachToTarget', { targetId, flatten: true });
    await browser.call('Emulation.setDeviceMetricsOverride', {
      width: 400,
      height: 800,
      deviceScaleFactor: 1,
      mobile: false,
      screenWidth: 400,
      screenHeight: 800,
      dontSetVisibleSize: false
    }, sessionId);
    await browser.call('Page.navigate', {
      url: pathToFileURL(join(buildDir, 'accessibility-repair.html')).href
    }, sessionId);

    const deadline = Date.now() + 10_000;
    let encoded: string | undefined;
    while (Date.now() < deadline) {
      const state = await browser.evaluate(sessionId, `({
        measurement: document.body?.dataset.accessibilityRepairMeasurement ?? null,
        error: document.body?.dataset.accessibilityRepairMeasurementError ?? null
      })`);
      if (state?.error) throw new Error(`Accessibility repair fixture failed: ${decodeURIComponent(state.error)}`);
      if (state?.measurement) {
        encoded = state.measurement as string;
        break;
      }
      await new Promise((resolvePoll) => setTimeout(resolvePoll, 50));
    }
    assert.ok(encoded, `400px Accessibility repair render timed out\n${browser.stderr()}`);
    const measurement = JSON.parse(decodeURIComponent(encoded));
    assert.equal(measurement.viewport, 400);
    assert.equal(measurement.instructions, 'Remove the stale Petal row. Add /Applications/Petal.app, then enable it. Return here and restart Petal.');
    assert.equal(measurement.fallback, 'Petal could not restart. Quit Petal, then open /Applications/Petal.app.');
    assert.ok(measurement.row.left >= -0.5 && measurement.row.right <= 400.5);
    assert.deepEqual(measurement.overflow, [], `400px repair UI overflows: ${JSON.stringify(measurement.overflow)}`);
    await browser.call('Target.closeTarget', { targetId });
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([removeTempPath(buildDir), removeTempPath(profileDir)]);
    }
  }
});

test('menubar utility row fits all three actions at the real 280px popover width', async () => {
  // The row is `grid-template-columns: 1fr 1fr` and now holds three items, so
  // "Quit" wraps to its own row. The popover is content-fit height (capped at
  // 480px), so wrapping is fine -- what is NOT fine is any label clipping in a
  // 137px grid cell. Measure it; do not reason about it.
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-menubar-utility-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-menubar-utility-chrome-'));
  let browser: Awaited<ReturnType<typeof launchRenderedTestBrowser>> | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: { tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } }) },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: { alias: { $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))), '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot))) } },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fileURLToPath(new URL('./menubar-utility-row.html', fixtureRoot)) }
      }
    });

    browser = await launchRenderedTestBrowser(profileDir);
    const { targetId } = await browser.call('Target.createTarget', { url: 'about:blank', width: 280, height: 800 });
    const { sessionId } = await browser.call('Target.attachToTarget', { targetId, flatten: true });
    await browser.call('Emulation.setDeviceMetricsOverride', {
      width: 280,
      height: 800,
      deviceScaleFactor: 1,
      mobile: false,
      screenWidth: 280,
      screenHeight: 800,
      dontSetVisibleSize: false
    }, sessionId);
    await browser.call('Page.navigate', {
      url: pathToFileURL(join(buildDir, 'menubar-utility-row.html')).href
    }, sessionId);

    const deadline = Date.now() + 10_000;
    let encoded: string | undefined;
    while (Date.now() < deadline) {
      const state = await browser.evaluate(sessionId, `({
        measurement: document.body?.dataset.menubarUtilityRowMeasurement ?? null,
        error: document.body?.dataset.menubarUtilityRowMeasurementError ?? null
      })`);
      if (state?.error) throw new Error(`menubar utility row fixture failed: ${decodeURIComponent(state.error)}`);
      if (state?.measurement) {
        encoded = state.measurement as string;
        break;
      }
      await new Promise((resolvePoll) => setTimeout(resolvePoll, 50));
    }
    assert.ok(encoded, `280px menubar utility row render timed out\n${browser.stderr()}`);
    const measurement = JSON.parse(decodeURIComponent(encoded));

    // The fixture models `.utility-row` inline; if production's real rule ever
    // drifts from that model the measurement stops describing the shipped UI.
    // Pin the three properties the model depends on (same cssBlock() bridge
    // this file already uses for .copy-status).
    const utilityRowStyles = cssBlock(menubarSource, '.utility-row');
    assert.match(utilityRowStyles, /grid-template-columns:\s*1fr 1fr;/, 'fixture models a 2-column grid');
    assert.match(utilityRowStyles, /gap:\s*6px;/, 'fixture models a 6px gap');
    assert.match(utilityRowStyles, /padding:\s*8px;/, 'fixture models 8px padding');

    assert.equal(measurement.viewport, 280);
    assert.deepEqual(
      measurement.items.map((item: { label: string }) => item.label),
      ['Open Petal', 'Settings', 'Quit']
    );
    assert.deepEqual(
      measurement.overflow,
      [],
      `menubar utility row clips text: ${JSON.stringify(measurement.overflow)}`
    );
    for (const item of measurement.items) {
      assert.ok(
        item.labelScrollWidth <= item.labelClientWidth,
        `"${item.label}" label overflows its cell: ${item.labelScrollWidth}px > ${item.labelClientWidth}px`
      );
    }
    // The row's own content must fit the popover. A `1fr` track will not shrink
    // below min-content, so an over-full row grows past the 280px popover and
    // is cut off by IT -- per-element scrollWidth never sees that, which is why
    // this assertion exists alongside the per-element one.
    assert.ok(
      measurement.row.scrollWidth <= measurement.row.clientWidth,
      `utility row content overflows itself: ${measurement.row.scrollWidth}px > ${measurement.row.clientWidth}px`
    );
    assert.ok(
      measurement.row.hostScrollWidth <= measurement.row.hostClientWidth,
      `utility row overflows the ${measurement.popoverWidth}px popover: ` +
        `${measurement.row.hostScrollWidth}px > ${measurement.row.hostClientWidth}px`
    );

    // The row must stay inside the popover, and "Quit" must wrap onto a second
    // row rather than being squeezed into a third column.
    assert.ok(measurement.row.left >= -0.5 && measurement.row.right <= 280.5);
    const tops = measurement.items.map((item: { top: number }) => item.top);
    assert.equal(tops[0], tops[1], 'Open Petal and Settings share the first row');
    assert.ok(tops[2] > tops[0], 'Quit wraps to its own row rather than shrinking the cells');
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([removeTempPath(buildDir), removeTempPath(profileDir)]);
    }
  }
});

test('desktop gallery grid reflow uses restrained list motion and FLIP', () => {
  const tileWrapStyles = cssBlock(gallerySource, '.tile-wrap');

  assert.match(gallerySource, /import \{ flip \} from 'svelte\/animate';/);
  assert.match(gallerySource, /import \{ fade \} from 'svelte\/transition';/);
  assert.match(gallerySource, /import \{ tileLayoutDuration, tileTransitionDuration \} from '\$lib\/motion';/);
  assert.match(gallerySource, /function transitionGalleryLayout\(mutate: \(\) => void\)/);
  assert.match(gallerySource, /tile\.animate\(/);
  assert.match(gallerySource, /data-participant-key=\{p\.key\}/);
  assert.match(gallerySource, /duration, easing: 'cubic-bezier\(0\.2, 0, 0, 1\)', fill: 'none'/);
  assert.doesNotMatch(tileWrapStyles, /(?:width|height) var\(--motion-/);
  assert.doesNotMatch(gallerySource, /transition:[^;]*grid-template/);
});

test('desktop menubar copied invite status wraps instead of truncating', () => {
  const copyStatusStyles = cssBlock(menubarSource, '.copy-status');

  assert.match(copyStatusStyles, /max-width:\s*calc\(280px - 28px\);/);
  assert.match(copyStatusStyles, /overflow-wrap:\s*anywhere;/);
  assert.match(copyStatusStyles, /white-space:\s*pre-line;/);
  assert.doesNotMatch(copyStatusStyles, /overflow:\s*hidden;/);
  assert.doesNotMatch(copyStatusStyles, /text-overflow:\s*ellipsis;/);
  assert.doesNotMatch(copyStatusStyles, /white-space:\s*nowrap;/);
});
