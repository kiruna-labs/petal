import type { TrackedShareWindow } from './publicationReconcile';
import {
  Track,
  type RemoteTrack,
  type LocalVideoTrack,
  type RemoteTrackPublication,
  type RemoteParticipant,
  type Room,
} from 'livekit-client';
import type { HarnessContext } from './context.ts';
import { commitLayoutModeTransition, layoutModeStateOf } from './tileLayout.ts';
import { dismissSpotlight, endAutoSpotlight } from '@petal/shared/logic/tileLayoutMode';
import {
  colorProfileFromMetadata,
  identityPaletteIndexFromMetadata,
  isAiTrackName,
  sharedSourceKindFromMetadata,
  sharedWindowShareInstanceFromMetadata,
  sharedWindowTitleFromMetadata,
  sharedWindowUrlFromMetadata,
  sharedWindowZOrderFromMetadata,
} from './trackNames.ts';
import { identityHeaderCss } from './telepointer.ts';
import { formatShareCountPillLabel, resolveForemostSharedWindowId } from './shareCountPill.ts';
import {
  createRemoteWindowHeader,
  type RemoteWindowHeaderController,
} from './remoteWindowHeader.ts';
import {
  CAMERA_DECODE_HEALTH_LOG_MS,
  formatCameraDecodeHealth,
  framesDecodedFromStatsReport,
  nextCameraDecodeHealthState,
  type CameraDecodeHealthState,
} from './cameraDecodeHealth.ts';
import {
  attachHoldLastFrame,
  type HoldLastFrameHandle,
  type HoldReason,
} from './holdLastFrame.ts';
import { noteVideoFrames } from './analytics.ts';
import { getTileReflowController } from './tileReflow.ts';

// ---------------------------------------------------------------------------
// Tiles. One BASE tile per participant (camera video, or an initials
// placeholder) plus one SEPARATE tile per screen-share track -- the browser
// can't render remote shares as independent native windows the way the desktop
// compositor does (SPEC.md §4.4), so shares become tiles in the grid.
// ---------------------------------------------------------------------------
function sanitizeId(s: string): string {
  return s.replace(/[^a-zA-Z0-9_-]/g, '');
}

function baseTileId(identity: string): string {
  return `tile-p-${sanitizeId(identity)}`;
}

function shareTileId(identity: string, key: string): string {
  return `tile-s-${sanitizeId(identity)}-${sanitizeId(key)}`;
}

export const REMOTE_CONTROL_SHARE_REMOVAL_GRACE_MS = 10_000;
export const SHARE_REPLACEMENT_GRACE_MS = 1_500;

interface SuspendedRemoteControl {
  targetUserId: string;
  windowId: number;
  oldTileId: string;
  timer: ReturnType<typeof setTimeout>;
}

interface PendingShareRemoval {
  identity: string;
  windowId: number;
  trackKey: string;
  tile: HTMLDivElement;
  timer: ReturnType<typeof setTimeout>;
}

export interface ParticipantNameLike {
  identity: string;
  name?: string | null;
}

interface FullRangeCanvasRenderer {
  canvas: HTMLCanvasElement;
  rafId: number | null;
}

export type BrowserColorCorrectionMode = 'none' | 'video-range-css' | 'full-range-canvas';

export function browserColorCorrectionMode(range: 'full' | 'video' | null | undefined): BrowserColorCorrectionMode {
  if (range === 'video') return 'video-range-css';
  if (range === 'full') return 'full-range-canvas';
  return 'none';
}

export function looksLikeTechnicalIdentity(value: string): boolean {
  const trimmed = value.trim();
  return (
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(trimmed) ||
    /^web-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(trimmed) ||
    /^[0-9a-f]{32}$/i.test(trimmed)
  );
}

export function participantDisplayName(identity: string, displayName?: string | null): string {
  const name = displayName?.trim();
  if (name && !looksLikeTechnicalIdentity(name)) return name;
  const readableIdentity = identity.trim();
  if (readableIdentity && !looksLikeTechnicalIdentity(readableIdentity)) return readableIdentity;
  return 'Guest';
}

export function displayNameForParticipant(participant: ParticipantNameLike): string {
  return participantDisplayName(participant.identity, participant.name);
}

/**
 * Whether this element is already showing exactly this track. Split out so a
 * caller can act BEFORE the swap happens -- #627's hold-last-frame has to
 * engage ahead of the gap, so it cannot use `attachVideoTrackIfChanged`'s
 * return value, which is only available afterwards.
 */
export function videoIsShowingTrack(
  video: HTMLVideoElement,
  track: RemoteTrack | LocalVideoTrack
): boolean {
  const currentStream = video.srcObject;
  const currentTracks =
    currentStream && typeof (currentStream as MediaStream).getTracks === 'function'
      ? (currentStream as MediaStream).getTracks()
      : [];
  return currentTracks.includes(track.mediaStreamTrack);
}

export function attachVideoTrackIfChanged(
  video: HTMLVideoElement,
  track: RemoteTrack | LocalVideoTrack
): boolean {
  if (videoIsShowingTrack(video, track)) return false;

  track.attach(video);
  return true;
}

function participantNameWithoutLocalSuffix(name: string): string {
  return name.replace(/\s*\(you\)\s*$/i, '').trim();
}

type SegmenterLike = {
  segment(input: string): Iterable<{ segment: string }>;
};

type SegmenterConstructor = new (
  locales?: string | string[],
  options?: { granularity?: 'grapheme' }
) => SegmenterLike;

function firstGrapheme(value: string): string {
  const trimmed = value.trim();
  if (!trimmed) return '';

  const Segmenter = (Intl as typeof Intl & { Segmenter?: SegmenterConstructor }).Segmenter;
  if (Segmenter) {
    const first = new Segmenter(undefined, { granularity: 'grapheme' })
      .segment(trimmed)
      [Symbol.iterator]()
      .next();
    if (!first.done) return first.value.segment;
  }

  return Array.from(trimmed)[0] ?? '';
}

function firstReadableInitial(word: string): string | null {
  return word.match(/[\p{L}\p{N}]/u)?.[0]?.toUpperCase() ?? null;
}

export function initialsFor(name: string): string {
  const initials = participantNameWithoutLocalSuffix(name)
    .split(/[\s_-]+/)
    .map(firstReadableInitial)
    .filter((initial): initial is string => initial !== null)
    .slice(0, 2)
    .join('');
  return initials || '?';
}

export function nameChipLabel(name: string, isLocal: boolean, variant: 'full' | 'compact' = 'full'): string {
  const baseName = (isLocal ? participantNameWithoutLocalSuffix(name) : name.trim()) || 'Guest';
  if (variant === 'compact') return initialsFor(baseName).slice(0, 1) || '?';
  return isLocal ? `${baseName} (you)` : baseName;
}

export function cameraOffNameLabel(name: string): string {
  return participantNameWithoutLocalSuffix(name) || 'Guest';
}

export function cameraOffNameFallback(name: string): string {
  return firstGrapheme(cameraOffNameLabel(name)) || '?';
}

