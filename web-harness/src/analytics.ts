// Closed PostHog product-event pipe (browser client).
//
// Sentry stays the crash tool. These twelve events answer “are users having
// a bad time?” as rates. The allowlist lives in
// `docs/POSTHOG_EVENT_ALLOWLIST.md` — no new event without an
// explicit add there. Local/CI builds are keyless and no-op; a production
// web-harness build bakes `VITE_PETAL_POSTHOG_KEY` the same way it bakes
// `VITE_SENTRY_DSN`.
//
// Host-side capture over `fetch`, not `posthog-js` (replay / autocapture /
// exception capture stay locked off). Never send room names, identities,
// window titles, device names, key codes, clipboard, coordinates, or tokens.

declare const __PETAL_BUILD_INFO__: { version: string } | undefined;

const DEFAULT_HOST = 'https://us.i.posthog.com';
const CAPTURE_PATH = '/i/v0/e/';
const STORAGE_KEY = 'petal-analytics-id';
const TYPE_IDLE_MS = 1000;
const SCROLL_IDLE_MS = 500;
const DISPLAY_RECONFIG_DEBOUNCE_MS = 1000;
const VIDEO_STALL_MS = 10_000;
const AUDIO_SILENCE_MS = 10_000;
const AUDIO_SAMPLE_MS = 1000;
const AUDIO_SILENCE_PEAK = 2;

export const EVENT_NAMES = [
  'meeting_joined',
  'meeting_left',
  'join_failed',
  'share_started',
  'share_stopped',
  'remote_audio_silent',
  'remote_video_stalled',
  'capture_restarted',
  'reconnect',
  'permission_denied',
  'remote_control_input',
  'device_changed',
] as const;

export type EventName = (typeof EVENT_NAMES)[number];

export type DurationBucket = '0_10s' | '10_30s' | '30_120s' | '120s_plus';
export type ReconnectCountBucket = '0' | '1' | '2_4' | '5_plus';
export type JoinFailedReason = 'network' | 'no_backend' | 'token' | 'timeout';
export type ShareStartedSource = 'window' | 'display' | 'picker';
export type ShareStoppedReason = 'user' | 'window_gone' | 'capture_failed';
export type VideoStallSource = 'stats' | 'gallery' | 'native';
export type RestartOutcome = 'recovered' | 'failed';
export type PermissionKind = 'screen' | 'mic' | 'camera';
export type RemoteControlInputKind = 'click' | 'type' | 'paste' | 'scroll';
export type DeviceKind = 'display' | 'camera' | 'mic';
export type DeviceChange = 'switched' | 'failed' | 'reconfigured' | 'sleep' | 'wake';

export type CapturedEvent = {
  name: EventName;
  properties: Record<string, string>;
};

type ViteImportMeta = ImportMeta & {
  env?: {
    VITE_PETAL_POSTHOG_KEY?: string;
    VITE_PETAL_POSTHOG_HOST?: string;
  };
};

type Meeting = {
  joinedAt: number;
  reconnects: number;
};

type VideoStallState = {
  lastFrames: number;
  lastProgressAt: number;
  alarmed: boolean;
};

type AudioSilenceState = {
  silentSince: number | null;
  alarmed: boolean;
};

type ClassifiedInput = 'click' | 'pointer_down' | 'pointer_up' | 'type' | 'paste' | 'scroll';

export type RemoteControlLike = {
  kind?: string;
  action?: string;
};

type EventSpec = {
  name: EventName;
  extras: Record<string, string>;
};

const COMMON_KEYS = ['build_version', 'os', 'os_version', 'arch', 'client'] as const;

let meeting: Meeting | null = null;
let leaveRequested = false;
let distinctId = '';
let keylessLogged = false;
let lastDisplayReconfigAt = 0;
let testSink: CapturedEvent[] | null = null;
let pagehideInstalled = false;

const videoStalls = new Map<string, VideoStallState>();
const audioSilences = new Map<string, AudioSilenceState>();

const coalescer = {
  pointerDown: false,
  lastTypeAt: 0,
  lastScrollAt: 0,
};

function envRecord(): ViteImportMeta['env'] {
  return (import.meta as ViteImportMeta).env;
}

