import type { HarnessContext, RemoteTelepointerState } from './context.ts';
import { TELEPOINTER_TOPIC, identityPaletteIndexFromMetadata, type TelepointerMessage } from './trackNames.ts';
import {
  colorForIdentity,
  mediaContentRectRelativeToTile,
  parseTelepointerPayload,
  telepointerKey,
  telepointerPosition,
} from './telepointer.ts';
import { participantDisplayName } from './tiles.ts';

// ---------------------------------------------------------------------------
// Remote telepointer rendering: the per-window overlay that draws other
// participants' cursors (and their click/type activity trails + handshake
// bursts) on top of the share tile for their window.
// ---------------------------------------------------------------------------
const TELEPOINTER_STALE_MS = 900;
const TELEPOINTER_REMOVE_MS = 2600;
const TELEPOINTER_ACTIVITY_TRAIL_MS = 1500;
const HANDSHAKE_WINDOW_MS = 1000;
const HANDSHAKE_DISTANCE = 0.06;
const HANDSHAKE_COOLDOWN_MS = 3000;

function nameFromTileChip(tile: HTMLElement): string | null {
  const label = Array.from(tile.querySelectorAll<HTMLSpanElement>('.name-chip span')).find(
    (span) => !span.classList.contains('audio-dot') && !span.classList.contains('tag')
  );
  return label?.textContent?.replace(/\s+\(you\)$/, '').trim() || null;
}

export function telepointerLabelForIdentity(identity: string, tileLabel?: string | null): string {
  return participantDisplayName(identity, tileLabel);
}

export function telepointerMessageFromAuthenticatedSender(
  message: TelepointerMessage,
  senderIdentity?: string
): TelepointerMessage | null {
  const trustedIdentity = senderIdentity?.trim();
  if (!trustedIdentity) return null;
  return { ...message, userId: trustedIdentity };
}

