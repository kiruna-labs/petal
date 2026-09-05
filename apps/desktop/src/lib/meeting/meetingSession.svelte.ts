// Meeting session controller: join/leave, presence, remote cameras, the
// gallery bridge, meeting phase, roster rename, and the derived gallery model.
//
// Extracted verbatim from /meeting/[room]/+page.svelte. Real join/leave
// (SPEC.md §4.6, idempotent on the Rust side), real presence (feeds gallery
// tiles + counts), and the gallery bridge (issue #26 — a hidden
// subscribe-only LiveKit participant in this webview for remote camera tiles).
// Zero behavior change.

import { goto } from '$app/navigation';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { GalleryParticipant } from '$lib/components/Gallery.svelte';
import { session } from '$lib/stores/session.svelte';
import {
  joinRoom,
  leaveRoom,
  roomPresence,
  colorForIdentity,
  identityColorCss,
  identityInkCss,
  paletteIndexForIdentityColor,
  resolveMeetingColors,
  renameRoom,
  roomDisplayLabel,
  type PresenceUpdate,
  type RoomRecord
} from '$lib/data/rooms';
import { meetingDisplayLabelFromCredential } from '$lib/data/meetingCode';
import { consumePendingRoomDisplayName } from '$lib/data/pendingRoomLabels';
import {
  cameraTrackNameForIdentity,
  cameraWindowId,
  connectGalleryBridge,
  type GalleryBridge,
  type GalleryBridgeConfig,
  type GalleryBridgeSignals,
  type RemoteCamera
} from '$lib/data/galleryBridge';
import { COMMANDS, EVENTS, hasTauriBridge } from '$lib/ipc';
import type { PresentParticipant, RoomLeftEvent } from '$lib/ipc';
import { createSpeakerSpotlight } from './speakerSpotlight.svelte';
export type MeetingPhase = 'connecting' | 'connected' | 'disconnected';

export interface MeetingSessionOptions {
  /** The route param room name. */
  roomName: () => string;
  /** ?lkUrl/?lkToken query params for the browser-preview bridge fallback. */
  bridgeQueryParams: () => URLSearchParams;
  /** Live route-owned state that feeds the derived gallery model. */
  micMuted: () => boolean;
  shareActive: () => boolean;
  /** #875: count of the LOCAL participant's own currently-shared windows
   * (not just the shareActive boolean) -- feeds the local tile's
   * non-interactive count pill. The route derives this from the same
   * `sharedWindowIds` command that already backs `shareActive`. */
  localShareCount: () => number;
  localCameraStream: () => MediaStream | null;
  /** Best-effort restore of the /main window geometry before the route
   * swap back (Tauri only); the meeting route passes the pill's
   * restoreHomeWindow. Gallery mode only — pill mode must not pre-restore
   * (growing a transparent pill window is itself a desktop flash). */
  prepareReturnToHome?: () => Promise<void>;
}

export interface MeetingSession {
  readonly joinedRoom: RoomRecord | null;
  readonly roomLabel: string;
  readonly meetingPhase: MeetingPhase;
  readonly stillJoined: boolean;
  readonly presence: PresentParticipant[];
  readonly remoteCameras: RemoteCamera[];
  readonly galleryParticipants: GalleryParticipant[];
  readonly galleryStateTitle: string | null;
  readonly galleryStateDetail: string | null;
  readonly galleryStateTone: 'warning' | 'info';
  readonly activeIdentity: ReturnType<typeof colorForIdentity>;
  readonly activeColor: string;
  handleRenameRoom(displayName: string | null): Promise<void>;
  handleLeave(): Promise<void>;
  /** Run the real join on mount; resolves to true if it redirected to the
   * canonical room route (caller should abort the rest of its mount). */
  join(): Promise<boolean>;
  /** Tear down listeners, bridge, and all timers (call from onDestroy). */
  dispose(): void;
}

