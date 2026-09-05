import type {
  JournalEntry,
  NetworkSnapshot,
  PipelineStageReport,
  PipelineStageMetrics,
  StatsSample,
  TrackHealth
} from '$lib/ipc';

export type {
  JournalEntry,
  NetworkSnapshot,
  PipelineStageReport,
  PipelineStageMetrics,
  StatsSample,
  TrackHealth
} from '$lib/ipc';

export type GaugeState = 'known' | 'estimated' | 'unknown';
export type GaugeTone = 'empty' | 'poor' | 'strained' | 'steady' | 'perfect';

export interface GaugeModel {
  id: string;
  label: string;
  score: number | null;
  value: string;
  detail: string;
  state: GaugeState;
  tone: GaugeTone;
}

export interface GaugeCockpitModel {
  overall: GaugeModel;
  dimensions: GaugeModel[];
}

export type PipelineNodeId = 'grabbed' | 'encodedSent' | 'received' | 'decoded' | 'displayEnqueued';
export type PipelineNodeState = 'measured' | 'waiting' | 'remote' | 'browser' | 'deferred';
export type PipelineRowSource = 'local' | 'remote' | 'browser' | 'legacy';
export type CaptureStateTone = 'live' | 'idle' | 'occluded' | 'wedged' | 'unknown';

export interface PipelineNodeModel {
  id: PipelineNodeId;
  label: string;
  value: string;
  detail: string;
  state: PipelineNodeState;
}

export interface PipelineRowModel {
  id: string;
  title: string;
  subtitle: string;
  source: PipelineRowSource;
  windowId: number | null;
  ownerIdentity: string | null;
  nodes: PipelineNodeModel[];
  displayEnqueued: Omit<PipelineNodeModel, 'id'> & { id: 'displayEnqueued' };
  captureState: CaptureStateModel;
  receiverFreeze: ReceiverFreezeModel;
}

export interface CaptureStateModel {
  tone: CaptureStateTone;
  label: string;
  detail: string;
  fps: string;
  occlusion: string;
  lockCopyMs: string;
  convertMs: string;
  captureFrameReturnMs: string;
}

export interface ReceiverFreezeModel {
  label: string;
  detail: string;
  freezeCount: string;
  framesDropped: string;
  qualityLimitationReason: string;
}

export const DIAGNOSTIC_THRESHOLDS = {
  highRttMs: 150,
  highJitterMs: 30,
  highLossPct: 2,
  highJitterBufferMs: 80,
  flappingReconnects: 2,
  mediaPipelineBudgetMs: 18
} as const;

export function fmt(v: number | null | undefined, digits = 0, unit = ''): string {
  if (v === null || v === undefined || Number.isNaN(v)) return '—';
  return v.toFixed(digits) + unit;
}

export function fmtKbps(v: number | null | undefined): string {
  if (v === null || v === undefined) return '—';
  return v >= 1000 ? (v / 1000).toFixed(2) + ' Mbps' : v.toFixed(0) + ' kbps';
}

function fmtFps(v: number | null | undefined): string {
  if (v === null || v === undefined || Number.isNaN(v)) return '—';
  const digits = Math.abs(v) > 0 && Math.abs(v) < 10 ? 1 : 0;
  return `${v.toFixed(digits)} fps`;
}

function fmtMs(v: number | null | undefined): string {
  if (v === null || v === undefined || Number.isNaN(v)) return '—';
  return `${v.toFixed(v < 10 ? 2 : 1)} ms`;
}

export function fmtTrackLatency(t: TrackHealth): string {
  if (t.glassToGlassMs !== null && t.glassToGlassMs !== undefined) {
    return `${t.glassToGlassMs.toFixed(0)} ms`;
  }
  if (t.glassToGlassEstimateMs !== null && t.glassToGlassEstimateMs !== undefined) {
    return `~${t.glassToGlassEstimateMs.toFixed(0)} ms`;
  }
  return '—';
}

export function fmtTrackFrames(t: TrackHealth): string {
  if (t.kind !== 'video') return '—';
  if (t.direction === 'send') {
    return `${t.framesEncoded} enc / ${t.keyFramesEncoded} key`;
  }
  if (t.direction === 'recv') {
    return `${t.framesDecoded} dec / ${t.keyFramesDecoded} key`;
  }
  return '—';
}

export function fmtTrackRtcp(t: TrackHealth): string {
  if (t.kind !== 'video') return '—';
  return `N ${t.nackCount} / P ${t.pliCount} / F ${t.firCount}`;
}

