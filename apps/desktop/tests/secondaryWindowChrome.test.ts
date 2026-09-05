import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import test from 'node:test';

const desktopRoot = fileURLToPath(new URL('../', import.meta.url));

function readSource(path: string) {
  return readFileSync(fileURLToPath(new URL(path, import.meta.url)), 'utf8');
}

test('standalone secondary windows do not reserve an empty titlebar strip', () => {
  for (const path of [
    '../src/routes/window-picker/+page.svelte',
    '../src/routes/network-cockpit/+page.svelte'
  ]) {
    const source = readSource(path);
    assert.doesNotMatch(source, /class="titlebar-drag"/);
    assert.doesNotMatch(source, /padding-top:\s*28px/);
  }
});

test('standalone secondary windows drag from their visible headers', () => {
  const windowPicker = readSource('../src/lib/components/WindowPicker.svelte');
  const networkCockpit = readSource('../src/lib/components/NetworkCockpit.svelte');

  assert.match(windowPicker, /<header class="picker-head" data-tauri-drag-region=/);
  assert.match(networkCockpit, /<header class="head" data-tauri-drag-region=/);
});

test('window picker prewarms windows and uses a full-height loading grid', () => {
  const route = readSource('../src/routes/window-picker/+page.svelte');
  const picker = readSource('../src/lib/components/WindowPicker.svelte');

  assert.match(route, /prewarmWindowPicker/);
  assert.match(picker, /windowMemory: WindowPickerSnapshot \| null = readStoredSnapshot\(\)/);
  assert.match(picker, /\.window-grid\.loading-grid\s*\{[\s\S]*align-content:\s*stretch;/);
  assert.match(picker, /grid-auto-rows:\s*minmax\(224px,\s*1fr\);/);
});

test('main app drag clearance is part of the painted shell, not a fixed empty strip', () => {
  const layout = readSource('../src/routes/+layout.svelte');

  assert.doesNotMatch(layout, /class="titlebar-drag"/);
  assert.match(layout, /<div class="shell-drag-surface" data-tauri-drag-region/);
  assert.match(layout, /\.shell-drag-surface\s*\{[\s\S]*background:\s*inherit;/);
});

test('Petal View has an emitted native route and transparent-overlay contract', () => {
  const nativeSource = readSource('../src-tauri/src/region_window.rs');
  const layout = readSource('../src/routes/+layout.svelte');
  const routeConfig = join(desktopRoot, 'src', 'routes', 'region-window', '+page.ts');
  const routeArtifact = join(desktopRoot, 'build', 'region-window.html');

  assert.match(nativeSource, /REGION_WINDOW_ROUTE:\s*&str\s*=\s*"region-window\.html"/);
  assert.match(nativeSource, /REGION_WINDOW_ROUTE\}\?placing=1/);
  assert.equal(existsSync(routeConfig), true, 'region-window route must opt into static prerendering');
  if (!existsSync(routeArtifact)) {
    const build = spawnSync('npm', ['run', 'build'], {
      cwd: desktopRoot,
      encoding: 'utf8',
      windowsHide: true
    });
    assert.equal(build.error, undefined, `desktop build could not start: ${build.error ?? ''}`);
    assert.equal(build.status, 0, `desktop build failed:\n${build.stderr.slice(-2000)}`);
  }
  assert.equal(existsSync(routeArtifact), true, 'desktop build must emit region-window.html');
  assert.match(layout, /region-window/);
  assert.match(layout, /background:\s*transparent\s*!important/);
});

test('Petal View consumes placement input until native settlement and keeps direct sharing reachable', () => {
  const nativeSource = readSource('../src-tauri/src/region_window.rs');
  const route = readSource('../src/routes/region-window/+page.svelte');
  const ipc = readSource('../src/lib/ipc.ts');

  assert.match(nativeSource, /region_placement_active/);
  assert.match(nativeSource, /region-placement-settled/);
  assert.match(nativeSource, /region-placement-released/);
  assert.match(nativeSource, /emit_placement_settled\(&app, &label\)/);
  assert.match(nativeSource, /emit_placement_released\(&app, &label\)/);
  assert.match(route, /data-region-share-control/);
  assert.match(route, /COMMANDS\.toggleRegionShare/);
  assert.match(route, /COMMANDS\.regionShareState/);
  assert.match(route, /event\.preventDefault\(\)/);
  assert.match(route, /placementActive \|\| placementSettlementPending/);
  assert.match(ipc, /regionPlacementSettled: 'region-placement-settled'/);
  assert.match(ipc, /regionPlacementReleased: 'region-placement-released'/);
  assert.match(ipc, /regionShareState: 'region_share_state'/);
  assert.match(ipc, /toggleRegionShare: 'toggle_region_share'/);
});
