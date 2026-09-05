import { test } from 'node:test';
import assert from 'node:assert/strict';
import { RoomEvent, type Room } from 'livekit-client';

import { autoJoinFromUrl } from '../src/deepLink.ts';
import { HARNESS_NAME_STORAGE_KEY, HARNESS_ROOM_STORAGE_KEY } from '../src/constants.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import { setupConnection } from '../src/connection.ts';
import { inviteLinkForCredential } from '../src/controls.ts';
import type { HarnessContext } from '../src/context.ts';

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

  emit(event: string) {
    for (const handler of this.handlers.get(event) ?? []) {
      handler();
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

test('successful connection replaces the address bar with the shareable invite URL', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const historyCalls = installHistoryMock();
  const { ctx, state, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const { createRoom } = makeFakeRoomFactory();
  const expectedUrl = inviteLinkForCredential(CREDENTIAL, ORIGIN, 'Design Review');

  await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');

  if (state.streamStatePollTimer !== null) clearInterval(state.streamStatePollTimer);
  assert.deepEqual(historyCalls, [[null, '', expectedUrl]]);
  assert.deepEqual(uiCalls.meetingScreens, [CREDENTIAL]);
});

test('disconnect resets the address bar to the bare origin', async () => {
  installBrowserGlobals(`${ORIGIN}/`, 'Riley');
  installFetchMock();
  const historyCalls = installHistoryMock();
  const { ctx, state, uiCalls } = makeConnectionContext('Riley', 'Design Review');
  const { createRoom, rooms } = makeFakeRoomFactory();
  const expectedUrl = inviteLinkForCredential(CREDENTIAL, ORIGIN, 'Design Review');

  await setupConnection(ctx, createRoom).connectToMeeting(CREDENTIAL, 'web-riley');

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
  const historyCalls = installHistoryMock();
  const displayNameInput = makeInput('Riley');
  const meetingCodeInput = makeInput();
  const joinHint = makeHint();
  const { ctx, state } = makeConnectionContext('Riley', 'Design Review');
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
  assert.deepEqual(historyCalls, [[null, '', sharedUrl]]);
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
