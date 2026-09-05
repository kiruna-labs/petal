import { test } from 'node:test';
import assert from 'node:assert/strict';
import { RoomEvent, Track, type Room } from 'livekit-client';

import { setupConnection } from '../src/connection.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import { SensitiveStringRegistry } from '../src/sensitiveStrings.ts';
import type { HarnessContext } from '../src/context.ts';

const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);

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

function installBrowserGlobals(fetchImpl?: typeof fetch) {
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
    value:
      fetchImpl ??
      (async () => ({
        ok: true,
        json: async () => ({
          url: 'wss://livekit.invalid',
          token: 'token',
          room: `petal-room-${CREDENTIAL}`,
        }),
      })),
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

  emit(event: string, ...args: unknown[]) {
    for (const handler of this.handlers.get(event) ?? []) handler(...args);
  }
}

function makeConnectionContext(topbarRight: FakeElement) {
  const logEvents: Array<{ message: string; kind?: string }> = [];
  const removedParticipants: string[] = [];
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
      // #679: the remote-share-started toast host. Real HarnessDom always
      // has this (context.ts); the fixture just lagged.
      toastEl: new FakeElement('div'),
    },
    state,
    ui: {
      logEvent: (message: string, kind?: string) => logEvents.push({ message, kind }),
      setConnState: () => {},
      showError: () => {},
      clearError: () => {},
      showMeetingScreen: () => {},
      showJoinScreen: () => {},
      // #679: real HarnessUi always has this (context.ts); the fixture just
      // lagged.
      showActionableToast: () => {},
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
      removeParticipantTiles: (identity: string) => removedParticipants.push(identity),
      publishViewerDemandForPublication: () => {},
      startSpeakerSmoothing: () => {},
      smoothSpeakingScores: () => {},
      // #679: real HarnessCallbacks always has these (context.ts); the
      // fixture just lagged.
      shareTileForWindowId: () => null,
      pinTile: () => {},
    },
    activeSpeakerTargets: new Set(),
  } as unknown as HarnessContext;
  return { ctx, state, logEvents, removedParticipants };
}

test('RoomEvent.TrackSubscriptionFailed logs an error via logEvent (previously unhandled, #283)', async () => {
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

    await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    rooms[0]!.emit(RoomEvent.TrackSubscriptionFailed, 'track-sid-1', { identity: 'remote-sam' }, 'server-error');

    const failure = logEvents.find((e) => /track subscription failed/.test(e.message));
    assert.ok(failure, 'expected a track-subscription-failed log line');
    assert.equal(failure!.kind, 'error');
    assert.match(failure!.message, /remote-sam/);
    assert.match(failure!.message, /track-sid-1/);
  } finally {
    fakeDom.restore();
  }
});

test('RoomEvent.MediaDevicesError logs an error via logEvent (previously unhandled, #283)', async () => {
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

    await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    rooms[0]!.emit(RoomEvent.MediaDevicesError, new Error('NotReadableError: device in use'), 'videoinput');

    const failure = logEvents.find((e) => /media devices error/.test(e.message));
    assert.ok(failure, 'expected a media-devices-error log line');
    assert.equal(failure!.kind, 'error');
    assert.match(failure!.message, /videoinput/);
    assert.match(failure!.message, /device in use/);
  } finally {
    fakeDom.restore();
  }
});

test('a failed token request now logs to the session log, not just the visible error banner (#283)', async () => {
  const fetchImpl: typeof fetch = async () =>
    ({
      ok: false,
      status: 500,
      headers: new Headers(),
      json: async () => ({ error: 'internal error' }),
    }) as unknown as Response;
  installBrowserGlobals(fetchImpl);
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, logEvents } = makeConnectionContext(topbarRight);

    await setupConnection(ctx, () => new FakeRoom() as unknown as Room, undefined).connectToMeeting(
      CREDENTIAL,
      'web-riley'
    );

    const failure = logEvents.find((e) => /token request failed/.test(e.message));
    assert.ok(failure, 'expected a token-request-failed log line (this was previously silent)');
    assert.equal(failure!.kind, 'error');
  } finally {
    fakeDom.restore();
  }
});

