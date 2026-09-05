export type StartupEventKind =
  | 'trackPublished'
  | 'trackSubscribed'
  | 'viewerDemand'
  | 'firstDecoded'
  | 'firstPresented'
  | 'statsTransition'
  | 'trackUnsubscribed'
  | 'trackUnpublished';

export type StartupCause =
  | 'healthy'
  | 'capture-first-frame-delay'
  | 'metadata-budget-delay'
  | 'published-but-unsubscribed'
  | 'quarter-bootstrap-layer-upgrade'
  | 'data-saver'
  | 'requested-low-layer'
  | 'source-throttling'
  | 'visible-raw-cadence-shortfall'
  | 'occluded-snapshot-backoff'
  | 'static-idle-source'
  | 'receiver-transport-unknown';

export type RequestedSubscription = 'high' | 'dimensions' | 'high-fallback' | 'none';
export type CapturePath = 'visible-raw' | 'occluded-snapshot' | 'static-idle' | 'unknown';

export interface StartupCorrelation {
  ownerIdentity: string;
  windowId: number;
  publicationSid: string;
  shareEpoch?: string | null;
}

export interface StartupEventObservation {
  kind: StartupEventKind;
  atMonotonicMs: number;
  elapsedMs: number;
  requestedSubscription?: RequestedSubscription;
  demandWidth?: number | null;
  demandHeight?: number | null;
  requestedWidth?: number | null;
  requestedHeight?: number | null;
  decodedWidth?: number | null;
  decodedHeight?: number | null;
  decodedFps?: number | null;
  presentedFps?: number | null;
  presentationSource?: 'requestVideoFrameCallback';
  rid?: string | 'unavailable';
  capturePath?: CapturePath;
  captureFps?: number | null;
}

export interface StartupTimelineSnapshot {
  correlation: {
    /** Ephemeral, room-local alias. Raw participant identities are never exported. */
    ownerAlias: string;
    windowId: number;
    publicationSid: string;
    shareEpoch: string | null;
  };
  clock: {
    basis: 'receiver-performance-now';
    crossPeerComparable: false;
    uncertainty: 'uncalibrated-cross-peer-clocks';
  };
  events: StartupEventObservation[];
  classification: StartupClassification;
}

export interface StartupClassificationInput {
  selectedMode?: 'automatic' | 'responsive' | 'sharpText' | 'dataSaver' | 'unknown';
  cadenceFloorFps?: number | null;
  firstRawElapsedMs?: number | null;
  firstRawTimeout?: boolean;
  metadataBudgetExpired?: boolean;
  publishedElapsedMs?: number | null;
  subscribedElapsedMs?: number | null;
  demandElapsedMs?: number | null;
  firstPresentedElapsedMs?: number | null;
  requestedSubscription?: RequestedSubscription;
  demandWidth?: number | null;
  demandHeight?: number | null;
  requestedWidth?: number | null;
  requestedHeight?: number | null;
  initialDecodedWidth?: number | null;
  initialDecodedHeight?: number | null;
  decodedWidth?: number | null;
  decodedHeight?: number | null;
  decodedFps?: number | null;
  presentedFps?: number | null;
  capturePath?: CapturePath;
  captureFps?: number | null;
  observationElapsedMs?: number | null;
}

export interface StartupClassification {
  cause: StartupCause;
  detail: string;
  /** The classifier never infers a cause that the available stages cannot prove. */
  evidenceComplete: boolean;
}

const DEFAULT_MAX_TIMELINES = 32;
const DEFAULT_MAX_EVENTS_PER_TIMELINE = 40;
const DEFAULT_TTL_MS = 10 * 60 * 1000;
const SUBSCRIPTION_TIMEOUT_MS = 2_000;
const LOW_FPS_RATIO = 0.8;

function positive(value: number | null | undefined): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : null;
}

function nonnegative(value: number | null | undefined): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null;
}

function covers(
  width: number | null | undefined,
  height: number | null | undefined,
  requestedWidth: number | null | undefined,
  requestedHeight: number | null | undefined,
): boolean {
  const w = positive(width);
  const h = positive(height);
  const rw = positive(requestedWidth);
  const rh = positive(requestedHeight);
  return w !== null && h !== null && rw !== null && rh !== null && w >= rw && h >= rh;
}

