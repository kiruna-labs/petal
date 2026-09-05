// #679: source-grep tests for the top-center "<Name> is sharing a window"
// notice. Per CLAUDE.md's "Native window-lifecycle changes need a
// live-exercising test, not just unit tests" -- the Rust unit tests in
// compositor.rs (share_pill_suppression_*) prove the suppression LOGIC is
// right given inputs; they do NOT prove `start_compositor_feed`'s
// `TrackSubscribed` arm actually calls it, or that the frontend route
// actually wires the emitted event to the real "Bring to foreground"
// command. THIS file is the one that matters (house pattern, see
// remoteWindowHeader.test.ts / windowsWindowSharing.test.ts): it asserts the
// real wiring, not just that the isolated helpers are correct.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { COMMANDS, EVENTS } from '../src/lib/ipc.ts';

const subscriber = readFileSync(
  new URL('../src-tauri/src/transport/subscriber.rs', import.meta.url),
  'utf8'
);
const compositor = readFileSync(new URL('../src-tauri/src/compositor.rs', import.meta.url), 'utf8');
const shareNoticeRs = readFileSync(new URL('../src-tauri/src/share_notice.rs', import.meta.url), 'utf8');
const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const layout = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
const sharedNoticeSvelte = readFileSync(
  new URL('../src/routes/share-notice/+page.svelte', import.meta.url),
  'utf8'
);
const sharedNoticePageTs = readFileSync(
  new URL('../src/routes/share-notice/+page.ts', import.meta.url),
  'utf8'
);
const toastSvelte = readFileSync(
  new URL('../../../shared/ui/components/Toast.svelte', import.meta.url),
  'utf8'
);

