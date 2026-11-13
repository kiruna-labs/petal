// ---------------------------------------------------------------------------
// Live streams, as a web guest sees them. A desktop participant who goes
// live publishes a `petal.live` participant attribute; this client derives
// the room's live state from those attributes (ported from the desktop's
// `streaming::wire::derive_live_state`, pinned to the same contract
// fixture), shows a persistent "This meeting is being live streamed" banner
// while any stream runs, and offers "Keep me off live streams" at all times,
// before anyone goes live too: the `petal.stream-optout` attribute, which
// keeps this guest's audio out of every stream's mix. The choice is
// remembered per room in localStorage and written on connect (before the mic
// publishes: `beforeMicrophonePublish`), on every toggle, and again after
// every reconnect; a failed write is retried until it sticks (the token
// grants canUpdateOwnMetadata). A web guest can't stream, so the toggle is
// always theirs to use.
// ---------------------------------------------------------------------------

import { RoomEvent } from 'livekit-client';

export const STREAM_LIVE_ATTRIBUTE = 'petal.live';
export const STREAM_OPTOUT_ATTRIBUTE = 'petal.stream-optout';
/** The only value that opts a participant out. */
export const STREAM_OPTOUT_VALUE = '1';
/**
 * What a client writes to be included. A streamer that has seen someone
 * opted out keeps them out until it sees a value like this: an attribute lost
 * to a rejoin is not an opt-in.
 */
export const STREAM_OPTIN_VALUE = '0';
/** Per room: `petal.keepOffLiveStreams:<room name>`. */
export const KEEP_OFF_LIVE_STREAMS_STORAGE_KEY = 'petal.keepOffLiveStreams';
/** First retry delay after a failed write; doubles up to the max. */
export const OPT_OUT_RETRY_MIN_MS = 250;
export const OPT_OUT_RETRY_MAX_MS = 5000;
/** How long a mic publish waits for the opt-out to reach the server. */
export const OPT_OUT_BEFORE_MIC_TIMEOUT_MS = 5000;

export interface LiveStream {
  ownerIdentity: string;
  windowId: number;
  paused: boolean;
  startedAt: number;
}

const U32_MAX = 0xffff_ffff;

function isUnsigned(value: unknown, max: number): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= max;
}

/** One `petal.live` value, or null when absent or malformed (as in Rust). */
export function parseLiveAttribute(value: string | null | undefined): Omit<LiveStream, 'ownerIdentity'> | null {
  if (!value) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(value);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return null;
  const { mode, startedAt, windowId, state } = parsed as Record<string, unknown>;
  if (mode !== 'client') return null;
  if (state !== 'live' && state !== 'paused') return null;
  if (!isUnsigned(startedAt, Number.MAX_SAFE_INTEGER) || !isUnsigned(windowId, U32_MAX)) return null;
  return { windowId, startedAt, paused: state === 'paused' };
}

/**
 * Every live share in the room, earliest first by `(startedAt, identity)`;
 * the first is the room's live stream.
 */
export function deriveLiveStreams(
  attributes: Iterable<{ identity: string; value: string | null | undefined }>
): LiveStream[] {
  const streams: LiveStream[] = [];
  for (const { identity, value } of attributes) {
    const parsed = parseLiveAttribute(value);
    if (parsed) streams.push({ ownerIdentity: identity, ...parsed });
  }
  return streams.sort((a, b) =>
    a.startedAt !== b.startedAt
      ? a.startedAt - b.startedAt
      : a.ownerIdentity < b.ownerIdentity
        ? -1
        : a.ownerIdentity > b.ownerIdentity
          ? 1
          : 0
  );
}

/** Whether a `petal.stream-optout` value opts that participant out. */
export function isOptedOut(value: string | null | undefined): boolean {
  return value === STREAM_OPTOUT_VALUE;
}

/** The attribute value for a setting. */
export function optOutValue(keepOff: boolean): string {
  return keepOff ? STREAM_OPTOUT_VALUE : STREAM_OPTIN_VALUE;
}

type KeyValueStorage = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;

/** Where the choice for `roomName` is remembered. */
export function keepOffStorageKey(roomName: string): string {
  return `${KEEP_OFF_LIVE_STREAMS_STORAGE_KEY}:${roomName}`;
}

export function loadKeepOffLiveStreams(storage: KeyValueStorage | null, roomName: string): boolean {
  try {
    return storage?.getItem(keepOffStorageKey(roomName)) === '1';
  } catch {
    return false;
  }
}

