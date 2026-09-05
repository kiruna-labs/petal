import type { Room } from 'livekit-client';
import { normalizedPointInContainedMedia } from './remoteControl.ts';
import { TELEPOINTER_TOPIC, type TelepointerMessage } from './trackNames.ts';

export const TELEPOINTER_SEND_INTERVAL_MS = 45;

interface TelepointerSenderOptions {
  windowId: number;
  getRoom: () => Room | null;
}

type TelepointerPoint = { x: number; y: number };

interface HoverTelepointerTarget {
  targetUserId: string;
  windowId: number;
}

interface PendingHoverTelepointer {
  target: HoverTelepointerTarget;
  point: TelepointerPoint;
}

export interface HoverTelepointerTileLike {
  dataset: {
    owner?: string;
    windowId?: string;
    hoverTelepointerBound?: string;
    [key: string]: string | undefined;
  };
  addEventListener: HTMLDivElement['addEventListener'];
  querySelector: HTMLDivElement['querySelector'];
  getBoundingClientRect: HTMLDivElement['getBoundingClientRect'];
}

function cockpitRemoteShareTile(): HoverTelepointerTileLike | null {
  for (const tile of Array.from(document.querySelectorAll<HTMLElement>('.share-tile[data-owner][data-window-id]'))) {
    if (hoverTelepointerTargetFromTile(tile)) return tile;
  }
  return null;
}

export function telepointerPublishOptions(): { reliable: boolean; topic: typeof TELEPOINTER_TOPIC } {
  return { reliable: false, topic: TELEPOINTER_TOPIC };
}

export function telepointerMessage(
  windowId: number,
  userId: string,
  point: { x: number; y: number },
  visible: boolean
): TelepointerMessage {
  return {
    windowId,
    userId,
    x: point.x,
    y: point.y,
    visible,
  };
}

export function hoverTelepointerTargetFromTile(tile: Pick<HoverTelepointerTileLike, 'dataset'>): HoverTelepointerTarget | null {
  const targetUserId = tile.dataset.owner?.trim() ?? '';
  const windowId = Number(tile.dataset.windowId);
  if (!targetUserId || !Number.isSafeInteger(windowId) || windowId < 1 || windowId > 0xffff_ffff) return null;
  return { targetUserId, windowId };
}

export function hoverTelepointerPointForTile(
  tile: Pick<HoverTelepointerTileLike, 'querySelector' | 'getBoundingClientRect'>,
  event: Pick<PointerEvent, 'clientX' | 'clientY'>,
  clamp = true
): TelepointerPoint | null {
  const video = tile.querySelector<HTMLVideoElement>('video');
  const rect = (video ?? tile).getBoundingClientRect();
  return normalizedPointInContainedMedia(
    { left: rect.left, top: rect.top, width: rect.width, height: rect.height },
    { width: video?.videoWidth ?? 0, height: video?.videoHeight ?? 0 },
    { x: event.clientX, y: event.clientY },
    { clamp }
  );
}