test('the compositor feed actually emits remote-share-started -- not just a payload helper that nothing calls', () => {
  const feedFn = subscriber.slice(subscriber.indexOf('pub(crate) fn start_compositor_feed'));
  const trackSubscribedArm = feedFn.slice(0, feedFn.indexOf('RoomEvent::TrackUnsubscribed'));

  assert.match(
    trackSubscribedArm,
    /crate::compositor::ensure_window\(/,
    'the pill wiring must live in the same TrackSubscribed arm that opens the window'
  );
  // The suppression check must run AFTER ensure_window (so `already_open`
  // reflects the state BEFORE this subscription, matching the existing
  // `already_open` republish log line) and BEFORE emitting.
  const ensureWindowIndex = trackSubscribedArm.indexOf('crate::compositor::ensure_window(');
  const suppressionIndex = trackSubscribedArm.indexOf(
    'crate::compositor::consume_share_started_pill_suppression('
  );
  const emitIndex = trackSubscribedArm.indexOf('crate::share_notice::emit_remote_share_started(');
  assert.ok(ensureWindowIndex >= 0, 'ensure_window call not found');
  assert.ok(suppressionIndex > ensureWindowIndex, 'suppression check must run after ensure_window');
  assert.ok(emitIndex > suppressionIndex, 'the emitter must run after the suppression check');

  // Never for an already-open window (republish/quality-switch).
  assert.match(trackSubscribedArm, /if !already_open \{/, 'the emit must be gated on !already_open');

  // Uses global `tauri::Emitter::emit`, not `emit_to` -- the documented
  // repeat bug in this codebase (Tauri 2's emit_to never matches a plain
  // listen()).
  assert.match(
    shareNoticeRs,
    /tauri::Emitter::emit\(app, "remote-share-started", payload\)/,
    'must use global emit, not emit_to'
  );
  // NB: the doc comment above deliberately mentions `emit_to(...)` (to
  // explain why it's avoided), so this checks the actual EMIT CALL LINE
  // specifically, not "the string emit_to never appears in the file".
  const emitCallLine = shareNoticeRs
    .split('\n')
    .find((line) => line.includes('tauri::Emitter::emit(app, "remote-share-started"'));
  assert.ok(emitCallLine, 'the emit call line was not found');
  assert.doesNotMatch(
    emitCallLine!,
    /emit_to\(/,
    'the actual emit call must never use emit_to (Tauri 2 emit_to does not match listen())'
  );
});

test('suppression is keyed off RemoveWindowReason, not a plain "is currently open" check', () => {
  // The naive gate this issue explicitly rejected.
  assert.match(
    compositor,
    /fn record_share_pill_suppression_for_remove_reason\(\s*key: &RemoteWindowKey,\s*reason: RemoveWindowReason,?\s*\)/
  );
  assert.match(
    compositor,
    /pub fn consume_share_started_pill_suppression\(owner_identity: &str, window_id: u32\) -> bool/
  );
  // remove_window must call the recorder for EVERY teardown, not just some.
  const removeWindowFn = compositor.slice(
    compositor.indexOf('pub fn remove_window('),
    compositor.indexOf('pub fn remove_window(') + 2000
  );
  assert.match(
    removeWindowFn,
    /record_share_pill_suppression_for_remove_reason\(&key, reason\);/,
    'remove_window must record suppression for every teardown reason'
  );
  // #679 review finding: the classification call must run BEFORE the
  // `s.windows.remove` early return, not after. `s.windows.remove` returns
  // None whenever the key is already retired (e.g. a prior ManualHide) --
  // if classification ran after that early return, a later GENUINE end for
  // that same already-retired key would skip classification entirely, and a
  // stale transport-side suppression would never clear (silently eating the
  // pill on a real stop-and-restart). This is the ordering bug an
  // adversarial review caught before merge; the Rust unit test
  // `share_pill_suppression_genuine_end_clears_even_for_a_key_outside_s_windows`
  // proves the classifier itself is order-independent, but only THIS
  // assertion proves `remove_window` actually calls it before, not after,
  // the early return -- ordering is the whole bug, so a test that only
  // checks the call exists would pass on a broken implementation.
  const classifyIndex = removeWindowFn.indexOf(
    'record_share_pill_suppression_for_remove_reason(&key, reason);'
  );
  const earlyReturnIndex = removeWindowFn.indexOf('let Some(removed) = removed else {');
  assert.ok(classifyIndex >= 0, 'classification call not found in remove_window');
  assert.ok(earlyReturnIndex >= 0, 'early return not found in remove_window');
  assert.ok(
    classifyIndex < earlyReturnIndex,
    'record_share_pill_suppression_for_remove_reason must run BEFORE the early return, not after'
  );
  // remove_all_windows (LeaveRoom) must clear every suppression entry, not
  // rely on its own per-key loop -- that loop only ever visits keys still in
  // s.windows, so an already-retired key's stale suppression would survive
  // the room boundary otherwise.
  const removeAllWindowsFn = compositor.slice(
    compositor.indexOf('pub fn remove_all_windows('),
    compositor.indexOf('pub fn remove_all_windows(') + 1200
  );
  assert.match(
    removeAllWindowsFn,
    /suppressed_reshare_pill\.clear\(\)/,
    'remove_all_windows must clear the whole suppression set, including retired-key entries its own loop never visits'
  );
  // Transport-side reasons suppress; genuine-end reasons clear.
  const recorderFn = compositor.slice(
    compositor.indexOf('fn record_share_pill_suppression_for_remove_reason'),
    compositor.indexOf('fn record_share_pill_suppression_for_remove_reason') + 1200
  );
  for (const transportSide of [
    'RemoveWindowReason::ParticipantDisconnected',
    'RemoveWindowReason::NoFrameWatchdog',
    'RemoveWindowReason::ManualHide',
    'RemoveWindowReason::ReconciledUnrecoverable'
  ]) {
    assert.ok(
      recorderFn.includes(transportSide),
      `${transportSide} must be classified in the suppression recorder`
    );
  }
  for (const genuineEnd of [
    'RemoveWindowReason::TrackUnsubscribed',
    'RemoveWindowReason::TrackUnpublished',
    'RemoveWindowReason::ReconciledPublicationGone',
    'RemoveWindowReason::LeaveRoom'
  ]) {
    assert.ok(recorderFn.includes(genuineEnd), `${genuineEnd} must be classified in the suppression recorder`);
  }
});

test('the pill panel is a singleton, hidden/shown only -- never closed (CLAUDE.md crash class 2)', () => {
  assert.match(shareNoticeRs, /window\.hide\(\)/);
  assert.doesNotMatch(
    shareNoticeRs,
    /window\.close\(\)|\.close\(\)/,
    'must never call .close() on the tauri_nspanel share-notice panel'
  );
  assert.match(shareNoticeRs, /panel\.hide\(\);/);
  assert.match(shareNoticeRs, /no_activate\(true\)/, 'the panel must never activate the app (focus-stealing)');
  assert.match(
    shareNoticeRs,
    /StyleMask::empty\(\)\.nonactivating_panel\(\)/,
    'must use the nonactivating_panel style mask, same recipe as create_hover_tab'
  );
  assert.match(
    shareNoticeRs,
    /set_ignore_cursor_events\(false\)/,
    'the pill has a clickable "Bring to foreground" link -- must NOT be click-through'
  );
});

test('lib.rs actually creates the panel and registers both commands', () => {
  assert.match(lib, /share_notice::create_share_notice_panel\(&handle\);/);
  assert.match(lib, /share_notice::share_notice_present,/);
  assert.match(lib, /share_notice::share_notice_dismiss,/);
});

test('the share-notice route is treated as a transparent overlay panel by the root layout', () => {
  assert.match(layout, /share-notice/);
});

test('the share-notice route prerenders (adapter-static needs a static file for WebviewUrl::App)', () => {
  assert.match(sharedNoticePageTs, /export const prerender = true;/);
  assert.match(sharedNoticePageTs, /export const ssr = false;/);
});

test('"Bring to foreground" calls compositor_activate_window with BOTH windowId and ownerIdentity', () => {
  // The menubar's own equivalent caller omits ownerIdentity (#678, a known
  // sibling bug this issue is explicit about NOT fixing here) -- this route
  // must not repeat it: two participants can share the same CGWindowID, so
  // omitting ownerIdentity risks activating the wrong participant's window.
  assert.match(
    sharedNoticeSvelte,
    /invoke\(COMMANDS\.compositorActivateWindow,\s*\{\s*windowId:\s*current\.windowId,\s*ownerIdentity:\s*current\.ownerIdentity\s*\}\)/
  );
});

test('the pill shows the latest remote share and auto-dismisses four seconds after it', () => {
  assert.match(sharedNoticeSvelte, /listen<RemoteShareStartedEvent>\(EVENTS\.remoteShareStarted,/);
  assert.match(sharedNoticeSvelte, /const AUTO_DISMISS_MS = 4000;/);
  assert.match(sharedNoticeSvelte, /showNow\(event\.payload\)/);
  assert.match(
    sharedNoticeSvelte,
    /setTimeout\(\(\) => void dismiss\(generation\), AUTO_DISMISS_MS\)/
  );
  assert.doesNotMatch(
    sharedNoticeSvelte,
    /let queue|function enqueue/,
    'new shares must replace the visible notice so the action always targets the latest window'
  );
});

test('the pill reuses the shared Toast component instead of a bespoke pill', () => {
  assert.match(sharedNoticeSvelte, /import Toast from '@petal\/shared\/ui\/components\/Toast\.svelte';/);
  assert.match(sharedNoticeSvelte, /actionLabel="Bring to foreground"/);
});

test('the share notice matches the drawing-pill reference without changing ordinary toasts', () => {
  assert.match(
    sharedNoticeSvelte,
    /\.share-notice-host :global\(\.pill\)[\s\S]*?background: var\(--surface-raised\);[\s\S]*?var\(--id-lilac\)/
  );
  assert.match(sharedNoticeSvelte, /\.share-notice-host :global\(\.icon\)[\s\S]*?display: none;/);
  assert.match(
    sharedNoticeSvelte,
    /\.share-notice-host :global\(\.action\)[\s\S]*?background: var\(--id-lilac\);[\s\S]*?color: var\(--bg-base\);/
  );
  assert.match(
    toastSvelte,
    /\.action::after[\s\S]*?width: 100%;[\s\S]*?height: 40px;/,
    'the compact visible action still needs a 40px interaction target'
  );
});

test('#679 review finding: the panel width budget is coupled to the real Toast message cap, not just its own hardcoded literal', () => {
  // share_notice.rs's own Rust unit test
  // (share_notice_width_budget_covers_the_longest_single_line_row) proves
  // SHARE_NOTICE_WIDTH covers its OWN hardcoded `message_cap = 360.0`
  // literal -- it catches someone shrinking SHARE_NOTICE_WIDTH, but NOT
  // someone widening Toast's real CSS cap out from under that literal
  // without updating it (the panel would then be too narrow for a single
  // line and the message would wrap sooner than the budget assumed). This
  // couples the two by asserting Toast's real CSS still matches the literal
  // share_notice.rs's budget comment cites.
  assert.match(
    toastSvelte,
    /max-width:\s*min\(360px,\s*calc\(100vw - 64px\)\)/,
    "Toast.svelte's message max-width changed -- update share_notice.rs's SHARE_NOTICE_WIDTH " +
      'budget comment and its message_cap literal (and this assertion) to match, or the panel ' +
      'can be too narrow for a single-line message'
  );
});

test('the IPC registry carries the new event and commands', () => {
  assert.equal(EVENTS.remoteShareStarted, 'remote-share-started');
  assert.equal(COMMANDS.shareNoticePresent, 'share_notice_present');
  assert.equal(COMMANDS.shareNoticeDismiss, 'share_notice_dismiss');
});