export function fmtTrackJitterBuffer(t: TrackHealth): string {
  if (t.direction !== 'recv') return '—';
  const actual = fmt(t.jitterBufferMs, 0, ' ms');
  if (actual === '—') return actual;
  if (t.jitterBufferTargetMs === null || t.jitterBufferTargetMs === undefined) return actual;
  const target = fmt(t.jitterBufferTargetMs, 0, ' target');
  const minimum =
    t.jitterBufferMinimumMs === null || t.jitterBufferMinimumMs === undefined
      ? null
      : fmt(t.jitterBufferMinimumMs, 0, ' min');
  return minimum ? `${actual} / ${target} / ${minimum}` : `${actual} / ${target}`;
}

export function fmtPct(v: number | null | undefined, digits = 0): string {
  if (v === null || v === undefined || Number.isNaN(v)) return '—';
  return v.toFixed(digits) + '%';
}

export function ts(tMs: number): string {
  const d = new Date(tMs);
  const p = (n: number, w = 2) => String(n).padStart(w, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

function isNumber(v: number | null | undefined): v is number {
  return typeof v === 'number' && Number.isFinite(v);
}

function clamp(v: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, v));
}

function lerp(a: number, b: number, t: number): number {
  return a + (b - a) * clamp(t, 0, 1);
}

function firstNumber(values: (number | null | undefined)[]): number | null {
  return values.find(isNumber) ?? null;
}

function maxPresent(values: (number | null | undefined)[]): number | null {
  const nums = values.filter(isNumber);
  return nums.length > 0 ? Math.max(...nums) : null;
}

function avgPresent(values: (number | null | undefined)[]): number | null {
  const nums = values.filter(isNumber);
  return nums.length > 0 ? nums.reduce((sum, v) => sum + v, 0) / nums.length : null;
}

function rawTrackName(t: TrackHealth): string {
  return t.rawTrackName ?? t.name.match(/^(.*) \([^)]+\)$/)?.[1] ?? t.name;
}

function parsedWindowId(t: TrackHealth): number | null {
  if (isNumber(t.windowId)) return t.windowId;
  const raw = rawTrackName(t);
  const suffix = raw.startsWith('petal-window-') ? raw.slice('petal-window-'.length) : '';
  const id = Number(suffix);
  return Number.isSafeInteger(id) && id > 0 ? id : null;
}

function isCameraTrack(t: TrackHealth): boolean {
  return rawTrackName(t).startsWith('petal-camera-');
}

function isLegacyWindowTrack(t: TrackHealth): boolean {
  return rawTrackName(t) === 'petal-window-capture';
}

function isPipelineWindowTrack(t: TrackHealth): boolean {
  if (t.kind !== 'video' || isCameraTrack(t)) return false;
  return parsedWindowId(t) !== null || isLegacyWindowTrack(t);
}

function isBrowserSender(identity: string | null): boolean {
  return /\b(web|browser|harness)\b/i.test(identity ?? '');
}

function stageValue(stage: PipelineStageMetrics): string {
  const hasResolution = isNumber(stage.width) && isNumber(stage.height);
  const resolution = hasResolution ? `${stage.width}×${stage.height}` : '—';
  return `${resolution} · ${fmtFps(stage.fps)}`;
}

function reportAge(receivedAtMs: number): string {
  const ageMs = Math.max(0, Date.now() - receivedAtMs);
  if (ageMs < 1500) return 'now';
  if (ageMs < 60_000) return `${Math.round(ageMs / 1000)}s ago`;
  return `${Math.round(ageMs / 60_000)}m ago`;
}

function remoteDetail(report: PipelineStageReport, fallbackDetail: string): string {
  const bandwidth =
    report.metrics.kbps !== null && report.metrics.kbps !== undefined
      ? fmtKbps(report.metrics.kbps)
      : fallbackDetail;
  return `reported by ${report.reporterId} · ${reportAge(report.receivedAtMs)} · ${bandwidth}`;
}

function captureStateLabel(state: CaptureStateTone): string {
  if (state === 'live') return 'Live';
  if (state === 'idle') return 'Idle static';
  if (state === 'occluded') return 'Occluded';
  if (state === 'wedged') return 'Wedged';
  return 'Waiting';
}

function captureStateDetail(state: CaptureStateTone, reportedBy: string | null): string {
  const suffix = reportedBy ? ` · reported by ${reportedBy}` : '';
  if (state === 'live') return `content updating${suffix}`;
  if (state === 'idle') return `source is not drawing${suffix}`;
  if (state === 'occluded') return `source appears covered${suffix}`;
  if (state === 'wedged') return `capture stalled before restart${suffix}`;
  return 'waiting for sender capture report';
}

