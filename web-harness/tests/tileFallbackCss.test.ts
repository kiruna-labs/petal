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

test('#204 the meeting tile grid places cells from the shared packer and never decides columns itself', async () => {
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  const tilesMatch = /\.tiles\s*\{(?<body>[^}]+)\}/.exec(css);
  const tilesBody = tilesMatch?.groups?.body ?? '';

  // #239: each track is at most the packed tile, and the block of tracks is
  // centred -- the space 16:9 tiles cannot use surrounds the group instead
  // of opening dead bands between neighbours. Columns are half tracks (a
  // tile spans two) so a short last row can be centred.
  assert.match(
    tilesBody,
    /grid-template-columns\s*:\s*repeat\(\s*calc\(var\(--gallery-cols\)\s*\*\s*2\),\s*minmax\(0,\s*calc\(\(var\(--gallery-tile-width\)\s*-\s*var\(--gallery-gap\)\)\s*\/\s*2\)\)\s*\)/i
  );
  assert.match(
    tilesBody,
    /grid-template-rows\s*:\s*repeat\(var\(--gallery-rows\),\s*minmax\(0,\s*var\(--gallery-tile-height\)\)\)/i
  );
  assert.match(tilesBody, /place-items\s*:\s*center/i);
  assert.match(tilesBody, /place-content\s*:\s*safe center/i);
  // An engine without the `safe` keyword drops that declaration: each one
  // follows a plain `center` to fall back on.
  const safeCentres = css.match(/place-content\s*:\s*safe center/gi) ?? [];
  const withFallback = css.match(/place-content\s*:\s*center;\s*place-content\s*:\s*safe center/gi) ?? [];
  assert.ok(safeCentres.length >= 2, 'the grid and the spotlight both centre safely');
  assert.equal(withFallback.length, safeCentres.length, 'every `safe center` has a plain `center` before it');
  assert.match(tilesBody, /gap\s*:\s*var\(--gallery-gap\)/i);
  assert.doesNotMatch(css, /auto-fit/i, 'CSS must not pick a column count of its own');
  assert.doesNotMatch(css, /--tile-min\b/, 'the minmax breakpoint knobs are gone with the packer');
  assert.doesNotMatch(css, /--tile-row-min\b/);

  const tileSizing = /\.tiles\.layout-grid\s*>\s*\.tile\s*\{(?<body>[^}]+)\}/.exec(css)?.groups?.body ?? '';
  assert.match(tileSizing, /width\s*:\s*min\(100%,\s*var\(--gallery-tile-width\)\)/i);
  assert.match(tileSizing, /height\s*:\s*min\(100%,\s*var\(--gallery-tile-height\)\)/i);
  assert.match(tileSizing, /aspect-ratio\s*:\s*16\s*\/\s*9/i);
  assert.match(tileSizing, /grid-column-end\s*:\s*span 2/i);
});

test('#248 camera video crops only where cameraFit.ts says so; shares always letterbox', async () => {
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  const base = /\.tile video,\s*\.tile canvas\.full-range-canvas\s*\{(?<body>[^}]+)\}/.exec(css)?.groups?.body ?? '';
  assert.match(base, /object-fit\s*:\s*contain/i, 'every tile video letterboxes by default');
  const cover = /\.tile video\.camera-video\[data-fit='cover'\]\s*\{(?<body>[^}]+)\}/.exec(css)?.groups?.body ?? '';
  assert.match(cover, /object-fit\s*:\s*cover/i);
  // No other rule may crop tile media: a share must never be cropped.
  const uncommented = css.replace(/\/\*[\s\S]*?\*\//g, '');
  const coverRules = [...uncommented.matchAll(/(?<selector>[^{}]+)\{[^}]*object-fit\s*:\s*cover[^}]*\}/gi)].map((m) =>
    (m.groups?.selector ?? '').trim()
  );
  const tileCoverRules = coverRules.filter((selector) => /\.tile\b/.test(selector));
  assert.deepEqual(tileCoverRules, [".tile video.camera-video[data-fit='cover']"]);
});

test('meeting tile breakpoints still tighten gap and padding through phone widths', async () => {
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  for (const width of [1024, 760, 560, 420]) {
    assert.match(css, new RegExp(`@media\\s*\\(max-width:\\s*${width}px\\)`, 'i'));
  }
  const phoneMatch = /@media\s*\(max-width:\s*560px\)\s*\{(?<body>[\s\S]+?)@media\s*\(max-width:\s*420px\)/i.exec(css);
  const phoneBody = phoneMatch?.groups?.body ?? '';
  // Several `.tiles {` blocks can sit inside that region; the phone override
  // is the one with the 10px gap, and none of them may set a column count.
  const phoneTilesBodies = [...phoneBody.matchAll(/\.tiles\s*\{(?<body>[^}]+)\}/g)].map((m) => m.groups?.body ?? '');
  assert.ok(phoneTilesBodies.length > 0, 'the 560px block still tunes .tiles');
  assert.ok(
    phoneTilesBodies.some((body) => /--tile-gap\s*:\s*10px/i.test(body) && /--tile-pad\s*:\s*12px/i.test(body)),
    'the phone override keeps the tighter gap and padding'
  );
  for (const body of phoneTilesBodies) {
    assert.doesNotMatch(body, /grid-template-columns/i, 'a phone width is a packer input, not a CSS override');
  }
});
