import {
  Track,
  type LocalVideoTrack,
  type RemoteParticipant,
  type RemoteTrack,
  type RemoteTrackPublication,
} from 'livekit-client';
import type { HarnessContext } from './context.ts';
import type { HarnessStallStats } from './context.ts';
import {
  PIPELINE_STATS_TOPIC,
  type CaptureStateReport,
  type PipelineStageMetrics,
  type PipelineLifecycle,
  type PipelineStatsMessage,
  type ReceiverFreezeMetrics,
} from './trackNames.ts';
import { windowIdFromTrackName } from './telepointer.ts';
import {
  deriveRemoteWindowStats,
  type RemoteWindowStatsState,
} from './remoteWindowStats.ts';
import {
  StartupTimelineRecorder,
  type CapturePath,
  type RequestedSubscription,
} from './startupTimeline.ts';
import { noteVideoFrames } from './analytics.ts';

const POLL_MS = 1000;
const MAX_RECORDED_MESSAGES = 200;
const PIPELINE_REDUCER_TTL_MS = 5_000;
const MAX_PIPELINE_REDUCER_KEYS = 200;
const STARTUP_STATS_TTL_MS = 10 * 60 * 1000;
const MAX_STARTUP_STATS_KEYS = 200;
// #256 soak tier: count any receiver frame-arrival gap above 1s as a stall.
const STALL_GAP_THRESHOLD_MS = 1000;
const encoder = new TextEncoder();
const decoder = new TextDecoder();

type StatsLike = Record<string, unknown>;
type StatsTrack = RemoteTrack & {
  getRTCStatsReport?: () => Promise<RTCStatsReport | null>;
  receiver?: RTCRtpReceiver;
};

interface CounterState {
  sampledAtMs: number;
  bytes: number | null;
  frames: number | null;
}

interface StartupStatsCacheEntry {
  signature: string;
  updatedAt: number;
}

interface InboundStatsTrack {
  identity: string;
  publication: RemoteTrackPublication;
  track: RemoteTrack;
}

interface StallStatsState extends HarnessStallStats {
  lastFrameAtMs: number | null;
  lastFramesDecoded: number | null;
}

function finiteNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function positiveInteger(value: unknown): number | null {
  const valueNumber = finiteNumber(value);
  if (valueNumber === null || valueNumber <= 0) return null;
  return Math.round(valueNumber);
}

function firstNumber(...values: unknown[]): number | null {
  for (const value of values) {
    const valueNumber = finiteNumber(value);
    if (valueNumber !== null) return valueNumber;
  }
  return null;
}

function firstPositiveInteger(...values: unknown[]): number | null {
  for (const value of values) {
    const valueNumber = positiveInteger(value);
    if (valueNumber !== null) return valueNumber;
  }
  return null;
}

function stageHasSignal(stage: PipelineStageMetrics | null): stage is PipelineStageMetrics {
  return !!stage && (stage.width !== null || stage.height !== null || stage.fps !== null || stage.kbps !== null);
}

function captureStateHasSignal(state: CaptureStateReport | null): state is CaptureStateReport {
  return !!state;
}

function receiverFreezeHasSignal(metrics: ReceiverFreezeMetrics | null): metrics is ReceiverFreezeMetrics {
  return !!metrics;
}

function nonemptyStage(stage: PipelineStageMetrics | null): PipelineStageMetrics | null {
  return stageHasSignal(stage) ? stage : null;
}

function statsKind(stats: StatsLike): string {
  const kind = typeof stats.kind === 'string' ? stats.kind : null;
  const mediaType = typeof stats.mediaType === 'string' ? stats.mediaType : null;
  return (kind ?? mediaType ?? '').toLowerCase();
}

function isVideoStats(stats: StatsLike): boolean {
  const kind = statsKind(stats);
  return kind === 'video' || (!kind && (finiteNumber(stats.frameWidth) !== null || finiteNumber(stats.framesEncoded) !== null));
}

function bestOutboundStats(report: RTCStatsReport): StatsLike | null {
  let best: StatsLike | null = null;
  report.forEach((raw) => {
    const stats = raw as StatsLike;
    if (stats.type !== 'outbound-rtp' || !isVideoStats(stats)) return;
    const currentBytes = finiteNumber(best?.bytesSent) ?? -1;
    const nextBytes = finiteNumber(stats.bytesSent) ?? -1;
    if (!best || nextBytes >= currentBytes) best = stats;
  });
  return best;
}

/** RID is not guaranteed on browser inbound-rtp reports. Never infer it from
 * dimensions: report the exact browser field or the literal unavailable. */