test('connecting registers the room and local identity in the sensitive-string registry before any log line', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state } = makeConnectionContext(topbarRight);
    const registry = new SensitiveStringRegistry();
    const createRoom = () => new FakeRoom() as unknown as Room;

    await setupConnection(ctx, createRoom, registry).connectToMeeting(CREDENTIAL, 'web-riley-secret');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    assert.equal(registry.scrub(CREDENTIAL), '<redacted:room>');
    assert.equal(registry.scrub('web-riley-secret'), '<redacted:participant-1>');
  } finally {
    fakeDom.restore();
  }
});

test('a joining remote participant is registered, and a departing one is unregistered', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state } = makeConnectionContext(topbarRight);
    const registry = new SensitiveStringRegistry();
    const rooms: FakeRoom[] = [];
    const createRoom = () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    };

    await setupConnection(ctx, createRoom, registry).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    rooms[0]!.emit(RoomEvent.ParticipantConnected, { identity: 'remote-jordan' });
    assert.equal(registry.scrub('remote-jordan'), '<redacted:participant-2>');

    rooms[0]!.emit(RoomEvent.ParticipantDisconnected, { identity: 'remote-jordan' });
    assert.equal(registry.scrub('remote-jordan'), 'remote-jordan');
  } finally {
    fakeDom.restore();
  }
});

test('#709: a participant already in the room when connect() resolves is registered for scrubbing without waiting for ParticipantConnected', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state } = makeConnectionContext(topbarRight);
    const registry = new SensitiveStringRegistry();
    const rooms: FakeRoom[] = [];
    const createRoom = () => {
      const room = new FakeRoom();
      // Simulates the real LiveKit SDK's behavior: `remoteParticipants` is
      // already populated with pre-existing room members the moment
      // `connect()` resolves, with no `ParticipantConnected` event ever
      // firing for them (that event is reserved for participants who join
      // AFTER this client).
      room.remoteParticipants.set('1ab294e1-7ed8-4a11-9c2e-abcdef012345', {
        identity: '1ab294e1-7ed8-4a11-9c2e-abcdef012345',
        name: 'Bob',
      });
      rooms.push(room);
      return room as unknown as Room;
    };

    await setupConnection(ctx, createRoom, registry).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    assert.equal(
      registry.scrub('1ab294e1-7ed8-4a11-9c2e-abcdef012345'),
      '<redacted:participant-2>',
      'the pre-existing participant identity must be scrubbed even though ParticipantConnected never fired for it'
    );
    assert.equal(
      registry.scrub('participant left: Bob'),
      'participant left: <redacted:session-value>',
      'the pre-existing participant display name must also be scrubbed'
    );
  } finally {
    fakeDom.restore();
  }
});

test('a stale participant disconnect cannot remove a same-identity replacement', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state, removedParticipants } = makeConnectionContext(topbarRight);
    const rooms: FakeRoom[] = [];
    const createRoom = () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    };

    await setupConnection(ctx as unknown as HarnessContext, createRoom).connectToMeeting(
      CREDENTIAL,
      'web-riley'
    );
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    const retired = { identity: 'remote-jordan' };
    const replacement = { identity: 'remote-jordan' };
    rooms[0]!.remoteParticipants.set('remote-jordan', replacement);
    rooms[0]!.emit(RoomEvent.ParticipantDisconnected, retired);

    assert.deepEqual(removedParticipants, []);
  } finally {
    fakeDom.restore();
  }
});

test('remote window publish and unpublish are recorded without changing subscription behavior', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state } = makeConnectionContext(topbarRight);
    const events: string[] = [];
    (ctx as any).hook = {
      pipelineStats: {
        trackPublished: (owner: string, windowId: number, sid: string) => events.push(`published:${owner}:${windowId}:${sid}`),
        trackUnpublished: (owner: string, windowId: number, sid: string) => events.push(`unpublished:${owner}:${windowId}:${sid}`),
      },
    };
    const rooms: FakeRoom[] = [];
    await setupConnection(ctx, () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    }).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    const publication = { kind: Track.Kind.Video, trackName: 'petal-window-42', trackSid: 'TR_startup' };
    const participant = { identity: 'remote-sam' };
    rooms[0]!.emit(RoomEvent.TrackPublished, publication, participant);
    rooms[0]!.emit(RoomEvent.TrackUnpublished, publication, participant);

    assert.deepEqual(events, [
      'published:remote-sam:42:TR_startup',
      'unpublished:remote-sam:42:TR_startup',
    ]);
  } finally {
    fakeDom.restore();
  }
});

