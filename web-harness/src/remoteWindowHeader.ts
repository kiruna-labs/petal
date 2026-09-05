import type { LocalVideoTrack, RemoteTrack } from 'livekit-client';
import type { HarnessContext } from './context.ts';
import {
  deriveRemoteWindowStats,
  formatRemoteWindowDebugStats,
  formatRemoteWindowFreshness,
  sparkPath,
  type RemoteWindowPlaybackQuality,
  type RemoteWindowStatsState,
} from './remoteWindowStats.ts';
import { identityHeaderCss } from './telepointer.ts';
import { identityPaletteIndexFromMetadata, type CaptureStateReport } from './trackNames.ts';
import { createAiChatPanel, type AiChatPanelController } from './aiChatPanel.ts';
import { aiChatEndReasonMessage, isNormalAiChatEnd, type AiChatSessionState } from './aiChat.ts';
import { debugHeaderControlVisible } from '@petal/shared/logic/debugHeaderVisibility';
import { sparkleIconSvg } from '@petal/shared/ui/icons';
import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';

type ShareVideoTrack = RemoteTrack | LocalVideoTrack;
type HeaderMode = 'view' | 'control' | 'draw';

export interface RemoteWindowHeaderOptions {
  ctx: HarnessContext;
  tile: HTMLDivElement;
  ownerIdentity: string;
  ownerName: string;
  isLocal: boolean;
  track: ShareVideoTrack;
  video: HTMLVideoElement;
  windowId: number | null;
  sourceTitle?: string | null;
  sourceUrl?: string | null;
  autoHide?: boolean;
  onOpenSourceUrl?: (sourceUrl: string) => void;
  onMinimizeWindow?: () => void;
  onExpandWindow?: () => void;
}

export interface RemoteWindowHeaderController {
  update: (options: RemoteWindowHeaderOptions) => void;
  syncMode: () => void;
  stopDebugStats: () => void;
  destroy: () => void;
}

type StatsTrack = ShareVideoTrack & {
  getRTCStatsReport?: () => Promise<RTCStatsReport | undefined>;
  receiver?: RTCRtpReceiver | null;
};

type PlaybackQualityVideo = HTMLVideoElement & {
  getVideoPlaybackQuality?: () => { totalVideoFrames?: number | null };
};

const STATS_POLL_MS = 1000;
const FPS_HISTORY_CAP = 60;
const VALID_WINDOW_ID_MAX = 0xffff_ffff;
const IDLE_DELAY_MS = 1800;
const REQUESTING_CONTROL_FEEDBACK_MS = 420;

const RAW_WINDOW_TRACK_RE = /^petal-window(?:-\d+|-capture)?$/i;
const RAW_CAMERA_TRACK_RE = /^petal-camera(?:-.+)?$/i;
const REMOTE_CONTROL_STATUS_DATA_ATTRS = [
  'class',
  'data-window-id',
  'data-remote-control-status',
  'data-remote-control-status-message',
  'data-remote-control-status-seq',
];

const BUG_ICON = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m8 2 1.88 1.88"></path><path d="M14.12 3.88 16 2"></path><path d="M9 7.13v-1a3.003 3.003 0 1 1 6 0v1"></path><path d="M12 20c-3.3 0-6-2.7-6-6v-3a4 4 0 0 1 4-4h4a4 4 0 0 1 4 4v3c0 3.3-2.7 6-6 6Z"></path><path d="M12 20v-9"></path><path d="M6.53 9C4.6 8.8 3 7.1 3 5"></path><path d="M6 13H2"></path><path d="M3 21c0-2.1 1.7-3.9 3.8-4"></path><path d="M20.97 5c0 2.1-1.6 3.8-3.5 4"></path><path d="M22 13h-4"></path><path d="M17.2 17c2.1.1 3.8 1.9 3.8 4"></path></svg>';
const OPEN_URL_ICON = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M7 7h10v10"></path><path d="M7 17 17 7"></path></svg>';
const VIEW_ICON = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7Z"></path><circle cx="12" cy="12" r="3"></circle></svg>';
const CONTROL_ICON = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m3 3 7.07 16.97 2.51-7.39 7.39-2.51L3 3Z"></path><path d="m13 13 6 6"></path></svg>';
const DRAW_ICON = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 20h9"></path><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"></path></svg>';
// #847: sparkle, not the old chat-bubble glyph -- distinguishes an AI
// session from ordinary chat/messaging affordances at a glance. Mirrors
// apps/desktop/src/lib/components/RemoteWindowHeader.svelte's identical swap.
const AI_CHAT_ICON = sparkleIconSvg(15);
const KEBAB_ICON = '<svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><circle cx="5" cy="12" r="1.8"></circle><circle cx="12" cy="12" r="1.8"></circle><circle cx="19" cy="12" r="1.8"></circle></svg>';
const HIDE_ICON = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 12h14"></path></svg>';
const FIT_ICON = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 3H3v5"></path><path d="m3 3 6 6"></path><path d="M16 21h5v-5"></path><path d="m21 21-6-6"></path><path d="M21 8V3h-5"></path><path d="m21 3-6 6"></path><path d="M3 16v5h5"></path><path d="m3 21 6-6"></path></svg>';

function validWindowId(windowId: number | null): windowId is number {
  return Number.isSafeInteger(windowId) && windowId !== null && windowId >= 1 && windowId <= VALID_WINDOW_ID_MAX;
}