export function createTelepointerSender({ windowId, getRoom }: TelepointerSenderOptions) {
  let telepointerTimer: ReturnType<typeof setInterval> | null = null;
  let telepointerPhase = 0;
  let hoverTimer: ReturnType<typeof setTimeout> | null = null;
  let pendingHoverTelepointer: PendingHoverTelepointer | null = null;
  let lastHoverTelepointerSentAt = 0;
  const encoder = new TextEncoder();

  function publishTelepointer(msg: TelepointerMessage) {
    const room = getRoom();
    if (!room) return;
    const bytes = encoder.encode(JSON.stringify(msg));
    room.localParticipant.publishData(bytes, telepointerPublishOptions()).catch(() => {});
  }

  async function publishCockpitTelepointer(): Promise<{ windowId: number }> {
    const room = getRoom();
    if (!room) throw new Error('telepointer requires an active room');
    const tile = cockpitRemoteShareTile();
    if (!tile) throw new Error('telepointer requires a remote share tile with owner and window id');
    const target = hoverTelepointerTargetFromTile(tile);
    if (!target) throw new Error('telepointer remote share tile did not expose a valid window id');
    const msg = telepointerMessage(target.windowId, room.localParticipant.identity, { x: 0.42, y: 0.58 }, true);
    const bytes = encoder.encode(JSON.stringify(msg));
    await room.localParticipant.publishData(bytes, telepointerPublishOptions());
    return { windowId: target.windowId };
  }

  function publishHoverTelepointer(target: HoverTelepointerTarget, point: TelepointerPoint, visible: boolean) {
    const room = getRoom();
    if (!room) return;
    publishTelepointer(telepointerMessage(target.windowId, room.localParticipant.identity, point, visible));
  }

  function clearHoverTimer() {
    if (hoverTimer !== null) {
      clearTimeout(hoverTimer);
      hoverTimer = null;
    }
  }

  function flushPendingHoverTelepointer() {
    clearHoverTimer();
    const pending = pendingHoverTelepointer;
    pendingHoverTelepointer = null;
    if (!pending) return;
    publishHoverTelepointer(pending.target, pending.point, true);
    lastHoverTelepointerSentAt = Date.now();
  }

  function scheduleHoverTelepointer(target: HoverTelepointerTarget, point: TelepointerPoint) {
    pendingHoverTelepointer = { target, point };
    const delay = Math.max(0, TELEPOINTER_SEND_INTERVAL_MS - (Date.now() - lastHoverTelepointerSentAt));
    if (delay === 0) {
      flushPendingHoverTelepointer();
      return;
    }
    if (hoverTimer === null) {
      hoverTimer = setTimeout(flushPendingHoverTelepointer, delay);
    }
  }

  function handleHoverTelepointerEnter(event: PointerEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    const target = hoverTelepointerTargetFromTile(tile);
    if (!target || !getRoom()) return;
    const point = hoverTelepointerPointForTile(tile, event, true);
    if (!point) return;
    pendingHoverTelepointer = null;
    clearHoverTimer();
    publishHoverTelepointer(target, point, true);
    lastHoverTelepointerSentAt = Date.now();
  }

  function handleHoverTelepointerMove(event: PointerEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    const target = hoverTelepointerTargetFromTile(tile);
    if (!target || !getRoom()) return;
    const point = hoverTelepointerPointForTile(tile, event, true);
    if (!point) return;
    scheduleHoverTelepointer(target, point);
  }

  function handleHoverTelepointerLeave(event: PointerEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    const target = hoverTelepointerTargetFromTile(tile);
    if (!target || !getRoom()) return;
    const point = hoverTelepointerPointForTile(tile, event, true) ?? { x: 0.5, y: 0.5 };
    pendingHoverTelepointer = null;
    clearHoverTimer();
    publishHoverTelepointer(target, point, false);
    lastHoverTelepointerSentAt = Date.now();
  }

  function bindHoverTelepointer(tile: HoverTelepointerTileLike) {
    if (!hoverTelepointerTargetFromTile(tile) || tile.dataset.hoverTelepointerBound === '1') return;
    tile.dataset.hoverTelepointerBound = '1';
    tile.addEventListener('pointerenter', handleHoverTelepointerEnter);
    tile.addEventListener('pointermove', handleHoverTelepointerMove);
    tile.addEventListener('pointerleave', handleHoverTelepointerLeave);
    tile.addEventListener('pointercancel', handleHoverTelepointerLeave);
  }

  function startTelepointerSender() {
    if (telepointerTimer !== null) return;
    telepointerPhase = 0;
    telepointerTimer = setInterval(() => {
      const room = getRoom();
      if (!room) return;
      telepointerPhase += 0.08;
      const x = 0.5 + 0.35 * Math.sin(telepointerPhase);
      const y = 0.5 + 0.35 * Math.sin(telepointerPhase * 0.7);
      const msg: TelepointerMessage = {
        windowId,
        userId: room.localParticipant.identity,
        x,
        y,
        visible: true,
      };
      publishTelepointer(msg);
    }, TELEPOINTER_SEND_INTERVAL_MS);
  }

  function stopTelepointerSender() {
    if (telepointerTimer !== null) {
      clearInterval(telepointerTimer);
      telepointerTimer = null;
    }
    pendingHoverTelepointer = null;
    clearHoverTimer();
    const room = getRoom();
    if (room) {
      const msg: TelepointerMessage = {
        windowId,
        userId: room.localParticipant.identity,
        x: 0.5,
        y: 0.5,
        visible: false,
      };
      publishTelepointer(msg);
    }
  }

  return { startTelepointerSender, stopTelepointerSender, bindHoverTelepointer, publishCockpitTelepointer };
}