function captureStateModel(track: TrackHealth | undefined, remoteTrack: TrackHealth | undefined): CaptureStateModel {
  const remote = remoteTrack?.remoteCaptureState ?? null;
  const report = track?.captureState ?? remote?.state ?? null;
  const tone = (report?.state ?? 'unknown') as CaptureStateTone;
  return {
    tone,
    label: captureStateLabel(tone),
    detail: captureStateDetail(tone, remote?.reporterId ?? null),
    fps: fmtFps(report?.fps),
    occlusion: fmtPct(report?.occlusionPct),
    lockCopyMs: fmtMs(report?.cpu.lockCopyMs),
    convertMs: fmtMs(report?.cpu.convertMs),
    captureFrameReturnMs: fmtMs(report?.cpu.captureFrameReturnMs)
  };
}

function receiverFreezeModel(track: TrackHealth | undefined, remoteTrack: TrackHealth | undefined): ReceiverFreezeModel {
  const remote = remoteTrack?.remoteReceiverFreeze ?? null;
  const metrics = track?.receiverFreeze ?? remote?.metrics ?? null;
  const reporter = remote?.reporterId ? ` · reported by ${remote.reporterId}` : '';
  const lifecycle = track?.remoteLifecycle ?? remoteTrack?.remoteLifecycle ?? null;
  const lifecycleDetail = lifecycle
    ? ` · lifecycle ${lifecycle.lifecycle.replace(/([A-Z])/g, ' $1').toLowerCase()} from ${lifecycle.reporterId}`
    : '';
  if (!metrics) {
    return {
      label: 'Waiting',
      detail: `waiting for receiver freeze report${lifecycleDetail}`,
      freezeCount: '—',
      framesDropped: '—',
      qualityLimitationReason: '—'
    };
  }
  // qualityLimitationReason is sourced from the receiver's inbound-rtp stats,
  // which never carries this field (WebRTC spec only defines it on
  // outbound-rtp) -- it is always null, not "measured: no limitation".
  // 'none' is itself a real, distinct WebRTC value, so coercing null to it
  // would misreport unmeasured data as measured. Use '—' for unmeasured.
  const reason = metrics.qualityLimitationReason?.trim() || '—';
  return {
    label: metrics.freezeCount > 0 || metrics.framesDropped > 0 ? 'Receiver pressure' : 'Receiver steady',
    detail: `freeze and drop counters${reporter}${lifecycleDetail}`,
    freezeCount: String(metrics.freezeCount),
    framesDropped: String(metrics.framesDropped),
    qualityLimitationReason: reason
  };
}

function measuredNode(
  id: PipelineNodeId,
  label: string,
  stage: PipelineStageMetrics,
  fallbackDetail: string
): PipelineNodeModel {
  return {
    id,
    label,
    value: stageValue(stage),
    detail: stage.kbps !== null && stage.kbps !== undefined ? fmtKbps(stage.kbps) : fallbackDetail,
    state: 'measured'
  };
}

function remoteNode(
  id: PipelineNodeId,
  label: string,
  report: PipelineStageReport,
  fallbackDetail: string
): PipelineNodeModel {
  return {
    id,
    label,
    value: stageValue(report.metrics),
    detail: remoteDetail(report, fallbackDetail),
    state: 'remote'
  };
}

function absentNode(
  id: PipelineNodeId,
  label: string,
  state: PipelineNodeState,
  detail: string
): PipelineNodeModel {
  return {
    id,
    label,
    value: '—',
    detail,
    state
  };
}

function sendStageNode(
  track: TrackHealth | undefined,
  id: 'grabbed' | 'encodedSent',
  label: string,
  source: PipelineRowSource,
  fallbackDetail: string
): PipelineNodeModel {
  const stage = id === 'grabbed' ? track?.grabbed : track?.encodedSent;
  if (stage) return measuredNode(id, label, stage, fallbackDetail);
  const report = id === 'grabbed' ? track?.remoteGrabbed : track?.remoteEncodedSent;
  if (report) return remoteNode(id, label, report, fallbackDetail);
  if (source === 'remote' || source === 'browser') {
    return absentNode(id, label, 'waiting', 'Waiting for sender report');
  }
  if (source === 'legacy') return absentNode(id, label, 'waiting', 'Legacy name lacks id');
  return absentNode(id, label, 'waiting', 'Waiting for stats');
}

function recvStageNode(
  track: TrackHealth | undefined,
  id: 'received' | 'decoded',
  label: string,
  source: PipelineRowSource,
  fallbackDetail: string
): PipelineNodeModel {
  const stage = id === 'received' ? track?.received : track?.decoded;
  if (stage) return measuredNode(id, label, stage, fallbackDetail);
  const report = id === 'received' ? track?.remoteReceived : track?.remoteDecoded;
  if (report) return remoteNode(id, label, report, fallbackDetail);
  if (source === 'local') return absentNode(id, label, 'waiting', 'Waiting for viewer report');
  if (source === 'legacy') return absentNode(id, label, 'waiting', 'Legacy name lacks id');
  return absentNode(id, label, 'waiting', 'Waiting for stats');
}

