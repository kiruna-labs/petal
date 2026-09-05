import type { SharePriority } from '../ipc.ts';
import {
  AI_CHAT_MENU_ITEM_ID,
  AI_CHAT_MENU_ITEM_LABEL,
  AI_CHAT_STOP_MENU_ITEM_LABEL
} from './aiChat.ts';

// Declarative vocabulary for the hover-tab native options menu. This module
// stays free of Tauri dependencies so menu construction and dispatch rules are
// directly unit-testable. Petal View uses the same share options without the
// hover-only placement section.

export const QUALITY_PRIORITY_CHOICES: ReadonlyArray<{ value: SharePriority; label: string }> = [
  { value: 'automatic', label: 'Automatic (recommended)' },
  { value: 'responsive', label: 'Responsive: smoother control' },
  { value: 'sharpText', label: 'Sharp text: preserve detail' },
  { value: 'dataSaver', label: 'Data saver: 15 fps, slower control' }
];

export const QUALITY_PRIORITY_SECTION_LABEL = 'Screen sharing priority';

export const HOVER_TAB_POSITION_SECTION_LABEL = 'Hover tab position';
export const HOVER_TAB_POSITION_CHOICES: ReadonlyArray<{
  value: HoverTabPosition;
  label: string;
  offset: number;
}> = [
  { value: 'top', label: 'Top', offset: 0 },
  { value: 'center', label: 'Center', offset: 0.5 },
  { value: 'bottom', label: 'Bottom', offset: 1 }
];

export type HoverTabPosition = 'top' | 'center' | 'bottom';

export const CONTROL_MODE_SECTION_LABEL = 'Remote control';

export type ControlMode = 'cursorPreserving' | 'fullControl';

export const CONTROL_MODE_CHOICES: ReadonlyArray<{ value: ControlMode; label: string }> = [
  { value: 'cursorPreserving', label: 'Cursor-preserving (default)' },
  { value: 'fullControl', label: 'Full control' }
];

export const DEBUG_MENU_ITEM_ID = 'open-network-cockpit';
export const DEBUG_MENU_ITEM_LABEL = 'Debug';

export type HoverTabMenuEntry =
  | { kind: 'section-label'; text: string }
  | { kind: 'priority'; id: string; text: string; value: SharePriority; checked: boolean }
  | { kind: 'separator' }
  | {
      kind: 'position';
      id: string;
      text: string;
      value: HoverTabPosition;
      checked: boolean;
    }
  | {
      kind: 'control-mode';
      id: string;
      text: string;
      value: ControlMode;
      checked: boolean;
      enabled: boolean;
    }
  | { kind: 'debug'; id: string; text: string }
  | { kind: 'annotation'; id: string; text: string; enabled: boolean; checked: boolean }
  | { kind: 'ai-chat'; id: string; text: string; enabled: boolean; checked: boolean }
  | { kind: 'remote-control-allowed'; id: string; text: string; enabled: boolean; checked: boolean };

export const REMOTE_CONTROL_ALLOWED_MENU_ITEM_ID = 'share-remote-control-allowed';
export const REMOTE_CONTROL_ALLOWED_MENU_ITEM_LABEL = 'Allow remote control';

export function priorityMenuItemId(value: SharePriority): string {
  return `share-priority-${value}`;
}

export function positionMenuItemId(value: HoverTabPosition): string {
  return `hover-tab-position-${value}`;
}

export function controlModeMenuItemId(value: ControlMode): string {
  return `control-mode-${value}`;
}

function positionIsChecked(position: HoverTabPosition, offset: number): boolean {
  const choice = HOVER_TAB_POSITION_CHOICES.find(({ value }) => value === position);
  return choice !== undefined && Math.abs(offset - choice.offset) < 0.001;
}

export function buildHoverTabMenuEntries(
  currentPriority: SharePriority,
  shared = false,
  drawActive = false,
  controlMode: ControlMode = 'cursorPreserving',
  remoteControlSupported = true,
  aiChatEnabled = false,
  aiChatActive = false,
  displayLike = false,
  includePosition = false,
  verticalOffset = 0.5,
  remoteControlAllowed = true
): HoverTabMenuEntry[] {
  const entries: HoverTabMenuEntry[] = [
    { kind: 'section-label', text: QUALITY_PRIORITY_SECTION_LABEL },
    ...QUALITY_PRIORITY_CHOICES.map(
      ({ value, label }): HoverTabMenuEntry => ({
        kind: 'priority',
        id: priorityMenuItemId(value),
        text: label,
        value,
        checked: currentPriority === value
      })
    )
  ];

  if (includePosition) {
    entries.push(
      { kind: 'separator' },
      { kind: 'section-label', text: HOVER_TAB_POSITION_SECTION_LABEL },
      ...HOVER_TAB_POSITION_CHOICES.map(
        ({ value, label }): HoverTabMenuEntry => ({
          kind: 'position',
          id: positionMenuItemId(value),
          text: label,
          value,
          checked: positionIsChecked(value, verticalOffset)
        })
      )
    );
  }

  // Remote-control modes are a WINDOWS-only feature. Omit the section on
  // unsupported platforms rather than showing choices that fail at the
  // command layer.
  if (remoteControlSupported) {
    entries.push(
      { kind: 'separator' },
      { kind: 'section-label', text: CONTROL_MODE_SECTION_LABEL },
      ...(displayLike
        ? CONTROL_MODE_CHOICES.filter(({ value }) => value === 'fullControl')
        : CONTROL_MODE_CHOICES
      ).map(
        ({ value, label }): HoverTabMenuEntry => ({
          kind: 'control-mode',
          id: controlModeMenuItemId(value),
          text: label,
          value,
          checked: controlMode === value,
          // Control mode is a sharer decision for an active share, so it is
          // only actionable on a shared window.
          enabled: shared
        })
      )
    );
  }
  entries.push(
    { kind: 'separator' },
    {
      // Deliberately NOT behind `remoteControlSupported`: that flag gates the
      // Windows-only control MODES, whereas permission applies on every
      // platform. Only actionable while actually sharing -- there is no
      // per-share lock to flip for a window we are not sharing.
      kind: 'remote-control-allowed',
      id: REMOTE_CONTROL_ALLOWED_MENU_ITEM_ID,
      text: REMOTE_CONTROL_ALLOWED_MENU_ITEM_LABEL,
      enabled: shared,
      checked: remoteControlAllowed
    },
    { kind: 'separator' },
    { kind: 'debug', id: DEBUG_MENU_ITEM_ID, text: DEBUG_MENU_ITEM_LABEL },
    {
      kind: 'annotation',
      id: 'draw-on-shared-window',
      text: drawActive ? 'Stop drawing on this window' : 'Draw on this shared window',
      enabled: shared,
      checked: drawActive
    }
  );
  if (aiChatEnabled && shared) {
    entries.push({
      kind: 'ai-chat',
      id: AI_CHAT_MENU_ITEM_ID,
      text: aiChatActive ? AI_CHAT_STOP_MENU_ITEM_LABEL : AI_CHAT_MENU_ITEM_LABEL,
      enabled: true,
      checked: aiChatActive
    });
  }
  return entries;
}
