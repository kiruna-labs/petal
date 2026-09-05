// Generalized share-options vocabulary. Petal View and the hover tab share
// priority/control/draw/AI entries, while only the hover tab opts into the
// placement section.
import {
  buildHoverTabMenuEntries,
  CONTROL_MODE_CHOICES,
  CONTROL_MODE_SECTION_LABEL,
  DEBUG_MENU_ITEM_ID,
  DEBUG_MENU_ITEM_LABEL,
  HOVER_TAB_POSITION_CHOICES,
  HOVER_TAB_POSITION_SECTION_LABEL,
  positionMenuItemId,
  priorityMenuItemId,
  QUALITY_PRIORITY_CHOICES,
  QUALITY_PRIORITY_SECTION_LABEL,
  controlModeMenuItemId
} from './hoverTabMenu.ts';
import type {
  ControlMode,
  HoverTabMenuEntry,
  HoverTabPosition
} from './hoverTabMenu.ts';
import type { SharePriority } from '../ipc.ts';

export {
  CONTROL_MODE_CHOICES,
  CONTROL_MODE_SECTION_LABEL,
  DEBUG_MENU_ITEM_ID,
  DEBUG_MENU_ITEM_LABEL,
  HOVER_TAB_POSITION_CHOICES,
  HOVER_TAB_POSITION_SECTION_LABEL,
  positionMenuItemId,
  priorityMenuItemId,
  QUALITY_PRIORITY_CHOICES,
  QUALITY_PRIORITY_SECTION_LABEL,
  controlModeMenuItemId
};
export type { ControlMode, HoverTabPosition };
export type { HoverTabMenuEntry as ShareOptionsMenuEntry } from './hoverTabMenu.ts';

export function buildShareOptionsMenuEntries(
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
  return buildHoverTabMenuEntries(
    currentPriority,
    shared,
    drawActive,
    controlMode,
    remoteControlSupported,
    aiChatEnabled,
    aiChatActive,
    displayLike,
    includePosition,
    verticalOffset,
    remoteControlAllowed
  );
}
