import type { HarnessContext } from './context.ts';
import type {
  CaptureStateReport,
  PipelineStatsMessage,
  ReceiverFreezeMetrics,
} from './trackNames.ts';
import type { StartupTimelineSnapshot } from './startupTimeline.ts';

export interface NetworkDiagnosticsMetrics {
  sent: PipelineStatsMessage[];
  received: Array<{
    message: PipelineStatsMessage;
    senderIdentity?: string;
    receivedAt: number;
  }>;
}

export interface NetworkDiagnosticsRow {
  id: string;
  title: string;
  subtitle: string;
  captureLabel: string;
  captureDetail: string;
  captureTone: CaptureStateReport['state'] | 'unknown';
  fps: string;
  occlusion: string;
  lockCopyMs: string;
  convertMs: string;
  captureFrameReturnMs: string;
  freezeCount: string;
  framesDropped: string;
  qualityLimitationReason: string;
  lifecycle: string;
  startupCause: string;
  firstPresented: string;
  requestedSubscription: string;
  decodedPresentation: string;
  rid: string;
  clockUncertainty: string;
}

interface DraftRow {
  ownerIdentity: string;
  windowId: number;
  publicationSid: string | null;
  shareEpoch: string | null;
  localSender: boolean;
  localReceiver: boolean;
  captureState: CaptureStateReport | null;
  captureReporter: string | null;
  receiverFreeze: ReceiverFreezeMetrics | null;
  receiverReporter: string | null;
  lifecycle: string | null;
}

const RENDER_MS = 1000;

function fmtFps(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return 'n/a';
  return `${value.toFixed(value > 0 && value < 10 ? 1 : 0)} fps`;
}

function fmtPct(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return 'n/a';
  return `${value.toFixed(0)}%`;
}

function fmtMs(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return 'n/a';
  return `${value.toFixed(value < 10 ? 2 : 1)} ms`;
}

function captureLabel(state: CaptureStateReport['state'] | 'unknown'): string {
  if (state === 'live') return 'Live';
  if (state === 'idle') return 'Idle static';
  if (state === 'occluded') return 'Occluded';
  if (state === 'wedged') return 'Wedged';
  return 'Waiting';
}

function captureDetail(state: CaptureStateReport['state'] | 'unknown', reporter: string | null): string {
  const suffix = reporter ? `, reported by ${reporter}` : '';
  if (state === 'live') return `content updating${suffix}`;
  if (state === 'idle') return `source is not drawing${suffix}`;
  if (state === 'occluded') return `source appears covered${suffix}`;
  if (state === 'wedged') return `capture stalled before restart${suffix}`;
  return 'waiting for sender capture report';
}

function rowKey(ownerIdentity: string, windowId: number, publicationSid?: string | null): string {
  // Owner-published epoch is not known when the receiver first subscribes.
  // The publication SID is the shared, authoritative identity until then.
  return `${ownerIdentity}:${windowId}:${publicationSid ?? 'legacy'}`;
}

function ensureDraft(rows: Map<string, DraftRow>, message: PipelineStatsMessage): DraftRow {
  const { ownerIdentity, windowId } = message;
  const id = rowKey(ownerIdentity, windowId, message.publicationSid);
  const existing = rows.get(id);
  if (existing) return existing;
  const row: DraftRow = {
    ownerIdentity,
    windowId,
    publicationSid: message.publicationSid ?? null,
    shareEpoch: message.shareEpoch ?? null,
    localSender: false,
    localReceiver: false,
    captureState: null,
    captureReporter: null,
    receiverFreeze: null,
    receiverReporter: null,
    lifecycle: null,
  };
  rows.set(id, row);
  return row;
}

