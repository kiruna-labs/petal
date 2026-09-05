import './style.css';
import { trackNameForWindow } from './trackNames';
import { createLogger } from './ui/logging';
import { createSessionLogFilename, sessionLogCollector } from './ui/sessionLogCollector';
import { setupUiHelpers } from './ui/uiHelpers';
import { setupHomeScreen } from './homeScreen';
import { verifyH264Negotiated as verifyH264NegotiatedForHook } from './codec';
import { createTestPattern } from './testPattern';
import { createTelepointerSender } from './telepointerSender';
import { autoJoinFromUrl } from './deepLink';
import type { HarnessContext, HarnessHook } from './context';
import { setupTileLayout } from './tileLayout';
import { setupTelepointerDisplay } from './telepointerDisplay';
import { setupDrawDisplay } from './drawDisplay';
import { setupDrawSender } from './drawSender';
import { setupAiChat } from './aiChatSession';
import { setupRemoteControlUi } from './remoteControlUi';
import { setupViewerDemand } from './viewerDemand';
import { setupPipelineStats } from './pipelineStats';
import { setupNetworkDiagnostics } from './networkDiagnostics';
import { setupHarnessApi } from './harnessApi';
import { setupCockpit } from './cockpit';
import { setupTiles } from './tiles';
import { setupConnection } from './connection';
import { installEncodedAudioWorkaroundFromUrl } from './encodedAudioProbe';
import { setupControls, shouldShowFirstVisitOnboarding } from './controls';
import { addSentryBreadcrumb, initSentry, installGlobalErrorMirror } from './sentryReporting';
import { initAnalytics } from './analytics';
import { FeedbackReportController } from './feedbackReport';
import { sensitiveStringRegistry } from './sensitiveStrings';
import {
  HARNESS_COLOR_STORAGE_KEY,
  HARNESS_DEBUG_MODE_STORAGE_KEY,
  HARNESS_LOCAL_ECHO_STORAGE_KEY,
  HARNESS_NAME_STORAGE_KEY,
  HARNESS_ROOM_STORAGE_KEY,
  HARNESS_TILE_LAYOUT_STORAGE_KEY,
} from './constants';

declare const __PETAL_BUILD_INFO__: {
  version: string;
  commit: string;
  buildDate: string;
};

// ---------------------------------------------------------------------------
// Thin bootstrap. All session logic lives in the extracted modules; main.ts
// only gathers DOM refs, owns the mutable `HarnessState`, wires the UI helpers
// and cross-module callbacks into a shared `HarnessContext`, and starts things.
//
// The test pattern's window_id is randomized per page load (a random 5-6 digit
// number) instead of a fixed constant, so that when multiple browser
// tabs/participants each share the test pattern in the same meeting, their
// track names never collide. Track-name formats themselves live in
// trackNames.ts (unit-tested contracts shared with the native app).
// ---------------------------------------------------------------------------
const WINDOW_ID = 100000 + Math.floor(Math.random() * 900000); // 100000-999999

// ---------------------------------------------------------------------------
// DOM refs
// ---------------------------------------------------------------------------
const joinScreen = document.querySelector<HTMLDivElement>('#join-screen')!;
const meetingScreen = document.querySelector<HTMLDivElement>('#meeting-screen')!;
const joinCard = joinScreen.querySelector<HTMLDivElement>('.join-card')!;
const connectingScreen = document.querySelector<HTMLDivElement>('#connecting-screen');
const connectingTitle = document.querySelector<HTMLElement>('#connecting-title');
const connectingStatus = document.querySelector<HTMLElement>('#connecting-status');

