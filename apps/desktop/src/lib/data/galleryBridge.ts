// In-webview gallery video bridge (issue #26), JS side.
//
// The native LiveKit connection decodes remote media into the COMPOSITOR's
// native windows (zero-copy CVPixelBuffer path) -- it cannot feed webview
// <video> elements. So the meeting route joins the same LiveKit room a
// second time from inside the webview, as a HIDDEN, SUBSCRIBE-ONLY
// participant (token minted by src-tauri/src/gallery_bridge.rs -- see its
// module doc for the full mechanism decision + double-subscribe cost
// analysis), using livekit-client the same way the web client
// (web-harness/src/main.ts) already attaches remote tracks.
//
// Scope, deliberately narrow:
// - Subscribes ONLY to `petal-camera-*` video publications (autoSubscribe
//   off, per-publication setSubscribed). Window shares stay native-only
//   (the compositor is THE high-fidelity path per SPEC.md §4.4), and audio
//   stays native-only (the ADM already plays remote audio; attaching it
//   here would double every voice).
// - Publishes nothing (the token can't -- least-privilege grants).
// - Never subscribes the app's OWN logical participant: local self-view
//   comes from a same-process feed (Windows native → canvas stream; macOS
//   direct getUserMedia), never from an SFU round trip of our own
//   publication.

import {
  Room,
  RoomEvent,
  Track,
  VideoQuality,
  type Participant,
  type RemoteParticipant,
  type RemoteTrack,
  type RemoteTrackPublication
} from 'livekit-client';
import { invoke } from '@tauri-apps/api/core';
import { COMMANDS } from '$lib/ipc';
import type { GalleryBridgeConfig } from '$lib/ipc';
// #247: local freeze-watchdog for remote camera tiles -- decision logic
// lives in cameraFreezeWatchdog.ts (framework-import-free, so it's directly
// unit-testable). See that module's header comment for the full rationale.
import {
  FREEZE_WATCHDOG_POLL_MS,
  type CameraFreezeState,
  type CameraDecodeHealthState,
  nextCameraFreezeState,
  isCameraFrameStale,
  framesDecodedFromStatsReport,
  nextCameraDecodeHealthState,
  formatCameraDecodeHealth,
  classifyCameraReceiveHealth
} from './cameraFreezeWatchdog.ts';

const CAMERA_TRACK_PREFIX = 'petal-camera-';
const WINDOW_TRACK_PREFIX = 'petal-window-';

export type { GalleryBridgeConfig } from '$lib/ipc';

/** One camera feed remote to the hidden bridge, keyed by its publisher's real
 * LiveKit identity (the same identity presence.rs reports, so the meeting
 * route can match streams to presence tiles directly). */
export interface RemoteCamera {
  identity: string;
  /** Display name from the publishing participant (fallback: identity). */
  name: string;
  /** Synthetic high-bit draw surface id derived from the full camera track name. */
  drawWindowId: number;
  stream: MediaStream;
}

export function cameraTrackNameForIdentity(identity: string): string {
  const slug =
    identity
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, '-')
      .replace(/^-+|-+$/g, '') || 'anon';
  return `${CAMERA_TRACK_PREFIX}${slug}`;
}

export function cameraWindowId(trackName: string): number {
  let hash = 0x811c_9dc5;
  for (let index = 0; index < trackName.length; index += 1) {
    hash ^= trackName.charCodeAt(index) & 0xff;
    hash = Math.imul(hash, 0x0100_0193) >>> 0;
  }
  return (hash | 0x8000_0000) >>> 0;
}

export interface GalleryBridgeActiveSpeaker {
  identity: string;
  /** Display name from LiveKit when available. */
  name: string;
  /** LiveKit audio level, 0..1. */
  audioLevel: number;
  isSpeaking: boolean;
}

export interface GalleryBridgeSignals {
  activeSpeakers: GalleryBridgeActiveSpeaker[];
  /** Participants currently publishing native window shares. The bridge
   * deliberately does NOT subscribe to these tracks; the compositor owns
   * native share rendering. This signal only lets the gallery prioritize the
   * sharer's webcam tile in spotlight mode. */
  sharingIdentities: string[];
  /** Count of `petal-window-*` publications per identity (display shares and
   * viewer-hidden windows included -- this is a raw publication count, not a
   * viewer-visibility count). Drives the #875 multi-share count pill; only
   * identities with count >= 1 appear (the pill itself gates on >= 2). */
  windowShareCounts: Record<string, number>;
  /** Participants with a subscribed camera feed paused by the SFU or otherwise
   * stream-stalled. Native shared windows surface this in the network cockpit;
   * gallery tiles use this only for webcam/camera tiles. */
  weakConnectionIdentities: string[];
  /** Participants whose camera feed has stopped making local decode progress
   * (see FREEZE_WATCHDOG_TIMEOUT_MS) independent of whether the SFU has
   * reported a pause -- catches a dead/frozen far end holding its last frame
   * with no server-side signal yet. */
  staleCameraIdentities: string[];
}