function processEnv(name: string): string | undefined {
  try {
    const proc = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process;
    const value = proc?.env?.[name];
    return value?.trim() || undefined;
  } catch {
    return undefined;
  }
}

export function apiKey(): string | undefined {
  const value = envRecord()?.VITE_PETAL_POSTHOG_KEY?.trim() || processEnv('VITE_PETAL_POSTHOG_KEY');
  if (!value || !value.startsWith('phc_')) return undefined;
  return value;
}

function host(): string {
  const value = envRecord()?.VITE_PETAL_POSTHOG_HOST?.trim() || processEnv('VITE_PETAL_POSTHOG_HOST');
  return (value || DEFAULT_HOST).replace(/\/$/, '');
}

export function durationBucket(durationMs: number): DurationBucket {
  if (durationMs < 10_000) return '0_10s';
  if (durationMs < 30_000) return '10_30s';
  if (durationMs < 120_000) return '30_120s';
  return '120s_plus';
}

export function reconnectCountBucket(count: number): ReconnectCountBucket {
  if (count <= 0) return '0';
  if (count === 1) return '1';
  if (count <= 4) return '2_4';
  return '5_plus';
}

export function joinFailedReasonFromError(error: unknown): JoinFailedReason {
  const message = error instanceof Error ? error.message : String(error);
  const name = error instanceof Error ? error.name : '';
  if (name === 'TimeoutError' || name === 'AbortError' || /timed out|timeout/i.test(message)) {
    return 'timeout';
  }
  if (/token request failed \(4|invalid token|malformed|decode|NotAllowed/i.test(message)) {
    return 'token';
  }
  return 'network';
}

export function isPermissionDeniedError(error: unknown): boolean {
  return Boolean(error && typeof error === 'object' && 'name' in error && error.name === 'NotAllowedError');
}

export function videoStallSource(source: string): VideoStallSource {
  if (source.includes('stats-frame-starvation') || source.startsWith('stats-')) return 'stats';
  if (source.includes('gallery') || source.includes('livekit-js')) return 'gallery';
  return 'native';
}

function buildVersion(): string {
  try {
    if (typeof __PETAL_BUILD_INFO__ !== 'undefined' && __PETAL_BUILD_INFO__?.version) {
      return __PETAL_BUILD_INFO__.version;
    }
  } catch {
    // node tests have no Vite define
  }
  return '0.0.0';
}

type NavigatorUa = Navigator & {
  userAgentData?: { platform?: string; architecture?: string };
};

function navigatorInfo(): NavigatorUa | undefined {
  return typeof navigator === 'undefined' ? undefined : (navigator as NavigatorUa);
}

export function osLabel(ua = navigatorInfo()?.userAgent ?? '', platform = navigatorInfo()?.platform ?? ''): string {
  const uaPlatform = navigatorInfo()?.userAgentData?.platform?.toLowerCase() ?? '';
  const haystack = `${ua} ${platform} ${uaPlatform}`;
  if (/win/i.test(haystack) && !/darwin/i.test(haystack)) return 'windows';
  if (/mac|iphone|ipad|ipod/i.test(haystack)) return /iphone|ipad|ipod/i.test(haystack) ? 'ios' : 'macos';
  if (/android/i.test(haystack)) return 'android';
  if (/linux/i.test(haystack)) return 'linux';
  return 'web';
}

export function osVersion(ua = navigatorInfo()?.userAgent ?? ''): string {
  const mac = /Mac OS X (\d+[._]\d+(?:[._]\d+)?)/.exec(ua);
  if (mac?.[1]) return mac[1].replace(/_/g, '.');
  const ios = /OS (\d+[._]\d+(?:[._]\d+)?)/.exec(ua);
  if (ios?.[1] && /iPhone|iPad|iPod/.test(ua)) return ios[1].replace(/_/g, '.');
  const win = /Windows NT (\d+\.\d+)/.exec(ua);
  if (win?.[1]) return win[1];
  const android = /Android (\d+(?:\.\d+)?)/.exec(ua);
  if (android?.[1]) return android[1];
  return 'unknown';
}

export function archLabel(ua = navigatorInfo()?.userAgent ?? ''): string {
  const uaArch = navigatorInfo()?.userAgentData?.architecture?.toLowerCase();
  if (uaArch === 'arm' || uaArch === 'arm64') return 'arm64';
  if (uaArch === 'x86' || uaArch === 'x86_64') return uaArch === 'x86' ? 'x86' : 'x86_64';
  if (/aarch64|arm64/i.test(ua)) return 'arm64';
  if (/x86_64|win64|wow64|amd64/i.test(ua)) return 'x86_64';
  return 'unknown';
}

export function commonProperties(): Record<string, string> {
  return {
    build_version: buildVersion(),
    os: osLabel(),
    os_version: osVersion(),
    arch: archLabel(),
    client: 'web',
  };
}

function newDistinctId(): string {
  const bytes = new Uint8Array(16);
  if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') {
    crypto.getRandomValues(bytes);
  } else {
    for (let i = 0; i < 16; i += 1) bytes[i] = Math.floor(Math.random() * 256);
  }
  return [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function readStoredId(): string | null {
  try {
    if (typeof localStorage === 'undefined') return null;
    const existing = localStorage.getItem(STORAGE_KEY)?.trim() ?? '';
    if (existing.length === 32 && /^[0-9a-f]+$/i.test(existing)) return existing.toLowerCase();
  } catch {
    return null;
  }
  return null;
}

function persistDistinctId(id: string): void {
  try {
    if (typeof localStorage === 'undefined') return;
    localStorage.setItem(STORAGE_KEY, id);
  } catch {
    // private mode / node tests
  }
}

function loadOrCreateDistinctId(): string {
  const existing = readStoredId();
  if (existing) return existing;
  const id = newDistinctId();
  persistDistinctId(id);
  return id;
}

export function inMeeting(): boolean {
  return meeting !== null;
}

function extrasFor(spec: EventSpec): Record<string, string> {
  return { ...commonProperties(), ...spec.extras };
}

function send(body: Record<string, unknown>): void {
  const key = apiKey();
  if (!key) return;
  const payload = JSON.stringify({ ...body, api_key: key });
  const url = `${host()}${CAPTURE_PATH}`;
  try {
    if (typeof navigator !== 'undefined' && typeof navigator.sendBeacon === 'function') {
      const blob = new Blob([payload], { type: 'application/json' });
      if (navigator.sendBeacon(url, blob)) return;
    }
  } catch {
    // fall through to fetch
  }
  if (typeof fetch === 'function') {
    void fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: payload,
      keepalive: true,
    }).catch(() => undefined);
  }
}

