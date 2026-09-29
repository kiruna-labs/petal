import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const style = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

test('web create/join placeholder drops optional copy and uses the larger fitted size', () => {
  assert.match(html, /placeholder="Enter meeting name or Petal invite"/);
  assert.doesNotMatch(html, /Enter meeting name or Petal invite \(optional\)/);
  assert.match(style, /\.meeting-field input\[type='text'\]\s*{[\s\S]*font-size:\s*14px;/);
});

test('web create/join header is about twenty percent shorter', () => {
  assert.match(style, /\.hero-quiet\s*{[\s\S]*height:\s*122px;/);
});

// #243: the maintainer asked for the eyebrow to go from every version.
test('web create/join header drops the "Ready to collaborate?" eyebrow', () => {
  assert.doesNotMatch(html, /Ready to collaborate/);
  assert.doesNotMatch(html, /quiet-eyebrow|quiet-ring/);
  assert.doesNotMatch(style, /\.quiet-eyebrow|\.quiet-ring/);
  assert.match(html, /<section class="hero-quiet">\s*<div class="quiet-bloom" aria-hidden="true"><\/div>\s*<h1 class="quiet-title">/);
});

/** The first `@media <query> { … }` block, braces balanced. */
function mediaBlock(query: string): string {
  const start = style.indexOf(`@media ${query} {`);
  assert.ok(start >= 0, `style.css needs an @media ${query} block`);
  let depth = 0;
  for (let i = style.indexOf('{', start); i < style.length; i++) {
    if (style[i] === '{') depth++;
    if (style[i] === '}' && --depth === 0) return style.slice(start, i + 1);
  }
  throw new Error(`unbalanced @media ${query} block`);
}

// #243: phones in both orientations get a full-screen home, sized to the
// dynamic viewport (100vh is the tallest one on mobile, URL bar hidden).
// homeScreenPhoneRendered.test.ts measures the result in a real browser.
test('web home fills phone screens in portrait and landscape', () => {
  const phone = mediaBlock('(max-width: 600px), (max-height: 500px)');
  assert.match(phone, /\.join-card\s*\{[^}]*min-height:\s*100vh;\s*min-height:\s*100dvh;[^}]*max-height:\s*none;/);
  assert.match(phone, /\.join-card\s*\{[^}]*border-radius:\s*0;/);
  // Three auto margins split the free space 1:2 around the title and fields.
  assert.match(phone, /\.hero-quiet\s*\{[^}]*margin-top:\s*auto;/);
  assert.match(phone, /\.home-body\s*\{[^}]*flex:\s*0 0 auto;[^}]*margin-bottom:\s*auto;/);
  assert.match(style, /\.web-status-bar\s*\{[^}]*margin-top:\s*auto;/);
  assert.match(phone, /\.home-topbar\s*\{[^}]*padding-top:\s*calc\(6px \+ env\(safe-area-inset-top, 0px\)\);/);
  // Landscape phones: title and fields side by side; the connecting card
  // shares .join-card but stays one stack.
  const landscape = mediaBlock('(max-height: 500px) and (min-width: 640px)');
  assert.match(landscape, /\.join-card:not\(\.connecting-card\)\s*\{[^}]*display:\s*grid;/);
});

// #243: on a phone Save sits under the keyboard, so its Enter key saves.
test('web name field saves from the keyboard', () => {
  assert.match(html, /<input id="display-name"[^>]*enterkeyhint="done"/);
});

// #243: colour bubble left, Save right, on the card's last row.
test('web name card puts the colour bubble beside Save', () => {
  const rowStart = html.indexOf('<div class="profile-onboarding-actions">');
  const cardEnd = html.indexOf('<div class="meeting-actions">');
  assert.ok(rowStart >= 0 && cardEnd > rowStart);
  const row = html.slice(rowStart, cardEnd);
  const bubble = row.indexOf('id="profile-color-bubble"');
  const save = row.indexOf('id="profile-onboarding-done"');
  assert.ok(bubble >= 0 && save > bubble, 'the bubble and then Save must share .profile-onboarding-actions');
  assert.ok(html.indexOf('id="display-name"') < rowStart, 'the name field sits above that row');
  assert.match(style, /\.profile-onboarding-actions\s*\{[^}]*display:\s*flex;[^}]*justify-content:\s*space-between;/);
  // The picker is only as wide as the bubble now; a percentage width would
  // squeeze the colour popover to the bubble's 30px.
  const popover = /\.profile-color-options\s*\{([^}]*)\}/.exec(style)?.[1] ?? '';
  assert.match(popover, /width:\s*min\(212px, calc\(100vw - 48px\)\);/);
  assert.doesNotMatch(popover, /(?:width|inline-size):[^;]*100%/);
});