export function createMeetingSession(options: MeetingSessionOptions): MeetingSession {
  const hasTauri = hasTauriBridge();
  const spotlight = createSpeakerSpotlight();

  const roomName = $derived(options.roomName());

  let joinedRoom = $state<RoomRecord | null>(null);
  const pendingRouteDisplayName = $derived.by(() => consumePendingRoomDisplayName(roomName));
  // Never fall back to `roomName` — that's the raw credential (`room-<hash>`),
  // and showing it flashes a technical ID before the real name resolves (#42).
  // Until the joined room's real display name loads, show the friendly default.
  const safeRouteRoomLabel = $derived(
    pendingRouteDisplayName ?? meetingDisplayLabelFromCredential(roomName) ?? 'Petal meeting'
  );
  const roomLabel = $derived(joinedRoom ? roomDisplayLabel(joinedRoom) : safeRouteRoomLabel);

  let meetingPhase = $state<MeetingPhase>('connecting');
  let disconnectDetail = $state('Returning to rooms.');
  let presence = $state<PresentParticipant[]>([]);
  let presenceRevision = 0;
  let remoteCameras = $state<RemoteCamera[]>([]);
  let bridgeSharingIdentities = $state<string[]>([]);
  let weakConnectionIdentities = $state<string[]>([]);
  let staleCameraIdentities = $state<string[]>([]);
  // #875: remote window-share counts per identity, straight from the
  // bridge's raw publication tracking (display shares and viewer-hidden
  // windows included).
  let windowShareCounts = $state<Record<string, number>>({});

  let galleryBridge: GalleryBridge | null = null;
  let galleryBridgeDisconnectExpected = false;
  // #782: set by dispose(). The route can now be unmounted by a client-side
  // navigation instead of a full reload, so an in-flight join() outlives its
  // own teardown and must not resurrect anything after it.
  let disposed = false;
  // Set the moment the USER asks to leave from this route (pill Leave
  // circle). `leave_room` emits `room-left` (session/room.rs), which the
  // listener below maps to the terminal 'Disconnected' warning card —
  // correct for an externally-ended meeting, but for the user's own Leave it
  // made the card flash for up to 900ms before the direct return (UX: the
  // yellow box was visible only as a flicker). The guard lets handleLeave's
  // own prepareReturnToHome + goto do the return silently.
  let selfLeaveRequested = false;

  let unlistenPresence: UnlistenFn | undefined;
  let unlistenRoomLeft: UnlistenFn | undefined;

  let joinReturnTimer: ReturnType<typeof setTimeout> | undefined;

  function returnToRooms(returnDelayMs = 900) {
    if (joinReturnTimer) clearTimeout(joinReturnTimer);
    joinReturnTimer = setTimeout(async () => {
      await options.prepareReturnToHome?.();
      goto('/main');
    }, returnDelayMs);
  }

  function beginTerminalReturn(detail: string, returnDelayMs = 900) {
    disconnectDetail = detail;
    meetingPhase = 'disconnected';
    returnToRooms(returnDelayMs);
  }

  function handleGalleryBridgeSignals(signals: GalleryBridgeSignals) {
    bridgeSharingIdentities = signals.sharingIdentities;
    weakConnectionIdentities = signals.weakConnectionIdentities;
    staleCameraIdentities = signals.staleCameraIdentities;
    windowShareCounts = signals.windowShareCounts;
    spotlight.updateActiveSpeaker(signals.activeSpeakers);
  }

  async function handleRenameRoom(displayName: string | null) {
    const cleaned = displayName?.trim() ?? '';
    const nextDisplayName = cleaned && cleaned !== roomName ? cleaned : null;
    try {
      joinedRoom = await renameRoom(joinedRoom?.name ?? roomName, nextDisplayName);
    } catch (e) {
      console.error('Failed to rename room', e);
    }
  }

  async function startGalleryBridge() {
    try {
      let cfg: Pick<GalleryBridgeConfig, 'url' | 'token'> | null = null;
      if (hasTauri) {
        cfg = await invoke<GalleryBridgeConfig>(COMMANDS.galleryBridgeConfig, {
          roomName,
          identity: session.participantId
        });
      } else {
        // Plain-browser preview/testing affordance (no Tauri backend to mint
        // a token): accept ?lkUrl=&lkToken= query params, same spirit as this
        // route's other browser-preview fallbacks. Harmless in the real app
        // (hasTauri short-circuits).
        const params = options.bridgeQueryParams();
        const url = params.get('lkUrl');
        const token = params.get('lkToken');
        if (url && token) cfg = { url, token };
      }
      if (!cfg) return;
      galleryBridgeDisconnectExpected = false;
      galleryBridge = await connectGalleryBridge(
        cfg,
        session.participantId,
        (cams) => {
          remoteCameras = cams;
        },
        handleGalleryBridgeSignals,
        () => {
          if (galleryBridgeDisconnectExpected) return;
          if (meetingPhase !== 'connected') return;
          beginTerminalReturn('Connection lost - returning you to the room list.', 1400);
        }
      );
    } catch (e) {
      // Non-fatal: the meeting works without in-tile remote video (tiles
      // fall back to the camera-off state); never block join on the bridge.
      console.warn('gallery bridge unavailable', e);
    }
  }

  function stopGalleryBridge() {
    const bridge = galleryBridge;
    galleryBridge = null;
    galleryBridgeDisconnectExpected = true;
    remoteCameras = [];
    bridgeSharingIdentities = [];
    weakConnectionIdentities = [];
    staleCameraIdentities = [];
    windowShareCounts = {};
    spotlight.reset();
    void bridge?.disconnect().catch(() => {});
  }

  const colorResolutionParticipants = $derived.by(() => {
    const participants = new Map<string, { identity: string }>();
    for (const p of presence) participants.set(p.identity, { identity: p.identity });
    for (const cam of remoteCameras) participants.set(cam.identity, { identity: cam.identity });
    return Array.from(participants.values());
  });
  const resolvedColorsByIdentity = $derived.by(() =>
    resolveMeetingColors(colorResolutionParticipants)
  );

  function resolvedColorFor(identity: string): string {
    return resolvedColorsByIdentity.get(identity) ?? identityColorCss(colorForIdentity(identity));
  }

  // #875: ink (foreground text/icon color) for the identity-tinted share
  // pill. `resolveMeetingColors` only resolves a BACKGROUND (with
  // collision-variant hue shifts); ink doesn't need to track those variants
  // to stay legible, so this derives straight from the identity's base
  // palette color, same as the local share button's own
  // `identityInkCss(session.identity)` in the route.
  function resolvedInkFor(identity: string): string {
    return identityInkCss(colorForIdentity(identity));
  }

  const galleryParticipants = $derived.by<GalleryParticipant[]>(() => {
    const localMicMuted = options.micMuted();
    const camerasById = new Map(remoteCameras.map((c) => [c.identity, c]));
    const speaking = new Set([
      ...spotlight.speakingIdentities,
      ...presence.filter((participant) => participant.speaking).map((participant) => participant.identity)
    ]);
    const sharing = new Set(bridgeSharingIdentities);
    const weak = new Set(weakConnectionIdentities);
    // #247: a stale (locally-detected-frozen) camera falls back to the
    // existing camera-off tile treatment, same as no stream at all --
    // holding the last decoded frame forever with no indication is exactly
    // the gap this watchdog closes, independent of whether the SFU/
    // ParticipantDisconnected has said anything yet.
    const stale = new Set(staleCameraIdentities);
    const activeSpeakerIdentity = spotlight.activeSpeakerIdentity;
    const shareActive = options.shareActive();
    const localShareCount = options.localShareCount();
    const localCameraStream = options.localCameraStream();
    const tiles: GalleryParticipant[] = presence.map((p) => {
      // A direct WebView preview remains authoritative where the platform
      // requires one (macOS). Windows falls back to the native publication
      // subscribed through the hidden gallery bridge, so Media Foundation
      // stays the sole camera owner.
      const camera = camerasById.get(p.identity);
      // Local self-view is always the direct preview (native-fed canvas
      // stream on Windows, getUserMedia on macOS); the bridge never
      // subscribes our own publication, so a
      // local participant never has a bridge camera.
      const stream = (p.isLocal ? localCameraStream : camera?.stream) ?? undefined;
      const usesBridgeCamera = !p.isLocal || !localCameraStream;
      const isStale = usesBridgeCamera && stale.has(p.identity);
      // #875: local tile shows its OWN shared-window count (not just the
      // shareActive boolean); remote tiles get the raw publication count
      // from the bridge's window-share tracking.
      const shareCount = p.isLocal ? localShareCount : (windowShareCounts[p.identity] ?? 0);
      return {
        id: p.identity,
        name: p.isLocal ? `${p.name} (you)` : p.name,
        videoOn: !!stream && !isStale,
        videoStream: isStale ? undefined : stream,
        drawWindowId: stream
          ? p.isLocal
            ? cameraWindowId(cameraTrackNameForIdentity(p.identity))
            : camera?.drawWindowId
          : undefined,
        mirrored: p.isLocal, // self-view only; remote streams render unmirrored
        speaking:
          speaking.has(p.identity) && !(p.isLocal ? localMicMuted : p.micMuted),
        activeSpeaker: p.identity === activeSpeakerIdentity,
        muted: p.isLocal ? localMicMuted : p.micMuted,
        weakConnection: weak.has(p.identity),
        isLocal: p.isLocal,
        // Native shares are compositor windows, not gallery tiles. Marking
        // the publisher here lets Gallery spotlight that participant's webcam
        // while the actual shared window remains independently movable.
        sharing: (p.isLocal && shareActive) || sharing.has(p.identity),
        shareCount,
        sharingLiveBackground: resolvedColorFor(p.identity),
        sharingLiveColor: resolvedInkFor(p.identity)
      };
    });
    // Union: a camera stream whose publisher isn't in presence (yet) still
    // gets a tile — presence (native events) and the bridge (livekit-client)
    // are independent feeds that can race by a beat on join.
    const present = new Set(presence.map((p) => p.identity));
    for (const cam of remoteCameras) {
      if (!present.has(cam.identity)) {
        const isStale = stale.has(cam.identity);
        tiles.push({
          id: cam.identity,
          name: cam.name,
          videoOn: !isStale,
          videoStream: isStale ? undefined : cam.stream,
          drawWindowId: isStale ? undefined : cam.drawWindowId,
          speaking: speaking.has(cam.identity),
          activeSpeaker: cam.identity === activeSpeakerIdentity,
          weakConnection: weak.has(cam.identity),
          sharing: sharing.has(cam.identity),
          shareCount: windowShareCounts[cam.identity] ?? 0,
          sharingLiveBackground: resolvedColorFor(cam.identity),
          sharingLiveColor: resolvedInkFor(cam.identity)
        });
      }
    }
    return tiles;
  });

  const galleryStateTitle = $derived(
    meetingPhase === 'connecting'
      ? null
      : meetingPhase === 'disconnected'
        ? 'Disconnected'
        : null
  );
  const galleryStateDetail = $derived(
    meetingPhase === 'connecting'
      ? null
      : meetingPhase === 'disconnected'
        ? disconnectDetail
        : null
  );
  const galleryStateTone = $derived<'warning' | 'info'>(
    meetingPhase === 'disconnected' ? 'warning' : 'info'
  );
  const activeIdentity = $derived(
    colorForIdentity(
      spotlight.activeSpeakerIdentity ?? presence.find((p) => p.isLocal)?.identity ?? session.participantId
    )
  );
  const activeColor = $derived(
    resolvedColorFor(
      spotlight.activeSpeakerIdentity ?? presence.find((p) => p.isLocal)?.identity ?? session.participantId
    )
  );

  /** Returns true if it redirected to the canonical room route (caller should
   * abort the rest of its mount, matching the original inline behavior). */
  async function join(): Promise<boolean> {
    await startListeners();
    try {
      // Real join (SPEC.md §4.6): idempotent on the Rust side, so a reload
      // of this route never duplicates membership.
      const remoteControlPolicy = session.remoteControlPolicy;
      joinedRoom = await joinRoom(
        roomName,
        session.participantId,
        session.name || 'Guest',
        remoteControlPolicy,
        paletteIndexForIdentityColor(session.identity)
      );
      if (joinedRoom.name !== roomName) {
        await goto(`/meeting/${encodeURIComponent(joinedRoom.name)}`, { replaceState: true });
        return true;
      }
      const snapshotRevision = presenceRevision;
      const snapshot = await roomPresence();
      if (snapshotRevision === presenceRevision) presence = snapshot;
      meetingPhase = 'connected';
    } catch (e) {
      console.error(`Failed to join room '${roomName}'`, e);
      beginTerminalReturn('Could not join this room - returning you to the room list.');
    }

    // Without this the continuation of an awaited join connects a SECOND
    // hidden bridge participant after teardown, which nothing disconnects.
    if (disposed) return false;

    // Remote camera tiles (issue #26) — after the native join so the
    // room exists; non-fatal if it can't connect.
    void startGalleryBridge();
    return false;
  }

  async function startListeners() {
    try {
      unlistenPresence = await listen<PresenceUpdate>(EVENTS.presenceUpdate, (event) => {
        if (event.payload.roomName === roomName) {
          presenceRevision += 1;
          presence = event.payload.participants;
        }
      });

      // Leave triggered from OUTSIDE this route (menubar popover Leave /
      // pill leave circle -- session.rs emits `room-left`): navigate home.
      // Room-name-guarded so leaving room A while joining room B doesn't
      // yank the new meeting back to /main.
      unlistenRoomLeft = await listen<RoomLeftEvent>(EVENTS.roomLeft, (event) => {
        if (event.payload.roomName === roomName) {
          // The user's own Leave already navigates via handleLeave; showing
          // the terminal card here would flash it for the leave duration.
          if (selfLeaveRequested) return;
          beginTerminalReturn('Meeting ended - returning you to the room list.');
        }
      });
    } catch {
      // No Tauri bridge (plain browser preview) — `listen` rejects; without
      // this guard the rejection aborts the rest of onMount.
    }
  }

  async function handleLeave() {
    // Mark BEFORE leaveRoom(): the Rust side emits `room-left` (session/
    // room.rs) while the command runs, and the listener must see the guard
    // set or it will paint the 'Disconnected' card for the leave.
    selfLeaveRequested = true;
    try {
      await leaveRoom();
    } catch (e) {
      console.error('Failed to leave room', e);
    }
    await options.prepareReturnToHome?.();
    goto('/main');
  }

  function dispose() {
    disposed = true;
    unlistenPresence?.();
    unlistenRoomLeft?.();
    if (joinReturnTimer) clearTimeout(joinReturnTimer);
    spotlight.dispose();
    // Tear down the bridge's duplicate subscriptions (issue #26).
    stopGalleryBridge();
  }

  return {
    get joinedRoom() {
      return joinedRoom;
    },
    get roomLabel() {
      return roomLabel;
    },
    get meetingPhase() {
      return meetingPhase;
    },
    get stillJoined() {
      return !selfLeaveRequested && meetingPhase !== 'disconnected';
    },
    get presence() {
      return presence;
    },
    get remoteCameras() {
      return remoteCameras;
    },
    get galleryParticipants() {
      return galleryParticipants;
    },
    get galleryStateTitle() {
      return galleryStateTitle;
    },
    get galleryStateDetail() {
      return galleryStateDetail;
    },
    get galleryStateTone() {
      return galleryStateTone;
    },
    get activeIdentity() {
      return activeIdentity;
    },
    get activeColor() {
      return activeColor;
    },
    handleRenameRoom,
    handleLeave,
    join,
    dispose
  };
}
