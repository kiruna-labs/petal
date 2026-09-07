import type {
  Room,
  LocalVideoTrack,
  LocalAudioTrack,
  RemoteTrackPublication,
} from 'livekit-client';
import type { LogKind } from './ui/logging';
// Type-only: erased at compile time, so this does NOT pull toastMount.ts
// (and therefore Toast.svelte) into anything that only imports context.ts's
// types -- connection.ts deliberately calls this through `ui.showActionableToast`
// rather than importing toastMount.ts directly, to keep its own module load
// free of a `.svelte` import (a bare `node --test` run cannot load `.svelte`
// files without a loader, which real UI-owning modules already register for).
import type { SharedToastAction } from './toastMount.ts';
import type { HoldReason } from './holdLastFrame.ts';
import type {
  LatencyProbeMessage,
  PipelineStatsMessage,
  RemoteControlMessage,
  RemoteControlCapability,
  RemoteControlModifiers,
  TelepointerMessage
} from './trackNames';
import type {
  CapturePath,
  RequestedSubscription,
  StartupTimelineSnapshot,
} from './startupTimeline';

// ---------------------------------------------------------------------------
// Shared harness context. main.ts is a thin bootstrap that owns the mutable
// session state; every extracted module reads and writes it through this
// object so the dense tiles <-> telepointerDisplay <-> remoteControlUi cycle is
// broken by injection rather than direct cross-imports. Fields that were
// module-level `let`s in the original single-file main.ts live in
// `HarnessState`; the DOM refs, UI helpers, and cross-module callbacks are
// wired once at bootstrap and passed in unchanged.
// ---------------------------------------------------------------------------

export type TileLayoutMode = 'grid' | 'spotlight';

export interface CodecCheck {
  label: string;
  mimeType: string | null;
  ok: boolean;
}

export interface HarnessRemoteControlTarget {
  targetUserId: string;
  windowId: number;
  tileId?: string;
}

export interface HarnessRemoteControlPoint {
  x: number;
  y: number;
}

export interface HarnessRemoteControlInput extends Partial<HarnessRemoteControlTarget> {
  target?: Partial<HarnessRemoteControlTarget>;
}

export interface HarnessPhotonFrame {
  generation: number;
  confidence: number;
  calibrationMatches: number;
  width: number;
  height: number;
}

export interface HarnessPressToPhotonInput extends HarnessRemoteControlInput {
  kind: 'click' | 'text';
  timeoutMs?: number;
  x?: number;
  y?: number;
  text?: string;
}

export interface HarnessPressToPhotonResult {
  inputKind: 'click' | 'text';
  baselineGeneration: number;
  observedGeneration: number;
  baselineConfidence: number;
  observedConfidence: number;
  pressToFrameCallbackMs: number;
  pressToEstimatedPhotonMs: number;
  publishCompleteMs: number;
  mediaTime: number;
  presentedFrames: number;
}

