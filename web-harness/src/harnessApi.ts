import type {
  HarnessContext,
  HarnessMeasurementApi,
  HarnessPhotonFrame,
  HarnessPressToPhotonInput,
  HarnessPressToPhotonResult,
  HarnessRemoteControlInput,
  HarnessRemoteControlTarget
} from './context';
import {
  LATENCY_PROBE_TOPIC,
  type LatencyProbeMessage,
  type RemoteControlMessage,
  type RemoteControlModifiers
} from './trackNames.ts';
import { canonicalRemoteControlFingerprint, decodeRemoteControlHotPath, newRemoteControlInputId, parseRemoteControlJson, remoteControlGrantEnvelopeIsValid, remoteControlPublishOptions } from './remoteControl.ts';
import {
  decodePhotonSentinelFrame,
  matchesExpectedPhotonGeneration,
  nextPhotonGeneration
} from './remoteControlPhoton.ts';
import {
  applyHostEmulationDecision,
  createHostEmulationState,
  hostEmulationDecision,
  receivedControlKinds
} from './remoteControlHostLedger.ts';

interface RemoteControlUiPrimitives {
  nextRemoteControlSeq: () => number;
  publishRemoteControl: (message: RemoteControlMessage) => Promise<void>;
  startRemoteControl: (tile: HTMLDivElement) => void;
  stopRemoteControl: (reason?: string) => void;
}

const EMPTY_REMOTE_CONTROL_MODIFIERS: RemoteControlModifiers = {
  alt: false,
  ctrl: false,
  meta: false,
  shift: false
};
const LATENCY_PROBE_INTERVAL_MS = 2000;
const LATENCY_PROBE_EXPIRY_MS = 30_000;
const MAX_OUTSTANDING_LATENCY_PROBES = 64;
const MAX_LATENCY_PROBE_METRICS = 120;
const MAX_COMPLETED_CLICK_REPLAY_WINDOW_MS = 10_000;
const JS_SAFE_PROBE_COUNTER_MOD = 4096;
const latencyProbeEncoder = new TextEncoder();

// #580: kinds the host authorizes per-packet against the active grant token.
// `request`/`release`/`status`/`result` are intentionally absent -- they are
// how a grant is established in the first place and carry no token.
const REMOTE_CONTROL_INPUT_KINDS = new Set(['pointer', 'wheel', 'key', 'text']);

type HarnessRemoteControlTokenlessInput = {
  kind: string;
  seq: number;
  windowId: number;
  targetUserId: string;
  at: number;
};

type HarnessRemoteControlPublishMetric = {
  kind: string;
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
};

type HarnessRemoteControlStatusMetric = {
  status: Extract<RemoteControlMessage, { kind: 'status' }>['status'];
  message: string;
  seq: number;
  windowId: number;
  targetUserId: string;
  controllerId: string;
  senderIdentity?: string;
  receivedAt: number;
};

type HarnessRemoteControlResultMetric = {
  inputId: string;
  inputSeq: number;
  outcome: string;
  deliveryRoute?: Extract<RemoteControlMessage, { kind: 'result' }>['deliveryRoute'];
  failureCode?: Extract<RemoteControlMessage, { kind: 'result' }>['failureCode'];
  windowId: number;
  receivedAt: number;
};

type HarnessRemoteControlTerminalDisposition = Pick<
  HarnessRemoteControlResultMetric,
  'outcome' | 'deliveryRoute' | 'failureCode'
>;

type HarnessCompletedClickReplay = {
  target: HarnessRemoteControlTarget;
  packet: Extract<RemoteControlMessage, { kind: 'pointer' }>;
  expiresAt: number;
  firstDisposition: HarnessRemoteControlTerminalDisposition | null;
  replayArmed: boolean;
  replayCompleted: boolean;
  auditTainted: boolean;
  auditResolve: (() => void) | null;
  auditReject: ((error: Error) => void) | null;
  expiryTimer: ReturnType<typeof setTimeout> | null;
};

type HarnessLatencyProbeMetric = {
  probeId: number;
  peerIdentity: string;
  rttMs: number;
  receivedAt: number;
};

function isLatencyProbeMessage(value: unknown): value is LatencyProbeMessage {
  const message = value as Partial<LatencyProbeMessage> | null;
  return (
    !!message &&
    message.v === 1 &&
    (message.kind === 'ping' || message.kind === 'pong') &&
    Number.isSafeInteger(message.probeId) &&
    typeof message.senderId === 'string' &&
    Number.isSafeInteger(message.sendTimeMs) &&
    (message.receiverReceiveTimeMs === undefined || Number.isSafeInteger(message.receiverReceiveTimeMs)) &&
    (message.receiverSendTimeMs === undefined || Number.isSafeInteger(message.receiverSendTimeMs))
  );
}