function displayEnqueuedNode(
  track: TrackHealth | undefined,
  source: PipelineRowSource
): Omit<PipelineNodeModel, 'id'> & { id: 'displayEnqueued' } {
  if (track?.displayEnqueued) {
    return measuredNode(
      'displayEnqueued',
      'Enqueued to display',
      track.displayEnqueued,
      'display layer enqueue'
    ) as Omit<PipelineNodeModel, 'id'> & { id: 'displayEnqueued' };
  }
  if (source === 'local' || source === 'legacy') {
    return absentNode('displayEnqueued', 'Enqueued to display', 'remote', 'Deferred #160') as Omit<
      PipelineNodeModel,
      'id'
    > & { id: 'displayEnqueued' };
  }
  return absentNode('displayEnqueued', 'Enqueued to display', 'waiting', 'Waiting for enqueue') as Omit<
    PipelineNodeModel,
    'id'
  > & { id: 'displayEnqueued' };
}

export function buildPipelineRows(tracks: TrackHealth[]): PipelineRowModel[] {
  const rows = new Map<
    string,
    {
      windowId: number | null;
      ownerIdentity: string | null;
      legacy: boolean;
      tracks: TrackHealth[];
    }
  >();

  for (const track of tracks) {
    if (!isPipelineWindowTrack(track)) continue;
    const windowId = parsedWindowId(track);
    const ownerIdentity = track.direction === 'recv' ? (track.ownerIdentity ?? null) : null;
    const legacy = isLegacyWindowTrack(track);
    const key = legacy
      ? `legacy:${track.direction}:${track.ownerIdentity ?? 'local'}:${track.sid}`
      : `${ownerIdentity ?? 'local'}:${windowId}`;
    const row =
      rows.get(key) ??
      ({
        windowId,
        ownerIdentity,
        legacy,
        tracks: []
      } satisfies {
        windowId: number | null;
        ownerIdentity: string | null;
        legacy: boolean;
        tracks: TrackHealth[];
      });
    row.tracks.push(track);
    rows.set(key, row);
  }

  return [...rows.entries()].map(([id, row]) => {
    const send = row.tracks.find((track) => track.direction === 'send');
    const recv = row.tracks.find((track) => track.direction === 'recv');
    const source: PipelineRowSource = row.legacy
      ? 'legacy'
      : send
        ? 'local'
        : isBrowserSender(row.ownerIdentity)
          ? 'browser'
          : 'remote';
    const owner = row.ownerIdentity;
    const title = row.legacy
      ? 'Legacy window share'
      : row.windowId !== null
        ? `Window ${row.windowId}`
        : 'Shared window';
    const subtitle =
      source === 'local'
        ? 'shared by you'
        : source === 'browser'
          ? `${owner ?? 'browser'} browser share`
          : source === 'legacy'
            ? 'unnamed legacy share'
            : `${owner ?? 'remote'} share`;

    return {
      id,
      title,
      subtitle,
      source,
      windowId: row.windowId,
      ownerIdentity: owner,
      nodes: [
        sendStageNode(send, 'grabbed', 'Grabbed', source, 'captured'),
        sendStageNode(send, 'encodedSent', 'Encoded/sent', source, 'encoded'),
        recvStageNode(recv, 'received', 'Received', source, 'inbound RTP'),
        recvStageNode(recv, 'decoded', 'Decoded', source, 'decoder')
      ],
      displayEnqueued: displayEnqueuedNode(recv, source),
      captureState: captureStateModel(send, recv),
      receiverFreeze: receiverFreezeModel(recv, send)
    };
  });
}

function recentAverage(
  history: StatsSample[],
  pick: (sample: StatsSample) => number | null | undefined,
  count = 20
): number | null {
  return avgPresent(history.slice(-count).map(pick));
}

export function sampleWindow(history: StatsSample[]): string {
  if (history.length < 2) return 'waiting for samples';
  const start = history[0]?.tMs ?? 0;
  const end = history.at(-1)?.tMs ?? start;
  const seconds = Math.max(1, Math.round((end - start) / 1000));
  return `${seconds}s window`;
}

function scoreLowMetric(value: number, perfect: number, warn: number, fail: number): number {
  if (value <= perfect) return 100;
  if (value >= fail) return 8;
  if (value <= warn) return lerp(100, 62, (value - perfect) / (warn - perfect || 1));
  return lerp(62, 18, (value - warn) / (fail - warn || 1));
}