export function setupTiles(
  ctx: HarnessContext,
  options: {
    remoteControlShareRemovalGraceMs?: number;
    shareReplacementGraceMs?: number;
  } = {}
) {
  const { dom, state, cb } = ctx;
  const { tilesEl, participantCountEl, displayNameInput } = dom;
  const { logEvent, showToast } = ctx.ui;
  const notifyDrawAvailability = () => cb.syncDrawAvailability?.();
  const remoteControlShareRemovalGraceMs =
    options.remoteControlShareRemovalGraceMs ?? REMOTE_CONTROL_SHARE_REMOVAL_GRACE_MS;
  const shareReplacementGraceMs = options.shareReplacementGraceMs ?? SHARE_REPLACEMENT_GRACE_MS;
  let suspendedRemoteControl: SuspendedRemoteControl | null = null;
  const fullRangeRenderers = new WeakMap<HTMLVideoElement, FullRangeCanvasRenderer>();
  // #627 hold-last-frame, one per remote share video. Keyed by video element so
  // a tile reused for a replacement track keeps its held frame across the swap
  // -- that swap is the single most common source of a black flash.
  const holdLastFrameHandles = new WeakMap<HTMLVideoElement, HoldLastFrameHandle>();
  const remoteWindowHeaders = new WeakMap<HTMLDivElement, RemoteWindowHeaderController>();
  // LiveKit can subscribe a replacement before it reports the old track
  // unsubscribed. Route only the current SID to the stable window tile; late
  // callbacks also validate the tile lifecycle before mutating it (issue #298).
  const shareTilesByTrack = new Map<string, HTMLDivElement>();
  const pendingShareRemovals = new Map<string, PendingShareRemoval>();
  // #875: per-identity window ids that currently have a LIVE share tile,
  // oldest-added first. This is the count pill's source of truth and its
  // metadata-absent fallback ordering. Deliberately hung off the same two
  // choke points every share tile passes through (`addShareTile` /
  // `finalizeShareTileRemoval`) instead of a second subscription layer --
  // every `petal-window-*` publication reaches one of those two functions
  // exactly once per attach/detach, local shares included.
  const sharedWindowIdsByIdentity = new Map<string, number[]>();
  const cameraDecodeHealthStates = new Map<string, CameraDecodeHealthState>();
  const cameraReadinessObservers = new WeakMap<HTMLVideoElement, AbortController>();
  let cameraDecodeHealthPoll: ReturnType<typeof setInterval> | null = null;

  function shareTileForTrack(identity: string, trackSid: string): HTMLDivElement | null {
    const tracked = shareTilesByTrack.get(`${identity}:${trackSid}`);
    if (tracked) return tracked;
    const fallback = document.getElementById(shareTileId(identity, trackSid));
    return fallback instanceof HTMLDivElement ? fallback : null;
  }

  function currentShareTileForTrack(identity: string, trackSid: string): HTMLDivElement | null {
    const tile = shareTileForTrack(identity, trackSid);
    return tile?.dataset.owner === identity && tile.dataset.trackSid === trackSid ? tile : null;
  }

  function stableShareKey(identity: string, windowId: number): string {
    return `${identity}:${windowId}`;
  }

  function takePendingShareTile(identity: string, windowId: number): HTMLDivElement | null {
    const stableKey = stableShareKey(identity, windowId);
    const pending = pendingShareRemovals.get(stableKey);
    if (!pending) return null;
    pendingShareRemovals.delete(stableKey);
    clearTimeout(pending.timer);
    return pending.tile;
  }

  function clearPendingShareRemovals(identity?: string) {
    for (const [stableKey, pending] of pendingShareRemovals) {
      if (identity !== undefined && pending.identity !== identity) continue;
      clearTimeout(pending.timer);
      pendingShareRemovals.delete(stableKey);
    }
  }
  const tileReflow = getTileReflowController(tilesEl);

  function validWindowIdFromTile(tile: HTMLDivElement | null | undefined): number | null {
    const windowId = Number(tile?.dataset.windowId);
    return Number.isSafeInteger(windowId) && windowId >= 1 && windowId <= 0xffff_ffff ? windowId : null;
  }

  // ---------------------------------------------------------------------
  // #875: multi-share count pill on the owner's CAMERA tile. Only the count
  // ("2+ windows shared") plus a click-to-spotlight-foremost affordance --
  // native's raise-everything-in-z-order behavior has no web equivalent
  // (there is no independent movable window to raise on this side).
  // ---------------------------------------------------------------------

  function metadataForIdentity(identity: string): string | null | undefined {
    return state.room?.localParticipant.identity === identity
      ? state.room?.localParticipant.metadata
      : state.room?.remoteParticipants.get(identity)?.metadata;
  }

  function noteShareWindowAdded(identity: string, windowId: number) {
    const ids = sharedWindowIdsByIdentity.get(identity) ?? [];
    if (!ids.includes(windowId)) {
      ids.push(windowId);
      sharedWindowIdsByIdentity.set(identity, ids);
    }
    refreshShareCountPill(identity);
  }

  function noteShareWindowRemoved(identity: string, windowId: number) {
    const ids = sharedWindowIdsByIdentity.get(identity);
    if (!ids || !ids.includes(windowId)) return;
    const next = ids.filter((id) => id !== windowId);
    if (next.length > 0) sharedWindowIdsByIdentity.set(identity, next);
    else sharedWindowIdsByIdentity.delete(identity);
    refreshShareCountPill(identity);
  }

  // Scoped by BOTH owner identity and window id -- NOT the identity-blind
  // `telepointerDisplay.ts#shareTileForWindowId`, which cross-matches a
  // colliding window id between two different participants (#678 class).
  // Mirrors the existing tile-recovery lookup in `addShareTile` below.
  function shareTileForOwnerAndWindowId(identity: string, windowId: number): HTMLDivElement | null {
    return (
      Array.from(document.querySelectorAll<HTMLDivElement>('.share-tile')).find(
        (candidate) => candidate.dataset.owner === identity && validWindowIdFromTile(candidate) === windowId
      ) ?? null
    );
  }

  function scrollShareTileIntoView(tile: HTMLDivElement) {
    if (typeof tile.scrollIntoView !== 'function') return;
    const reducedMotion =
      typeof window !== 'undefined' &&
      typeof window.matchMedia === 'function' &&
      window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    // Covers the tile sitting in `.spotlight-strip` (overflow-x: auto) or
    // scrolled out of the grid's own `overflow: auto` -- neither scrolls
    // itself today, which is the gap this closes.
    tile.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: reducedMotion ? 'auto' : 'smooth' });
  }

  function handleShareCountPillClick(identity: string, event: MouseEvent) {
    event.stopPropagation();
    const zOrder = sharedWindowZOrderFromMetadata(metadataForIdentity(identity));
    const tiledIds = sharedWindowIdsByIdentity.get(identity) ?? [];
    const windowId = resolveForemostSharedWindowId(zOrder, tiledIds);
    if (windowId === null) return;
    const tile = shareTileForOwnerAndWindowId(identity, windowId);
    if (!tile) return;
    cb.pinTile(tile, 'manual');
    scrollShareTileIntoView(tile);
  }

  function renderShareCountPill(baseTile: HTMLDivElement, identity: string, isLocal: boolean) {
    const count = sharedWindowIdsByIdentity.get(identity)?.length ?? 0;
    const label = formatShareCountPillLabel(count);
    let pill = baseTile.querySelector<HTMLButtonElement>('.share-count-pill');
    if (!label) {
      pill?.remove();
      return;
    }
    if (!pill) {
      pill = document.createElement('button');
      pill.type = 'button';
      pill.className = 'share-count-pill';
      baseTile.appendChild(pill);
      baseTile.classList.add('has-share-count-pill');
    }
    pill.textContent = label;
    const tint = identityHeaderCss(identity, identityPaletteIndexFromMetadata(metadataForIdentity(identity)));
    pill.style.setProperty('--share-pill-bg', tint.background);
    pill.style.setProperty('--share-pill-ink', tint.ink);
    if (isLocal) {
      // Locked decision (#875): the local user's own pill shows how much
      // they're exposing but is NON-interactive this iteration -- disabling
      // the real <button> both drops it from the tab order and (same as the
      // remote case) keeps tileLayout.ts's own click handler from firing a
      // redundant pin, since `closest('button, ...')` bails on it either way.
      pill.disabled = true;
      pill.onclick = null;
      pill.setAttribute('aria-label', `${count} ${count === 1 ? 'window' : 'windows'} you are sharing`);
    } else {
      pill.disabled = false;
      const name = labelForIdentity(identity, false);
      pill.setAttribute('aria-label', `${count} windows shared by ${name} — spotlight foremost`);
      pill.onclick = (event) => handleShareCountPillClick(identity, event);
    }
  }

  function refreshShareCountPill(identity: string) {
    const baseTile = document.getElementById(baseTileId(identity)) as HTMLDivElement | null;
    if (!baseTile) return;
    const isLocal = state.room?.localParticipant.identity === identity;
    renderShareCountPill(baseTile, identity, isLocal);
  }

  function clearSuspendedRemoteControl() {
    if (!suspendedRemoteControl) return;
    clearTimeout(suspendedRemoteControl.timer);
    suspendedRemoteControl = null;
  }

  function clampByte(value: number): number {
    return value <= 0 ? 0 : value >= 255 ? 255 : value;
  }

  function applyFullRangeCorrection(frame: ImageData) {
    const data = frame.data;
    for (let i = 0; i < data.length; i += 4) {
      // Browser video gives us decoded RGB, not raw YUV. This reverses the
      // browser's limited-range levels assumption per channel; it is an
      // approximation of the ideal YUV-space inverse, but fixes the visible
      // contrast crush for native full-range BT.709 shares (#121).
      data[i] = clampByte((data[i] - 16) * (255 / 219));
      data[i + 1] = clampByte((data[i + 1] - 16) * (255 / 219));
      data[i + 2] = clampByte((data[i + 2] - 16) * (255 / 219));
    }
  }

  function stopFullRangeRenderer(video: HTMLVideoElement) {
    const renderer = fullRangeRenderers.get(video);
    if (!renderer) return;
    if (renderer.rafId !== null) cancelAnimationFrame(renderer.rafId);
    renderer.canvas.remove();
    video.classList.remove('full-range-source-video');
    video.closest('.tile')?.classList.remove('full-range-color-corrected');
    fullRangeRenderers.delete(video);
  }

  function stopVideoRangeCorrection(video: HTMLVideoElement) {
    video.classList.remove('video-range-source-video');
    video.closest('.tile')?.classList.remove('video-range-color-corrected');
  }

  function startVideoRangeCorrection(tile: HTMLDivElement, video: HTMLVideoElement) {
    video.classList.add('video-range-source-video');
    tile.classList.add('video-range-color-corrected');
  }

  function stopFullRangeRenderersForTile(tile: HTMLElement) {
    tile.querySelectorAll<HTMLVideoElement>('video').forEach((video) => {
      stopFullRangeRenderer(video);
      stopVideoRangeCorrection(video);
    });
  }

  /**
   * #627 hold-last-frame. Skipped while the full-range canvas renderer owns the
   * tile: that path already holds its last frame structurally (the canvas keeps
   * its pixels and `startFullRangeRenderer`'s render loop draws only when
   * `readyState >= 2`, so a gap leaves the last painted frame on screen), and
   * stacking a second, uncorrected copy over it would show a colour shift
   * during the freeze. That gate is load-bearing for this exemption rather
   * than incidental, so `holdLastFrame.test.ts` asserts it still exists.
   */
  function ensureHoldLastFrame(tile: HTMLDivElement, video: HTMLVideoElement) {
    if (fullRangeRenderers.has(video)) return;
    attachHoldLastFrame(tile, video, holdLastFrameHandles);
  }

  function stopHoldLastFrame(video: HTMLVideoElement) {
    holdLastFrameHandles.get(video)?.stop();
  }

  function stopHoldLastFrameForTile(tile: HTMLElement) {
    tile.querySelectorAll<HTMLVideoElement>('video').forEach(stopHoldLastFrame);
  }

  /**
   * Engage the held frame for one share track ahead of a disruption the caller
   * already knows about (track mute, stream pause). The stall watchdog would
   * catch these on its own, but only after `HOLD_STALL_MS` of black -- and the
   * rule is zero black frames, not briefly-black ones.
   */
  function holdShareFrame(identity: string, trackSid: string, reason: HoldReason) {
    const tile = shareTilesByTrack.get(`${identity}:${trackSid}`);
    const video = tile?.querySelector('video');
    if (!video) return;
    holdLastFrameHandles.get(video)?.noteGap(reason);
  }

  function withTileReflowAnimation<T>(mutate: () => T): T {
    return tileReflow.withAnimation(mutate);
  }

  function mutateWithoutTileReflow<T>(mutate: () => T): T {
    return tileReflow.withoutAnimation(mutate);
  }

  function ensureRemoteWindowHeader(
    tile: HTMLDivElement,
    identity: string,
    isLocal: boolean,
    track: RemoteTrack | LocalVideoTrack,
    video: HTMLVideoElement,
    label: string,
    windowId?: number | null,
    participantMetadata?: string | null
  ) {
    const ownerName = participantDisplayName(identity, label);
    const normalizedWindowId = windowId ?? validWindowIdFromTile(tile);
    const sourceTitle =
      normalizedWindowId !== null ? sharedWindowTitleFromMetadata(participantMetadata, normalizedWindowId) : null;
    const sourceUrl =
      normalizedWindowId !== null ? sharedWindowUrlFromMetadata(participantMetadata, normalizedWindowId) : null;
    const options = {
      ctx,
      tile,
      ownerIdentity: identity,
      ownerName,
      isLocal,
      track,
      video,
      windowId: normalizedWindowId,
      sourceTitle,
      sourceUrl,
      // The header is the user's only handle on this tile's remote-window
      // controls (View/Control/Draw/Debug, size toggle) -- mirrors native's
      // real compositor surface, which forces autoHide=false for the same
      // reason (apps/desktop/src/routes/compositor/surface/+page.svelte).
      // Idle-collapsing it away also isn't needed here since the tile now
      // reserves fixed space for it instead of floating over the video.
      autoHide: false,
      // #785: land where the user actually was. An unconditional grid + persist
      // here overwrote the saved preference just like the auto-spotlight did.
      onMinimizeWindow: () => {
        if (state.pinnedTileId === tile.id) state.pinnedTileId = null;
        commitLayoutModeTransition(state, dismissSpotlight(layoutModeStateOf(state)));
        cb.applyTileLayout();
      },
      onExpandWindow: () => cb.pinTile(tile, 'manual'),
    };
    let header = remoteWindowHeaders.get(tile);
    if (!header) {
      header = createRemoteWindowHeader(options);
      remoteWindowHeaders.set(tile, header);
      tile.classList.add('has-remote-window-header');
    } else {
      header.update(options);
    }
  }

  function syncRemoteWindowHeaders() {
    document.querySelectorAll<HTMLDivElement>('.share-tile').forEach((tile) => {
      remoteWindowHeaders.get(tile)?.syncMode();
    });
  }

  function destroyRemoteWindowHeader(tile: HTMLElement) {
    if (!(tile instanceof HTMLDivElement)) return;
    const header = remoteWindowHeaders.get(tile);
    if (!header) return;
    header.destroy();
    remoteWindowHeaders.delete(tile);
    tile.classList.remove('has-remote-window-header');
  }

  function startFullRangeRenderer(tile: HTMLDivElement, video: HTMLVideoElement) {
    if (fullRangeRenderers.has(video) || typeof requestAnimationFrame !== 'function') return;
    const canvas = document.createElement('canvas');
    canvas.className = 'full-range-canvas';
    const context = canvas.getContext('2d', { willReadFrequently: true });
    if (!context) return;

    tile.insertBefore(canvas, tile.querySelector('.name-chip'));
    video.classList.add('full-range-source-video');
    tile.classList.add('full-range-color-corrected');

    const renderer: FullRangeCanvasRenderer = { canvas, rafId: null };
    fullRangeRenderers.set(video, renderer);

    const render = () => {
      const current = fullRangeRenderers.get(video);
      if (current !== renderer) return;
      if ('isConnected' in tile && !tile.isConnected) {
        stopFullRangeRenderer(video);
        return;
      }

      const width = video.videoWidth;
      const height = video.videoHeight;
      if (width > 0 && height > 0 && video.readyState >= 2) {
        if (canvas.width !== width) canvas.width = width;
        if (canvas.height !== height) canvas.height = height;
        try {
          context.drawImage(video, 0, 0, width, height);
          const frame = context.getImageData(0, 0, width, height);
          applyFullRangeCorrection(frame);
          context.putImageData(frame, 0, 0);
        } catch (err) {
          logEvent(`full-range canvas correction disabled: ${(err as Error).message ?? err}`, 'warn');
          stopFullRangeRenderer(video);
          return;
        }
      }
      renderer.rafId = requestAnimationFrame(render);
    };
    renderer.rafId = requestAnimationFrame(render);
  }

  function syncShareColorProfile(
    tile: HTMLDivElement,
    video: HTMLVideoElement,
    metadata: string | undefined | null,
    windowId?: number | null
  ) {
    const profile =
      windowId !== null && windowId !== undefined ? colorProfileFromMetadata(metadata, windowId) : null;
    switch (browserColorCorrectionMode(profile?.range)) {
      case 'video-range-css':
        stopFullRangeRenderer(video);
        startVideoRangeCorrection(tile, video);
        ensureHoldLastFrame(tile, video);
        break;
      case 'full-range-canvas':
        stopVideoRangeCorrection(video);
        // The full-range canvas is itself a hold-last-frame surface; a second
        // uncorrected copy over it would shift colour mid-freeze (#627).
        stopHoldLastFrame(video);
        startFullRangeRenderer(tile, video);
        break;
      case 'none':
        stopFullRangeRenderer(video);
        stopVideoRangeCorrection(video);
        ensureHoldLastFrame(tile, video);
        break;
    }
  }

  function suspendRemoteControlForRemovedShare(tile: HTMLDivElement, identity: string, windowId: number) {
    const active = state.activeRemoteControl;
    if (
      !active ||
      active.tileId !== tile.id ||
      active.targetUserId !== identity ||
      active.windowId !== windowId
    ) {
      return false;
    }

    clearSuspendedRemoteControl();
    suspendedRemoteControl = {
      targetUserId: identity,
      windowId,
      oldTileId: tile.id,
      timer: setTimeout(() => {
        const suspended = suspendedRemoteControl;
        if (
          suspended &&
          state.activeRemoteControl?.targetUserId === suspended.targetUserId &&
          state.activeRemoteControl.windowId === suspended.windowId &&
          state.activeRemoteControl.tileId === suspended.oldTileId
        ) {
          cb.stopRemoteControl('share ended');
          showToast('Remote control ended because the shared window disappeared');
        }
        if (suspendedRemoteControl === suspended) suspendedRemoteControl = null;
      }, remoteControlShareRemovalGraceMs),
    };
    return true;
  }

  function rebindSuspendedRemoteControl(identity: string, windowId: number | null | undefined, tile: HTMLDivElement) {
    const suspended = suspendedRemoteControl;
    if (!suspended || windowId === null || windowId === undefined) return;
    if (suspended.targetUserId !== identity || suspended.windowId !== windowId) return;
    const active = state.activeRemoteControl;
    if (
      !active ||
      active.targetUserId !== identity ||
      active.windowId !== windowId ||
      active.tileId !== suspended.oldTileId
    ) {
      clearSuspendedRemoteControl();
      return;
    }
    clearSuspendedRemoteControl();
    active.tileId = tile.id;
    active.pointerId = null;
  }

  function labelForIdentity(identity: string, isLocal: boolean): string {
    if (isLocal) return participantDisplayName(identity, state.room?.localParticipant.name ?? displayNameInput.value);
    const participant = state.room?.remoteParticipants.get(identity);
    return participant ? displayNameForParticipant(participant) : participantDisplayName(identity);
  }

  function labelSpanForNameChip(chip: Element | null): HTMLSpanElement | null {
    return chip?.querySelector<HTMLSpanElement>('.name-chip-label') ?? null;
  }

  function fitNameChipLabel(chip: HTMLDivElement) {
    const label = labelSpanForNameChip(chip);
    if (!label) return;
    const fullLabel = label.dataset.fullLabel ?? label.textContent ?? '';
    const compactLabel = (label.dataset.compactLabel ?? initialsFor(fullLabel).slice(0, 1)) || '?';
    label.textContent = fullLabel;

    const chipWidth = chip.clientWidth ?? 0;
    const labelWidth = label.clientWidth ?? 0;
    const chipHasBox =
      chipWidth > 0 || (typeof chip.getClientRects === 'function' && chip.getClientRects().length > 0);
    if (!chipHasBox) return;

    const chipOverflows = (chip.scrollWidth ?? 0) > chipWidth;
    const labelOverflows = labelWidth > 0 && (label.scrollWidth ?? 0) > labelWidth;
    if (chipOverflows || labelOverflows) label.textContent = compactLabel;
  }

  function fitCameraOffName(tile: HTMLElement) {
    const label = tile.querySelector<HTMLSpanElement>('.initials');
    if (!label) return;

    const fullLabel = label.dataset.fullLabel ?? label.textContent ?? '';
    const compactLabel = label.dataset.compactLabel ?? cameraOffNameFallback(fullLabel);
    label.textContent = fullLabel;

    const tileWidth = tile.clientWidth ?? 0;
    const labelWidth = label.scrollWidth ?? 0;
    const tileHasBox =
      tileWidth > 0 || (typeof tile.getClientRects === 'function' && tile.getClientRects().length > 0);
    if (!tileHasBox) return;

    const availableWidth = Math.max(0, tileWidth - 32);
    if (labelWidth > availableWidth + 0.5) label.textContent = compactLabel;
  }

  function setCameraOffName(tile: HTMLElement, name: string) {
    const label = tile.querySelector<HTMLSpanElement>('.initials');
    if (!label) return;

    const fullLabel = cameraOffNameLabel(name);
    label.dataset.fullLabel = fullLabel;
    label.dataset.compactLabel = cameraOffNameFallback(name);
    fitCameraOffName(tile);
  }

  function setNameChipLabel(chip: HTMLDivElement, name: string, isLocal: boolean) {
    const label = labelSpanForNameChip(chip);
    if (!label) return;
    const fullLabel = nameChipLabel(name, isLocal);
    label.dataset.fullLabel = fullLabel;
    label.dataset.compactLabel = nameChipLabel(name, isLocal, 'compact');
    chip.title = fullLabel;
    fitNameChipLabel(chip);
  }

  function fitTileNameChips(tile: HTMLElement) {
    tile.querySelectorAll<HTMLDivElement>('.name-chip').forEach((chip) => fitNameChipLabel(chip));
  }

  function fitTileLabels(tile: HTMLElement) {
    fitCameraOffName(tile);
    fitTileNameChips(tile);
  }

  function resetCameraVideoReadiness(tile: HTMLDivElement, video: HTMLVideoElement) {
    cameraReadinessObservers.get(video)?.abort();
    tile.classList.add('camera-starting');
    tile.classList.remove('camera-ready');
    video.classList.remove('camera-video-ready');
  }

  function markCameraVideoReady(tile: HTMLDivElement, video: HTMLVideoElement) {
    if (video.classList.contains('camera-video-ready')) return;
    cameraReadinessObservers.get(video)?.abort();
    cameraReadinessObservers.delete(video);
    video.classList.add('camera-video-ready');
    tile.classList.add('camera-ready');
    tile.classList.remove('camera-off');
    tile.classList.remove('camera-starting');
    tile.querySelector('.initials')?.classList.remove('hidden');
  }

  function videoHasRenderableFrame(video: HTMLVideoElement): boolean {
    return video.readyState >= 2 && video.videoWidth > 0 && video.videoHeight > 0;
  }

  function waitForCameraVideoReady(tile: HTMLDivElement, video: HTMLVideoElement) {
    resetCameraVideoReadiness(tile, video);
    if (videoHasRenderableFrame(video)) {
      markCameraVideoReady(tile, video);
      return;
    }

    const abortController = new AbortController();
    cameraReadinessObservers.set(video, abortController);
    const tryMarkReady = () => {
      if (!videoHasRenderableFrame(video)) return;
      markCameraVideoReady(tile, video);
    };
    video.addEventListener('loadeddata', tryMarkReady, { signal: abortController.signal });
    video.addEventListener('canplay', tryMarkReady, { signal: abortController.signal });
    video.addEventListener('playing', tryMarkReady, { signal: abortController.signal });
    video.requestVideoFrameCallback?.(() => {
      if (!abortController.signal.aborted) markCameraVideoReady(tile, video);
    });
  }

  const nameChipObservers = new WeakMap<HTMLDivElement, ResizeObserver>();
  const cameraOffNameObservers = new WeakMap<HTMLElement, ResizeObserver>();

  function observeTileNameChips(tile: HTMLElement) {
    if (typeof ResizeObserver === 'undefined') return;
    if (!cameraOffNameObservers.has(tile)) {
      const observer = new ResizeObserver(() => fitCameraOffName(tile));
      observer.observe(tile);
      cameraOffNameObservers.set(tile, observer);
    }
    tile.querySelectorAll<HTMLDivElement>('.name-chip').forEach((chip) => {
      if (nameChipObservers.has(chip)) return;
      const observer = new ResizeObserver(() => fitNameChipLabel(chip));
      observer.observe(chip);
      nameChipObservers.set(chip, observer);
    });
  }

  function disconnectTileNameChipObservers(tile: HTMLElement) {
    cameraOffNameObservers.get(tile)?.disconnect();
    cameraOffNameObservers.delete(tile);
    tile.querySelectorAll<HTMLDivElement>('.name-chip').forEach((chip) => {
      nameChipObservers.get(chip)?.disconnect();
      nameChipObservers.delete(chip);
    });
  }

  function makeNameChip(name: string, isLocal: boolean, tag?: string): HTMLDivElement {
    const chip = document.createElement('div');
    chip.className = 'name-chip';
    const audioDot = document.createElement('span');
    audioDot.className = 'audio-dot hidden';
    chip.appendChild(audioDot);
    const nameSpan = document.createElement('span');
    nameSpan.className = 'name-chip-label';
    chip.appendChild(nameSpan);
    setNameChipLabel(chip, name, isLocal);
    if (tag) {
      const tagSpan = document.createElement('span');
      tagSpan.className = 'tag';
      tagSpan.textContent = tag;
      chip.appendChild(tagSpan);
    }
    return chip;
  }

  function ensureBaseTile(identity: string, isLocal: boolean): HTMLDivElement {
    const displayName = labelForIdentity(identity, isLocal);
    let tile = document.getElementById(baseTileId(identity)) as HTMLDivElement | null;
    if (!tile) {
      const newTile = document.createElement('div');
      newTile.id = baseTileId(identity);
      newTile.className = 'tile camera-off';
      newTile.dataset.owner = identity;
      const initials = document.createElement('span');
      initials.className = 'initials';
      newTile.appendChild(initials);
      newTile.appendChild(makeNameChip(displayName, isLocal));
      setCameraOffName(newTile, displayName);
      withTileReflowAnimation(() => {
        // Local tile always first.
        if (isLocal && tilesEl.firstChild) {
          tilesEl.insertBefore(newTile, tilesEl.firstChild);
        } else {
          tilesEl.appendChild(newTile);
        }
      });
      tile = newTile;
      fitTileLabels(tile);
      observeTileNameChips(tile);
      updateParticipantCount();
    } else {
      setCameraOffName(tile, displayName);
      const chip = tile.querySelector<HTMLDivElement>('.name-chip');
      if (chip) setNameChipLabel(chip, displayName, isLocal);
      fitTileLabels(tile);
      observeTileNameChips(tile);
    }
    cb.bindTileInteractions(tile);
    cb.applyTileLayout();
    // #875: (re)apply the pill in case a share attached before this base tile
    // existed (e.g. a local screen share started before `ensureBaseTile` ran
    // for the local identity) -- otherwise its count would be silently
    // dropped rather than shown once the tile catches up.
    refreshShareCountPill(identity);
    return tile;
  }

  function setTileCamera(
    identity: string,
    isLocal: boolean,
    track: RemoteTrack | LocalVideoTrack,
    drawWindowId?: number | null
  ) {
    const tile = ensureBaseTile(identity, isLocal);
    if (drawWindowId !== null && drawWindowId !== undefined) {
      tile.dataset.drawWindowId = String(drawWindowId);
    }
    let video = tile.querySelector('video');
    if (!video) {
      video = document.createElement('video');
      video.className = 'camera-video';
      video.autoplay = true;
      video.playsInline = true;
      video.muted = isLocal; // avoid local echo when previewing our own media
      tile.insertBefore(video, tile.querySelector('.name-chip'));
    }
    const attachedNewTrack = attachVideoTrackIfChanged(video, track);
    if (attachedNewTrack || !video.classList.contains('camera-video-ready')) {
      waitForCameraVideoReady(tile, video);
    }
    notifyDrawAvailability();
  }

  function clearTileCamera(identity: string) {
    const tile = document.getElementById(baseTileId(identity));
    if (!tile) return;
    const video = tile.querySelector<HTMLVideoElement>('video');
    if (video) {
      cameraReadinessObservers.get(video)?.abort();
      cameraReadinessObservers.delete(video);
      video.pause?.();
      video.srcObject = null;
      video.classList.remove('camera-video-ready');
    }
    tile.classList.add('camera-off');
    tile.classList.remove('camera-starting');
    tile.classList.remove('camera-ready');
    delete tile.dataset.drawWindowId;
    fitCameraOffName(tile);
    tile.querySelector('.initials')?.classList.remove('hidden');
    notifyDrawAvailability();
  }

  function cameraDecodeHealthKey(identity: string, pub: RemoteTrackPublication): string {
    return `${identity}:${pub.trackSid || pub.trackName || 'camera'}`;
  }

  async function pollCameraDecodeHealth() {
    if (!state.room) {
      cameraDecodeHealthStates.clear();
      return;
    }
    const seen = new Set<string>();
    const now = Date.now();
    const reads: Promise<void>[] = [];
    state.room.remoteParticipants.forEach((participant: RemoteParticipant) => {
      participant.trackPublications.forEach((pub) => {
        const remotePub = pub as RemoteTrackPublication;
        if (!isCameraTrack(remotePub) || !remotePub.track) return;
        const key = cameraDecodeHealthKey(participant.identity, remotePub);
        seen.add(key);
        reads.push(
          (async () => {
            let report: RTCStatsReport | null = null;
            try {
              const statsTrack = remotePub.track as RemoteTrack & {
                getRTCStatsReport?: () => Promise<RTCStatsReport | undefined | null>;
              };
              report =
                typeof statsTrack.getRTCStatsReport === 'function'
                  ? ((await statsTrack.getRTCStatsReport()) ?? null)
                  : null;
            } catch {
              report = null;
            }
            const framesDecoded = framesDecodedFromStatsReport(report);
            const next = nextCameraDecodeHealthState(
              cameraDecodeHealthStates.get(key),
              framesDecoded,
              now
            );
            cameraDecodeHealthStates.set(key, next.state);
            noteVideoFrames(key, framesDecoded, 'gallery', now);
            if (next.health) {
              logEvent(
                formatCameraDecodeHealth({
                  identity: participant.identity,
                  trackName: remotePub.trackName,
                  ...next.health,
                })
              );
            }
          })()
        );
      });
    });
    await Promise.all(reads);
    Array.from(cameraDecodeHealthStates.keys()).forEach((key) => {
      if (!seen.has(key)) cameraDecodeHealthStates.delete(key);
    });
  }

  function ensureCameraDecodeHealthPoll() {
    if (cameraDecodeHealthPoll !== null || typeof setInterval !== 'function') return;
    cameraDecodeHealthPoll = setInterval(
      () => void pollCameraDecodeHealth(),
      CAMERA_DECODE_HEALTH_LOG_MS
    );
    (cameraDecodeHealthPoll as { unref?: () => void }).unref?.();
  }

  function addShareTile(
    identity: string,
    isLocal: boolean,
    key: string,
    track: RemoteTrack | LocalVideoTrack,
    label: string,
    windowId?: number | null,
    participantMetadata?: string | null
  ) {
    const beforeShareCount = cb.shareTileCount();
    const id = shareTileId(identity, key);
    const trackKey = `${identity}:${key}`;
    let tile =
      windowId !== null && windowId !== undefined
        ? takePendingShareTile(identity, windowId)
        : null;
    tile ??= shareTilesByTrack.get(trackKey) ?? (document.getElementById(id) as HTMLDivElement | null);
    if (!tile && windowId !== null && windowId !== undefined) {
      tile = Array.from(document.querySelectorAll<HTMLDivElement>('.share-tile')).find(
        (candidate) => candidate.dataset.owner === identity && validWindowIdFromTile(candidate) === windowId
      ) ?? null;
    }
    if (!tile) {
      const newTile = document.createElement('div');
      newTile.id = id;
      newTile.className = 'tile share-tile';
      newTile.dataset.owner = identity;
      if (windowId !== null && windowId !== undefined) {
        newTile.dataset.windowId = String(windowId);
        newTile.dataset.drawWindowId = String(windowId);
      }
      newTile.appendChild(makeNameChip(participantDisplayName(identity, label), isLocal, 'sharing'));
      const mutateShareTile = () => {
        tilesEl.appendChild(newTile);
      };
      if (beforeShareCount === 0) mutateWithoutTileReflow(mutateShareTile);
      else withTileReflowAnimation(mutateShareTile);
      tile = newTile;
      fitTileNameChips(tile);
      observeTileNameChips(tile);
    } else if (windowId !== null && windowId !== undefined) {
      tile.dataset.windowId = String(windowId);
      tile.dataset.drawWindowId = String(windowId);
      fitTileNameChips(tile);
      observeTileNameChips(tile);
    }
    if (windowId !== null && windowId !== undefined) {
      tile.dataset.sourceKind = sharedSourceKindFromMetadata(participantMetadata, windowId);
      const shareInstanceId = sharedWindowShareInstanceFromMetadata(participantMetadata, windowId);
      if (shareInstanceId) tile.dataset.shareInstanceId = shareInstanceId;
      else delete tile.dataset.shareInstanceId;
    }
    for (const [knownTrackKey, knownTile] of shareTilesByTrack) {
      if (knownTile === tile && knownTrackKey !== trackKey) shareTilesByTrack.delete(knownTrackKey);
    }
    tile.dataset.trackSid = key;
    shareTilesByTrack.set(trackKey, tile);
    cb.bindTileInteractions(tile);
    let video = tile.querySelector('video');
    if (!video) {
      video = document.createElement('video');
      video.autoplay = true;
      video.playsInline = true;
      video.muted = isLocal;
      video.classList.add('share-video');
      tile.insertBefore(video, tile.querySelector('.name-chip'));
      video.addEventListener('loadedmetadata', () => {
        cb.repositionRemoteTelepointers();
        cb.repositionRemoteDraw();
      });
    }
    // #627: a republish swaps this element's srcObject, which leaves it with no
    // frame to present for as long as the new track takes to produce one. Hold
    // the last frame BEFORE the swap, not after -- reacting to the gap once it
    // has started still shows black for however long detection takes.
    if (!isLocal) {
      const willSwapTrack = !videoIsShowingTrack(video, track);
      ensureHoldLastFrame(tile, video);
      if (willSwapTrack) holdLastFrameHandles.get(video)?.noteGap('source-swap');
    }
    attachVideoTrackIfChanged(video, track);
    // These are receiver-local facts only. They make a slow/black initial
    // share distinguishable from a missing subscription without touching media
    // policy. A data attribute keeps re-layout/re-attach from emitting again.
    if (!isLocal && windowId !== null && windowId !== undefined && key) {
      const lifecycleKey = `${identity}:${windowId}:${key}`;
      video.dataset.shareLifecycle = lifecycleKey;
      const isCurrentLifecycle = () =>
        tile.dataset.owner === identity &&
        tile.dataset.windowId === String(windowId) &&
        tile.dataset.trackSid === key &&
        video.dataset.shareLifecycle === lifecycleKey;
      const markDecoded = () => {
        if (!isCurrentLifecycle()) return;
        if (video.dataset.pipelineDecoded === lifecycleKey) return;
        video.dataset.pipelineDecoded = lifecycleKey;
        ctx.hook.pipelineStats?.trackFirstDecoded(identity, windowId, key);
      };
      const markPresented = () => {
        if (!isCurrentLifecycle()) return;
        if (video.dataset.pipelinePresented === lifecycleKey) return;
        video.dataset.pipelinePresented = lifecycleKey;
        ctx.hook.pipelineStats?.trackFirstPresented(identity, windowId, key);
      };
      video.addEventListener('loadeddata', markDecoded, { once: true });
      video.addEventListener('playing', markDecoded, { once: true });
      // Use the numeric DOM readiness boundary rather than referencing the
      // `HTMLMediaElement` constructor, which is absent in Node test runners.
      if (video.readyState >= 2) markDecoded();
      if (typeof video.requestVideoFrameCallback === 'function') {
        video.requestVideoFrameCallback(() => markPresented());
      }
    }
    syncShareColorProfile(tile, video, participantMetadata, windowId);
    if (!isLocal) rebindSuspendedRemoteControl(identity, windowId, tile);
    if (windowId !== null && windowId !== undefined) {
      if (!isLocal) {
        cb.bindHoverTelepointer(tile);
        cb.ensureRemoteControlAffordance(tile);
        cb.publishViewerDemand(tile, 'open');
      }
      cb.renderTelepointersForWindow(windowId);
      cb.renderDrawForWindow(windowId, identity);
      // #875: the count pill's source of truth. A replacement SID for the
      // SAME window (share-tile reuse above) is a no-op here since the id is
      // already tracked -- the pill must not blip on a republish/quality
      // switch, matching the #679 "genuinely new" suppression just above.
      noteShareWindowAdded(identity, windowId);
    }
    ensureRemoteWindowHeader(tile, identity, isLocal, track, video, label, windowId, participantMetadata);
    maybeAutoSpotlightFirstShare(beforeShareCount, tile);
    notifyDrawAvailability();
  }

  function maybeAutoSpotlightFirstShare(beforeShareCount: number, tile: HTMLDivElement) {
    if (beforeShareCount !== 0 || cb.shareTileCount() !== 1) {
      cb.applyTileLayout();
      return;
    }
    cb.pinTile(tile, 'auto');
    logEvent('spotlighted first active screen share', 'ok');
  }

  /**
   * #785: the exit condition mirroring `maybeAutoSpotlightFirstShare`'s 0 -> 1
   * entry. Only the LAST share leaving restores; while any share is still on
   * screen the spotlight is still doing its job. A no-op unless an automatic
   * switch actually recorded something, so a user who chose spotlight
   * themselves stays there.
   */
  function restoreLayoutAfterLastShare() {
    if (cb.shareTileCount() !== 0) return;
    commitLayoutModeTransition(state, endAutoSpotlight(layoutModeStateOf(state)));
  }

  function finalizeShareTileRemoval(
    identity: string,
    tile: HTMLDivElement,
    windowId: number | null
  ) {
    if (tile instanceof HTMLDivElement) cb.publishViewerDemand(tile, 'closed');
    if (state.activeRemoteControl?.tileId === tile.id && windowId === null) {
      cb.stopRemoteControl('share ended');
    }
    if (tile?.id === state.pinnedTileId) state.pinnedTileId = null;
    if (tile instanceof HTMLDivElement) {
      destroyRemoteWindowHeader(tile);
      stopFullRangeRenderersForTile(tile);
      stopHoldLastFrameForTile(tile);
      disconnectTileNameChipObservers(tile);
    }
    withTileReflowAnimation(() => tile.remove());
    for (const [knownTrackKey, knownTile] of shareTilesByTrack) {
      if (knownTile === tile) shareTilesByTrack.delete(knownTrackKey);
    }
    if (windowId !== null) cb.removeTelepointersForWindow(windowId);
    if (windowId !== null) cb.removeDrawForWindow(windowId, identity);
    // #875: the tile is genuinely gone (not a pending-removal grace period --
    // that keeps the tile mounted and never reaches this function), so the
    // pill's source of truth drops it here, not in `removeShareTile`.
    if (windowId !== null) noteShareWindowRemoved(identity, windowId);
    restoreLayoutAfterLastShare();
    cb.applyTileLayout();
    notifyDrawAvailability();
  }

  function removeShareTile(identity: string, key: string) {
    const trackKey = `${identity}:${key}`;
    const tile = shareTilesByTrack.get(trackKey) ?? document.getElementById(shareTileId(identity, key));
    // A new SID has already taken over this stable window tile. The delayed
    // old unsubscribe is expected and must not tear down the replacement.
    if (tile instanceof HTMLDivElement && tile.dataset.trackSid !== key) {
      shareTilesByTrack.delete(trackKey);
      return;
    }
    if (!(tile instanceof HTMLDivElement)) return;
    const windowId = validWindowIdFromTile(tile);
    if (windowId === null) {
      shareTilesByTrack.delete(trackKey);
      finalizeShareTileRemoval(identity, tile, null);
      return;
    }

    const stableKey = stableShareKey(identity, windowId);
    const existing = pendingShareRemovals.get(stableKey);
    if (existing?.tile === tile && existing.trackKey === trackKey) return;
    if (existing) {
      clearTimeout(existing.timer);
      pendingShareRemovals.delete(stableKey);
    }

    // Keep the last rendered frame mounted briefly. Full reconnect can report
    // old-unsubscribe before replacement-subscribe; the replacement reclaims
    // this exact tile/video and cancels the timer (#298).
    shareTilesByTrack.delete(trackKey);
    suspendRemoteControlForRemovedShare(tile, identity, windowId);
    const pending: PendingShareRemoval = {
      identity,
      windowId,
      trackKey,
      tile,
      timer: setTimeout(() => {
        if (pendingShareRemovals.get(stableKey) !== pending) return;
        pendingShareRemovals.delete(stableKey);
        finalizeShareTileRemoval(identity, tile, windowId);
      }, shareReplacementGraceMs),
    };
    pendingShareRemovals.set(stableKey, pending);
  }

  function removeParticipantTiles(identity: string) {
    // A FULL reconnect replaces the participant SID while preserving its
    // identity. LiveKit emits old TrackUnsubscribed events and then
    // ParticipantDisconnected before the replacement TrackSubscribed. Keep
    // stable window-share surfaces on the same bounded replacement lease.
    document.querySelectorAll<HTMLDivElement>('.share-tile').forEach((tile) => {
      if (tile.dataset.owner !== identity) return;
      const windowId = validWindowIdFromTile(tile);
      const trackSid = tile.dataset.trackSid;
      if (windowId !== null && trackSid) removeShareTile(identity, trackSid);
    });
    // #820 (web-harness twin of e0cf46bc): don't kill remote control here on
    // event order alone -- a resume aftershock on the SHARER's end fires a
    // stale ParticipantDisconnected here too, and this used to unconditionally
    // override the grace suspend the loop above just established. Symptom:
    // host logged "dropping tokenless input ... no-active-request" right
    // after a still-valid grant. `suspendedRemoteControl` is the authoritative
    // "is this exact session currently grace-protected" signal.
    const active = state.activeRemoteControl;
    const activeSessionIsSuspended =
      active?.targetUserId === identity &&
      suspendedRemoteControl?.targetUserId === identity &&
      suspendedRemoteControl?.windowId === active.windowId;
    if (active?.targetUserId === identity && !activeSessionIsSuspended) {
      clearSuspendedRemoteControl();
      cb.stopRemoteControl('participant left');
    }
    withTileReflowAnimation(() => {
      document.querySelectorAll<HTMLElement>('.tile').forEach((tile) => {
        if (tile.dataset.owner !== identity) return;
        if (tile instanceof HTMLDivElement && tile.classList.contains('share-tile')) {
          const windowId = validWindowIdFromTile(tile);
          if (
            windowId !== null &&
            pendingShareRemovals.get(stableShareKey(identity, windowId))?.tile === tile
          ) {
            return;
          }
          cb.publishViewerDemand(tile, 'closed');
        }
        if (tile.id === state.pinnedTileId) state.pinnedTileId = null;
        destroyRemoteWindowHeader(tile);
        stopFullRangeRenderersForTile(tile);
      stopHoldLastFrameForTile(tile);
        stopHoldLastFrameForTile(tile);
        disconnectTileNameChipObservers(tile);
        tile.remove();
      });
    });
    Array.from(cameraDecodeHealthStates.keys()).forEach((key) => {
      if (key.startsWith(`${identity}:`)) cameraDecodeHealthStates.delete(key);
    });
    for (const [trackKey, tile] of shareTilesByTrack) {
      if (
        tile.dataset.owner === identity &&
        !Array.from(pendingShareRemovals.values()).some((pending) => pending.tile === tile)
      ) {
        shareTilesByTrack.delete(trackKey);
      }
    }
    cb.removeTelepointersForParticipant(identity);
    cb.removeDrawForParticipant(identity);
    updateParticipantCount();
    restoreLayoutAfterLastShare();
    cb.applyTileLayout();
    notifyDrawAvailability();
  }

  function clearTiles() {
    clearPendingShareRemovals();
    clearSuspendedRemoteControl();
    if (state.activeRemoteControl) cb.stopRemoteControl('share ended');
    document.querySelectorAll<HTMLElement>('.tile').forEach((tile) => {
      destroyRemoteWindowHeader(tile);
      stopFullRangeRenderersForTile(tile);
      stopHoldLastFrameForTile(tile);
      disconnectTileNameChipObservers(tile);
    });
    while (tilesEl.firstChild) tilesEl.firstChild.remove();
    cameraDecodeHealthStates.clear();
    shareTilesByTrack.clear();
    sharedWindowIdsByIdentity.clear();
    notifyDrawAvailability();
  }

  function setParticipantAudioActive(identity: string, active: boolean) {
    const tile = document.getElementById(baseTileId(identity));
    tile?.querySelector<HTMLElement>('.name-chip .audio-dot')?.classList.toggle('hidden', !active);
  }

  function updateParticipantCount() {
    const count = state.room ? 1 + state.room.remoteParticipants.size : 0;
    participantCountEl.textContent = String(
      Math.max(count, document.querySelectorAll('.tile:not(.share-tile)').length)
    );
    syncRemoteWindowHeaders();
  }

  function ensureFrameMetadataWorker(): Worker | null {
    if (state.frameMetadataWorker) return state.frameMetadataWorker;
    try {
      state.frameMetadataWorker = new Worker(
        new URL('livekit-client/frame-metadata-worker', import.meta.url),
        { type: 'module' }
      );
      return state.frameMetadataWorker;
    } catch (err) {
      logEvent(`frame metadata worker unavailable: ${(err as Error).message ?? err}`, 'warn');
      return null;
    }
  }

  function setPausedOverlay(tile: HTMLElement | null, paused: boolean) {
    if (!tile) return;
    tile.classList.toggle('stream-paused', paused);
    let overlay = tile.querySelector<HTMLDivElement>('.pause-overlay');
    if (paused && !overlay) {
      overlay = document.createElement('div');
      overlay.className = 'pause-overlay';
      overlay.innerHTML = '<span class="pause-bars" aria-hidden="true"><i></i><i></i></span><span>Video paused</span>';
      tile.appendChild(overlay);
    } else if (!paused) {
      overlay?.remove();
    }
  }

  function setPublicationPaused(
    participant: RemoteParticipant,
    pub: RemoteTrackPublication,
    paused: boolean,
    source = 'stream state'
  ) {
    if (pub.kind !== Track.Kind.Video) return;
    const tile = isCameraTrack(pub)
      ? document.getElementById(baseTileId(participant.identity))
      : currentShareTileForTrack(participant.identity, pub.trackSid);
    const wasPaused = tile?.classList.contains('stream-paused') ?? false;
    if (wasPaused === paused) return;
    setPausedOverlay(tile, paused);
    if (paused) logEvent(`${source}: ${participant.identity} / ${pub.trackName ?? '(unnamed)'} paused`, 'warn');
    else logEvent(`${source}: ${participant.identity} / ${pub.trackName ?? '(unnamed)'} resumed`, 'ok');
  }

  function publicationPaused(pub: RemoteTrackPublication): boolean {
    const streamState = String((pub.track as { streamState?: unknown } | undefined)?.streamState ?? '').toLowerCase();
    return streamState === String(Track.StreamState.Paused).toLowerCase() || streamState === 'paused';
  }

  function syncStreamStates(currentRoom: Room) {
    currentRoom.remoteParticipants.forEach((participant) => {
      participant.trackPublications.forEach((pub) => {
        const remotePub = pub as RemoteTrackPublication;
        if (remotePub.kind !== Track.Kind.Video || !remotePub.track) return;
        const paused = publicationPaused(remotePub);
        const tile = isCameraTrack(remotePub)
          ? document.getElementById(baseTileId(participant.identity))
          : currentShareTileForTrack(participant.identity, remotePub.trackSid);
        if (!tile) return;
        const wasPaused = tile.classList.contains('stream-paused');
        if (paused !== wasPaused) setPublicationPaused(participant, remotePub, paused, 'stream state poll');
      });
    });
  }

  ensureCameraDecodeHealthPoll();

  function updateParticipantShareColorProfiles(participant: RemoteParticipant) {
    participant.trackPublications.forEach((pub) => {
      const remotePub = pub as RemoteTrackPublication;
      if (remotePub.kind !== Track.Kind.Video || isCameraTrack(remotePub)) return;
      const tile = currentShareTileForTrack(participant.identity, remotePub.trackSid);
      if (!(tile instanceof HTMLDivElement)) return;
      const video = tile.querySelector<HTMLVideoElement>('video');
      if (!video) return;
      const windowId = validWindowIdFromTile(tile);
      syncShareColorProfile(tile, video, participant.metadata, windowId);
      const header = remoteWindowHeaders.get(tile);
      if (header && remotePub.track) {
        header.update({
          ctx,
          tile,
          ownerIdentity: participant.identity,
          ownerName: displayNameForParticipant(participant),
          isLocal: false,
          track: remotePub.track,
          video,
          windowId,
          sourceTitle: windowId !== null ? sharedWindowTitleFromMetadata(participant.metadata, windowId) : null,
          sourceUrl: windowId !== null ? sharedWindowUrlFromMetadata(participant.metadata, windowId) : null,
          onMinimizeWindow: () => {
            if (state.pinnedTileId === tile.id) state.pinnedTileId = null;
            commitLayoutModeTransition(state, dismissSpotlight(layoutModeStateOf(state)));
            cb.applyTileLayout();
          },
          onExpandWindow: () => cb.pinTile(tile, 'manual'),
        });
      }
    });
  }

  /** True when a subscribed video track is a camera feed (vs. a window share). */
  function isCameraTrack(pub: RemoteTrackPublication): boolean {
    const name = pub.trackName ?? '';
    // #657: `petal-ai-*` is the assistant's voice, never a camera and never a
    // shared window. Classified explicitly and FIRST so it can never fall
    // through to the `pub.source === Camera` guess below and surface as a
    // participant tile.
    if (isAiTrackName(name)) return false;
    if (name.startsWith('petal-camera-')) return true;
    if (name.startsWith('petal-window-')) return false;
    return pub.source === Track.Source.Camera;
  }

  function refreshParticipantGrid() {
    if (!state.room) {
      clearTiles();
      updateParticipantCount();
      cb.applyTileLayout();
      return;
    }
    ensureBaseTile(state.room.localParticipant.identity, true);
    state.room.remoteParticipants.forEach((p: RemoteParticipant) => {
      ensureBaseTile(p.identity, false);
    });
    updateParticipantCount();
    cb.applyTileLayout();
  }

  // #298: the receiver's own picture of what it is rendering, in the shape
  // `publicationReconcile` diffs against the SFU's authoritative publication
  // set. Read from the tile dataset so it reflects the DOM, not a parallel
  // cache that could itself be the thing that diverged.
  function trackedShareWindows(): TrackedShareWindow[] {
    const tracked: TrackedShareWindow[] = [];
    const seen = new Set<HTMLDivElement>();
    const localIdentity = state.room?.localParticipant.identity;
    for (const tile of shareTilesByTrack.values()) {
      if (seen.has(tile)) continue;
      seen.add(tile);
      const identity = tile.dataset.owner;
      if (identity === localIdentity) continue;
      const trackSid = tile.dataset.trackSid;
      const windowId = validWindowIdFromTile(tile);
      if (!identity || !trackSid || windowId === null) continue;
      tracked.push({ identity, windowId, trackSid });
    }
    return tracked;
  }

  return {
    ensureBaseTile,
    setTileCamera,
    clearTileCamera,
    addShareTile,
    removeShareTile,
    holdShareFrame,
    removeParticipantTiles,
    clearTiles,
    setParticipantAudioActive,
    updateParticipantCount,
    updateParticipantShareColorProfiles,
    refreshParticipantGrid,
    isCameraTrack,
    publicationPaused,
    setPublicationPaused,
    syncStreamStates,
    ensureFrameMetadataWorker,
    fitTileLabels,
    trackedShareWindows,
    syncRemoteWindowHeaders,
  };
}
