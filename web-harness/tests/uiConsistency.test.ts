import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const styleSource = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

test('browser client imports the shared Petal foundations and uses defined font roles', () => {
  assert.match(styleSource, /@import ['"]@petal\/shared\/ui\/tokens\.css['"]/);
  assert.match(styleSource, /@import ['"]@petal\/shared\/ui\/meeting-controls\.css['"]/);
  assert.doesNotMatch(styleSource, /var\(--font-sans\)/);
  assert.doesNotMatch(styleSource, /var\(--text-secondary\)/);
});

test('browser user-facing copy wraps instead of using ellipsis', () => {
  assert.doesNotMatch(styleSource, /text-overflow:\s*ellipsis/);
  assert.match(styleSource, /\.room-name\s*\{[\s\S]*overflow-wrap:\s*anywhere;[\s\S]*white-space:\s*normal;/);
  assert.match(styleSource, /\.remote-telepointer__label\s*\{[\s\S]*overflow-wrap:\s*anywhere;[\s\S]*white-space:\s*normal;/);
  assert.match(styleSource, /\.remote-window-header__title\s*\{[\s\S]*overflow-wrap:\s*anywhere;[\s\S]*white-space:\s*normal;/);
});

test('browser controls use the shared interaction register', () => {
  assert.match(styleSource, /\.btn\s*\{[\s\S]*border-radius:\s*var\(--radius-control\);/);
  assert.match(styleSource, /\.btn:focus-visible[\s\S]*outline:\s*var\(--focus-ring-width\) solid var\(--focus-ring\);/);
  assert.match(styleSource, /\.btn:disabled[\s\S]*opacity:\s*var\(--disabled-opacity\);/);
  assert.match(styleSource, /\.local-echo-ripple,[\s\S]*\.local-echo-key-flash\s*\{[\s\S]*animation:\s*none;/);
  assert.doesNotMatch(styleSource, /transition:\s*opacity 260ms linear/);
});