export function classifyStartup(input: StartupClassificationInput): StartupClassification {
  if (input.firstRawTimeout || (nonnegative(input.firstRawElapsedMs) ?? 0) >= 5_000) {
    return { cause: 'capture-first-frame-delay', detail: 'native first-frame evidence exceeded its startup budget', evidenceComplete: true };
  }
  if (input.metadataBudgetExpired) {
    return { cause: 'metadata-budget-delay', detail: 'native metadata publication exhausted its startup budget', evidenceComplete: true };
  }
  if (
    nonnegative(input.publishedElapsedMs) !== null &&
    nonnegative(input.subscribedElapsedMs) === null &&
    (nonnegative(input.observationElapsedMs) ?? 0) >= SUBSCRIPTION_TIMEOUT_MS
  ) {
    return { cause: 'published-but-unsubscribed', detail: 'publication was observed but subscription did not arrive within 2000ms', evidenceComplete: true };
  }
  if (input.selectedMode === 'dataSaver' || input.cadenceFloorFps === 15) {
    return { cause: 'data-saver', detail: 'the selected 15fps Data Saver contract intentionally cannot meet the interactive latency SLO', evidenceComplete: true };
  }
  if (input.capturePath === 'occluded-snapshot') {
    return { cause: 'occluded-snapshot-backoff', detail: 'sender reported the occluded snapshot fallback path', evidenceComplete: true };
  }
  if (input.capturePath === 'static-idle') {
    return { cause: 'static-idle-source', detail: 'sender reported a static or idle source rather than a moving low-fps source', evidenceComplete: true };
  }

  const demandWidth = input.demandWidth ?? input.requestedWidth;
  const demandHeight = input.demandHeight ?? input.requestedHeight;
  const initialBelowDemand =
    positive(input.initialDecodedWidth) !== null &&
    positive(input.initialDecodedHeight) !== null &&
    !covers(input.initialDecodedWidth, input.initialDecodedHeight, demandWidth, demandHeight);
  const currentCoversDemand = covers(input.decodedWidth, input.decodedHeight, demandWidth, demandHeight);
  if (
    initialBelowDemand &&
    currentCoversDemand &&
    nonnegative(input.subscribedElapsedMs) !== null &&
    nonnegative(input.demandElapsedMs) !== null &&
    input.demandElapsedMs! >= input.subscribedElapsedMs!
  ) {
    return { cause: 'quarter-bootstrap-layer-upgrade', detail: 'the receiver first decoded below demand and upgraded only after post-subscription demand', evidenceComplete: true };
  }
  if (
    input.requestedSubscription === 'dimensions' &&
    positive(input.requestedWidth) !== null && positive(input.requestedHeight) !== null &&
    positive(demandWidth) !== null && positive(demandHeight) !== null &&
    (input.requestedWidth! < demandWidth! || input.requestedHeight! < demandHeight!)
  ) {
    return { cause: 'requested-low-layer', detail: 'the selected dimension request was below the visible device-pixel demand', evidenceComplete: true };
  }

  const floor = positive(input.cadenceFloorFps) ?? 30;
  // Zero is a valid measured cadence (a visible subscribed stream with no
  // advancing frames), not missing evidence.
  const delivered = nonnegative(input.presentedFps) ?? nonnegative(input.decodedFps);
  const captureFps = nonnegative(input.captureFps);
  if (delivered !== null && delivered >= floor * LOW_FPS_RATIO) {
    return { cause: 'healthy', detail: `delivered cadence ${delivered.toFixed(1)}fps meets the ${floor}fps mode floor tolerance`, evidenceComplete: true };
  }
  if (input.capturePath === 'visible-raw' && captureFps !== null && captureFps < floor * LOW_FPS_RATIO) {
    return { cause: 'source-throttling', detail: `visible sender capture was already limited to ${captureFps.toFixed(1)}fps`, evidenceComplete: true };
  }
  if (input.capturePath === 'visible-raw' && delivered !== null) {
    return { cause: 'visible-raw-cadence-shortfall', detail: `visible raw capture delivered only ${delivered.toFixed(1)}fps against a ${floor}fps floor`, evidenceComplete: captureFps !== null };
  }
  return {
    cause: 'receiver-transport-unknown',
    detail: 'browser evidence cannot localize the low cadence; capture path or sender-stage evidence is missing',
    evidenceComplete: false,
  };
}

interface TimelineState {
  internalKey: string;
  ownerIdentity: string;
  ownerAlias: string;
  windowId: number;
  publicationSid: string;
  shareEpoch: string | null;
  startedAt: number;
  updatedAt: number;
  events: StartupEventObservation[];
  classificationInput: StartupClassificationInput;
}

export interface StartupRecorderOptions {
  now?: () => number;
  maxTimelines?: number;
  maxEventsPerTimeline?: number;
  ttlMs?: number;
}

export class StartupTimelineRecorder {
  private readonly now: () => number;
  private readonly maxTimelines: number;
  private readonly maxEventsPerTimeline: number;
  private readonly ttlMs: number;
  private readonly timelines = new Map<string, TimelineState>();
  private readonly ownerAliases = new Map<string, string>();
  private nextOwnerAlias = 1;

  constructor(options: StartupRecorderOptions = {}) {
    this.now = options.now ?? (() => performance.now());
    this.maxTimelines = options.maxTimelines ?? DEFAULT_MAX_TIMELINES;
    this.maxEventsPerTimeline = options.maxEventsPerTimeline ?? DEFAULT_MAX_EVENTS_PER_TIMELINE;
    this.ttlMs = options.ttlMs ?? DEFAULT_TTL_MS;
  }

