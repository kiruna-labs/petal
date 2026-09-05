import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { chromium } from 'playwright';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

test('the real 400px AI chat panel keeps consent controls reachable and handles authoritative outcomes', async (t) => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-ai-chat-panel-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: {
        tsconfigRaw: JSON.stringify({
          compilerOptions: { target: 'ES2022', useDefineForClassFields: true },
        }),
      },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot))),
        },
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: {
          input: fileURLToPath(new URL('./ai-chat-panel.html', fixtureRoot)),
        },
      },
    });

    browser = await chromium.launch({
      headless: true,
      args: [
        '--allow-file-access-from-files',
        '--single-process',
        '--no-zygote',
        '--disable-gpu',
        '--disable-software-rasterizer',
      ],
    });
    const page = await browser.newPage({ viewport: { width: 400, height: 700 } });
    await page.goto(pathToFileURL(join(buildDir, 'ai-chat-panel.html')).href);
    await page.waitForFunction(() => document.body.dataset.aiChatPanelReady === 'true');
    await page.evaluate(() => (window as any).aiChatPanelFixture.live());

    // Item 13: the desktop surface itself carries the third-party disclosure.
    const disclosure = page.locator('.disclosure');
    await assert.doesNotReject(() => disclosure.waitFor({ state: 'visible' }));
    assert.match(await disclosure.textContent() ?? '', /window and room voice are sent to Google/i);

    // Item 6: long real payload, real component/CSS, real 400px Chromium layout.
    await page.evaluate(() =>
      (window as any).aiChatPanelFixture.controlRequest({
        summary: `The AI wants to type a long command. ${'reasoning '.repeat(80)}`,
        literalText: `dangerous-command --force ${'payload '.repeat(240)}`,
        element: `AXTextArea “Deployment command field ${'nested target '.repeat(50)}”`,
      }),
    );
    const measurement = await page.evaluate(() => {
      const panel = document.querySelector<HTMLElement>('.ai-chat')!;
      const reject = document.querySelector<HTMLElement>('.control-reject')!;
      reject.scrollIntoView({ block: 'nearest' });
      const panelRect = panel.getBoundingClientRect();
      const rejectRect = reject.getBoundingClientRect();
      return {
        viewport: innerWidth,
        panel: {
          width: panelRect.width,
          height: panelRect.height,
          clientHeight: panel.clientHeight,
          scrollHeight: panel.scrollHeight,
        },
        reject: {
          top: rejectRect.top,
          bottom: rejectRect.bottom,
          left: rejectRect.left,
          right: rejectRect.right,
          text: reject.textContent?.trim(),
        },
        panelTop: panelRect.top,
        panelBottom: panelRect.bottom,
      };
    });
    assert.equal(measurement.viewport, 400);
    assert.ok(measurement.panel.width <= 400.5, `panel is ${measurement.panel.width}px wide`);
    assert.ok(measurement.panel.height <= 260.5, `panel is ${measurement.panel.height}px tall`);
    assert.ok(
      measurement.panel.scrollHeight > measurement.panel.clientHeight,
      'long approval payload did not exercise the panel scroll path',
    );
    assert.equal(measurement.reject.text, 'Reject');
    assert.ok(measurement.reject.top >= measurement.panelTop - 0.5);
    assert.ok(measurement.reject.bottom <= measurement.panelBottom + 0.5);
    assert.ok(measurement.reject.left >= -0.5 && measurement.reject.right <= 400.5);
    t.diagnostic(`400px approval measurement: ${JSON.stringify(measurement)}`);

    // Item 5: grant is read back from Rust status, persists after resolution,
    // and can be revoked without a pending approval card.
    await page.getByRole('button', { name: 'Allow for this session' }).click();
    await page.evaluate(() => (window as any).aiChatPanelFixture.controlResolved());
    const standing = page.getByRole('region', { name: 'AI standing window access' });
    await standing.waitFor({ state: 'visible' });
    assert.match(await standing.textContent() ?? '', /standing access/i);
    await page.getByRole('button', { name: 'Revoke access' }).click();
    await page.getByRole('region', { name: 'Window control refused' }).waitFor({ state: 'visible' });
    const rejectCalls = await page.evaluate(() =>
      (window as any).aiChatPanelFixture.calls.filter((call: any) => call.command === 'ai_chat_control_reject'),
    );
    assert.equal(rejectCalls.at(-1)?.payload.sessionId, 77);

    // Item 12: a resolved false never reads as success for resume, approval,
    // or PTT. The visible state is reverted/preserved and a note is rendered.
    await page.evaluate(() => {
      const fixture = (window as any).aiChatPanelFixture;
      fixture.outcomes.resume = false;
    });
    await page.getByRole('button', { name: 'Allow the AI to ask again' }).click();
    await page.getByRole('region', { name: 'Window control refused' }).waitFor({ state: 'visible' });
    assert.match(await page.locator('.action-notice').textContent() ?? '', /Nothing changed/i);

    await page.evaluate(async () => {
      const fixture = (window as any).aiChatPanelFixture;
      fixture.outcomes.resume = true;
      fixture.setControlStatus('ask');
      await fixture.controlRequest({ summary: 'A fresh request' }, 'fc_stale');
      fixture.outcomes.approve = false;
    });
    await page.getByRole('button', { name: 'Allow once' }).click();
    assert.equal(await page.locator('[aria-label="Window control request"]').count(), 0);
    assert.match(await page.locator('.action-notice').textContent() ?? '', /no longer current/i);

    await page.evaluate(async () => {
      const fixture = (window as any).aiChatPanelFixture;
      fixture.outcomes.ptt = false;
      await fixture.floor(null);
    });
    await page.getByRole('button', { name: 'Hold to talk' }).dispatchEvent('pointerdown');
    await page.waitForFunction(() => document.querySelector('.action-notice')?.textContent?.includes('Talk could not start'));
    assert.equal(await page.getByRole('button', { name: 'Hold to talk' }).getAttribute('aria-pressed'), 'false');

    // Item 13's remaining desktop floor parity: the real owner-side state
    // event names the holder and makes the controls unreachable until release.
    await page.evaluate(() => (window as any).aiChatPanelFixture.floor('peer-bob'));
    const floorButton = page.getByRole('button', { name: 'peer-bob is talking' });
    assert.equal(await floorButton.isDisabled(), true);
    assert.equal(await page.locator('.text-input').isDisabled(), true);
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
