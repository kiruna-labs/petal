import { Track, VideoQuality, type RemoteTrackPublication } from 'livekit-client';
import type { HarnessContext } from './context';
import { containedMediaRect, type RectLike, type SizeLike, windowIdFromTrackName } from './telepointer';
import { VIEWER_DEMAND_TOPIC, type ViewerDemandMessage } from './trackNames';

const HEARTBEAT_MS = 2000;
const RESIZE_DEBOUNCE_MS = 150;
const encoder = new TextEncoder();

export function viewerDemandPixelGeometry(
  logicalWidth: number,
  logicalHeight: number,
  deviceScale: number
): { width: number; height: number; scale: number; pixelWidth: number; pixelHeight: number } {
  const scale = Number.isFinite(deviceScale) && deviceScale > 0
    ? Math.min(4, Math.max(0.5, deviceScale))
    : 1;
  const width = Math.max(0, Math.round(logicalWidth));
  const height = Math.max(0, Math.round(logicalHeight));
  return {
    width,
    height,
    scale,
    pixelWidth: Math.min(4096, Math.ceil(width * scale)),
    pixelHeight: Math.min(4096, Math.ceil(height * scale)),
  };
}

export function viewerDemandContainedMediaGeometry(
  bounds: RectLike,
  media: SizeLike,
  deviceScale: number
): { width: number; height: number; scale: number; pixelWidth: number; pixelHeight: number } {
  const content = containedMediaRect(bounds, media);
  return viewerDemandPixelGeometry(content.width, content.height, deviceScale);
}

/**
 * LiveKit treats requested dimensions as an upper bound when choosing a
 * simulcast layer. Petal publishes window shares at full and half resolution,
 * so a demand between those layers must be rounded up to full resolution or
 * the receiver would still upscale the half-resolution stream.
 */
export function viewerDemandSubscriptionGeometry(
  pixelWidth: number,
  pixelHeight: number,
  sourceDimensions?: SizeLike
): { width: number; height: number } {
  const requested = {
    width: Math.max(1, Math.ceil(pixelWidth)),
    height: Math.max(1, Math.ceil(pixelHeight)),
  };
  if (!sourceDimensions || sourceDimensions.width <= 0 || sourceDimensions.height <= 0) {
    return requested;
  }

  const source = {
    width: Math.ceil(sourceDimensions.width),
    height: Math.ceil(sourceDimensions.height),
  };
  const half = {
    width: Math.max(1, Math.floor(source.width / 2)),
    height: Math.max(1, Math.floor(source.height / 2)),
  };
  if (requested.width > half.width || requested.height > half.height) return source;
  return requested;
}

/**
 * The share's intrinsic media size, remembered across the gap in which a
 * republished track has not yet produced metadata (#627).
 *
 * `containedMediaRect` deliberately falls back to the raw element box when the
 * media size is unknown. For a share tile that fallback is a trap: a track swap
 * leaves `videoWidth === 0` for a few hundred ms, and the 150ms ResizeObserver
 * debounce below fires squarely inside that gap -- so the demand JUMPS UP to the
 * uncontained box size. The sender applies an increase immediately
 * (`session/share.rs` `ViewerDemandResolutionState::reconcile`), so that one
 * transient value instantly reverses a downsize it had just committed after a
 * 6s hold. Result: two full track republishes ~300ms apart, every ~8s, forever,
 * each blacking the tile.
 *
 * The element's box has not changed and the replacement track carries the same
 * aspect, so the last known media size is the correct value to keep using.
 * Never let an unknown media size reach `containedMediaRect` from here.
 */
export function shareMediaSize(
  tile: HTMLDivElement,
  video: { videoWidth?: number; videoHeight?: number } | null | undefined
): SizeLike {
  const width = video?.videoWidth ?? 0;
  const height = video?.videoHeight ?? 0;
  if (width > 0 && height > 0) {
    tile.dataset.shareMediaWidth = String(width);
    tile.dataset.shareMediaHeight = String(height);
    return { width, height };
  }
  const rememberedWidth = Number(tile.dataset.shareMediaWidth ?? 0);
  const rememberedHeight = Number(tile.dataset.shareMediaHeight ?? 0);
  if (rememberedWidth > 0 && rememberedHeight > 0) {
    return { width: rememberedWidth, height: rememberedHeight };
  }
  return { width: 0, height: 0 };
}

