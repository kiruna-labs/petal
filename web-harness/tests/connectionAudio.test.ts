import { test } from 'node:test';
import assert from 'node:assert/strict';
import { RoomEvent, Track, type Room } from 'livekit-client';

import { setupConnection } from '../src/connection.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import type { HarnessContext } from '../src/context.ts';

const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);

function nextTurn(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

function audioStatsReport(): RTCStatsReport {
  return new Map([
    ['audio', { type: 'inbound-rtp', kind: 'audio', packetsReceived: 1, payloadType: 111 }],
  ]) as unknown as RTCStatsReport;
}

type FakeHandler = (...args: unknown[]) => void;

class FakeClassList {
  private values = new Set<string>();

  add(name: string) {
    this.values.add(name);
  }

  remove(name: string) {
    this.values.delete(name);
  }

  contains(name: string) {
    return this.values.has(name);
  }

  toggle(name: string, force?: boolean) {
    const next = force ?? !this.values.has(name);
    if (next) this.values.add(name);
    else this.values.delete(name);
  }
}

class FakeElement {
  readonly classList = new FakeClassList();
  readonly dataset: Record<string, string> = {};
  readonly style = { display: '' };
  readonly children: FakeElement[] = [];
  readonly listeners = new Map<string, FakeHandler[]>();
  textContent = '';
  type = '';
  className = '';
  parent: FakeElement | null = null;
  attributes = new Map<string, string>();

  readonly tagName: string;

  constructor(tagName: string) {
    this.tagName = tagName;
  }

  appendChild(child: FakeElement) {
    child.parent = this;
    this.children.push(child);
    return child;
  }

  prepend(child: FakeElement) {
    child.parent = this;
    this.children.unshift(child);
    return child;
  }

  remove() {
    if (!this.parent) return;
    this.parent.children.splice(this.parent.children.indexOf(this), 1);
    this.parent = null;
  }

  setAttribute(name: string, value: string) {
    this.attributes.set(name, value);
  }

  getAttribute(name: string) {
    return this.attributes.get(name) ?? null;
  }

  addEventListener(event: string, handler: FakeHandler) {
    this.listeners.set(event, [...(this.listeners.get(event) ?? []), handler]);
  }

  async click() {
    for (const handler of this.listeners.get('click') ?? []) await handler();
  }

  querySelector(selector: string): FakeElement | null {
    for (const child of this.children) {
      if (matchesSelector(child, selector)) return child;
      const nested = child.querySelector(selector);
      if (nested) return nested;
    }
    return null;
  }
}

class FakeDocument {
  readonly body = new FakeElement('body');

  createElement(tagName: string) {
    return new FakeElement(tagName.toLowerCase());
  }

  querySelector(selector: string) {
    return this.body.querySelector(selector);
  }
}

function matchesSelector(element: FakeElement, selector: string) {
  if (selector.startsWith('.')) return element.className.split(/\s+/).includes(selector.slice(1));
  return element.tagName === selector.toLowerCase();
}

function installFakeDom() {
  const originalDocument = globalThis.document;
  const document = new FakeDocument();
  Object.defineProperty(globalThis, 'document', { configurable: true, value: document });
  return {
    document,
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
  startAudioCalls = 0;
  remoteParticipants = new Map<string, unknown>();

  on(event: string, handler: FakeHandler) {
    this.handlers.set(event, [...(this.handlers.get(event) ?? []), handler]);
    return this;
  }

  async connect() {}

  async startAudio() {
    this.startAudioCalls += 1;
    this.canPlaybackAudio = true;
  }

  emit(event: string, ...args: unknown[]) {
    for (const handler of this.handlers.get(event) ?? []) handler(...args);
  }
}

function makeConnectionContext(topbarRight: FakeElement) {
  const logEvents: string[] = [];
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
      logEvent: (message: string) => logEvents.push(message),
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
      // #657 petal.ai-chat callbacks the connection wiring calls on
      // participant-left and disconnect.
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
  return { ctx, state, logEvents };
}

test('AudioPlaybackStatusChanged exposes a user-gesture unlock and clears it after startAudio', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    fakeDom.document.body.appendChild(topbarRight);
    const { ctx, state, logEvents } = makeConnectionContext(topbarRight);
    const rooms: FakeRoom[] = [];
    const createRoom = () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    };

    await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    rooms[0]!.canPlaybackAudio = false;
    rooms[0]!.emit(RoomEvent.AudioPlaybackStatusChanged);

    const prompt = topbarRight.querySelector('.audio-playback-prompt');
    assert.ok(prompt);
    assert.equal(prompt.textContent, 'Enable audio');
    assert.match(logEvents.join('\n'), /remote audio playback is blocked/);

    await prompt.click();

    assert.equal(rooms[0]!.startAudioCalls, 1);
    assert.equal(topbarRight.querySelector('.audio-playback-prompt'), null);
    assert.match(logEvents.join('\n'), /remote audio playback enabled/);
  } finally {
    fakeDom.restore();
  }
});

