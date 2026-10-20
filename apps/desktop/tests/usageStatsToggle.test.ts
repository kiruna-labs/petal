// #377: the PostHog usage-stats opt-out. Source-text assertions for the same
// reason as sentryEnabledToggle.test.ts (runes at module scope); the rendered
// 400px fit check for the Settings row lives in platformTheming.test.ts.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  USAGE_STATS_DESCRIPTION,
  USAGE_STATS_LABEL,
  USAGE_STATS_STORAGE_KEY,
  setUsageStatsEnabled,
  usageStatsEnabled
} from '@petal/shared/logic/usageStats';

const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');
const sessionStoreSource = read('../src/lib/stores/session.svelte.ts');
const layoutSource = read('../src/routes/+layout.svelte');
const settingsSource = read('../src/lib/components/Settings.svelte');
const settingsPageSource = read('../src/routes/settings/+page.svelte');
const ipcSource = read('../src/lib/ipc.ts');
const libRsSource = read('../src-tauri/src/lib.rs');

test('usageStatsEnabled defaults to true in the persisted session shape', () => {
  assert.match(sessionStoreSource, /usageStatsEnabled:\s*boolean;/);
  assert.match(sessionStoreSource, /usageStatsEnabled:\s*true/);
});

test('updateUsageStatsEnabled persists and syncs to Rust', () => {
  const fn = sessionStoreSource.match(
    /export function updateUsageStatsEnabled\(enabled: boolean\) \{[\s\S]*?\n\}/
  );
  assert.ok(fn, 'updateUsageStatsEnabled must be exported');
  assert.match(fn[0], /session\.usageStatsEnabled = enabled;/);
  assert.match(fn[0], /commit\(session\);/);
  assert.match(fn[0], /hasTauriBridge\(\)/);
  assert.match(fn[0], /invoke\(COMMANDS\.setUsageStatsEnabled,\s*\{\s*enabled\s*\}\)/);
});

test('the layout syncs the stored choice at startup (Rust holds events until it does)', () => {
  assert.match(layoutSource, /void syncUsageStatsEnabled\(\);/);
  assert.match(
    layoutSource,
    /invoke\(COMMANDS\.setUsageStatsEnabled,\s*\{\s*enabled:\s*session\.usageStatsEnabled\s*\}\)/
  );
});

test('the IPC registry and both Rust handler lists carry set_usage_stats_enabled', () => {
  assert.match(ipcSource, /setUsageStatsEnabled: 'set_usage_stats_enabled'/);
  assert.match(ipcSource, /\[COMMANDS\.setUsageStatsEnabled\]: \{ enabled: boolean \};/);
  assert.match(ipcSource, /\[COMMANDS\.setUsageStatsEnabled\]: void;/);
  const handlers = libRsSource.split('invoke_handler(tauri::generate_handler![').slice(1);
  assert.equal(handlers.length, 2, 'expected the macOS and Windows handler lists');
  for (const handler of handlers) {
    assert.match(handler, /analytics::set_usage_stats_enabled/);
    assert.match(handler, /logging::set_sentry_enabled/);
  }
});

test('Settings renders the shared copy and the route wires the toggle', () => {
  assert.match(settingsSource, /from '@petal\/shared\/logic\/usageStats'/);
  assert.match(settingsSource, /\{USAGE_STATS_LABEL\}/);
  assert.match(settingsSource, /\{USAGE_STATS_DESCRIPTION\}/);
  assert.match(settingsSource, /onUsageStatsEnabledChange\?\.\(e\.currentTarget\.checked\)/);
  assert.match(settingsPageSource, /usageStatsEnabled=\{session\.usageStatsEnabled\}/);
  assert.match(settingsPageSource, /onUsageStatsEnabledChange=\{updateUsageStatsEnabled\}/);
  assert.equal(USAGE_STATS_LABEL, 'Share anonymous usage and performance statistics');
  assert.ok(USAGE_STATS_DESCRIPTION.length > 0);
});

function memoryStorage(initial: Record<string, string> = {}) {
  const values = new Map(Object.entries(initial));
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => void values.set(key, value)
  };
}

test('shared consent check: default on, explicit opt-out off, round-trips', () => {
  assert.equal(usageStatsEnabled(memoryStorage()), true);
  assert.equal(usageStatsEnabled(memoryStorage({ [USAGE_STATS_STORAGE_KEY]: '1' })), true);
  assert.equal(usageStatsEnabled(memoryStorage({ [USAGE_STATS_STORAGE_KEY]: '0' })), false);
  const storage = memoryStorage();
  setUsageStatsEnabled(false, storage);
  assert.equal(storage.getItem(USAGE_STATS_STORAGE_KEY), '0');
  assert.equal(usageStatsEnabled(storage), false);
  setUsageStatsEnabled(true, storage);
  assert.equal(usageStatsEnabled(storage), true);
});

test('shared consent check: an opt-out holds for the page when storage refuses the write', () => {
  const throwing = {
    getItem: () => {
      throw new Error('blocked');
    },
    setItem: () => {
      throw new Error('blocked');
    }
  };
  assert.equal(usageStatsEnabled(throwing), true);
  setUsageStatsEnabled(false, throwing);
  assert.equal(usageStatsEnabled(throwing), false);
  setUsageStatsEnabled(true, throwing);
  assert.equal(usageStatsEnabled(throwing), true);
});
