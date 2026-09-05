import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import {
  CONTROL_MODE_CHOICES,
  DEBUG_MENU_ITEM_ID,
  DEBUG_MENU_ITEM_LABEL,
  HOVER_TAB_POSITION_CHOICES,
  HOVER_TAB_POSITION_SECTION_LABEL,
  QUALITY_PRIORITY_CHOICES,
  QUALITY_PRIORITY_SECTION_LABEL,
  buildHoverTabMenuEntries
} from '../src/lib/data/hoverTabMenu.ts';
import {
  buildShareOptionsMenuEntries,
  type ShareOptionsMenuEntry
} from '../src/lib/data/shareOptionsMenu.ts';
import { dispatchShareOptionsMenuEntry } from '../src/lib/shareOptionsPopup.ts';

const __dirname = dirname(fileURLToPath(import.meta.url));
const hoverTabSource = readFileSync(resolve(__dirname, '../src/routes/hover-tab/+page.svelte'), 'utf8');
const popupSource = readFileSync(resolve(__dirname, '../src/lib/shareOptionsPopup.ts'), 'utf8');
const regionSource = readFileSync(resolve(__dirname, '../src/routes/region-window/+page.svelte'), 'utf8');

function cssRule(source: string, selector: string): string {
  const escapedSelector = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = source.match(new RegExp(`(?:^|\\n)  ${escapedSelector}\\s*\\{([\\s\\S]*?)\\n  \\}`));
  assert.ok(match, `${selector} rule should exist`);
  return match[1];
}

test('the native menu keeps the existing priority, control, debug, draw, and AI entries', () => {
  const entries = buildHoverTabMenuEntries('automatic', true, false, 'fullControl', true, true, true);
  assert.deepEqual(entries[0], { kind: 'section-label', text: QUALITY_PRIORITY_SECTION_LABEL });
  assert.deepEqual(
    entries.filter((entry) => entry.kind === 'priority').map((entry) => entry.value),
    QUALITY_PRIORITY_CHOICES.map((choice) => choice.value)
  );
  const modes = entries.filter((entry) => entry.kind === 'control-mode');
  assert.deepEqual(modes.map((entry) => entry.value), CONTROL_MODE_CHOICES.map((choice) => choice.value));
  assert.ok(modes.every((entry) => entry.enabled));
  assert.equal(entries.filter((entry) => entry.kind === 'debug').length, 1);
  assert.equal(entries.filter((entry) => entry.kind === 'annotation').length, 1);
  assert.equal(entries.filter((entry) => entry.kind === 'ai-chat').length, 1);
  assert.equal(DEBUG_MENU_ITEM_ID, 'open-network-cockpit');
  assert.equal(DEBUG_MENU_ITEM_LABEL, 'Debug');
});


test('the per-share remote-control lock is offered on every platform, and only while sharing', () => {
  // Not behind `remoteControlSupported`: that flag gates the Windows-only
  // control MODES, but permission applies everywhere. macOS shipping without
  // this toggle was the whole gap.
  const shared = buildHoverTabMenuEntries('automatic', true, false, 'cursorPreserving', false);
  const toggle = shared.find((entry) => entry.kind === 'remote-control-allowed');
  assert.ok(toggle, 'sharing on a non-Windows platform must still offer the lock');
  assert.equal(toggle.enabled, true);
  assert.equal(toggle.checked, true, 'default is ALLOW');

  // Not sharing: nothing to lock.
  const unshared = buildHoverTabMenuEntries('automatic', false);
  const disabled = unshared.find((entry) => entry.kind === 'remote-control-allowed');
  assert.ok(disabled);
  assert.equal(disabled.enabled, false);

  // Reflects a denial rather than always claiming allowed.
  const denied = buildHoverTabMenuEntries(
    'automatic', true, false, 'cursorPreserving', true, false, false, false, false, 0.5, false
  );
  const deniedToggle = denied.find((entry) => entry.kind === 'remote-control-allowed');
  assert.equal(deniedToggle?.checked, false);
});