export function setupTelepointerDisplay(ctx: HarnessContext) {
  const { remoteTelepointers, handshakeCooldowns, state } = ctx;

  function shareTileForWindowId(windowId: number): HTMLDivElement | null {
    return document.querySelector<HTMLDivElement>(`.share-tile[data-window-id="${windowId}"]`);
  }

  function ensureTelepointerLayer(tile: HTMLDivElement): HTMLDivElement {
    let layer = tile.querySelector<HTMLDivElement>('.telepointer-layer');
    if (!layer) {
      layer = document.createElement('div');
      layer.className = 'telepointer-layer';
      tile.appendChild(layer);
    }
    return layer;
  }

  function makeTelepointerElement(message: TelepointerMessage): HTMLDivElement {
    const pointer = document.createElement('div');
    pointer.className = 'remote-telepointer';
    pointer.dataset.pointerKey = telepointerKey(message);
    pointer.style.setProperty('--pointer-color', colorForIdentity(message.userId, paletteIndexForIdentity(message.userId)));

    const arrow = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    arrow.classList.add('remote-telepointer__arrow');
    arrow.setAttribute('width', '22');
    arrow.setAttribute('height', '22');
    arrow.setAttribute('viewBox', '0 0 24 24');
    arrow.setAttribute('aria-hidden', 'true');
    const fill = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    fill.classList.add('remote-telepointer__arrow-fill');
    fill.setAttribute('d', 'M5 3l5 16 2.5-6.5L19 10z');
    arrow.append(fill);
    pointer.appendChild(arrow);

    const label = document.createElement('div');
    label.className = 'remote-telepointer__label';
    label.textContent = labelForTelepointer(message.userId);
    pointer.appendChild(label);

    const typing = document.createElement('div');
    typing.className = 'remote-telepointer__typing';
    typing.setAttribute('aria-hidden', 'true');
    typing.append(document.createElement('span'), document.createElement('span'), document.createElement('span'));
    pointer.appendChild(typing);

    return pointer;
  }

  function positionTelepointer(pointer: HTMLDivElement, tile: HTMLDivElement, message: TelepointerMessage) {
    // #892: bounds must be the video's content box relative to the tile, not
    // the bare tile -- a native sharer's cursor rendered ~22px high in every
    // header-bearing web tile (same root cause as the draw-offset bug).
    const { bounds, media } = mediaContentRectRelativeToTile(tile);
    const point = telepointerPosition(bounds, media, { x: message.x, y: message.y });
    pointer.style.transform = `translate3d(${point.x.toFixed(1)}px, ${point.y.toFixed(1)}px, 0)`;
  }

  function labelForTelepointer(identity: string): string {
    const participant = state.room?.remoteParticipants.get(identity);
    if (participant) return participantDisplayName(identity, participant.name);

    let tileLabel: string | null = null;
    document.querySelectorAll<HTMLElement>('.tile').forEach((tile) => {
      if (tileLabel || tile.dataset.owner !== identity) return;
      tileLabel = nameFromTileChip(tile);
    });
    return telepointerLabelForIdentity(identity, tileLabel);
  }

  function paletteIndexForIdentity(identity: string): number | null {
    const room = state.room;
    if (!room) return null;
    if (room.localParticipant.identity === identity) {
      return identityPaletteIndexFromMetadata(room.localParticipant.metadata);
    }
    return identityPaletteIndexFromMetadata(room.remoteParticipants.get(identity)?.metadata);
  }

  function removeRemoteTelepointer(key: string) {
    const state = remoteTelepointers.get(key);
    if (!state) return;
    if (state.staleTimer) clearTimeout(state.staleTimer);
    if (state.removeTimer) clearTimeout(state.removeTimer);
    if (state.activityTimer) clearTimeout(state.activityTimer);
    state.element?.remove();
    remoteTelepointers.delete(key);
  }

  function hideRemoteTelepointer(key: string) {
    const state = remoteTelepointers.get(key);
    if (!state) return;
    if (state.staleTimer) clearTimeout(state.staleTimer);
    if (state.removeTimer) clearTimeout(state.removeTimer);
    state.element?.classList.add('is-hidden');
    state.removeTimer = setTimeout(() => removeRemoteTelepointer(key), 180);
  }

  function renderRemoteTelepointer(state: RemoteTelepointerState) {
    const tile = shareTileForWindowId(state.message.windowId);
    if (!tile) return;
    const layer = ensureTelepointerLayer(tile);
    if (!state.element || state.element.parentElement !== layer) {
      state.element?.remove();
      state.element = makeTelepointerElement(state.message);
      layer.appendChild(state.element);
    }
    state.element.classList.remove('is-hidden', 'is-stale');
    state.element.style.setProperty('--pointer-color', colorForIdentity(state.message.userId, paletteIndexForIdentity(state.message.userId)));
    state.element.querySelector<HTMLDivElement>('.remote-telepointer__label')!.textContent = labelForTelepointer(
      state.message.userId
    );
    positionTelepointer(state.element, tile, state.message);
  }

  function triggerRemoteTelepointerPulse(state: RemoteTelepointerState) {
    state.pulseKey += 1;
    const element = state.element;
    if (!element) return;
    element.querySelector('.remote-telepointer__ripple')?.remove();
    const ripple = document.createElement('span');
    ripple.className = 'remote-telepointer__ripple';
    ripple.dataset.pulseKey = String(state.pulseKey);
    element.appendChild(ripple);
    setTimeout(() => ripple.remove(), 520);
  }

  function scheduleRemoteTelepointerActivityClear(state: RemoteTelepointerState) {
    if (state.activityTimer) clearTimeout(state.activityTimer);
    state.activityTimer = setTimeout(() => {
      state.element?.classList.remove('is-controlling', 'is-typing');
      state.activityTimer = null;
    }, TELEPOINTER_ACTIVITY_TRAIL_MS);
  }

  function maybeRenderHandshake(currentKey: string, current: RemoteTelepointerState) {
    if (current.lastClickAt <= 0) return;
    for (const [otherKey, other] of remoteTelepointers.entries()) {
      if (otherKey === currentKey || other.lastClickAt <= 0) continue;
      if (other.message.windowId !== current.message.windowId) continue;
      if (Math.abs(current.lastClickAt - other.lastClickAt) > HANDSHAKE_WINDOW_MS) continue;
      const distance = Math.hypot(current.message.x - other.message.x, current.message.y - other.message.y);
      if (distance > HANDSHAKE_DISTANCE) continue;
      const pair = [currentKey, otherKey].sort().join('|');
      const now = Date.now();
      if ((handshakeCooldowns.get(pair) ?? 0) > now) continue;
      handshakeCooldowns.set(pair, now + HANDSHAKE_COOLDOWN_MS);
      const tile = shareTileForWindowId(current.message.windowId);
      if (!tile) return;
      const layer = ensureTelepointerLayer(tile);
      const burst = document.createElement('div');
      burst.className = 'remote-telepointer__handshake';
      const x = ((current.message.x + other.message.x) / 2) * 100;
      const y = ((current.message.y + other.message.y) / 2) * 100;
      burst.style.left = `${x}%`;
      burst.style.top = `${y}%`;
      burst.append(document.createElement('span'), document.createElement('span'));
      layer.appendChild(burst);
      setTimeout(() => burst.remove(), 1200);
      return;
    }
  }

  function renderTelepointersForWindow(windowId: number) {
    remoteTelepointers.forEach((state) => {
      if (state.message.windowId === windowId && state.message.visible) renderRemoteTelepointer(state);
    });
  }

  function scheduleTelepointerExpiry(key: string) {
    const state = remoteTelepointers.get(key);
    if (!state) return;
    if (state.staleTimer) clearTimeout(state.staleTimer);
    if (state.removeTimer) clearTimeout(state.removeTimer);
    state.staleTimer = setTimeout(() => {
      state.element?.classList.add('is-stale');
    }, TELEPOINTER_STALE_MS);
    state.removeTimer = setTimeout(() => removeRemoteTelepointer(key), TELEPOINTER_REMOVE_MS);
  }

  function updateRemoteTelepointer(message: TelepointerMessage) {
    const key = telepointerKey(message);
    if (!message.visible) {
      hideRemoteTelepointer(key);
      return;
    }

    let state = remoteTelepointers.get(key);
    if (!state) {
      state = {
        message,
        lastSeen: Date.now(),
        element: null,
        staleTimer: null,
        removeTimer: null,
        activityTimer: null,
        pulseKey: 0,
        lastClickAt: 0
      };
      remoteTelepointers.set(key, state);
    } else {
      state.message = message;
      state.lastSeen = Date.now();
    }
    renderRemoteTelepointer(state);
    if (message.activity === 'click') {
      state.lastClickAt = Date.now();
      state.element?.classList.add('is-controlling');
      state.element?.classList.remove('is-typing');
      triggerRemoteTelepointerPulse(state);
      scheduleRemoteTelepointerActivityClear(state);
      maybeRenderHandshake(key, state);
    } else if (message.activity === 'type') {
      state.element?.classList.add('is-controlling', 'is-typing');
      scheduleRemoteTelepointerActivityClear(state);
    }
    scheduleTelepointerExpiry(key);
  }

  function removeTelepointersForParticipant(identity: string) {
    Array.from(remoteTelepointers.entries()).forEach(([key, state]) => {
      if (state.message.userId === identity) removeRemoteTelepointer(key);
    });
  }

  function removeTelepointersForWindow(windowId: number) {
    Array.from(remoteTelepointers.entries()).forEach(([key, state]) => {
      if (state.message.windowId === windowId) removeRemoteTelepointer(key);
    });
  }

  function handleRemoteTelepointerPayload(payload: Uint8Array, senderIdentity?: string, topic?: string) {
    if (topic !== TELEPOINTER_TOPIC) return;
    const parsed = parseTelepointerPayload(payload);
    if (!parsed) {
      ctx.ui.logEvent('ignored malformed telepointer payload', 'warn');
      return;
    }
    const message = telepointerMessageFromAuthenticatedSender(parsed, senderIdentity);
    if (!message) {
      ctx.ui.logEvent('ignored telepointer payload without authenticated sender identity', 'warn');
      return;
    }
    updateRemoteTelepointer(message);
  }

  function repositionRemoteTelepointers() {
    remoteTelepointers.forEach((state) => {
      if (state.element && state.message.visible) {
        const tile = shareTileForWindowId(state.message.windowId);
        if (tile) positionTelepointer(state.element, tile, state.message);
      }
    });
  }

  function clearRemoteTelepointers() {
    Array.from(remoteTelepointers.keys()).forEach(removeRemoteTelepointer);
  }

  return {
    shareTileForWindowId,
    renderTelepointersForWindow,
    removeTelepointersForParticipant,
    removeTelepointersForWindow,
    handleRemoteTelepointerPayload,
    repositionRemoteTelepointers,
    clearRemoteTelepointers,
  };
}
