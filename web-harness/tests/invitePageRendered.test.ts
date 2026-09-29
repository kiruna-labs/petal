import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { resolve } from 'node:path';
import test from 'node:test';
import type { VercelRequest, VercelResponse } from '../api/_lib/vercel.js';
import joinHandler from '../api/j.ts';

const repoRoot = resolve(import.meta.dirname, '../..');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

const IPHONE_UA =
  'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1';
const MAC_UA = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36';

async function invitePage(label: string, userAgent: string): Promise<string> {
  let body = '';
  const response = {
    status: () => response,
    setHeader: () => response,
    send: (sent: unknown) => {
      body = String(sent);
      return response;
    },
    json: () => response,
    end: () => response,
  };
  await joinHandler(
    { method: 'GET', query: { label, code: 'abc-defg-hjk' }, headers: { 'user-agent': userAgent } } as unknown as VercelRequest,
    response as unknown as VercelResponse,
  );
  return body;
}

// #242: a room label with no spaces or hyphens (a pasted slug) wraps inside
// the page instead of widening it -- which on a phone zooms the whole page out.
test('the invite page never scrolls sideways for a long unbreakable room label', { timeout: 60_000 }, async () => {
  const label = 'Donaudampfschifffahrtsgesellschaftskapitaen';
  const browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-gpu'] });
  try {
    const cases = [
      { name: 'phone', userAgent: IPHONE_UA, viewport: { width: 375, height: 667 }, isMobile: true, hasTouch: true },
      { name: 'small phone', userAgent: IPHONE_UA, viewport: { width: 320, height: 568 }, isMobile: true, hasTouch: true },
      { name: 'computer', userAgent: MAC_UA, viewport: { width: 1280, height: 800 }, isMobile: false, hasTouch: false },
    ];
    for (const { name, userAgent, ...device } of cases) {
      const context = await browser.newContext({ ...device, userAgent });
      // Nothing leaves the machine: no fonts, no petal:// hand-off, no downloads.
      await context.route('**/*', (route: { abort: () => Promise<void> }) => route.abort());
      const page = await context.newPage();
      await page.setContent(await invitePage(label, userAgent), { waitUntil: 'load' });
      const layout = await page.evaluate(() => ({
        innerWidth: window.innerWidth,
        scrollWidth: document.documentElement.scrollWidth,
        headingRight: document.querySelector('h1')!.getBoundingClientRect().right,
        heading: document.querySelector('h1')!.textContent,
      }));
      assert.match(layout.heading ?? '', new RegExp(label), name);
      assert.equal(layout.innerWidth, device.viewport.width, `${name}: the page is not zoomed out`);
      assert.ok(layout.scrollWidth <= layout.innerWidth, `${name}: ${layout.scrollWidth}px wide in a ${layout.innerWidth}px viewport`);
      assert.ok(layout.headingRight <= layout.innerWidth, `${name}: the heading ends at ${layout.headingRight}px`);
      await context.close();
    }
  } finally {
    await browser.close();
  }
});
