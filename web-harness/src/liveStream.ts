// ---------------------------------------------------------------------------
// Live streams, as a web guest sees them. A desktop participant who goes
// live publishes a `petal.live` participant attribute; this client derives
// the room's live state from those attributes (ported from the desktop's
// `streaming::wire::derive_live_state`, pinned to the same contract
// fixture), shows a persistent "This meeting is being live streamed" banner
// while any stream runs, and offers "Keep me off live streams": the
// `petal.stream-optout` attribute, which keeps this guest's audio out of
// every stream's mix. The choice is remembered in localStorage and written
// on connect and on every toggle (the token grants canUpdateOwnMetadata).
// A web guest can't stream, so the toggle is always theirs to use.
// ---------------------------------------------------------------------------

import { RoomEvent } from 'livekit-client';

export const STREAM_LIVE_ATTRIBUTE = 'petal.live';
export const STREAM_OPTOUT_ATTRIBUTE = 'petal.stream-optout';
/** The only value that opts a participant out. */
export const STREAM_OPTOUT_VALUE = '1';
export const KEEP_OFF_LIVE_STREAMS_STORAGE_KEY = 'petal.keepOffLiveStreams';

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

/** The attribute value for a setting; empty deletes the key. */
export function optOutValue(keepOff: boolean): string {
  return keepOff ? STREAM_OPTOUT_VALUE : '';
}

type KeyValueStorage = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;

export function loadKeepOffLiveStreams(storage: KeyValueStorage | null): boolean {
  try {
    return storage?.getItem(KEEP_OFF_LIVE_STREAMS_STORAGE_KEY) === '1';
  } catch {
    return false;
  }
}

export function saveKeepOffLiveStreams(storage: KeyValueStorage | null, keepOff: boolean): void {
  try {
    if (keepOff) storage?.setItem(KEEP_OFF_LIVE_STREAMS_STORAGE_KEY, '1');
    else storage?.removeItem(KEEP_OFF_LIVE_STREAMS_STORAGE_KEY);
  } catch {
    // Private mode / storage disabled: the choice lasts this session only.
  }
}

/** The parts of a LiveKit room this module touches. */
export interface LiveStreamRoom {
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
  detach(): void;
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
 * opt-out attribute in step with the remembered choice.
 */
export function attachLiveStream(
  room: LiveStreamRoom,
  options: {
    storage: KeyValueStorage | null;
    createView: (onToggle: (keepOff: boolean) => void) => LiveStreamView;
    log?: (line: string, kind?: 'ok' | 'warn') => void;
  }
): LiveStreamController {
  let keepOff = loadKeepOffLiveStreams(options.storage);
  let live = false;
  const view = options.createView((next) => void controller.setKeepOff(next));

  async function writeOptOut() {
    try {
      await room.localParticipant.setAttributes({ [STREAM_OPTOUT_ATTRIBUTE]: optOutValue(keepOff) });
    } catch (error) {
      options.log?.(`couldn't save "Keep me off live streams": ${error}`, 'warn');
    }
  }

  function refresh() {
    live = deriveLiveStreams(roomAttributes(room)).length > 0;
    view.render(live, keepOff);
  }

  const listeners: Array<[RoomEvent, () => void]> = [
    [RoomEvent.ParticipantAttributesChanged, refresh],
    [RoomEvent.ParticipantConnected, refresh],
    [RoomEvent.ParticipantDisconnected, refresh],
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
      saveKeepOffLiveStreams(options.storage, keepOff);
      view.render(live, keepOff);
      await writeOptOut();
    },
    detach() {
      for (const [event, listener] of listeners) room.off(event, listener);
      view.destroy();
    },
  };
  // Absent means included, so only an opt-out needs writing on join.
  if (keepOff) void writeOptOut();
  refresh();
  return controller;
}

/** The banner strip, placed right under the meeting topbar. */
export function createLiveStreamBanner(
  doc: Document,
  after: Element,
  onToggle: (keepOff: boolean) => void
): LiveStreamView {
  const banner = doc.createElement('div');
  banner.className = 'live-stream-banner';
  banner.setAttribute('role', 'status');
  banner.hidden = true;
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
      banner.hidden = !live;
      checkbox.checked = keepOff;
    },
    destroy() {
      banner.remove();
    },
  };
}