test('hover-only position entries offer Top, Center, and Bottom without leaking into Petal View', () => {
  const hoverEntries = buildShareOptionsMenuEntries(
    'automatic',
    true,
    false,
    'cursorPreserving',
    true,
    false,
    false,
    false,
    true,
    0.5
  );
  const positions = hoverEntries.filter((entry) => entry.kind === 'position');
  assert.deepEqual(
    positions.map((entry) => entry.value),
    HOVER_TAB_POSITION_CHOICES.map((choice) => choice.value)
  );
  assert.equal(positions.find((entry) => entry.value === 'center')?.checked, true);
  assert.ok(hoverEntries.some((entry) => entry.kind === 'section-label' && entry.text === HOVER_TAB_POSITION_SECTION_LABEL));

  const regionEntries = buildShareOptionsMenuEntries('automatic', true);
  assert.equal(regionEntries.some((entry) => entry.kind === 'position'), false);
});

test('native menu dispatch invokes each enabled action and blocks disabled ones', () => {
  const calls: string[] = [];
  const actions = {
    onPriority: (value: string) => calls.push(`priority:${value}`),
    onControlMode: (value: string) => calls.push(`control:${value}`),
    onDraw: (active: boolean) => calls.push(`draw:${active}`),
    onAiChat: () => calls.push('ai'),
    onDebug: () => calls.push('debug'),
    onPosition: (value: string) => calls.push(`position:${value}`)
  };
  const entries: ShareOptionsMenuEntry[] = [
    { kind: 'priority', id: 'p', text: 'Priority', value: 'sharpText', checked: false },
    { kind: 'position', id: 'top', text: 'Top', value: 'top', checked: false },
    { kind: 'control-mode', id: 'c', text: 'Control', value: 'fullControl', checked: false, enabled: true },
    { kind: 'control-mode', id: 'disabled-c', text: 'Control', value: 'cursorPreserving', checked: false, enabled: false },
    { kind: 'annotation', id: 'draw', text: 'Draw', enabled: true, checked: false },
    { kind: 'annotation', id: 'disabled-draw', text: 'Draw', enabled: false, checked: false },
    { kind: 'ai-chat', id: 'ai', text: 'AI', enabled: true, checked: false },
    { kind: 'ai-chat', id: 'disabled-ai', text: 'AI', enabled: false, checked: false },
    { kind: 'debug', id: 'debug', text: 'Debug' }
  ];
  for (const entry of entries) dispatchShareOptionsMenuEntry(entry, actions)?.();
  assert.deepEqual(calls, [
    'priority:sharpText',
    'position:top',
    'control:fullControl',
    'draw:true',
    'ai',
    'debug'
  ]);
});

test('unsupported platforms omit control-mode entries without changing other menu content', () => {
  const entries = buildHoverTabMenuEntries('dataSaver', false, false, 'cursorPreserving', false);
  assert.equal(entries.some((entry) => entry.kind === 'control-mode'), false);
  assert.equal(entries.filter((entry) => entry.kind === 'priority').length, QUALITY_PRIORITY_CHOICES.length);
  assert.equal(entries.filter((entry) => entry.kind === 'debug').length, 1);
  assert.equal(entries.filter((entry) => entry.kind === 'annotation').length, 1);
});

test('Draw entry is checked only while drawing and can stop drawing from the menu', () => {
  const entry = buildHoverTabMenuEntries('automatic', true, true).find((item) => item.kind === 'annotation');
  assert.deepEqual(entry, {
    kind: 'annotation',
    id: 'draw-on-shared-window',
    text: 'Stop drawing on this window',
    enabled: true,
    checked: true
  });
  assert.match(hoverTabSource, /onDraw: \(active\) => void selectDraw\(active\)/);
  assert.match(hoverTabSource, /Drawing is active on this shared window/);
});