export function actualInboundRid(report: RTCStatsReport): string | 'unavailable' {
  let best: StatsLike | null = null;
  report.forEach((raw) => {
    const stats = raw as StatsLike;
    if (stats.type !== 'inbound-rtp' || !isVideoStats(stats)) return;
    const currentBytes = finiteNumber(best?.bytesReceived) ?? -1;
    const nextBytes = finiteNumber(stats.bytesReceived) ?? -1;
    if (!best || nextBytes >= currentBytes) best = stats;
  });
  const selected = best as StatsLike | null;
  const rid = selected && typeof selected.rid === 'string' ? selected.rid.trim() : '';
  return rid && rid.length <= 32 ? rid : 'unavailable';
}

function deriveRate(current: number | null, previous: number | null, previousSampledAtMs: number | null, sampledAtMs: number): number | null {
  if (current === null || previous === null || previousSampledAtMs === null || sampledAtMs <= previousSampledAtMs) {
    return null;
  }
  const delta = current - previous;
  if (delta < 0) return null;
  const seconds = (sampledAtMs - previousSampledAtMs) / 1000;
  return seconds > 0 ? delta / seconds : null;
}

function mediaTrackStage(track: LocalVideoTrack): PipelineStageMetrics {
  const settings = track.mediaStreamTrack.getSettings?.() ?? {};
  return {
    width: firstPositiveInteger(settings.width),
    height: firstPositiveInteger(settings.height),
    fps: firstNumber(settings.frameRate),
    kbps: null,
  };
}

function captureStateForTrack(
  track: LocalVideoTrack,
  grabbed: PipelineStageMetrics | null,
  encodedSent: PipelineStageMetrics | null
): CaptureStateReport {
  const mediaTrack = track.mediaStreamTrack;
  const readyState = mediaTrack.readyState;
  const state = readyState === 'ended' ? 'wedged' : mediaTrack.muted ? 'idle' : 'live';
  return {
    state,
    fps: firstNumber(encodedSent?.fps, grabbed?.fps),
    dirtyRectCount: null,
    dirtyAreaPx: null,
    occlusionPct: null,
    cpu: {
      lockCopyMs: null,
      convertMs: null,
      captureFrameReturnMs: null,
    },
  };
}

async function encodedStageForTrack(
  track: LocalVideoTrack,
  key: string,
  states: Map<string, CounterState>,
  sampledAtMs: number
): Promise<PipelineStageMetrics | null> {
  const sender = track.sender;
  const settings = track.mediaStreamTrack.getSettings?.() ?? {};
  if (!sender) {
    return nonemptyStage({
      width: firstPositiveInteger(settings.width),
      height: firstPositiveInteger(settings.height),
      fps: null,
      kbps: null,
    });
  }

  let report: RTCStatsReport;
  try {
    report = await sender.getStats();
  } catch {
    return null;
  }
  const outbound = bestOutboundStats(report);
  if (!outbound) return null;

  const previous = states.get(key) ?? null;
  const bytes = firstNumber(outbound.bytesSent);
  const frames = firstNumber(outbound.framesEncoded);
  const kbps = deriveRate(bytes, previous?.bytes ?? null, previous?.sampledAtMs ?? null, sampledAtMs);
  const encodedFps = deriveRate(frames, previous?.frames ?? null, previous?.sampledAtMs ?? null, sampledAtMs);
  states.set(key, { sampledAtMs, bytes, frames });

  return nonemptyStage({
    width: firstPositiveInteger(outbound.frameWidth, settings.width),
    height: firstPositiveInteger(outbound.frameHeight, settings.height),
    fps: firstNumber(outbound.framesPerSecond, encodedFps),
    kbps: kbps === null ? null : kbps * 8 / 1000,
  });
}

async function getRemoteStatsReport(track: RemoteTrack): Promise<RTCStatsReport | null> {
  const statsTrack = track as StatsTrack;
  if (typeof statsTrack.getRTCStatsReport === 'function') {
    const report = await statsTrack.getRTCStatsReport();
    if (report) return report;
  }
  if (typeof statsTrack.receiver?.getStats === 'function') {
    return statsTrack.receiver.getStats();
  }
  return null;
}

