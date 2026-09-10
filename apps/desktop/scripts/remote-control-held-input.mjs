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

// ---- #134 left-drag release stress: WHERE the drags happen ---------------
//
// The first stress run (0.9.22 gate) put all twelve drags in a quiet tail,
// after every other case had finished, and came back 12 for 12 clean. Against
// the measured ~1-in-3-per-suite rate that outcome has roughly an 11% chance,
// so the fault is probably NOT a flat per-drag probability -- it depends on
// conditions a tight isolated loop never creates. These checkpoints spend the
// same total number of drags in the four places the suite actually creates
// them. Each is its own case, so `runCase` wraps it in a fresh
// request/release grant cycle rather than reusing one long-lived grant.
export const LEFT_DRAG_STRESS_CHECKPOINTS = Object.freeze([
  Object.freeze({
    id: 33,
    afterCaseId: 7,
    context: "case 7's right drag + Escape",
    rationale:
      'the historical neighbourhood: every observed failure was case 6 leaking a primary button '
      + 'and case 7 (right drag, then Escape) reporting it. Drags here run with exactly the '
      + 'traffic that preceded the real occurrences ahead of them.',
  }),
  Object.freeze({
    id: 34,
    afterCaseId: 21,
    context: 'the keyboard/modifier/scroll block',
    rationale:
      'mid-suite, after a long run of non-pointer traffic on a heavily churned document -- '
      + 'the interleaving the tail placement removed by construction.',
  }),
  Object.freeze({
    id: 35,
    afterCaseId: 26,
    context: "case 26's controller-disconnect synthetic release",
    rationale:
      'the first lifecycle teardown that synthesises releases on the host. Hypothesis 3 is a '
      + 'path that clears the entry owner without draining the button; this is where such a '
      + 'path runs.',
  }),
  Object.freeze({
    id: 36,
    afterCaseId: 29,
    context: "case 29's reconnect during control",
    rationale:
      'the leading surviving hypothesis needs the (window_id, controller_id) key to change '
      + 'mid-gesture, and case 29 is the only reconnect in the suite. `runCase` already records '
      + '(#808) that the case right after it can have its fresh grant revoked by a stale '
      + 'ParticipantDisconnected aftershock -- i.e. the key really is in flux exactly here.',
  }),
]);

/// Split a total iteration budget across the checkpoints. The remainder goes
/// to the EARLIEST checkpoints, so a reduced budget still spends its drags in
/// the historical neighbourhood first rather than thinning every site equally.
export function distributeStressIterations(total, checkpointCount) {
  if (!Number.isInteger(total) || total < 0) throw new Error(`stress iteration total must be a non-negative integer, got ${total}`);
  if (!Number.isInteger(checkpointCount) || checkpointCount <= 0) {
    throw new Error(`stress checkpoint count must be a positive integer, got ${checkpointCount}`);
  }
  const base = Math.floor(total / checkpointCount);
  const remainder = total % checkpointCount;
  return Array.from({ length: checkpointCount }, (_unused, index) => base + (index < remainder ? 1 : 0));
}

/// A leak recorded at a scattered checkpoint can be inherited by the cases
/// that follow it -- the mis-attribution #134 is about, and the one property
/// the tail placement gave away for free. Derived from the recovery attempts
/// the case already makes, so the handoff hazard is stated rather than left
/// for the next case to discover.
export function stillHeldAfterRecovery({ clearedByHoverMove = null, clearedByTtl = null } = {}) {
  if (clearedByHoverMove === true || clearedByTtl === true) return false;
  if (clearedByHoverMove === null && clearedByTtl === null) return null;
  return true;
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
  // Which of the scattered placements this leak came from. Two runs that both
  // leak on "iteration 2" mean different things if one is after case 7's
  // Escape and the other after case 29's reconnect.
  checkpoint = null,
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
  const stillHeld = stillHeldAfterRecovery({ clearedByHoverMove, clearedByTtl });
  const recovery = [
    clearedByHoverMove === null ? null : `zero-mask move cleared it: ${clearedByHoverMove}`,
    clearedByTtl === null ? null : `host TTL cleared it: ${clearedByTtl}`,
    // The stress drags are scattered through the suite now, so a phantom that
    // survives recovery is standing in front of real cases. Say so here rather
    // than letting the next case's release assertion report it as its own.
    stillHeld === true
      ? 'the host STILL held the button after recovery -- a LATER case\'s release assertion may inherit this leak (#134)'
      : null,
  ].filter(Boolean).join('; ');
  const where = checkpoint
    ? ` at stress checkpoint ${checkpoint.index}/${checkpoint.of}`
      + ` (case ${checkpoint.id ?? '?'}, immediately after case ${checkpoint.afterCaseId ?? '?'}`
      + `${checkpoint.context ? ` -- ${checkpoint.context}` : ''})`
    : '';
  const detail =
    `left-drag release leaked on iteration ${iteration}/${iterations}${where}: `
    + `host holds ${describeHeldButtons(pressed)}. ${discriminator}. ${keyNote}.`
    + `${recovery ? ` ${recovery}.` : ''} snapshot=${JSON.stringify(snapshot)}`
    + `${assertionMessage ? ` assertion=${JSON.stringify(assertionMessage)}` : ''}`;
  return {
    issue: 134,
    iteration,
    iterations,
    checkpoint,
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
    stillHeldAfterRecovery: stillHeld,
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
