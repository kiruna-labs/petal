// #245: the REAL FeedbackModal in Chromium, with the real bundled
// `@userdispatch/sdk` and its network intercepted -- nothing leaves the test.
// Send stays disabled until the message and a well-formed email are both
// present, the inline error waits for blur (or Enter) and is announced, the
// address goes out as the SDK's
// `email` field on both wire paths (JSON, and multipart with the diagnostics
// archive) and never inside the archive, and a reopened modal is prefilled
// with the last address sent. At the 400 px main window and a wider meeting
// window, nothing in the modal overflows.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { chromium, type Page } from 'playwright';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);
const EMAIL = 'riley@example.org';
const STORAGE_KEY = 'petal.feedbackEmail.v1';

function fixturePath(name: string): string {
  return fileURLToPath(new URL(name, fixtureRoot));
}

type Submission = { contentType: string; body: string };

/** Named multipart fields, plus the uploaded file's bytes, from a raw body. */
function multipartFields(submission: Submission): { fields: Record<string, string>; file: string | null } {
  const boundary = submission.contentType.split('boundary=')[1];
  const fields: Record<string, string> = {};
  let file: string | null = null;
  for (const part of submission.body.split(`--${boundary}`)) {
    const name = part.match(/name="([^"]+)"(; filename="[^"]+")?/);
    if (!name) continue;
    const content = part.split('\r\n\r\n').slice(1).join('\r\n\r\n').replace(/\r\n$/, '');
    if (name[2]) file = content;
    else fields[name[1]] = content;
  }
  return { fields, file };
}

async function send(page: Page, submissions: Submission[]): Promise<Submission> {
  const before = submissions.length;
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.getByText('Feedback sent. Thank you!').waitFor({ timeout: 10_000 });
  assert.equal(submissions.length, before + 1, 'exactly one request per Send');
  return submissions[before];
}