export function saveKeepOffLiveStreams(storage: KeyValueStorage | null, roomName: string, keepOff: boolean): void {
  try {
    if (keepOff) storage?.setItem(keepOffStorageKey(roomName), '1');
    else storage?.removeItem(keepOffStorageKey(roomName));
  } catch {
    // Private mode / storage disabled: the choice lasts this session only.
  }
}

/** The parts of a LiveKit room this module touches. */
export interface LiveStreamRoom {
  /** The LiveKit room name: the opt-out is remembered per room. */
  name: string;
  localParticipant: {
    identity: string;
    attributes: Readonly<Record<string, string>>;
    setAttributes(attributes: Record<string, string>): Promise<void>;
  };
  remoteParticipants: ReadonlyMap<string, { identity: string; attributes: Readonly<Record<string, string>> }>;
  on(event: RoomEvent, listener: (...args: never[]) => void): void;
  off(event: RoomEvent, listener: (...args: never[]) => void): void;
}

export interface LiveStreamView {
  render(live: boolean, keepOff: boolean): void;
  destroy(): void;
}

export interface LiveStreamController {
  readonly live: boolean;
  readonly keepOff: boolean;
  setKeepOff(keepOff: boolean): Promise<void>;
  /** Resolves once the server has the current opt-out (or after `timeoutMs`); true when it does. */
  optOutSettled(timeoutMs: number): Promise<boolean>;
  detach(): void;
}

let activeController: LiveStreamController | null = null;

export const OPT_OUT_NOT_SAVED_ERROR = 'Couldn\'t save "Keep me off live streams", so the mic stays off. Try again.';

/**
 * Call before publishing a microphone: waits (bounded) until the server has
 * this guest's opt-out, so a streamer never hears an opted-out mic. Throws
 * when an opt-out still isn't saved: the mic must not publish without it.
 */
export async function beforeMicrophonePublish(timeoutMs = OPT_OUT_BEFORE_MIC_TIMEOUT_MS): Promise<void> {
  const controller = activeController;
  if (!controller) return;
  if (!(await controller.optOutSettled(timeoutMs)) && controller.keepOff) {
    throw new Error(OPT_OUT_NOT_SAVED_ERROR);
  }
}

function roomAttributes(room: LiveStreamRoom) {
  const all = [{ identity: room.localParticipant.identity, value: room.localParticipant.attributes[STREAM_LIVE_ATTRIBUTE] }];
  for (const participant of room.remoteParticipants.values()) {
    all.push({ identity: participant.identity, value: participant.attributes[STREAM_LIVE_ATTRIBUTE] });
  }
  return all;
}

/**
 * Follow a connected `room`'s live streams into `view` and keep this guest's
 * opt-out attribute in step with the remembered choice: written until the
 * server takes it, and again after every reconnect (a full reconnect rejoins
 * with no attributes).
 */