function stopControlEvent(event: Event) {
  event.preventDefault();
  event.stopPropagation();
}

function isRawRemoteTrackName(label: string): boolean {
  return RAW_WINDOW_TRACK_RE.test(label) || RAW_CAMERA_TRACK_RE.test(label);
}

function sourceLabelFor(sourceTitle: string): string {
  const trimmed = sourceTitle.trim();
  if (!trimmed || RAW_WINDOW_TRACK_RE.test(trimmed)) return 'Shared window';
  if (RAW_CAMERA_TRACK_RE.test(trimmed)) return 'Camera';
  const parts = trimmed.split(/\s+—\s*/).map((part) => part.trim());
  if (parts.length > 1) {
    const appName = parts[parts.length - 1];
    if (appName && !isRawRemoteTrackName(appName)) return appName;
    const windowTitle = parts.find((part) => part && !isRawRemoteTrackName(part));
    if (windowTitle) return windowTitle;
    if (parts.some((part) => RAW_CAMERA_TRACK_RE.test(part))) return 'Camera';
    if (parts.some((part) => RAW_WINDOW_TRACK_RE.test(part))) return 'Shared window';
  }
  return trimmed;
}

function sourceTitleFor(options: RemoteWindowHeaderOptions): string {
  const explicit = options.sourceTitle?.trim();
  if (explicit) return explicit;
  const trackLabel = options.track.mediaStreamTrack.label?.trim();
  if (trackLabel) return trackLabel;
  return validWindowId(options.windowId) ? `Window ${options.windowId}` : 'Shared window';
}

function ownerLabelFor(options: RemoteWindowHeaderOptions): string {
  return options.ownerName.trim() || 'Someone';
}

function remoteControlFeedbackLabel(status: string | null | undefined): string | null {
  switch (status) {
    case 'awaitingConsent':
      return 'Waiting for approval';
    case 'denied':
      return 'Control denied';
    case 'accessibilityDenied':
      return 'Needs access';
    case 'disabled':
      return 'Disabled';
    case 'targetPaused':
      return 'Paused';
    case 'targetUnavailable':
    case 'requestUnavailable':
      return 'Unavailable';
    case 'requestFailed':
      return 'Input ignored';
    case 'textTruncated':
      return 'Text capped';
    case 'notForeground':
      return 'Not foreground';
    case 'occluded':
      return 'Covered';
    case 'integrityBlocked':
      return 'Blocked';
    case 'secureField':
      return 'Secure field';
    case 'unsupportedRoute':
      return 'Unsupported';
    case 'staleShareInstance':
      return 'Share changed';
    case 'injectionTimeout':
      return 'Timed out';
    default:
      return status ? 'Input ignored' : null;
  }
}

function clearElement(element: HTMLElement) {
  while (element.firstChild) element.firstChild.remove();
}

function appendElement<K extends keyof HTMLElementTagNameMap>(
  parent: HTMLElement,
  tagName: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tagName);
  element.className = className;
  parent.appendChild(element);
  return element;
}

function appendText(parent: HTMLElement, className: string, text: string): HTMLSpanElement {
  const element = appendElement(parent, 'span', className);
  element.textContent = text;
  return element;
}

function makeIconButton(className: string, label: string, icon: string): HTMLButtonElement {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = className;
  button.title = label;
  button.setAttribute('aria-label', label);
  const iconElement = appendElement(button, 'span', 'remote-window-header__icon');
  iconElement.setAttribute('aria-hidden', 'true');
  iconElement.innerHTML = icon;
  button.addEventListener('pointerdown', (event) => event.stopPropagation());
  button.addEventListener('wheel', (event) => event.stopPropagation());
  button.addEventListener('keydown', (event) => event.stopPropagation());
  button.addEventListener('keyup', (event) => event.stopPropagation());
  return button;
}

function makeLabeledButton(
  className: string,
  label: string,
  title: string,
  icon: string,
): HTMLButtonElement {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = className;
  button.title = title;
  button.setAttribute('aria-label', title);
  const iconElement = appendElement(button, 'span', 'remote-window-header__icon');
  iconElement.setAttribute('aria-hidden', 'true');
  iconElement.innerHTML = icon;
  appendText(button, 'remote-window-header__button-label', label);
  button.addEventListener('pointerdown', (event) => event.stopPropagation());
  button.addEventListener('wheel', (event) => event.stopPropagation());
  button.addEventListener('keydown', (event) => event.stopPropagation());
  button.addEventListener('keyup', (event) => event.stopPropagation());
  return button;
}

function setButtonPressed(button: HTMLButtonElement, active: boolean) {
  button.classList.toggle('is-active', active);
  button.setAttribute('aria-pressed', active ? 'true' : 'false');
}

function setButtonDisabled(button: HTMLButtonElement, disabled: boolean) {
  button.disabled = disabled;
  button.setAttribute('aria-disabled', disabled ? 'true' : 'false');
}

function openSourceUrl(url: string) {
  const opener = globalThis.window?.open;
  if (typeof opener === 'function') opener.call(globalThis.window, url, '_blank', 'noopener,noreferrer');
}

async function getStatsReport(track: ShareVideoTrack): Promise<RTCStatsReport | null> {
  const statsTrack = track as StatsTrack;
  if (typeof statsTrack.getRTCStatsReport === 'function') {
    const report = await statsTrack.getRTCStatsReport();
    if (report) return report;
  }
  if (typeof statsTrack.receiver?.getStats === 'function') {
    return statsTrack.receiver.getStats();
  }
  return null;
}

