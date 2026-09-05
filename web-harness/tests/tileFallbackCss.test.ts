import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import assert from 'node:assert/strict';

test('camera-off initials are geometrically centered by the tile CSS', async () => {
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  const match = /\.tile \.initials\s*\{(?<body>[^}]+)\}/.exec(css);
  const body = match?.groups?.body ?? '';

  assert.match(body, /position\s*:\s*absolute/i);
  assert.match(body, /left\s*:\s*50%/i);
  assert.match(body, /top\s*:\s*50%/i);
  assert.match(body, /transform\s*:\s*translate\(-50%,\s*-50%\)/i);
  assert.match(body, /line-height\s*:\s*1/i);
  assert.match(body, /max-width\s*:\s*calc\(100%\s*-\s*32px\)/i);
  assert.match(body, /white-space\s*:\s*nowrap/i);
});

test('camera-off tiles hide the bottom-left name chip', async () => {
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  const match = /\.tile\.camera-off \.name-chip\s*\{(?<body>[^}]+)\}/.exec(css);
  const body = match?.groups?.body ?? '';

  assert.match(body, /display\s*:\s*none/i);
});

test('meeting tile grid has responsive breakpoints through phone widths', async () => {
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  const tilesMatch = /\.tiles\s*\{(?<body>[^}]+)\}/.exec(css);
  const tilesBody = tilesMatch?.groups?.body ?? '';

  assert.match(
    tilesBody,
    /grid-template-columns\s*:\s*repeat\(auto-fit,\s*minmax\(min\(100%,\s*var\(--tile-min\)\),\s*1fr\)\)/i
  );

  for (const width of [1024, 760, 560, 420]) {
    assert.match(css, new RegExp(`@media\\s*\\(max-width:\\s*${width}px\\)`, 'i'));
  }

  const phoneMatch = /@media\s*\(max-width:\s*560px\)\s*\{(?<body>[\s\S]+?)@media\s*\(max-width:\s*420px\)/i.exec(
    css
  );
  const phoneBody = phoneMatch?.groups?.body ?? '';
  assert.match(phoneBody, /\.tiles\s*\{[\s\S]*grid-template-columns\s*:\s*minmax\(0,\s*1fr\)/i);
});
