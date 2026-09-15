import { test } from 'node:test';
import assert from 'node:assert/strict';
import { RoomEvent, type Room } from 'livekit-client';

import { setupConnection } from '../src/connection.ts';
import { VIEWER_DEMAND_TOPIC } from '../src/trackNames.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import type { HarnessContext } from '../src/context.ts';

// A native receiver's starvation watchdog asks a stalled browser
// sharer to repair its publication over `petal.viewer-demand`
// (`needsRepublish: true`). Before this fix connection.ts's dispatcher
// registered a permanent no-op for this topic (`topics.on(VIEWER_DEMAND_TOPIC,
// () => {})`), so the request reached the web client and was silently
// dropped -- the receiver stayed frozen forever. This test exercises the
// REAL `RoomEvent.DataReceived` -> topic-dispatch wiring in connection.ts,
// not just the handler in isolation, so reverting that one registration
// line is what turns this test red.

const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);

type FakeHandler = (...args: unknown[]) => void;

class FakeElement {
  readonly classList = { add() {}, remove() {}, contains: () => false, toggle() {} };
  readonly dataset: Record<string, string> = {};
  readonly style = { display: '' };
  readonly children: FakeElement[] = [];
  readonly listeners = new Map<string, FakeHandler[]>();
  textContent = '';
  className = '';
  parent: FakeElement | null = null;

  appendChild(child: FakeElement) {
    child.parent = this;
    this.children.push(child);
    return child;
  }

  addEventListener(event: string, handler: FakeHandler) {
    this.listeners.set(event, [...(this.listeners.get(event) ?? []), handler]);
  }

  querySelector(): FakeElement | null {
    return null;
  }
}

class FakeDocument {
  readonly body = new FakeElement();
  createElement() {
    return new FakeElement();
  }
  querySelector() {
    return null;
  }
}

function installFakeDom() {
  const originalDocument = globalThis.document;
  const document = new FakeDocument();
  Object.defineProperty(globalThis, 'document', { configurable: true, value: document });
  return {
    restore: () => {
      if (originalDocument === undefined) Reflect.deleteProperty(globalThis, 'document');
      else Object.defineProperty(globalThis, 'document', { configurable: true, value: originalDocument });
    },
  };
}

function installBrowserGlobals() {
  Object.defineProperty(globalThis, 'location', {
    configurable: true,
    value: new URL('https://meet.petal.live/'),
  });
  Object.defineProperty(globalThis, 'history', {
    configurable: true,
    value: { replaceState: () => {} },
  });
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: { setItem: () => {}, getItem: () => null },
  });
  Object.defineProperty(globalThis, 'fetch', {
    configurable: true,
    value: async () => ({
      ok: true,
      json: async () => ({
        url: 'wss://livekit.invalid',
        token: 'token',
        room: `petal-room-${CREDENTIAL}`,
      }),
    }),
  });
}

class FakeRoom {
  private handlers = new Map<string, FakeHandler[]>();
  canPlaybackAudio = true;
  remoteParticipants = new Map<string, unknown>();

  on(event: string, handler: FakeHandler) {
    this.handlers.set(event, [...(this.handlers.get(event) ?? []), handler]);
    return this;
  }

  async connect() {}
  async startAudio() {}

  emit(event: string, ...args: unknown[]) {
    for (const handler of this.handlers.get(event) ?? []) handler(...args);
  }
}