/**
 * The sender's own report of whether the shared window's content is
 * currently changing, delivered cross-peer over the existing
 * `petal.pipeline-stats` data channel (native already sends this for the
 * NetworkCockpit; see apps/desktop/src-tauri/src/session/share.rs
 * `idle_refresh_frame_at` for the sender-side behavior it describes). Used
 * to label the debug overlay's FPS reading instead of guessing from the
 * number alone: while idle, the sender re-pushes the last frame once per
 * second purely to keep the track alive, so an inbound FPS of ~1 here is a
 * correct, healthy reading of that keepalive -- not a stalled/broken share.
 */
export function latestCaptureStateFor(
  ctx: HarnessContext,
  ownerIdentity: string,
  windowId: number | null
): CaptureStateReport | null {
  if (windowId === null) return null;
  const api = ctx.hook.pipelineStats;
  if (!api) return null;
  let best: { state: CaptureStateReport; receivedAt: number } | null = null;
  for (const entry of api.metrics().received) {
    const { message } = entry;
    if (message.windowId !== windowId) continue;
    if (message.reporterId !== ownerIdentity && entry.senderIdentity !== ownerIdentity) continue;
    if (!message.captureState) continue;
    if (!best || entry.receivedAt > best.receivedAt) best = { state: message.captureState, receivedAt: entry.receivedAt };
  }
  return best?.state ?? null;
}

export function isIdleKeepaliveFps(ctx: HarnessContext, ownerIdentity: string, windowId: number | null, fps: number | null): boolean {
  if (fps === null || fps <= 0) return false;
  return latestCaptureStateFor(ctx, ownerIdentity, windowId)?.state === 'idle';
}

function videoPlaybackQuality(video: HTMLVideoElement): RemoteWindowPlaybackQuality | null {
  const getVideoPlaybackQuality = (video as PlaybackQualityVideo).getVideoPlaybackQuality;
  if (typeof getVideoPlaybackQuality !== 'function') return null;
  const quality = getVideoPlaybackQuality.call(video);
  return {
    totalVideoFrames: quality.totalVideoFrames,
  };
}