function capture(spec: EventSpec): void {
  const properties = extrasFor(spec);
  if (testSink) {
    testSink.push({ name: spec.name, properties });
    return;
  }
  if (!apiKey()) return;
  send({
    event: spec.name,
    distinct_id: distinctId || loadOrCreateDistinctId(),
    properties: {
      $geoip_disable: true,
      $ip: null,
      ...properties,
    },
  });
}

export function meetingJoined(): void {
  meeting = { joinedAt: Date.now(), reconnects: 0 };
  leaveRequested = false;
  capture({ name: 'meeting_joined', extras: {} });
}

export function noteLeaveRequested(): void {
  leaveRequested = true;
}

export function consumeLeaveRequested(): boolean {
  const requested = leaveRequested;
  leaveRequested = false;
  return requested;
}

export function meetingLeft(): void {
  if (!meeting) return;
  const current = meeting;
  meeting = null;
  videoStalls.clear();
  audioSilences.clear();
  capture({
    name: 'meeting_left',
    extras: {
      duration_bucket: durationBucket(Date.now() - current.joinedAt),
      reconnect_count_bucket: reconnectCountBucket(current.reconnects),
    },
  });
}

export function joinFailed(reason: JoinFailedReason): void {
  capture({ name: 'join_failed', extras: { reason } });
}

export function joinFailedFromError(error: unknown): void {
  joinFailed(joinFailedReasonFromError(error));
}

export function shareStarted(source: ShareStartedSource): void {
  capture({ name: 'share_started', extras: { source } });
}

export function shareStopped(reason: ShareStoppedReason): void {
  capture({ name: 'share_stopped', extras: { reason } });
}

