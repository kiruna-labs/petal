#!/usr/bin/env node

import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { escapeRegExp } from '../web-harness/src/escapeRegExp.mjs';

const repoRoot = resolve(import.meta.dirname, '..');
const browserUrl = (process.env.PETAL_BROWSER_URL ?? 'http://127.0.0.1:4173').replace(/\/$/, '');
const expectedVersion =
  process.env.PETAL_EXPECTED_VERSION ??
  JSON.parse(readFileSync(resolve(repoRoot, 'web-harness/package.json'), 'utf8')).version;
const escapedExpectedVersion = escapeRegExp(expectedVersion);
// The footer link carries `?platform=macos|windows` (web-harness/src/main.ts
// picks it from the user agent; backend/api/download.ts serves a platform
// artifact for it). Compare origin + path and validate the param -- an exact
// string match against the bare URL fails on every build since the param landed.
const downloadHref = 'https://app.petal.live/api/download';
const downloadPlatforms = ['macos', 'windows'];
const widths = [320, 380, 400, 420];

let chromium;
try {
  const playwrightModule = process.env.PETAL_PLAYWRIGHT_MODULE ?? resolve(repoRoot, 'apps/desktop/node_modules/playwright');
  ({ chromium } = createRequire(import.meta.url)(playwrightModule));
} catch (error) {
  console.error(
    `Browser verification requires Playwright. Install apps/desktop dependencies or set PETAL_PLAYWRIGHT_MODULE. ${
      error instanceof Error ? error.message : String(error)
    }`
  );
  process.exit(2);
}

const browser = await chromium.launch({
  headless: true,
  ...(process.env.PETAL_CHROME_BIN ? { executablePath: process.env.PETAL_CHROME_BIN } : {}),
});

try {
  for (const width of widths) {
    const page = await browser.newPage({ viewport: { width, height: 800 } });
    try {
      await page.goto(`${browserUrl}/`, { waitUntil: 'networkidle', timeout: 30_000 });
      await page.waitForFunction(() => document.querySelector('#build-version-text')?.textContent, null, {
        timeout: 10_000,
      });
      const result = await page.evaluate(({ escapedExpectedVersion: escapedExpected, downloadHref: expectedHref, downloadPlatforms: platforms }) => {
        const version = document.querySelector('#build-version-text');
        const link = document.querySelector('.web-status-bar__download');
        const footer = document.querySelector('#build-version');
        if (!version || !link || !footer) return { ok: false, reason: 'footer elements missing' };

        try {
          link.focus({ preventScroll: true, focusVisible: true });
        } catch {
          link.focus();
        }
        const linkStyle = getComputedStyle(link);
        const linkRect = link.getBoundingClientRect();
        const footerRect = footer.getBoundingClientRect();
        const versionPattern = new RegExp(`^v${escapedExpected} · .+ · \\d{4}-\\d{2}-\\d{2}$`);
        const hrefText = link.getAttribute('href') ?? '';
        let downloadLinkOk = false;
        let platform = null;
        try {
          const url = new URL(hrefText);
          platform = url.searchParams.get('platform');
          const label = platform === 'windows' ? 'Download Petal for Windows' : 'Download Petal for macOS';
          downloadLinkOk =
            `${url.origin}${url.pathname}` === expectedHref &&
            platforms.includes(platform) &&
            link.textContent === label;
        } catch {
          downloadLinkOk = false;
        }
        const visible = (element, rect) => {
          const style = getComputedStyle(element);
          return style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0;
        };

        return {
          ok:
            versionPattern.test(version.textContent ?? '') &&
            downloadLinkOk &&
            visible(version, version.getBoundingClientRect()) &&
            visible(link, linkRect) &&
            link.matches(':focus-visible') &&
            linkStyle.outlineStyle !== 'none' &&
            Number.parseFloat(linkStyle.outlineWidth) > 0 &&
            document.documentElement.scrollWidth <= window.innerWidth + 1 &&
            linkRect.left >= -1 &&
            linkRect.right <= window.innerWidth + 1 &&
            footerRect.left >= -1 &&
            footerRect.right <= window.innerWidth + 1,
          versionText: version.textContent,
          href: hrefText,
          platform,
          scrollWidth: document.documentElement.scrollWidth,
          viewportWidth: window.innerWidth,
          focusVisible: link.matches(':focus-visible'),
        };
      }, { escapedExpectedVersion, downloadHref, downloadPlatforms });

      if (!result.ok) {
        throw new Error(JSON.stringify(result));
      }
      console.log(`ok   browser footer at ${width}px: ${result.versionText}`);
    } finally {
      await page.close();
    }
  }
} finally {
  await browser.close();
}