function scoreHighRatio(ratio: number): number {
  if (ratio >= 1.15) return 100;
  if (ratio >= 1) return lerp(86, 100, (ratio - 1) / 0.15);
  if (ratio >= 0.75) return lerp(62, 86, (ratio - 0.75) / 0.25);
  if (ratio >= 0.45) return lerp(34, 62, (ratio - 0.45) / 0.3);
  return lerp(10, 34, ratio / 0.45);
}

function scoreSystemLoad(value: number): number {
  if (value <= 45) return 100;
  if (value <= 75) return lerp(100, 62, (value - 45) / 30);
  if (value <= 95) return lerp(62, 18, (value - 75) / 20);
  return 8;
}

function scoreThermal(state: string | null | undefined): number | null {
  const normalized = state?.toLowerCase().trim();
  if (!normalized) return null;
  if (['nominal', 'normal', 'none'].includes(normalized)) return 100;
  if (['fair', 'moderate', 'warm'].includes(normalized)) return 72;
  if (['serious', 'heavy', 'hot'].includes(normalized)) return 36;
  if (['critical', 'throttled'].includes(normalized)) return 12;
  return null;
}

export function stateLabel(state: GaugeState): string {
  if (state === 'known') return 'live';
  if (state === 'estimated') return 'est.';
  return 'unknown';
}

function toneForScore(score: number | null): GaugeTone {
  if (score === null) return 'empty';
  if (score < 42) return 'poor';
  if (score < 66) return 'strained';
  if (score < 88) return 'steady';
  return 'perfect';
}

function healthWord(score: number | null): string {
  if (score === null) return 'unknown';
  if (score < 42) return 'poor';
  if (score < 66) return 'strained';
  if (score < 88) return 'good';
  return 'perfect';
}

function metricGauge(
  id: string,
  label: string,
  score: number | null,
  value: string,
  detail: string,
  state: GaugeState
): GaugeModel {
  const rounded = score === null ? null : Math.round(clamp(score, 0, 100));
  return {
    id,
    label,
    score: rounded,
    value,
    detail,
    state: rounded === null ? 'unknown' : state,
    tone: toneForScore(rounded)
  };
}

function unknownGauge(id: string, label: string, detail: string): GaugeModel {
  return metricGauge(id, label, null, '—', detail, 'unknown');
}

function latencyGauge(model: NetworkSnapshot): GaugeModel {
  const latestSample = model.history.at(-1);
  const measured = maxPresent([
    model.glassToGlassMs,
    latestSample?.glassToGlassMs,
    ...model.tracks.map((t) => t.glassToGlassMs)
  ]);
  if (measured !== null) {
    return metricGauge(
      'latency',
      'Latency',
      scoreLowMetric(measured, 45, DIAGNOSTIC_THRESHOLDS.highRttMs, 320),
      `${measured.toFixed(0)} ms`,
      'glass-to-glass',
      'known'
    );
  }

  const providedEstimate = maxPresent([
    model.glassToGlassEstimateMs,
    latestSample?.glassToGlassEstimateMs,
    ...model.tracks.map((t) => t.glassToGlassEstimateMs)
  ]);
  if (providedEstimate !== null) {
    return metricGauge(
      'latency',
      'Latency',
      scoreLowMetric(providedEstimate, 45, DIAGNOSTIC_THRESHOLDS.highRttMs, 320),
      `~${providedEstimate.toFixed(0)} ms`,
      'glass-to-glass estimate',
      'estimated'
    );
  }

  const rtt = recentAverage(model.history, (s) => s.rttMs);
  if (rtt === null) return unknownGauge('latency', 'Latency', 'waiting for RTT');

  const jitterBuffer = maxPresent(model.tracks.map((t) => t.jitterBufferMs)) ?? 0;
  const estimate = rtt / 2 + jitterBuffer + DIAGNOSTIC_THRESHOLDS.mediaPipelineBudgetMs;
  const estimatedWarnMs =
    DIAGNOSTIC_THRESHOLDS.highRttMs / 2 +
    DIAGNOSTIC_THRESHOLDS.highJitterBufferMs +
    DIAGNOSTIC_THRESHOLDS.mediaPipelineBudgetMs;
  return metricGauge(
    'latency',
    'Latency',
    scoreLowMetric(estimate, 45, estimatedWarnMs, 320),
    `~${estimate.toFixed(0)} ms`,
    jitterBuffer > 0 ? 'RTT/2 + receive buffer' : 'RTT/2 + media budget',
    'estimated'
  );
}

function jitterGauge(model: NetworkSnapshot): GaugeModel {
  const jitter = recentAverage(model.history, (s) => s.jitterMs);
  if (jitter === null) return unknownGauge('jitter', 'Jitter', 'waiting for media stats');
  return metricGauge(
    'jitter',
    'Jitter',
    scoreLowMetric(jitter, 3, DIAGNOSTIC_THRESHOLDS.highJitterMs, 80),
    `${jitter.toFixed(1)} ms`,
    'recent average',
    'known'
  );
}