test('FeedbackModal requires a well-formed email, sends it as the SDK field, and remembers it', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-feedback-modal-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      // A build-time public key, exactly how a real build turns feedback on.
      define: { 'import.meta.env.VITE_USERDISPATCH_PUBLIC_KEY': JSON.stringify('pk_fixture_245_abcdefgh') },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          '$app/environment': fixturePath('sveltekit-environment.ts'),
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot)))
        }
      },
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fixturePath('feedback-modal.html') } }
    });

    browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files'] });

    for (const viewport of [{ width: 400, height: 640 }, { width: 840, height: 560 }]) {
      const context = await browser.newContext({ viewport });
      const submissions: Submission[] = [];
      await context.route(/userdispatch\.com/, async (route) => {
        const request = route.request();
        submissions.push({
          contentType: request.headers()['content-type'] ?? '',
          body: request.postDataBuffer()?.toString('utf8') ?? ''
        });
        await route.fulfill({
          status: 201,
          contentType: 'application/json',
          body: JSON.stringify({ data: { id: 'sub_fixture', status: 'new', created_at: '2026-09-25T00:00:00Z' } })
        });
      });
      const page = await context.newPage();
      await page.goto(pathToFileURL(join(buildDir, 'feedback-modal.html')).href);
      await page.waitForFunction(() => document.body.dataset.ready === 'true');
      await page.evaluate(() => (window as unknown as { __feedbackModal: { open(): Promise<void> } }).__feedbackModal.open());

      const message = page.locator('textarea');
      const email = page.getByLabel('Your email');
      const sendButton = page.getByRole('button', { name: 'Send', exact: true });
      const error = page.locator('#feedback-email-error');
      assert.equal(await email.inputValue(), '', `${viewport.width}: nothing remembered yet`);
      assert.equal(await email.getAttribute('type'), 'email');
      assert.equal(await email.getAttribute('autocomplete'), 'email');
      assert.equal(await sendButton.isDisabled(), true);

      await message.fill('Audio dropped for everyone after I shared a window.');
      assert.equal(await sendButton.isDisabled(), true, 'a message alone is not enough');
      await email.fill('riley@example');
      // Always in the DOM (aria-describedby never dangles), inside an
      // always-rendered polite live region; only the error itself toggles --
      // a region that is hidden until it speaks is not reliably announced.
      assert.equal(await error.count(), 1);
      assert.equal(await email.getAttribute('aria-describedby'), 'feedback-email-error feedback-email-hint');
      const region = page.locator('[aria-live="polite"]:has(> #feedback-email-error)');
      assert.equal(await region.count(), 1);
      assert.equal(await region.isVisible(), true);
      assert.equal(await error.getAttribute('aria-live'), null);
      assert.equal(await error.isHidden(), true, 'no error while the field is still being typed in');
      assert.equal(await error.textContent(), '');
      // Enter on a bad address: Send is disabled so nothing is submitted, and
      // instead of doing nothing the field says why -- without leaving it.
      await email.press('Enter');
      assert.equal(await error.isHidden(), false);
      assert.equal(await error.textContent(), 'Enter a valid email address, like name@example.com.');
      assert.equal(await email.getAttribute('aria-invalid'), 'true');
      assert.equal(await email.evaluate((el) => el === document.activeElement), true);
      assert.equal(submissions.length, 0);
      assert.equal(await sendButton.isDisabled(), true);

      await email.fill(`  ${EMAIL} `);
      assert.equal(await error.isHidden(), true, 'the error clears once the address is fixed');
      assert.equal(await email.getAttribute('aria-invalid'), 'false');
      assert.equal(await sendButton.isDisabled(), false);

      const fit = await page.evaluate(() => {
        const modal = document.querySelector('.modal') as HTMLElement;
        const send = [...document.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Send') as HTMLElement;
        const box = send.getBoundingClientRect();
        return {
          overflowing: [...modal.querySelectorAll<HTMLElement>('*')]
            .filter((el) => el.scrollWidth > el.clientWidth + 1 && !el.matches('textarea'))
            .map((el) => el.className),
          sendInView: box.top >= 0 && box.bottom <= innerHeight && box.left >= 0 && box.right <= innerWidth
        };
      });
      assert.deepEqual(fit.overflowing, [], `${viewport.width}: nothing in the modal overflows`);
      assert.equal(fit.sendInView, true, `${viewport.width}: Send stays on screen`);

      // JSON path: no attachment.
      const plain = await send(page, submissions);
      assert.match(plain.contentType, /^application\/json/);
      const json = JSON.parse(plain.body);
      assert.deepEqual(Object.keys(json).sort(), ['email', 'message', 'subject', 'type']);
      assert.equal(json.email, EMAIL, 'sent trimmed, as the SDK email field');
      assert.equal(
        await page.evaluate((key) => localStorage.getItem(key), STORAGE_KEY),
        EMAIL,
        'remembered on the device once sent'
      );

      // Close and reopen: prefilled, and still editable.
      await page.evaluate(() => (window as unknown as { __feedbackModal: { close(): Promise<void> } }).__feedbackModal.close());
      await page.evaluate(() => (window as unknown as { __feedbackModal: { open(): Promise<void> } }).__feedbackModal.open());
      assert.equal(await email.inputValue(), EMAIL, 'prefilled on reopen');
      assert.equal(await error.isHidden(), true);

      // Multipart path: the diagnostics archive rides along, the address stays out of it.
      await message.fill('Second report, with diagnostics.');
      await page.getByRole('checkbox').check();
      const multipart = await send(page, submissions);
      assert.match(multipart.contentType, /^multipart\/form-data/);
      const { fields, file } = multipartFields(multipart);
      assert.equal(fields.email, EMAIL);
      assert.equal(file, 'PK fixture diagnostics', 'non-vacuity: the archive really was attached');
      assert.doesNotMatch(file ?? '', /riley@example\.org/);

      // Emptying a prefilled field disables Send again, with its own error.
      await email.fill('');
      await email.blur();
      await message.fill('Third report.');
      assert.equal(await sendButton.isDisabled(), true);
      assert.equal(await error.textContent(), 'Enter your email address.');
      await context.close();
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
