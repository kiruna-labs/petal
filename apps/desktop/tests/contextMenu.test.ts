import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const menuSource = readFileSync(resolve(__dirname, '../src/lib/components/ContextMenu.svelte'), 'utf8');
const layoutSource = readFileSync(resolve(__dirname, '../src/routes/+layout.svelte'), 'utf8');

test('context menu intercepts right-click and swallows the engine default menu', () => {
  assert.match(menuSource, /window\.addEventListener\('contextmenu', onContextMenu\)/);
  // preventDefault runs unconditionally: the engine default menu never
  // appears on either platform.
  assert.match(menuSource, /\/\/ Swallow the engine's default menu[\s\S]*?event\.preventDefault\(\);/);
  assert.doesNotMatch(menuSource, /isWindows/);
});

test('context menu offers editing actions with per-action enablement', () => {
  assert.match(menuSource, /label: 'Cut'/);
  assert.match(menuSource, /label: 'Copy'/);
  assert.match(menuSource, /label: 'Paste'/);
  assert.match(menuSource, /label: 'Select all'/);
  // Cut/Copy require a selection; Paste and Select all are always available.
  assert.match(menuSource, /label: 'Cut', shortcut: `\$\{mod\}X`, enabled: hasSelection/);
  assert.match(menuSource, /label: 'Copy', shortcut: `\$\{mod\}C`, enabled: hasSelection/);
  assert.match(menuSource, /label: 'Paste', shortcut: `\$\{mod\}V`, enabled: true/);
  assert.match(menuSource, /label: 'Select all', shortcut: `\$\{mod\}A`, enabled: true/);
  // Shortcut key follows the platform (Cmd on macOS, Ctrl elsewhere).
  assert.match(menuSource, /const mod = isMac\(\) \? '⌘' : 'Ctrl\+'/);
  // Non-editable surfaces with a live selection get a bare Copy; empty
  // surfaces show no menu at all.
  assert.match(menuSource, /return \[\{ label: 'Copy', shortcut: `\$\{mod\}C`, enabled: true, onSelect: copySelection \}\];/);
  assert.match(menuSource, /return \[\];/);
});

test('context menu clipboard ops run against the right-clicked editable via execCommand', () => {
  assert.match(menuSource, /document\.execCommand\('copy'\)/);
  assert.match(menuSource, /document\.execCommand\('cut'\)/);
  assert.match(menuSource, /document\.execCommand\('paste'\)/);
  assert.match(menuSource, /onmousedown=\{onMenuMouseDown\}/);
});

test('context menu is a token-driven Petal surface (uiConsistency sweep)', () => {
  assert.match(menuSource, /background: var\(--popover-bg\);/);
  assert.match(menuSource, /border-radius: var\(--radius-popover\);/);
  assert.match(menuSource, /box-shadow: var\(--shadow-float\), var\(--shadow-inset-hairline\);/);
  assert.match(menuSource, /color: var\(--text-soft\);/);
  assert.match(menuSource, /font: 600 12px var\(--font-ui\);/);
});

test('root layout mounts the menu on every platform', () => {
  assert.match(layoutSource, /<ContextMenu \/>/);
  // No platform gate around the mount: the menu replaces the engine default
  // on Windows and macOS alike.
  assert.doesNotMatch(layoutSource, /platformKey\(\) === 'windows'[\s\S]*?<ContextMenu/);
  assert.match(layoutSource, /import ContextMenu from '\$lib\/components\/ContextMenu\.svelte'/);
});