export function setupViewerDemand(ctx: HarnessContext) {
  const { state } = ctx;
  const observedTargets = new WeakSet<Element>();
  const resizeTimers = new WeakMap<Element, ReturnType<typeof setTimeout>>();
  const resizeObserver = typeof ResizeObserver === 'function'
    ? new ResizeObserver((entries) => {
        entries.forEach((entry) => {
          const tile = entry.target.closest?.('.share-tile');
          if (!(tile instanceof HTMLDivElement)) return;
          const prior = resizeTimers.get(entry.target);
          if (prior) clearTimeout(prior);
          resizeTimers.set(entry.target, setTimeout(() => {
            resizeTimers.delete(entry.target);
            publishViewerDemand(tile, 'heartbeat');
          }, RESIZE_DEBOUNCE_MS));
        });
      })
    : null;

  function syncResizeObservation(tile: HTMLDivElement, kind: ViewerDemandMessage['kind']) {
    if (!resizeObserver) return;
    const target = tile.querySelector('video') ?? tile;
    if (kind === 'closed') {
      resizeObserver.unobserve(target);
      observedTargets.delete(target);
      return;
    }
    if (!observedTargets.has(target)) {
      observedTargets.add(target);
      resizeObserver.observe(target);
    }
  }

  function nextSeq(): number {
    state.viewerDemandSeq =
      state.viewerDemandSeq >= Number.MAX_SAFE_INTEGER ? 1 : state.viewerDemandSeq + 1;
    return state.viewerDemandSeq;
  }

  function demandTargetFromTile(tile: HTMLDivElement): { targetUserId: string; windowId: number } | null {
    const targetUserId = tile.dataset.owner?.trim() ?? '';
    const windowId = Number(tile.dataset.windowId);
    if (!targetUserId || !Number.isSafeInteger(windowId) || windowId < 1 || windowId > 0xffff_ffff) return null;
    return { targetUserId, windowId };
  }

  function publishViewerDemand(tile: HTMLDivElement, kind: ViewerDemandMessage['kind']) {
    if (!state.room) return;
    syncResizeObservation(tile, kind);
    const target = demandTargetFromTile(tile);
    if (!target || target.targetUserId === state.room.localParticipant.identity) return;
    const video = tile.querySelector('video');
    const videoRect = video?.getBoundingClientRect();
    const rect = videoRect && videoRect.width > 0 && videoRect.height > 0
      ? videoRect
      : tile.getBoundingClientRect();
    const visible = kind !== 'closed' && rect.width > 0 && rect.height > 0 && tile.isConnected;
    const media = shareMediaSize(tile, video);
    const mediaKnown = media.width > 0 && media.height > 0;
    // A viewer that has never presented a frame has NO valid basis for a
    // resolution demand -- the element box is a layout artifact, not a display
    // size, and one box-sized packet is enough to make the sender republish
    // (raises used to apply instantly; see session/share.rs
    // ViewerDemandResolutionState). Send a presence-only demand (0x0 pixels):
    // it still counts as live demand for the quality/fps floor, but
    // contributes nothing to the sender's capture-resolution reconciler.
    const geometry = mediaKnown
      ? viewerDemandContainedMediaGeometry(rect, media, globalThis.devicePixelRatio)
      : viewerDemandPixelGeometry(0, 0, globalThis.devicePixelRatio);
    const message: ViewerDemandMessage = {
      v: 2,
      kind,
      targetUserId: target.targetUserId,
      viewerId: state.room.localParticipant.identity,
      windowId: target.windowId,
      seq: nextSeq(),
      visible,
      needsRepublish: false,
      ...geometry,
    };
    tile.dataset.viewerDemandPixelWidth = String(message.pixelWidth);
    tile.dataset.viewerDemandPixelHeight = String(message.pixelHeight);
    tile.dataset.viewerDemandScale = String(message.scale);
    if (visible) {
      const participant = state.room.remoteParticipants.get(target.targetUserId);
      const publication = participant
        ? Array.from(participant.trackPublications.values()).find((candidate) =>
            candidate.kind === Track.Kind.Video && candidate.trackName === `petal-window-${target.windowId}`
          ) as RemoteTrackPublication | undefined
        : undefined;
      if (publication && !mediaKnown) {
        // No frame yet: request the top layer from the SFU (mirrors the
        // native subscriber's pre-first-frame policy in viewer_demand.rs's
        // demand_pixel_dimensions) instead of deriving a dimension preference
        // from geometry we do not have. Subscriber-side only -- the sender
        // sees the presence-only packet above.
        if (typeof publication.setVideoQuality === 'function') {
          publication.setVideoQuality(VideoQuality.HIGH);
        }
      } else if (publication) {
        // LiveKit can briefly report `simulcasted === false` while the remote
        // publication is being established even when the sender advertised
        // multiple layers. The tile already has a subscribed video track at
        // this point, so send the dimension preference unconditionally; it is
        // harmless for a single-layer publication and avoids getting stuck on
        // the half-resolution layer for the lifetime of the tile.
        if (typeof publication.setVideoDimensions === 'function') {
          const subscription = viewerDemandSubscriptionGeometry(
            message.pixelWidth,
            message.pixelHeight,
            publication.dimensions
          );
          // `setVideoDimensions(sourceDimensions)` can still be interpreted as
          // a size ceiling by older LiveKit servers. Use the explicit HIGH
          // layer request when the smallest sufficient layer is the source;
          // smaller tiles retain dimension-based selection.
          if (
            publication.dimensions &&
            subscription.width === publication.dimensions.width &&
            subscription.height === publication.dimensions.height
          ) {
            publication.setVideoQuality(VideoQuality.HIGH);
            ctx.hook.pipelineStats?.trackViewerDemand(
              target.targetUserId,
              target.windowId,
              publication.trackSid,
              'high',
              message.pixelWidth,
              message.pixelHeight,
              subscription.width,
              subscription.height,
            );
          } else {
            publication.setVideoDimensions(subscription);
            ctx.hook.pipelineStats?.trackViewerDemand(
              target.targetUserId,
              target.windowId,
              publication.trackSid,
              'dimensions',
              message.pixelWidth,
              message.pixelHeight,
              subscription.width,
              subscription.height,
            );
          }
        } else if (publication.simulcasted) {
          publication.setVideoQuality(VideoQuality.HIGH);
          ctx.hook.pipelineStats?.trackViewerDemand(
            target.targetUserId,
            target.windowId,
            publication.trackSid,
            'high-fallback',
            message.pixelWidth,
            message.pixelHeight,
            message.pixelWidth,
            message.pixelHeight,
          );
        }
      }
    }
    state.room.localParticipant
      .publishData(encoder.encode(JSON.stringify(message)), {
        topic: VIEWER_DEMAND_TOPIC,
        reliable: true,
      })
      .catch(() => {});
  }

  /**
   * Send a PRESENCE-ONLY demand as soon as LiveKit announces a publication,
   * so the owner keeps the share at Full quality without waiting for
   * TrackSubscribed and a real tile (#299).
   *
   * This must never carry a resolution claim. TrackPublished fires for every
   * announcement -- including the re-announcement caused by the owner's OWN
   * republish -- and this packet is derived from nothing a viewer has
   * rendered. When it advertised viewport-sized pixels, it closed the 0.8.x
   * feedback loop: the owner commits a 6s-held downsize -> republish ->
   * TrackPublished -> viewport-sized demand ~300ms later -> instant upsize ->
   * second republish, phase-locked to the 2s heartbeat at a pair of
   * republishes every 8.0s, forever (the shipped-0.8.1 flicker; see the
   * 2026-07-30 session log, windows 8245 and 157). Zero pixels keep the
   * presence semantics (quality floor) and contribute nothing to the sender's
   * capture-resolution reconciler; exact geometry stays owned by the
   * tile-backed demand, and only once the tile has presented a frame.
   */
  function publishViewerDemandForPublication(ownerIdentity: string, publication: RemoteTrackPublication) {
    if (!state.room || publication.kind !== Track.Kind.Video) return;
    const windowId = windowIdFromTrackName(publication.trackName);
    if (windowId === null || ownerIdentity === state.room.localParticipant.identity) return;
    const geometry = viewerDemandPixelGeometry(
      0,
      0,
      typeof globalThis.devicePixelRatio === 'number' ? globalThis.devicePixelRatio : 1,
    );
    const message: ViewerDemandMessage = {
      v: 2,
      kind: 'open',
      targetUserId: ownerIdentity,
      viewerId: state.room.localParticipant.identity,
      windowId,
      seq: nextSeq(),
      visible: true,
      needsRepublish: false,
      ...geometry,
    };
    state.room.localParticipant
      .publishData(encoder.encode(JSON.stringify(message)), {
        topic: VIEWER_DEMAND_TOPIC,
        reliable: true,
      })
      .catch(() => {});
  }

  function publishViewerDemandForRemoteShares(kind: ViewerDemandMessage['kind']) {
    document.querySelectorAll<HTMLDivElement>('.share-tile[data-owner][data-window-id]').forEach((tile) => {
      publishViewerDemand(tile, kind);
    });
  }

  function startViewerDemandHeartbeat() {
    stopViewerDemandHeartbeat();
    state.viewerDemandTimer = setInterval(() => {
      publishViewerDemandForRemoteShares('heartbeat');
    }, HEARTBEAT_MS);
  }

  function stopViewerDemandHeartbeat() {
    if (state.viewerDemandTimer !== null) {
      clearInterval(state.viewerDemandTimer);
      state.viewerDemandTimer = null;
    }
  }

  return {
    publishViewerDemand,
    publishViewerDemandForPublication,
    startViewerDemandHeartbeat,
    stopViewerDemandHeartbeat,
  };
}
