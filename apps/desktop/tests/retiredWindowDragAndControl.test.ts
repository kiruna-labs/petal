import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// #843: a remote window that is visible but currently in `CompositorState::retired`
// (mid-republish-storm reveal) silently refused both drag and remote-control
// entry, because both paths resolved through the OPEN-ONLY `resolve_open_window_key`
// instead of the retired-inclusive `resolve_window_key`.
//
// compositor.rs's own Rust unit tests (`owner_identity_for_window_843_resolves_through_the_retired_pool`,
// `resolve_open_window_key_excludes_retired_windows_but_resolve_window_key_does_not`)
// prove the resolver LOGIC is correct. This file proves the WIRING -- that the
// real command entry points actually call the fixed functions, in the right
// order -- which a Rust unit test cannot do here: `compositor_start_drag` needs
// a live `tauri::AppHandle` (no test-mode Tauri app fixture in this crate; see
// the comment on `share_pill_suppression_genuine_end_clears_even_for_a_key_outside_s_windows`
// in compositor.rs for the established rationale/pattern this file follows).
const compositorRs = readFileSync(new URL('../src-tauri/src/compositor.rs', import.meta.url), 'utf8');

/**
 * Return the source between `open` (a `{` or `(` index) and its MATCHING
 * close, skipping braces/parens that appear inside string literals or line
 * comments. Used instead of `indexOf` position comparisons: a flat "A appears
 * before B" check cannot tell "B is nested inside A's closure" from "B merely
 * appears later in the function", and legal Rust shadowing makes the latter
 * trivial to arrange while reintroducing the #843 race. An adversarial review
 * defeated the position-based version of this file exactly that way.
 */
function balancedFrom(source: string, open: number): string {
  const closer = source[open] === '{' ? '}' : ')';
  const opener = source[open];
  let depth = 0;
  let i = open;
  while (i < source.length) {
    const c = source[i];
    if (c === '"') {
      i += 1;
      while (i < source.length && source[i] !== '"') i += source[i] === '\\' ? 2 : 1;
    } else if (c === '/' && source[i + 1] === '/') {
      while (i < source.length && source[i] !== '\n') i += 1;
    } else if (c === opener) {
      depth += 1;
    } else if (c === closer) {
      depth -= 1;
      if (depth === 0) return source.slice(open, i + 1);
    }
    i += 1;
  }
  assert.fail(`unbalanced ${opener} starting at ${open}`);
}

/** Collapse all whitespace so an assertion cannot be defeated by reformatting
 *  the code across multiple lines (rustfmt, or a deliberate mutation). */
function flat(source: string): string {
  return source.replace(/\s+/g, ' ');
}

function extractFn(source: string, signature: string): string {
  const start = source.indexOf(signature);
  assert.ok(start >= 0, `could not find "${signature}" in compositor.rs`);
  // Slice to the next top-level `fn ` after this one's opening brace, a
  // generous-but-workable function-body bound for this file's flat style.
  const braceOpen = source.indexOf('{', start);
  assert.ok(braceOpen > start, `could not find the opening brace for "${signature}"`);
  const nextFn = source.indexOf('\nfn ', braceOpen);
  const nextPubFn = source.indexOf('\npub', braceOpen);
  const candidates = [nextFn, nextPubFn].filter((i) => i > 0);
  const end = candidates.length ? Math.min(...candidates) : source.length;
  return source.slice(start, end);
}

test('owner_identity_for_window resolves through the retired pool, not the open-only one', () => {
  const body = extractFn(compositorRs, 'pub(crate) fn owner_identity_for_window(');
  assert.match(
    body,
    /let key = resolve_window_key\(window_id, owner_identity\)\?;/,
    'must call the retired-inclusive resolve_window_key, not resolve_open_window_key -- ' +
      'this is exactly what let a mid-republish-storm control request fail with ' +
      '"remote window N is not open"'
  );
});

