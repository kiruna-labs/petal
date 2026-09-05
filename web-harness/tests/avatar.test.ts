import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import assert from 'node:assert/strict';

test('empty profile avatar renders a complete placeholder, not a transparent half-stroke', async () => {
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  const match = /\.profile-avatar\.is-empty::after\s*\{(?<body>[^}]+)\}/.exec(css);
  const body = match?.groups?.body ?? '';

  assert.ok(body.includes('radial-gradient'), 'empty avatar should use a filled person placeholder');
  assert.doesNotMatch(body, /border-top-color\s*:\s*transparent/i);
  assert.doesNotMatch(body, /border\s*:[^;]+currentColor/i);
});