export interface HarnessRemoteControlApi {
  targets: () => HarnessRemoteControlTarget[];
  active: () => (HarnessRemoteControlTarget & { grantToken: string | null }) | null;
  /**
   * #580: proof that this peer actually holds a grant token for the target.
   * Without one the host drops every input packet
   * (TOKENLESS_GRANT_COMPATIBILITY_ENABLED = false), and absence-asserting
   * cases pass vacuously. Drivers must gate on `granted` before trusting a run.
   */
  grant: (input?: HarnessRemoteControlInput) => {
    target: HarnessRemoteControlTarget;
    granted: boolean;
    grantToken: string | null;
    /** #820/case-30: non-null only when the host negotiated the v2 envelope
     * (a macOS host advertises legacy-only by contract -- CONTRACTS.md). */
    controlSessionId: string | null;
    tokenlessInputs: number;
  };
  metrics: () => {
    published: Array<{
      kind: string;
      /** #580: null when the packet went out carrying no grant token. */
      grantToken?: string | null;
      action?: string;
      seq: number;
      windowId: number;
      targetUserId: string;
      at: number;
      reliable: boolean;
      button?: number;
      buttons?: number;
      clickCount?: number;
      key?: string;
      code?: string;
      modifiers?: RemoteControlModifiers;
      x?: number;
      y?: number;
      deltaX?: number;
      deltaY?: number;
      deltaMode?: number;
    }>;
    statuses: Array<{
      status: Extract<RemoteControlMessage, { kind: 'status' }>['status'];
      message: string;
      seq: number;
      windowId: number;
      targetUserId: string;
      controllerId: string;
      senderIdentity?: string;
      receivedAt: number;
    }>;
    results: Array<{
      inputId: string;
      inputSeq: number;
      outcome: string;
      /** Additive v2 result metadata; absent for legacy peers/results. */
      deliveryRoute?: Extract<RemoteControlMessage, { kind: 'result' }>['deliveryRoute'];
      /** Optional privacy-safe terminal failure classification. */
      failureCode?: Extract<RemoteControlMessage, { kind: 'result' }>['failureCode'];
      windowId: number;
      receivedAt: number;
    }>;
    pending: string[];
    /** #580: input packets published with no grant token; the host drops these. */
    tokenlessInputs: Array<{
      kind: string;
      seq: number;
      windowId: number;
      targetUserId: string;
      at: number;
    }>;
  };
  resetMetrics: () => void;
  /**
   * #539 test-only proof hook. Republishes the last completed harness-created
   * v2 left-click once, preserving its operation identity while advancing only
   * the outer transport sequence. It never accepts or returns packet data.
   */
  replayLastCompletedClick: () => Promise<void>;
  request: (input?: HarnessRemoteControlInput) => HarnessRemoteControlTarget;
  release: (input?: HarnessRemoteControlInput) => HarnessRemoteControlTarget;
  pointer: (
    input: HarnessRemoteControlInput & {
      action?: 'move' | 'down' | 'up';
      x: number;
      y: number;
      button?: number;
      buttons?: number;
      /** #373: multi-click count (2 = double-click, 3 = triple-click, ...). */
      clickCount?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ) => HarnessRemoteControlTarget;
  click: (
    input: HarnessRemoteControlInput & {
      x: number;
      y: number;
      button?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ) => HarnessRemoteControlTarget;
  /** #373: synthesizes a down+up pair with clickCount=2 at the same point. */
  doubleClick: (
    input: HarnessRemoteControlInput & {
      x: number;
      y: number;
      button?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ) => Promise<HarnessRemoteControlTarget>;
  drag: (
    input: HarnessRemoteControlInput & {
      from: HarnessRemoteControlPoint;
      to: HarnessRemoteControlPoint;
      steps?: number;
      button?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ) => Promise<HarnessRemoteControlTarget>;
  wheel: (
    input: HarnessRemoteControlInput & {
      x: number;
      y: number;
      deltaX?: number;
      deltaY: number;
      deltaMode?: 0 | 1 | 2;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ) => HarnessRemoteControlTarget;
  key: (
    input: HarnessRemoteControlInput & {
      action?: 'down' | 'up' | 'press';
      key: string;
      code?: string;
      repeat?: boolean;
      location?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ) => HarnessRemoteControlTarget;
  text: (
    input: HarnessRemoteControlInput & {
      text: string;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ) => HarnessRemoteControlTarget;
  photonFrame: (input?: HarnessRemoteControlInput) => HarnessPhotonFrame | null;
  pressToPhoton: (input: HarnessPressToPhotonInput) => Promise<HarnessPressToPhotonResult>;
}

export interface HarnessLatencyProbeApi {
  latestRttMs: () => number | null;
  metrics: () => Array<{
    probeId: number;
    peerIdentity: string;
    rttMs: number;
    receivedAt: number;
  }>;
  resetMetrics: () => void;
  ping: () => LatencyProbeMessage | null;
}

export interface HarnessPipelineStatsApi {
  metrics: () => {
    sent: PipelineStatsMessage[];
    received: Array<{
      message: PipelineStatsMessage;
      senderIdentity?: string;
      receivedAt: number;
    }>;
  };
  resetMetrics: () => void;
  publish: () => Promise<PipelineStatsMessage[]>;
  stallStats: (windowId: number) => HarnessStallStats | null;
  resetStallStats: (windowId: number) => void;
  startupTimeline: () => StartupTimelineSnapshot[];
  trackPublished: (ownerIdentity: string, windowId: number, publicationSid: string) => void;
  /** Receiver-observed lifecycle facts, sent directly to the share owner. */
  trackSubscribed: (ownerIdentity: string, windowId: number, publicationSid: string) => void;
  trackFirstDecoded: (ownerIdentity: string, windowId: number, publicationSid: string) => void;
  trackFirstPresented: (ownerIdentity: string, windowId: number, publicationSid: string) => void;
  trackUnsubscribed: (ownerIdentity: string, windowId: number, publicationSid: string) => void;
  trackUnpublished: (ownerIdentity: string, windowId: number, publicationSid: string) => void;
  trackViewerDemand: (
    ownerIdentity: string,
    windowId: number,
    publicationSid: string,
    requestedSubscription: RequestedSubscription,
    demandWidth: number,
    demandHeight: number,
    requestedWidth: number,
    requestedHeight: number,
  ) => void;
  trackReceiverStats: (
    ownerIdentity: string,
    windowId: number,
    publicationSid: string,
    stats: {
      decodedWidth: number | null;
      decodedHeight: number | null;
      decodedFps: number | null;
      presentedFps: number | null;
      rid: string | 'unavailable';
      capturePath: CapturePath;
      captureFps: number | null;
      shareEpoch?: string | null;
    },
  ) => void;
  resetSession: () => void;
}

export interface HarnessStallStats {
  framesSeen: number;
  maxGapMs: number;
  gapsOverThreshold: number;
}

export interface HarnessMeasurementApi {
  captureCrop: (windowId: number, x: number, y: number, w: number, h: number) => ImageData | null;
  captureFramePng: (windowId: number) => {
    width: number;
    height: number;
    currentTime: number;
    dataUrl: string;
  } | null;
  stallStats: (windowId: number) => HarnessStallStats | null;
  resetStallStats: (windowId: number) => void;
}

/** One step outcome from a `runScenario` unattended cockpit run. */
export interface CockpitStepResult {
  step: string;
  ok: boolean;
  detail: string;
}

/** Final outcome of one unattended `?auto=<scenarioId>` cockpit run. */
export interface CockpitScenarioResult {
  scenarioId: string;
  ok: boolean;
  classification: 'PASS' | 'TEST-FAIL' | 'INFRA-FAIL';
  steps: CockpitStepResult[];
}

// ---------------------------------------------------------------------------
// Test-cockpit walking-skeleton automation hook (#254). `join`/`sharePattern`
// wrap the same `connectToMeeting`/`startTestPatternShare` paths the
// interactive UI calls, so a headless driver (apps/desktop/scripts/
// cockpit.mjs) exercises the real thing. `lastResult` lets an external CDP
// poller read the outcome of a `?auto=` unattended run without needing to
// consume the `petal.cockpit` LiveKit data topic itself.
// ---------------------------------------------------------------------------
export interface HarnessCockpitApi {
  join: (code: string) => Promise<void>;
  sharePattern: () => Promise<void>;
  runScenario: (scenarioId: string, code: string | null) => Promise<CockpitScenarioResult>;
  lastResult: CockpitScenarioResult | null;
}

export interface HarnessHook {
  room: Room | null;
  localVideoTrack: LocalVideoTrack | null;
  screenTrack: LocalVideoTrack | null;
  micTrack: LocalAudioTrack | null;
  codecChecks: CodecCheck[];
  remoteControl: HarnessRemoteControlApi | null;
  latencyProbe: HarnessLatencyProbeApi | null;
  pipelineStats: HarnessPipelineStatsApi | null;
  cockpitAutoScenario: HarnessCockpitApi | null;
  testPatternMeasurement: HarnessMeasurementApi | null;
  /** #510 production encoded-audio receiver workaround state. */
  encodedAudioProbe: import('./encodedAudioProbe.ts').EncodedAudioWorkaroundState;
  /** Plugin host bridge (plugins/README.md). Set by setupPlugins; connection.ts
   * calls roomConnected/roomDisconnected so plugins get meeting.* events. */
  plugins?: import('./plugins/setupPlugins.ts').PluginsHook | null;
}

export interface ActiveRemoteControl {
  tileId: string;
  targetUserId: string;
  windowId: number;
  pointerId: number | null;
  grantToken: string | null;
  targetKind?: 'window' | 'display';
  shareInstanceId?: string;
  hostCapabilities?: RemoteControlCapability[];
  /** Present only after the host has accepted the v2 grant. */
  controlSessionId?: string;
  resultCapability?: {
    version: 2;
    retryEnabled: boolean;
    retryDeadlineMs: number;
    dedupGuaranteeWindowMs: number;
  };
  nextInputSeq?: number;
  /** #370 corrective pass (Bug C): set from the most recent "active" status
   * packet's `supportsBinaryHotPath` flag. Only true once we've actually
   * observed the host advertise it -- `publishRemoteControl` gates the
   * binary encode on this, never assumes support. Defaults to false/absent
   * so a not-yet-upgraded host is always sent JSON. */
  supportsBinaryHotPath?: boolean;
}

export interface RemoteTelepointerState {
  message: TelepointerMessage;
  lastSeen: number;
  element: HTMLDivElement | null;
  staleTimer: ReturnType<typeof setTimeout> | null;
  removeTimer: ReturnType<typeof setTimeout> | null;
  activityTimer: ReturnType<typeof setTimeout> | null;
  pulseKey: number;
  lastClickAt: number;
}

// Mutable session state formerly held as module-level `let`s in main.ts.
export interface HarnessState {
  room: Room | null;
  frameMetadataWorker: Worker | null;
  streamStatePollTimer: ReturnType<typeof setInterval> | null;
  viewerDemandTimer: ReturnType<typeof setInterval> | null;
  pipelineStatsTimer: ReturnType<typeof setInterval> | null;
  // #298 receiver-side publication reconciliation pass.
  publicationReconcileTimer: ReturnType<typeof setInterval> | null;
  localVideoTrack: LocalVideoTrack | null; // synthetic test pattern
  localAudioTrack: LocalAudioTrack | null; // synthetic 440Hz tone
  localCameraTrack: LocalVideoTrack | null;
  screenTrack: LocalVideoTrack | null;
  screenWindowId: number | null;
  micTrack: LocalAudioTrack | null;
  sharing: boolean; // test pattern
  screenSharing: boolean; // real getDisplayMedia
  micOn: boolean; // synthetic tone
  realMicOn: boolean; // real microphone published
  webcamOn: boolean;
  currentMeetingCode: string | null;
  tileLayoutMode: TileLayoutMode;
  /** #785: the mode an AUTOMATIC spotlight (first share arriving) left behind,
   * restored when the last share goes away. `undefined`/null means nothing to
   * restore -- the user is in the mode they chose. Owned by
   * tileLayout.ts's `commitLayoutModeTransition`; never assign it directly. */
  autoSpotlightRestoreMode?: TileLayoutMode | null;
  pinnedTileId: string | null;
  layoutModeButtons: Record<TileLayoutMode, HTMLButtonElement> | null;
  speakerSmoothingTimer: ReturnType<typeof setInterval> | null;
  activeRemoteControl: ActiveRemoteControl | null;
  remoteControlSeq: number;
  viewerDemandSeq: number;
  audioCtx: AudioContext | null;
  oscillator: OscillatorNode | null;
  syntheticCameraIntervalId: ReturnType<typeof setInterval> | null;
  /** Refs #378: opt-in, default OFF -- see constants.ts's
   * HARNESS_LOCAL_ECHO_STORAGE_KEY. Mirrors desktop's
   * session.localEchoEnabled; gates all local-echo rendering in
   * remoteControlUi.ts. */
  localEchoEnabled: boolean;
  /** #669: opt-in, default OFF -- see constants.ts's
   * HARNESS_DEBUG_MODE_STORAGE_KEY. Gates the remote-window header's Debug
   * button via the shared `debugHeaderControlVisible` predicate (also
   * consumed by the desktop client). Unlike desktop's Rust-owned setting,
   * localStorage works fine here -- a browser tab is one JS realm, so there
   * is no cross-webview propagation problem to solve. */
  debugModeEnabled: boolean;
}

export interface HarnessDom {
  joinScreen: HTMLDivElement;
  meetingScreen: HTMLDivElement;
  joinCard: HTMLDivElement;
  displayNameInput: HTMLInputElement;
  meetingCodeInput: HTMLInputElement;
  joinBtn: HTMLButtonElement;
  createBtn: HTMLButtonElement | null;
  connError: HTMLElement;
  joinHint: HTMLElement;
  roomNameEl: HTMLElement;
  roomCopyButton: HTMLButtonElement;
  roomRenameButton: HTMLButtonElement;
  elapsedEl: HTMLElement;
  connState: HTMLElement;
  participantCountEl: HTMLElement;
  tilesEl: HTMLDivElement;
  networkDiagnosticsRows: HTMLDivElement;
  topbarRight: HTMLDivElement;
  ctlAudio: HTMLButtonElement;
  ctlVideo: HTMLButtonElement;
  audioCaret: HTMLButtonElement;
  videoCaret: HTMLButtonElement;
  devicesMenu: HTMLElement;
  devicesMenuTitle: HTMLElement;
  devicesMenuBody: HTMLElement;
  ctlShare: HTMLButtonElement;
  ctlShareLabel: HTMLElement;
  ctlDraw: HTMLButtonElement;
  ctlDrawLabel: HTMLElement;
  ctlInvite: HTMLButtonElement;
  ctlInviteTooltip: HTMLElement;
  ctlLeave: HTMLButtonElement;
  shareBtn: HTMLButtonElement;
  shareState: HTMLElement;
  canvas: HTMLCanvasElement;
  trackNameDisplay: HTMLElement;
  shareScreenState: HTMLElement;
  micRealState: HTMLElement;
  micCheckbox: HTMLInputElement;
  micState: HTMLElement;
  localEchoCheckbox: HTMLInputElement;
  debugModeCheckbox: HTMLInputElement;
  webcamState: HTMLElement;
  cameraTrackNameDisplay: HTMLElement;
  sessionLog: HTMLDivElement;
  toastEl: HTMLDivElement;
}

// UI helpers returned by setupUiHelpers, plus the logger.
export interface HarnessUi {
  logEvent: (message: string, kind?: LogKind) => void;
  setConnState: (text: string, cls: 'idle' | 'connecting' | 'connected' | 'error') => void;
  showError: (message: string) => void;
  clearError: () => void;
  showMeetingScreen: (code: string, roomMetadata?: string | null) => void;
  showJoinScreen: () => void;
  /** Join-link auto-join interstitial: replaces the home screen with a
   * "Joining <label>" card so a link never flashes the main menu first.
   * Optional because partially-mocked test contexts omit it -- callers use
   * `?.`. Dismissed by `showMeetingScreen`/`showJoinScreen`. */
  showConnectingScreen?: (label: string) => void;
  /** Live sub-status on the connecting interstitial ("Requesting access…",
   * retry notices). No-op when the interstitial is not up. */
  setConnectingStatus?: (text: string) => void;
  showToast: (message: string) => void;
  /** #679: like `showToast`, but forwards an optional inline action
   * (e.g. "Bring to front") and a custom auto-dismiss duration -- used by
   * the remote-share-started notice. */
  showActionableToast: (message: string, dismissMs: number, action?: SharedToastAction) => void;
  setShareState: (text: string, on: boolean) => void;
  setMicState: (text: string, on: boolean) => void;
  setScreenShareState: (text: string, on: boolean) => void;
  setRealMicState: (text: string, on: boolean) => void;
  setWebcamState: (text: string, on: boolean) => void;
  setJoinControlsEnabled: (enabled: boolean) => void;
  setAudioControl: (state: 'off' | 'live' | 'muted') => void;
  setVideoControl: (on: boolean) => void;
  setShareControl: (on: boolean, identity?: string | null, paletteIndex?: number | null) => void;
}

// Cross-module callbacks wired at bootstrap. These are the seams that would
// otherwise create import cycles between the tile/telepointer/remote-control
// clusters; each is assigned once in main.ts after all modules are set up.
export interface HarnessCallbacks {
  syncHarnessHook: () => void;
  updateUnifiedCtaLabel: () => void;
  recordRecentRoom: (code: string) => void;
  refreshRecentRooms: () => void;
  roomDisplayLabelForCredential: (code: string) => string;
  renameRoomDisplayName: (code: string, displayName: string | null) => string;
  verifyH264Negotiated: (track: LocalVideoTrack, label: string) => Promise<void>;
  startTelepointerSender: () => void;
  stopTelepointerSender: () => void;
  bindHoverTelepointer: (tile: HTMLDivElement) => void;
  startCanvasAnimation: () => void;
  // tileLayout
  applyTileLayout: () => void;
  applySpeakingRings: () => void;
  startSpeakerSmoothing: () => void;
  smoothSpeakingScores: () => void;
  resetActiveSpeakers: () => void;
  shareTileCount: () => number;
  pinTile: (tile: HTMLDivElement, source: 'manual' | 'auto') => void;
  bindTileInteractions: (tile: HTMLDivElement) => void;
  fitTileLabels: (tile: HTMLElement) => void;
  /** #669: re-run every live remote-window header's `syncMode()` so a Debug
   * mode toggle reaches already-open headers without needing a new share. */
  syncRemoteWindowHeaders: () => void;
  // telepointerDisplay
  shareTileForWindowId: (windowId: number) => HTMLDivElement | null;
  renderTelepointersForWindow: (windowId: number) => void;
  removeTelepointersForParticipant: (identity: string) => void;
  removeTelepointersForWindow: (windowId: number) => void;
  handleRemoteTelepointerPayload: (payload: Uint8Array, senderIdentity?: string, topic?: string) => void;
  handleRemoteDrawPayload: (payload: Uint8Array, senderIdentity?: string, topic?: string) => void;
  handleRemoteControlPayload: (payload: Uint8Array, senderIdentity?: string) => void;
  handleLatencyProbePayload: (payload: Uint8Array, senderIdentity?: string) => void;
  handlePipelineStatsPayload: (payload: Uint8Array, senderIdentity?: string) => void;
  startLatencyProbe: () => void;
  stopLatencyProbe: () => void;
  resetRemoteControlHarnessSession?: () => void;
  /** RC-N2W (#819): make this peer answer a native controller's request and
   * record what it receives. Off by default -- a harness peer that advertised
   * itself as controllable to any room member would be a behaviour change to a
   * deployed page, not a test fixture. It never injects and never claims an
   * input was applied; see remoteControlHostLedger.ts. */
  enableRemoteControlHostEmulation?: () => void;
  remoteControlHostLedger?: () => {
    granted: boolean;
    kinds: string[];
    count: number;
    publishError: string | null;
  };
  startPipelineStats: () => void;
  stopPipelineStats: () => void;
  repositionRemoteTelepointers: () => void;
  repositionRemoteDraw: () => void;
  clearRemoteTelepointers: () => void;
  clearRemoteDraw: () => void;
  renderDrawForWindow: (windowId: number, ownerIdentity?: string) => void;
  removeDrawForWindow: (windowId: number, ownerIdentity?: string) => void;
  removeDrawForParticipant: (identity: string) => void;
  // aiChat (#657). Sender identity for every inbound message comes from the
  // authenticated LiveKit participant, never the payload; see aiChat.ts.
  handleAiChatPayload: (payload: Uint8Array, senderIdentity?: string, topic?: string) => void;
  aiChatSessionFor: (
    windowId: number,
    ownerIdentity: string
  ) => import('./aiChat').AiChatSessionState | null;
  startAiChat: (windowId: number, ownerIdentity: string) => void;
  stopAiChat: (windowId: number, ownerIdentity: string) => void;
  aiChatPttStart: (windowId: number, ownerIdentity: string) => void;
  aiChatPttEnd: (windowId: number, ownerIdentity: string) => void;
  aiChatSendText: (windowId: number, ownerIdentity: string, text: string) => void;
  aiChatLocalPttHeld: (windowId: number, ownerIdentity: string) => boolean;
  /** Release every push-to-talk floor this client holds. Idempotent. */
  aiChatReleaseAllPtt: (reason: string) => void;
  aiChatOwnerLeft: (ownerIdentity: string) => void;
  resetAiChat: () => void;
  onAiChatChange: (listener: () => void) => () => void;
  // drawSender
  setDrawMode: (on: boolean) => void;
  syncDrawAvailability?: () => void;
  // remoteControlUi
  stopRemoteControl: (reason?: string) => void;
  startRemoteControl: (tile: HTMLDivElement) => void;
  activeRemoteControlForTile: (tile: HTMLDivElement) => ActiveRemoteControl | null;
  ensureRemoteControlAffordance: (tile: HTMLDivElement) => void;
  // viewerDemand
  publishViewerDemand: (tile: HTMLDivElement, kind: 'open' | 'closed' | 'heartbeat') => void;
  publishViewerDemandForPublication: (ownerIdentity: string, publication: import('livekit-client').RemoteTrackPublication) => void;
  startViewerDemandHeartbeat: () => void;
  stopViewerDemandHeartbeat: () => void;
  // tiles
  ensureBaseTile: (identity: string, isLocal: boolean) => HTMLDivElement;
  setTileCamera: (
    identity: string,
    isLocal: boolean,
    track: import('livekit-client').RemoteTrack | LocalVideoTrack,
    drawWindowId?: number | null
  ) => void;
  clearTileCamera: (identity: string) => void;
  addShareTile: (
    identity: string,
    isLocal: boolean,
    key: string,
    track: import('livekit-client').RemoteTrack | LocalVideoTrack,
    label: string,
    windowId?: number | null,
    participantMetadata?: string | null
  ) => void;
  removeShareTile: (identity: string, key: string) => void;
  /** #627: hold the last rendered frame for a share ahead of a known gap. */
  holdShareFrame: (identity: string, trackSid: string, reason: HoldReason) => void;
  removeParticipantTiles: (identity: string) => void;
  clearTiles: () => void;
  setParticipantAudioActive: (identity: string, active: boolean) => void;
  updateParticipantCount: () => void;
  updateParticipantShareColorProfiles: (participant: import('livekit-client').RemoteParticipant) => void;
  refreshParticipantGrid: () => void;
  trackedShareWindows: () => import('./publicationReconcile').TrackedShareWindow[];
  isCameraTrack: (pub: RemoteTrackPublication) => boolean;
  publicationPaused: (pub: RemoteTrackPublication) => boolean;
  setPublicationPaused: (
    participant: import('livekit-client').RemoteParticipant,
    pub: RemoteTrackPublication,
    paused: boolean,
    source?: string
  ) => void;
  syncStreamStates: (currentRoom: Room) => void;
  ensureFrameMetadataWorker: () => Worker | null;
  // connection
  connectToMeeting: (meetingCode: string, identity: string) => Promise<void>;
  resolveIdentity: () => string;
  submitMeetingField: () => Promise<void>;
  // controls
  startTestPatternShare: () => Promise<void>;
  startCockpitWebcam: () => Promise<{ trackName: string }>;
  stopCockpitWebcam: () => Promise<{ trackName: string; stopped: boolean }>;
  startCockpitAudioTone: () => Promise<{ trackName: string }>;
  measureCockpitRemoteAudio: (windowMs?: number) => Promise<{
    ok: boolean;
    rms: number;
    energyDelta: number;
    durationDelta: number;
    trackSid: string;
    publisher: string;
    detail: string;
  }>;
  measureCockpitRemoteCamera: (windowMs?: number) => Promise<{
    ok: boolean;
    classification: 'PASS' | 'TEST-FAIL' | 'INFRA-FAIL';
    fps: number;
    width: number;
    height: number;
    framesDecodedDelta: number | null;
    nonBlackRatio: number;
    interFrameDiff: number;
    trackSid: string;
    publisher: string;
    detail: string;
  }>;
  publishCockpitTelepointer: () => Promise<{ windowId: number }>;
  publishCockpitDrawStroke: () => Promise<{ windowId: number }>;
}

export interface HarnessContext {
  windowId: number;
  dom: HarnessDom;
  state: HarnessState;
  ui: HarnessUi;
  hook: HarnessHook;
  cb: HarnessCallbacks;
  // Shared collections (created once, mutated in place).
  activeSpeakerTargets: Set<string>;
  speakerScores: Map<string, number>;
  remoteTelepointers: Map<string, RemoteTelepointerState>;
  handshakeCooldowns: Map<string, number>;
}