function lossGauge(model: NetworkSnapshot): GaugeModel {
  const loss = recentAverage(model.history, (s) => s.lossPct);
  if (loss === null) return unknownGauge('loss', 'Packet loss', 'waiting for RTCP loss');
  return metricGauge(
    'loss',
    'Packet loss',
    scoreLowMetric(loss, 0.05, DIAGNOSTIC_THRESHOLDS.highLossPct, 8),
    `${loss.toFixed(loss < 1 ? 2 : 1)}%`,
    'recent worst path',
    'known'
  );
}

function bandwidthGauge(model: NetworkSnapshot): GaugeModel {
  const sendVideo = model.tracks.filter(
    (t) => t.direction === 'send' && t.kind === 'video' && t.targetKbps > 0
  );
  const bandwidthLimited = model.tracks.some((t) => t.qualityLimitation === 'bandwidth');
  if (sendVideo.length > 0) {
    const worstRatio = Math.min(...sendVideo.map((t) => t.actualKbps / t.targetKbps));
    const score = bandwidthLimited ? Math.min(scoreHighRatio(worstRatio), 45) : scoreHighRatio(worstRatio);
    return metricGauge(
      'bandwidth',
      'Bandwidth',
      score,
      fmtPct(worstRatio * 100),
      bandwidthLimited ? 'encoder bandwidth-limited' : 'actual / target send',
      'known'
    );
  }

  const latestSample = model.history.at(-1);
  const available = firstNumber([
    model.availableOutgoingKbps,
    latestSample?.availableOutgoingKbps,
    ...model.tracks.map((t) => t.availableKbps)
  ]);
  if (available !== null) {
    const activeSend = latestSample?.sendKbps ?? 0;
    const ratio = activeSend > 0 ? available / activeSend : available / 4000;
    return metricGauge(
      'bandwidth',
      'Bandwidth',
      scoreHighRatio(ratio),
      fmtKbps(available),
      'available outgoing',
      'known'
    );
  }

  if (model.connected && latestSample && latestSample.sendKbps + latestSample.recvKbps > 0) {
    return metricGauge(
      'bandwidth',
      'Bandwidth',
      70,
      fmtKbps(latestSample.sendKbps + latestSample.recvKbps),
      'throughput observed',
      'estimated'
    );
  }

  return unknownGauge('bandwidth', 'Bandwidth', 'no active media');
}

function systemGauge(model: NetworkSnapshot): GaugeModel {
  const latestSample = model.history.at(-1);
  const cpu = firstNumber([model.system?.cpuPct, latestSample?.cpuPct]);
  const memory = firstNumber([model.system?.memoryPct, latestSample?.memoryPct]);
  const thermal = model.system?.thermalState ?? model.system?.thermalPressure ?? latestSample?.thermalState;
  const thermalScoreValue = scoreThermal(thermal);
  const scores = [
    cpu === null ? null : scoreSystemLoad(cpu),
    memory === null ? null : scoreSystemLoad(memory),
    thermalScoreValue
  ].filter(isNumber);

  if (scores.length > 0) {
    const score = Math.min(...scores);
    const value = cpu !== null ? `CPU ${cpu.toFixed(0)}%` : thermal ? thermal : fmtPct(memory);
    const detail = [
      memory !== null ? `mem ${memory.toFixed(0)}%` : null,
      thermal ? `thermal ${thermal}` : null
    ]
      .filter(Boolean)
      .join(' · ');
    return metricGauge('system', 'System', score, value, detail || 'system pressure', 'known');
  }

  if (model.tracks.some((t) => t.softwareEncoder)) {
    return metricGauge('system', 'System', 30, 'Software encoder', 'hardware encoder unavailable', 'known');
  }
  if (model.tracks.some((t) => t.qualityLimitation === 'cpu')) {
    return metricGauge('system', 'System', 34, 'CPU-limited', 'encoder limitation', 'known');
  }
  if (model.tracks.some((t) => t.qualityLimitation === 'other')) {
    return metricGauge('system', 'System', 52, 'Limited', 'media pipeline limitation', 'known');
  }
  if (model.connected && model.tracks.length > 0) {
    return metricGauge('system', 'System', 82, 'No limiter', 'system signals pending', 'estimated');
  }
  return unknownGauge('system', 'System', 'waiting for system signals');
}