const displayNameInput = document.querySelector<HTMLInputElement>('#display-name')!;
const profileAvatarInitial = document.querySelector<HTMLElement>('#profile-avatar-initial');
const profileAvatarButton = document.querySelector<HTMLButtonElement>('#profile-avatar-button');
const profileColorBubble = document.querySelector<HTMLButtonElement>('#profile-color-bubble');
const profileColorSwatches = Array.from(document.querySelectorAll<HTMLButtonElement>('.color-swatch'));
const profileOnboarding = document.querySelector<HTMLDivElement>('#profile-onboarding');
const profileOnboardingDone = document.querySelector<HTMLButtonElement>('#profile-onboarding-done');
const meetingCodeInput = document.querySelector<HTMLInputElement>('#meeting-code')!;
const joinBtn = document.querySelector<HTMLButtonElement>('#join-btn')!;
const createBtn = document.querySelector<HTMLButtonElement>('#create-btn');
const connError = document.querySelector<HTMLElement>('#conn-error')!;
const joinHint = document.querySelector<HTMLElement>('#join-hint')!;
const buildVersion = document.querySelector<HTMLElement>('#build-version-text');
const desktopDownload = document.querySelector<HTMLAnchorElement>('#desktop-download');

const roomNameEl = document.querySelector<HTMLElement>('#room-name')!;
const roomCopyButton = document.querySelector<HTMLButtonElement>('#room-copy')!;
const roomRenameButton = document.querySelector<HTMLButtonElement>('#room-rename')!;
const elapsedEl = document.querySelector<HTMLElement>('#elapsed')!;
const connState = document.querySelector<HTMLElement>('#conn-state')!;
const participantCountEl = document.querySelector<HTMLElement>('#participant-count')!;
const tilesEl = document.querySelector<HTMLDivElement>('#tiles')!;
const networkDiagnosticsRows = document.querySelector<HTMLDivElement>('#network-diagnostics-rows')!;
const topbarRight = document.querySelector<HTMLDivElement>('.topbar-right')!;

const ctlAudio = document.querySelector<HTMLButtonElement>('#ctl-audio')!;
const ctlVideo = document.querySelector<HTMLButtonElement>('#ctl-video')!;
const audioCaret = document.querySelector<HTMLButtonElement>('#ctl-audio-options')!;
const videoCaret = document.querySelector<HTMLButtonElement>('#ctl-video-options')!;
const devicesMenu = document.querySelector<HTMLElement>('#devices-menu')!;
const devicesMenuTitle = document.querySelector<HTMLElement>('#devices-menu-title')!;
const devicesMenuBody = document.querySelector<HTMLElement>('#devices-menu-body')!;
const ctlShare = document.querySelector<HTMLButtonElement>('#ctl-share')!;
const ctlShareLabel = document.querySelector<HTMLElement>('#ctl-share-label')!;
const ctlDraw = document.querySelector<HTMLButtonElement>('#ctl-draw')!;
const ctlDrawLabel = document.querySelector<HTMLElement>('#ctl-draw-label')!;
const ctlInvite = document.querySelector<HTMLButtonElement>('#ctl-invite')!;
const ctlInviteTooltip = document.querySelector<HTMLElement>('#ctl-invite-tooltip')!;
const ctlLeave = document.querySelector<HTMLButtonElement>('#ctl-leave')!;

const INVITE_TOOLTIP_GUTTER_PX = 12;
let inviteTooltipShift = 0;

function keepInviteTooltipInViewport() {
  requestAnimationFrame(() => {
    const rect = ctlInviteTooltip.getBoundingClientRect();
    // A resize can run after a previous correction. Calculate from the
    // unshifted box so repeated events converge instead of losing the shift.
    const unshiftedLeft = rect.left - inviteTooltipShift;
    const unshiftedRight = rect.right - inviteTooltipShift;
    inviteTooltipShift = unshiftedLeft < INVITE_TOOLTIP_GUTTER_PX
      ? INVITE_TOOLTIP_GUTTER_PX - unshiftedLeft
      : unshiftedRight > window.innerWidth - INVITE_TOOLTIP_GUTTER_PX
        ? window.innerWidth - INVITE_TOOLTIP_GUTTER_PX - unshiftedRight
        : 0;
    ctlInviteTooltip.style.setProperty('--invite-tooltip-shift', `${inviteTooltipShift}px`);
  });
}

