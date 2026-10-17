// Whole-display share oracles for the loopback tier (#334/#349, #335/#353).
//
// A macOS display share hands the host replay the sentinel pid 1
// (DISPLAY_REPLAY_TARGET_PID). Before #349/#353 that sentinel flowed into the
// window routes as if it were a process: AX hit-tested launchd, SkyLight
// posted clicks into it and reported them delivered, keys were refused with a
// window mismatch. Nothing reached the display. These helpers are the pure
// half of the cases that pin that shut; the live half is in
// remote-control-scenario.mjs (cases 37-41).

/// Normalized [0,1] coordinates for a global point (top-left origin, points)
/// inside the shared display's frame -- the inverse of the host's
/// `normalized_to_global`.
export function normalizedPointOnDisplay(displayFrame, point) {
  const { x, y, width, height } = displayFrame ?? {};
  if (![x, y, width, height].every(Number.isFinite) || width <= 0 || height <= 0) {
    throw new Error(`display frame is malformed: ${JSON.stringify(displayFrame)}`);
  }
  const nx = (point.x - x) / width;
  const ny = (point.y - y) / height;
  if (!(nx >= 0 && nx <= 1 && ny >= 0 && ny <= 1)) {
    throw new Error(`point (${point.x},${point.y}) is not on display ${JSON.stringify(displayFrame)}`);
  }
  return { x: Math.round(nx * 10000) / 10000, y: Math.round(ny * 10000) / 10000 };
}

/// A global point at fractional (fx, fy) of a window's AppleScript bounds
/// `[left, top, right, bottom]`.
export function pointInBounds(bounds, fx, fy) {
  const [left, top, right, bottom] = bounds;
  return { x: left + (right - left) * fx, y: top + (bottom - top) * fy };
}

/// Parse `osascript`'s rendering of a window's `bounds` ("1120, 40, 1920, 1080").
export function parseAppleScriptBounds(text) {
  const parts = String(text ?? '').split(',').map((part) => Number(part.trim()));
  if (parts.length !== 4 || !parts.every(Number.isFinite)) {
    throw new Error(`could not parse AppleScript bounds ${JSON.stringify(text)}`);
  }
  const [left, top, right, bottom] = parts;
  if (right <= left || bottom <= top) throw new Error(`empty AppleScript bounds ${JSON.stringify(text)}`);
  return parts;
}

/// True when two `[left, top, right, bottom]` rects share any area.
export function boundsOverlap(a, b) {
  return a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3];
}

/// True when the window's bounds cover the display's last addressable point,
/// i.e. where the host clamps a normalized (1.0, 1.0) click (#349 review).
export function boundsCoverDisplayCorner(bounds, displayFrame) {
  const cornerX = displayFrame.x + displayFrame.width - 1;
  const cornerY = displayFrame.y + displayFrame.height - 1;
  return bounds[0] <= cornerX && cornerX < bounds[2] && bounds[1] <= cornerY && cornerY < bounds[3];
}

/// A `wvous-br-corner` value that triggers an action when the cursor reaches
/// the bottom-right corner (0 and 1 are "no action"; unset reads as null).
export function hotCornerActive(value) {
  if (value === null || value === undefined || String(value).trim() === '') return false;
  const parsed = Number(String(value).trim());
  return Number.isFinite(parsed) && parsed !== 0 && parsed !== 1;
}

// `pid=1` / `pid 1` as a PROCESS identity in a host log line. `target_pid=`
// is excluded on purpose: the replay pipeline legitimately carries the
// sentinel there (enqueue/latency lines) -- what must never happen is a
// route treating it as a process (AX on launchd, SLEventPostToPid(1),
// raise(pid=1), a key refused for pid 1).
const PID_ONE_AS_PROCESS = /(?:^|[^A-Za-z0-9_])pid[= ]1(?![0-9])/;

/// Host log lines that show a display-share input going to pid 1 or failing
/// its route. `windowId` scopes the window-tagged failure lines.
export function displayRouteViolations(lines, windowId) {
  const id = String(windowId);
  return (Array.isArray(lines) ? lines : []).filter((line) => {
    if (typeof line !== 'string' || !line.includes('remote-control')) return false;
    if (PID_ONE_AS_PROCESS.test(line)) return true;
    const mentionsWindow = new RegExp(`window(?:_id)?=${id}(?![0-9])`).test(line);
    return mentionsWindow && (
      line.includes('pointer RELEASE not injected')
      || line.includes('routes-exhausted')
      || line.includes('refusing to report delivery')
    );
  });
}

/// The host's `enqueue replay` lines for this display share. Their presence
/// is what makes an empty `displayRouteViolations` mean something: no lines
/// at all means the log oracle is blind, not that routing was clean.
export function displayEnqueueLines(lines, windowId) {
  const pattern = new RegExp(`enqueue replay .*window=${windowId}(?![0-9]).*target_pid=1(?![0-9])`);
  return (Array.isArray(lines) ? lines : []).filter(
    (line) => typeof line === 'string' && pattern.test(line)
  );
}

/// Verdict for one display case's slice of the host log.
export function displayRoutingVerdict(lines, windowId) {
  const enqueued = displayEnqueueLines(lines, windowId);
  const violations = displayRouteViolations(lines, windowId);
  if (enqueued.length === 0) {
    return {
      ok: false,
      enqueued: 0,
      violations,
      message: `host log shows no display-share replay for window ${windowId} (target_pid=1); the pid-1 oracle would be blind`,
    };
  }
  if (violations.length > 0) {
    return {
      ok: false,
      enqueued: enqueued.length,
      violations,
      message: `display-share input was routed to pid 1 or failed its route (#334/#335): ${violations.slice(0, 3).map((line) => line.trim()).join(' | ')}`,
    };
  }
  return { ok: true, enqueued: enqueued.length, violations: [], message: `${enqueued.length} display replays, none routed to pid 1` };
}

const DELIVERED_OUTCOMES = new Set(['applied', 'submitted']);

/// Terminal v2 results for the display target that were NOT delivered.
export function undeliveredDisplayResults(records, windowId) {
  return (Array.isArray(records) ? records : []).filter(
    (record) => record?.windowId === windowId && !DELIVERED_OUTCOMES.has(record.outcome)
  );
}