export function buildGaugeCockpit(
  model: NetworkSnapshot,
  hasLiveBackend: boolean
): GaugeCockpitModel {
  const dimensions = [
    latencyGauge(model),
    jitterGauge(model),
    lossGauge(model),
    bandwidthGauge(model),
    systemGauge(model)
  ];
  const scored = dimensions.filter((g) => g.score !== null);
  let score: number | null = null;
  let detail = hasLiveBackend ? 'join a room to sample' : 'no live backend';
  let state: GaugeState = 'unknown';

  if (model.connected && scored.length > 0) {
    score = scored.reduce((sum, g) => sum + (g.score ?? 0), 0) / scored.length;
    state = scored.some((g) => g.state === 'estimated') ? 'estimated' : 'known';
    detail = healthWord(score);

    if (model.reconnectCount >= DIAGNOSTIC_THRESHOLDS.flappingReconnects) score = Math.min(score, 68);
    if (model.quality.some((q) => q.quality === 'poor')) score = Math.min(score, 58);
    if (model.quality.some((q) => q.quality === 'lost')) score = Math.min(score, 28);
    if (model.analysis.some((f) => f.severity === 'warn')) score = Math.min(score, 72);
    detail = healthWord(score);
  } else if (model.connected && model.analysis.some((f) => f.severity === 'warn')) {
    score = 58;
    state = 'estimated';
    detail = 'findings present';
  }

  const rounded = score === null ? null : Math.round(clamp(score, 0, 100));
  return {
    overall: {
      id: 'overall',
      label: 'Overall',
      score: rounded,
      value: rounded === null ? '—' : String(rounded),
      detail,
      state: rounded === null ? 'unknown' : state,
      tone: toneForScore(rounded)
    },
    dimensions
  };
}

// ─── Gauge history graphs (issue: replace semicircular gauges with trend
//     graphs) ────────────────────────────────────────────────────────────
//
// Each gauge becomes a line graph of that dimension's HEALTH SCORE (0–100,
// higher = better) over the sample window, drawn on a red→amber→green
// background. Plotting the score (not the raw metric) means every graph reads
// the same way — the line rising toward the green top is always "better",
// whether the underlying metric is latency (lower better) or bandwidth
// headroom (higher better). The per-sample scorers below mirror the aggregate
// gauge builders exactly (same thresholds), so the latest point of each line
// matches its gauge's current score.
//
// The two horizontal zone boundaries live at the same scores as
// `toneForScore`: 66 (steady/strained) and 42 (strained/poor). In graph
// space (y = 100 − score) that's y = 34 and y = 58.
export const GAUGE_ZONE_LINES = [34, 58] as const;

const LATENCY_PERFECT_MS = 45;
const LATENCY_FAIL_MS = 320;

function sampleLatencyScore(s: StatsSample, tracks: TrackHealth[]): number | null {
  if (isNumber(s.glassToGlassMs)) {
    return scoreLowMetric(s.glassToGlassMs, LATENCY_PERFECT_MS, DIAGNOSTIC_THRESHOLDS.highRttMs, LATENCY_FAIL_MS);
  }
  if (isNumber(s.glassToGlassEstimateMs)) {
    return scoreLowMetric(s.glassToGlassEstimateMs, LATENCY_PERFECT_MS, DIAGNOSTIC_THRESHOLDS.highRttMs, LATENCY_FAIL_MS);
  }
  if (isNumber(s.rttMs)) {
    const jitterBuffer = maxPresent(tracks.map((t) => t.jitterBufferMs)) ?? 0;
    const estimate = s.rttMs / 2 + jitterBuffer + DIAGNOSTIC_THRESHOLDS.mediaPipelineBudgetMs;
    const warn =
      DIAGNOSTIC_THRESHOLDS.highRttMs / 2 +
      DIAGNOSTIC_THRESHOLDS.highJitterBufferMs +
      DIAGNOSTIC_THRESHOLDS.mediaPipelineBudgetMs;
    return scoreLowMetric(estimate, LATENCY_PERFECT_MS, warn, LATENCY_FAIL_MS);
  }
  return null;
}

function sampleJitterScore(s: StatsSample): number | null {
  return isNumber(s.jitterMs) ? scoreLowMetric(s.jitterMs, 3, DIAGNOSTIC_THRESHOLDS.highJitterMs, 80) : null;
}

function sampleLossScore(s: StatsSample): number | null {
  return isNumber(s.lossPct) ? scoreLowMetric(s.lossPct, 0.05, DIAGNOSTIC_THRESHOLDS.highLossPct, 8) : null;
}

function sampleBandwidthScore(s: StatsSample): number | null {
  if (isNumber(s.availableOutgoingKbps)) {
    const ratio = s.sendKbps > 0 ? s.availableOutgoingKbps / s.sendKbps : s.availableOutgoingKbps / 4000;
    return scoreHighRatio(ratio);
  }
  return null;
}

