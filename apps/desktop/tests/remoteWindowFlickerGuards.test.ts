import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// #840 (live incident 2026-08-20): a sharer republishing its display-share
// track every ~300ms made the macOS receiver hide and re-reveal a live remote
// window 94 times in 73 seconds, while the SFU held a publication throughout.
// #841 is the sharer-side loop that drove it.
//
// These pin the native-side wiring that Rust unit tests cannot reach (AppKit
// closures, an async fn needing a live SessionState). Two review passes each
// defeated an earlier version of this file with call-site mutants, so the
// assertions below are deliberately shaped against those specific escapes:
//   * argument tampering after a matched call prefix  -> match the FULL call
//     including its closing parens, with nothing appended;
//   * a second, unguarded copy added elsewhere in the same fn -> COUNT the
//     occurrences rather than testing adjacency of one of them;
//   * deleting a guard whose pure helper still has a unit test -> assert the
//     guard's own early return exists at its call site.

const compositor = readFileSync(
  new URL('../src-tauri/src/compositor.rs', import.meta.url),
  'utf8'
);
const subscriber = readFileSync(
  new URL('../src-tauri/src/transport/subscriber.rs', import.meta.url),
  'utf8'
);
const share = readFileSync(
  new URL('../src-tauri/src/session/share.rs', import.meta.url),
  'utf8'
);

/** Body of a top-level item, from `signature` up to the next item `endMarker`. */
function fnBody(source: string, signature: string, endMarker: string): string {
  const start = source.indexOf(signature);
  assert.notEqual(start, -1, `could not locate ${signature}`);
  const end = source.indexOf(endMarker, start);
  assert.notEqual(end, -1, `could not locate end marker after ${signature}`);
  return source.slice(start, end);
}

function count(haystack: string, needle: RegExp): number {
  return (haystack.match(needle) ?? []).length;
}