const ctlInviteCell = ctlInvite.closest<HTMLElement>('.control-cell');
ctlInviteCell?.addEventListener('mouseenter', keepInviteTooltipInViewport);
ctlInviteCell?.addEventListener('focusin', keepInviteTooltipInViewport);
window.addEventListener('resize', keepInviteTooltipInViewport);

const shareBtn = document.querySelector<HTMLButtonElement>('#share-btn')!;
const shareState = document.querySelector<HTMLElement>('#share-state')!;
const canvas = document.querySelector<HTMLCanvasElement>('#test-canvas')!;
const trackNameDisplay = document.querySelector<HTMLElement>('#track-name-display')!;

const shareScreenState = document.querySelector<HTMLElement>('#share-screen-state')!;
const micRealState = document.querySelector<HTMLElement>('#mic-real-state')!;

const micCheckbox = document.querySelector<HTMLInputElement>('#mic-checkbox')!;
const micState = document.querySelector<HTMLElement>('#mic-state')!;
const localEchoCheckbox = document.querySelector<HTMLInputElement>('#local-echo-checkbox')!;
const debugModeCheckbox = document.querySelector<HTMLInputElement>('#debug-mode-checkbox')!;

const webcamState = document.querySelector<HTMLElement>('#webcam-state')!;
const cameraTrackNameDisplay = document.querySelector<HTMLElement>('#camera-track-name-display')!;

const sessionLog = document.querySelector<HTMLDivElement>('#session-log')!;
const downloadSessionLogBtn = document.querySelector<HTMLButtonElement>('#download-session-log')!;
const toastEl = document.querySelector<HTMLDivElement>('#toast')!;
const feedbackHomeTrigger = document.querySelector<HTMLButtonElement>('#feedback-home-trigger')!;
const feedbackMeetingTrigger = document.querySelector<HTMLButtonElement>('#feedback-meeting-trigger')!;
const feedbackDialog = document.querySelector<HTMLDialogElement>('#feedback-dialog')!;
const feedbackForm = document.querySelector<HTMLFormElement>('#feedback-form')!;
const feedbackMessage = document.querySelector<HTMLTextAreaElement>('#feedback-message')!;
const feedbackConsent = document.querySelector<HTMLInputElement>('#feedback-consent')!;
const feedbackSubmit = document.querySelector<HTMLButtonElement>('#feedback-submit')!;
const feedbackCancel = document.querySelector<HTMLButtonElement>('#feedback-cancel')!;
const feedbackStatus = document.querySelector<HTMLElement>('#feedback-status')!;
const feedbackShareReason = document.querySelector<HTMLElement>('#feedback-share-reason')!;

trackNameDisplay.textContent = trackNameForWindow(WINDOW_ID);

let getSessionLogContext = () => ({
  identity: undefined as string | undefined,
  room: meetingCodeInput.value.trim() || undefined,
});

const baseLogEvent = createLogger(sessionLog, () => getSessionLogContext());
// Every local log line also becomes a (PII-scrubbed) Sentry breadcrumb, so a
// captured error's Sentry report carries the same recent-activity trail this
// session log shows locally -- see sentryReporting.ts. `addSentryBreadcrumb`
// is a no-op until `initSentry` runs below, and stays a no-op forever if
// VITE_SENTRY_DSN was never set.
const logEvent: typeof baseLogEvent = (message, kind = 'info') => {
  baseLogEvent(message, kind);
  addSentryBreadcrumb(message, kind);
};