function sampleSystemScore(s: StatsSample): number | null {
  const parts = [
    isNumber(s.cpuPct) ? scoreSystemLoad(s.cpuPct) : null,
    isNumber(s.memoryPct) ? scoreSystemLoad(s.memoryPct) : null,
    scoreThermal(s.thermalState)
  ].filter(isNumber);
  return parts.length > 0 ? Math.min(...parts) : null;
}

/**
 * Per-sample health-score series (0–100, higher = better) for every gauge,
 * aligned index-for-index to `model.history`. Absent inputs yield `null`,
 * which the renderer bridges. Keyed by the matching `GaugeModel.id`.
 */
export function buildGaugeSeries(model: NetworkSnapshot): Record<string, (number | null)[]> {
  const history = model.history;
  const latency = history.map((s) => sampleLatencyScore(s, model.tracks));
  const jitter = history.map(sampleJitterScore);
  const loss = history.map(sampleLossScore);
  const bandwidth = history.map(sampleBandwidthScore);
  const system = history.map(sampleSystemScore);
  const overall = history.map((_, i) => {
    const parts = [latency[i], jitter[i], loss[i], bandwidth[i], system[i]].filter(isNumber);
    return parts.length > 0 ? parts.reduce((sum, v) => sum + v, 0) / parts.length : null;
  });
  return { latency, jitter, loss, bandwidth, system, overall };
}

interface GraphPoint {
  x: number;
  y: number;
}

function seriesPoints(scores: (number | null)[], w: number, h: number): GraphPoint[] {
  const n = scores.length;
  const pts: GraphPoint[] = [];
  scores.forEach((score, i) => {
    if (!isNumber(score)) return;
    const x = n > 1 ? (i / (n - 1)) * w : w / 2;
    const y = h - (clamp(score, 0, 100) / 100) * h;
    pts.push({ x, y });
  });
  return pts;
}

// Catmull-Rom → cubic-bézier smoothing: a single continuous curve through
// every present point, no external charting dependency. Nulls are bridged
// (the series keeps each point at its real time index, so a gap just draws a
// longer smooth span). Rendered with a 0..100 viewBox + non-scaling stroke so
// it stretches to any card width while the line stays crisp.
function catmullRomPath(pts: GraphPoint[]): string {
  const f = (v: number) => v.toFixed(2);
  if (pts.length === 1) return `M0 ${f(pts[0].y)}L100 ${f(pts[0].y)}`;
  let d = `M${f(pts[0].x)} ${f(pts[0].y)}`;
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[i - 1] ?? pts[i];
    const p1 = pts[i];
    const p2 = pts[i + 1];
    const p3 = pts[i + 2] ?? p2;
    const c1x = p1.x + (p2.x - p0.x) / 6;
    const c1y = p1.y + (p2.y - p0.y) / 6;
    const c2x = p2.x - (p3.x - p1.x) / 6;
    const c2y = p2.y - (p3.y - p1.y) / 6;
    d += `C${f(c1x)} ${f(c1y)},${f(c2x)} ${f(c2y)},${f(p2.x)} ${f(p2.y)}`;
  }
  return d;
}

/** Smooth line through the score series in a 0..w × 0..h box (empty if no points). */
export function smoothLinePath(scores: (number | null)[], w = 100, h = 100): string {
  const pts = seriesPoints(scores, w, h);
  return pts.length === 0 ? '' : catmullRomPath(pts);
}

/** The line closed down to the baseline, for a soft area fill under it. */
export function smoothAreaPath(scores: (number | null)[], w = 100, h = 100): string {
  const pts = seriesPoints(scores, w, h);
  if (pts.length === 0) return '';
  const line = catmullRomPath(pts);
  const last = pts[pts.length - 1];
  const first = pts[0];
  return `${line}L${last.x.toFixed(2)} ${h}L${first.x.toFixed(2)} ${h}Z`;
}

export function sparkPath(values: (number | null)[], w = 150, h = 30): string {
  const nums = values.filter((v): v is number => v !== null && !Number.isNaN(v));
  if (nums.length < 2) return '';
  const min = Math.min(...nums);
  const max = Math.max(...nums);
  const span = max - min || 1;
  const n = values.length;
  let d = '';
  let pen = false;
  values.forEach((v, i) => {
    if (v === null || Number.isNaN(v)) {
      pen = false;
      return;
    }
    const x = n > 1 ? (i / (n - 1)) * w : 0;
    const y = h - 2 - ((v - min) / span) * (h - 4);
    d += `${pen ? 'L' : 'M'}${x.toFixed(1)} ${y.toFixed(1)}`;
    pen = true;
  });
  return d;
}
