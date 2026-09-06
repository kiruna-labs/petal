import {
  Room,
  RoomEvent,
  Track,
  ConnectionState,
  ConnectionError,
  ConnectionErrorReason,
  type RemoteTrack,
  type RemoteTrackPublication,
  type RemoteParticipant,
  type SubscriptionError,
  type TrackPublication,
  type Participant,
} from 'livekit-client';
import type { HarnessContext } from './context.ts';
import { livekitRoomName } from '@petal/shared/logic/meetingCode';
import {
  AI_CHAT_TOPIC,
  LATENCY_PROBE_TOPIC,
  PIPELINE_STATS_TOPIC,
  REMOTE_CONTROL_TOPIC,
  cameraWindowId,
  isAiTrackName,
  mergeIdentityPaletteIndexMetadata,
  trackNameForCamera,
} from './trackNames.ts';
import { IDENTITY_COLOR_PALETTE, windowIdFromTrackName } from './telepointer.ts';
import { HARNESS_COLOR_STORAGE_KEY, HARNESS_ROOM_STORAGE_KEY } from './constants.ts';
import { displayNameFromInput, inviteLinkForCredential, tokenRequestBody } from './controls.ts';
import { displayNameForParticipant } from './tiles.ts';
import { commitLayoutModeTransition, layoutModeStateOf } from './tileLayout.ts';
import { endAutoSpotlight } from '@petal/shared/logic/tileLayoutMode';
import { sensitiveStringRegistry, type SensitiveStringRegistry } from './sensitiveStrings.ts';
import type { FeedbackReportController } from './feedbackReport.ts';
import { startAudioReceiverTelemetry } from './audioReceiverTelemetry.ts';
import {
  consumeLeaveRequested,
  deviceChanged,
  inMeeting,
  isPermissionDeniedError,
  joinFailed,
  joinFailedFromError,
  meetingJoined,
  meetingLeft,
  permissionDenied,
  reconnectFailed,
  reconnectRecovered,
  startRemoteAudioSilenceWatchdog,
} from './analytics.ts';
import {
  discoverWindowPublications,
  runReconciliationPass,
  FIRST_PASS_GRACE_MS,
  RECONCILE_INTERVAL_MS,
  RecoveryLedger,
  type RoomLike,
} from './publicationReconcile.ts';

// ---------------------------------------------------------------------------
// Token endpoint client + the connect flow shared by both the Create-meeting
// and Join paths (all LiveKit RoomEvent wiring lives here).
// ---------------------------------------------------------------------------
interface TokenResponse {
  url: string;
  token: string;
  room: string;
  displayName?: string;
}

export const TOKEN_REQUEST_RETRY_DELAYS_MS = [1000, 2000, 4000] as const;
const TOKEN_REQUEST_MAX_RETRY_AFTER_MS = 5000;
// On a bad network a fetch with no deadline can sit in the browser's TCP
// stack for over a minute before failing -- the retry ladder above never even
// starts. Bound every attempt so retries actually happen (user-reported:
// "web joined after a long delay" on a lossy network).
export const TOKEN_REQUEST_ATTEMPT_TIMEOUT_MS = 10_000;
// The initial LiveKit connect gets the same treatment as the token fetch:
// one dropped websocket dial must not fail the whole join.
export const CONNECT_RETRY_DELAYS_MS = [1000, 2000] as const;

type TokenFetch = typeof fetch;
type TokenDelay = (ms: number) => Promise<void>;

class TokenRequestHttpError extends Error {
  readonly transient: boolean;

  constructor(message: string, transient: boolean) {
    super(message);
    this.transient = transient;
  }
}

type ViteImportMeta = ImportMeta & {
  env?: {
    VITE_PETAL_BACKEND_URL?: string;
    PROD?: boolean;
  };
};

type RoomFactory = (options?: ConstructorParameters<typeof Room>[0]) => Room;

function localStoredPaletteIndex(): number | null {
  const raw = localStorage.getItem(HARNESS_COLOR_STORAGE_KEY);
  if (raw === null) return null;
  const index = Number(raw);
  return Number.isInteger(index) && index >= 0 && index < IDENTITY_COLOR_PALETTE.length ? index : null;
}

async function setLocalParticipantMetadata(participant: unknown, metadata: string): Promise<void> {
  const setter = (participant as { setMetadata?: (metadata: string) => Promise<void> }).setMetadata;
  if (typeof setter === 'function') {
    await setter.call(participant, metadata);
  }
}