// Restore last-used display name so re-testing doesn't require re-typing every
// reload (dev-tool convenience only, never used for anything beyond the
// current browser).
displayNameInput.value = localStorage.getItem(HARNESS_NAME_STORAGE_KEY) ?? '';
const storedProfileColor = localStorage.getItem(HARNESS_COLOR_STORAGE_KEY);
meetingCodeInput.value = localStorage.getItem(HARNESS_ROOM_STORAGE_KEY) ?? '';
if (desktopDownload) {
  const platform = /Windows/i.test(`${navigator.userAgent} ${navigator.platform}`) ? 'windows' : 'macos';
  desktopDownload.href = `https://app.petal.live/api/download?platform=${platform}`;
  desktopDownload.textContent = platform === 'windows'
    ? 'Download Petal for Windows'
    : 'Download Petal for macOS';
}
if (buildVersion) {
  const buildInfo = __PETAL_BUILD_INFO__;
  buildVersion.textContent = `v${buildInfo.version} · ${buildInfo.commit} · ${buildInfo.buildDate}`;
}

// ---------------------------------------------------------------------------
// Test/automation hook. `window.__petalHarness` is referenced by the repo's
// browser-driven verification (codec/stats checks) -- keep the `room` and
// `localVideoTrack` keys stable. `codecChecks` records the result of every
// negotiated-codec verification (see codec.ts's verifyH264Negotiated).
// ---------------------------------------------------------------------------
const harnessHook: HarnessHook = {
  room: null,
  localVideoTrack: null,
  screenTrack: null,
  micTrack: null,
  codecChecks: [],
  remoteControl: null,
  latencyProbe: null,
  pipelineStats: null,
  cockpitAutoScenario: null,
  testPatternMeasurement: null,
  encodedAudioProbe: installEncodedAudioWorkaroundFromUrl(),
};
(window as typeof window & { __petalHarness?: unknown }).__petalHarness = harnessHook;

// ---------------------------------------------------------------------------
// Sentry error reporting (#283). `initSentry` is a no-op unless
// VITE_SENTRY_DSN is set at build time -- local dev stays fully off. The
// uncaught-error mirror into the local session log is installed
// unconditionally, so local visibility never depends on Sentry being
// configured. See sentryReporting.ts for the PII-scrub wiring.
//
// PostHog product events use the same bake gate (`VITE_PETAL_POSTHOG_KEY`).
// Host-side capture only -- not posthog-js. See analytics.ts.
// ---------------------------------------------------------------------------
initSentry(logEvent);
initAnalytics();
installGlobalErrorMirror(logEvent);

// ---------------------------------------------------------------------------
// Shared context: mutable state + DOM refs + (soon) UI helpers and callbacks.
// ---------------------------------------------------------------------------
const ctx: HarnessContext = {
  windowId: WINDOW_ID,
  dom: {
    joinScreen,
    meetingScreen,
    joinCard,
    displayNameInput,
    meetingCodeInput,
    joinBtn,
    createBtn,
    connError,
    joinHint,
    roomNameEl,
    roomCopyButton,
    roomRenameButton,
    elapsedEl,
    connState,
    participantCountEl,
    tilesEl,
    networkDiagnosticsRows,
    topbarRight,
    ctlAudio,
    ctlVideo,
    audioCaret,
    videoCaret,
    devicesMenu,
    devicesMenuTitle,
    devicesMenuBody,
    ctlShare,
    ctlShareLabel,
    ctlDraw,
    ctlDrawLabel,
    ctlInvite,
    ctlInviteTooltip,
    ctlLeave,
    shareBtn,
    shareState,
    canvas,
    trackNameDisplay,
    shareScreenState,
    micRealState,
    micCheckbox,
    micState,
    localEchoCheckbox,
    debugModeCheckbox,
    webcamState,
    cameraTrackNameDisplay,
    sessionLog,
    toastEl,
  },
  state: {
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
    tileLayoutMode:
      localStorage.getItem(HARNESS_TILE_LAYOUT_STORAGE_KEY) === 'spotlight' ? 'spotlight' : 'grid',
    pinnedTileId: null,
    layoutModeButtons: null,
    speakerSmoothingTimer: null,
    activeRemoteControl: null,
    remoteControlSeq: 0,
    viewerDemandSeq: 0,
    audioCtx: null,
    oscillator: null,
    syntheticCameraIntervalId: null,
    localEchoEnabled: localStorage.getItem(HARNESS_LOCAL_ECHO_STORAGE_KEY) === '1',
    debugModeEnabled: localStorage.getItem(HARNESS_DEBUG_MODE_STORAGE_KEY) === '1',
  },
  // ui and cb are filled in below, before any handler can fire.
  ui: null as unknown as HarnessContext['ui'],
  hook: harnessHook,
  cb: null as unknown as HarnessContext['cb'],
  activeSpeakerTargets: new Set<string>(),
  speakerScores: new Map<string, number>(),
  remoteTelepointers: new Map(),
  handshakeCooldowns: new Map<string, number>(),
};