export function parsePipelineStatsPayload(payload: Uint8Array | string): PipelineStatsMessage | null {
  let text: string;
  try {
    text = typeof payload === 'string' ? payload : decoder.decode(payload);
  } catch {
    return null;
  }

  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }
  if (!raw || typeof raw !== 'object') return null;
  const candidate = raw as Partial<PipelineStatsMessage>;
  if (
    candidate.v !== 1 ||
    (candidate.role !== 'sender' && candidate.role !== 'receiver') ||
    typeof candidate.reporterId !== 'string' ||
    typeof candidate.ownerIdentity !== 'string' ||
    typeof candidate.windowId !== 'number' ||
    !Number.isSafeInteger(candidate.windowId) ||
    candidate.windowId < 1 ||
    typeof candidate.seq !== 'number' ||
    !Number.isSafeInteger(candidate.seq) ||
    typeof candidate.sentAtMs !== 'number' ||
    !Number.isFinite(candidate.sentAtMs)
  ) {
    return null;
  }

  const optionalString = (value: unknown): string | null | undefined => {
    if (value === undefined || value === null) return null;
    if (typeof value !== 'string' || !value.trim() || value.length > 160) return undefined;
    return value.trim();
  };
  const publicationSid = optionalString(candidate.publicationSid);
  const shareEpoch = optionalString(candidate.shareEpoch);
  const lifecycleValue = candidate.lifecycle;
  const lifecycle: PipelineLifecycle | null | undefined =
    lifecycleValue === undefined || lifecycleValue === null
      ? null
      : ['captureReady', 'published', 'subscribed', 'firstDecoded', 'firstPresented', 'unsubscribed', 'unpublished', 'terminalFailure'].includes(lifecycleValue as string)
        ? lifecycleValue as PipelineLifecycle
        : undefined;
  // Identity ownership is checked at receipt time, where the authenticated
  // LiveKit sender is available. Keep the standalone parser legacy-friendly.
  if (publicationSid === undefined || shareEpoch === undefined || lifecycle === undefined) return null;

  const stage = (value: unknown): PipelineStageMetrics | null | undefined => {
    if (value === null) return null;
    if (!value || typeof value !== 'object') return undefined;
    const rawStage = value as Partial<PipelineStageMetrics>;
    const validNullableNumber = (number: unknown) =>
      number === null || (typeof number === 'number' && Number.isFinite(number));
    if (
      !validNullableNumber(rawStage.width) ||
      !validNullableNumber(rawStage.height) ||
      !validNullableNumber(rawStage.fps) ||
      !validNullableNumber(rawStage.kbps)
    ) {
      return undefined;
    }
    return {
      width: rawStage.width ?? null,
      height: rawStage.height ?? null,
      fps: rawStage.fps ?? null,
      kbps: rawStage.kbps ?? null,
    };
  };

  const validNullableNumber = (number: unknown) =>
    number === null || (typeof number === 'number' && Number.isFinite(number));
  const captureState = (value: unknown): CaptureStateReport | null | undefined => {
    // `undefined` means the field is genuinely ABSENT -- e.g. an
    // already-shipped desktop build sending a v1 message from before this
    // field existed. That is valid (no capture-state data available yet),
    // not malformed, so it must map to `null` like an explicit null does --
    // NOT to `undefined` (which signals "present but invalid" and causes the
    // whole message to be rejected below). Backward-compat regression: see
    // issue #180 review.
    if (value === null || value === undefined) return null;
    if (typeof value !== 'object') return undefined;
    const rawState = value as Partial<CaptureStateReport>;
    if (!['live', 'idle', 'occluded', 'wedged'].includes(rawState.state ?? '')) return undefined;
    if (
      !validNullableNumber(rawState.fps) ||
      !validNullableNumber(rawState.dirtyRectCount) ||
      !validNullableNumber(rawState.dirtyAreaPx) ||
      !validNullableNumber(rawState.occlusionPct) ||
      !rawState.cpu ||
      typeof rawState.cpu !== 'object' ||
      !validNullableNumber(rawState.cpu.lockCopyMs) ||
      !validNullableNumber(rawState.cpu.convertMs) ||
      !validNullableNumber(rawState.cpu.captureFrameReturnMs)
    ) {
      return undefined;
    }
    return {
      state: rawState.state as CaptureStateReport['state'],
      fps: rawState.fps ?? null,
      dirtyRectCount: rawState.dirtyRectCount ?? null,
      dirtyAreaPx: rawState.dirtyAreaPx ?? null,
      occlusionPct: rawState.occlusionPct ?? null,
      cpu: {
        lockCopyMs: rawState.cpu.lockCopyMs ?? null,
        convertMs: rawState.cpu.convertMs ?? null,
        captureFrameReturnMs: rawState.cpu.captureFrameReturnMs ?? null,
      },
    };
  };
  const receiverFreeze = (value: unknown): ReceiverFreezeMetrics | null | undefined => {
    // Same backward-compat rule as captureState above: field absent (older
    // sender) must parse as null, not as a rejection of the whole message.
    if (value === null || value === undefined) return null;
    if (typeof value !== 'object') return undefined;
    const rawMetrics = value as Partial<ReceiverFreezeMetrics>;
    if (
      typeof rawMetrics.freezeCount !== 'number' ||
      !Number.isSafeInteger(rawMetrics.freezeCount) ||
      rawMetrics.freezeCount < 0 ||
      typeof rawMetrics.framesDropped !== 'number' ||
      !Number.isSafeInteger(rawMetrics.framesDropped) ||
      rawMetrics.framesDropped < 0 ||
      !(
        rawMetrics.qualityLimitationReason === null ||
        typeof rawMetrics.qualityLimitationReason === 'string'
      )
    ) {
      return undefined;
    }
    return {
      freezeCount: rawMetrics.freezeCount,
      framesDropped: rawMetrics.framesDropped,
      qualityLimitationReason: rawMetrics.qualityLimitationReason,
    };
  };

  const grabbed = stage(candidate.grabbed);
  const encodedSent = stage(candidate.encodedSent);
  const received = stage(candidate.received);
  const decoded = stage(candidate.decoded);
  const parsedCaptureState = captureState(candidate.captureState);
  const parsedReceiverFreeze = receiverFreeze(candidate.receiverFreeze);
  if (
    grabbed === undefined ||
    encodedSent === undefined ||
    received === undefined ||
    decoded === undefined ||
    parsedCaptureState === undefined ||
    parsedReceiverFreeze === undefined
  ) {
    return null;
  }

  return {
    v: 1,
    role: candidate.role,
    reporterId: candidate.reporterId.trim(),
    ownerIdentity: candidate.ownerIdentity.trim(),
    windowId: candidate.windowId,
    seq: candidate.seq,
    sentAtMs: candidate.sentAtMs,
    grabbed,
    encodedSent,
    received,
    decoded,
    captureState: parsedCaptureState,
    receiverFreeze: parsedReceiverFreeze,
    publicationSid,
    shareEpoch,
    lifecycle,
  };
}