export function attachLiveStream(
  room: LiveStreamRoom,
  options: {
    storage: KeyValueStorage | null;
    createView: (onToggle: (keepOff: boolean) => void) => LiveStreamView;
    log?: (line: string, kind?: 'ok' | 'warn') => void;
    /** Injected in tests. */
    sleep?: (ms: number) => Promise<void>;
  }
): LiveStreamController {
  const sleep = options.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  let keepOff = loadKeepOffLiveStreams(options.storage, room.name);
  let live = false;
  let detached = false;
  // What the server holds, as far as we know: '' (absent) on a fresh join,
  // null while unknown (a failed write may still have applied).
  let confirmed: string | null = '';
  // Bumped on every change or reconnect; a write settles only its own.
  let revision = 0;
  let writer: Promise<void> | null = null;
  const settledListeners = new Set<() => void>();
  const view = options.createView((next) => void controller.setKeepOff(next));

  function notifySettled() {
    for (const listener of [...settledListeners]) listener();
  }

  function isSettled() {
    return confirmed === optOutValue(keepOff);
  }

  /** Write until the server has the current choice; one writer at a time. */
  function ensureWritten(): Promise<void> {
    if (writer) return writer;
    if (detached || isSettled()) return Promise.resolve();
    writer = writeLoop();
    return writer;
  }

  async function writeLoop(): Promise<void> {
    // Let ensureWritten() record this writer before anything below runs.
    await Promise.resolve();
    let delay = OPT_OUT_RETRY_MIN_MS;
    let failures = 0;
    while (!detached && !isSettled()) {
      const value = optOutValue(keepOff);
      const sent = revision;
      try {
        await room.localParticipant.setAttributes({ [STREAM_OPTOUT_ATTRIBUTE]: value });
        // Changed (or reconnected) while in flight: unknown until rewritten.
        confirmed = sent === revision ? value : null;
        failures = 0;
        delay = OPT_OUT_RETRY_MIN_MS;
      } catch (error) {
        confirmed = null;
        failures += 1;
        if (failures === 1 || failures % 10 === 0) {
          options.log?.(`couldn't save "Keep me off live streams" (attempt ${failures}): ${error}`, 'warn');
        }
        await sleep(delay);
        delay = Math.min(delay * 2, OPT_OUT_RETRY_MAX_MS);
      }
    }
    // Same synchronous step as the last check: a change after this starts a
    // new writer instead of joining one that is done.
    writer = null;
    notifySettled();
  }

  function refresh() {
    live = deriveLiveStreams(roomAttributes(room)).length > 0;
    view.render(live, keepOff);
  }

  function reconnected() {
    // A full reconnect rejoins with no attributes: write ours again.
    revision += 1;
    confirmed = null;
    void ensureWritten();
    refresh();
  }

  const listeners: Array<[RoomEvent, () => void]> = [
    [RoomEvent.ParticipantAttributesChanged, refresh],
    [RoomEvent.ParticipantConnected, refresh],
    [RoomEvent.ParticipantDisconnected, refresh],
    [RoomEvent.Reconnected, reconnected],
  ];
  for (const [event, listener] of listeners) room.on(event, listener);

  const controller: LiveStreamController = {
    get live() {
      return live;
    },
    get keepOff() {
      return keepOff;
    },
    async setKeepOff(next: boolean) {
      if (next === keepOff) return;
      keepOff = next;
      revision += 1;
      saveKeepOffLiveStreams(options.storage, room.name, keepOff);
      view.render(live, keepOff);
      await ensureWritten();
    },
    optOutSettled(timeoutMs: number) {
      // Only an opt-out has to be there before the mic; "0" goes out anyway.
      if (!keepOff || isSettled()) return Promise.resolve(true);
      return new Promise<boolean>((resolve) => {
        const done = (settled: boolean) => {
          settledListeners.delete(onSettled);
          clearTimeout(timer);
          resolve(settled);
        };
        const onSettled = () => {
          if (!keepOff || isSettled()) done(true);
          else if (detached) done(false);
        };
        const timer = setTimeout(() => {
          options.log?.('"Keep me off live streams" not saved yet', 'warn');
          done(false);
        }, timeoutMs);
        settledListeners.add(onSettled);
      });
    },
    detach() {
      detached = true;
      for (const [event, listener] of listeners) room.off(event, listener);
      view.destroy();
      notifySettled();
      if (activeController === controller) activeController = null;
    },
  };
  activeController = controller;
  // "1" or "0" on every join (see STREAM_OPTIN_VALUE).
  void ensureWritten();
  refresh();
  return controller;
}

/**
 * The strip right under the meeting topbar. "Keep me off live streams" is
 * always there, so a guest can opt out before anyone goes live; while a
 * stream runs the strip turns red and says so.
 */
export function createLiveStreamBanner(
  doc: Document,
  after: Element,
  onToggle: (keepOff: boolean) => void
): LiveStreamView {
  const banner = doc.createElement('div');
  banner.className = 'live-stream-banner';
  banner.setAttribute('role', 'status');
  banner.dataset.live = 'false';
  const dot = doc.createElement('span');
  dot.className = 'live-stream-dot';
  dot.setAttribute('aria-hidden', 'true');
  const text = doc.createElement('span');
  text.className = 'live-stream-text';
  text.textContent = 'This meeting is being live streamed';
  const label = doc.createElement('label');
  label.className = 'live-stream-optout';
  const checkbox = doc.createElement('input');
  checkbox.type = 'checkbox';
  checkbox.addEventListener('change', () => onToggle(checkbox.checked));
  label.append(checkbox, ' Keep me off live streams');
  banner.append(dot, text, label);
  after.after(banner);
  return {
    render(live, keepOff) {
      banner.dataset.live = String(live);
      dot.hidden = !live;
      text.hidden = !live;
      checkbox.checked = keepOff;
    },
    destroy() {
      banner.remove();
    },
  };
}
