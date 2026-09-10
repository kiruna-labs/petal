// #134: the host-side release oracle for the remote-control scenario suite.
//
// `published()` reads only the CONTROLLER's own wire echo, which publishes an
// Up whether or not native injection landed, so it can never see a phantom
// held button on the host. `remote-control-status`'s `pressedInputs` is the
// host's own bookkeeping and is the only assertion in the suite that can.
//
// Until this module existed, case 7 (`right drag held button`) was the ONLY
// case that read it, which made it a shared tripwire for every case before it:
// case 6's left drag leaked `buttons: 1` and case 7 reported the failure
// against its own right drag (`buttons: 2`). A case that presses must prove
// it released, under its own name.

/// The host's own `HELD_INPUT_TTL` (`remote_control_core.rs`), swept every
/// `HELD_INPUT_SWEEP_INTERVAL` (250ms). A phantom press self-clears shortly
/// after this, so any grace period here MUST stay well below it -- a longer
/// one would let the host's own safety net erase the evidence and read as a
/// release that never happened.
export const HOST_HELD_INPUT_TTL_MS = 1200;

/// The Up travels the data channel and is applied asynchronously, so an
/// immediate read can race a release that is genuinely on its way. Poll
/// instead of sampling once -- but only for a fraction of the TTL above.
export const DEFAULT_RELEASE_GRACE_MS = 600;
export const DEFAULT_RELEASE_POLL_INTERVAL_MS = 50;

/// The pressed entries in a `remote-control-status` snapshot, tolerant of a
/// snapshot that omits the field entirely.
export function heldInputs(snapshot) {
  const pressed = snapshot?.pressedInputs;
  return Array.isArray(pressed) ? pressed : [];
}

/// DOM-style button bitmask (1 = primary, 2 = right, 4 = middle), rendered so
/// a failure message names the actual stuck button rather than a number the
/// reader has to decode. Exactly the decoding the #134 investigation had to do
/// by hand from `{"buttons":1,...}`.
export function describeHeldButtons(pressed) {
  const names = [];
  for (const entry of pressed) {
    const mask = Number(entry?.buttons ?? 0);
    const parts = [];
    if (mask & 1) parts.push('primary');
    if (mask & 2) parts.push('right');
    if (mask & 4) parts.push('middle');
    names.push(parts.length ? parts.join('+') : `mask=${mask}`);
  }
  return names.length ? names.join(', ') : 'none';
}

export function releaseFailureMessage(gesture, snapshot, graceMs) {
  const pressed = heldInputs(snapshot);
  return (
    `${gesture} left the host holding ${describeHeldButtons(pressed)} `
    + `${graceMs}ms after the gesture ended -- the press was never released (#134): `
    + JSON.stringify(snapshot)
  );
}

/// Poll `readStatus` until the host reports no pressed inputs, or fail naming
/// the gesture that pressed. `sleep` and `now` are injected so this is unit
/// testable without real timers.
export async function assertReleasedWithin({
  gesture,
  readStatus,
  sleep,
  graceMs = DEFAULT_RELEASE_GRACE_MS,
  intervalMs = DEFAULT_RELEASE_POLL_INTERVAL_MS,
  now = () => Date.now(),
}) {
  if (typeof gesture !== 'string' || !gesture) throw new Error('assertReleasedWithin needs a gesture label');
  if (typeof readStatus !== 'function') throw new Error('assertReleasedWithin needs a readStatus function');
  if (typeof sleep !== 'function') throw new Error('assertReleasedWithin needs a sleep function');
  if (graceMs >= HOST_HELD_INPUT_TTL_MS) {
    throw new Error(
      `release grace ${graceMs}ms must stay below the host's own held-input TTL `
      + `(${HOST_HELD_INPUT_TTL_MS}ms) or the TTL sweeper releases the button for us and the leak reads as a pass (#134)`
    );
  }
  const deadline = now() + graceMs;
  let snapshot = await readStatus();
  while (heldInputs(snapshot).length > 0 && now() < deadline) {
    await sleep(intervalMs);
    snapshot = await readStatus();
  }
  if (heldInputs(snapshot).length > 0) {
    throw new Error(releaseFailureMessage(gesture, snapshot, graceMs));
  }
  return snapshot;
}

// ---- #134 left-drag release stress: evidence shaping ----------------------
//
// The leak reproduces about one suite run in three, so a single left drag per
// run finds it by luck. The stress case drags repeatedly and asserts a release
// after each one; everything below turns the occurrence it eventually catches
// into evidence that identifies the mechanism, rather than another
// "pressedInputs was not empty".

/// The host's own log line from #140's instrumentation. Its PRESENCE means
/// native injection of the Up failed and said why; its ABSENCE means no Up was
/// ever processed for that button. Those are different bugs.
export const RELEASE_NOT_INJECTED_NEEDLE = 'pointer RELEASE not injected';

export function releaseNotInjectedLines(lines) {
  return (Array.isArray(lines) ? lines : []).filter(
    (line) => typeof line === 'string' && line.includes(RELEASE_NOT_INJECTED_NEEDLE)
  );
}