/** Exact reducer identity: a replacement publication must never inherit a
 * previous publication's high-water mark or terminal tombstone. */
export function pipelineStatsCorrelationKey(message: PipelineStatsMessage, reporterIdentity: string): string {
  return `${message.ownerIdentity}:${message.windowId}:${reporterIdentity}:${message.publicationSid ?? 'legacy'}:${message.shareEpoch ?? 'legacy'}`;
}

/** A transient unsubscribe is not terminal: a reconnect may resubscribe the
 * same publication. Clear only its one-shot receiver transitions so the next
 * subscribe/decode/present observations are emitted again. */
export function resetLifecycleTransitions(
  delivered: Set<string>, ownerIdentity: string, windowId: number, publicationSid: string,
): void {
  const needle = `:${ownerIdentity}:${windowId}:${publicationSid}:`;
  for (const key of delivered) {
    if (key.includes(needle) && !key.endsWith(':unsubscribed')) delivered.delete(key);
  }
}

/** Bounded reducer bookkeeping; exported for deterministic unit coverage. */
export function prunePipelineReducerMaps(
  highWater: Map<string, { seq: number; receivedAt: number }>,
  terminals: Map<string, number>,
  now: number,
): void {
  for (const [key, value] of highWater) {
    if (now - value.receivedAt > PIPELINE_REDUCER_TTL_MS) highWater.delete(key);
  }
  for (const [key, value] of terminals) {
    if (now - value > PIPELINE_REDUCER_TTL_MS) terminals.delete(key);
  }
  while (highWater.size > MAX_PIPELINE_REDUCER_KEYS) highWater.delete(highWater.keys().next().value!);
  while (terminals.size > MAX_PIPELINE_REDUCER_KEYS) terminals.delete(terminals.keys().next().value!);
}

/** Bounded per-publication transition dedupe. Inactive publications cannot
 * retain opaque owner/SID correlation indefinitely. */
export function pruneStartupStatsCache(
  cache: Map<string, StartupStatsCacheEntry>,
  now: number,
): void {
  for (const [key, value] of cache) {
    if (now - value.updatedAt > STARTUP_STATS_TTL_MS) cache.delete(key);
  }
  while (cache.size > MAX_STARTUP_STATS_KEYS) cache.delete(cache.keys().next().value!);
}

function pushBounded<T>(items: T[], item: T) {
  items.push(item);
  if (items.length > MAX_RECORDED_MESSAGES) items.splice(0, items.length - MAX_RECORDED_MESSAGES);
}