test('the fixed tab exposes native options only through pointer and keyboard context-menu paths', () => {
  assert.match(hoverTabSource, /aria-haspopup="menu"/);
  assert.match(hoverTabSource, /oncontextmenu=\{onActionContextMenu\}/);
  assert.match(hoverTabSource, /onkeydown=\{onActionKeyDown\}/);
  assert.match(hoverTabSource, /event\.preventDefault\(\);/);
  assert.match(hoverTabSource, /event\.stopPropagation\(\);/);
  assert.match(hoverTabSource, /keyboardInvocation && actionButton/);
  assert.match(hoverTabSource, /new LogicalPosition\(rect\.left, rect\.bottom\)/);
  assert.match(hoverTabSource, /getCurrentWindow\(\)/);
  assert.doesNotMatch(hoverTabSource, /class="hover-tab-options"|class="hover-tab-tray"/);
  assert.match(hoverTabSource, /class="hover-tab-action hover-tab-trigger"/);
  assert.match(regionSource, /popupShareOptionsMenu\(entries,/);
});

test('the fixed tab keeps a concise native tooltip and preserves rich accessibility context', () => {
  assert.match(hoverTabSource, /shareActionTooltip/);
  assert.match(hoverTabSource, /right-click for options/);
  assert.match(hoverTabSource, /data-allow-native-tooltip=\{isWindows\(\) \? 'true' : undefined\}/);
  assert.match(hoverTabSource, /title=\{isWindows\(\) \? shareActionTooltip : undefined\}/);
  assert.match(hoverTabSource, /aria-label=\{shareActionAriaLabel\}/);
  assert.match(hoverTabSource, /aria-keyshortcuts="Shift\+F10,ContextMenu"/);
});

test('the shared popup forwards optional keyboard placement and always closes native resources', () => {
  assert.match(popupSource, /export interface ShareOptionsMenuPopupOptions/);
  assert.match(popupSource, /if \(options\)[\s\S]*menu\.popup\(options\.position, options\.window\)/);
  assert.match(popupSource, /menu\.popup\(\);/);
  assert.match(popupSource, /await menu\.close\(\);/);
  assert.match(popupSource, /await Promise\.all\(items\.map\(\(item\) => item\.close\(\)\)\);/);
});


test('the lock dispatches the OPPOSITE of its current state, and never while disabled', () => {
  const calls: boolean[] = [];
  const actions = {
    onPriority() {},
    onControlMode() {},
    onDraw() {},
    onAiChat() {},
    onDebug() {},
    onRemoteControlAllowed(allowed: boolean) {
      calls.push(allowed);
    }
  };
  // Checked (allowed) -> clicking must DENY.
  dispatchShareOptionsMenuEntry(
    { kind: 'remote-control-allowed', id: 'x', text: 'Allow remote control', enabled: true, checked: true },
    actions
  )?.();
  assert.deepEqual(calls, [false]);

  // Unchecked (denied) -> clicking must ALLOW.
  dispatchShareOptionsMenuEntry(
    { kind: 'remote-control-allowed', id: 'x', text: 'Allow remote control', enabled: true, checked: false },
    actions
  )?.();
  assert.deepEqual(calls, [false, true]);

  // Disabled entries yield no callback at all -- a second guard beyond the
  // native enabled flag, so a stale menu cannot flip a lock.
  assert.equal(
    dispatchShareOptionsMenuEntry(
      { kind: 'remote-control-allowed', id: 'x', text: 'Allow remote control', enabled: false, checked: true },
      actions
    ),
    undefined
  );
  assert.deepEqual(calls, [false, true], 'a disabled entry must not dispatch');
});

test('the fixed CSS prevents copy or transparent overflow from changing the native hit surface', () => {
  const hostRule = cssRule(hoverTabSource, '.hover-tab-host');
  const pillRule = cssRule(hoverTabSource, '.hover-tab-host :global(.pill.attach)');
  const insetPillRule = cssRule(hoverTabSource, '.hover-tab-host.inset :global(.pill.attach-right)');
  const buttonRule = cssRule(hoverTabSource, '.hover-tab-action');
  const insetButtonRule = cssRule(hoverTabSource, '.hover-tab-host.inset .hover-tab-action');
  assert.match(hostRule, /width: 40px;/);
  assert.match(hostRule, /height: 40px;/);
  assert.match(hostRule, /overflow: hidden;/);
  assert.match(pillRule, /width: 40px;/);
  assert.match(pillRule, /max-width: 40px;/);
  assert.match(pillRule, /height: 40px;/);
  assert.match(pillRule, /border-radius: 0 12px 12px 0;/);
  assert.match(insetPillRule, /border-radius: 12px 0 0 12px;/);
  assert.match(buttonRule, /width: 40px;/);
  assert.match(buttonRule, /height: 40px;/);
  assert.match(buttonRule, /border-radius: 0 10px 10px 0;/);
  assert.match(insetButtonRule, /border-radius: 10px 0 0 10px;/);
});
