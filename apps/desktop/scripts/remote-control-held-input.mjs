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