test('remote audio tracks still attach to hidden audio elements for playback', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state } = makeConnectionContext(topbarRight);
    const rooms: FakeRoom[] = [];
    const createRoom = () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    };
    let attachedElement: FakeElement | null = null;
    let participantAudioActive = false;
    (ctx.cb as unknown as { setParticipantAudioActive: (identity: string, active: boolean) => void }).setParticipantAudioActive = (
      _identity,
      active
    ) => {
      participantAudioActive = active;
    };

    await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    rooms[0]!.emit(
      RoomEvent.TrackSubscribed,
      {
        kind: Track.Kind.Audio,
        sid: 'audio-track',
        attach: (element: FakeElement) => {
          attachedElement = element;
        },
      },
      { trackName: 'mic', trackSid: 'pub-1' },
      { identity: 'remote-riley', name: 'Riley' }
    );

    assert.equal(participantAudioActive, true);
    assert.ok(attachedElement);
    const attached = attachedElement as FakeElement;
    assert.equal(attached.tagName, 'audio');
    assert.equal(attached.dataset.trackSid, 'audio-track');
    assert.equal(attached.dataset.participant, 'remote-riley');
  } finally {
    fakeDom.restore();
  }
});

test('subscribed remote audio starts receiver telemetry and unsubscribed cleanup suppresses late logs', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state, logEvents } = makeConnectionContext(topbarRight);
    const rooms: FakeRoom[] = [];
    const createRoom = () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    };
    let resolveReport!: (report: RTCStatsReport) => void;
    const reportPromise = new Promise<RTCStatsReport>((resolve) => {
      resolveReport = resolve;
    });
    const track = {
      kind: Track.Kind.Audio,
      sid: 'audio-track',
      attach: () => {},
      detach: () => [],
      getRTCStatsReport: async () => reportPromise,
    };
    const publication = { trackName: 'mic', trackSid: 'pub-1' };
    const participant = { identity: 'remote-riley', name: 'Riley' };

    await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    rooms[0]!.emit(RoomEvent.TrackSubscribed, track, publication, participant);
    resolveReport(audioStatsReport());
    await nextTurn();
    assert.match(logEvents.join('\n'), /audio receiver stats 1\/3/);

    // Stop the first successful poll before starting the deferred cleanup case;
    // otherwise its real timer would keep this test fixture alive.
    rooms[0]!.emit(RoomEvent.TrackUnsubscribed, track, publication, participant);

    const telemetryLogs = logEvents.filter((message) => message.includes('audio receiver stats'));
    let lateResolve!: (report: RTCStatsReport) => void;
    const lateReport = new Promise<RTCStatsReport>((resolve) => {
      lateResolve = resolve;
    });
    const lateTrack = { ...track, getRTCStatsReport: async () => lateReport };
    rooms[0]!.emit(RoomEvent.TrackSubscribed, lateTrack, publication, participant);
    rooms[0]!.emit(RoomEvent.TrackUnsubscribed, lateTrack, publication, participant);
    lateResolve(audioStatsReport());
    await nextTurn();
    assert.equal(logEvents.filter((message) => message.includes('audio receiver stats')).length, telemetryLogs.length);
  } finally {
    fakeDom.restore();
  }
});

test('room disconnect cleanup suppresses an in-flight receiver telemetry result', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state, logEvents } = makeConnectionContext(topbarRight);
    const rooms: FakeRoom[] = [];
    const createRoom = () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    };
    let resolveReport!: (report: RTCStatsReport) => void;
    const reportPromise = new Promise<RTCStatsReport>((resolve) => {
      resolveReport = resolve;
    });
    const track = {
      kind: Track.Kind.Audio,
      sid: 'audio-track',
      attach: () => {},
      detach: () => [],
      getRTCStatsReport: async () => reportPromise,
    };

    await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
    rooms[0]!.emit(RoomEvent.TrackSubscribed, track, { trackName: 'mic', trackSid: 'pub-1' }, { identity: 'remote-riley', name: 'Riley' });
    rooms[0]!.emit(RoomEvent.Disconnected);
    resolveReport(audioStatsReport());
    await nextTurn();
    assert.equal(logEvents.some((message) => message.includes('audio receiver stats')), false);
  } finally {
    fakeDom.restore();
  }
});

// #659: `ActiveSpeakersChanged` is per-participant, aggregate over every
// audio track that identity publishes -- including the AI chat assistant's
// voice, published under the sharer's own identity but deliberately never
// muted by the room mic-mute button. A muted mic transmits zero energy, so
// a muted identity reported as "speaking" can only be some other track
// (the assistant), never their own voice.
test('a muted participant never lights the speaking ring even if LiveKit reports it', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state } = makeConnectionContext(topbarRight);
    const rooms: FakeRoom[] = [];
    const createRoom = () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    };

    await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    rooms[0]!.emit(RoomEvent.ActiveSpeakersChanged, [
      { identity: 'muted-sharer', isMicrophoneEnabled: false },
      { identity: 'genuine-speaker', isMicrophoneEnabled: true },
    ]);

    assert.equal(
      ctx.activeSpeakerTargets.has('muted-sharer'),
      false,
      "a muted identity's speaking flag must not light up -- the assistant's voice (or any \
       other non-mic track) cannot be their own speech"
    );
    assert.equal(
      ctx.activeSpeakerTargets.has('genuine-speaker'),
      true,
      'an unmuted participant genuinely reported as speaking must still light up'
    );
  } finally {
    fakeDom.restore();
  }
});