// ---------------------------------------------------------------------------
// `window.__petalHarness.remoteControl` automation API. Drives the same
// `petal.remote-control` publish path as the interactive UI, but resolves its
// target from tile DOM (or the currently-active session) so browser automation
// can request/click/drag/type against a remote share without a real pointer.
// ---------------------------------------------------------------------------
export function setupHarnessApi(ctx: HarnessContext, rc: RemoteControlUiPrimitives) {
  const { state, hook } = ctx;
  const { shareTileForWindowId } = ctx.cb;
  const publishedMetrics: HarnessRemoteControlPublishMetric[] = [];
  const tokenlessInputPublishes: HarnessRemoteControlTokenlessInput[] = [];
  const statusMetrics: HarnessRemoteControlStatusMetric[] = [];
  const resultMetrics: HarnessRemoteControlResultMetric[] = [];
  const grants = new Map<string, {
    controlSessionId: string;
    retryEnabled: boolean;
    dedupGuaranteeWindowMs: number;
    nextInputSeq: number;
  }>();
  // RC-N2W (#819). Off unless a cockpit scenario turns it on -- see
  // remoteControlHostLedger.ts for what this does and does not prove.
  const hostEmulation = createHostEmulationState();
  const pendingDiscrete = new Map<string, {
    target: HarnessRemoteControlTarget;
    controlSessionId: string;
    inputSeq: number;
    operationFingerprint: string;
    expiresAt: number;
  }>();
  const outstandingLatencyProbes = new Map<string, Map<number, number>>();
  const latencyProbeMetrics: HarnessLatencyProbeMetric[] = [];
  let latencyProbeTimer: ReturnType<typeof setInterval> | null = null;
  let latencyProbeSeq = 0;
  let completedClickReplay: HarnessCompletedClickReplay | null = null;
  let cropCanvas: HTMLCanvasElement | null = null;
  let cropContext: CanvasRenderingContext2D | null = null;

  function clearCompletedClickReplay(
    expected?: HarnessCompletedClickReplay,
    reason = 'completed click replay is unavailable'
  ) {
    if (expected && completedClickReplay !== expected) return;
    const replay = completedClickReplay;
    if (replay?.expiryTimer != null) {
      clearTimeout(replay.expiryTimer);
    }
    completedClickReplay = null;
    replay?.auditReject?.(new Error(reason));
    if (replay) {
      replay.auditResolve = null;
      replay.auditReject = null;
    }
  }

  function clearCompletedClickReplayForTarget(
    target: Pick<HarnessRemoteControlTarget, 'targetUserId' | 'windowId'>
  ) {
    if (
      completedClickReplay?.target.targetUserId === target.targetUserId
      && completedClickReplay.target.windowId === target.windowId
    ) {
      clearCompletedClickReplay();
    }
  }

  function scheduleCompletedClickReplayExpiry(replay: HarnessCompletedClickReplay) {
    const delayMs = Math.max(0, replay.expiresAt - Date.now());
    replay.expiryTimer = setTimeout(() => {
      if (completedClickReplay !== replay) return;
      completedClickReplay = null;
      replay.expiryTimer = null;
      const resolve = replay.auditResolve;
      const reject = replay.auditReject;
      replay.auditResolve = null;
      replay.auditReject = null;
      if (replay.replayCompleted && !replay.auditTainted) {
        resolve?.();
      } else {
        reject?.(new Error('completed click replay audit failed'));
      }
    }, delayMs);
  }

  function boundedDedupWindowMs(value: number): number {
    if (!Number.isSafeInteger(value) || value <= 0) return 0;
    return Math.min(value, MAX_COMPLETED_CLICK_REPLAY_WINDOW_MS);
  }

  function terminalDisposition(
    message: Extract<RemoteControlMessage, { kind: 'result' }>
  ): HarnessRemoteControlTerminalDisposition {
    return {
      outcome: message.outcome,
      ...(message.deliveryRoute === undefined ? {} : { deliveryRoute: message.deliveryRoute }),
      ...(message.failureCode === undefined ? {} : { failureCode: message.failureCode })
    };
  }

  function sameTerminalDisposition(
    left: HarnessRemoteControlTerminalDisposition,
    right: HarnessRemoteControlTerminalDisposition
  ): boolean {
    return left.outcome === right.outcome
      && left.deliveryRoute === right.deliveryRoute
      && left.failureCode === right.failureCode;
  }

  function resultMatchesCompletedClick(
    message: Extract<RemoteControlMessage, { kind: 'result' }>,
    senderIdentity: string | undefined,
    replay: HarnessCompletedClickReplay
  ): boolean {
    const packet = replay.packet;
    return senderIdentity === replay.target.targetUserId
      && state.room?.localParticipant.identity === message.targetUserId
      && message.controllerId === replay.target.targetUserId
      && message.targetUserId === packet.controllerId
      && message.windowId === packet.windowId
      && message.controlSessionId === packet.controlSessionId
      && message.inputId === packet.inputId
      && message.inputSeq === packet.inputSeq
      && message.operationFingerprintVersion === 1
      && message.operationFingerprint === packet.operationFingerprint;
  }

  function normalizedHarnessModifiers(modifiers?: Partial<RemoteControlModifiers>): RemoteControlModifiers {
    return { ...EMPTY_REMOTE_CONTROL_MODIFIERS, ...modifiers };
  }

  function normalizedHarnessPoint(value: number, label: string): number {
    if (!Number.isFinite(value)) throw new Error(`remoteControl.${label} must be a finite normalized number`);
    return Math.min(1, Math.max(0, value));
  }

  function pointerButtonMask(button: number): number {
    if (button === 1) return 4;
    if (button === 2) return 2;
    return 1;
  }

  function remoteControlTargetFromTile(
    tile: HTMLDivElement
  ): Pick<HarnessRemoteControlTarget, 'targetUserId' | 'windowId'> | null {
    const targetUserId = tile.dataset.owner?.trim() ?? '';
    const windowId = Number(tile.dataset.windowId);
    if (!targetUserId || !Number.isSafeInteger(windowId) || windowId < 1 || windowId > 0xffff_ffff) return null;
    return { targetUserId, windowId };
  }

  function harnessTargetFromTile(tile: HTMLDivElement): HarnessRemoteControlTarget | null {
    const target = remoteControlTargetFromTile(tile);
    if (!target) return null;
    return { ...target, tileId: tile.id };
  }

  function harnessRemoteTargets(): HarnessRemoteControlTarget[] {
    return Array.from(document.querySelectorAll<HTMLDivElement>('.share-tile'))
      .map(harnessTargetFromTile)
      .filter((target): target is HarnessRemoteControlTarget => !!target);
  }

  function tileForHarnessTarget(target: Partial<HarnessRemoteControlTarget>): HTMLDivElement | null {
    if (target.tileId) {
      const tile = document.getElementById(target.tileId);
      if (tile instanceof HTMLDivElement) return tile;
    }
    if (Number.isSafeInteger(target.windowId)) {
      return shareTileForWindowId(target.windowId as number);
    }
    return null;
  }

  function captureCrop(windowId: number, x: number, y: number, w: number, h: number): ImageData | null {
    if (typeof document === 'undefined') return null;
    const tile = shareTileForWindowId(windowId);
    const video = tile?.querySelector<HTMLVideoElement>('video') ?? null;
    if (!cropCanvas) {
      cropCanvas = document.createElement('canvas');
      cropContext = cropCanvas.getContext('2d', { willReadFrequently: true });
    }
    if (!video || !cropContext || video.videoWidth <= 0 || video.videoHeight <= 0 || video.readyState < 2) return null;

    const width = video.videoWidth;
    const height = video.videoHeight;
    const cropX = Math.max(0, Math.floor(x));
    const cropY = Math.max(0, Math.floor(y));
    const cropRight = Math.min(width, Math.floor(x + w));
    const cropBottom = Math.min(height, Math.floor(y + h));
    const cropW = cropRight - cropX;
    const cropH = cropBottom - cropY;
    if (cropW <= 0 || cropH <= 0) return null;

    cropCanvas.width = width;
    cropCanvas.height = height;
    try {
      cropContext.drawImage(video, 0, 0, width, height);
      return cropContext.getImageData(cropX, cropY, cropW, cropH);
    } catch {
      return null;
    }
  }

  function captureFramePng(windowId: number) {
    if (typeof document === 'undefined') return null;
    const tile = shareTileForWindowId(windowId);
    const video = tile?.querySelector<HTMLVideoElement>('video') ?? null;
    if (!cropCanvas) {
      cropCanvas = document.createElement('canvas');
      cropContext = cropCanvas.getContext('2d', { willReadFrequently: true });
    }
    if (!video || !cropContext || video.videoWidth <= 0 || video.videoHeight <= 0 || video.readyState < 2) return null;

    cropCanvas.width = video.videoWidth;
    cropCanvas.height = video.videoHeight;
    try {
      cropContext.drawImage(video, 0, 0, cropCanvas.width, cropCanvas.height);
      return {
        width: cropCanvas.width,
        height: cropCanvas.height,
        currentTime: video.currentTime,
        dataUrl: cropCanvas.toDataURL('image/png')
      };
    } catch {
      return null;
    }
  }

  function photonFrameForTarget(target: HarnessRemoteControlTarget): HarnessPhotonFrame | null {
    if (typeof document === 'undefined') return null;
    const tile = tileForHarnessTarget(target);
    const video = tile?.querySelector<HTMLVideoElement>('video') ?? null;
    if (!cropCanvas) {
      cropCanvas = document.createElement('canvas');
      cropContext = cropCanvas.getContext('2d', { willReadFrequently: true });
    }
    if (!video || !cropContext || video.videoWidth <= 0 || video.videoHeight <= 0 || video.readyState < 2) {
      return null;
    }

    cropCanvas.width = video.videoWidth;
    cropCanvas.height = video.videoHeight;
    try {
      cropContext.drawImage(video, 0, 0, cropCanvas.width, cropCanvas.height);
      const decoded = decodePhotonSentinelFrame(
        cropContext.getImageData(0, 0, cropCanvas.width, cropCanvas.height)
      );
      return decoded
        ? { ...decoded, width: cropCanvas.width, height: cropCanvas.height }
        : null;
    } catch {
      return null;
    }
  }

  function resolveHarnessRemoteTarget(input?: HarnessRemoteControlInput): HarnessRemoteControlTarget {
    const requested = input?.target ?? input;
    if (requested?.targetUserId && Number.isSafeInteger(requested.windowId)) {
      const tile = tileForHarnessTarget(requested);
      return {
        targetUserId: requested.targetUserId,
        windowId: requested.windowId as number,
        tileId: tile?.id ?? requested.tileId
      };
    }

    if (Number.isSafeInteger(requested?.windowId)) {
      const matched = harnessRemoteTargets().filter((target) => target.windowId === requested!.windowId);
      if (matched.length === 1) return matched[0];
      if (matched.length > 1) throw new Error(`remoteControl target windowId ${requested!.windowId} is ambiguous`);
    }

    if (state.activeRemoteControl) {
      return {
        targetUserId: state.activeRemoteControl.targetUserId,
        windowId: state.activeRemoteControl.windowId,
        tileId: state.activeRemoteControl.tileId
      };
    }

    const targets = harnessRemoteTargets();
    if (targets.length === 1) return targets[0];
    if (targets.length === 0) throw new Error('remoteControl has no remote share tiles to target');
    throw new Error('remoteControl target is ambiguous; pass { targetUserId, windowId }');
  }

  function remoteControlBaseForTarget(target: HarnessRemoteControlTarget) {
    if (!state.room) throw new Error('remoteControl requires an active LiveKit room');
    const active = state.activeRemoteControl;
    const grantToken =
      active?.targetUserId === target.targetUserId && active.windowId === target.windowId
        ? active.grantToken ?? undefined
        : undefined;
    return {
      v: 1 as const,
      targetUserId: target.targetUserId,
      controllerId: state.room.localParticipant.identity,
      windowId: target.windowId,
      seq: rc.nextRemoteControlSeq(),
      // Omit the key entirely (rather than set it to undefined) to match the
      // wire's skip_serializing_if convention and keep object shape stable
      // for callers that compare the message strictly.
      ...(grantToken !== undefined ? { grantToken } : {})
    };
  }

  function grantKey(target: Pick<HarnessRemoteControlTarget, 'targetUserId' | 'windowId'>) {
    return `${target.targetUserId}\u0000${target.windowId}`;
  }

  async function addV2DiscreteEnvelope(
    target: HarnessRemoteControlTarget,
    message: Extract<RemoteControlMessage, { kind: 'pointer' | 'key' | 'text' }>
  ): Promise<RemoteControlMessage> {
    const grant = grants.get(grantKey(target));
    if (!grant || grant.retryEnabled) return message;
    const inputSeq = grant.nextInputSeq++;
    const envelope = { controlSessionId: grant.controlSessionId, inputId: newRemoteControlInputId(), inputSeq };
    const operationFingerprint = await canonicalRemoteControlFingerprint(message, envelope);
    const sentAt = Date.now();
    pendingDiscrete.set(envelope.inputId, {
      target,
      controlSessionId: envelope.controlSessionId,
      inputSeq: envelope.inputSeq,
      operationFingerprint,
      expiresAt: sentAt + grant.dedupGuaranteeWindowMs
    });
    return { ...message, ...envelope, operationFingerprintVersion: 1, operationFingerprint };
  }

  async function publishHarnessRemoteControl(
    target: HarnessRemoteControlTarget,
    message: RemoteControlMessage
  ): Promise<HarnessRemoteControlTarget> {
    // A replay is only valid for the immediately preceding harness operation.
    // Any normal publish supersedes the retained click before it can be used.
    clearCompletedClickReplay();
    const options = remoteControlPublishOptions(message);
    // #580: the host drops any input packet that carries no grant token
    // (TOKENLESS_GRANT_COMPATIBILITY_ENABLED = false, #493). Count them so a
    // driver can assert zero instead of reading a silent success -- publishing
    // still happens, because case 24 ("release drops later input") needs the
    // tokenless packet to actually reach the host and be rejected there.
    if (REMOTE_CONTROL_INPUT_KINDS.has(message.kind) && !message.grantToken) {
      tokenlessInputPublishes.push({
        kind: message.kind,
        seq: message.seq,
        windowId: message.windowId,
        targetUserId: message.targetUserId,
        at: Date.now()
      });
    }
    publishedMetrics.push({
      kind: message.kind,
      grantToken: message.grantToken ?? null,
      action: 'action' in message ? message.action : undefined,
      seq: message.seq,
      windowId: message.windowId,
      targetUserId: message.targetUserId,
      at: Date.now(),
      reliable: options.reliable,
      button: 'button' in message ? message.button : undefined,
      buttons: 'buttons' in message ? message.buttons : undefined,
      clickCount: 'clickCount' in message ? message.clickCount : undefined,
      key: 'key' in message ? message.key : undefined,
      code: 'code' in message ? message.code : undefined,
      modifiers: 'modifiers' in message ? message.modifiers : undefined,
      x: 'x' in message ? message.x : undefined,
      y: 'y' in message ? message.y : undefined,
      deltaX: 'deltaX' in message ? message.deltaX : undefined,
      deltaY: 'deltaY' in message ? message.deltaY : undefined,
      deltaMode: 'deltaMode' in message ? message.deltaMode : undefined
    });
    const discrete = message.kind === 'pointer' && message.action === 'click' || message.kind === 'key' || message.kind === 'text';
    const packet = discrete
      ? await addV2DiscreteEnvelope(
          target,
          message as Extract<RemoteControlMessage, { kind: 'pointer' | 'key' | 'text' }>
        )
      : message;
    let retained: HarnessCompletedClickReplay | null = null;
    if (
      packet.kind === 'pointer'
      && packet.action === 'click'
      && packet.button === 0
      && packet.buttons === 0
      && packet.operationFingerprintVersion === 1
      && packet.controlSessionId
      && packet.inputId
      && packet.inputSeq !== undefined
    ) {
      const pending = pendingDiscrete.get(packet.inputId);
      if (pending && pending.expiresAt > Date.now()) {
        retained = {
          target,
          packet,
          expiresAt: pending.expiresAt,
          firstDisposition: null,
          replayArmed: false,
          replayCompleted: false,
          auditTainted: false,
          auditResolve: null,
          auditReject: null,
          expiryTimer: null
        };
        completedClickReplay = retained;
        scheduleCompletedClickReplayExpiry(retained);
      }
    }
    try {
      await rc.publishRemoteControl(packet);
    } catch (error) {
      if (packet.inputId) pendingDiscrete.delete(packet.inputId);
      if (retained) clearCompletedClickReplay(retained);
      throw error;
    }
    return target;
  }

  async function replayLastCompletedClick(): Promise<void> {
    const replay = completedClickReplay;
    if (
      !replay
      || !replay.firstDisposition
      || replay.replayArmed
      || replay.replayCompleted
      || replay.auditTainted
      || Date.now() >= replay.expiresAt
    ) {
      if (replay && Date.now() >= replay.expiresAt) clearCompletedClickReplay(replay);
      throw new Error('completed click replay is unavailable');
    }
    const grant = grants.get(grantKey(replay.target));
    if (
      !grant
      || grant.controlSessionId !== replay.packet.controlSessionId
      || grant.retryEnabled
    ) {
      clearCompletedClickReplay(replay);
      throw new Error('completed click replay is unavailable');
    }
    const seq = rc.nextRemoteControlSeq();
    if (seq === replay.packet.seq) {
      clearCompletedClickReplay(replay);
      throw new Error('completed click replay requires a fresh transport sequence');
    }
    const packet = { ...replay.packet, seq };
    // Arm before publish: a synchronous test transport may deliver the cached
    // result before the publish promise settles. A rejection rolls this state
    // back below so it can never authorize a later stray packet.
    replay.replayArmed = true;
    try {
      await rc.publishRemoteControl(packet);
    } catch {
      clearCompletedClickReplay(replay);
      throw new Error('completed click replay publish failed');
    }
    if (completedClickReplay !== replay || Date.now() >= replay.expiresAt) {
      clearCompletedClickReplay(replay);
      throw new Error('completed click replay audit failed');
    }
    await new Promise<void>((resolve, reject) => {
      replay.auditResolve = resolve;
      replay.auditReject = reject;
    });
  }

  // #580: this used to raw-publish a bare `request` envelope whenever the
  // caller named a target explicitly, which never minted a grant token --
  // so every later input packet went out tokenless and the host dropped
  // 100% of them ("dropping tokenless input ... compatibility window has
  // ended"). Never reintroduce an explicit-target branch that skips
  // rc.startRemoteControl: harness and UI must share one grant contract.
  function harnessRequest(input?: HarnessRemoteControlInput): HarnessRemoteControlTarget {
    const target = resolveHarnessRemoteTarget(input);
    clearCompletedClickReplayForTarget(target);
    const tile = tileForHarnessTarget(target);
    if (!tile) {
      throw new Error(
        `remoteControl.request found no share tile for window ${target.windowId}; a grant token cannot be minted without one`
      );
    }
    rc.startRemoteControl(tile);
    const active = state.activeRemoteControl;
    if (active?.targetUserId !== target.targetUserId || active.windowId !== target.windowId) {
      throw new Error(
        `remoteControl.request did not take control of window ${target.windowId}; no grant will be issued`
      );
    }
    return target;
  }

  function harnessRelease(input?: HarnessRemoteControlInput): HarnessRemoteControlTarget {
    const target = resolveHarnessRemoteTarget(input);
    clearCompletedClickReplayForTarget(target);
    if (
      state.activeRemoteControl?.targetUserId === target.targetUserId &&
      state.activeRemoteControl.windowId === target.windowId
    ) {
      rc.stopRemoteControl('automation');
    } else {
      publishHarnessRemoteControl(target, { ...remoteControlBaseForTarget(target), kind: 'release' });
    }
    return target;
  }

  function harnessPointerMessage(
    input: HarnessRemoteControlInput & {
      action?: 'move' | 'down' | 'up';
      x: number;
      y: number;
      button?: number;
      buttons?: number;
      clickCount?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ): { target: HarnessRemoteControlTarget; message: RemoteControlMessage } {
    const target = resolveHarnessRemoteTarget(input);
    const action = input.action ?? 'move';
    return {
      target,
      message: {
        ...remoteControlBaseForTarget(target),
        kind: 'pointer',
        action,
        x: normalizedHarnessPoint(input.x, 'x'),
        y: normalizedHarnessPoint(input.y, 'y'),
        button: input.button ?? (action === 'move' ? -1 : 0),
        buttons: input.buttons ?? (action === 'down' ? 1 : 0),
        ...(input.clickCount !== undefined ? { clickCount: input.clickCount } : {}),
        modifiers: normalizedHarnessModifiers(input.modifiers)
      }
    };
  }

  function harnessPointer(
    input: HarnessRemoteControlInput & {
      action?: 'move' | 'down' | 'up';
      x: number;
      y: number;
      button?: number;
      buttons?: number;
      clickCount?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ): HarnessRemoteControlTarget {
    const { target, message } = harnessPointerMessage(input);
    void publishHarnessRemoteControl(target, message);
    return target;
  }

  async function publishHarnessPointer(
    input: HarnessRemoteControlInput & {
      action?: 'move' | 'down' | 'up';
      x: number;
      y: number;
      button?: number;
      buttons?: number;
      clickCount?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ): Promise<HarnessRemoteControlTarget> {
    const { target, message } = harnessPointerMessage(input);
    await publishHarnessRemoteControl(target, message);
    return target;
  }

  function harnessClick(
    input: HarnessRemoteControlInput & {
      x: number;
      y: number;
      button?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ): HarnessRemoteControlTarget {
    const target = resolveHarnessRemoteTarget(input);
    const base = remoteControlBaseForTarget(target);
    void publishHarnessRemoteControl(target, {
      ...base,
      kind: 'pointer',
      action: 'click',
      x: normalizedHarnessPoint(input.x, 'x'),
      y: normalizedHarnessPoint(input.y, 'y'),
      button: input.button ?? 0,
      buttons: 0,
      modifiers: normalizedHarnessModifiers(input.modifiers)
    });
    return target;
  }

  // #373: a real double-click as a down/up pair with clickCount=2 (not the
  // synthetic `action: 'click'` semantic packet -- that path never carries a
  // multi-click count and is only ever consumed by the harness itself, not
  // real controllers). Mirrors what a real controller's DOM `pointerdown`
  // `detail` would carry on the second click of a sequence.
  async function harnessDoubleClick(
    input: HarnessRemoteControlInput & {
      x: number;
      y: number;
      button?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ): Promise<HarnessRemoteControlTarget> {
    await publishHarnessPointer({
      ...input,
      action: 'down',
      buttons: pointerButtonMask(input.button ?? 0),
      clickCount: 2
    });
    return publishHarnessPointer({
      ...input,
      action: 'up',
      buttons: 0,
      clickCount: 2
    });
  }

  async function harnessDrag(
    input: HarnessRemoteControlInput & {
      from: { x: number; y: number };
      to: { x: number; y: number };
      steps?: number;
      button?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ): Promise<HarnessRemoteControlTarget> {
    const steps = Math.min(120, Math.max(2, Math.round(input.steps ?? 12)));
    const down = await publishHarnessPointer({
      ...input,
      action: 'down',
      x: input.from.x,
      y: input.from.y,
      button: input.button ?? 0,
      buttons: pointerButtonMask(input.button ?? 0)
    });
    for (let i = 1; i <= steps; i += 1) {
      const t = i / steps;
      await publishHarnessPointer({
        ...input,
        action: 'move',
        x: input.from.x + (input.to.x - input.from.x) * t,
        y: input.from.y + (input.to.y - input.from.y) * t,
        button: -1,
        buttons: pointerButtonMask(input.button ?? 0)
      });
    }
    await publishHarnessPointer({
      ...input,
      action: 'up',
      x: input.to.x,
      y: input.to.y,
      button: input.button ?? 0,
      buttons: 0
    });
    return down;
  }

  function harnessWheel(
    input: HarnessRemoteControlInput & {
      x: number;
      y: number;
      deltaX?: number;
      deltaY: number;
      deltaMode?: 0 | 1 | 2;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ): HarnessRemoteControlTarget {
    const target = resolveHarnessRemoteTarget(input);
    publishHarnessRemoteControl(target, {
      ...remoteControlBaseForTarget(target),
      kind: 'wheel',
      x: normalizedHarnessPoint(input.x, 'x'),
      y: normalizedHarnessPoint(input.y, 'y'),
      deltaX: input.deltaX ?? 0,
      deltaY: input.deltaY,
      deltaMode: input.deltaMode ?? 0,
      modifiers: normalizedHarnessModifiers(input.modifiers)
    });
    return target;
  }

  function harnessKey(
    input: HarnessRemoteControlInput & {
      action?: 'down' | 'up' | 'press';
      key: string;
      code?: string;
      repeat?: boolean;
      location?: number;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ): HarnessRemoteControlTarget {
    const target = resolveHarnessRemoteTarget(input);
    const action = input.action ?? 'press';
    const code = input.code ?? input.key;
    const publish = (phase: 'down' | 'up') =>
      publishHarnessRemoteControl(target, {
        ...remoteControlBaseForTarget(target),
        kind: 'key',
        action: phase,
        key: input.key,
        code,
        repeat: phase === 'down' ? (input.repeat ?? false) : false,
        location: input.location,
        modifiers: normalizedHarnessModifiers(input.modifiers)
      });
    if (action === 'press') {
      publish('down');
      publish('up');
    } else {
      publish(action);
    }
    return target;
  }

  function harnessText(
    input: HarnessRemoteControlInput & {
      text: string;
      modifiers?: Partial<RemoteControlModifiers>;
    }
  ): HarnessRemoteControlTarget {
    const target = resolveHarnessRemoteTarget(input);
    publishHarnessRemoteControl(target, {
      ...remoteControlBaseForTarget(target),
      kind: 'text',
      text: input.text,
      modifiers: normalizedHarnessModifiers(input.modifiers)
    });
    return target;
  }

  function harnessPhotonFrame(input?: HarnessRemoteControlInput): HarnessPhotonFrame | null {
    return photonFrameForTarget(resolveHarnessRemoteTarget(input));
  }

  function observePhotonGeneration(
    video: HTMLVideoElement,
    target: HarnessRemoteControlTarget,
    expectedGeneration: number,
    timeoutMs: number
  ) {
    const requestFrame = video.requestVideoFrameCallback?.bind(video);
    if (!requestFrame) {
      throw new Error('requestVideoFrameCallback is unavailable; cannot measure press-to-photon');
    }

    let callbackId: number | null = null;
    let timeoutId: ReturnType<typeof setTimeout> | null = null;
    let settled = false;
    let rejectObservation: ((reason?: unknown) => void) | null = null;

    const cleanup = () => {
      if (timeoutId !== null) clearTimeout(timeoutId);
      timeoutId = null;
      if (callbackId !== null) video.cancelVideoFrameCallback?.(callbackId);
      callbackId = null;
    };
    const cancel = (reason: unknown) => {
      if (settled) return;
      settled = true;
      cleanup();
      rejectObservation?.(reason);
    };

    const promise = new Promise<{
      frame: HarnessPhotonFrame;
      callbackNowMs: number;
      expectedDisplayTimeMs: number;
      mediaTime: number;
      presentedFrames: number;
    }>((resolve, reject) => {
      rejectObservation = reject;
      const onFrame: VideoFrameRequestCallback = (now, metadata) => {
        callbackId = null;
        if (settled) return;
        const frame = photonFrameForTarget(target);
        if (matchesExpectedPhotonGeneration(frame, expectedGeneration)) {
          settled = true;
          cleanup();
          resolve({
            frame,
            callbackNowMs: now,
            expectedDisplayTimeMs: metadata.expectedDisplayTime,
            mediaTime: metadata.mediaTime,
            presentedFrames: metadata.presentedFrames
          });
          return;
        }
        callbackId = requestFrame(onFrame);
      };
      callbackId = requestFrame(onFrame);
      timeoutId = setTimeout(
        () => cancel(new Error(`sentinel generation ${expectedGeneration} was not visible within ${timeoutMs}ms`)),
        timeoutMs
      );
    });

    return { promise, cancel };
  }

  async function measurePressToPhoton(
    input: HarnessPressToPhotonInput
  ): Promise<HarnessPressToPhotonResult> {
    const target = resolveHarnessRemoteTarget(input);
    const tile = tileForHarnessTarget(target);
    const video = tile?.querySelector<HTMLVideoElement>('video') ?? null;
    if (!video) throw new Error('press-to-photon target has no video element');
    const baseline = photonFrameForTarget(target);
    if (!baseline) throw new Error('press-to-photon sentinel frame is not decodable');
    const expectedGeneration = nextPhotonGeneration(baseline.generation);
    const timeoutMs = Math.min(10_000, Math.max(100, Math.round(input.timeoutMs ?? 2_000)));
    const observer = observePhotonGeneration(video, target, expectedGeneration, timeoutMs);
    const sentAt = performance.now();

    try {
      if (input.kind === 'click') {
        const x = normalizedHarnessPoint(input.x ?? 0.75, 'x');
        const y = normalizedHarnessPoint(input.y ?? 0.58, 'y');
        await publishHarnessRemoteControl(target, {
          ...remoteControlBaseForTarget(target),
          kind: 'pointer',
          action: 'click',
          x,
          y,
          button: 0,
          buttons: 0,
          modifiers: EMPTY_REMOTE_CONTROL_MODIFIERS
        });
      } else {
        const text = input.text ?? 'x';
        if (Array.from(text).length !== 1) {
          throw new Error('press-to-photon text input must contain exactly one Unicode character');
        }
        await publishHarnessRemoteControl(target, {
          ...remoteControlBaseForTarget(target),
          kind: 'text',
          text,
          modifiers: EMPTY_REMOTE_CONTROL_MODIFIERS
        });
      }
      const publishCompleteMs = performance.now() - sentAt;
      const observed = await observer.promise;
      const estimatedDisplayTime = Number.isFinite(observed.expectedDisplayTimeMs)
        ? Math.max(observed.callbackNowMs, observed.expectedDisplayTimeMs)
        : observed.callbackNowMs;
      return {
        inputKind: input.kind,
        baselineGeneration: baseline.generation,
        observedGeneration: observed.frame.generation,
        baselineConfidence: baseline.confidence,
        observedConfidence: observed.frame.confidence,
        pressToFrameCallbackMs: Math.max(0, observed.callbackNowMs - sentAt),
        pressToEstimatedPhotonMs: Math.max(0, estimatedDisplayTime - sentAt),
        publishCompleteMs: Math.max(0, publishCompleteMs),
        mediaTime: observed.mediaTime,
        presentedFrames: observed.presentedFrames
      };
    } catch (error) {
      observer.cancel(error);
      await observer.promise.catch(() => {});
      throw error;
    }
  }

  function handleRemoteControlPayload(payload: Uint8Array, senderIdentity?: string) {
    let message: RemoteControlMessage;
    const binary = decodeRemoteControlHotPath(payload, state.room?.localParticipant.identity ?? '', senderIdentity ?? '');
    if (binary) {
      message = binary;
    } else {
      const parsed = parseRemoteControlJson(new TextDecoder().decode(payload));
      if (!parsed) return;
      message = parsed;
    }
    if (message.v !== 1) return;
    // RC-N2W (#819): the only place in this harness where it acts as a control
    // HOST rather than a controller. Runs before the controller-side branches
    // below because those return early on every kind an input carries.
    if (hostEmulation.enabled) {
      const localIdentity = state.room?.localParticipant.identity ?? '';
      const decision = hostEmulationDecision(hostEmulation, message, localIdentity, senderIdentity);
      applyHostEmulationDecision(hostEmulation, decision);
      if (decision.action === 'grant' || decision.action === 'stop') {
        const action = decision.action;
        void rc.publishRemoteControl(decision.status).catch((error) => {
          hostEmulation.publishError = String((error as Error)?.message ?? error);
          if (action === 'grant') {
            hostEmulation.granted = false;
            hostEmulation.controllerId = null;
            hostEmulation.grantToken = null;
          }
        });
        return;
      }
      if (decision.action === 'record') return;
    }
    if (message.kind === 'result') {
      const pending = pendingDiscrete.get(message.inputId);
      const replay = completedClickReplay;
      const correlated = !!pending
        && pending.target.targetUserId === message.controllerId
        && pending.target.windowId === message.windowId
        && pending.controlSessionId === message.controlSessionId
        && pending.inputSeq === message.inputSeq
        && pending.operationFingerprint === message.operationFingerprint
        && (!senderIdentity || senderIdentity === pending.target.targetUserId);
      const knownOutcome = ['applied', 'submitted', 'unauthorized', 'grantExpired', 'targetUnavailable', 'targetOffScreen', 'accessibilityDenied', 'resolveFailed', 'replayFailed', 'superseded', 'malformed', 'admissionOverloaded'].includes(message.outcome);
      const localRecipient = state.room?.localParticipant.identity === message.targetUserId;
      if (correlated && knownOutcome && localRecipient) {
        pendingDiscrete.delete(message.inputId);
        resultMetrics.push({
          inputId: message.inputId,
          inputSeq: message.inputSeq,
          outcome: message.outcome,
          deliveryRoute: message.deliveryRoute,
          failureCode: message.failureCode,
          windowId: message.windowId,
          receivedAt: Date.now()
        });
        if (resultMetrics.length > 120) resultMetrics.splice(0, resultMetrics.length - 120);
        if (replay && replay.packet.inputId === message.inputId) {
          if (
            Date.now() < replay.expiresAt
            && resultMatchesCompletedClick(message, senderIdentity, replay)
          ) {
            replay.firstDisposition = terminalDisposition(message);
          } else {
            clearCompletedClickReplay(replay);
          }
        }
        return;
      }
      if (
        replay
        && replay.firstDisposition
        && replay.replayArmed
        && knownOutcome
        && Date.now() < replay.expiresAt
        && resultMatchesCompletedClick(message, senderIdentity, replay)
      ) {
        resultMetrics.push({
          inputId: message.inputId,
          inputSeq: message.inputSeq,
          outcome: message.outcome,
          deliveryRoute: message.deliveryRoute,
          failureCode: message.failureCode,
          windowId: message.windowId,
          receivedAt: Date.now()
        });
        if (resultMetrics.length > 120) resultMetrics.splice(0, resultMetrics.length - 120);
        if (
          !replay.replayCompleted
          && !replay.auditTainted
          && sameTerminalDisposition(replay.firstDisposition, terminalDisposition(message))
        ) {
          replay.replayCompleted = true;
        } else {
          replay.auditTainted = true;
        }
      }
      return;
    }
    if (message.kind !== 'status') return;
    // #370 corrective pass (Moderate finding): `parseRemoteControlJson`
    // already validates `status` against the canonical wire status list --
    // this used to be a second, hand-copied, shorter allowlist that silently
    // dropped the real `requestFailed`/`textTruncated` statuses. Binary
    // frames can never decode to kind 'status' in the first place, so the
    // JSON-path validation above is the only gate this needs.
    statusMetrics.push({
      status: message.status,
      message: message.message,
      seq: message.seq,
      windowId: message.windowId,
      targetUserId: message.targetUserId,
      controllerId: message.controllerId,
      senderIdentity,
      receivedAt: Date.now()
    });
    // Security: only the host we're actually requesting control of may mutate
    // our local v2 grant/result-capability state -- same check as the UI path
    // (remoteControlUi's senderIsHost) and the 'result' correlation above
    // (:747, :752), which both treat `controllerId` as the host's identity in
    // status/result messages (see grantKey usage below -- it's keyed on
    // message.controllerId, not targetUserId). Without this, any room peer
    // could broadcast a spoofed 'active'/'stopped' status naming our own
    // controllerId to poison our controlSessionId or force our grant to
    // appear stopped, defeating #377's per-grant token binding for
    // harness-driven control specifically.
    const senderIsHost = senderIdentity === message.controllerId;
    const grantEnvelopeIsValid = remoteControlGrantEnvelopeIsValid(message);
    if (senderIsHost && grantEnvelopeIsValid && message.status === 'active' && message.controlSessionId && message.resultCapability) {
      const key = grantKey({ targetUserId: message.controllerId, windowId: message.windowId });
      const previousGrant = grants.get(key);
      if (previousGrant && previousGrant.controlSessionId !== message.controlSessionId) {
        clearCompletedClickReplayForTarget({
          targetUserId: message.controllerId,
          windowId: message.windowId
        });
      }
      grants.set(grantKey({ targetUserId: message.controllerId, windowId: message.windowId }), {
        controlSessionId: message.controlSessionId,
        retryEnabled: message.resultCapability.retryEnabled,
        dedupGuaranteeWindowMs: boundedDedupWindowMs(
          message.resultCapability.dedupGuaranteeWindowMs
        ),
        nextInputSeq: 1
      });
    } else if (senderIsHost && grantEnvelopeIsValid && message.status === 'stopped') {
      clearCompletedClickReplayForTarget({
        targetUserId: message.controllerId,
        windowId: message.windowId
      });
      grants.delete(grantKey({ targetUserId: message.controllerId, windowId: message.windowId }));
      for (const [inputId, pending] of pendingDiscrete) {
        if (pending.target.targetUserId === message.controllerId && pending.target.windowId === message.windowId) pendingDiscrete.delete(inputId);
      }
    }
  }

  function outstandingLatencyProbeCount(): number {
    let count = 0;
    outstandingLatencyProbes.forEach((probes) => {
      count += probes.size;
    });
    return count;
  }

  function pruneOutstandingLatencyProbes(now = Date.now()) {
    for (const [peerIdentity, probes] of outstandingLatencyProbes) {
      for (const [probeId, sendTimeMs] of probes) {
        if (now - sendTimeMs > LATENCY_PROBE_EXPIRY_MS) probes.delete(probeId);
      }
      if (probes.size === 0) outstandingLatencyProbes.delete(peerIdentity);
    }
    while (outstandingLatencyProbeCount() > MAX_OUTSTANDING_LATENCY_PROBES) {
      let oldestPeerIdentity: string | null = null;
      let oldestProbeId: number | null = null;
      let oldestSendTimeMs = Number.POSITIVE_INFINITY;
      for (const [peerIdentity, probes] of outstandingLatencyProbes) {
        for (const [probeId, sendTimeMs] of probes) {
          if (sendTimeMs < oldestSendTimeMs) {
            oldestPeerIdentity = peerIdentity;
            oldestProbeId = probeId;
            oldestSendTimeMs = sendTimeMs;
          }
        }
      }
      if (oldestPeerIdentity === null || oldestProbeId === null) break;
      const probes = outstandingLatencyProbes.get(oldestPeerIdentity);
      probes?.delete(oldestProbeId);
      if (probes?.size === 0) outstandingLatencyProbes.delete(oldestPeerIdentity);
    }
  }

  function rememberOutstandingLatencyProbe(peerIdentity: string, probeId: number, sendTimeMs: number) {
    pruneOutstandingLatencyProbes(sendTimeMs);
    let probes = outstandingLatencyProbes.get(peerIdentity);
    if (!probes) {
      probes = new Map<number, number>();
      outstandingLatencyProbes.set(peerIdentity, probes);
    }
    probes.set(probeId, sendTimeMs);
    pruneOutstandingLatencyProbes(sendTimeMs);
  }

  function takeOutstandingLatencyProbe(peerIdentity: string, probeId: number): number | undefined {
    const probes = outstandingLatencyProbes.get(peerIdentity);
    if (!probes) return undefined;
    const sendTimeMs = probes.get(probeId);
    probes.delete(probeId);
    if (probes.size === 0) outstandingLatencyProbes.delete(peerIdentity);
    return sendTimeMs;
  }

  function latencyProbeTargetIdentities(): string[] {
    if (!state.room) return [];
    return Array.from(state.room.remoteParticipants.keys());
  }

  function nextLatencyProbeId(sendTimeMs: number): number {
    latencyProbeSeq = (latencyProbeSeq + 1) % JS_SAFE_PROBE_COUNTER_MOD;
    return sendTimeMs * JS_SAFE_PROBE_COUNTER_MOD + latencyProbeSeq;
  }

  function publishLatencyProbe(message: LatencyProbeMessage, destinationIdentity?: string): Promise<void> {
    if (!state.room) return Promise.resolve();
    const options = destinationIdentity
      ? { topic: LATENCY_PROBE_TOPIC, reliable: true, destinationIdentities: [destinationIdentity] }
      : { topic: LATENCY_PROBE_TOPIC, reliable: true };
    return state.room.localParticipant
      .publishData(latencyProbeEncoder.encode(JSON.stringify(message)), options)
      .catch((err) => {
        console.debug(`latency probe publish failed: ${(err as Error).message ?? err}`);
      });
  }

  function pingLatencyProbe(destinationIdentity?: string): LatencyProbeMessage | null {
    if (!state.room) return null;
    const targetIdentities = destinationIdentity ? [destinationIdentity] : latencyProbeTargetIdentities();
    let firstMessage: LatencyProbeMessage | null = null;
    for (const peerIdentity of targetIdentities) {
      const sendTimeMs = Date.now();
      const probeId = nextLatencyProbeId(sendTimeMs);
      rememberOutstandingLatencyProbe(peerIdentity, probeId, sendTimeMs);
      const message: LatencyProbeMessage = {
        v: 1,
        kind: 'ping',
        probeId,
        senderId: state.room.localParticipant.identity,
        sendTimeMs
      };
      void publishLatencyProbe(message, peerIdentity);
      firstMessage ??= message;
    }
    return firstMessage;
  }

  function handleLatencyProbePayload(payload: Uint8Array, senderIdentity?: string) {
    if (!state.room || !senderIdentity || senderIdentity === state.room.localParticipant.identity) return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(new TextDecoder().decode(payload));
    } catch {
      return;
    }
    if (!isLatencyProbeMessage(parsed)) return;

    if (parsed.kind === 'ping') {
      const receiverReceiveTimeMs = Date.now();
      void publishLatencyProbe(
        {
          v: 1,
          kind: 'pong',
          probeId: parsed.probeId,
          senderId: state.room.localParticipant.identity,
          sendTimeMs: parsed.sendTimeMs,
          receiverReceiveTimeMs,
          receiverSendTimeMs: Date.now()
        },
        senderIdentity
      );
      return;
    }

    const sentAt = takeOutstandingLatencyProbe(senderIdentity, parsed.probeId);
    if (sentAt === undefined) return;
    const rttMs = Math.max(0, Date.now() - sentAt);
    latencyProbeMetrics.push({
      probeId: parsed.probeId,
      peerIdentity: senderIdentity,
      rttMs,
      receivedAt: Date.now()
    });
    latencyProbeMetrics.splice(0, Math.max(0, latencyProbeMetrics.length - MAX_LATENCY_PROBE_METRICS));
    console.debug(`latency probe: peer RTT to ${senderIdentity} ${rttMs.toFixed(1)} ms`);
  }

  function startLatencyProbe() {
    stopLatencyProbe();
    pingLatencyProbe();
    latencyProbeTimer = setInterval(() => {
      pingLatencyProbe();
    }, LATENCY_PROBE_INTERVAL_MS);
  }

  function stopLatencyProbe() {
    if (latencyProbeTimer !== null) {
      clearInterval(latencyProbeTimer);
      latencyProbeTimer = null;
    }
    outstandingLatencyProbes.clear();
  }

  function resetRemoteControlSession() {
    clearCompletedClickReplay(undefined, 'completed click replay session ended');
    grants.clear();
    pendingDiscrete.clear();
  }

  hook.remoteControl = {
    targets: harnessRemoteTargets,
    active: () =>
      state.activeRemoteControl
        ? {
            targetUserId: state.activeRemoteControl.targetUserId,
            windowId: state.activeRemoteControl.windowId,
            tileId: state.activeRemoteControl.tileId,
            grantToken: state.activeRemoteControl.grantToken ?? null
          }
        : null,
    // #580: a driver must be able to prove it actually holds a grant before
    // it trusts any case -- especially the absence-asserting ones, which pass
    // vacuously when nothing can be injected at all.
    grant: (input?: HarnessRemoteControlInput) => {
      const target = resolveHarnessRemoteTarget(input);
      const active = state.activeRemoteControl;
      const matched =
        active?.targetUserId === target.targetUserId && active.windowId === target.windowId;
      const grantToken = matched ? active.grantToken ?? null : null;
      return {
        target,
        granted: !!grantToken,
        grantToken,
        // #820/case-30: whether the host negotiated the v2 envelope. A macOS
        // host advertises legacy-only BY CONTRACT (docs/CONTRACTS.md), so
        // v2-only cases must skip on it rather than time out.
        controlSessionId: matched ? active.controlSessionId ?? null : null,
        tokenlessInputs: tokenlessInputPublishes.filter(
          (entry) => entry.windowId === target.windowId && entry.targetUserId === target.targetUserId
        ).length
      };
    },
    metrics: () => ({ published: publishedMetrics.slice(), statuses: statusMetrics.slice(), results: resultMetrics.slice(), pending: Array.from(pendingDiscrete.keys()), tokenlessInputs: tokenlessInputPublishes.slice() }),
    resetMetrics: () => {
      publishedMetrics.length = 0;
      statusMetrics.length = 0;
      resultMetrics.length = 0;
      tokenlessInputPublishes.length = 0;
      pendingDiscrete.clear();
      clearCompletedClickReplay();
    },
    replayLastCompletedClick,
    request: harnessRequest,
    release: harnessRelease,
    pointer: harnessPointer,
    click: harnessClick,
    doubleClick: harnessDoubleClick,
    drag: harnessDrag,
    wheel: harnessWheel,
    key: harnessKey,
    text: harnessText,
    photonFrame: harnessPhotonFrame,
    pressToPhoton: measurePressToPhoton
  };

  hook.latencyProbe = {
    latestRttMs: () => latencyProbeMetrics.at(-1)?.rttMs ?? null,
    metrics: () => latencyProbeMetrics.slice(),
    resetMetrics: () => {
      latencyProbeMetrics.length = 0;
      outstandingLatencyProbes.clear();
    },
    ping: pingLatencyProbe
  };

  const measurementApi: HarnessMeasurementApi = {
    captureCrop,
    captureFramePng,
    stallStats: (windowId: number) => hook.pipelineStats?.stallStats(windowId) ?? null,
    resetStallStats: (windowId: number) => {
      hook.pipelineStats?.resetStallStats(windowId);
    }
  };
  hook.testPatternMeasurement = measurementApi;

  // Test-only visual proof mode: place the decoded video above the harness UI.
  // Optional exactWidth/exactHeight CSS dimensions let automation demand and
  // capture a source-sized layer instead of inheriting the meeting tile size.
  const exactFrameUrl = typeof window === 'undefined' ? null : new URL(window.location.href);
  const exactFrameWindowId = Number(exactFrameUrl?.searchParams.get('exactFrame'));
  const exactFrameCssWidth = Number(exactFrameUrl?.searchParams.get('exactWidth'));
  const exactFrameCssHeight = Number(exactFrameUrl?.searchParams.get('exactHeight'));
  if (typeof document !== 'undefined' && Number.isSafeInteger(exactFrameWindowId) && exactFrameWindowId > 0) {
    window.setInterval(() => {
      const tile = shareTileForWindowId(exactFrameWindowId);
      const video = tile?.querySelector<HTMLVideoElement>('video') ?? null;
      if (!video || video.readyState < 2 || video.videoWidth <= 0 || video.videoHeight <= 0) return;
      const width = video.videoWidth;
      const height = video.videoHeight;
      const deviceScale = Math.max(1, window.devicePixelRatio || 1);
      const cssWidth = exactFrameCssWidth > 0 ? exactFrameCssWidth : width / deviceScale;
      const cssHeight = exactFrameCssHeight > 0 ? exactFrameCssHeight : height / deviceScale;
      video.id = 'petal-exact-decoded-frame';
      video.setAttribute('aria-label', `Decoded receiver frame ${width} by ${height}`);
      video.style.setProperty('display', 'block', 'important');
      video.style.setProperty('position', 'fixed', 'important');
      video.style.setProperty('left', '0', 'important');
      video.style.setProperty('top', '0', 'important');
      video.style.setProperty('z-index', '2147483647', 'important');
      video.style.setProperty('width', `${cssWidth}px`, 'important');
      video.style.setProperty('height', `${cssHeight}px`, 'important');
      video.style.setProperty('max-width', 'none', 'important');
      video.style.setProperty('max-height', 'none', 'important');
      video.style.setProperty('object-fit', 'fill', 'important');
      video.style.setProperty('transform', 'none', 'important');
      document.documentElement.style.width = `${cssWidth}px`;
      document.documentElement.style.height = `${cssHeight}px`;
      document.body.style.margin = '0';
      document.body.style.width = `${cssWidth}px`;
      document.body.style.height = `${cssHeight}px`;
      document.body.style.overflow = 'hidden';
    }, 100);
  }

  function enableRemoteControlHostEmulation() {
    hostEmulation.enabled = true;
  }

  function remoteControlHostLedger() {
    return {
      granted: hostEmulation.granted,
      kinds: receivedControlKinds(hostEmulation),
      count: hostEmulation.received.length,
      publishError: hostEmulation.publishError
    };
  }

  return {
    handleRemoteControlPayload,
    handleLatencyProbePayload,
    startLatencyProbe,
    stopLatencyProbe,
    resetRemoteControlSession,
    enableRemoteControlHostEmulation,
    remoteControlHostLedger
  };
}