export function buildNetworkDiagnosticsRows(
  metrics: NetworkDiagnosticsMetrics,
  localIdentity?: string | null,
  startupTimelines: StartupTimelineSnapshot[] = [],
): NetworkDiagnosticsRow[] {
  const rows = new Map<string, DraftRow>();
  const applyMessage = (message: PipelineStatsMessage, receivedFrom: string | null) => {
    const row = ensureDraft(rows, message);
    if (message.lifecycle) row.lifecycle = message.lifecycle;
    if (message.role === 'sender') {
      row.localSender ||= message.reporterId === localIdentity;
      if (message.captureState) {
        row.captureState = message.captureState;
        row.captureReporter = receivedFrom;
      }
    } else {
      row.localReceiver ||= message.reporterId === localIdentity;
      if (message.receiverFreeze) {
        row.receiverFreeze = message.receiverFreeze;
        row.receiverReporter = receivedFrom;
      }
    }
  };

  for (const message of metrics.sent) applyMessage(message, null);
  for (const item of metrics.received) applyMessage(item.message, item.senderIdentity ?? item.message.reporterId);

  return [...rows.values()]
    .sort((a, b) => a.ownerIdentity.localeCompare(b.ownerIdentity) || a.windowId - b.windowId)
    .map((row) => {
      const state = row.captureState?.state ?? 'unknown';
      const freeze = row.receiverFreeze;
      const startup = startupTimelines.find((timeline) =>
        timeline.correlation.windowId === row.windowId &&
        timeline.correlation.publicationSid === row.publicationSid
      );
      const demand = startup ? [...startup.events].reverse().find((event) => event.kind === 'viewerDemand') : undefined;
      const presentation = startup ? [...startup.events].reverse().find((event) => event.kind === 'firstPresented') : undefined;
      const decoded = startup ? [...startup.events].reverse().find((event) => event.kind === 'statsTransition') : undefined;
      const relation = row.localSender
        ? 'shared by you'
        : row.localReceiver
          ? `${row.ownerIdentity} share`
          : row.ownerIdentity === localIdentity
            ? 'shared by you'
            : `${row.ownerIdentity} share`;
      return {
        id: rowKey(row.ownerIdentity, row.windowId, row.publicationSid),
        title: `Window ${row.windowId}`,
        subtitle: relation,
        captureLabel: captureLabel(state),
        captureDetail: captureDetail(state, row.captureReporter),
        captureTone: state,
        fps: fmtFps(row.captureState?.fps),
        occlusion: fmtPct(row.captureState?.occlusionPct),
        lockCopyMs: fmtMs(row.captureState?.cpu.lockCopyMs),
        convertMs: fmtMs(row.captureState?.cpu.convertMs),
        captureFrameReturnMs: fmtMs(row.captureState?.cpu.captureFrameReturnMs),
        freezeCount: freeze ? String(freeze.freezeCount) : 'n/a',
        framesDropped: freeze ? String(freeze.framesDropped) : 'n/a',
        // See remoteWindowStats.ts's deriveRemoteWindowStats: this is sourced
        // from inbound-rtp stats, which never carries qualityLimitationReason
        // (WebRTC spec defines it only on outbound-rtp) -- always null, not
        // "measured: no limitation". 'none' is a real, distinct WebRTC value,
        // so coercing null to it would misreport unmeasured data as measured.
        qualityLimitationReason: freeze?.qualityLimitationReason?.trim() || '—',
        lifecycle: lifecycleLabel(row.lifecycle),
        startupCause: startup?.classification.cause ?? 'waiting for startup evidence',
        firstPresented: presentation
          ? `${presentation.elapsedMs.toFixed(0)} ms (estimated display, rVFC)`
          : 'waiting',
        requestedSubscription: demand
          ? `${demand.requestedSubscription ?? 'none'} ${demand.requestedWidth ?? 'n/a'}x${demand.requestedHeight ?? 'n/a'} (device ${demand.demandWidth ?? 'n/a'}x${demand.demandHeight ?? 'n/a'})`
          : 'waiting',
        decodedPresentation: decoded
          ? `${decoded.decodedWidth ?? 'n/a'}x${decoded.decodedHeight ?? 'n/a'} @ ${fmtFps(decoded.presentedFps ?? decoded.decodedFps)}`
          : 'waiting',
        rid: decoded?.rid ?? 'unavailable',
        clockUncertainty: startup
          ? 'receiver monotonic; cross-peer clocks uncalibrated'
          : 'waiting',
      };
    });
}

function lifecycleLabel(value: string | null): string {
  if (!value) return 'waiting for lifecycle evidence';
  return value.replace(/([A-Z])/g, ' $1').toLowerCase();
}

function metric(label: string, value: string): HTMLSpanElement {
  const item = document.createElement('span');
  const strong = document.createElement('b');
  item.textContent = `${label} `;
  strong.textContent = value;
  item.append(strong);
  return item;
}