// Refs #378: reflect the persisted opt-in choice into the checkbox itself --
// the `checked` attribute isn't reactive, so this has to be set explicitly
// (mirrors how tileLayoutMode above is read from storage into state, just
// applied to a DOM control instead of a state field the button classes
// derive from).
localEchoCheckbox.checked = ctx.state.localEchoEnabled;
// #669: same reflect-persisted-state-into-the-checkbox step as local echo
// above.
debugModeCheckbox.checked = ctx.state.debugModeEnabled;

getSessionLogContext = () => ({
  identity: ctx.state.room?.localParticipant.identity,
  room: ctx.state.currentMeetingCode ?? undefined,
});

downloadSessionLogBtn.addEventListener('click', () => {
  const context = getSessionLogContext();
  const url = URL.createObjectURL(sessionLogCollector.exportBlob());
  const link = document.createElement('a');
  link.href = url;
  link.download = createSessionLogFilename(context);
  document.body.appendChild(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
  logEvent(`downloaded session log as ${link.download}`, 'ok');
});

// #293 intentionally constructs a fixed diagnostics Blob rather than uploading
// the raw local session-log export above. The SDK client itself is instantiated
// only inside an explicit dialog submission.
const feedbackReport = new FeedbackReportController({
  publicKey: (import.meta as ImportMeta & { env?: { VITE_USERDISPATCH_PUBLIC_KEY?: string } }).env?.VITE_USERDISPATCH_PUBLIC_KEY,
  dom: {
    homeTrigger: feedbackHomeTrigger,
    meetingTrigger: feedbackMeetingTrigger,
    dialog: feedbackDialog,
    form: feedbackForm,
    message: feedbackMessage,
    consent: feedbackConsent,
    submit: feedbackSubmit,
    cancel: feedbackCancel,
    status: feedbackStatus,
    shareReason: feedbackShareReason,
  },
  getState: () => ({
    connected: ctx.state.room !== null,
    sharing: ctx.state.sharing,
    screenSharing: ctx.state.screenSharing,
  }),
  registry: sensitiveStringRegistry,
});
feedbackReport.install();

// ---------------------------------------------------------------------------
// UI helpers (`updateUnifiedCtaLabel` is populated by setupHomeScreen below;
// forward through ctx.cb so the closure always sees the wired value).
// ---------------------------------------------------------------------------
ctx.ui = {
  logEvent,
  ...setupUiHelpers({
    joinScreen,
    meetingScreen,
    connectingScreen,
    connectingTitle,
    connectingStatus,
    displayNameInput,
    meetingCodeInput,
    joinBtn,
    createBtn,
    connError,
    roomNameEl,
    elapsedEl,
    connState,
    shareState,
    shareScreenState,
    micRealState,
    micState,
    webcamState,
    toastEl,
    ctlAudio,
    ctlVideo,
    ctlShare,
    ctlShareLabel,
    roomCopyButton,
    ctlInvite,
    ctlInviteTooltip,
    updateUnifiedCtaLabel: () => ctx.cb.updateUnifiedCtaLabel(),
    logEvent,
  }),
};

// ---------------------------------------------------------------------------
// Shared helpers that mirror harness state into the automation hook + verify
// negotiated codecs.
// ---------------------------------------------------------------------------
function syncHarnessHook() {
  harnessHook.room = ctx.state.room;
  harnessHook.localVideoTrack = ctx.state.localVideoTrack;
  harnessHook.screenTrack = ctx.state.screenTrack;
  harnessHook.micTrack = ctx.state.micTrack;
}

function verifyH264Negotiated(track: import('livekit-client').LocalVideoTrack, label: string) {
  return verifyH264NegotiatedForHook(track, label, harnessHook, logEvent);
}

const { startCanvasAnimation, getFrameCount: getPatternFrameCount } = createTestPattern(canvas);
startCanvasAnimation();

const {
  startTelepointerSender,
  stopTelepointerSender,
  bindHoverTelepointer,
  publishCockpitTelepointer,
} = createTelepointerSender({
  windowId: WINDOW_ID,
  getRoom: () => ctx.state.room,
});

// ---------------------------------------------------------------------------
// Module setup + callback wiring. Each module reads/writes ctx; the callbacks
// break the tiles <-> telepointerDisplay <-> remoteControlUi cycle. Callbacks
// are assigned before any DOM/room event can fire.
// ---------------------------------------------------------------------------
ctx.cb = {
  syncHarnessHook,
  updateUnifiedCtaLabel: () => {},
  recordRecentRoom: () => {},
  refreshRecentRooms: () => {},
  roomDisplayLabelForCredential: (code: string) => code,
  renameRoomDisplayName: (code: string) => code,
  verifyH264Negotiated,
  startTelepointerSender,
  stopTelepointerSender,
  bindHoverTelepointer,
  publishCockpitTelepointer,
  startCanvasAnimation,
} as unknown as HarnessContext['cb'];

const tileLayout = setupTileLayout(ctx);
Object.assign(ctx.cb, {
  applyTileLayout: tileLayout.applyTileLayout,
  applySpeakingRings: tileLayout.applySpeakingRings,
  startSpeakerSmoothing: tileLayout.startSpeakerSmoothing,
  smoothSpeakingScores: tileLayout.smoothSpeakingScores,
  resetActiveSpeakers: tileLayout.resetActiveSpeakers,
  shareTileCount: tileLayout.shareTileCount,
  pinTile: tileLayout.pinTile,
  bindTileInteractions: tileLayout.bindTileInteractions,
});

const telepointerDisplay = setupTelepointerDisplay(ctx);
const drawDisplay = setupDrawDisplay(ctx);
Object.assign(ctx.cb, {
  shareTileForWindowId: telepointerDisplay.shareTileForWindowId,
  renderTelepointersForWindow: telepointerDisplay.renderTelepointersForWindow,
  removeTelepointersForParticipant: telepointerDisplay.removeTelepointersForParticipant,
  removeTelepointersForWindow: telepointerDisplay.removeTelepointersForWindow,
  handleRemoteTelepointerPayload: telepointerDisplay.handleRemoteTelepointerPayload,
  handleRemoteDrawPayload: drawDisplay.handleRemoteDrawPayload,
  handleRemoteControlPayload: () => {},
  handleLatencyProbePayload: () => {},
  handlePipelineStatsPayload: () => {},
  startLatencyProbe: () => {},
  stopLatencyProbe: () => {},
  resetRemoteControlHarnessSession: () => {},
  startPipelineStats: () => {},
  stopPipelineStats: () => {},
  repositionRemoteTelepointers: telepointerDisplay.repositionRemoteTelepointers,
  repositionRemoteDraw: drawDisplay.repositionRemoteDraw,
  clearRemoteTelepointers: telepointerDisplay.clearRemoteTelepointers,
  clearRemoteDraw: drawDisplay.clearRemoteDraw,
  renderDrawForWindow: drawDisplay.renderDrawForWindow,
  removeDrawForWindow: drawDisplay.removeDrawForWindow,
  removeDrawForParticipant: drawDisplay.removeDrawForParticipant,
});

const drawSender = setupDrawSender(ctx);
Object.assign(ctx.cb, {
  setDrawMode: drawSender.setDrawMode,
  syncDrawAvailability: drawSender.syncDrawAvailability,
  publishCockpitDrawStroke: drawSender.publishCockpitDrawStroke,
});

// #657 petal.ai-chat. This client drives and observes; the Gemini session
// itself always runs on the sharer's machine.
const aiChat = setupAiChat(ctx);
Object.assign(ctx.cb, {
  handleAiChatPayload: aiChat.handlePayload,
  aiChatSessionFor: aiChat.sessionFor,
  startAiChat: aiChat.requestStart,
  stopAiChat: aiChat.requestStop,
  aiChatPttStart: aiChat.pttStart,
  aiChatPttEnd: aiChat.pttEnd,
  aiChatSendText: aiChat.sendText,
  aiChatLocalPttHeld: aiChat.localPttHeld,
  aiChatReleaseAllPtt: aiChat.releaseAllPtt,
  aiChatOwnerLeft: aiChat.ownerLeft,
  resetAiChat: aiChat.reset,
  onAiChatChange: aiChat.onChange,
});

const remoteControlUi = setupRemoteControlUi(ctx);
Object.assign(ctx.cb, {
  stopRemoteControl: remoteControlUi.stopRemoteControl,
  startRemoteControl: remoteControlUi.startRemoteControl,
  activeRemoteControlForTile: remoteControlUi.activeRemoteControlForTile,
  ensureRemoteControlAffordance: remoteControlUi.ensureRemoteControlAffordance,
});

const harnessApi = setupHarnessApi(ctx, {
  nextRemoteControlSeq: remoteControlUi.nextRemoteControlSeq,
  publishRemoteControl: remoteControlUi.publishRemoteControl,
  startRemoteControl: remoteControlUi.startRemoteControl,
  stopRemoteControl: remoteControlUi.stopRemoteControl,
});
Object.assign(ctx.cb, {
  handleRemoteControlPayload: (payload: Uint8Array, senderIdentity?: string) => {
    remoteControlUi.handleRemoteControlPayload(payload, senderIdentity);
    harnessApi.handleRemoteControlPayload(payload, senderIdentity);
  },
  handleLatencyProbePayload: harnessApi.handleLatencyProbePayload,
  startLatencyProbe: harnessApi.startLatencyProbe,
  stopLatencyProbe: harnessApi.stopLatencyProbe,
  resetRemoteControlHarnessSession: harnessApi.resetRemoteControlSession,
  enableRemoteControlHostEmulation: harnessApi.enableRemoteControlHostEmulation,
  remoteControlHostLedger: harnessApi.remoteControlHostLedger,
});

const viewerDemand = setupViewerDemand(ctx);
Object.assign(ctx.cb, {
  publishViewerDemand: viewerDemand.publishViewerDemand,
  publishViewerDemandForPublication: viewerDemand.publishViewerDemandForPublication,
  startViewerDemandHeartbeat: viewerDemand.startViewerDemandHeartbeat,
  stopViewerDemandHeartbeat: viewerDemand.stopViewerDemandHeartbeat,
});

const pipelineStats = setupPipelineStats(ctx);
Object.assign(ctx.cb, {
  handlePipelineStatsPayload: pipelineStats.handlePipelineStatsPayload,
  startPipelineStats: pipelineStats.startPipelineStats,
  stopPipelineStats: pipelineStats.stopPipelineStats,
});
setupNetworkDiagnostics(ctx);

const tiles = setupTiles(ctx);
Object.assign(ctx.cb, {
  ensureBaseTile: tiles.ensureBaseTile,
  setTileCamera: tiles.setTileCamera,
  clearTileCamera: tiles.clearTileCamera,
  addShareTile: tiles.addShareTile,
  removeShareTile: tiles.removeShareTile,
  holdShareFrame: tiles.holdShareFrame,
  removeParticipantTiles: tiles.removeParticipantTiles,
  clearTiles: tiles.clearTiles,
  setParticipantAudioActive: tiles.setParticipantAudioActive,
  updateParticipantCount: tiles.updateParticipantCount,
  updateParticipantShareColorProfiles: tiles.updateParticipantShareColorProfiles,
  refreshParticipantGrid: tiles.refreshParticipantGrid,
  trackedShareWindows: tiles.trackedShareWindows,
  isCameraTrack: tiles.isCameraTrack,
  publicationPaused: tiles.publicationPaused,
  setPublicationPaused: tiles.setPublicationPaused,
  syncStreamStates: tiles.syncStreamStates,
  ensureFrameMetadataWorker: tiles.ensureFrameMetadataWorker,
  fitTileLabels: tiles.fitTileLabels,
  syncRemoteWindowHeaders: tiles.syncRemoteWindowHeaders,
});

const connection = setupConnection(ctx, undefined, sensitiveStringRegistry, feedbackReport);
Object.assign(ctx.cb, {
  connectToMeeting: connection.connectToMeeting,
});

const controls = setupControls(ctx, feedbackReport);
Object.assign(ctx.cb, {
  resolveIdentity: controls.resolveIdentity,
  submitMeetingField: controls.submitMeetingField,
  renameRoomDisplayName: controls.renameRoomDisplayName,
  startTestPatternShare: controls.startTestPatternShare,
  startCockpitWebcam: controls.startCockpitWebcam,
  stopCockpitWebcam: controls.stopCockpitWebcam,
  startCockpitAudioTone: controls.startCockpitAudioTone,
  measureCockpitRemoteAudio: controls.measureCockpitRemoteAudio,
  measureCockpitRemoteCamera: controls.measureCockpitRemoteCamera,
});

// Test-cockpit walking-skeleton automation hook (#254). Depends on
// ctx.cb.connectToMeeting/resolveIdentity/startTestPatternShare, all wired
// above, so this must come after `setupConnection`/`setupControls`.
const cockpit = setupCockpit(ctx, getPatternFrameCount);

// The layout picker installs itself into the topbar; do this after the layout
// callbacks are wired (installLayoutPicker -> applyTileLayout).
tileLayout.installLayoutPicker();

// Home screen wires the unified Create/Join CTA; capture its callbacks.
const {
  updateUnifiedCtaLabel,
  recordRecentRoom,
  refreshRecentRooms,
  roomDisplayLabelForCredential,
} = setupHomeScreen({
  joinCard,
  displayNameInput,
  profileAvatarInitial,
  profileAvatarButton,
  profileColorBubble,
  profileColorSwatches,
  profileOnboarding,
  profileOnboardingDone,
  showFirstVisitOnboarding: shouldShowFirstVisitOnboarding(localStorage) && !storedProfileColor,
  meetingCodeInput,
  joinBtn,
  createBtn,
  connError,
  joinHint,
  submitMeetingField: () => ctx.cb.submitMeetingField(),
  showToast: ctx.ui.showToast,
  logEvent,
});
ctx.cb.updateUnifiedCtaLabel = updateUnifiedCtaLabel;
ctx.cb.recordRecentRoom = recordRecentRoom;
ctx.cb.refreshRecentRooms = refreshRecentRooms;
ctx.cb.roomDisplayLabelForCredential = roomDisplayLabelForCredential;

controls.installControls();

window.addEventListener('resize', () => {
  ctx.cb.repositionRemoteTelepointers();
  ctx.cb.repositionRemoteDraw();
});

autoJoinFromUrl({
  displayNameInput,
  meetingCodeInput,
  joinHint,
  logEvent,
  connectToMeeting: (code, identity) => ctx.cb.connectToMeeting(code, identity),
  resolveIdentity: () => ctx.cb.resolveIdentity(),
  showError: ctx.ui.showError,
  updateUnifiedCtaLabel: () => ctx.cb.updateUnifiedCtaLabel(),
  showConnectingScreen: ctx.ui.showConnectingScreen,
});

// Test-cockpit unattended driver (#254): `?auto=<scenarioId>` runs its own
// join (independent of `autoJoinFromUrl`'s name-gated interactive path)
// unattended, so it must be checked after the interactive auto-join above.
cockpit.maybeRunAutoScenario();
