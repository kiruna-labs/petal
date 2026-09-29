import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// #243: the web home's inline mark had lost the sub-path that hollows out the
// top petal, so the petal rendered filled, and nothing compared the copies.
// Every inline copy of the mark must carry the approved path from
// icon-source.svg verbatim, inside an even-odd fill (the rule that turns the
// inner sub-paths into holes).
const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');

const iconSource = read('../../apps/desktop/src-tauri/icons/icon-source.svg');
const sourcePaths = [...iconSource.matchAll(/<path d="([^"]+)"/g)].map((match) => match[1]);

const INLINE_COPIES: Record<string, string> = {
  'web home (web-harness/index.html)': read('../index.html'),
  'invite page (web-harness/api/j.ts)': read('../api/j.ts'),
  'desktop Logo.svelte': read('../../apps/desktop/src/lib/components/Logo.svelte'),
  'site favicon (site/public/favicon.svg)': read('../../site/public/favicon.svg'),
};

/** The `<svg>…</svg>` element that contains `index`. */
function enclosingSvg(source: string, index: number): string {
  const start = source.lastIndexOf('<svg', index);
  const end = source.indexOf('</svg>', index);
  assert.ok(start >= 0 && end > index, 'the mark path must sit inside an <svg> element');
  return source.slice(start, end);
}

test('icon-source.svg holds exactly one mark path, with its inner sub-paths', () => {
  assert.equal(sourcePaths.length, 1);
  // Outer outline, the top petal's hollow, the bowl, and the bowl's two
  // hollows. Fewer sub-paths means a petal renders filled.
  assert.equal(sourcePaths[0].match(/M /g)?.length, 5);
  assert.match(iconSource, /<g [^>]*fill-rule="evenodd"[^>]*>\s*<path d="/);
});

for (const [name, source] of Object.entries(INLINE_COPIES)) {
  test(`${name} draws the mark exactly as icon-source.svg does`, () => {
    const index = source.indexOf(`d="${sourcePaths[0]}"`);
    assert.ok(index >= 0, `${name} must carry the approved mark path verbatim`);
    assert.match(enclosingSvg(source, index), /fill-rule="evenodd"/);
  });
}

test('the macOS menubar glyph uses the same path', () => {
  const glyph = read('../../apps/desktop/src-tauri/src/menubar_petal_glyph.txt');
  assert.equal(glyph.trim(), sourcePaths[0]);
});