export function remoteAudioSilent(durationMs: number): void {
  capture({
    name: 'remote_audio_silent',
    extras: { duration_bucket: durationBucket(durationMs) },
  });
}

export function remoteVideoStalled(source: VideoStallSource | string): void {
  const resolved = source === 'stats' || source === 'gallery' || source === 'native' ? source : videoStallSource(source);
  capture({
    name: 'remote_video_stalled',
    extras: {
      // NO duration_bucket. Both clients used to hardcode '0_10s' here, so
      // every stall in PostHog read 0_10s regardless of how long it lasted --
      // a fabricated dimension that made the dashboard look correct while
      // being wrong. Missing data is hidden, never guessed (data honesty).
      // Native dropped it too; plumb a real duration before adding it back,
      // and keep both clients in lockstep when you do.
      source: resolved,
    },
  });
}

export function captureRestarted(outcome: RestartOutcome): void {
  capture({ name: 'capture_restarted', extras: { outcome } });
}

export function reconnectRecovered(): void {
  if (meeting) meeting.reconnects = Math.min(meeting.reconnects + 1, Number.MAX_SAFE_INTEGER);
  capture({ name: 'reconnect', extras: { outcome: 'recovered' } });
}

export function reconnectFailed(): void {
  capture({ name: 'reconnect', extras: { outcome: 'failed' } });
}

export function permissionDenied(kind: PermissionKind): void {
  capture({ name: 'permission_denied', extras: { kind } });
}

export function deviceChanged(kind: DeviceKind, change: DeviceChange): void {
  if (!inMeeting()) return;
  if (kind === 'display' && change === 'reconfigured') {
    const now = Date.now();
    if (lastDisplayReconfigAt !== 0 && now - lastDisplayReconfigAt < DISPLAY_RECONFIG_DEBOUNCE_MS) {
      return;
    }
    lastDisplayReconfigAt = now;
  }
  capture({ name: 'device_changed', extras: { kind, change } });
}

function burst(lastAt: number, now: number, idleMs: number): { emit: boolean; next: number } {
  return { emit: lastAt === 0 || now - lastAt >= idleMs, next: now };
}

export function classifyRemoteControl(message: RemoteControlLike): ClassifiedInput | null {
  switch (message.kind) {
    case 'pointer':
      if (message.action === 'click') return 'click';
      if (message.action === 'down') return 'pointer_down';
      if (message.action === 'up') return 'pointer_up';
      return null;
    case 'key':
      return 'type';
    case 'text':
      return 'paste';
    case 'wheel':
      return 'scroll';
    default:
      return null;
  }
}

export function noteRemoteControlApplied(message: RemoteControlLike, now = Date.now()): RemoteControlInputKind | null {
  const classified = classifyRemoteControl(message);
  if (!classified) return null;
  let kind: RemoteControlInputKind | null = null;
  if (classified === 'click') {
    coalescer.pointerDown = false;
    kind = 'click';
  } else if (classified === 'pointer_down') {
    coalescer.pointerDown = true;
  } else if (classified === 'pointer_up') {
    if (coalescer.pointerDown) {
      coalescer.pointerDown = false;
      kind = 'click';
    }
  } else if (classified === 'type') {
    const result = burst(coalescer.lastTypeAt, now, TYPE_IDLE_MS);
    coalescer.lastTypeAt = result.next;
    if (result.emit) kind = 'type';
  } else if (classified === 'scroll') {
    const result = burst(coalescer.lastScrollAt, now, SCROLL_IDLE_MS);
    coalescer.lastScrollAt = result.next;
    if (result.emit) kind = 'scroll';
  } else {
    kind = 'paste';
  }
  if (kind) capture({ name: 'remote_control_input', extras: { kind } });
  return kind;
}