export function createRemoteWindowHeader(options: RemoteWindowHeaderOptions): RemoteWindowHeaderController {
  let current = options;
  let debugActive = false;
  let requestingControl = false;
  let statsState: RemoteWindowStatsState | null = null;
  let statsTimer: ReturnType<typeof setInterval> | null = null;
  let statsRun = 0;
  let statsOverlay: HTMLDivElement | null = null;
  let fpsHistory: (number | null)[] = [];
  let idleTimer: ReturnType<typeof setTimeout> | null = null;
  let requestingTimer: ReturnType<typeof setTimeout> | null = null;
  let revealed = true;
  let freshnessTimer: ReturnType<typeof setInterval> | null = null;
  let lastFrameReceivedMs: number | null = null;
  let lastPresentedFrameCount: number | null = null;
  let aiChatPanel: AiChatPanelController | null = null;
  let unsubscribeAiChat: (() => void) | null = null;
  // destroy() still runs syncMode() on its way out (via stopDebugStats), and
  // syncMode drives the AI panel. Without this flag that final pass rebuilds
  // the panel onto a tile that is being torn down -- a phantom "AI chat live"
  // surface attached to nothing.
  let destroyed = false;

  const root = document.createElement('div');
  root.className = 'remote-window-header';
  root.setAttribute('role', 'group');
  root.setAttribute('aria-label', 'Remote window header');

  const left = appendElement(root, 'div', 'remote-window-header__left');
  const windowActions = appendElement(left, 'div', 'remote-window-header__window-actions');
  windowActions.setAttribute('role', 'group');
  windowActions.setAttribute('aria-label', 'Shared window actions');
  const sizeButton = makeIconButton(
    'control-button remote-window-header__icon-control remote-window-header__size-toggle',
    'Expand remote window',
    FIT_ICON,
  );
  windowActions.appendChild(sizeButton);

  const titleCluster = appendElement(left, 'div', 'remote-window-header__title-cluster');
  const title = appendElement(titleCluster, 'span', 'remote-window-header__title');
  const sourceLabel = appendText(title, 'remote-window-header__source-label', '');
  const ownerLabel = appendText(title, 'remote-window-header__owner-label', '');

  const right = appendElement(root, 'div', 'remote-window-header__right');
  const statusChip = appendElement(right, 'span', 'remote-window-header__status-chip');
  statusChip.setAttribute('role', 'status');

  const debugButton = makeLabeledButton(
    'remote-window-header__header-btn remote-window-header__debug',
    'Debug',
    'Show debug stats',
    BUG_ICON,
  );
  right.appendChild(debugButton);

  function syncFreshnessTooltip() {
    debugButton.title = formatRemoteWindowFreshness(lastFrameReceivedMs);
  }

  function refreshFreshness() {
    const quality = videoPlaybackQuality(current.video);
    const presented = quality?.totalVideoFrames;
    if (typeof presented === 'number' && Number.isFinite(presented)) {
      if (lastPresentedFrameCount === null || presented > lastPresentedFrameCount) {
        lastFrameReceivedMs = Date.now();
      }
      lastPresentedFrameCount = presented;
    }
    syncFreshnessTooltip();
  }

  // Deliberately polling-only (no requestVideoFrameCallback here): tiles.ts's
  // own tile-replacement tracking registers its own rVFC on this same video
  // element, and a second independent registrant shifted its callback
  // ordering assumptions (broke tilesRemoteControl.test.ts's "queued
  // callbacks from the replaced track" case). The 1s poll is precise enough
  // for a coarse live/stale/waiting tooltip.
  function startFreshnessTracking() {
    refreshFreshness();
    if (freshnessTimer === null) {
      freshnessTimer = setInterval(refreshFreshness, STATS_POLL_MS);
      // Unlike statsTimer (opt-in, only runs while debug mode is active),
      // this timer starts unconditionally on every header create/update, and
      // tiles.ts only calls destroy() on one specific removal path — so a
      // caller that never removes a tile leaves this interval running
      // forever. unref() (a no-op on a real browser's numeric timer handle)
      // stops it alone from keeping a Node process alive, which is what
      // hung the test suite after tiles were left un-destroyed.
      (freshnessTimer as unknown as { unref?: () => void })?.unref?.();
    }
  }

  const openUrlButton = makeLabeledButton(
    'remote-window-header__header-btn remote-window-header__open-url',
    'Open URL',
    'Open URL',
    OPEN_URL_ICON,
  );
  right.appendChild(openUrlButton);

  // #657 AI chat. The Gemini session always runs on the SHARER's machine, so
  // this button only ever ASKS -- the owner decides and answers with `state`.
  const aiChatButton = makeLabeledButton(
    'remote-window-header__header-btn remote-window-header__ai-chat',
    'AI chat',
    'Start AI chat on this window',
    AI_CHAT_ICON,
  );
  right.appendChild(aiChatButton);

  const switcher = appendElement(right, 'div', 'remote-window-header__mode-switcher');
  switcher.setAttribute('role', 'group');
  switcher.setAttribute('aria-label', 'Remote window mode');
  appendElement(switcher, 'span', 'remote-window-header__active-indicator').setAttribute('aria-hidden', 'true');
  const viewButton = makeLabeledButton(
    'remote-window-header__segment',
    'View',
    'View shared window',
    VIEW_ICON,
  );
  const controlButton = makeLabeledButton(
    'remote-window-header__segment',
    'Control',
    'Request remote control',
    CONTROL_ICON,
  );
  const drawButton = makeLabeledButton(
    'remote-window-header__segment',
    'Draw',
    'Draw on shared window',
    DRAW_ICON,
  );
  switcher.appendChild(viewButton);
  switcher.appendChild(controlButton);
  switcher.appendChild(drawButton);

  const overflowButton = makeIconButton(
    'remote-window-header__header-btn remote-window-header__overflow-button',
    'More remote window modes',
    KEBAB_ICON,
  );
  overflowButton.setAttribute('aria-haspopup', 'menu');
  overflowButton.setAttribute('aria-expanded', 'false');
  right.appendChild(overflowButton);

  const overflowMenu = appendElement(right, 'div', 'remote-window-header__overflow-menu');
  overflowMenu.setAttribute('role', 'menu');
  overflowMenu.setAttribute('aria-label', 'Remote window modes');
  overflowMenu.hidden = true;
  const viewMenuButton = makeLabeledButton(
    'remote-window-header__overflow-item',
    'View shared window',
    'View shared window',
    VIEW_ICON,
  );
  const controlMenuButton = makeLabeledButton(
    'remote-window-header__overflow-item',
    'Request remote control',
    'Request remote control',
    CONTROL_ICON,
  );
  const drawMenuButton = makeLabeledButton(
    'remote-window-header__overflow-item',
    'Draw on shared window',
    'Draw on shared window',
    DRAW_ICON,
  );
  for (const button of [viewMenuButton, controlMenuButton, drawMenuButton]) {
    button.setAttribute('role', 'menuitemradio');
    overflowMenu.appendChild(button);
  }

  function setOverflowMenuOpen(open: boolean) {
    overflowMenu.hidden = !open;
    overflowButton.setAttribute('aria-expanded', open ? 'true' : 'false');
    current.tile.classList.toggle('remote-window-menu-open', open);
  }

  const cleanupDismissibleLayer = installDismissibleLayer({
    isOpen: () => !overflowMenu.hidden,
    getInsideNodes: () => [overflowMenu, overflowButton],
    getPopupNodes: () => [overflowMenu],
    getOpener: () => overflowButton,
    onDismiss: () => setOverflowMenuOpen(false)
  });

  function autoHideEnabled(): boolean {
    return current.autoHide !== false;
  }

  function applyRevealState() {
    root.classList.toggle('idle', !revealed);
  }

  function scheduleIdle() {
    if (idleTimer !== null) clearTimeout(idleTimer);
    idleTimer = null;
    if (!autoHideEnabled()) return;
    idleTimer = setTimeout(() => {
      revealed = false;
      applyRevealState();
    }, IDLE_DELAY_MS);
  }

  function reveal() {
    revealed = true;
    applyRevealState();
    scheduleIdle();
  }

  function clearRequestingControl() {
    requestingControl = false;
    if (requestingTimer !== null) {
      clearTimeout(requestingTimer);
      requestingTimer = null;
    }
  }

  function beginRequestingControl() {
    requestingControl = true;
    if (requestingTimer !== null) clearTimeout(requestingTimer);
    requestingTimer = setTimeout(() => {
      requestingControl = false;
      requestingTimer = null;
      syncMode();
    }, REQUESTING_CONTROL_FEEDBACK_MS);
  }

  function controlAvailable(): boolean {
    return (
      !current.isLocal &&
      validWindowId(current.windowId) &&
      !!current.ctx.state.room &&
      typeof (current.ctx.cb as Partial<HarnessContext['cb']>).startRemoteControl === 'function' &&
      typeof (current.ctx.cb as Partial<HarnessContext['cb']>).stopRemoteControl === 'function'
    );
  }

  function drawAvailable(): boolean {
    return validWindowId(current.windowId) && !!current.ctx.state.room;
  }

  // --- AI chat (#657) -------------------------------------------------------
  // Only offered for OTHER people's shares: the Gemini session runs on the
  // machine that owns the window's pixels and accessibility tree, which this
  // browser client never is. The owner still enforces its own preconditions
  // and answers a refusal with `state.error`.
  function aiChatCallbacks(): Partial<HarnessContext['cb']> {
    return current.ctx.cb as Partial<HarnessContext['cb']>;
  }

  function aiChatAvailable(): boolean {
    const cb = aiChatCallbacks();
    return (
      !current.isLocal &&
      validWindowId(current.windowId) &&
      !!current.ctx.state?.room &&
      typeof cb.startAiChat === 'function' &&
      typeof cb.stopAiChat === 'function'
    );
  }

  function aiChatSession(): AiChatSessionState | null {
    const cb = aiChatCallbacks();
    if (!validWindowId(current.windowId) || typeof cb.aiChatSessionFor !== 'function') return null;
    return cb.aiChatSessionFor(current.windowId, current.ownerIdentity);
  }

  /**
   * Local display-name resolver. Deliberately not imported from tiles.ts:
   * tiles.ts already imports this module, and the repo breaks those cycles by
   * injection rather than cross-imports.
   */
  function displayNameForIdentity(identity: string): string {
    const room = current.ctx.state?.room;
    if (!room) return identity;
    if (room.localParticipant?.identity === identity) {
      return room.localParticipant.name?.trim() || identity;
    }
    return room.remoteParticipants?.get(identity)?.name?.trim() || identity;
  }

  function aiChatPanelOptions() {
    const cb = aiChatCallbacks();
    const windowId = validWindowId(current.windowId) ? current.windowId : 0;
    return {
      tile: current.tile,
      windowId,
      ownerIdentity: current.ownerIdentity,
      displayNameFor: displayNameForIdentity,
      localIdentity: current.ctx.state?.room?.localParticipant?.identity ?? null,
      onStop: () => cb.stopAiChat?.(windowId, current.ownerIdentity),
      onPttStart: () => cb.aiChatPttStart?.(windowId, current.ownerIdentity),
      onPttEnd: () => cb.aiChatPttEnd?.(windowId, current.ownerIdentity),
      onSendText: (text: string) => cb.aiChatSendText?.(windowId, current.ownerIdentity, text),
    };
  }

  function destroyAiChatPanel() {
    aiChatPanel?.destroy();
    aiChatPanel = null;
  }

  function syncAiChat() {
    if (destroyed) return;
    const cb = aiChatCallbacks();
    const available = aiChatAvailable();
    const session = aiChatSession();
    const active = session?.active === true;
    const error = session?.error ?? null;

    aiChatButton.classList.toggle('is-hidden', !available);
    setButtonDisabled(aiChatButton, !available);
    setButtonPressed(aiChatButton, active);
    aiChatButton.classList.toggle('is-warning', !!error && !isNormalAiChatEnd(error));
    const action = active ? 'Stop AI chat' : 'Start AI chat on this window';
    // A refusal is worth surfacing on the control itself; the panel carries
    // the same sentence, but the button is where the user just clicked.
    aiChatButton.title = !active && error ? aiChatEndReasonMessage(error) : action;
    aiChatButton.setAttribute('aria-label', action);
    const aiLabel = aiChatButton.querySelector<HTMLElement>('.remote-window-header__button-label');
    if (aiLabel) aiLabel.textContent = active ? 'Stop AI chat' : 'AI chat';

    // The panel exists exactly as long as there is session state to show --
    // and staleness expiry deletes that state, so a crashed host cannot leave
    // a phantom "AI chat live" badge behind.
    if (!session || !available) {
      destroyAiChatPanel();
      return;
    }
    const options = aiChatPanelOptions();
    const held = cb.aiChatLocalPttHeld?.(options.windowId, current.ownerIdentity) === true;
    if (!aiChatPanel) aiChatPanel = createAiChatPanel(options);
    aiChatPanel.update(options, session, held);
  }

  function toggleAiChat() {
    if (!aiChatAvailable() || !validWindowId(current.windowId)) return;
    const cb = aiChatCallbacks();
    if (aiChatSession()?.active === true) cb.stopAiChat?.(current.windowId, current.ownerIdentity);
    else cb.startAiChat?.(current.windowId, current.ownerIdentity);
    syncAiChat();
  }

  function controlActive(): boolean {
    const checker = (current.ctx.cb as Partial<HarnessContext['cb']>).activeRemoteControlForTile;
    if (typeof checker === 'function') return !!checker(current.tile);
    return current.ctx.state.activeRemoteControl?.tileId === current.tile.id && !!current.ctx.state.room;
  }

  function drawActive(): boolean {
    return current.ctx.dom.tilesEl.classList.contains('draw-mode-active');
  }

  function streamPaused(): boolean {
    return current.tile.classList.contains('stream-paused');
  }

  function statusState(): { text: string; title: string; warning: boolean; paused: boolean } | null {
    if (requestingControl) {
      return {
        text: 'Requesting control',
        title: 'Requesting control from the shared Mac',
        warning: false,
        paused: false,
      };
    }
    const feedbackLabel = remoteControlFeedbackLabel(current.tile.dataset.remoteControlStatus);
    if (feedbackLabel) {
      // Structural "not now" answers render neutral, not as a warning:
      // requestUnavailable (not shared) and awaitingConsent (sharer is
      // being asked -- consent flow). A `denied` IS a warning.
      const neutral = ['requestUnavailable', 'awaitingConsent'].includes(
        current.tile.dataset.remoteControlStatus ?? ''
      );
      const warning = !neutral;
      return {
        text: feedbackLabel,
        title: current.tile.dataset.remoteControlStatusMessage || 'Remote control is not available right now.',
        warning,
        paused: !warning,
      };
    }
    if (streamPaused()) {
      return {
        text: 'Video paused',
        title: 'Video paused',
        warning: false,
        paused: true,
      };
    }
    return null;
  }

  function activeMode(): HeaderMode {
    if (drawActive()) return 'draw';
    if (controlActive() || requestingControl) return 'control';
    return 'view';
  }

  function windowExpanded(): boolean {
    return current.tile.classList.contains('is-spotlight');
  }

  function syncMode() {
    const mode = activeMode();
    const modeIndex = mode === 'control' ? 1 : mode === 'draw' ? 2 : 0;
    const isControlActive = controlActive();
    const canControl = controlAvailable() || isControlActive;
    const canDraw = drawAvailable();
    const rawSourceTitle = sourceTitleFor(current);
    const source = sourceLabelFor(rawSourceTitle);
    const owner = ownerLabelFor(current);
    const currentStatus = statusState();
    const sourceUrl = current.sourceUrl?.trim() || '';
    const ownerIdentity = current.ownerIdentity || current.ownerName;
    const room = current.ctx.state.room;
    const ownerMetadata =
      room?.localParticipant.identity === current.ownerIdentity
        ? room.localParticipant.metadata
        : room?.remoteParticipants.get(current.ownerIdentity)?.metadata;
    const headerColor = identityHeaderCss(ownerIdentity, identityPaletteIndexFromMetadata(ownerMetadata));

    root.dataset.mode = mode;
    root.style?.setProperty('--active-mode-index', String(modeIndex));
    root.style?.setProperty('--identity-header-bg', headerColor.background);
    root.style?.setProperty('--identity-header-ink', headerColor.ink);
    root.title = `${source} by ${owner}`;
    titleCluster.title = root.title;

    sourceLabel.textContent = source;
    ownerLabel.textContent = ` by ${owner}`;

    setButtonPressed(viewButton, mode === 'view');
    setButtonPressed(controlButton, mode === 'control');
    setButtonPressed(drawButton, mode === 'draw');
    viewMenuButton.setAttribute('aria-checked', mode === 'view' ? 'true' : 'false');
    controlMenuButton.setAttribute('aria-checked', mode === 'control' ? 'true' : 'false');
    drawMenuButton.setAttribute('aria-checked', mode === 'draw' ? 'true' : 'false');
    setButtonPressed(debugButton, debugActive);

    // #376 item 2: distinguish a PERMANENT reason (you can't control your own
    // shared window) from a TRANSIENT one (this tile's control wiring --
    // windowId/room -- hasn't finished arriving yet), instead of one flat
    // "unavailable" message for both. The transient case re-enables on its
    // own the next time syncMode() runs (tiles.ts calls update() as fresh
    // metadata lands), with no user action needed -- "preparing" reflects
    // that instead of reading like a dead end.
    const preparing = !canControl && !current.isLocal;
    controlButton.classList.toggle('requesting', requestingControl);
    controlButton.classList.toggle('preparing', preparing);
    syncFreshnessTooltip();
    debugButton.setAttribute('aria-label', debugActive ? 'Hide debug stats' : 'Show debug stats');
    controlButton.title = requestingControl
      ? 'Requesting control'
      : isControlActive
        ? 'Remote control active'
        : canControl
          ? 'Request remote control'
          : current.isLocal
            ? "You can't control your own shared window"
            : 'Preparing remote control…';
    controlButton.setAttribute('aria-label', controlButton.title);
    const controlMenuTitle = isControlActive ? 'Stop remote control' : controlButton.title;
    controlMenuButton.title = controlMenuTitle;
    controlMenuButton.setAttribute('aria-label', controlMenuTitle);
    const controlMenuLabel = controlMenuButton.querySelector<HTMLElement>('.remote-window-header__button-label');
    if (controlMenuLabel) controlMenuLabel.textContent = controlMenuTitle;
    drawButton.title = drawActive() ? 'Drawing on shared window' : 'Draw on shared window';
    drawButton.setAttribute('aria-label', drawButton.title);
    drawMenuButton.title = drawActive() ? 'Stop drawing' : 'Draw on shared window';
    drawMenuButton.setAttribute('aria-label', drawMenuButton.title);
    const drawMenuLabel = drawMenuButton.querySelector<HTMLElement>('.remote-window-header__button-label');
    if (drawMenuLabel) drawMenuLabel.textContent = drawMenuButton.title;

    setButtonDisabled(viewButton, requestingControl);
    setButtonDisabled(controlButton, !canControl || requestingControl);
    setButtonDisabled(drawButton, !canDraw || requestingControl);
    setButtonDisabled(viewMenuButton, requestingControl);
    setButtonDisabled(controlMenuButton, !canControl || requestingControl);
    setButtonDisabled(drawMenuButton, !canDraw || requestingControl);
    const expanded = windowExpanded();
    const sizeAction = expanded ? 'Minimize remote window' : 'Expand remote window';
    sizeButton.title = sizeAction;
    sizeButton.setAttribute('aria-label', sizeAction);
    sizeButton.setAttribute('aria-expanded', expanded ? 'true' : 'false');
    const sizeIcon = sizeButton.querySelector<HTMLElement>('.remote-window-header__icon');
    if (sizeIcon) sizeIcon.innerHTML = expanded ? HIDE_ICON : FIT_ICON;
    setButtonDisabled(
      sizeButton,
      expanded
        ? typeof current.onMinimizeWindow !== 'function'
        : typeof current.onExpandWindow !== 'function',
    );

    statusChip.hidden = !currentStatus;
    statusChip.classList.toggle('warning', !!currentStatus?.warning);
    statusChip.classList.toggle('paused', !!currentStatus?.paused);
    statusChip.textContent = currentStatus?.text ?? '';
    statusChip.title = currentStatus?.title ?? '';

    openUrlButton.classList.toggle('is-hidden', !sourceUrl);
    setButtonDisabled(openUrlButton, !sourceUrl);

    // #669: Debug mode -- default OFF, mirrors the desktop client's
    // Rust-owned setting via the shared `debugHeaderControlVisible`
    // predicate. `aiChatLive: false` because this client has no equivalent
    // to desktop's measured "hide Debug while the AI chat live disclosure is
    // showing" rule; width suppression stays CSS-only (unchanged
    // @container/@media max-width: 640px rules in style.css), so
    // `viewportWidth` is always-satisfied here too.
    const debugVisible = debugHeaderControlVisible({
      debugModeEnabled: current.ctx.state.debugModeEnabled,
      aiChatLive: false,
      viewportWidth: Number.POSITIVE_INFINITY,
    });
    debugButton.classList.toggle('is-hidden', !debugVisible);
    setButtonDisabled(debugButton, !debugVisible);

    syncAiChat();
  }

  function ensureStatsOverlay(): HTMLDivElement {
    if (statsOverlay) return statsOverlay;
    statsOverlay = document.createElement('div');
    statsOverlay.className = 'remote-window-stats';
    statsOverlay.setAttribute('aria-live', 'polite');
    current.tile.appendChild(statsOverlay);
    return statsOverlay;
  }

  function renderStatsMessage(message: string) {
    const overlay = ensureStatsOverlay();
    clearElement(overlay);
    const line = document.createElement('div');
    line.className = 'remote-window-stats__message';
    line.textContent = message;
    overlay.appendChild(line);
  }

  function renderStatsLines(
    lines: ReturnType<typeof formatRemoteWindowDebugStats>,
    fpsGraphPath: string,
    fpsIdleKeepalive: boolean
  ) {
    const overlay = ensureStatsOverlay();
    clearElement(overlay);
    for (const stat of lines) {
      const rowElement = document.createElement('div');
      rowElement.className = stat.prominent ? 'remote-window-stats__row is-prominent' : 'remote-window-stats__row';
      const label = document.createElement('span');
      label.className = 'remote-window-stats__label';
      label.textContent = stat.label;
      const value = document.createElement('span');
      value.className = 'remote-window-stats__value';
      value.textContent = stat.label === 'FPS' && fpsIdleKeepalive ? `${stat.value} (idle keepalive)` : stat.value;
      rowElement.appendChild(label);
      rowElement.appendChild(value);
      overlay.appendChild(rowElement);
      if (stat.label === 'FPS' && fpsGraphPath) {
        const graph = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
        graph.setAttribute('class', 'remote-window-stats__spark');
        graph.setAttribute('viewBox', '0 0 120 28');
        graph.setAttribute('preserveAspectRatio', 'none');
        graph.setAttribute('aria-hidden', 'true');
        const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
        path.setAttribute('d', fpsGraphPath);
        graph.appendChild(path);
        overlay.appendChild(graph);
      }
    }
  }

  async function pollStats(run: number) {
    if (!debugActive || run !== statsRun) return;
    if ('isConnected' in current.tile && !current.tile.isConnected) {
      stopDebugStats();
      return;
    }

    let report: RTCStatsReport | null = null;
    try {
      report = await getStatsReport(current.track);
    } catch {
      report = null;
    }

    if (!debugActive || run !== statsRun) return;
    if (!report) {
      renderStatsMessage('Stats unavailable');
      return;
    }

    const derived = deriveRemoteWindowStats(report, statsState, Date.now(), {
      width: current.video.videoWidth,
      height: current.video.videoHeight,
    }, videoPlaybackQuality(current.video));
    statsState = derived.state;

    fpsHistory.push(derived.snapshot.fps);
    if (fpsHistory.length > FPS_HISTORY_CAP) fpsHistory.shift();
    const fpsIdleKeepalive = isIdleKeepaliveFps(current.ctx, current.ownerIdentity, current.windowId, derived.snapshot.fps);

    renderStatsLines(formatRemoteWindowDebugStats(derived.snapshot), sparkPath(fpsHistory), fpsIdleKeepalive);
  }

  function startDebugStats() {
    if (debugActive) return;
    debugActive = true;
    statsState = null;
    fpsHistory = [];
    statsRun += 1;
    renderStatsMessage('Stats warming up');
    void pollStats(statsRun);
    statsTimer = setInterval(() => {
      void pollStats(statsRun);
    }, STATS_POLL_MS);
    syncMode();
  }

  function stopDebugStats() {
    debugActive = false;
    statsRun += 1;
    if (statsTimer !== null) {
      clearInterval(statsTimer);
      statsTimer = null;
    }
    statsState = null;
    fpsHistory = [];
    statsOverlay?.remove();
    statsOverlay = null;
    syncMode();
  }

  function selectView() {
    if (requestingControl) return;
    if (drawActive()) current.ctx.cb.setDrawMode(false);
    if (controlActive()) current.ctx.cb.stopRemoteControl('view mode');
    clearRequestingControl();
    syncMode();
  }

  function selectControl() {
    if (requestingControl) return;
    if (controlActive()) {
      clearRequestingControl();
      current.ctx.cb.stopRemoteControl('manual');
      syncMode();
      return;
    }
    if (!controlAvailable()) return;
    if (drawActive()) current.ctx.cb.setDrawMode(false);
    beginRequestingControl();
    current.ctx.cb.startRemoteControl(current.tile);
    syncMode();
  }

  function selectDraw() {
    if (requestingControl || !drawAvailable()) return;
    if (controlActive()) {
      clearRequestingControl();
      current.ctx.cb.stopRemoteControl('draw mode');
    }
    current.ctx.cb.setDrawMode(!drawActive());
    syncMode();
  }

  viewButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    selectView();
  });
  controlButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    selectControl();
  });
  drawButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    selectDraw();
  });
  overflowButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    setOverflowMenuOpen(Boolean(overflowMenu.hidden));
  });
  viewMenuButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    selectView();
    setOverflowMenuOpen(false);
  });
  controlMenuButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    selectControl();
    setOverflowMenuOpen(false);
  });
  drawMenuButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    selectDraw();
    setOverflowMenuOpen(false);
  });
  root.addEventListener('keydown', (event) => {
    if ((event as KeyboardEvent).key === 'Escape') setOverflowMenuOpen(false);
  });
  aiChatButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    toggleAiChat();
  });
  debugButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    if (debugActive) stopDebugStats();
    else startDebugStats();
  });
  openUrlButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    const sourceUrl = current.sourceUrl?.trim();
    if (!sourceUrl) return;
    if (current.onOpenSourceUrl) current.onOpenSourceUrl(sourceUrl);
    else openSourceUrl(sourceUrl);
  });
  sizeButton.addEventListener('click', (event) => {
    stopControlEvent(event);
    if (windowExpanded()) current.onMinimizeWindow?.();
    else current.onExpandWindow?.();
    // Both callbacks update the tile's authoritative layout classes. Read
    // those back instead of assuming the requested transition succeeded.
    syncMode();
  });
  root.addEventListener('pointerenter', reveal);
  root.addEventListener('pointermove', reveal);
  root.addEventListener('focusin', reveal);

  let observer: MutationObserver | null = null;
  if (typeof MutationObserver !== 'undefined') {
    observer = new MutationObserver(() => syncMode());
    observer.observe(current.tile, { attributes: true, attributeFilter: REMOTE_CONTROL_STATUS_DATA_ATTRS });
    observer.observe(current.ctx.dom.tilesEl, { attributes: true, attributeFilter: ['class'] });
  }

  current.tile.appendChild(root);
  applyRevealState();
  // Session state arrives on the data channel, not through a tile mutation, so
  // the header has to be told when it changed. Without this the badge and the
  // transcript would only refresh on some unrelated re-render.
  {
    const subscribe = (current.ctx.cb as Partial<HarnessContext['cb']>).onAiChatChange;
    if (typeof subscribe === 'function') unsubscribeAiChat = subscribe(() => syncAiChat());
  }
  syncMode();
  startFreshnessTracking();
  scheduleIdle();

  return {
    update(nextOptions) {
      current = nextOptions;
      if (!autoHideEnabled()) {
        revealed = true;
        applyRevealState();
      }
      syncMode();
      startFreshnessTracking();
      scheduleIdle();
      if (debugActive) void pollStats(statsRun);
    },
    syncMode,
    stopDebugStats,
    destroy() {
      destroyed = true;
      observer?.disconnect();
      cleanupDismissibleLayer();
      clearRequestingControl();
      if (idleTimer !== null) clearTimeout(idleTimer);
      idleTimer = null;
      if (freshnessTimer !== null) clearInterval(freshnessTimer);
      freshnessTimer = null;
      unsubscribeAiChat?.();
      unsubscribeAiChat = null;
      // Tearing the tile down must not strand a held push-to-talk floor. The
      // panel releases on destroy; this second call covers the case where the
      // panel was never created (or already gone) while the floor was held --
      // scoped to THIS window so it cannot cut short another tile's turn.
      destroyAiChatPanel();
      if (validWindowId(current.windowId)) {
        (current.ctx.cb as Partial<HarnessContext['cb']>).aiChatPttEnd?.(
          current.windowId,
          current.ownerIdentity,
        );
      }
      stopDebugStats();
      setOverflowMenuOpen(false);
      root.remove();
    },
  };
}