test('#840: the retired-pool restore orders the panel only when it is being revealed', () => {
  // `orderWindow:relativeTo:` re-inserts an ordered-out window into the
  // WindowServer screen list, so ordering a panel this call just hid showed it
  // with `revealed_first_frame == false` -- which made the next hold fail and
  // closed the flicker loop. Scoped to the restore path; the first-frame
  // REVEAL path orders unconditionally on purpose.
  const restore = fnBody(
    compositor,
    'fn show_retired_window_on_main(',
    'fn reveal_remote_window_after_first_frame_on_main('
  );
  assert.match(restore, /if label == panel_label && reveal \{\s*\n\s*order_below_anchor/);
  // Counting defeats "add a second, unguarded copy further down the fn",
  // which any adjacency-only regex misses.
  assert.equal(
    count(restore, /order_below_anchor\(/g),
    1,
    'the restore path must contain exactly one order_below_anchor call, inside the reveal gate'
  );
  assert.equal(
    count(restore, /order_chrome_above_panel\(/g),
    1,
    'the restore path must contain exactly one order_chrome_above_panel call, inside the reveal gate'
  );
});

test('#840: the non-terminal teardown arm routes a failed hold through the fallback decision', () => {
  const apply = fnBody(
    subscriber,
    'fn apply_teardown_decision(',
    'fn undisplayable_hold_fallback('
  );
  const holdArm = apply.slice(
    apply.indexOf('TeardownDecision::HoldForReplacement | TeardownDecision::HoldForTransientUnsubscribe'),
    apply.indexOf('TeardownDecision::RemoveWindow =>')
  );
  assert.ok(holdArm.length > 0, 'could not locate the hold arm');
  // Match the WHOLE argument expression: a mutant appending `&& false` (or any
  // other operator) to the openness check must not slip through on a prefix.
  assert.match(
    holdArm,
    /match undisplayable_hold_fallback\(crate::compositor::is_open_for_owner\(\s*\n?\s*owner_identity,\s*\n?\s*window_id,\s*\n?\s*\)\) \{/,
    'the hold arm must decide on the unmodified is_open_for_owner result'
  );
  assert.match(
    holdArm,
    /UndisplayableHoldFallback::Remove => \{\s*\n\s*remove_window_state\(/,
    'removal must be reachable only from the Remove arm'
  );
  assert.equal(
    count(holdArm, /crate::compositor::remove_window\(/g),
    1,
    'the hold arm must contain exactly one guarded remove_window call'
  );
});

test('#840: the fallback keeps an open-but-unrevealed window tracked', () => {
  assert.match(
    subscriber,
    /fn undisplayable_hold_fallback\(window_is_open: bool\) -> UndisplayableHoldFallback \{\s*\n\s*if window_is_open \{\s*\n\s*UndisplayableHoldFallback::KeepTracked/
  );
});

test('#840: the retired-pool reuse branch reveals from the layer, not from a literal', () => {
  // `apply_retired_reuse_reveal_state` has its own unit test AND the in-file
  // lifecycle model calls it -- so a mutant that leaves the helper intact and
  // instead hardcodes the CALL SITE (`..., false)`, restoring the pre-#840
  // unconditional reveal-gate reset) keeps every Rust test green. Pin the call
  // site itself, and pin that its result is what reaches the AppKit restore.
  const ensure = fnBody(compositor, 'pub fn ensure_window(', 'pub(crate) fn open_window_frames(');
  assert.match(
    ensure,
    /let reveal_now = apply_retired_reuse_reveal_state\(\s*\n?\s*&mut win_state\.revealed_first_frame,\s*\n?\s*win_state\.layer_has_content,\s*\n?\s*\);/,
    'the reuse branch must derive reveal_now from win_state.layer_has_content, not a literal'
  );
  assert.match(
    ensure,
    /show_retired_window_on_main\(\s*\n\s*app,\s*\n\s*&key,\s*\n\s*&mut win_state,\s*\n\s*passive_anchor,\s*\n\s*"ensure_window",\s*\n\s*reveal_now,\s*\n\s*\);/,
    'the reuse branch must pass reveal_now (not `false`) to show_retired_window_on_main'
  );
  // The bug was that a warm layer was thrown away on reuse. Reuse must not
  // clear it; only the pool-strip path, which genuinely empties the layer, may.
  assert.equal(
    count(ensure, /win_state\.layer_has_content = /g),
    0,
    'the reuse branch must not reset layer_has_content -- surviving reuse is the point'
  );
  const strip = fnBody(compositor, 'fn strip_retired_window_for_pool(', 'fn enforce_retired_pool_cap(');
  assert.match(
    strip,
    /window\.layer_has_content = false;/,
    'the only path that empties the layer must be the only path that clears the flag'
  );
});

test('#840: the teardown arm logs the hold OUTCOME, not its intent', () => {
  // The pre-fix line printed "the window keeps its last frame on screen"
  // BEFORE attempting the hold, so during the incident it asserted 187 holds
  // that had just failed -- a signal that cannot distinguish the two states it
  // is read for. Every surviving info! must sit after the attempt.
  const apply = fnBody(subscriber, 'fn apply_teardown_decision(', 'fn undisplayable_hold_fallback(');
  const beforeAttempt = apply.slice(0, apply.indexOf('hold_window_last_frame('));
  assert.equal(
    count(beforeAttempt, /log::info!\(/g),
    0,
    'nothing may claim a hold outcome at info! level before hold_window_last_frame is called'
  );
  assert.doesNotMatch(
    apply,
    /keeps its last frame on screen/,
    'the intent-phrased claim must not come back'
  );
});

test('#841: the shared slot claim is what consults the rate limiter', () => {
  // The limiter's pure helper has a unit test, but deleting the call site left
  // every test green -- so pin the early return inside the claim itself.
  const claim = fnBody(
    share,
    'fn claim_republish_reconcile_slot(',
    '/// Shared republish-and-restore-fps-cap path'
  );
  assert.match(
    claim,
    /if let Some\(wait\) = republish_reconcile_wait\([\s\S]{0,200}?\{[\s\S]{0,400}?return false;\s*\n\s*\}/,
    'a suppressed republish must refuse the slot, not fall through to publishing'
  );
  assert.match(
    claim,
    /last_by_window\.insert\(window_id, now\)/,
    'an allowed republish must record its timestamp so the next one is limited'
  );
});

test('#869: a suppressed resize republish still pushes the frame, and claims before bumping the intent', () => {
  // The #841 limiter made refusal the COMMON case on any ROI-bearing share.
  // The pump's pre-existing `continue` then dropped EVERY frame for the whole
  // 3s suppression window -- a viewer freeze, and a partial reintroduction of
  // the #714 letterbox-instead-of-drop rule. And bumping the intent before
  // claiming cancelled the in-flight quality republish that owned the slot,
  // without replacing it.
  const pump = fnBody(share, 'ResizeDecision::StableResize { width, height } => {', 'let mut current = pump_published');

  assert.doesNotMatch(
    pump,
    /\bcontinue;/,
    'the StableResize arm must never skip the push -- a refused or failed republish still owes the viewer a (letterboxed) frame'
  );

  const claimIdx = pump.indexOf('claim_republish_reconcile_slot(');
  const intentIdx = pump.indexOf('begin_republish_intent(');
  assert.ok(claimIdx >= 0, 'the StableResize arm must claim the shared slot itself');
  assert.ok(intentIdx >= 0, 'the StableResize arm must still bump the republish intent when it proceeds');
  assert.ok(
    claimIdx < intentIdx,
    'the slot must be claimed BEFORE begin_republish_intent -- bumping first cancels the in-flight quality republish that owns the slot (#869)'
  );

  // The claim is the CALLER's, and claiming twice is fatal rather than merely
  // redundant: the pump's claim stamps Instant::now(), nothing awaits before
  // the callee, so a second claim reads ~0ms against the 3s interval, refuses,
  // and returns false WITHOUT republishing -- the resize republish dies on
  // every stable resize. #841 and #869 each added the guard to their own half
  // of this path, and each lane's test pinned its own half, so both passed.
  const resizeFn = fnBody(
    share,
    'async fn republish_window_for_resize(',
    'async fn republish_window_for_resolution('
  );
  assert.equal(
    count(resizeFn, /claim_republish_reconcile_slot\(/g),
    0,
    'republish_window_for_resize must NOT claim the slot -- its only caller already did, and a second claim always refuses'
  );
});

// #841 (root cause, 2026-08-21): the quality reconcile path was limited but
// the RESIZE path was not, and the resize path was the ~3/sec republisher in
// the incident log. Both must claim the same per-window slot, and each must
// return WITHOUT republishing when the claim is refused. Counting defeats
// "add a second, unguarded republish further down the same fn".
for (const [label, signature, endMarker, refusalReturn] of [
  [
    'quality reconcile',
    'async fn republish_for_quality_reconcile(',
    'async fn republish_window_for_quality(',
    'return;',
  ],
] as const) {
  test(`#841: the ${label} republish path claims the shared rate-limit slot`, () => {
    const body = fnBody(share, signature, endMarker);
    assert.equal(
      count(body, /claim_republish_reconcile_slot\(/g),
      1,
      `the ${label} path must claim the slot exactly once`
    );
    // Match the FULL guard including its refusing return, so a mutant that
    // keeps the call but drops the early return cannot pass on a prefix.
    assert.match(
      body,
      new RegExp(
        `if !claim_republish_reconcile_slot\\(window_id, [\\s\\S]{0,40}?\\) \\{\\s*\\n\\s*${refusalReturn}\\s*\\n\\s*\\}`
      ),
      `a refused claim must ${refusalReturn.trim()} before the ${label} path republishes`
    );
    // ...and the claim must come BEFORE the publish it guards. Sliced past
    // the signature, whose own name would otherwise match the publish call.
    const afterSignature = body.slice(signature.length);
    const claimAt = afterSignature.indexOf('claim_republish_reconcile_slot(');
    const publishAt = afterSignature.search(/republish_window_(with_target|for_quality)\(/);
    assert.ok(claimAt >= 0, `could not locate the ${label} path's slot claim`);
    assert.ok(publishAt >= 0, `could not locate the ${label} path's republish call`);
    assert.ok(publishAt > claimAt, `the ${label} path must claim before it republishes`);
  });
}

test('#841: the resize republish target comes from the one size authority', () => {
  // `republish_target_for_resize` re-capped the already-capped FRAME size as
  // if it were the source backing size, bypassing the ROI memo -- a third
  // writer of the stream size. It must ask what every sibling path asks.
  const resize = fnBody(
    share,
    'async fn republish_window_for_resize(',
    'async fn republish_window_for_resolution('
  );
  assert.match(
    resize,
    /capture_config\.capture_size_for_resolution\(resolution\)/,
    'the resize path must derive its target from capture_size_for_resolution'
  );
  assert.doesNotMatch(
    share,
    /fn republish_target_for_resize\(/,
    'the frame-size-derived resize target must not come back'
  );
  assert.doesNotMatch(
    resize,
    /cap_capture_size_for_limits\(/,
    'the resize path must not re-cap a size itself; that is the authority\'s job'
  );
});

test('#841: the capture frame callback does not re-derive source geometry', () => {
  // The oscillator: `logical_* = frame_pixels / source_scale` on every
  // accepted frame. The long axis was an identity and the short axis was
  // rounding noise, which flipped the computed target 1654<->1652 forever.
  const capture = readFileSync(
    new URL('../src-tauri/src/capture.rs', import.meta.url),
    'utf8'
  );
  const observe = fnBody(
    capture,
    'fn observe_delivered_frame(',
    '#[derive(Debug, Clone, Copy, PartialEq)]'
  );
  for (const field of ['logical_width', 'logical_height', 'backing_scale']) {
    assert.doesNotMatch(
      observe,
      new RegExp(`self\\.${field}\\s*=`),
      `a delivered frame must never assign ${field} (#841)`
    );
  }
  assert.equal(
    count(capture, /layout\.logical_width\s*=/g),
    0,
    'nothing may write source geometry from a delivered frame'
  );
});