export function noteVideoFrames(
  key: string,
  framesDecoded: number | null,
  source: VideoStallSource,
  now = Date.now()
): void {
  if (!inMeeting()) return;
  if (framesDecoded === null) return;
  let state = videoStalls.get(key);
  if (!state) {
    if (framesDecoded <= 0) return;
    videoStalls.set(key, { lastFrames: framesDecoded, lastProgressAt: now, alarmed: false });
    return;
  }
  if (framesDecoded > state.lastFrames) {
    state.lastFrames = framesDecoded;
    state.lastProgressAt = now;
    state.alarmed = false;
    return;
  }
  if (framesDecoded < state.lastFrames) {
    state.lastFrames = framesDecoded;
  }
  if (!state.alarmed && now - state.lastProgressAt >= VIDEO_STALL_MS) {
    state.alarmed = true;
    remoteVideoStalled(source);
  }
}

export function clearVideoStall(key: string): void {
  videoStalls.delete(key);
}

export function noteAudioEnergy(key: string, audible: boolean, muted: boolean, now = Date.now()): void {
  if (!inMeeting()) return;
  if (muted) {
    audioSilences.delete(key);
    return;
  }
  let state = audioSilences.get(key);
  if (!state) {
    state = { silentSince: audible ? null : now, alarmed: false };
    audioSilences.set(key, state);
    return;
  }
  if (audible) {
    state.silentSince = null;
    state.alarmed = false;
    return;
  }
  if (state.silentSince === null) state.silentSince = now;
  if (!state.alarmed && now - state.silentSince >= AUDIO_SILENCE_MS) {
    state.alarmed = true;
    remoteAudioSilent(AUDIO_SILENCE_MS);
  }
}

export function startRemoteAudioSilenceWatchdog(options: {
  key: string;
  mediaStreamTrack: MediaStreamTrack;
  isMuted: () => boolean;
}): () => void {
  const AudioContextCtor =
    typeof window === 'undefined'
      ? undefined
      : window.AudioContext ?? (window as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
  if (!AudioContextCtor) return () => undefined;
  let stopped = false;
  let interval: ReturnType<typeof setInterval> | null = null;
  let probeCtx: AudioContext | null = null;
  try {
    probeCtx = new AudioContextCtor();
    const source = probeCtx.createMediaStreamSource(new MediaStream([options.mediaStreamTrack]));
    const analyser = probeCtx.createAnalyser();
    analyser.fftSize = 512;
    source.connect(analyser);
    const data = new Uint8Array(analyser.frequencyBinCount);
    interval = setInterval(() => {
      if (stopped) return;
      analyser.getByteTimeDomainData(data);
      let peak = 0;
      for (const value of data) peak = Math.max(peak, Math.abs(value - 128));
      noteAudioEnergy(options.key, peak > AUDIO_SILENCE_PEAK, options.isMuted());
    }, AUDIO_SAMPLE_MS);
  } catch {
    return () => undefined;
  }
  return () => {
    if (stopped) return;
    stopped = true;
    if (interval !== null) clearInterval(interval);
    audioSilences.delete(options.key);
    void probeCtx?.close().catch(() => undefined);
  };
}

export function commonPropertyKeys(): readonly string[] {
  return COMMON_KEYS;
}

export function installTestSink(): CapturedEvent[] {
  resetAnalyticsState();
  testSink = [];
  return testSink;
}

export function uninstallTestSink(): void {
  testSink = null;
  resetAnalyticsState();
}

export function resetAnalyticsState(): void {
  meeting = null;
  leaveRequested = false;
  lastDisplayReconfigAt = 0;
  videoStalls.clear();
  audioSilences.clear();
  coalescer.pointerDown = false;
  coalescer.lastTypeAt = 0;
  coalescer.lastScrollAt = 0;
}

function onPageHide(): void {
  meetingLeft();
}

function onOrientationChange(): void {
  deviceChanged('display', 'reconfigured');
}

export function initAnalytics(): boolean {
  distinctId = loadOrCreateDistinctId();
  if (!pagehideInstalled && typeof window !== 'undefined') {
    window.addEventListener('pagehide', onPageHide);
    const orientation = typeof screen !== 'undefined' ? screen.orientation : undefined;
    orientation?.addEventListener?.('change', onOrientationChange);
    pagehideInstalled = true;
  }
  if (!apiKey()) {
    if (!keylessLogged) {
      keylessLogged = true;
      console.info('analytics: PostHog key absent -- product events disabled this run');
    }
    return false;
  }
  return true;
}