function makeConnectionContext(
  topbarRight: FakeElement,
  handleViewerDemandPayload: (payload: Uint8Array, senderIdentity?: string) => void
) {
  const state = {
    room: null,
    frameMetadataWorker: null,
    streamStatePollTimer: null,
    viewerDemandTimer: null,
    pipelineStatsTimer: null,
    publicationReconcileTimer: null,
    localVideoTrack: null,
    localAudioTrack: null,
    localCameraTrack: null,
    screenTrack: null,
    screenWindowId: null,
    micTrack: null,
    sharing: false,
    screenSharing: false,
    micOn: false,
    realMicOn: false,
    webcamOn: false,
    currentMeetingCode: null,
    tileLayoutMode: 'grid',
    pinnedTileId: null,
    layoutModeButtons: null,
    speakerSmoothingTimer: null,
    activeRemoteControl: null,
    remoteControlSeq: 0,
    viewerDemandSeq: 0,
    audioCtx: null,
    oscillator: null,
    syntheticCameraIntervalId: null,
  };

  const ctx = {
    windowId: 1,
    dom: {
      shareBtn: { disabled: false, textContent: '' },
      micCheckbox: { disabled: false, checked: false },
      displayNameInput: { value: 'Riley' },
      cameraTrackNameDisplay: { textContent: '' },
      topbarRight,
    },
    state,
    ui: {
      logEvent: () => {},
      setConnState: () => {},
      showError: () => {},
      clearError: () => {},
      showMeetingScreen: () => {},
      showJoinScreen: () => {},
      setJoinControlsEnabled: () => {},
      setShareState: () => {},
      setScreenShareState: () => {},
      setMicState: () => {},
      setRealMicState: () => {},
      setWebcamState: () => {},
      setAudioControl: () => {},
      setVideoControl: () => {},
      setShareControl: () => {},
    },
    hook: { pipelineStats: null },
    cb: {
      syncHarnessHook: () => {},
      startViewerDemandHeartbeat: () => {},
      stopViewerDemandHeartbeat: () => {},
      startLatencyProbe: () => {},
      stopLatencyProbe: () => {},
      startPipelineStats: () => {},
      stopPipelineStats: () => {},
      ensureFrameMetadataWorker: () => null,
      recordRecentRoom: () => {},
      roomDisplayLabelForCredential: () => 'Design Review',
      refreshParticipantGrid: () => {},
      trackedShareWindows: () => [],
      syncStreamStates: () => {},
      stopTelepointerSender: () => {},
      clearTiles: () => {},
      clearRemoteTelepointers: () => {},
      clearRemoteDraw: () => {},
      handleAiChatPayload: () => {},
      aiChatOwnerLeft: () => {},
      resetAiChat: () => {},
      setDrawMode: () => {},
      stopRemoteControl: () => {},
      resetActiveSpeakers: () => {},
      updateParticipantCount: () => {},
      applyTileLayout: () => {},
      ensureBaseTile: () => ({}) as HTMLDivElement,
      updateParticipantShareColorProfiles: () => {},
      handleRemoteControlPayload: () => {},
      handleLatencyProbePayload: () => {},
      handlePipelineStatsPayload: () => {},
      handleRemoteDrawPayload: () => {},
      handleRemoteTelepointerPayload: () => {},
      handleViewerDemandPayload,
      setTileCamera: () => {},
      clearTileCamera: () => {},
      addShareTile: () => {},
      setPublicationPaused: () => {},
      isCameraTrack: () => false,
      publicationPaused: () => false,
      setParticipantAudioActive: () => {},
      removeShareTile: () => {},
      removeParticipantTiles: () => {},
      startSpeakerSmoothing: () => {},
      smoothSpeakingScores: () => {},
    },
    activeSpeakerTargets: new Set(),
  } as unknown as HarnessContext;
  return { ctx, state };
}

test('a viewer-demand DataReceived packet is routed to cb.handleViewerDemandPayload', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = new FakeElement();
    const calls: Array<{ payload: Uint8Array; senderIdentity: string | undefined }> = [];
    const handleViewerDemandPayload = (payload: Uint8Array, senderIdentity?: string) => {
      calls.push({ payload, senderIdentity });
    };
    const { ctx, state } = makeConnectionContext(topbarRight, handleViewerDemandPayload);
    const rooms: FakeRoom[] = [];
    const createRoom = () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    };

    await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
    if (state.viewerDemandTimer !== null) clearInterval(state.viewerDemandTimer);
    if (state.pipelineStatsTimer !== null) clearInterval(state.pipelineStatsTimer);
    if (state.publicationReconcileTimer !== null) clearInterval(state.publicationReconcileTimer);

    const room = rooms[0]!;
    const payload = new TextEncoder().encode(
      JSON.stringify({
        v: 2,
        kind: 'heartbeat',
        targetUserId: 'web-riley',
        viewerId: 'native-quinn',
        windowId: 42,
        seq: 1,
        visible: true,
        width: 0,
        height: 0,
        scale: 1,
        pixelWidth: 0,
        pixelHeight: 0,
        needsRepublish: true,
      })
    );
    const participant = { identity: 'native-quinn' };
    room.emit(RoomEvent.DataReceived, payload, participant, 'RELIABLE', VIEWER_DEMAND_TOPIC);

    assert.equal(calls.length, 1, 'handleViewerDemandPayload must be called exactly once for a viewer-demand packet');
    assert.equal(calls[0]!.senderIdentity, 'native-quinn');
    assert.deepEqual(JSON.parse(new TextDecoder().decode(calls[0]!.payload)).windowId, 42);
  } finally {
    fakeDom.restore();
  }
});