test('TrackPublished invokes early window demand before TrackSubscribed adds the share tile', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state } = makeConnectionContext(topbarRight);
    const events: string[] = [];
    (ctx as any).hook = {};
    ctx.cb.publishViewerDemandForPublication = (owner, publication) => {
      events.push(`demand:${owner}:${publication.trackName}`);
    };
    ctx.cb.addShareTile = () => {
      events.push('subscribed');
    };
    const rooms: FakeRoom[] = [];
    await setupConnection(ctx, () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    }).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    const publication = {
      kind: Track.Kind.Video,
      trackName: 'petal-window-42',
      trackSid: 'TR_startup',
    };
    const participant = { identity: 'remote-sam' };
    rooms[0]!.emit(RoomEvent.TrackPublished, publication, participant);

    assert.deepEqual(events, ['demand:remote-sam:petal-window-42']);

    rooms[0]!.emit(RoomEvent.TrackSubscribed, { kind: Track.Kind.Video }, publication, participant);
    assert.deepEqual(events, ['demand:remote-sam:petal-window-42', 'subscribed']);
  } finally {
    fakeDom.restore();
  }
});

test('#679: a new remote share tile fires an actionable "is sharing a window" toast, but a republish of the same window does not', async () => {
  installBrowserGlobals();
  const fakeDom = installFakeDom();
  try {
    const topbarRight = fakeDom.document.createElement('div');
    const { ctx, state } = makeConnectionContext(topbarRight);
    (ctx as any).hook = {};

    // Tracks which windowIds currently have a live tile, mirroring the real
    // `shareTileForWindowId`/`addShareTile` relationship closely enough to
    // exercise the suppression check in attachRemoteShareTrack: the FIRST
    // TrackSubscribed for a windowId finds no existing tile (fires the
    // toast); a SECOND TrackSubscribed for the SAME windowId (a
    // republish/quality-switch) finds the tile `addShareTile` just created
    // and must not re-fire it.
    const tilesByWindowId = new Set<number>();
    ctx.cb.shareTileForWindowId = (windowId: number) => (tilesByWindowId.has(windowId) ? ({} as HTMLDivElement) : null);
    ctx.cb.addShareTile = (_identity, _isLocal, _key, _track, _label, windowId) => {
      if (windowId !== null && windowId !== undefined) tilesByWindowId.add(windowId);
    };
    const toasts: Array<{ message: string; dismissMs: number; actionLabel?: string }> = [];
    ctx.ui.showActionableToast = (message: string, dismissMs: number, action?: { actionLabel: string; onAction: () => void }) => {
      toasts.push({ message, dismissMs, actionLabel: action?.actionLabel });
    };

    const rooms: FakeRoom[] = [];
    await setupConnection(ctx, () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    }).connectToMeeting(CREDENTIAL, 'web-riley');
    if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

    const publication = { kind: Track.Kind.Video, trackName: 'petal-window-77', trackSid: 'TR_share' };
    const participant = { identity: 'remote-sam', name: 'Sam', metadata: undefined };

    rooms[0]!.emit(RoomEvent.TrackSubscribed, { kind: Track.Kind.Video }, publication, participant);
    assert.deepEqual(toasts, [{ message: 'Sam is sharing a window', dismissMs: 4000, actionLabel: 'Bring to front' }]);

    // Republish under the SAME window id (quality-switch unpublish +
    // republish) -- must NOT fire a second toast.
    rooms[0]!.emit(RoomEvent.TrackSubscribed, { kind: Track.Kind.Video }, publication, participant);
    assert.deepEqual(
      toasts,
      [{ message: 'Sam is sharing a window', dismissMs: 4000, actionLabel: 'Bring to front' }],
      'a republish of an already-open share must not re-fire the notice'
    );
  } finally {
    fakeDom.restore();
  }
});
