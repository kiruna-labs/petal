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

/// The `[left, top, right, bottom]` strip the sacrificial TextEdit window is
/// placed in for the display-share cases. It must be flush with the display's
/// right edge (case 41 clicks the clamped x = 1.0) and must NOT overlap the
/// photon sentinel, which the click oracles raise to defocus TextEdit. The
/// preferred width shrinks to start right of the sentinel when the display is
/// too narrow for both side by side (a 1512-point laptop display put an
/// 800-point strip over a sentinel ending at x = 1080). Returns null when no
/// strip of at least `minWidth` fits, so the caller can say so instead of
/// running cases whose re-arm would cover their own target.
export function displayTextEditBounds(displayFrame, sentinel, options = {}) {
  const { width = 800, topInset = 40, minWidth = 320, gap = 16 } = options;
  const right = displayFrame.x + displayFrame.width;
  const top = displayFrame.y + topInset;
  const bottom = displayFrame.y + displayFrame.height;
  let left = right - width;
  if (sentinel && boundsOverlap([left, top, right, bottom], sentinel)) {
    left = Math.max(left, sentinel[2] + gap);
  }
  if (right - left < minWidth) return null;
  return [left, top, right, bottom];
}

/// True when the window's bounds cover the display's last addressable column
/// at height `y` -- where the host clamps a normalized x = 1.0 click (#349
/// review: unclamped, 1.0 mapped one point PAST the display).
export function boundsCoverDisplayRightEdge(bounds, displayFrame, y) {
  const lastX = displayFrame.x + displayFrame.width - 1;
  return bounds[0] <= lastX && lastX < bounds[2] && bounds[1] <= y && y < bounds[3];
}

/// A `wvous-br-corner` value that fires an action when the cursor reaches the
/// bottom-right corner (0 and 1 are "no action"; unset reads as null).
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