test('compositor_start_drag restores-then-drags in one main-thread hop instead of racing a separate lookup', () => {
  const body = extractFn(compositorRs, 'pub fn compositor_start_drag(');
  assert.match(
    body,
    /activate_window_then\(\s*&app,\s*window_id,\s*owner_identity\.as_deref\(\),\s*move \|_app, window\| \{/,
    'must delegate to activate_window_then, threading the drag-start into the SAME ' +
      'main-thread restore hop -- the old code called activate_window (async restore) ' +
      'and then separately re-resolved with resolve_open_window_key on the command ' +
      'thread, a race that silently dropped the drag for a retired window'
  );
  assert.match(
    body,
    /window\.start_dragging\(\)/,
    'the drag must actually start inside that closure'
  );
  // The old racy shape must be gone, not just supplemented. Matched against
  // whitespace-collapsed source: the single-line form of this regex silently
  // passed when the reintroduced call was split across lines.
  assert.doesNotMatch(
    flat(body),
    /resolve_open_window_key\s*\(/,
    'must not ALSO do a separate open-only re-resolve on the command thread -- that was the race'
  );
});

test('activate_window_then runs its restore, raise, AND the after_raise continuation on the same main-thread closure', () => {
  const body = extractFn(compositorRs, 'fn activate_window_then(');
  const onMainIndex = body.indexOf('crate::platform::on_main(');
  assert.ok(onMainIndex >= 0, 'must dispatch the restore via crate::platform::on_main');

  // Structural, not positional: take the balanced argument list of the
  // on_main(...) call, then the balanced body of the closure inside it, and
  // require the continuation to be called from WITHIN that body. A mutation
  // that moves `after_raise(...)` out of the closure (re-shadowing
  // `app_for_thread` so the literal text still appears later in the function)
  // reintroduces the #843 race, and defeated the previous indexOf version of
  // this assertion while staying green.
  const onMainArgs = balancedFrom(body, body.indexOf('(', onMainIndex));
  const closureBraceIndex = onMainArgs.indexOf('{', onMainArgs.indexOf('move ||'));
  assert.ok(closureBraceIndex > 0, 'on_main must be passed a `move ||` closure');
  const closureBody = balancedFrom(onMainArgs, closureBraceIndex);

  assert.match(
    flat(closureBody),
    /after_raise\s*\(\s*&app_for_thread\s*,\s*&window\s*\)\s*;/,
    'after_raise must be invoked INSIDE the on_main closure, not after on_main returns -- ' +
      'on_main is fire-and-forget (run_on_main_thread), so calling after_raise outside it ' +
      'would reintroduce the exact race this function exists to close'
  );

  // The raise (order_chrome_above_panel) must happen before after_raise, so a
  // caller's continuation always sees an already-raised/keyed panel. Both
  // indexes are taken WITHIN the closure body, so ordering here is real
  // control flow rather than mere textual position in the function.
  const raiseIndex = closureBody.indexOf('order_chrome_above_panel(&app_for_thread, &key_for_main);');
  const afterRaiseIndex = closureBody.indexOf('after_raise(&app_for_thread, &window);');
  assert.ok(raiseIndex >= 0, 'the closure must raise the panel');
  assert.ok(raiseIndex < afterRaiseIndex, 'raise must precede after_raise');
});

test('compositor_begin_resize restores-then-reads-geometry in one main-thread hop instead of racing a separate lookup (#855)', () => {
  const body = extractFn(compositorRs, 'pub async fn compositor_begin_resize(');

  assert.match(
    flat(body),
    /activate_window_then\s*\(\s*&app\s*,\s*window_id\s*,\s*owner_identity\.as_deref\(\)\s*,\s*move \|_app, window\| \{/,
    'must delegate to activate_window_then, threading the resize-begin into the SAME ' +
      'main-thread restore hop -- the old code called activate_window (async restore) ' +
      'and then separately re-resolved with resolve_open_window_key on the command ' +
      'thread, a race that silently dropped the resize for a retired window'
  );

  // Structural, not positional: pull the balanced argument list of the
  // activate_window_then(...) call, then the balanced body of the `move
  // |_app, window|` continuation inside it, and require the resolve +
  // gesture-flag writes + geometry reads + channel send to all be NESTED
  // inside that continuation body -- not merely present somewhere later in
  // the function. A single-line regex or an indexOf ordering check can both
  // be defeated by re-shadowing/reformatting (as happened during #843's
  // review); balanced-brace extraction cannot.
  const callIndex = body.indexOf('activate_window_then(');
  assert.ok(callIndex >= 0, 'must call activate_window_then');
  const callArgs = balancedFrom(body, body.indexOf('(', callIndex));
  const closureBraceIndex = callArgs.indexOf('{', callArgs.indexOf('move |_app, window|'));
  assert.ok(closureBraceIndex > 0, 'activate_window_then must be passed a `move |_app, window|` continuation');
  const closureBody = balancedFrom(callArgs, closureBraceIndex);
  const flatClosure = flat(closureBody);

  assert.match(
    flatClosure,
    /resolve_open_window_key\(window_id, owner_for_continuation\.as_deref\(\)\)/,
    'the key resolve must happen INSIDE the continuation, after the restore has landed'
  );
  assert.match(
    flatClosure,
    /cancel_programmatic_resize_for_user_gesture\(window\)/,
    'the gesture-cancel must happen INSIDE the continuation'
  );
  assert.match(
    flatClosure,
    /user_resize_active\.store\(true, Ordering::Relaxed\)/,
    'the gesture-active flag must be set INSIDE the continuation'
  );
  assert.match(
    flatClosure,
    /window\s*\.\s*outer_position\(\)/,
    'position must be read INSIDE the continuation (on the main thread, after restore)'
  );
  assert.match(
    flatClosure,
    /window\s*\.\s*outer_size\(\)/,
    'size must be read INSIDE the continuation (on the main thread, after restore)'
  );
  assert.match(
    flatClosure,
    /tx\.send\(result\)/,
    'the result must be sent back to the async command from INSIDE the continuation'
  );

  // The old racy shape must be gone from the command's OWN body (i.e. on the
  // command/async-fn thread, outside the continuation), not just supplemented.
  const outsideClosure = flat(body).replace(flatClosure, '');
  assert.doesNotMatch(
    outsideClosure,
    /\.\s*outer_position\(\)|\.\s*outer_size\(\)/,
    'geometry must not ALSO be read outside the continuation on the command thread -- that was the race'
  );
  // Adversarial-review hardening (#855): the geometry check alone was
  // empirically defeated by reinserting an open-only resolve + `?` early
  // return on the command thread before `rx.await` -- the original bug in
  // effect, with all closure assertions still matching. Forbid the racy
  // resolve and the gesture-flag store ANYWHERE outside the continuation.
  assert.doesNotMatch(
    outsideClosure,
    /resolve_open_window_key\s*\(/,
    'the open-only resolve must not appear outside the continuation -- a command-thread ' +
      'resolve races the queued restore, which is the exact #855 bug'
  );
  assert.doesNotMatch(
    outsideClosure,
    /user_resize_active\s*\.\s*store\s*\(/,
    'the gesture-active flag must not be set outside the continuation'
  );

  assert.match(
    flat(body),
    /match rx\.await \{/,
    'the async command must await the oneshot receiver rather than returning synchronously'
  );
});

test('activate_window (no continuation) is implemented in terms of activate_window_then, not a parallel copy', () => {
  const body = extractFn(compositorRs, 'fn activate_window(app: &AppHandle, window_id: u32, owner_identity: Option<&str>) {');
  assert.match(
    body,
    /activate_window_then\(app, window_id, owner_identity, \|_app, _window\| \{\}\);/,
    'activate_window must delegate to activate_window_then with a no-op continuation -- ' +
      'a second, separately-maintained copy of the restore logic is exactly how this class ' +
      'of resolver-mismatch bug happens again'
  );
});
