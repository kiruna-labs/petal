// User-facing copy for a failed meeting join (#356).
//
// `join_room` rejects with the native `{ kind, message }` error, whose message
// carries the real cause: the backend token timeout (PETAL-DESKTOP-33) or the
// OS socket error from the LiveKit dial (PETAL-DESKTOP-35, Windows
// "os error 10013"). Before #356 the meeting route dropped all of that and
// flashed "Could not join this room" for 900ms, so a user blocked by their
// own firewall had no idea why, or what to change.

const SERVER_UNREACHABLE =
  "Couldn't reach Petal's server. Check your internet connection, then try again.";
const BLOCKED =
  "Your firewall, antivirus or VPN blocked Petal's connection to the meeting server. Allow Petal through it, then try again.";
const REFUSED =
  'The meeting server refused the connection. A proxy or firewall on this network may be blocking Petal.';
const TIMED_OUT =
  'Connecting to the meeting server timed out. Check your internet connection, then try again.';
const UNREACHABLE =
  "The meeting server can't be reached from this network. Check your connection or VPN, then try again.";
const FALLBACK = 'Petal could not join this room. Check your connection, then try again.';

/** OS socket error codes (Windows WSA* and macOS errno) -> copy. Matched on
 * the locale-independent "(os error N)" suffix, never the localized text. */
const OS_ERROR_COPY: Record<number, string> = {
  10013: BLOCKED, // WSAEACCES: socket access denied (firewall/AV/VPN filter)
  1: BLOCKED, // EPERM (macOS): a network filter / firewall denied the socket
  13: BLOCKED, // EACCES
  10061: REFUSED, // WSAECONNREFUSED
  61: REFUSED, // ECONNREFUSED (macOS)
  10060: TIMED_OUT, // WSAETIMEDOUT
  60: TIMED_OUT, // ETIMEDOUT (macOS)
  10051: UNREACHABLE, // WSAENETUNREACH
  10065: UNREACHABLE, // WSAEHOSTUNREACH
  51: UNREACHABLE, // ENETUNREACH (macOS)
  65: UNREACHABLE // EHOSTUNREACH (macOS)
};

function errorParts(error: unknown): { kind: string; message: string } {
  if (typeof error === 'string') return { kind: '', message: error };
  if (error instanceof Error) return { kind: '', message: error.message };
  if (error && typeof error === 'object') {
    const record = error as { kind?: unknown; message?: unknown };
    return {
      kind: typeof record.kind === 'string' ? record.kind : '',
      message: typeof record.message === 'string' ? record.message : ''
    };
  }
  return { kind: '', message: '' };
}

export function joinFailureMessage(error: unknown): string {
  const { kind, message } = errorParts(error);
  const osError = /\(os error (\d+)\)/i.exec(message);
  if (osError) {
    const copy = OS_ERROR_COPY[Number(osError[1])];
    if (copy) return copy;
  }
  if (/backend token (request timed out|connection failed|transport failed)/i.test(message)) {
    return SERVER_UNREACHABLE;
  }
  if (kind === 'joinTimeout' || /connect attempt timed out/i.test(message)) return TIMED_OUT;
  return FALLBACK;
}