  record(
    correlation: StartupCorrelation,
    kind: StartupEventKind,
    details: Omit<StartupEventObservation, 'kind' | 'atMonotonicMs' | 'elapsedMs'> = {},
  ): void {
    if (!correlation.ownerIdentity || !correlation.publicationSid || !Number.isSafeInteger(correlation.windowId) || correlation.windowId < 1) return;
    const at = this.now();
    this.prune(at);
    const key = `${correlation.ownerIdentity}:${correlation.windowId}:${correlation.publicationSid}`;
    let timeline = this.timelines.get(key);
    if (!timeline) {
      const ownerAlias = this.ownerAliases.get(correlation.ownerIdentity) ?? `peer-${this.nextOwnerAlias++}`;
      this.ownerAliases.set(correlation.ownerIdentity, ownerAlias);
      timeline = {
        internalKey: key,
        ownerIdentity: correlation.ownerIdentity,
        ownerAlias,
        windowId: correlation.windowId,
        publicationSid: correlation.publicationSid,
        shareEpoch: correlation.shareEpoch ?? null,
        startedAt: at,
        updatedAt: at,
        events: [],
        classificationInput: {},
      };
      this.timelines.set(key, timeline);
    }
    if (correlation.shareEpoch) timeline.shareEpoch = correlation.shareEpoch;
    timeline.updatedAt = at;
    const observation: StartupEventObservation = {
      kind,
      atMonotonicMs: at,
      elapsedMs: Math.max(0, at - timeline.startedAt),
      ...details,
    };
    const previous = timeline.events[timeline.events.length - 1];
    const samePayload = previous?.kind === kind && JSON.stringify({ ...previous, atMonotonicMs: 0, elapsedMs: 0 }) ===
      JSON.stringify({ ...observation, atMonotonicMs: 0, elapsedMs: 0 });
    if (!samePayload) {
      timeline.events.push(observation);
      if (timeline.events.length > this.maxEventsPerTimeline) {
        timeline.events.splice(0, timeline.events.length - this.maxEventsPerTimeline);
      }
    }
    this.updateClassificationInput(timeline, observation);
    while (this.timelines.size > this.maxTimelines) this.timelines.delete(this.timelines.keys().next().value!);
    this.pruneOwnerAliases();
  }

  snapshot(): StartupTimelineSnapshot[] {
    const now = this.now();
    this.prune(now);
    return [...this.timelines.values()].map((timeline) => ({
      correlation: {
        ownerAlias: timeline.ownerAlias,
        windowId: timeline.windowId,
        publicationSid: timeline.publicationSid,
        shareEpoch: timeline.shareEpoch,
      },
      clock: {
        basis: 'receiver-performance-now',
        crossPeerComparable: false,
        uncertainty: 'uncalibrated-cross-peer-clocks',
      },
      events: timeline.events.map((event) => ({ ...event })),
      classification: classifyStartup({
        ...timeline.classificationInput,
        observationElapsedMs: Math.max(0, now - timeline.startedAt),
      }),
    }));
  }

  reset(): void {
    this.timelines.clear();
    this.ownerAliases.clear();
    this.nextOwnerAlias = 1;
  }

  private prune(now: number): void {
    for (const [key, timeline] of this.timelines) {
      if (now - timeline.updatedAt > this.ttlMs) this.timelines.delete(key);
    }
    this.pruneOwnerAliases();
  }

  private pruneOwnerAliases(): void {
    const retainedOwners = new Set([...this.timelines.values()].map((timeline) => timeline.ownerIdentity));
    for (const ownerIdentity of this.ownerAliases.keys()) {
      if (!retainedOwners.has(ownerIdentity)) this.ownerAliases.delete(ownerIdentity);
    }
  }

  private updateClassificationInput(timeline: TimelineState, event: StartupEventObservation): void {
    const input = timeline.classificationInput;
    if (event.kind === 'trackPublished') input.publishedElapsedMs = event.elapsedMs;
    if (event.kind === 'trackSubscribed') input.subscribedElapsedMs = event.elapsedMs;
    if (event.kind === 'firstPresented') input.firstPresentedElapsedMs = event.elapsedMs;
    if (event.kind === 'viewerDemand') {
      input.demandElapsedMs = event.elapsedMs;
      input.requestedSubscription = event.requestedSubscription;
      input.demandWidth = event.demandWidth;
      input.demandHeight = event.demandHeight;
      input.requestedWidth = event.requestedWidth;
      input.requestedHeight = event.requestedHeight;
    }
    if (event.kind === 'statsTransition') {
      if (input.initialDecodedWidth === undefined && positive(event.decodedWidth) !== null) {
        input.initialDecodedWidth = event.decodedWidth;
        input.initialDecodedHeight = event.decodedHeight;
      }
      input.decodedWidth = event.decodedWidth;
      input.decodedHeight = event.decodedHeight;
      input.decodedFps = event.decodedFps;
      input.presentedFps = event.presentedFps;
      input.capturePath = event.capturePath;
      input.captureFps = event.captureFps;
    }
  }
}
