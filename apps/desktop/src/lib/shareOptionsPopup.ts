import { LogicalPosition } from '@tauri-apps/api/dpi';
import { CheckMenuItem, Menu, MenuItem, PredefinedMenuItem } from '@tauri-apps/api/menu';
import { getCurrentWindow } from '@tauri-apps/api/window';
import type { ShareOptionsMenuEntry, ControlMode, HoverTabPosition } from '$lib/data/shareOptionsMenu';
import type { SharePriority } from '$lib/ipc';

export interface ShareOptionsMenuPopupOptions {
  position?: LogicalPosition;
  window?: ReturnType<typeof getCurrentWindow>;
}

export interface ShareOptionsMenuActions {
  onPriority(value: SharePriority): void;
  onControlMode(value: ControlMode): void;
  onDraw(active: boolean): void;
  onAiChat(): void;
  onDebug(): void;
  onPosition?(value: HoverTabPosition): void;
  /** Flip the per-share remote-control lock. `allowed` is the NEW value. */
  onRemoteControlAllowed?(allowed: boolean): void;
}

/**
 * Return the real callback for one declarative entry. Disabled entries return
 * no callback as an additional guard beyond the native enabled flag; tests can
 * execute this seam without creating OS menu resources.
 */
export function dispatchShareOptionsMenuEntry(
  entry: ShareOptionsMenuEntry,
  actions: ShareOptionsMenuActions
): (() => void) | undefined {
  switch (entry.kind) {
    case 'priority':
      return () => actions.onPriority(entry.value);
    case 'position':
      return actions.onPosition ? () => actions.onPosition?.(entry.value) : undefined;
    case 'control-mode':
      return entry.enabled ? () => actions.onControlMode(entry.value) : undefined;
    case 'annotation':
      return entry.enabled ? () => actions.onDraw(!entry.checked) : undefined;
    case 'ai-chat':
      return entry.enabled ? () => actions.onAiChat() : undefined;
    case 'remote-control-allowed':
      return entry.enabled && actions.onRemoteControlAllowed
        ? () => actions.onRemoteControlAllowed?.(!entry.checked)
        : undefined;
    case 'debug':
      return () => actions.onDebug();
    case 'section-label':
    case 'separator':
      return undefined;
  }
}

/**
 * Build, show, and deterministically dispose one native share-options menu.
 * Callers own target-specific state and callbacks; this helper owns only the
 * Tauri menu resource lifecycle so Petal View and the hover tab cannot drift.
 */
export async function popupShareOptionsMenu(
  entries: readonly ShareOptionsMenuEntry[],
  actions: ShareOptionsMenuActions,
  options?: ShareOptionsMenuPopupOptions
): Promise<void> {
  const items = await Promise.all(
    entries.map((entry) => {
      switch (entry.kind) {
        case 'section-label':
          return MenuItem.new({ text: entry.text, enabled: false });
        case 'priority':
          return CheckMenuItem.new({
            id: entry.id,
            text: entry.text,
            checked: entry.checked,
            action: dispatchShareOptionsMenuEntry(entry, actions)
          });
        case 'position':
          return CheckMenuItem.new({
            id: entry.id,
            text: entry.text,
            checked: entry.checked,
            action: dispatchShareOptionsMenuEntry(entry, actions)
          });
        case 'control-mode':
          return CheckMenuItem.new({
            id: entry.id,
            text: entry.text,
            enabled: entry.enabled,
            checked: entry.checked,
            action: dispatchShareOptionsMenuEntry(entry, actions)
          });
        case 'annotation':
          return CheckMenuItem.new({
            id: entry.id,
            text: entry.text,
            enabled: entry.enabled,
            checked: entry.checked,
            action: dispatchShareOptionsMenuEntry(entry, actions)
          });
        case 'ai-chat':
          return CheckMenuItem.new({
            id: entry.id,
            text: entry.text,
            enabled: entry.enabled,
            checked: entry.checked,
            action: dispatchShareOptionsMenuEntry(entry, actions)
          });
        case 'remote-control-allowed':
          return CheckMenuItem.new({
            id: entry.id,
            text: entry.text,
            enabled: entry.enabled,
            checked: entry.checked,
            action: dispatchShareOptionsMenuEntry(entry, actions)
          });
        case 'separator':
          return PredefinedMenuItem.new({ item: 'Separator' });
        case 'debug':
          return MenuItem.new({
            id: entry.id,
            text: entry.text,
            action: dispatchShareOptionsMenuEntry(entry, actions)
          });
      }
    })
  );
  try {
    const menu = await Menu.new({ items });
    try {
      if (options) {
        await menu.popup(options.position, options.window);
      } else {
        // Preserve Tauri's pointer-relative default for mouse context menus.
        await menu.popup();
      }
    } finally {
      await menu.close();
    }
  } finally {
    await Promise.all(items.map((item) => item.close()));
  }
}
