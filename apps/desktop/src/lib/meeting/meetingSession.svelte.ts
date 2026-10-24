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
import { emit, listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { GalleryParticipant } from '#lib/components/Gallery.svelte';
import { session } from '#lib/stores/session.svelte.js';
import {
  currentRoom,
  joinRoom,
  leaveRoom,
  listRooms,
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
} from '#lib/data/rooms.js';
import { meetingDisplayLabelFromCredential } from '#lib/data/meetingCode.js';
import { findRoomRecord, meetingInviteAccessCode } from '#lib/data/inviteLinks.js';
import { consumePendingRoomDisplayName } from '#lib/data/pendingRoomLabels.js';
import {
  cameraTrackNameForIdentity,
  cameraWindowId,
  connectGalleryBridge,
  type GalleryBridge,
  type GalleryBridgeConfig,
  type GalleryBridgeSignals,
  type RemoteCamera
} from '#lib/data/galleryBridge.js';
import { COMMANDS, EVENTS, hasTauriBridge } from '#lib/ipc.js';
import type { PresentParticipant, ResilienceEvent, RoomLeftEvent } from '#lib/ipc.js';
import { createSpeakerSpotlight } from './speakerSpotlight.svelte';
import { clearMeetingRouteRoom, setMeetingRouteRoom } from '#lib/stores/meetingRoute.js';
import { joinFailureMessage } from '#lib/data/joinFailure.js';
import {
  GALLERY_BRIDGE_MAX_RESTARTS,
  galleryBridgeRestartDelayMs,
  rejoinedMessage,
  runBoundedRejoin
} from './connectionRecovery';
export type MeetingPhase = 'connecting' | 'connected' | 'disconnected';

export interface MeetingSessionOptions {
  /** The route param room name. */
  roomName: () => string;
  /** ?lkUrl/?lkToken query params for the browser-preview bridge fallback. */
  bridgeQueryParams: () => Pick<URLSearchParams, 'get'>;
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
  /** Public access code for the invite controls -- never the credential (#42). */
  readonly inviteAccessCode: string | null;
  readonly meetingPhase: MeetingPhase;
  readonly stillJoined: boolean;
  readonly presence: PresentParticipant[];
  readonly remoteCameras: RemoteCamera[];
  readonly galleryParticipants: GalleryParticipant[];
  readonly galleryStateTitle: string | null;
  readonly galleryStateDetail: string | null;
  /** A participant's collision-resolved meeting color (tiles, share headers, chat avatars). */
  resolvedColorFor(identity: string): string;
  readonly galleryStateTone: 'warning' | 'info';
  /** Buttons on the state card: a failed join offers Try again (#356). */
  readonly galleryStateActions: { label: string; onClick: () => void; primary?: boolean }[];
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
  // This route's saved record, read from rooms.json at mount. `join_room`
  // resolves only after the whole join (audio preparation alone can hold it
  // 45s, #787), and until then the invite controls had no access code for a
  // room the native side created or stored: "Invite link unavailable until
  // the access code is repaired." for a room whose record had one.
  let savedRoom = $state<RoomRecord | null>(null);
  const knownRoom = $derived(joinedRoom ?? savedRoom);
  const inviteAccessCode = $derived(meetingInviteAccessCode(joinedRoom, savedRoom, roomName));
  const pendingRouteDisplayName = $derived.by(() => consumePendingRoomDisplayName(roomName));
  // Never fall back to `roomName` — that's the raw credential (`room-<hash>`),
  // and showing it flashes a technical ID before the real name resolves (#42).
  // Until the joined room's real display name loads, show the friendly default.
  const safeRouteRoomLabel = $derived(
    pendingRouteDisplayName ?? meetingDisplayLabelFromCredential(roomName) ?? 'Petal meeting'
  );
  const roomLabel = $derived(knownRoom ? roomDisplayLabel(knownRoom) : safeRouteRoomLabel);

  let meetingPhase = $state<MeetingPhase>('connecting');
  let disconnectDetail = $state('Returning to rooms.');
  // #356: the join failed; the card explains why and offers Try again
  // instead of auto-returning to the room list.
  let joinFailed = $state(false);
  // A room-left that lands while a join is in flight (the room died during
  // the join tail, or a popover/pill Leave): replayed once the join resolves
  // so a dead room is never shown as connected.
  let pendingRoomLeft: RoomLeftEvent | null = null;
  // #354: set while the automatic rejoin after a lost connection runs.
  let reconnecting = $state<{ attempt: number; total: number } | null>(null);
  // Bumped per rejoin run; a run that sees a newer epoch (or dispose) stops.
  let recoveryEpoch = 0;
  // A Leave from another surface while the rejoin runs (#354).
  let rejoinCancelled = false;
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
    const generation = galleryBridgeGeneration;
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
      if (generation !== galleryBridgeGeneration || disposed) return;
      galleryBridgeDisconnectExpected = false;
      const bridge = await connectGalleryBridge(
        cfg,
        session.participantId,
        (cams) => {
          remoteCameras = cams;
        },
        handleGalleryBridgeSignals,
        () => {
          if (galleryBridgeDisconnectExpected) return;
          if (meetingPhase !== 'connected') return;
          // #354: the native room is the authority on membership. A lost
          // bridge only costs camera tiles -- reconnect it; if the native
          // room is gone too, its `room-left` drives the rejoin.
          restartGalleryBridgeSoon();
        }
      );
      // Stopped or disposed while connecting: drop the late bridge.
      if (generation !== galleryBridgeGeneration || disposed) {
        void bridge.disconnect().catch(() => {});
        return;
      }
      galleryBridge = bridge;
      resetGalleryBridgeRestarts();
    } catch (e) {
      // Non-fatal: the meeting works without in-tile remote video (tiles
      // fall back to the camera-off state); never block join on the bridge.
      console.warn('gallery bridge unavailable', e);
      if (galleryBridgeRestarts > 0 && meetingPhase === 'connected') restartGalleryBridgeSoon();
    }
  }

  // Each restart mints a token; the backend allows 20/min per IP, shared
  // with native rejoins on the same NAT -- so back off and cap (#354).
  let galleryBridgeRestartTimer: ReturnType<typeof setTimeout> | undefined;
  let galleryBridgeRestarts = 0;
  let galleryBridgeGeneration = 0;
  function resetGalleryBridgeRestarts() {
    galleryBridgeRestarts = 0;
  }
  function restartGalleryBridgeSoon() {
    stopGalleryBridge();
    clearTimeout(galleryBridgeRestartTimer);
    if (galleryBridgeRestarts >= GALLERY_BRIDGE_MAX_RESTARTS) {
      console.warn('gallery bridge: giving up after repeated restarts');
      return;
    }
    const delayMs = galleryBridgeRestartDelayMs(galleryBridgeRestarts);
    galleryBridgeRestarts += 1;
    galleryBridgeRestartTimer = setTimeout(() => {
      if (!disposed && meetingPhase === 'connected') void startGalleryBridge();
    }, delayMs);
  }

  function stopGalleryBridge() {
    galleryBridgeGeneration += 1;
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
    const participants = new Map<string, { identity: string; paletteIndex?: number | null }>();
    for (const cam of remoteCameras) participants.set(cam.identity, { identity: cam.identity });
    // #317: presence carries the chosen palette index, so it wins over a camera-only entry.
    for (const p of presence) participants.set(p.identity, { identity: p.identity, paletteIndex: paletteIndexFor(p.identity) });
    return Array.from(participants.values());
  });
  const resolvedColorsByIdentity = $derived.by(() =>
    resolveMeetingColors(colorResolutionParticipants)
  );

  // #317: the chosen palette index from presence; our own entry falls back to
  // the locally chosen color until the metadata echo lands.
  function paletteIndexFor(identity: string): number | null | undefined {
    const entry = presence.find((p) => p.identity === identity);
    if (entry?.paletteIndex != null) return entry.paletteIndex;
    const isLocal = entry ? entry.isLocal : identity === session.participantId;
    return isLocal ? paletteIndexForIdentityColor(session.identity) : null;
  }

  function resolvedColorFor(identity: string): string {
    return (
      resolvedColorsByIdentity.get(identity) ??
      identityColorCss(colorForIdentity(identity, paletteIndexFor(identity)))
    );
  }

  // #875: ink (foreground text/icon color) for the identity-tinted share
  // pill. `resolveMeetingColors` only resolves a BACKGROUND (with
  // collision-variant hue shifts); ink doesn't need to track those variants
  // to stay legible, so this derives straight from the identity's base
  // palette color, same as the local share button's own
  // `identityInkCss(session.identity)` in the route.
  function resolvedInkFor(identity: string): string {
    return identityInkCss(colorForIdentity(identity, paletteIndexFor(identity)));
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
    reconnecting
      ? 'Connection lost'
      : meetingPhase === 'connecting'
        ? null
        : meetingPhase === 'disconnected'
          ? joinFailed
            ? "Couldn't join this meeting"
            : 'Disconnected'
          : null
  );
  const galleryStateActions = $derived(
    joinFailed && meetingPhase === 'disconnected'
      ? [
          { label: 'Try again', onClick: () => void retryJoin(), primary: true },
          { label: 'Back to rooms', onClick: () => returnToRooms(0) }
        ]
      : []
  );
  const galleryStateDetail = $derived(
    reconnecting
      ? reconnecting.attempt > 0
        ? `Reconnecting… (attempt ${reconnecting.attempt} of ${reconnecting.total})`
        : 'Reconnecting…'
      : meetingPhase === 'connecting'
        ? null
        : meetingPhase === 'disconnected'
          ? disconnectDetail
          : null
  );
  const galleryStateTone = $derived<'warning' | 'info'>(
    reconnecting || meetingPhase === 'disconnected' ? 'warning' : 'info'
  );
  const activeIdentity = $derived.by(() => {
    const identity =
      spotlight.activeSpeakerIdentity ?? presence.find((p) => p.isLocal)?.identity ?? session.participantId;
    return colorForIdentity(identity, paletteIndexFor(identity));
  });
  const activeColor = $derived(
    resolvedColorFor(
      spotlight.activeSpeakerIdentity ?? presence.find((p) => p.isLocal)?.identity ?? session.participantId
    )
  );

  /** Returns true if it redirected to the canonical room route (caller should
   * abort the rest of its mount, matching the original inline behavior). */
  async function join(): Promise<boolean> {
    // Not awaited: a local file read that must never delay the join itself.
    void loadSavedRoom();
    await startListeners();
    return attemptJoin();
  }

  async function retryJoin() {
    if (!joinFailed || disposed) return;
    joinFailed = false;
    pendingRoomLeft = null;
    meetingPhase = 'connecting';
    await attemptJoin();
  }

  async function attemptJoin(): Promise<boolean> {
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
        await goto(`/meeting/${encodeURIComponent(joinedRoom.name)}`, { replace: true });
        return true;
      }
      const snapshotRevision = presenceRevision;
      const snapshot = await roomPresence();
      if (snapshotRevision === presenceRevision) presence = snapshot;
      const stashed = pendingRoomLeft;
      pendingRoomLeft = null;
      if (stashed) {
        handleRoomLeft(stashed);
        return false;
      }
      meetingPhase = 'connected';
    } catch (e) {
      console.error(`Failed to join room '${roomName}'`, e);
      pendingRoomLeft = null;
      // #356: say why (server unreachable, firewall-blocked socket, ...)
      // and let the user retry, instead of a 900ms generic flash.
      disconnectDetail = joinFailureMessage(e);
      joinFailed = true;
      meetingPhase = 'disconnected';
      return false;
    }

    // Without this the continuation of an awaited join connects a SECOND
    // hidden bridge participant after teardown, which nothing disconnects.
    if (disposed) return false;

    // Remote camera tiles (issue #26) — after the native join so the
    // room exists; non-fatal if it can't connect.
    void startGalleryBridge();
    return false;
  }

  async function loadSavedRoom() {
    try {
      const room = findRoomRecord(await listRooms(), roomName);
      if (!disposed) savedRoom = room;
    } catch {
      // No Tauri bridge (plain browser preview) -- the joined record still arrives.
    }
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
          // A Leave from another surface (popover/pill): never rejoin (#354).
          if (event.payload.userLeave) {
            rejoinCancelled = true;
            beginTerminalReturn('You left the meeting - returning you to the room list.');
            return;
          }
          // Mid-rejoin: a failed attempt's own cleanup emits room-left before
          // its invoke rejects (cleared when the next attempt starts); one
          // that lands after a successful attempt is replayed by the rejoin.
          if (reconnecting) {
            pendingRoomLeft = event.payload;
            return;
          }
          // The failure card owns the screen after a failed join (#356).
          if (joinFailed) return;
          // Mid-join: a failing join's own cleanup emits room-left before
          // the invoke rejects. Stash it; attemptJoin replays it on success.
          if (meetingPhase === 'connecting') {
            pendingRoomLeft = event.payload;
            return;
          }
          handleRoomLeft(event.payload);
        }
      });
      setMeetingRouteRoom(roomName);
    } catch {
      // No Tauri bridge (plain browser preview) — `listen` rejects; without
      // this guard the rejection aborts the rest of onMount.
    }
  }

  function handleRoomLeft(event: RoomLeftEvent) {
    // #354: a lost connection is not the meeting ending -- rejoin.
    if (event.connectionLost) {
      void recoverLostConnection(event.stoppedShareCount ?? 0);
      return;
    }
    beginTerminalReturn('Meeting ended - returning you to the room list.');
  }

  function showResilienceToast(event: ResilienceEvent) {
    // ToastHost renders `resilience-event` from either side; the native side
    // leaves a rejoinable loss to this controller (#354).
    void emit(EVENTS.resilienceEvent, event).catch(() => {});
  }

  async function recoverLostConnection(stoppedShareCount: number) {
    if (reconnecting) return;
    const epoch = ++recoveryEpoch;
    const stale = () => disposed || epoch !== recoveryEpoch;
    rejoinCancelled = false;
    pendingRoomLeft = null;
    meetingPhase = 'connecting';
    clearTimeout(galleryBridgeRestartTimer);
    stopGalleryBridge();
    presence = [];
    reconnecting = { attempt: 0, total: 0 };
    showResilienceToast({ kind: 'reconnecting' });
    const outcome = await runBoundedRejoin({
      join: async () => {
        joinedRoom = await joinRoom(
          roomName,
          session.participantId,
          session.name || 'Guest',
          session.remoteControlPolicy,
          paletteIndexForIdentityColor(session.identity)
        );
      },
      // Room-scoped: never tear down a different room the user joined since.
      leave: async () => {
        if ((await currentRoom()) === roomName) await leaveRoom();
      },
      sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
      cancelled: () => stale() || selfLeaveRequested || rejoinCancelled,
      onAttempt: (attempt, total) => {
        // Anything stashed so far came from the previous attempt's cleanup.
        pendingRoomLeft = null;
        reconnecting = { attempt, total };
      },
      onAttemptFailed: (attempt, total, error) => {
        console.warn(`rejoin attempt ${attempt}/${total} failed`, error);
      }
    });
    if (stale()) return;
    if (outcome.kind === 'cancelled') {
      reconnecting = null;
      showResilienceToast({ kind: 'reconnectAbandoned' });
      return;
    }
    if (outcome.kind === 'failed') {
      reconnecting = null;
      pendingRoomLeft = null;
      showResilienceToast({ kind: 'disconnected', reason: 'rejoinFailed' });
      beginTerminalReturn(
        "Lost the connection to this meeting and couldn't reconnect. Check your network, then rejoin from the room list.",
        6000
      );
      return;
    }
    try {
      const snapshotRevision = presenceRevision;
      const snapshot = await roomPresence();
      if (!stale() && snapshotRevision === presenceRevision) presence = snapshot;
    } catch {
      // presence-update events repopulate the roster anyway.
    }
    if (stale()) return;
    // The rejoined room died (or was left) while we finished up: replay it
    // instead of showing a dead room as connected.
    const stashed = pendingRoomLeft;
    pendingRoomLeft = null;
    reconnecting = null;
    if (stashed) {
      handleRoomLeft(stashed);
      return;
    }
    meetingPhase = 'connected';
    resetGalleryBridgeRestarts();
    void startGalleryBridge();
    showResilienceToast({ kind: 'reconnected', message: rejoinedMessage(stoppedShareCount) });
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
    clearMeetingRouteRoom(roomName);
    if (joinReturnTimer) clearTimeout(joinReturnTimer);
    clearTimeout(galleryBridgeRestartTimer);
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
    get inviteAccessCode() {
      return inviteAccessCode;
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
    resolvedColorFor,
    get galleryStateTone() {
      return galleryStateTone;
    },
    get galleryStateActions() {
      return galleryStateActions;
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