function renderRows(container: HTMLElement, rows: NetworkDiagnosticsRow[]) {
  container.replaceChildren();
  if (rows.length === 0) {
    const empty = document.createElement('p');
    empty.className = 'network-empty';
    empty.textContent = 'No window-share stats yet.';
    container.append(empty);
    return;
  }

  for (const row of rows) {
    const article = document.createElement('article');
    article.className = `network-row state-${row.captureTone}`;

    const head = document.createElement('div');
    head.className = 'network-row-head';
    const title = document.createElement('strong');
    title.textContent = row.title;
    const subtitle = document.createElement('span');
    subtitle.textContent = row.subtitle;
    head.append(title, subtitle);

    const capture = document.createElement('div');
    capture.className = 'network-band';
    const captureMain = document.createElement('div');
    captureMain.className = 'network-band-main';
    const captureLabelEl = document.createElement('strong');
    captureLabelEl.textContent = row.captureLabel;
    const captureDetailEl = document.createElement('span');
    captureDetailEl.textContent = row.captureDetail;
    captureMain.append(captureLabelEl, captureDetailEl);
    const captureMetrics = document.createElement('div');
    captureMetrics.className = 'network-metrics';
    captureMetrics.append(
      metric('fps', row.fps),
      metric('occlusion', row.occlusion),
      metric('lock/copy', row.lockCopyMs),
      metric('convert', row.convertMs),
      metric('capture return', row.captureFrameReturnMs)
    );
    capture.append(captureMain, captureMetrics);

    const receiver = document.createElement('div');
    receiver.className = 'network-band';
    const receiverMain = document.createElement('div');
    receiverMain.className = 'network-band-main';
    const receiverLabel = document.createElement('strong');
    receiverLabel.textContent = 'Receiver';
    const receiverDetail = document.createElement('span');
    receiverDetail.textContent = row.lifecycle;
    receiverMain.append(receiverLabel, receiverDetail);
    const receiverMetrics = document.createElement('div');
    receiverMetrics.className = 'network-metrics';
    receiverMetrics.append(
      metric('freezes', row.freezeCount),
      metric('dropped', row.framesDropped),
      metric('limit', row.qualityLimitationReason)
    );
    receiver.append(receiverMain, receiverMetrics);

    const startup = document.createElement('div');
    startup.className = 'network-band';
    const startupMain = document.createElement('div');
    startupMain.className = 'network-band-main';
    const startupLabel = document.createElement('strong');
    startupLabel.textContent = 'Startup';
    const startupDetail = document.createElement('span');
    startupDetail.textContent = row.startupCause;
    startupMain.append(startupLabel, startupDetail);
    const startupMetrics = document.createElement('div');
    startupMetrics.className = 'network-metrics';
    startupMetrics.append(
      metric('first presented', row.firstPresented),
      metric('requested', row.requestedSubscription),
      metric('decoded', row.decodedPresentation),
      metric('RID', row.rid),
      metric('clock', row.clockUncertainty),
    );
    startup.append(startupMain, startupMetrics);

    article.append(head, capture, receiver, startup);
    container.append(article);
  }
}

export function shouldRenderNetworkDiagnostics(
  devPanel: { open: boolean } | null,
  networkPanel: { open: boolean } | null
): boolean {
  return Boolean(devPanel?.open && networkPanel?.open);
}

export function setupNetworkDiagnostics(ctx: HarnessContext) {
  const devPanel = document.querySelector<HTMLDetailsElement>('#dev-panel');
  const networkPanel = document.querySelector<HTMLDetailsElement>('#network-panel');
  let timer: number | null = null;

  const render = () => {
    const metrics = ctx.hook.pipelineStats?.metrics() ?? { sent: [], received: [] };
    const startup = ctx.hook.pipelineStats?.startupTimeline() ?? [];
    const rows = buildNetworkDiagnosticsRows(metrics, ctx.state.room?.localParticipant.identity ?? null, startup);
    renderRows(ctx.dom.networkDiagnosticsRows, rows);
  };

  const syncTimer = () => {
    const shouldRun = shouldRenderNetworkDiagnostics(devPanel, networkPanel);
    if (shouldRun) {
      render();
      if (timer === null) timer = window.setInterval(render, RENDER_MS);
      return;
    }
    if (timer !== null) {
      window.clearInterval(timer);
      timer = null;
    }
  };

  devPanel?.addEventListener('toggle', syncTimer);
  networkPanel?.addEventListener('toggle', syncTimer);
  syncTimer();
}