export function setupConnection(
  ctx: HarnessContext,
  createRoom: RoomFactory = (options) => new Room(options),
  registry: SensitiveStringRegistry = sensitiveStringRegistry,
  feedbackReport?: FeedbackReportController
) {
  const { dom, state, cb } = ctx;
  const { shareBtn, micCheckbox, cameraTrackNameDisplay, displayNameInput } = dom;
  const {
    logEvent,
    setConnState,
    showError,
    clearError,
    showMeetingScreen,
    showJoinScreen,
    setJoinControlsEnabled,
    setShareState,
    setScreenShareState,
    setMicState,
    setRealMicState,
    setWebcamState,
    setAudioControl,
    setVideoControl,
    setShareControl,
    showActionableToast,
  } = ctx.ui;

  let audioPlaybackPrompt: HTMLButtonElement | null = null;
  const audioReceiverTelemetryCleanup = new Map<RemoteTrack, () => void>();

  function removeAudioPlaybackPrompt() {
    audioPlaybackPrompt?.remove();
    audioPlaybackPrompt = null;
  }

  function ensureAudioPlaybackPrompt(room: Room) {
    if (typeof document === 'undefined') return;
    if (audioPlaybackPrompt) return;

    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'audio-playback-prompt';
    button.textContent = 'Enable audio';
    button.setAttribute('aria-label', 'Enable remote audio playback');
    button.addEventListener('click', async () => {
      try {
        logEvent('audio playback blocked by browser policy; requesting user-gesture unlock...', 'warn');
        await room.startAudio();
        syncAudioPlaybackPrompt(room);
        logEvent('remote audio playback enabled', 'ok');
      } catch (err) {
        logEvent(`remote audio playback unlock failed: ${(err as Error).message ?? err}`, 'error');
      }
    });

    (dom.topbarRight ?? document.body).prepend(button);
    audioPlaybackPrompt = button;
  }

  function syncAudioPlaybackPrompt(room: Room) {
    if (room.canPlaybackAudio === false) {
      ensureAudioPlaybackPrompt(room);
      return;
    }
    removeAudioPlaybackPrompt();
  }

  // Diagnostic-only: a subscribed audio track and an <audio> element with no
  // JS-visible error can still produce total silence (blocked-but-unreported
  // playback, a decode failure, or downstream device/output routing) -- none
  // of which the existing log lines can distinguish. This taps the raw
  // MediaStreamTrack with a WebAudio analyser (independent of whether the
  // <audio> element itself is actually audible) so the session log records
  // whether real signal is arriving at all, separate from element playback
  // state. Self-cleans after a few samples; never throws into the caller.
  function diagnoseAudioPlayback(audioEl: HTMLAudioElement, track: RemoteTrack, participantIdentity: string) {
    const tag = `audio diag (${participantIdentity})`;
    audioEl.addEventListener('playing', () => logEvent(`${tag}: element entered playing state`, 'ok'), { once: true });
    audioEl.addEventListener('pause', () => logEvent(`${tag}: element paused`, 'warn'));
    audioEl.addEventListener('stalled', () => logEvent(`${tag}: element stalled (no data arriving)`, 'warn'));
    audioEl.addEventListener('error', () =>
      logEvent(`${tag}: element error: ${audioEl.error?.message ?? audioEl.error?.code ?? 'unknown'}`, 'error')
    );

    const mediaStreamTrack = (track as { mediaStreamTrack?: MediaStreamTrack }).mediaStreamTrack;
    if (!mediaStreamTrack) return;
    logEvent(
      `${tag}: mediaStreamTrack readyState=${mediaStreamTrack.readyState} muted=${mediaStreamTrack.muted} enabled=${mediaStreamTrack.enabled}`,
      'info'
    );

    const AudioContextCtor = window.AudioContext ?? (window as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
    if (!AudioContextCtor) return;
    try {
      const probeCtx = new AudioContextCtor();
      const source = probeCtx.createMediaStreamSource(new MediaStream([mediaStreamTrack]));
      const analyser = probeCtx.createAnalyser();
      analyser.fftSize = 512;
      source.connect(analyser);
      const data = new Uint8Array(analyser.frequencyBinCount);
      let samples = 0;
      const maxSamples = 3;
      const cleanup = () => {
        clearInterval(interval);
        source.disconnect();
        probeCtx.close().catch(() => {});
      };
      const interval = setInterval(() => {
        analyser.getByteTimeDomainData(data);
        let peak = 0;
        for (const value of data) peak = Math.max(peak, Math.abs(value - 128));
        logEvent(
          `${tag}: audioContext.state=${probeCtx.state} level-probe peak=${peak}/128 (0 = silence)`,
          peak > 2 ? 'ok' : 'warn'
        );
        samples += 1;
        if (samples >= maxSamples) cleanup();
      }, 4000);
    } catch (err) {
      logEvent(`${tag}: audio level probe failed to start: ${(err as Error).message ?? err}`, 'warn');
    }
  }

  function resetFailedJoinUi() {
    setJoinControlsEnabled(true);
    shareBtn.disabled = true;
    shareBtn.textContent = 'Share test pattern';
    micCheckbox.disabled = true;
    state.room = null;
    ctx.hook?.plugins?.roomDisconnected();
    state.sharing = false;
    state.screenSharing = false;
    setShareState('not sharing', false);
    setScreenShareState('not sharing', false);
    setShareControl(false);
    cb.syncHarnessHook();
    // A join-link auto-join runs behind the connecting interstitial; a failed
    // join must land back on the menu (which also dismisses the interstitial)
    // with the error visible, never leave a dead spinner up.
    showJoinScreen();
  }

  function startPipelineStats() {
    (cb as Partial<HarnessContext['cb']>).startPipelineStats?.();
  }

  function stopPipelineStats() {
    (cb as Partial<HarnessContext['cb']>).stopPipelineStats?.();
  }

  // The real tile-side attach path, shared by the TrackSubscribed handler and
  // the reconciliation pass below. Idempotent: `addShareTile` reuses the
  // (owner, window) tile and `attachVideoTrackIfChanged` no-ops when the video
  // is already showing this exact track.
  function attachRemoteShareTrack(
    participant: RemoteParticipant,
    pub: RemoteTrackPublication,
    track: RemoteTrack
  ) {
    cb.ensureBaseTile(participant.identity, false);
    const windowId = windowIdFromTrackName(pub.trackName);
    if (windowId !== null && pub.trackSid) {
      ctx.hook.pipelineStats?.trackSubscribed(participant.identity, windowId, pub.trackSid);
    }
    // #679 parity: only a GENUINELY new share tile gets the notice -- a
    // republish/quality-switch TrackSubscribed for a window that already has
    // a live tile must not re-fire it (same intent as the native suppression
    // in compositor::consume_share_started_pill_suppression, simplified here
    // since web-harness has no retire/hold state machine to key a
    // reconnect-specific suppression off of).
    const isNewShareTile = windowId !== null && cb.shareTileForWindowId(windowId) === null;
    cb.addShareTile(
      participant.identity,
      false,
      pub.trackSid,
      track,
      displayNameForParticipant(participant),
      windowId,
      participant.metadata
    );
    cb.setPublicationPaused(participant, pub, cb.publicationPaused(pub));
    if (isNewShareTile && windowId !== null) {
      notifyRemoteShareStarted(participant, windowId);
    }
  }

  // #679: mirrors the native top-center "<Name> is sharing a window" pill.
  // Web has no native windows to raise, so "Bring to foreground" maps onto
  // the existing pin/expand affordance (cb.pinTile) instead of a literal
  // foreground-raise. Routed through `ui.showActionableToast` (NOT a direct
  // import of toastMount.ts/Toast.svelte here) to keep connection.ts's
  // dependency graph the same as every other UI reaction in this file --
  // callback injection via ctx.ui/ctx.cb, never a concrete UI module. Also
  // never lets a rendering failure here (e.g. no real DOM available) break
  // the tile it is only decorating -- this is a courtesy notice, not core
  // share-tile wiring.
  function notifyRemoteShareStarted(participant: RemoteParticipant, windowId: number) {
    try {
      const displayName = displayNameForParticipant(participant);
      showActionableToast(`${displayName} is sharing a window`, 4000, {
        actionLabel: 'Bring to front',
        onAction: () => {
          const tile = cb.shareTileForWindowId(windowId);
          if (tile) cb.pinTile(tile, 'manual');
        }
      });
    } catch (error) {
      logEvent(`remote-share-started notice failed to render: ${error}`, 'warn');
    }
  }

  // #298 receiver-side reconciliation. Purely after-the-fact: the SDK already
  // dispatches TrackSubscribed for tracks published before we joined, so a
  // pass during the join window would only race a track arriving normally.
  function startPublicationReconcile(room: Room) {
    stopPublicationReconcile();
    const ledger = new RecoveryLedger();
    const startedAt = Date.now();
    const timer = setInterval(() => {
      if (state.room !== room) return;
      const now = Date.now();
      if (now - startedAt < FIRST_PASS_GRACE_MS) return;
      runReconciliationPass(
        discoverWindowPublications(room as unknown as RoomLike),
        cb.trackedShareWindows(),
        ledger,
        {
          setSubscribed: (identity, trackSid, subscribed) => {
            room.remoteParticipants
              .get(identity)
              ?.trackPublications.get(trackSid)
              ?.setSubscribed(subscribed);
          },
          attachTrack: (identity, trackSid) => {
            const participant = room.remoteParticipants.get(identity);
            const pub = participant?.trackPublications.get(trackSid);
            const track = pub?.track;
            if (!participant || !pub || pub.kind !== Track.Kind.Video || cb.isCameraTrack(pub)) {
              return;
            }
            if (!track) {
              // The publication claims a subscription but the SDK holds no
              // track handle yet — demand is the only lever left.
              pub.setSubscribed(true);
              return;
            }
            attachRemoteShareTrack(participant, pub, track);
          },
          retireTile: (identity, trackSid) => cb.removeShareTile(identity, trackSid),
          log: (message) => logEvent(message, 'warn'),
        },
        now
      );
    }, RECONCILE_INTERVAL_MS);
    // A bare repeating timer keeps Node's event loop alive forever, which
    // hangs any headless test that drives a fake room through connect and
    // never disconnects. Unref where the runtime supports it (Node); browsers
    // return a plain number and are unaffected either way.
    (timer as unknown as { unref?: () => void }).unref?.();
    state.publicationReconcileTimer = timer;
  }

  function stopPublicationReconcile() {
    if (state.publicationReconcileTimer !== null) {
      clearInterval(state.publicationReconcileTimer);
      state.publicationReconcileTimer = null;
    }
  }

  function backendApiUrl(path: string): string {
    // Prefer an explicit build-time backend URL. In a PRODUCTION build with none
    // set, fall back to the deployed backend so a public deploy can always mint
    // tokens (issue #177 -- the web counterpart of the native "no baked
    // backend URL" bug). In DEV, leave it empty so `/api/token` hits the local
    // Vite token middleware (server/tokenPlugin.ts) talking to a local
    // livekit-server, never the production backend.
    const env = (import.meta as ViteImportMeta).env;
    const configured = env?.VITE_PETAL_BACKEND_URL?.trim().replace(/\/$/, '');
    const base = configured || (env?.PROD ? 'https://app.petal.live' : '');
    return `${base}${path}`;
  }

  function syncAddressBar(meetingCode: string) {
    const url = inviteLinkForCredential(meetingCode, location.origin, cb.roomDisplayLabelForCredential(meetingCode));
    if (url === `${location.origin}/` || url === location.origin || url === '/') return;
    history.replaceState(null, '', url);
  }

  async function fetchToken(meetingCode: string, identity: string, displayName: string): Promise<TokenResponse> {
    return requestTokenWithRetry(backendApiUrl('/api/token'), meetingCode, identity, displayName, {
      onRetry: (attempt, error, delayMs) => {
        logEvent(
          `token request attempt ${attempt} failed (${error.message}); retrying in ${Math.round(delayMs / 100) / 10}s`,
          'warn'
        );
        ctx.ui.setConnectingStatus?.('Network is slow — still trying to reach the meeting server…');
      },
    });
  }

  async function connectToMeeting(meetingCode: string, identity: string) {
    const displayName = displayNameFromInput(displayNameInput.value);
    clearError();
    setJoinControlsEnabled(false);
    setConnState('connecting', 'connecting');
    // Register room + local identity with the Sentry PII-scrub registry
    // before any log line that could embed them is emitted (#283).
    registry.registerRoom(meetingCode);
    registry.registerParticipant(identity);
    registry.registerReportingValue(displayName);
    logEvent(`connecting to meeting "${meetingCode}" as "${displayName}"...`);

    let tokenResponse: TokenResponse;
    try {
      // `meetingCode` is now the full bearer capability. The actual LiveKit
      // room preserves Petal's invisible wire prefix: `petal-room-<credential>`.
      const livekitRoom = livekitRoomName(meetingCode);
      registry.registerRoom(livekitRoom);
      logEvent(`meeting "${meetingCode}" -> livekit room "${livekitRoom}"`);
      ctx.ui.setConnectingStatus?.('Requesting access…');
      tokenResponse = await fetchToken(meetingCode, identity, displayName);
      registry.registerRoom(tokenResponse.room);
      logEvent(`backend assigned livekit room "${tokenResponse.room}"`);
    } catch (err) {
      setConnState('error', 'error');
      logEvent(`token request failed: ${(err as Error).message ?? err}`, 'error');
      showError(`Token request failed: ${(err as Error).message ?? err}`);
      joinFailedFromError(err);
      resetFailedJoinUi();
      return;
    }

    const metadataWorker = cb.ensureFrameMetadataWorker();
    const newRoom = createRoom(metadataWorker ? { frameMetadata: { worker: metadataWorker } } : undefined);
    state.room = newRoom;
    ctx.hook?.plugins?.roomConnected(newRoom);
    state.currentMeetingCode = meetingCode;

    newRoom.on(RoomEvent.ConnectionStateChanged, (connectionState: ConnectionState) => {
      if (connectionState === ConnectionState.Connected) {
        setConnState('connected', 'connected');
        cb.startViewerDemandHeartbeat();
        cb.startLatencyProbe();
        startPipelineStats();
      } else if (connectionState === ConnectionState.Connecting) {
        setConnState(connectionState, 'connecting');
      } else if (connectionState === ConnectionState.Reconnecting) {
        setConnState(connectionState, 'connecting');
        logEvent('reconnecting...', 'warn');
      } else if (connectionState === ConnectionState.Disconnected) {
        setConnState('disconnected', 'idle');
        cb.stopLatencyProbe();
        cb.resetRemoteControlHarnessSession?.();
        stopPipelineStats();
        stopPublicationReconcile();
        for (const stop of audioReceiverTelemetryCleanup.values()) stop();
        audioReceiverTelemetryCleanup.clear();
      }
    });

    newRoom.on(RoomEvent.Reconnecting, () => {
      logEvent('connection lost, attempting to reconnect...', 'warn');
    });

    newRoom.on(RoomEvent.Reconnected, () => {
      logEvent('reconnected successfully', 'ok');
      reconnectRecovered();
    });

    newRoom.on(RoomEvent.AudioPlaybackStatusChanged, () => {
      syncAudioPlaybackPrompt(newRoom);
      if (newRoom.canPlaybackAudio === false) {
        logEvent('remote audio playback is blocked until the user enables audio', 'warn');
      } else {
        logEvent('remote audio playback is allowed', 'ok');
      }
    });

    newRoom.on(RoomEvent.ParticipantConnected, (p: RemoteParticipant) => {
      registry.registerParticipant(p.identity);
      const displayName = displayNameForParticipant(p);
      registry.registerReportingValue(displayName);
      logEvent(`participant joined: ${displayName}`, 'ok');
      cb.ensureBaseTile(p.identity, false);
      cb.updateParticipantCount();
    });

    newRoom.on(RoomEvent.ParticipantDisconnected, (p: RemoteParticipant) => {
      const displayName = displayNameForParticipant(p);
      registry.registerReportingValue(displayName);
      // A superseded participant instance can finish disconnecting after a
      // same-identity FULL-reconnect replacement is already registered. Never
      // let that stale callback retire the replacement's surfaces.
      const currentParticipant = newRoom.remoteParticipants.get(p.identity);
      if (currentParticipant && currentParticipant !== p) {
        logEvent(`superseded participant left: ${displayName}`, 'warn');
        return;
      }
      logEvent(`participant left: ${displayName}`, 'warn');
      registry.unregisterParticipant(p.identity);
      // Tile teardown first: it destroys the remote-window headers, which
      // release any push-to-talk floor still held for that owner's windows.
      cb.removeParticipantTiles(p.identity);
      // #657: the AI session runs on the owner's machine. Once they are gone
      // it cannot still be running, and the contract says a receiver clears
      // its UI on owner disconnect rather than waiting out the heartbeat.
      cb.aiChatOwnerLeft(p.identity);
    });

    newRoom.on(RoomEvent.ParticipantMetadataChanged, (_metadata, participant) => {
      if (!('trackPublications' in participant)) return;
      cb.updateParticipantShareColorProfiles(participant as RemoteParticipant);
      cb.repositionRemoteDraw();
      cb.repositionRemoteTelepointers();
    });

    // Receiver-local startup chronology only. TrackPublished can precede
    // TrackSubscribed by enough time to explain a blank startup; recording it
    // here does not change subscription policy or publication state (#299).
    newRoom.on(RoomEvent.TrackPublished, (pub: RemoteTrackPublication, participant: RemoteParticipant) => {
      if (pub.kind !== Track.Kind.Video || cb.isCameraTrack(pub)) return;
      const windowId = windowIdFromTrackName(pub.trackName);
      if (windowId !== null && pub.trackSid) {
        ctx.hook.pipelineStats?.trackPublished(participant.identity, windowId, pub.trackSid);
      }
      // Publish demand before TrackSubscribed completes, so the OWNER learns
      // of viewer intent as early as possible. This does NOT itself request
      // a subscription quality/dimension from the SFU for THIS subscriber --
      // that still happens later, in the tile-backed open demand below, once
      // a real tile/video element exists to size against (LiveKit's
      // setVideoDimensions/setVideoQuality need a subscribed RemoteTrack to
      // be meaningful). This early packet is a partial step, not a full fix
      // for reduced-layer startup selection; the subscription-quality-request
      // timing gap is a real remaining #299 follow-up, not closed here.
      cb.publishViewerDemandForPublication(participant.identity, pub);
    });

    newRoom.on(RoomEvent.TrackUnpublished, (pub: RemoteTrackPublication, participant: RemoteParticipant) => {
      if (pub.kind !== Track.Kind.Video || cb.isCameraTrack(pub)) return;
      const windowId = windowIdFromTrackName(pub.trackName);
      if (windowId !== null && pub.trackSid) {
        ctx.hook.pipelineStats?.trackUnpublished(participant.identity, windowId, pub.trackSid);
      }
    });

    newRoom.on(RoomEvent.DataReceived, (payload, participant, _kind, topic) => {
      if (topic === REMOTE_CONTROL_TOPIC) {
        cb.handleRemoteControlPayload(payload, participant?.identity);
        return;
      }
      if (topic === LATENCY_PROBE_TOPIC) {
        cb.handleLatencyProbePayload(payload, participant?.identity);
        return;
      }
      if (topic === PIPELINE_STATS_TOPIC) {
        cb.handlePipelineStatsPayload(payload, participant?.identity);
        return;
      }
      if (topic === AI_CHAT_TOPIC) {
        cb.handleAiChatPayload(payload, participant?.identity, topic);
        return;
      }
      cb.handleRemoteDrawPayload(payload, participant?.identity, topic);
      cb.handleRemoteTelepointerPayload(payload, participant?.identity, topic);
    });

    if ((RoomEvent as Record<string, unknown>).ActiveSpeakersChanged) {
      newRoom.on(RoomEvent.ActiveSpeakersChanged, (speakers) => {
        ctx.activeSpeakerTargets.clear();
        speakers.forEach((speaker) => {
          // #659: `ActiveSpeakersChanged` is per-PARTICIPANT, aggregate over
          // every audio track that identity publishes -- including, during an
          // AI chat session, the assistant's voice (published under the
          // sharer's own identity, but deliberately never muted by the room
          // mic-mute button; see the `isAssistantVoice` handling above). A
          // muted mic transmits zero energy, so any "speaking" attributed to a
          // muted identity cannot be their own voice -- skip it here, the same
          // way the native client's presence.rs does. An unmuted participant
          // genuinely reported as speaking is unaffected.
          if (!speaker.isMicrophoneEnabled) return;
          ctx.activeSpeakerTargets.add(speaker.identity);
        });
        cb.startSpeakerSmoothing();
        cb.smoothSpeakingScores();
      });
    }

    newRoom.on(
      RoomEvent.TrackSubscribed,
      (track: RemoteTrack, pub: RemoteTrackPublication, participant: RemoteParticipant) => {
        logEvent(`track subscribed: ${participant.identity} / ${pub.trackName ?? '(unnamed)'} (${track.kind})`, 'ok');
        // A track subscription is proof the participant exists, regardless of
        // whether ParticipantConnected has fired yet for them (LiveKit can fire
        // TrackSubscribed for already-published tracks very early in the join
        // sequence) -- ensure the base tile exists first.
        cb.ensureBaseTile(participant.identity, false);
        if (track.kind === Track.Kind.Video) {
          if (cb.isCameraTrack(pub)) {
            cb.setTileCamera(participant.identity, false, track, cameraWindowId(pub.trackName ?? trackNameForCamera(participant.identity)));
            cb.setPublicationPaused(participant, pub, cb.publicationPaused(pub));
          } else {
            attachRemoteShareTrack(participant, pub, track);
          }
        }
        if (track.kind === Track.Kind.Audio) {
          // #657: `petal-ai-*` is the ASSISTANT's voice, published by the
          // window's owner. It is played like any other audio, but it is not
          // that participant's microphone -- so it must never light their
          // speaking indicator, and muting your own mic must not mute it.
          // Classified explicitly here rather than falling through to the
          // human-mic branch.
          const isAssistantVoice = isAiTrackName(pub.trackName);
          if (!isAssistantVoice) cb.setParticipantAudioActive(participant.identity, true);
          // Audio still needs browser autoplay permission. The room-level
          // AudioPlaybackStatusChanged/startAudio flow above handles strict
          // browsers such as Safari when this async attach is blocked.
          const audioEl = document.createElement('audio');
          audioEl.autoplay = true;
          audioEl.dataset.trackSid = track.sid ?? '';
          audioEl.dataset.participant = participant.identity;
          if (isAssistantVoice) audioEl.dataset.aiChat = 'true';
          audioEl.style.display = 'none';
          document.body.appendChild(audioEl);
          track.attach(audioEl);
          diagnoseAudioPlayback(audioEl, track, participant.identity);
          audioReceiverTelemetryCleanup.get(track)?.();
          const stopTelemetry = startAudioReceiverTelemetry(track, logEvent);
          const mediaStreamTrack = (track as { mediaStreamTrack?: MediaStreamTrack }).mediaStreamTrack;
          const stopSilence =
            isAssistantVoice || !mediaStreamTrack
              ? () => undefined
              : startRemoteAudioSilenceWatchdog({
                  key: track.sid || pub.trackSid || participant.identity,
                  mediaStreamTrack,
                  isMuted: () => Boolean(track.isMuted || mediaStreamTrack.muted),
                });
          audioReceiverTelemetryCleanup.set(track, () => {
            stopTelemetry();
            stopSilence();
          });
        }
      }
    );

    newRoom.on(
      RoomEvent.TrackUnsubscribed,
      (track: RemoteTrack, pub: RemoteTrackPublication, participant: RemoteParticipant) => {
        logEvent(`track unsubscribed: ${participant.identity} / ${pub.trackName ?? '(unnamed)'} (${track.kind})`);
        if (track.kind === Track.Kind.Video) {
          cb.setPublicationPaused(participant, pub, false);
          if (cb.isCameraTrack(pub)) {
            cb.clearTileCamera(participant.identity);
          } else {
            cb.removeShareTile(participant.identity, pub.trackSid);
          }
        } else {
          // #657: symmetry with the subscribe branch. An assistant track going
          // away says nothing about the owner's microphone, so it must not
          // clear their speaking indicator.
          if (!isAiTrackName(pub.trackName)) cb.setParticipantAudioActive(participant.identity, false);
          audioReceiverTelemetryCleanup.get(track)?.();
          audioReceiverTelemetryCleanup.delete(track);
          track.detach().forEach((el) => el.remove());
        }
        if (track.kind === Track.Kind.Video && !cb.isCameraTrack(pub)) {
          const windowId = windowIdFromTrackName(pub.trackName);
          if (windowId !== null && pub.trackSid) {
            ctx.hook.pipelineStats?.trackUnsubscribed(participant.identity, windowId, pub.trackSid);
          }
        }
      }
    );

    newRoom.on(
      RoomEvent.TrackStreamStateChanged,
      (pub: RemoteTrackPublication, streamState: Track.StreamState, participant: RemoteParticipant) => {
        const paused = streamState === Track.StreamState.Paused;
        // #627: a paused stream has no frames to present. Hold the last one
        // before the video element runs dry rather than after.
        if (paused && pub.kind === Track.Kind.Video) {
          cb.holdShareFrame(participant.identity, pub.trackSid, 'paused');
        }
        cb.setPublicationPaused(participant, pub, paused);
      }
    );

    // #627: TrackMuted/TrackUnmuted were previously unhandled. A muted remote
    // video track stops delivering frames, and a `<video>` with nothing to
    // present renders black -- which the "never show a black frame" rule
    // forbids. Engage the held frame the instant mute is announced; the hold
    // releases itself when real frames resume, so unmute needs no handler of
    // its own beyond the log.
    newRoom.on(RoomEvent.TrackMuted, (pub: TrackPublication, participant: Participant) => {
      if (pub.kind !== Track.Kind.Video) return;
      cb.holdShareFrame(participant.identity, pub.trackSid, 'muted');
      logEvent(`remote video track muted: ${participant.identity} / ${pub.trackSid} (holding last frame)`, 'warn');
    });

    // #283: previously unhandled -- a failed remote-track subscription was
    // invisible both locally and to Sentry.
    newRoom.on(
      RoomEvent.TrackSubscriptionFailed,
      (trackSid: string, participant: RemoteParticipant, reason?: SubscriptionError) => {
        const reasonSuffix = reason !== undefined ? ` (reason: ${reason})` : '';
        logEvent(`track subscription failed: ${participant.identity} / ${trackSid}${reasonSuffix}`, 'error');
      }
    );

    // #283: previously unhandled -- camera/mic acquisition failures (e.g. a
    // device disappearing mid-call, or a permission revoke) were invisible
    // both locally and to Sentry.
    newRoom.on(RoomEvent.MediaDevicesError, (error: Error, kind?: MediaDeviceKind) => {
      const kindSuffix = kind ? ` (${kind})` : '';
      logEvent(`media devices error${kindSuffix}: ${error.message ?? error}`, 'error');
      if (isPermissionDeniedError(error)) {
        if (kind === 'audioinput') permissionDenied('mic');
        else if (kind === 'videoinput') permissionDenied('camera');
      } else if (kind === 'audioinput') {
        deviceChanged('mic', 'failed');
      } else if (kind === 'videoinput') {
        deviceChanged('camera', 'failed');
      }
    });

    newRoom.on(RoomEvent.Disconnected, (reason?: unknown) => {
      const userLeft = consumeLeaveRequested() || isClientInitiatedDisconnect(reason);
      if (!userLeft && inMeeting()) reconnectFailed();
      meetingLeft();
      setConnState('disconnected', 'idle');
      logEvent('disconnected', 'warn');
      setJoinControlsEnabled(true);
      shareBtn.disabled = true;
      micCheckbox.disabled = true;
      state.room = null;
      ctx.hook?.plugins?.roomDisconnected();
      ctx.hook.pipelineStats?.resetSession();
      cb.stopViewerDemandHeartbeat();
      cb.stopLatencyProbe();
      cb.resetRemoteControlHarnessSession?.();
      stopPipelineStats();
      stopPublicationReconcile();
      for (const stop of audioReceiverTelemetryCleanup.values()) stop();
      audioReceiverTelemetryCleanup.clear();
      if (state.streamStatePollTimer !== null) {
        clearInterval(state.streamStatePollTimer);
        state.streamStatePollTimer = null;
      }
      state.frameMetadataWorker?.terminate();
      state.frameMetadataWorker = null;
      state.sharing = false;
      state.micOn = false;
      state.webcamOn = false;
      if (state.localCameraTrack) {
        state.localCameraTrack.mediaStreamTrack.stop();
        state.localCameraTrack = null;
      }
      if (state.screenTrack) {
        state.screenTrack.mediaStreamTrack.stop();
        state.screenTrack = null;
      }
      state.screenSharing = false;
      state.screenWindowId = null;
      if (state.micTrack) {
        state.micTrack.mediaStreamTrack.stop();
        state.micTrack = null;
      }
      state.realMicOn = false;
      cb.stopTelepointerSender();
      setShareState('not sharing', false);
      setScreenShareState('not sharing', false);
      setMicState('off', false);
      setRealMicState('off', false);
      setWebcamState('off', false);
      micCheckbox.checked = false;
      shareBtn.textContent = 'Share test pattern';
      setAudioControl('off');
      setVideoControl(false);
      setShareControl(false);
      cb.setDrawMode(false);
      cb.syncHarnessHook();
      cb.clearTiles();
      cb.clearRemoteTelepointers();
      cb.clearRemoteDraw();
      // #657: releases any held push-to-talk floor before dropping the
      // sessions, so a disconnect mid-press cannot leave the host tapping the
      // room's microphone.
      cb.resetAiChat();
      cb.stopRemoteControl('disconnected');
      state.pinnedTileId = null;
      // #785: the session's shares are gone, so an automatic spotlight must not
      // outlive it -- otherwise the next join starts in a mode the user never
      // chose. A no-op when the user picked spotlight themselves.
      commitLayoutModeTransition(state, endAutoSpotlight(layoutModeStateOf(state)));
      cb.resetActiveSpeakers();
      cb.updateParticipantCount();
      cb.applyTileLayout();
      removeAudioPlaybackPrompt();
      state.currentMeetingCode = null;
      feedbackReport?.onDisconnect();
      history.replaceState(null, '', location.origin);
      showJoinScreen();
      // Session ended -- clear the Sentry PII-scrub registry so a future
      // session's breadcrumbs never keep an old room/identity around.
      registry.reset();
    });

    try {
      ctx.ui.setConnectingStatus?.('Connecting to the meeting…');
      await connectWithRetry(() => newRoom.connect(tokenResponse.url, tokenResponse.token), {
        onRetry: (attempt, error, delayMs) => {
          logEvent(
            `room connect attempt ${attempt} failed (${error.message}); retrying in ${Math.round(delayMs / 100) / 10}s`,
            'warn'
          );
          ctx.ui.setConnectingStatus?.('Connection hiccup — retrying…');
        },
      });
      // #709: `ParticipantConnected` only fires for participants who join
      // AFTER us -- anyone already in the room when `connect()` resolves is
      // reachable via `remoteParticipants` but was never registered with the
      // PII-scrub registry, so their raw identity (and, once they act,
      // display name) reached Sentry unredacted in things like latency-probe
      // log lines. Register every pre-existing participant immediately, the
      // same way the `ParticipantConnected` handler below does for later
      // joiners.
      newRoom.remoteParticipants?.forEach((participant) => {
        registry.registerParticipant(participant.identity);
        registry.registerReportingValue(displayNameForParticipant(participant));
      });
      if (newRoom.localParticipant) {
        await setLocalParticipantMetadata(
          newRoom.localParticipant,
          mergeIdentityPaletteIndexMetadata(newRoom.localParticipant.metadata, localStoredPaletteIndex())
        ).catch((err) => logEvent(`identity color metadata publish failed: ${(err as Error).message ?? err}`, 'warn'));
      }
      setConnState('connected', 'connected');
      syncAddressBar(meetingCode);
      logEvent(`connected to "${meetingCode}" as "${displayName}"`, 'ok');
      meetingJoined();
      localStorage.setItem(HARNESS_ROOM_STORAGE_KEY, meetingCode);
      cb.recordRecentRoom(meetingCode);
      shareBtn.disabled = false;
      micCheckbox.disabled = false;
      displayNameInput.value = displayName;
      cameraTrackNameDisplay.textContent = trackNameForCamera(identity);
      cb.syncHarnessHook();
      showMeetingScreen(meetingCode, tokenResponse.displayName);
      syncAudioPlaybackPrompt(newRoom);
      cb.refreshParticipantGrid();
      if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
      state.streamStatePollTimer = setInterval(() => cb.syncStreamStates(newRoom), 1000);
      cb.syncStreamStates(newRoom);
      cb.startViewerDemandHeartbeat();
      startPipelineStats();
      startPublicationReconcile(newRoom);
    } catch (err) {
      setConnState('error', 'error');
      showError(`Connect failed: ${(err as Error).message ?? err}`);
      emitJoinFailed(err);
      resetFailedJoinUi();
      if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
      state.streamStatePollTimer = null;
      state.frameMetadataWorker?.terminate();
      state.frameMetadataWorker = null;
      cb.stopViewerDemandHeartbeat();
      stopPipelineStats();
    }
  }

  return { connectToMeeting };
}

export async function requestTokenWithRetry(
  tokenUrl: string,
  meetingCode: string,
  identity: string,
  displayName: string,
  options: {
    fetchImpl?: TokenFetch;
    delay?: TokenDelay;
    retryDelaysMs?: readonly number[];
    attemptTimeoutMs?: number;
    onRetry?: (attempt: number, error: Error, delayMs: number) => void;
  } = {}
): Promise<TokenResponse> {
  const fetchImpl = options.fetchImpl ?? fetch;
  const delay = options.delay ?? ((ms) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  const retryDelaysMs = options.retryDelaysMs ?? TOKEN_REQUEST_RETRY_DELAYS_MS;
  const attemptTimeoutMs = options.attemptTimeoutMs ?? TOKEN_REQUEST_ATTEMPT_TIMEOUT_MS;
  let lastTransientError: Error | null = null;

  for (let attempt = 0; attempt <= retryDelaysMs.length; attempt += 1) {
    const abort = new AbortController();
    const attemptTimer = setTimeout(() => abort.abort(), attemptTimeoutMs);
    try {
      const res = await fetchImpl(tokenUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(tokenRequestBody(meetingCode, identity, displayName)),
        signal: abort.signal,
      }).catch((err: unknown) => {
        // An abort we triggered is a deadline, not a user cancel -- surface it
        // as the transient timeout it is so the retry ladder (and the error
        // copy in meetingActionError.ts) treat it as a network problem.
        if (abort.signal.aborted) throw new Error('token request timed out');
        throw err;
      });
      const body = await parseTokenResponseBody(res);
      if (res.ok) return body as TokenResponse;

      const error = new TokenRequestHttpError(
        'error' in body ? body.error : `token request failed (${res.status})`,
        isTransientTokenStatus(res.status)
      );
      if (!error.transient || attempt >= retryDelaysMs.length) throw error;
      lastTransientError = error;
      const delayMs = tokenRetryDelayMs(res, retryDelaysMs[attempt]!);
      options.onRetry?.(attempt + 1, error, delayMs);
      await delay(delayMs);
    } catch (err) {
      const error = err instanceof Error ? err : new Error(String(err));
      if (error instanceof TokenRequestHttpError) {
        if (!error.transient || attempt >= retryDelaysMs.length) throw error;
        lastTransientError = error;
        options.onRetry?.(attempt + 1, error, retryDelaysMs[attempt]!);
        await delay(retryDelaysMs[attempt]!);
        continue;
      }
      if (!isTransientTokenError(error) || attempt >= retryDelaysMs.length) {
        throw error;
      }
      lastTransientError = error;
      options.onRetry?.(attempt + 1, error, retryDelaysMs[attempt]!);
      await delay(retryDelaysMs[attempt]!);
    } finally {
      clearTimeout(attemptTimer);
    }
  }

  throw lastTransientError ?? new Error('token request failed');
}

async function parseTokenResponseBody(res: Response): Promise<TokenResponse | { error: string }> {
  try {
    return (await res.json()) as TokenResponse | { error: string };
  } catch {
    return { error: `token request failed (${res.status})` };
  }
}

function isClientInitiatedDisconnect(reason: unknown): boolean {
  if (reason == null) return false;
  const value = typeof reason === 'number' ? reason : String(reason);
  return value === 1 || value === 'CLIENT_INITIATED' || String(reason).includes('CLIENT_INITIATED');
}

function emitJoinFailed(error: unknown): void {
  if (error instanceof ConnectionError) {
    if (
      error.reason === ConnectionErrorReason.NotAllowed ||
      error.reason === ConnectionErrorReason.Cancelled
    ) {
      joinFailed('token');
      return;
    }
    if (error.reason === ConnectionErrorReason.Timeout) {
      joinFailed('timeout');
      return;
    }
    joinFailed('network');
    return;
  }
  joinFailedFromError(error);
}

// A failed INITIAL LiveKit connect is retryable unless the server told us
// no (bad token / user-initiated cancel). livekit-client's reconnect policy
// only covers an already-established session; the first dial gets no retry
// from the SDK at all, and on a lossy network one dropped websocket
// handshake used to fail the whole join.
export function isTransientConnectError(error: unknown): boolean {
  if (error instanceof ConnectionError) {
    return (
      error.reason !== ConnectionErrorReason.NotAllowed &&
      error.reason !== ConnectionErrorReason.Cancelled &&
      error.reason !== ConnectionErrorReason.LeaveRequest
    );
  }
  // Anything else out of `Room.connect` here is network-shaped (fetch/ws
  // failures surface as plain errors) -- retrying is bounded and cheap.
  return true;
}

export async function connectWithRetry(
  connect: () => Promise<void>,
  options: {
    delay?: TokenDelay;
    retryDelaysMs?: readonly number[];
    onRetry?: (attempt: number, error: Error, delayMs: number) => void;
  } = {}
): Promise<void> {
  const delay = options.delay ?? ((ms) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  const retryDelaysMs = options.retryDelaysMs ?? CONNECT_RETRY_DELAYS_MS;

  for (let attempt = 0; attempt <= retryDelaysMs.length; attempt += 1) {
    try {
      await connect();
      return;
    } catch (err) {
      const error = err instanceof Error ? err : new Error(String(err));
      if (!isTransientConnectError(error) || attempt >= retryDelaysMs.length) throw error;
      options.onRetry?.(attempt + 1, error, retryDelaysMs[attempt]!);
      await delay(retryDelaysMs[attempt]!);
    }
  }
}

function isTransientTokenStatus(status: number): boolean {
  return status === 429 || status >= 500;
}

function isTransientTokenError(error: Error): boolean {
  return !/^token request failed \((?:4\d\d)\)/.test(error.message);
}

function tokenRetryDelayMs(res: Response, fallbackMs: number): number {
  const retryAfter = res.headers.get('Retry-After');
  if (!retryAfter) return fallbackMs;

  const seconds = Number(retryAfter);
  if (Number.isFinite(seconds) && seconds >= 0) {
    return Math.min(seconds * 1000, TOKEN_REQUEST_MAX_RETRY_AFTER_MS);
  }

  const dateMs = Date.parse(retryAfter);
  if (Number.isFinite(dateMs)) {
    return Math.min(Math.max(0, dateMs - Date.now()), TOKEN_REQUEST_MAX_RETRY_AFTER_MS);
  }

  return fallbackMs;
}
