import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ConnectionError, DisconnectReason, RoomEvent, type Room } from 'livekit-client';

import { autoJoinFromUrl } from '../src/deepLink.ts';
import { HARNESS_NAME_STORAGE_KEY, HARNESS_REJOIN_SESSION_KEY, HARNESS_ROOM_STORAGE_KEY } from '../src/constants.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import { setupConnection } from '../src/connection.ts';
import { inviteLinkForCredential } from '../src/controls.ts';
import type { HarnessContext } from '../src/context.ts';
import { noteLeaveRequested } from '../src/analytics.ts';
import { SensitiveStringRegistry } from '../src/sensitiveStrings.ts';
import { setupMeetingContinuity } from '../src/meetingContinuity.ts';
import { FakeBrowserPage, FakeDialog, FakeElement, settle } from './fixtures/fakeBrowserPage.ts';

const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);
const ORIGIN = 'https://meet.petal.live';
type FakeHandler = (...args: unknown[]) => void;

class MemoryStorage implements Storage {
  private values = new Map<string, string>();

  get length() {
    return this.values.size;
  }

  clear() {
    this.values.clear();
  }

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  key(index: number) {
    return Array.from(this.values.keys())[index] ?? null;
  }

  removeItem(key: string) {
    this.values.delete(key);
  }

  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

function installBrowserGlobals(url: string, storedName?: string) {
  const storage = new MemoryStorage();
  if (storedName) storage.setItem(HARNESS_NAME_STORAGE_KEY, storedName);
  Object.defineProperty(globalThis, 'location', {
    configurable: true,
    value: new URL(url),
  });
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: storage,
  });
}

function installHistoryMock() {
  const calls: Array<[unknown, string, string]> = [];
  Object.defineProperty(globalThis, 'history', {
    configurable: true,
    value: {
      state: null,
      replaceState(state: unknown, title: string, url: string) {
        calls.push([state, title, url]);
      },
    },
  });
  return calls;
}

function makeInput(value = '') {
  return {
    value,
    focused: false,
    focus() {
      this.focused = true;
    },
  } as HTMLInputElement & { focused: boolean };
}

function makeHint() {
  const classes = new Set(['hidden']);
  return {
    textContent: '',
    classList: {
      add(name: string) {
        classes.add(name);
      },
      remove(name: string) {
        classes.delete(name);
      },
      contains(name: string) {
        return classes.has(name);
      },
    },
  } as HTMLElement;
}