export interface GalleryBridge {
  disconnect(): Promise<void>;
}

/**
 * Connect the hidden bridge participant and keep `onChange` fed with the
 * current set of subscribed camera streams (called on every add/remove; an
 * empty array means no cameras). Returns a handle whose `disconnect()` MUST
 * be called on leave/unmount so the duplicate subscriptions are torn down.
 *
 * `localIdentity` is the app's REAL room identity (not the -gallery one).
 * The local participant is deliberately NEVER subscribed: the local
 * self-view comes from a same-process feed (Windows native → canvas stream;
 * macOS direct getUserMedia), never from an SFU round trip of our own
 * publication.
 */
export async function connectGalleryBridge(
  config: Pick<GalleryBridgeConfig, 'url' | 'token'>,
  localIdentity: string,
  onChange: (cameras: RemoteCamera[]) => void,
  onSignals?: (signals: GalleryBridgeSignals) => void,
  onDisconnected?: () => void
): Promise<GalleryBridge> {
  const room = new Room();
  const cameras = new Map<string, RemoteCamera>();
  const windowShares = new Map<string, Set<string>>();
  const weakConnections = new Map<string, Set<string>>();
  const cameraTracks = new Map<string, RemoteTrack>();
  const freezeStates = new Map<string, CameraFreezeState>();
  const decodeHealthStates = new Map<string, CameraDecodeHealthState>();
  const staleCameras = new Set<string>();
  let activeSpeakers: GalleryBridgeActiveSpeaker[] = [];
  let streamStatePoll: ReturnType<typeof setInterval> | null = null;
  let freezeWatchdogPoll: ReturnType<typeof setInterval> | null = null;

  const emit = () => onChange([...cameras.values()]);
  const emitSignals = () =>
    onSignals?.({
      activeSpeakers,
      sharingIdentities: [...windowShares.entries()]
        .filter(([, trackKeys]) => trackKeys.size > 0)
        .map(([identity]) => identity),
      windowShareCounts: Object.fromEntries(
        [...windowShares.entries()]
          .filter(([, trackKeys]) => trackKeys.size > 0)
          .map(([identity, trackKeys]) => [identity, trackKeys.size])
      ),
      weakConnectionIdentities: [...weakConnections.entries()]
        .filter(([, trackKeys]) => trackKeys.size > 0)
        .map(([identity]) => identity),
      staleCameraIdentities: [...staleCameras]
    });

  const isCameraPub = (pub: RemoteTrackPublication, participant: RemoteParticipant) =>
    pub.kind === Track.Kind.Video &&
    pub.trackName.startsWith(CAMERA_TRACK_PREFIX) &&
    participant.identity !== localIdentity;

  const isWindowSharePub = (pub: RemoteTrackPublication) =>
    pub.kind === Track.Kind.Video && pub.trackName.startsWith(WINDOW_TRACK_PREFIX);

  const maybeSubscribe = (pub: RemoteTrackPublication, participant: RemoteParticipant) => {
    if (!isCameraPub(pub, participant)) return;
    pub.setVideoQuality(VideoQuality.HIGH);
    void pub.setSubscribed(true);
  };
  const addWindowShare = (pub: RemoteTrackPublication, participant: RemoteParticipant) => {
    if (!isWindowSharePub(pub)) return;
    const trackKey = pub.trackSid || pub.trackName;
    const tracks = windowShares.get(participant.identity) ?? new Set<string>();
    tracks.add(trackKey);
    windowShares.set(participant.identity, tracks);
    emitSignals();
  };
  const dropWindowShare = (pub: RemoteTrackPublication, participant: RemoteParticipant) => {
    if (!isWindowSharePub(pub)) return;
    const tracks = windowShares.get(participant.identity);
    if (!tracks) return;
    tracks.delete(pub.trackSid || pub.trackName);
    if (tracks.size === 0) windowShares.delete(participant.identity);
    emitSignals();
  };
  const clearParticipantShares = (identity: string) => {
    if (windowShares.delete(identity)) emitSignals();
  };
  const setWeakConnection = (
    pub: RemoteTrackPublication,
    participant: RemoteParticipant,
    paused: boolean
  ) => {
    if (!isCameraPub(pub, participant)) return;
    const trackKey = pub.trackSid || pub.trackName;
    const tracks = weakConnections.get(participant.identity) ?? new Set<string>();
    const wasPaused = tracks.has(trackKey);
    if (wasPaused === paused) return;
    if (paused) tracks.add(trackKey);
    else tracks.delete(trackKey);
    if (tracks.size > 0) weakConnections.set(participant.identity, tracks);
    else weakConnections.delete(participant.identity);
    void invoke(COMMANDS.recordVideoStreamState, {
      participantIdentity: participant.identity,
      trackName: pub.trackName,
      state: paused ? 'paused' : 'active',
      source: 'livekit-js-stream-state'
    }).catch(() => {});
    emitSignals();
  };
  const clearParticipantWeakConnection = (identity: string) => {
    if (weakConnections.delete(identity)) emitSignals();
  };
  const cameraStreamPaused = (identity: string) => (weakConnections.get(identity)?.size ?? 0) > 0;
  const mapActiveSpeaker = (p: Participant): GalleryBridgeActiveSpeaker => ({
    identity: p.identity,
    name: p.name || p.identity,
    audioLevel: p.audioLevel,
    isSpeaking: p.isSpeaking
  });
  // #659: LiveKit's active-speaker signal is per-participant, aggregate over
  // every audio track that identity publishes -- including, during an AI
  // chat session, the assistant's voice (published under the sharer's own
  // identity, deliberately never muted by the room mic-mute button). A
  // muted mic transmits zero energy, so a muted identity reported as an
  // active speaker can only be some other track, never their own voice --
  // same reasoning and same fix as connection.ts's ActiveSpeakersChanged
  // handler and presence.rs's apply_speaking.
  const isGenuinelySpeaking = (p: Participant) => p.isMicrophoneEnabled;
  const publicationPaused = (pub: RemoteTrackPublication) =>
    String((pub.track as { streamState?: unknown } | undefined)?.streamState ?? '').toLowerCase() === 'paused';
  const syncStreamStates = () => {
    room.remoteParticipants.forEach((participant) => {
      participant.trackPublications.forEach((pub) => {
        const remotePub = pub as RemoteTrackPublication;
        if (!isCameraPub(remotePub, participant) || !remotePub.track) return;
        setWeakConnection(remotePub, participant, publicationPaused(remotePub));
      });
    });
  };
  const checkCameraFreezeWatchdog = async () => {
    const now = Date.now();
    let changed = false;
    for (const [identity, track] of cameraTracks) {
      let report: RTCStatsReport | undefined;
      try {
        report = await track.getRTCStatsReport();
      } catch {
        report = undefined;
      }
      const framesDecoded = framesDecodedFromStatsReport(report);
      const previous = freezeStates.get(identity);
      const next = nextCameraFreezeState(previous, framesDecoded, now);
      freezeStates.set(identity, next);
      const decodeHealth = nextCameraDecodeHealthState(
        decodeHealthStates.get(identity),
        framesDecoded,
        now
      );
      decodeHealthStates.set(identity, decodeHealth.state);
      const stale = isCameraFrameStale(next, now);
      if (decodeHealth.health) {
        console.info(
          formatCameraDecodeHealth({
            identity,
            ...decodeHealth.health,
            gapSinceLastFrameMs: now - next.lastProgressAt
          })
        );
        const signal = classifyCameraReceiveHealth(
          decodeHealth.health.framesDecoded === null ? null : decodeHealth.health.decodedFps,
          cameraStreamPaused(identity),
          stale
        );
        if (signal) {
          // Tauri's generic InvokeArgs requires a string index signature.
          // Rebuild the closed, allowlisted payload at this boundary instead
          // of widening the diagnostic model with one.
          void invoke<boolean>(COMMANDS.recordCameraReceiveHealth, {
            cadence: signal.cadence,
            decoderRender: signal.decoderRender
          }).catch(() => {});
        }
      }

      const wasStale = staleCameras.has(identity);
      if (stale === wasStale) continue;
      changed = true;
      if (stale) {
        staleCameras.add(identity);
        console.warn(`gallery bridge: camera tile stale (no decode progress) for '${identity}'`);
      } else {
        staleCameras.delete(identity);
      }
      void invoke(COMMANDS.recordVideoStreamState, {
        participantIdentity: identity,
        trackName: track.sid ?? '',
        state: stale ? 'stalled' : 'active',
        source: 'gallery-bridge-freeze-watchdog'
      }).catch(() => {});
    }
    if (changed) emitSignals();
  };

  // Listeners BEFORE connect: TrackSubscribed/TrackPublished can fire for
  // already-published tracks very early in the join handshake (the exact
  // ordering gotcha web-harness/src/main.ts documents).
  room.on(RoomEvent.TrackPublished, (pub: RemoteTrackPublication, p: RemoteParticipant) => {
    maybeSubscribe(pub, p);
    addWindowShare(pub, p);
  });
  room.on(RoomEvent.ActiveSpeakersChanged, (speakers: Participant[]) => {
    activeSpeakers = speakers.filter(isGenuinelySpeaking).map(mapActiveSpeaker);
    emitSignals();
  });
  room.on(
    RoomEvent.TrackSubscribed,
    (track: RemoteTrack, pub: RemoteTrackPublication, p: RemoteParticipant) => {
      if (!isCameraPub(pub, p)) return;
      pub.setVideoQuality(VideoQuality.HIGH);
      setWeakConnection(pub, p, publicationPaused(pub));
      cameras.set(p.identity, {
        identity: p.identity,
        name: p.name || p.identity,
        drawWindowId: cameraWindowId(pub.trackName),
        stream: new MediaStream([track.mediaStreamTrack])
      });
      cameraTracks.set(p.identity, track);
      freezeStates.delete(p.identity);
      decodeHealthStates.delete(p.identity);
      clearStaleCamera(p.identity);
      emit();
    }
  );
  const drop = (identity: string) => {
    if (cameras.delete(identity)) emit();
  };
  const clearStaleCamera = (identity: string) => {
    if (staleCameras.delete(identity)) emitSignals();
  };
  const clearFreezeWatchdogState = (identity: string) => {
    cameraTracks.delete(identity);
    freezeStates.delete(identity);
    decodeHealthStates.delete(identity);
    clearStaleCamera(identity);
  };
  room.on(
    RoomEvent.TrackUnsubscribed,
    (_t: RemoteTrack, pub: RemoteTrackPublication, p: RemoteParticipant) => {
      if (isCameraPub(pub, p)) {
        drop(p.identity);
        setWeakConnection(pub, p, false);
        clearFreezeWatchdogState(p.identity);
      }
    }
  );
  room.on(
    RoomEvent.TrackStreamStateChanged,
    (pub: RemoteTrackPublication, streamState: Track.StreamState, p: RemoteParticipant) => {
      setWeakConnection(pub, p, streamState === Track.StreamState.Paused);
    }
  );
  room.on(RoomEvent.TrackUnpublished, (pub: RemoteTrackPublication, p: RemoteParticipant) => {
    if (isCameraPub(pub, p)) {
      drop(p.identity);
      setWeakConnection(pub, p, false);
      clearFreezeWatchdogState(p.identity);
    }
    dropWindowShare(pub, p);
  });
  room.on(RoomEvent.ParticipantDisconnected, (p: RemoteParticipant) => {
    drop(p.identity);
    clearParticipantShares(p.identity);
    clearParticipantWeakConnection(p.identity);
    clearFreezeWatchdogState(p.identity);
    activeSpeakers = activeSpeakers.filter((speaker) => speaker.identity !== p.identity);
    emitSignals();
  });
  room.on(RoomEvent.Disconnected, () => {
    if (cameras.size > 0) {
      cameras.clear();
      emit();
    }
    if (
      windowShares.size > 0 ||
      activeSpeakers.length > 0 ||
      weakConnections.size > 0 ||
      staleCameras.size > 0
    ) {
      windowShares.clear();
      weakConnections.clear();
      staleCameras.clear();
      activeSpeakers = [];
      emitSignals();
    }
    cameraTracks.clear();
    freezeStates.clear();
    decodeHealthStates.clear();
    onDisconnected?.();
  });

  // autoSubscribe OFF: everything except petal-camera-* video (audio, window
  // shares) must never be pulled into the webview -- see module doc.
  await room.connect(config.url, config.token, { autoSubscribe: false });
  streamStatePoll = setInterval(syncStreamStates, 1000);
  freezeWatchdogPoll = setInterval(() => void checkCameraFreezeWatchdog(), FREEZE_WATCHDOG_POLL_MS);

  // Publications that existed before we joined don't fire TrackPublished.
  room.remoteParticipants.forEach((p) => {
    p.trackPublications.forEach((pub) => {
      const remotePub = pub as RemoteTrackPublication;
      maybeSubscribe(remotePub, p);
      addWindowShare(remotePub, p);
    });
  });
  activeSpeakers = room.activeSpeakers.filter(isGenuinelySpeaking).map(mapActiveSpeaker);
  syncStreamStates();
  emitSignals();

  return {
    async disconnect() {
      if (streamStatePoll !== null) {
        clearInterval(streamStatePoll);
        streamStatePoll = null;
      }
      if (freezeWatchdogPoll !== null) {
        clearInterval(freezeWatchdogPoll);
        freezeWatchdogPoll = null;
      }
      await room.disconnect();
    }
  };
}