/// The `reason=<ax-error|routes-exhausted|sink-error|injection-cancelled|
/// injection-timeout>` tag #140 attaches to that line.
export function parseReleaseFailureReason(line) {
  const match = /reason=([A-Za-z0-9_-]+)/.exec(typeof line === 'string' ? line : '');
  return match ? match[1] : null;
}

/// `(window_id, controller_id)` is the key `pressed_inputs` is filed under. An
/// entry whose key is absent from the live `sessions` list is the signature of
/// the leading surviving hypothesis: the Up was processed against a DIFFERENT
/// key and the original entry was never touched.
export function inputKey(entry) {
  return `(window ${entry?.windowId ?? '?'}, controller ${entry?.controllerId ?? '?'})`;
}

export function summarizeReleaseStressLeak({
  iteration,
  iterations,
  snapshot,
  logLines = [],
  sessionKeysBeforeGesture = [],
  clearedByHoverMove = null,
  clearedByTtl = null,
  assertionMessage = null,
}) {
  const pressed = heldInputs(snapshot);
  const failureLines = releaseNotInjectedLines(logLines);
  const reasons = [...new Set(failureLines.map(parseReleaseFailureReason).filter(Boolean))];
  const pressedKeys = pressed.map(inputKey);
  const sessionKeys = (Array.isArray(snapshot?.sessions) ? snapshot.sessions : []).map(inputKey);
  const keyMismatch = pressedKeys.some((key) => !sessionKeys.includes(key));
  const before = Array.isArray(sessionKeysBeforeGesture) ? sessionKeysBeforeGesture : [];
  // The smoking gun for hypothesis 2 would be a held entry filed under the key
  // the session had BEFORE the drag and no longer has: the Up was then applied
  // to a key holding nothing while the original entry sat untouched.
  const keyChangedDuringGesture = before.length > 0
    && (before.length !== sessionKeys.length || before.some((key) => !sessionKeys.includes(key)));
  const heldKeyIsPreGestureKey = pressedKeys.length > 0
    && pressedKeys.every((key) => before.includes(key) && !sessionKeys.includes(key));
  // The discriminator the #134 analysis asked for, stated in the failure
  // itself so nobody has to re-derive it from the artifact.
  const discriminator = failureLines.length
    ? `the host logged ${failureLines.length} '${RELEASE_NOT_INJECTED_NEEDLE}' line(s) `
      + `(reason=${reasons.length ? reasons.join('|') : 'unparsed'}) -- native injection of the Up FAILED`
    : `NO '${RELEASE_NOT_INJECTED_NEEDLE}' line accompanied it -- the Up was never PROCESSED `
      + '(never arrived, or was applied under a different key), rather than injected and failed';
  const keyNote = pressedKeys.length
    ? `held key(s) ${pressedKeys.join(', ')} vs live session key(s) ${sessionKeys.join(', ') || 'none'}`
      + ` (pre-gesture ${before.join(', ') || 'unrecorded'})`
      + `${keyMismatch ? ' -- KEY MISMATCH' : ' -- keys agree'}`
      + `${heldKeyIsPreGestureKey ? '; the held key is the PRE-GESTURE key -- the key changed mid-gesture' : ''}`
      + `${keyChangedDuringGesture && !heldKeyIsPreGestureKey ? '; the session key changed during the gesture' : ''}`
    : 'no held key in the snapshot';
  const recovery = [
    clearedByHoverMove === null ? null : `zero-mask move cleared it: ${clearedByHoverMove}`,
    clearedByTtl === null ? null : `host TTL cleared it: ${clearedByTtl}`,
  ].filter(Boolean).join('; ');
  const detail =
    `left-drag release leaked on iteration ${iteration}/${iterations}: `
    + `host holds ${describeHeldButtons(pressed)}. ${discriminator}. ${keyNote}.`
    + `${recovery ? ` ${recovery}.` : ''} snapshot=${JSON.stringify(snapshot)}`
    + `${assertionMessage ? ` assertion=${JSON.stringify(assertionMessage)}` : ''}`;
  return {
    issue: 134,
    iteration,
    iterations,
    heldButtons: describeHeldButtons(pressed),
    pressedKeys,
    sessionKeys,
    sessionKeysBeforeGesture: before,
    keyMismatch,
    keyChangedDuringGesture,
    heldKeyIsPreGestureKey,
    releaseFailureLines: failureLines,
    releaseFailureReasons: reasons,
    upWasProcessed: failureLines.length > 0 ? 'injection-failed' : 'never-processed',
    clearedByHoverMove,
    clearedByTtl,
    snapshot,
    assertionMessage,
    detail,
  };
}

/// #134 is OPEN and this case exists to observe it, so a caught leak must not
/// be able to fail a release: quarantined it reports `skip` (never a pass) with
/// the whole leak in its detail. Drop the quarantine with the fix -- #134
/// tracks that removal.
export function stressLeakOutcome(leak, { quarantined = true } = {}) {
  return quarantined
    ? { status: 'skip', detail: `QUARANTINED (#134, known-open, recorded not enforced) -- ${leak.detail}` }
    : { status: 'fail', detail: leak.detail };
}