function makeConnectionContext(displayName: string, displayLabel: string) {
  const shareBtn = { disabled: false, textContent: '' };
  const micCheckbox = { disabled: false, checked: false };
  const displayNameInput = { value: displayName };
  const cameraTrackNameDisplay = { textContent: '' };
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
  };
  const uiCalls = {
    meetingScreens: [] as string[],
    joinScreens: 0,
  };
  const ctx = {
    windowId: 1,
    hook: {
      pipelineStats: null,
    },
    dom: {
      shareBtn,
      micCheckbox,
      displayNameInput,
      cameraTrackNameDisplay,
    },
    state,
    ui: {
      logEvent: () => {},
      setConnState: () => {},
      showError: () => {},
      clearError: () => {},
      showMeetingScreen: (code: string) => uiCalls.meetingScreens.push(code),
      showJoinScreen: () => {
        uiCalls.joinScreens += 1;
      },
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
      ensureFrameMetadataWorker: () => null,
      recordRecentRoom: () => {},
      roomDisplayLabelForCredential: () => displayLabel,
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
  return { ctx, state, uiCalls };
}

class FakeRoom {
  private handlers = new Map<string, FakeHandler[]>();
  remoteParticipants = new Map<string, unknown>();

  on(event: string, handler: FakeHandler) {
    const existing = this.handlers.get(event) ?? [];
    existing.push(handler);
    this.handlers.set(event, existing);
    return this;
  }

  async connect() {}

  emit(event: string, ...args: unknown[]) {
    for (const handler of this.handlers.get(event) ?? []) {
      handler(...args);
    }
  }
}

function makeFakeRoomFactory() {
  const rooms: FakeRoom[] = [];
  return {
    rooms,
    createRoom: () => {
      const room = new FakeRoom();
      rooms.push(room);
      return room as unknown as Room;
    },
  };
}

// #244: the Back guard / reload rejoin / disconnect notice layer, wired the
// way main.ts does (ctx.hook.continuity), on a fake page whose location +
// history are the globals connection.ts writes to.
function installMeetingPage(url: string, ctx: HarnessContext) {
  const page = new FakeBrowserPage(url);
  page.installGlobals();
  const title = new FakeElement();
  const notices: string[] = [];
  ctx.hook.continuity = setupMeetingContinuity({
    leaveDialog: new FakeDialog() as unknown as HTMLDialogElement,
    notice: {
      title,
      detail: new FakeElement(),
      rejoin: new FakeElement('btn primary'),
      home: new FakeElement('btn ghost'),
      announcer: new FakeElement('sr-only'),
    } as unknown as Parameters<typeof setupMeetingContinuity>[0]['notice'],
    showJoinScreen: () => ctx.ui.showJoinScreen(),
    showDisconnectedScreen: () => notices.push(title.textContent),
    leave: async () => {},
    rejoin: async () => true,
    logEvent: () => {},
    win: page.window as unknown as Window,
  });
  const stored = () => page.sessionStorage.getItem(HARNESS_REJOIN_SESSION_KEY);
  return { page, notices, stored };
}

function installFetchMock() {
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

test('invite URL waits for a display name instead of auto-joining with a generated identity', async () => {
  installBrowserGlobals(`https://meet.petal.live/testing/${ACCESS_CODE}`);
  const displayNameInput = makeInput();
  const meetingCodeInput = makeInput();
  const joinHint = makeHint();
  const events: string[] = [];
  let connected = false;
  let ctaUpdated = false;

  autoJoinFromUrl({
    displayNameInput,
    meetingCodeInput,
    joinHint,
    logEvent: (message) => events.push(message),
    connectToMeeting: async () => {
      connected = true;
    },
    resolveIdentity: () => {
      throw new Error('resolveIdentity should not run without a display name');
    },
    showError: (message) => events.push(`error:${message}`),
    updateUnifiedCtaLabel: () => {
      ctaUpdated = true;
    },
  });

  assert.equal(meetingCodeInput.value, ACCESS_CODE);
  assert.equal(connected, false);
  assert.equal(ctaUpdated, true);
  assert.equal((displayNameInput as typeof displayNameInput & { focused: boolean }).focused, true);
  assert.equal(joinHint.textContent, 'Enter your name to join this invite.');
  assert.equal(joinHint.classList.contains('hidden'), false);
  assert.match(events.join('\n'), /waiting for display name/);
});

test('invite URL auto-joins when a display name is already stored', async () => {
  installBrowserGlobals(`https://meet.petal.live/?code=${ACCESS_CODE}`, 'Riley');
  const displayNameInput = makeInput('Riley');
  const meetingCodeInput = makeInput();
  const joinHint = makeHint();
  const connected: Array<{ code: string; identity: string }> = [];

  autoJoinFromUrl({
    displayNameInput,
    meetingCodeInput,
    joinHint,
    logEvent: () => {},
    connectToMeeting: async (code, identity) => {
      connected.push({ code, identity });
    },
    resolveIdentity: () => displayNameInput.value,
    showError: () => {},
    updateUnifiedCtaLabel: () => {},
  });

  assert.deepEqual(connected, [{ code: CREDENTIAL, identity: 'Riley' }]);
  assert.equal(meetingCodeInput.value, ACCESS_CODE);
  assert.equal(joinHint.classList.contains('hidden'), true);
});

test('auto-join swaps to the connecting interstitial instead of flashing the menu', async () => {
  installBrowserGlobals(`https://meet.petal.live/?code=${ACCESS_CODE}`, 'Riley');
  const displayNameInput = makeInput('Riley');
  const meetingCodeInput = makeInput();
  const joinHint = makeHint();
  const timeline: string[] = [];

  autoJoinFromUrl({
    displayNameInput,
    meetingCodeInput,
    joinHint,
    logEvent: () => {},
    connectToMeeting: async () => {
      timeline.push('connect');
    },
    resolveIdentity: () => 'web-riley',
    showError: () => {},
    updateUnifiedCtaLabel: () => {},
    showConnectingScreen: (label) => timeline.push(`connecting-screen:${label}`),
  });

  // The interstitial must be up BEFORE the (potentially slow) connect starts,
  // and it must show the public access code, never the internal credential.
  assert.deepEqual(timeline, [`connecting-screen:${ACCESS_CODE}`, 'connect']);
});

test('waiting for a display name never shows the connecting interstitial', async () => {
  installBrowserGlobals(`https://meet.petal.live/testing/${ACCESS_CODE}`);
  const displayNameInput = makeInput();
  const meetingCodeInput = makeInput();
  const joinHint = makeHint();
  let connectingShown = false;

  autoJoinFromUrl({
    displayNameInput,
    meetingCodeInput,
    joinHint,
    logEvent: () => {},
    connectToMeeting: async () => {},
    resolveIdentity: () => 'web-riley',
    showError: () => {},
    updateUnifiedCtaLabel: () => {},
    showConnectingScreen: () => {
      connectingShown = true;
    },
  });

  assert.equal(connectingShown, false);
});

test('a failed join lands back on the join screen with the error surfaced', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  // Non-transient rejection so the token retry ladder exits immediately.
  Object.defineProperty(globalThis, 'fetch', {
    configurable: true,
    value: async () => ({
      ok: false,
      status: 403,
      json: async () => ({ error: 'invalid room credential' }),
    }),
  });
  installHistoryMock();
  const { ctx, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const errors: string[] = [];
  (ctx.ui as { showError: (message: string) => void }).showError = (message) => errors.push(message);
  const { createRoom } = makeFakeRoomFactory();

  await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');

  // resetFailedJoinUi must dismiss any connecting interstitial by returning
  // to the join screen -- a dead spinner is never an acceptable end state.
  assert.equal(uiCalls.joinScreens, 1);
  assert.equal(errors.length, 1);
  assert.match(errors[0]!, /invalid room credential/);
});

test('successful connection puts the shareable invite URL in the address bar, under a Back guard', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const { ctx, state, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const { page, stored } = installMeetingPage(`${ORIGIN}/`, ctx);
  const { createRoom } = makeFakeRoomFactory();
  const expectedUrl = inviteLinkForCredential(CREDENTIAL, ORIGIN, 'Design Review');

  await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');

  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
  assert.deepEqual(
    page.history.calls.map(([kind, , url]) => [kind, url]),
    [
      ['replace', expectedUrl],
      ['push', expectedUrl],
    ]
  );
  assert.equal(page.history.index, 1);
  // #244: this tab now rejoins when that URL is reloaded (api/j.ts).
  assert.equal(stored(), ACCESS_CODE);
  assert.deepEqual(uiCalls.meetingScreens, [CREDENTIAL]);
});

test('leaving resets the address bar to the bare origin and steps off the Back guard', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const { ctx, state, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const { page, notices, stored } = installMeetingPage(`${ORIGIN}/`, ctx);
  const { createRoom, rooms } = makeFakeRoomFactory();

  await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

  noteLeaveRequested();
  rooms[0]!.emit(RoomEvent.Disconnected, DisconnectReason.CLIENT_INITIATED);
  await settle();

  assert.deepEqual(page.history.urls(), [`${ORIGIN}/`, `${ORIGIN}/`]);
  assert.equal(page.history.index, 0, 'Back from home goes where it went before the meeting');
  assert.equal(stored(), null);
  assert.equal(state.currentMeetingCode, null);
  assert.equal(uiCalls.joinScreens, 1);
  assert.deepEqual(notices, []);
});

test('the scrub registry is cleared only after the guard has unwound, and never over a newer join', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const { ctx, state } = makeConnectionContext('Riley', 'Design Review');
  installMeetingPage(`${ORIGIN}/`, ctx);
  const { createRoom, rooms } = makeFakeRoomFactory();
  const registry = new SensitiveStringRegistry();
  const connection = setupConnection(ctx, createRoom, registry);

  await connection.connectToMeeting(CREDENTIAL, 'web-riley');
  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
  noteLeaveRequested();
  rooms[0]!.emit(RoomEvent.Disconnected, DisconnectReason.CLIENT_INITIATED);
  assert.equal(registry.scrub(`/design-review/${ACCESS_CODE}`), '/design-review/<redacted:room>', 'still scrubbed while unwinding');
  await settle();
  assert.equal(registry.scrub(ACCESS_CODE), ACCESS_CODE, 'reset once settled');

  await connection.connectToMeeting(CREDENTIAL, 'web-riley');
  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
  noteLeaveRequested();
  rooms[1]!.emit(RoomEvent.Disconnected, DisconnectReason.CLIENT_INITIATED);
  const nextJoin = connection.connectToMeeting(CREDENTIAL, 'web-riley');
  await settle();
  await nextJoin;
  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
  assert.equal(registry.scrub(ACCESS_CODE), '<redacted:room>', 'a join that started meanwhile keeps its registrations');
});

test('a disconnect the user did not ask for keeps the invite URL and explains itself instead of going home', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const { ctx, state, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const { page, notices, stored } = installMeetingPage(`${ORIGIN}/`, ctx);
  const { createRoom, rooms } = makeFakeRoomFactory();
  const expectedUrl = inviteLinkForCredential(CREDENTIAL, ORIGIN, 'Design Review');

  await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

  rooms[0]!.emit(RoomEvent.Disconnected, DisconnectReason.PARTICIPANT_REMOVED);
  await settle();

  assert.deepEqual(notices, ['Removed from the meeting']);
  assert.equal(uiCalls.joinScreens, 0, 'no silent jump to the home screen');
  assert.equal(page.location.href, expectedUrl, 'a reload still rejoins');
  assert.equal(stored(), ACCESS_CODE, 'the rejoin code is kept');
  assert.equal(state.currentMeetingCode, null);
});

test('a failed rejoin from the notice stays on the notice with the real error, never the home screen', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const { ctx, state, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const errors: string[] = [];
  (ctx.ui as { showError: (message: string) => void }).showError = (message) => errors.push(message);
  const page = new FakeBrowserPage(`${ORIGIN}/`);
  page.installGlobals();
  const title = new FakeElement();
  const detail = new FakeElement();
  const rejoinButton = new FakeElement('btn primary');
  const { createRoom, rooms } = makeFakeRoomFactory();
  const connection = setupConnection(ctx, createRoom);
  let rejoined: Promise<void> | null = null;
  ctx.hook.continuity = setupMeetingContinuity({
    leaveDialog: new FakeDialog() as unknown as HTMLDialogElement,
    notice: {
      title,
      detail,
      rejoin: rejoinButton,
      home: new FakeElement('btn ghost'),
      announcer: new FakeElement('sr-only'),
    } as unknown as Parameters<typeof setupMeetingContinuity>[0]['notice'],
    showJoinScreen: () => ctx.ui.showJoinScreen(),
    showDisconnectedScreen: () => {},
    leave: async () => {},
    // As main.ts wires it.
    rejoin: async (code) => {
      rejoined = connection.connectToMeeting(code, 'web-riley');
      await rejoined;
      return state.room !== null;
    },
    logEvent: () => {},
    win: page.window as unknown as Window,
  });
  await connection.connectToMeeting(CREDENTIAL, 'web-riley');
  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
  rooms[0]!.emit(RoomEvent.Disconnected, DisconnectReason.PARTICIPANT_REMOVED);
  await settle();

  // The backend now refuses (non-transient, so no retry ladder).
  Object.defineProperty(globalThis, 'fetch', {
    configurable: true,
    value: async () => ({ ok: false, status: 403, json: async () => ({ error: 'invalid room credential' }) }),
  });
  rejoinButton.click();
  await settle();
  await rejoined;
  await settle();

  assert.equal(uiCalls.joinScreens, 0, 'the home screen never flashes');
  assert.deepEqual(errors, [], 'the error is not parked on the hidden home screen');
  assert.equal(title.textContent, 'Removed from the meeting');
  assert.equal(detail.textContent, 'Token request failed: invalid room credential');

  // Outside a rejoin the same failure still lands on the home screen with the error.
  await connection.connectToMeeting(CREDENTIAL, 'web-riley');
  assert.equal(uiCalls.joinScreens, 1);
  assert.deepEqual(errors, ['Token request failed: invalid room credential']);
});

// livekit-client's Room.connect() tears the room down, emitting Disconnected,
// before it rejects a failed attempt. Each room takes the next step of the plan.
type ConnectPlan = 'ok' | 'fail-transient' | 'fail-final';
function makeScriptedRoomFactory(plan: ConnectPlan[]) {
  const rooms: FakeRoom[] = [];
  class ScriptedRoom extends FakeRoom {
    async connect() {
      const step = plan.shift() ?? 'ok';
      if (step === 'ok') return;
      const transient = step === 'fail-transient';
      this.emit(RoomEvent.Disconnected, transient ? DisconnectReason.JOIN_FAILURE : DisconnectReason.UNKNOWN_REASON);
      throw transient
        ? ConnectionError.serverUnreachable('could not establish signal connection')
        : ConnectionError.notAllowed('could not establish signal connection', 403);
    }
  }
  return {
    rooms,
    createRoom: () => {
      const room = new ScriptedRoom();
      rooms.push(room);
      return room as unknown as Room;
    },
  };
}

// The notice and rejoin wiring main.ts uses, over a real setupConnection.
function installNoticePage(ctx: HarnessContext, connection: ReturnType<typeof setupConnection>) {
  const page = new FakeBrowserPage(`${ORIGIN}/`);
  page.installGlobals();
  const title = new FakeElement();
  const detail = new FakeElement();
  const rejoin = new FakeElement('btn primary');
  const announcer = new FakeElement('sr-only');
  const notices: string[] = [];
  const rejoins: string[] = [];
  ctx.hook.continuity = setupMeetingContinuity({
    leaveDialog: new FakeDialog() as unknown as HTMLDialogElement,
    notice: { title, detail, rejoin, home: new FakeElement('btn ghost'), announcer } as unknown as Parameters<
      typeof setupMeetingContinuity
    >[0]['notice'],
    showJoinScreen: () => ctx.ui.showJoinScreen(),
    showDisconnectedScreen: () => notices.push(title.textContent),
    leave: async () => {},
    rejoin: async (code) => {
      rejoins.push(code);
      await connection.connectToMeeting(code, 'web-riley');
      return ctx.state.room !== null;
    },
    logEvent: () => {},
    win: page.window as unknown as Window,
  });
  return { page, title, detail, rejoin, announcer, notices, rejoins };
}

test('a first connect attempt that fails and is retried never shows the notice or starts a second join', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const { ctx, state, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const { rooms, createRoom } = makeScriptedRoomFactory(['fail-transient', 'ok']);
  const connection = setupConnection(ctx, createRoom);
  const { page, notices, rejoins, announcer } = installNoticePage(ctx, connection);
  page.hide();
  page.show(); // an earlier tab switch: a real drop now would rejoin by itself

  await connection.connectToMeeting(CREDENTIAL, 'web-riley'); // retries after 1 s
  await settle();
  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

  assert.deepEqual(notices, [], 'no notice over "Joining…"');
  assert.equal(announcer.textContent, '');
  assert.deepEqual(rejoins, [], 'no automatic rejoin racing the retry ladder');
  assert.equal(rooms.length, 1);
  assert.equal(uiCalls.joinScreens, 0);
  assert.deepEqual(uiCalls.meetingScreens, [CREDENTIAL]);
});

test('a Rejoin after a kick whose LiveKit connect fails keeps "Removed from the meeting" and shows the error', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const { ctx, state, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const { rooms, createRoom } = makeScriptedRoomFactory(['ok', 'fail-final']);
  const connection = setupConnection(ctx, createRoom);
  const { title, detail, rejoin, notices } = installNoticePage(ctx, connection);
  await connection.connectToMeeting(CREDENTIAL, 'web-riley');
  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
  rooms[0]!.emit(RoomEvent.Disconnected, DisconnectReason.PARTICIPANT_REMOVED);
  await settle();

  rejoin.click();
  await settle(20);

  assert.equal(title.textContent, 'Removed from the meeting');
  assert.equal(detail.textContent, 'Connect failed: could not establish signal connection');
  assert.ok(notices.every((shown) => shown === 'Removed from the meeting'), JSON.stringify(notices));
  assert.equal(uiCalls.joinScreens, 0);
});

test('a join from home whose connect fails goes back home with the error and no notice', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const { ctx, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const errors: string[] = [];
  (ctx.ui as { showError: (message: string) => void }).showError = (message) => errors.push(message);
  const { createRoom } = makeScriptedRoomFactory(['fail-final']);
  const connection = setupConnection(ctx, createRoom);
  const { notices, announcer } = installNoticePage(ctx, connection);

  await connection.connectToMeeting(CREDENTIAL, 'web-riley');
  await settle();

  assert.deepEqual(notices, []);
  assert.equal(announcer.textContent, '', 'nothing announced');
  assert.equal(uiCalls.joinScreens, 1);
  assert.deepEqual(errors, ['Connect failed: could not establish signal connection']);
});

test('without the #244 layer wired (partial contexts), a disconnect still resets to the bare origin', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const historyCalls = installHistoryMock();
  const { ctx, state, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const { createRoom, rooms } = makeFakeRoomFactory();
  const expectedUrl = inviteLinkForCredential(CREDENTIAL, ORIGIN, 'Design Review');

  await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');
  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);

  rooms[0]!.emit(RoomEvent.Disconnected);

  assert.deepEqual(historyCalls, [
    [null, '', expectedUrl],
    [null, '', ORIGIN],
  ]);
  assert.equal(state.currentMeetingCode, null);
  assert.equal(uiCalls.joinScreens, 1);
});