export function setupPipelineStats(ctx: HarnessContext) {
  const { state, hook } = ctx;
  const sentMessages: PipelineStatsMessage[] = [];
  const receivedMessages: Array<{ message: PipelineStatsMessage; senderIdentity?: string; receivedAt: number }> = [];
  const outboundStates = new Map<string, CounterState>();
  const inboundStates = new Map<string, RemoteWindowStatsState>();
  const ownerEpochByPublication = new Map<string, string>();
  const stallStates = new Map<number, StallStatsState>();
  const deliveredLifecycle = new Set<string>();
  const receivedHighWater = new Map<string, { seq: number; receivedAt: number }>();
  const terminalEpochs = new Map<string, number>();
  const startupTimeline = new StartupTimelineRecorder();
  const lastStartupStats = new Map<string, StartupStatsCacheEntry>();
  let seq = 0;

  function nextSeq(): number {
    seq = seq >= Number.MAX_SAFE_INTEGER ? 1 : seq + 1;
    return seq;
  }

  function localPublicationSid(track: LocalVideoTrack): string | null {
    if (!state.room) return null;
    for (const publication of state.room.localParticipant.trackPublications.values()) {
      if (publication.track === track) return publication.trackSid ?? null;
    }
    return null;
  }

  function ownerEpoch(publicationSid: string): string {
    const existing = ownerEpochByPublication.get(publicationSid);
    if (existing) return existing;
    const epoch = `e${nextSeq().toString(36)}${Date.now().toString(36)}`;
    ownerEpochByPublication.set(publicationSid, epoch);
    return epoch;
  }

  function publishMessage(message: PipelineStatsMessage): Promise<void> {
    if (!state.room) return Promise.resolve();
    const destinationIdentity =
      message.role === 'receiver' && message.ownerIdentity !== message.reporterId
        ? message.ownerIdentity
        : null;
    const options = destinationIdentity
      ? { topic: PIPELINE_STATS_TOPIC, reliable: true, destinationIdentities: [destinationIdentity] }
      : { topic: PIPELINE_STATS_TOPIC, reliable: true };
    return state.room.localParticipant
      .publishData(encoder.encode(JSON.stringify(message)), options)
      .catch((err) => {
        console.debug(`pipeline stats publish failed: ${(err as Error).message ?? err}`);
      });
  }

  function lifecycleKey(message: PipelineStatsMessage): string {
    return `${message.role}:${message.ownerIdentity}:${message.windowId}:${message.publicationSid ?? ''}:${message.shareEpoch ?? ''}:${message.lifecycle ?? ''}`;
  }

  function epochKey(message: PipelineStatsMessage, reporterIdentity: string): string {
    return pipelineStatsCorrelationKey(message, reporterIdentity);
  }

  function emitReceiverLifecycle(
    ownerIdentity: string,
    windowId: number,
    publicationSid: string,
    lifecycle: PipelineLifecycle,
  ): void {
    if (!state.room || !ownerIdentity || !publicationSid || ownerIdentity === state.room.localParticipant.identity) return;
    const message: PipelineStatsMessage = {
      v: 1, role: 'receiver', reporterId: state.room.localParticipant.identity,
      ownerIdentity, windowId, seq: nextSeq(), sentAtMs: Date.now(),
      grabbed: null, encodedSent: null, received: null, decoded: null,
      captureState: null, receiverFreeze: null,
      // The owner generates the canonical epoch. Until its sender observation
      // arrives, use the publication SID as the stable shared correlation
      // anchor rather than minting an incompatible receiver-local epoch.
      publicationSid, shareEpoch: null, lifecycle,
    };
    const key = lifecycleKey(message);
    if (deliveredLifecycle.has(key)) return;
    deliveredLifecycle.add(key);
    void publishMessage(message);
    pushBounded(sentMessages, message);
  }

  async function collectLocalSenderMessage(
    track: LocalVideoTrack,
    windowId: number,
    sampledAtMs: number
  ): Promise<PipelineStatsMessage | null> {
    if (!state.room) return null;
    const reporterId = state.room.localParticipant.identity;
    const publicationSid = localPublicationSid(track);
    const grabbed = nonemptyStage(mediaTrackStage(track));
    const encodedSent = await encodedStageForTrack(track, `${reporterId}:${windowId}`, outboundStates, sampledAtMs);
    const captureState = captureStateForTrack(track, grabbed, encodedSent);
    if (!stageHasSignal(grabbed) && !stageHasSignal(encodedSent) && !captureStateHasSignal(captureState)) return null;
    return {
      v: 1,
      role: 'sender',
      reporterId,
      ownerIdentity: reporterId,
      windowId,
      seq: nextSeq(),
      sentAtMs: sampledAtMs,
      grabbed,
      encodedSent,
      received: null,
      decoded: null,
      captureState,
      receiverFreeze: null,
      publicationSid,
      shareEpoch: publicationSid ? ownerEpoch(publicationSid) : null,
      lifecycle: publicationSid ? 'published' : null,
    };
  }

  function remoteShareTracks(): InboundStatsTrack[] {
    if (!state.room) return [];
    const tracks: InboundStatsTrack[] = [];
    state.room.remoteParticipants.forEach((participant: RemoteParticipant) => {
      participant.trackPublications.forEach((publication: RemoteTrackPublication) => {
        const track = publication.track;
        const windowId = windowIdFromTrackName(publication.trackName);
        if (!track || track.kind !== Track.Kind.Video || windowId === null) return;
        tracks.push({ identity: participant.identity, publication, track });
      });
    });
    return tracks;
  }

  async function collectReceiverMessage(
    target: InboundStatsTrack,
    sampledAtMs: number
  ): Promise<PipelineStatsMessage | null> {
    if (!state.room) return null;
    const windowId = windowIdFromTrackName(target.publication.trackName);
    if (windowId === null) return null;
    let report: RTCStatsReport | null = null;
    try {
      report = await getRemoteStatsReport(target.track);
    } catch {
      report = null;
    }
    if (!report) return null;

    const key = `${target.identity}:${windowId}:${target.publication.trackSid}`;
    const derived = deriveRemoteWindowStats(report, inboundStates.get(key) ?? null, sampledAtMs);
    inboundStates.set(key, derived.state);
    updateStallStats(windowId, derived.snapshot.framesDecoded, sampledAtMs);
    noteVideoFrames(
      `${target.identity}:${windowId}:${target.publication.trackSid}`,
      derived.snapshot.framesDecoded,
      'stats',
      sampledAtMs
    );
    const received = nonemptyStage({
      width: derived.snapshot.width,
      height: derived.snapshot.height,
      fps: derived.snapshot.fps,
      kbps: derived.snapshot.bitrateBps === null ? null : derived.snapshot.bitrateBps / 1000,
    });
    const decoded = nonemptyStage({
      width: derived.snapshot.width,
      height: derived.snapshot.height,
      fps: derived.snapshot.fps,
      kbps: null,
    });
    const receiverFreeze: ReceiverFreezeMetrics = {
      freezeCount: derived.snapshot.freezeCount ?? 0,
      framesDropped: derived.snapshot.framesDropped ?? 0,
      qualityLimitationReason: derived.snapshot.qualityLimitationReason,
    };
    const senderEvidence = latestSenderEvidence(target.identity, windowId, target.publication.trackSid);
    const capturePath: CapturePath = senderEvidence?.captureState?.state === 'live'
      ? 'visible-raw'
      : senderEvidence?.captureState?.state === 'occluded'
        ? 'occluded-snapshot'
        : senderEvidence?.captureState?.state === 'idle'
          ? 'static-idle'
          : 'unknown';
    const startupStats = {
      decodedWidth: derived.snapshot.width,
      decodedHeight: derived.snapshot.height,
      decodedFps: derived.snapshot.fps,
      presentedFps: derived.snapshot.presentedFps,
      rid: actualInboundRid(report),
      capturePath,
      captureFps: senderEvidence?.captureState?.fps ?? senderEvidence?.grabbed?.fps ?? null,
      shareEpoch: senderEvidence?.shareEpoch ?? null,
    } as const;
    const startupStatsKey = `${target.identity}:${windowId}:${target.publication.trackSid}`;
    const startupStatsSignature = JSON.stringify(startupStats);
    pruneStartupStatsCache(lastStartupStats, sampledAtMs);
    if (lastStartupStats.get(startupStatsKey)?.signature !== startupStatsSignature) {
      lastStartupStats.set(startupStatsKey, { signature: startupStatsSignature, updatedAt: sampledAtMs });
      pruneStartupStatsCache(lastStartupStats, sampledAtMs);
      recordReceiverStats(target.identity, windowId, target.publication.trackSid, startupStats);
    } else {
      lastStartupStats.get(startupStatsKey)!.updatedAt = sampledAtMs;
    }
    if (!stageHasSignal(received) && !stageHasSignal(decoded) && !receiverFreezeHasSignal(receiverFreeze)) return null;
    return {
      v: 1,
      role: 'receiver',
      reporterId: state.room.localParticipant.identity,
      ownerIdentity: target.identity,
      windowId,
      seq: nextSeq(),
      sentAtMs: sampledAtMs,
      grabbed: null,
      encodedSent: null,
      received,
      decoded,
      captureState: null,
      receiverFreeze,
      publicationSid: target.publication.trackSid ?? null,
      shareEpoch: null,
      lifecycle: null,
    };
  }

  function latestSenderEvidence(
    ownerIdentity: string,
    windowId: number,
    publicationSid: string,
  ): PipelineStatsMessage | null {
    for (let index = receivedMessages.length - 1; index >= 0; index -= 1) {
      const message = receivedMessages[index].message;
      if (
        message.role === 'sender' &&
        message.ownerIdentity === ownerIdentity &&
        message.windowId === windowId &&
        message.publicationSid === publicationSid
      ) return message;
    }
    return null;
  }

  function recordReceiverStats(
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
  ): void {
    startupTimeline.record(
      { ownerIdentity, windowId, publicationSid, shareEpoch: stats.shareEpoch },
      'statsTransition',
      stats,
    );
  }

  function updateStallStats(windowId: number, framesDecoded: number | null, sampledAtMs: number) {
    if (framesDecoded === null) return;
    let state = stallStates.get(windowId);
    if (!state) {
      state = {
        framesSeen: 0,
        maxGapMs: 0,
        gapsOverThreshold: 0,
        lastFrameAtMs: null,
        lastFramesDecoded: null,
      };
      stallStates.set(windowId, state);
    }
    const previousFrames = state.lastFramesDecoded;
    let frameDelta = previousFrames === null ? framesDecoded : framesDecoded - previousFrames;
    if (frameDelta < 0) frameDelta = framesDecoded;
    if (frameDelta <= 0) {
      state.lastFramesDecoded = framesDecoded;
      return;
    }

    if (state.lastFrameAtMs !== null) {
      const gapMs = sampledAtMs - state.lastFrameAtMs;
      state.maxGapMs = Math.max(state.maxGapMs, gapMs);
      if (gapMs > STALL_GAP_THRESHOLD_MS) state.gapsOverThreshold += 1;
    }
    state.framesSeen += frameDelta;
    state.lastFrameAtMs = sampledAtMs;
    state.lastFramesDecoded = framesDecoded;
  }

  async function publishPipelineStats(): Promise<PipelineStatsMessage[]> {
    if (!state.room) return [];
    const sampledAtMs = Date.now();
    const messages: PipelineStatsMessage[] = [];
    if (state.sharing && state.localVideoTrack) {
      const message = await collectLocalSenderMessage(state.localVideoTrack, ctx.windowId, sampledAtMs);
      if (message) messages.push(message);
    }
    if (state.screenSharing && state.screenTrack && state.screenWindowId !== null) {
      const message = await collectLocalSenderMessage(state.screenTrack, state.screenWindowId, sampledAtMs);
      if (message) messages.push(message);
    }
    for (const target of remoteShareTracks()) {
      const message = await collectReceiverMessage(target, sampledAtMs);
      if (message) messages.push(message);
    }

    for (const message of messages) {
      await publishMessage(message);
      pushBounded(sentMessages, message);
    }
    return messages;
  }

  function startPipelineStats() {
    stopPipelineStats();
    state.pipelineStatsTimer = setInterval(() => {
      void publishPipelineStats();
    }, POLL_MS);
    void publishPipelineStats();
    // A reconnect can replay publications without firing TrackSubscribed in
    // the expected order. This is diagnostic reconciliation only; it never
    // changes subscription or publication state.
    for (const target of remoteShareTracks()) {
      const windowId = windowIdFromTrackName(target.publication.trackName);
      const sid = target.publication.trackSid;
      if (windowId !== null && sid) recordTrackSubscribed(target.identity, windowId, sid);
    }
  }

  function startupStatsKey(ownerIdentity: string, windowId: number, publicationSid: string): string {
    return `${ownerIdentity}:${windowId}:${publicationSid}`;
  }

  function recordTrackSubscribed(ownerIdentity: string, windowId: number, publicationSid: string): void {
    startupTimeline.record({ ownerIdentity, windowId, publicationSid }, 'trackSubscribed');
    emitReceiverLifecycle(ownerIdentity, windowId, publicationSid, 'subscribed');
  }

  function stopPipelineStats() {
    if (state.pipelineStatsTimer !== null) {
      clearInterval(state.pipelineStatsTimer);
      state.pipelineStatsTimer = null;
    }
    outboundStates.clear();
    inboundStates.clear();
    deliveredLifecycle.clear();
    receivedHighWater.clear();
    terminalEpochs.clear();
    lastStartupStats.clear();
  }

  function handlePipelineStatsPayload(payload: Uint8Array, senderIdentity?: string) {
    const message = parsePipelineStatsPayload(payload);
    if (!message || !state.room || !senderIdentity || senderIdentity === state.room.localParticipant.identity) return;
    if (message.reporterId !== senderIdentity) {
      message.reporterId = senderIdentity;
    }
    // Do not let the browser assert another side's state. Sender facts are
    // broadcast by the owner; receiver facts are direct replies to that owner.
    if (
      (message.role === 'sender' && message.ownerIdentity !== senderIdentity) ||
      (message.role === 'receiver' && (
        message.ownerIdentity !== state.room.localParticipant.identity ||
        message.ownerIdentity === senderIdentity
      ))
    ) return;
    const key = epochKey(message, senderIdentity);
    const receivedAt = Date.now();
    prunePipelineReducerMaps(receivedHighWater, terminalEpochs, receivedAt);
    if (terminalEpochs.has(key)) return;
    const previous = receivedHighWater.get(key);
    if (previous !== undefined && message.seq <= previous.seq) return;
    if (receivedHighWater.size >= MAX_PIPELINE_REDUCER_KEYS && !receivedHighWater.has(key)) {
      receivedHighWater.delete(receivedHighWater.keys().next().value!);
    }
    receivedHighWater.set(key, { seq: message.seq, receivedAt });
    if (message.lifecycle === 'unpublished' || message.lifecycle === 'terminalFailure') {
      if (message.publicationSid) {
        lastStartupStats.delete(startupStatsKey(message.ownerIdentity, message.windowId, message.publicationSid));
      }
      if (terminalEpochs.size >= MAX_PIPELINE_REDUCER_KEYS && !terminalEpochs.has(key)) {
        terminalEpochs.delete(terminalEpochs.keys().next().value!);
      }
      terminalEpochs.set(key, receivedAt);
      for (let i = receivedMessages.length - 1; i >= 0; i -= 1) {
        const item = receivedMessages[i];
        if (epochKey(item.message, item.senderIdentity ?? item.message.reporterId) === key) receivedMessages.splice(i, 1);
      }
      return;
    }
    pushBounded(receivedMessages, { message, senderIdentity, receivedAt: Date.now() });
  }

  const api = {
    metrics: () => ({
      sent: sentMessages.slice(),
      received: receivedMessages.slice(),
    }),
    resetMetrics: () => {
      sentMessages.length = 0;
      receivedMessages.length = 0;
      outboundStates.clear();
      inboundStates.clear();
      receivedHighWater.clear();
      terminalEpochs.clear();
      lastStartupStats.clear();
      startupTimeline.reset();
    },
    publish: publishPipelineStats,
    stallStats: (windowId: number): HarnessStallStats | null => {
      const state = stallStates.get(windowId);
      if (!state) return null;
      return {
        framesSeen: state.framesSeen,
        maxGapMs: state.maxGapMs,
        gapsOverThreshold: state.gapsOverThreshold,
      };
    },
    resetStallStats: (windowId: number) => {
      stallStates.delete(windowId);
    },
    startupTimeline: () => startupTimeline.snapshot(),
    trackPublished: (ownerIdentity: string, windowId: number, publicationSid: string) =>
      startupTimeline.record({ ownerIdentity, windowId, publicationSid }, 'trackPublished'),
    trackSubscribed: recordTrackSubscribed,
    trackFirstDecoded: (ownerIdentity: string, windowId: number, publicationSid: string) => {
      startupTimeline.record({ ownerIdentity, windowId, publicationSid }, 'firstDecoded');
      emitReceiverLifecycle(ownerIdentity, windowId, publicationSid, 'firstDecoded');
    },
    trackFirstPresented: (ownerIdentity: string, windowId: number, publicationSid: string) => {
      startupTimeline.record(
        { ownerIdentity, windowId, publicationSid },
        'firstPresented',
        { presentationSource: 'requestVideoFrameCallback' },
      );
      emitReceiverLifecycle(ownerIdentity, windowId, publicationSid, 'firstPresented');
    },
    trackUnsubscribed: (ownerIdentity: string, windowId: number, publicationSid: string) => {
      lastStartupStats.delete(startupStatsKey(ownerIdentity, windowId, publicationSid));
      startupTimeline.record({ ownerIdentity, windowId, publicationSid }, 'trackUnsubscribed');
      resetLifecycleTransitions(deliveredLifecycle, ownerIdentity, windowId, publicationSid);
      emitReceiverLifecycle(ownerIdentity, windowId, publicationSid, 'unsubscribed');
    },
    trackUnpublished: (ownerIdentity: string, windowId: number, publicationSid: string) => {
      lastStartupStats.delete(startupStatsKey(ownerIdentity, windowId, publicationSid));
      startupTimeline.record({ ownerIdentity, windowId, publicationSid }, 'trackUnpublished');
    },
    trackViewerDemand: (
      ownerIdentity: string,
      windowId: number,
      publicationSid: string,
      requestedSubscription: RequestedSubscription,
      demandWidth: number,
      demandHeight: number,
      requestedWidth: number,
      requestedHeight: number,
    ) => startupTimeline.record(
      { ownerIdentity, windowId, publicationSid },
      'viewerDemand',
      { requestedSubscription, demandWidth, demandHeight, requestedWidth, requestedHeight },
    ),
    trackReceiverStats: recordReceiverStats,
    resetSession: () => {
      ownerEpochByPublication.clear();
      startupTimeline.reset();
      lastStartupStats.clear();
    },
  };
  hook.pipelineStats = api;

  return {
    handlePipelineStatsPayload,
    startPipelineStats,
    stopPipelineStats,
    publishPipelineStats,
  };
}