test('loading an invite URL and auto-joining preserves the same shareable URL', async () => {
  const sharedUrl = inviteLinkForCredential(CREDENTIAL, ORIGIN, 'Design Review');
  installBrowserGlobals(sharedUrl, 'Riley');
  installFetchMock();
  const displayNameInput = makeInput('Riley');
  const meetingCodeInput = makeInput();
  const joinHint = makeHint();
  const { ctx, state } = makeConnectionContext('Riley', 'Design Review');
  const { page } = installMeetingPage(sharedUrl, ctx);
  const { createRoom } = makeFakeRoomFactory();
  const connection = setupConnection(ctx, createRoom);
  const joins: Promise<void>[] = [];

  autoJoinFromUrl({
    displayNameInput,
    meetingCodeInput,
    joinHint,
    logEvent: () => {},
    connectToMeeting: (code, identity) => {
      const join = connection.connectToMeeting(code, identity);
      joins.push(join);
      return join;
    },
    resolveIdentity: () => 'web-riley',
    showError: () => {},
    updateUnifiedCtaLabel: () => {},
  });
  await Promise.all(joins);

  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
  assert.equal(meetingCodeInput.value, ACCESS_CODE);
  assert.deepEqual(page.history.urls(), [sharedUrl, sharedUrl]);
  assert.equal(page.location.href, sharedUrl);
});

test('invite URL takes precedence over a legacy persisted credential without displaying it', () => {
  installBrowserGlobals(`https://meet.petal.live/?code=${ACCESS_CODE}`);
  localStorage.setItem(HARNESS_ROOM_STORAGE_KEY, CREDENTIAL);
  const displayNameInput = makeInput();
  const meetingCodeInput = makeInput(localStorage.getItem(HARNESS_ROOM_STORAGE_KEY) ?? '');
  const joinHint = makeHint();

  autoJoinFromUrl({
    displayNameInput,
    meetingCodeInput,
    joinHint,
    logEvent: () => {},
    connectToMeeting: async () => {},
    resolveIdentity: () => 'web-riley',
    showError: () => {},
    updateUnifiedCtaLabel: () => {},
  });

  assert.equal(meetingCodeInput.value, ACCESS_CODE);
  assert.doesNotMatch(meetingCodeInput.value, /^room-[0-9a-f]{32}$/);
});
