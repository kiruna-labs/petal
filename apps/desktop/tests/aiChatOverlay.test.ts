import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// #844: the receiver-side AI-chat transcript/typed-input UI moved from an
// in-webview popover under RemoteWindowHeader.svelte's header strip (always
// covered by the native video NSView layered in front, per that popover's
// own removed doc comment) into a SEPARATE native overlay window
// (routes/compositor/ai-chat/+page.svelte), created/positioned/retired the
// same way the existing control/pointer overlays are
// (src-tauri/src/compositor.rs's `create_ai_chat_overlay`).
//
// This crate has no `tauri::test` mock-builder harness (no live AppHandle to
// drive a real reveal/retire cycle here), so -- following the established
// pattern in retiredWindowDragAndControl.test.ts -- these assert against the
// real compiled source of compositor.rs: that the wiring exists INSIDE the
// real functions (brace-matched, not just present anywhere in the file), not
// merely that the logic is *possible* somewhere.
const compositorRs = readFileSync(new URL('../src-tauri/src/compositor.rs', import.meta.url), 'utf8');
const remoteWindowHeader = readFileSync(
  new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
  'utf8'
);
const surfaceRoute = readFileSync(
  new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
  'utf8'
);
const ipcSource = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');
const appkitRs = readFileSync(new URL('../src-tauri/src/platform/appkit.rs', import.meta.url), 'utf8');

/** Same brace-matcher as retiredWindowDragAndControl.test.ts: returns the
 *  source between `open` (a `{` or `(` index) and its MATCHING close,
 *  skipping braces/parens inside string literals or line comments. */
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

/** Collapse whitespace so an assertion can't be defeated by reformatting. */
function flat(source: string): string {
  return source.replace(/\s+/g, ' ');
}

function extractFn(source: string, signature: string): string {
  const start = source.indexOf(signature);
  assert.ok(start >= 0, `could not find "${signature}" in compositor.rs`);
  const braceOpen = source.indexOf('{', start);
  assert.ok(braceOpen > start, `could not find the opening brace for "${signature}"`);
  const nextFn = source.indexOf('\nfn ', braceOpen);
  const nextPubFn = source.indexOf('\npub', braceOpen);
  const candidates = [nextFn, nextPubFn].filter((i) => i > 0);
  const end = candidates.length ? Math.min(...candidates) : source.length;
  return source.slice(start, end);
}

/** Precise (brace-matched, not next-top-level-`fn`-bounded) extraction for a
 *  plain JS/TS function -- `extractFn` above bounds itself by Rust's `\nfn `/
 *  `\npub` markers, which don't exist in a .svelte <script> block. */
function extractJsFn(source: string, signature: string): string {
  const start = source.indexOf(signature);
  assert.ok(start >= 0, `could not find "${signature}"`);
  const braceOpen = source.indexOf('{', start);
  assert.ok(braceOpen > start, `could not find the opening brace for "${signature}"`);
  return balancedFrom(source, braceOpen);
}

test('the ai-chat overlay label/route helpers exist and are distinct from control/pointer/panel', () => {
  assert.match(compositorRs, /fn ai_chat_label_for_key\(key: &RemoteWindowKey\) -> String \{/);
  assert.match(compositorRs, /"remote-window-ai-chat-\{\}"/);
  assert.match(compositorRs, /fn ai_chat_route_url\(window_id: u32, owner_identity: &str\) -> String \{/);
});

test('the ai-chat overlay is a NONACTIVATING NSPanel, not a plain create_chrome_webview child', () => {
  // #844 adversarial-review FIX 1: a plain child WebviewWindow (what
  // control/pointer use via create_chrome_webview) cannot take keyboard
  // focus while the app is inactive, and the only way to key one (tao's
  // set_focus()) activates the whole app. The overlay must instead be a
  // PanelBuilder<_, AiChatOverlayPanel> built with can_become_key_window,
  // the nonactivating style mask, AND no_activate(true) -- dropping any one
  // of the three reopens either the activation-steal or the can't-type bug.
  const body = extractFn(compositorRs, 'fn create_ai_chat_overlay(');
  const flatBody = flat(body);
  assert.match(
    flatBody,
    /panel!\(AiChatOverlayPanel \{\s*config: \{\s*can_become_key_window: true,/,
    'the panel type must declare can_become_key_window: true'
  );
  assert.match(
    flatBody,
    /PanelBuilder::<_, AiChatOverlayPanel>::new\(app, &label\)/,
    'must build through PanelBuilder::<_, AiChatOverlayPanel>, not a plain WebviewWindowBuilder'
  );
  assert.match(
    flatBody,
    /\.no_activate\(true\)/,
    'must set no_activate(true) at build time'
  );
  assert.match(
    flatBody,
    /\.style_mask\(tauri_nspanel::StyleMask::empty\(\)\.nonactivating_panel\(\)\)/,
    'must set the nonactivating panel style mask'
  );
  assert.doesNotMatch(
    flatBody,
    /create_chrome_webview\(/,
    'must NOT go through create_chrome_webview (that builds a plain, activating child WebviewWindow)'
  );

  // And the real creation call site (ensure_window) must actually invoke it,
  // alongside control/pointer -- a helper nothing calls is dead code.
  const ensureWindowBody = extractFn(compositorRs, 'pub fn ensure_window(');
  assert.match(
    flat(ensureWindowBody),
    /create_control_overlay\([^)]*\);\s*create_pointer_overlay\([^)]*\);\s*create_ai_chat_overlay\(/,
    'ensure_window must create the ai-chat overlay alongside control/pointer when a new remote ' +
      'window is built'
  );
});

test('the AiChatOverlayPanel type is declared locally, inside create_ai_chat_overlay -- not at module scope', () => {
  // tauri_panel!'s macro expansion brings its own `use` imports into the
  // ENCLOSING scope. RemoteWindowPanel is already declared at module scope
  // in this file; a second module-scope tauri_panel! block for
  // AiChatOverlayPanel collides with it (E0252 "define_class defined
  // multiple times" etc -- caught by cargo check, not by this test, but
  // this pins the fix so it can't silently regress into the same mistake).
  const moduleLevelBlocks = (
    compositorRs.match(/^tauri_panel! \{/gm) ?? []
  ).length;
  assert.equal(
    moduleLevelBlocks,
    1,
    'exactly one module-scope tauri_panel! block may exist in this file (RemoteWindowPanel\'s) -- ' +
      'AiChatOverlayPanel must be declared inside its own function instead'
  );
  const body = extractFn(compositorRs, 'fn create_ai_chat_overlay(');
  assert.match(
    flat(body),
    /use tauri_nspanel::tauri_panel;\s*tauri_panel! \{\s*panel!\(AiChatOverlayPanel \{/,
    'AiChatOverlayPanel must be declared inside create_ai_chat_overlay, with its own local import'
  );
});

test('show_retired_window_on_main only reveals the ai-chat overlay when BOTH reveal and the disclosure flag are true', () => {
  const body = extractFn(compositorRs, 'fn show_retired_window_on_main(');
  const flatBody = flat(body);

  // The flag must be read from win_state before the per-label loop -- not
  // hardcoded, not read from something else.
  assert.match(
    flatBody,
    /let ai_chat_overlay_open = win_state\.ai_chat_overlay_open;/,
    'must read the persisted disclosure flag from win_state'
  );

  // The show/hide decision for this one label must differ from the other
  // three (which follow `reveal` alone) -- structurally, inside an
  // if/else keyed on `label == ai_chat_label`. balancedFrom's line-comment
  // skip relies on real newlines, so it must run on the UNFLATTENED body.
  //
  // #844 adversarial-review FIX 3: an EARLIER version of this assertion used
  // `assert.match(..., /reveal && ai_chat_overlay_open/)` -- a plain
  // substring match. The review mutation-tested it by appending `|| true`
  // to the real condition (`reveal && ai_chat_overlay_open || true`,
  // reintroducing exactly the resurrect-a-closed-overlay bug this test
  // exists to prevent) and the substring regex still matched, because the
  // mutated text still CONTAINS the substring "reveal && ai_chat_overlay_
  // open" -- it just has more appended after it. Extracting the branch body
  // and asserting EXACT equality closes that gap.
  const shouldShowIndex = body.indexOf('let should_show = if label == ai_chat_label {');
  assert.ok(shouldShowIndex >= 0, 'must branch the show/hide decision specifically for the ai-chat label');
  const ifBranchBody = balancedFrom(body, body.indexOf('{', shouldShowIndex));
  const ifBranchCondition = flat(ifBranchBody).replace(/^\{\s*/, '').replace(/\s*\}$/, '').trim();
  assert.equal(
    ifBranchCondition,
    'reveal && ai_chat_overlay_open',
    'the ai-chat overlay branch of should_show must be EXACTLY `reveal && ai_chat_overlay_open` -- ' +
      'not reveal alone (that would resurrect a closed overlay on every republish glitch), not the ' +
      'flag alone (that would show it on a window that is not even being revealed), and not that ' +
      'expression with anything else appended (e.g. `|| true`, which defeats a substring match while ' +
      'reintroducing the exact bug this condition exists to prevent)'
  );
  // The `else` arm (every other chrome window's condition) gets the same
  // rigor -- a `|| true` there would make retire/reveal always run, hiding
  // nothing.
  const elseBraceIndex = body.indexOf('{', body.indexOf('} else {', shouldShowIndex) + 2);
  const elseBranchBody = balancedFrom(body, elseBraceIndex);
  const elseBranchCondition = flat(elseBranchBody).replace(/^\{\s*/, '').replace(/\s*\}$/, '').trim();
  assert.equal(elseBranchCondition, 'reveal', 'every other chrome window must follow `reveal` alone, exactly');

  // #844 second adversarial re-review: pinning should_show's DEFINITION
  // (above) is not enough on its own -- a mutant at the CONSUMER site
  // (`if should_show || true { ... }`) escapes that check entirely, since
  // it never touches the `let should_show = ...` line at all. Pin the
  // consumer's condition and both branch bodies with the same exact-equality
  // rigor.
  const consumerIfIndex = body.indexOf('if should_show {', elseBraceIndex);
  assert.ok(consumerIfIndex >= 0, 'must find the should_show consumer site');
  const consumerConditionText = flat(body.slice(consumerIfIndex + 'if '.length, body.indexOf('{', consumerIfIndex))).trim();
  assert.equal(
    consumerConditionText,
    'should_show',
    'the consumer site must test EXACTLY `should_show`, nothing appended'
  );
  const consumerIfBody = flat(balancedFrom(body, body.indexOf('{', consumerIfIndex)));
  assert.equal(consumerIfBody, '{ let _ = win.show(); }', 'the true branch must show and do nothing else');
  const consumerElseBraceIndex = body.indexOf('{', body.indexOf('else', consumerIfIndex));
  const consumerElseBody = flat(balancedFrom(body, consumerElseBraceIndex));
  assert.equal(consumerElseBody, '{ let _ = win.hide(); }', 'the false branch must hide and do nothing else');

  // It must also come back UN-blanked: strip_retired_window_for_pool can
  // have sent it to about:blank while evicted from the warm pool, so a
  // retired-window reveal must re-navigate it, the same way control/pointer
  // already do.
  assert.match(
    flatBody,
    /if label == ai_chat_label \{[^}]*ai_chat_route_url\(window_id, &win_state\.owner_identity\)/,
    'must re-navigate the ai-chat overlay to its real route on reveal, in case it was stripped to about:blank'
  );
});

test('reveal_remote_window_after_first_frame_on_main also respects the disclosure flag, not an assumed-always-false invariant', () => {
  const body = extractFn(compositorRs, 'fn reveal_remote_window_after_first_frame_on_main(');
  const flatBody = flat(body);
  assert.match(
    flatBody,
    /let ai_chat_overlay_open = with_state\(\|s\| \{? ?s\.windows ?\.get\(key\) ?\.map\(\|w\| w\.ai_chat_overlay_open\) ?\.unwrap_or\(false\),? ?\}?\);/,
    'must read the persisted flag from state rather than assuming it is always false at first-frame reveal'
  );
  // #844 adversarial-review FIX 3 (same exact-equality treatment applied to
  // show_retired_window_on_main's should_show above): extract the `if`
  // statement's condition precisely and assert it EQUALS `ai_chat_overlay_open`,
  // rather than a substring match that a `|| true` appended to the condition
  // could still satisfy.
  const ifIndex = body.indexOf('if ai_chat_overlay_open');
  assert.ok(ifIndex >= 0, 'must gate ai-chat overlay visibility on the flag on this reveal path too');
  const conditionStart = ifIndex + 'if '.length;
  const conditionEnd = body.indexOf('{', conditionStart);
  const condition = flat(body.slice(conditionStart, conditionEnd)).trim();
  assert.equal(
    condition,
    'ai_chat_overlay_open',
    'the condition must be EXACTLY the flag, not the flag with anything appended (e.g. `|| true`)'
  );
  const ifBody = flat(balancedFrom(body, conditionEnd));
  assert.equal(
    ifBody,
    '{ let _ = win.show(); }',
    'the true branch must show the overlay and do nothing else'
  );
  const elseIndex = body.indexOf('else', conditionEnd);
  const elseBraceIndex = body.indexOf('{', elseIndex);
  const elseBody = flat(balancedFrom(body, elseBraceIndex));
  assert.equal(
    elseBody,
    '{ let _ = win.hide(); }',
    'the false branch must hide the overlay and do nothing else'
  );
});

test('ensure_window resets the disclosure flag only for a genuinely NEW share on a reused key', () => {
  const body = extractFn(compositorRs, 'pub fn ensure_window(');
  // balancedFrom must run on the UNFLATTENED body (its line-comment skip
  // needs real newlines); flatten only the extracted result for the regex.
  const reusedIndex = body.indexOf('if let Some(mut win_state) = reused {');
  assert.ok(reusedIndex >= 0, 'must find the retired-reuse branch');
  const reusedBlock = flat(balancedFrom(body, body.indexOf('{', reusedIndex)));
  assert.match(
    reusedBlock,
    /win_state\.ai_chat_overlay_open = false;/,
    'a fresh share on a reused key must not inherit a stale disclosure from an unrelated earlier session'
  );
});

test('order_chrome_above_panel and remove_window both cover the ai-chat overlay alongside control/pointer', () => {
  const orderBody = extractFn(compositorRs, 'fn order_chrome_above_panel(');
  assert.match(
    flat(orderBody),
    /for label in \[\s*control_label_for_key\(key\),\s*pointer_label_for_key\(key\),\s*ai_chat_label_for_key\(key\),\s*\]/,
    'order_chrome_above_panel must re-assert z-order for the ai-chat overlay too, or a shown overlay ' +
      'can end up behind the panel'
  );

  const removeBody = extractFn(compositorRs, 'pub fn remove_window(');
  assert.match(
    flat(removeBody),
    /for label in \[\s*control_label_for_key\(&key_for_main\),\s*pointer_label_for_key\(&key_for_main\),\s*ai_chat_label_for_key\(&key_for_main\),\s*panel_label_for_key\(&key_for_main\),\s*\]/,
    'remove_window must hide (never destroy) the ai-chat overlay in the same pass as control/pointer/panel'
  );
});

test('compositor_set_ai_chat_overlay_open persists the disclosure flag before touching AppKit', () => {
  const body = extractFn(compositorRs, 'pub fn compositor_set_ai_chat_overlay_open(');
  const flatBody = flat(body);
  assert.match(
    flatBody,
    /with_state\(\|s\| \{ if let Some\(win\) = s\.windows\.get_mut\(&key\) \{ win\.ai_chat_overlay_open = open; \} \}\);/,
    'the command must persist the new open/closed state into CompositorWindow before showing/hiding, ' +
      'so a retire that happens moments later still knows whether to bring the overlay back'
  );
});

test('compositor_set_ai_chat_overlay_open never calls tao set_focus() -- keys via raise_panel_and_make_key instead', () => {
  // #844 adversarial-review FIX 1, mirroring ai_chat/panel.rs's own #738
  // structural guard (ai_chat_panel_present_never_shows_or_keys_the_panel):
  // tao's WebviewWindow::set_focus() wraps
  // [NSApp activateIgnoringOtherApps:YES] (see the #678 comment on
  // raise_panel_only, elsewhere in this file) -- calling it here would
  // activate Petal app-wide and could surface the gallery over whatever app
  // the user was actually working in, on every badge click.
  const body = extractFn(compositorRs, 'pub fn compositor_set_ai_chat_overlay_open(');
  const flatBody = flat(body);
  assert.doesNotMatch(
    flatBody,
    /overlay\.set_focus\(\)/,
    'must never call tao set_focus() on the overlay -- that is the activation-stealing path'
  );
  assert.match(
    flatBody,
    /raise_panel_and_make_key\(&overlay\)/,
    'must key the overlay via the #356 raw-makeKeyWindow recipe instead'
  );
});

test('compositor_set_ai_chat_overlay_open broadcasts the new state so the header can stay in sync', () => {
  // #844 adversarial-review FIX 2: without this, the header's own
  // optimistic local toggle could desync from the real overlay (the
  // overlay's own Escape-to-close bypasses the badge entirely, and a
  // retired-window restore reloads the header fresh). Broadcasting makes
  // Rust the single source of truth.
  const body = extractFn(compositorRs, 'pub fn compositor_set_ai_chat_overlay_open(');
  const flatBody = flat(body);
  const mutateIndex = flatBody.indexOf('win.ai_chat_overlay_open = open;');
  const emitIndex = flatBody.indexOf('emit_ai_chat_overlay_open_changed(&app, &key, open);');
  assert.ok(mutateIndex >= 0, 'must persist the flag');
  assert.ok(emitIndex >= 0, 'must emit the change');
  assert.ok(mutateIndex < emitIndex, 'must persist BEFORE emitting, so a listener that immediately re-queries sees the new value');
});

test('compositor_ai_chat_overlay_is_open exists for the header to seed real state on mount', () => {
  const body = extractFn(compositorRs, 'pub fn compositor_ai_chat_overlay_is_open(');
  assert.match(
    flat(body),
    /\.map\(\|w\| w\.ai_chat_overlay_open\)\s*\.unwrap_or\(false\)/,
    'must read the real persisted flag, defaulting to false only when the window cannot be resolved'
  );
});

test('add_child_window_above / remove_child_window exist as raw addChildWindow:ordered:/removeChildWindow: calls', () => {
  // #844 second adversarial re-review (DRAG-FOLLOW): as an independent
  // nonactivating panel, the overlay no longer auto-follows the remote
  // window panel during a native drag the way control/pointer do via
  // WebviewWindowBuilder::parent() (addChildWindow). These raw AppKit
  // helpers restore that by construction.
  const attachBody = extractFn(appkitRs, 'pub fn add_child_window_above(');
  assert.match(
    flat(attachBody),
    /msg_send!\[parent_ns, addChildWindow: child_ns, ordered: 1isize\];/,
    'must call addChildWindow:ordered: with NSWindowAbove (1)'
  );
  const detachBody = extractFn(appkitRs, 'pub fn remove_child_window(');
  assert.match(
    flat(detachBody),
    /msg_send!\[parent_ns, removeChildWindow: child_ns\];/,
    'must call removeChildWindow:'
  );
});

test('attach_ai_chat_overlay / detach_ai_chat_overlay wrap the raw appkit calls with the ai-chat label lookups', () => {
  const attachBody = extractFn(compositorRs, 'fn attach_ai_chat_overlay(');
  assert.match(flat(attachBody), /panel_label_for_key\(key\)/);
  assert.match(flat(attachBody), /ai_chat_label_for_key\(key\)/);
  assert.match(flat(attachBody), /add_child_window_above\(&panel, &overlay\)/);

  const detachBody = extractFn(compositorRs, 'fn detach_ai_chat_overlay(');
  assert.match(flat(detachBody), /panel_label_for_key\(key\)/);
  assert.match(flat(detachBody), /ai_chat_label_for_key\(key\)/);
  assert.match(flat(detachBody), /remove_child_window\(&panel, &overlay\)/);
});

test('compositor_set_ai_chat_overlay_open attaches AFTER show() and detaches BEFORE hide()', () => {
  // Attaching before show() risks addChildWindow's `ordered:` parameter
  // revealing the overlay through an uncontrolled second path (same effect
  // order_above_panel/order_below_anchor's own doc comments document for
  // orderWindow:relativeTo:) -- so ordering here is load-bearing, not
  // stylistic, and worth pinning precisely rather than just "both calls
  // exist somewhere in the function."
  const body = extractFn(compositorRs, 'pub fn compositor_set_ai_chat_overlay_open(');
  const showIndex = body.indexOf('let _ = overlay.show();');
  const attachIndex = body.indexOf('attach_ai_chat_overlay(&app, &key);');
  assert.ok(showIndex >= 0 && attachIndex >= 0, 'must find both the show() call and the attach call');
  assert.ok(attachIndex > showIndex, 'must attach AFTER show(), not before');

  const detachIndex = body.indexOf('detach_ai_chat_overlay(&app, &key);');
  const hideIndex = body.indexOf('let _ = overlay.hide();');
  assert.ok(detachIndex >= 0 && hideIndex >= 0, 'must find both the detach call and the hide() call');
  assert.ok(detachIndex < hideIndex, 'must detach BEFORE hide(), not after');
});

test('show_retired_window_on_main and reveal_remote_window_after_first_frame_on_main keep attach/detach in sync with visibility', () => {
  const retiredBody = flat(extractFn(compositorRs, 'fn show_retired_window_on_main('));
  assert.match(
    retiredBody,
    /if label == ai_chat_label \{ if should_show \{ attach_ai_chat_overlay\(app, key\); \} else \{ detach_ai_chat_overlay\(app, key\); \} \}/,
    'must attach exactly when should_show is true, detach exactly when false'
  );

  const revealBody = flat(extractFn(compositorRs, 'fn reveal_remote_window_after_first_frame_on_main('));
  assert.match(
    revealBody,
    /if ai_chat_overlay_open \{ attach_ai_chat_overlay\(app, key\); \} else \{ detach_ai_chat_overlay\(app, key\); \}/,
    'must attach exactly when the disclosure flag is true, detach exactly when false'
  );
});

test('remove_window detaches the ai-chat overlay before hiding it in the teardown loop', () => {
  const body = flat(extractFn(compositorRs, 'pub fn remove_window('));
  assert.match(
    body,
    /if label == ai_chat_label_for_key\(&key_for_main\) \{ detach_ai_chat_overlay\(&app_main, &key_for_main\); \} let _ = win\.hide\(\);/,
    'a window retired while the overlay was still OPEN (attached) must be detached before the ' +
      'teardown loop hides it -- this covers a genuine unpublish/teardown, not just the toggle ' +
      'command or the hold-path/retire-reveal cycle the other tests cover'
  );
});

test('RemoteWindowHeader.svelte no longer contains the dead in-webview ai-chat popover', () => {
  assert.doesNotMatch(
    remoteWindowHeader,
    /ai-chat-remote-panel/,
    'the transcript/PTT/input popover markup+CSS must be fully removed -- it moved to ' +
      'routes/compositor/ai-chat/+page.svelte, and dormant code does not merge (CLAUDE.md)'
  );
  assert.doesNotMatch(remoteWindowHeader, /onSendAiChatText/, 'the old send-text prop/wiring must be gone too');
  // The in-strip PTT button (fc4d8ec0) must survive untouched.
  assert.match(remoteWindowHeader, /class="ai-chat-header-ptt"/);
});

test('aiChatOverlayOpen is a PROP, not local state -- Rust stays the single source of truth', () => {
  // #844 adversarial-review FIX 2: an earlier version kept
  // `let aiChatOverlayOpen = $state(false)` and mutated it optimistically on
  // click, which desynced from the real overlay (the overlay's own
  // Escape-to-close bypassed it entirely, and a retired-window restore
  // reloads this whole header webview with a hardcoded `false`). It must
  // now be a prop the host (surface/+page.svelte) feeds from Rust.
  assert.match(
    remoteWindowHeader,
    /onToggleAiChatOverlay\?: \(open: boolean\) => void;/,
    'must declare the overlay-toggle prop'
  );
  assert.match(
    remoteWindowHeader,
    /aiChatOverlayOpen\?: boolean;/,
    'must declare aiChatOverlayOpen as a PROP in the Props interface'
  );
  assert.doesNotMatch(
    remoteWindowHeader,
    /let aiChatOverlayOpen = \$state/,
    'must NOT keep a local, independently-mutable copy of the open state'
  );

  const fnBody = extractJsFn(remoteWindowHeader, 'function toggleAiChatOverlay() {');
  assert.equal(
    flat(fnBody).replace(/^\{\s*/, '').replace(/\s*\}$/, '').trim(),
    'onToggleAiChatOverlay?.(!aiChatOverlayOpen);',
    'toggling must ONLY request the new state from the host -- never assign aiChatOverlayOpen itself'
  );
  assert.match(
    remoteWindowHeader,
    /onclick={toggleAiChatOverlay}/,
    'the badge button must call toggleAiChatOverlay'
  );

  // The dead-session auto-close effect must request the close through the
  // callback too, never by assigning the (now-prop) aiChatOverlayOpen.
  const effectBody = remoteWindowHeader.slice(
    remoteWindowHeader.indexOf('if (aiChatActive) return;'),
    remoteWindowHeader.indexOf('function toggleAiChatOverlay()')
  );
  assert.match(
    flat(effectBody),
    /if \(aiChatOverlayOpen\) onToggleAiChatOverlay\?\.\(false\);/,
    'a dead session must request the overlay close via the callback, not a direct assignment'
  );
  assert.doesNotMatch(flat(effectBody), /aiChatOverlayOpen = /, 'must never assign the prop directly');
});

test('the surface route wires the header toggle straight to the new compositor command', () => {
  const fnBody = extractJsFn(surfaceRoute, 'function onToggleAiChatOverlay(open: boolean) {');
  assert.match(
    flat(fnBody),
    /invoke\(COMMANDS\.compositorSetAiChatOverlayOpen, \{ windowId, ownerIdentity, open \}\)/,
    'must invoke compositor_set_ai_chat_overlay_open with the new open state'
  );
  assert.match(surfaceRoute, /\{onToggleAiChatOverlay\}/, 'must actually pass the callback to RemoteWindowHeader');
});

test('the surface route seeds aiChatOverlayOpen on mount AND keeps it live via the change event', () => {
  // #844 adversarial-review FIX 2, the other half: this webview can mount
  // long after the overlay was toggled (a retired-window restore reloads it
  // fresh), so waiting for the next event alone would show a stale `false`
  // until something happened to toggle it again.
  assert.match(
    surfaceRoute,
    /let aiChatOverlayOpen = \$state\(false\);/,
    'must own the state that gets fed down to the header as a prop'
  );

  const seedBody = extractJsFn(surfaceRoute, 'async function refreshAiChatOverlayOpen() {');
  assert.match(
    flat(seedBody),
    /aiChatOverlayOpen = await invoke<boolean>\(COMMANDS\.compositorAiChatOverlayIsOpen, \{\s*windowId,\s*ownerIdentity\s*\}\);/,
    'must ask Rust for the REAL current state on mount'
  );
  assert.match(surfaceRoute, /void refreshAiChatOverlayOpen\(\);/, 'must actually call the seed function on mount');

  const listenerBody = surfaceRoute.slice(
    surfaceRoute.indexOf('listen<AiChatOverlayOpenChangedEvent>'),
    surfaceRoute.indexOf('unlistenAiChatOverlayOpenChanged = unlisten;')
  );
  assert.match(listenerBody, /if \(event\.payload\.windowId !== windowId\) return;/);
  assert.match(listenerBody, /if \(event\.payload\.ownerIdentity !== ownerIdentity\) return;/);
  assert.match(listenerBody, /aiChatOverlayOpen = event\.payload\.open;/);
  // Torn down with the panel, like every other listener on this page.
  assert.match(surfaceRoute, /unlistenAiChatOverlayOpenChanged\?\.\(\)/);
  assert.match(surfaceRoute, /unlistenAiChatOverlayOpenChanged = undefined;/);

  assert.match(surfaceRoute, /\{aiChatOverlayOpen\}/, 'must actually pass the live state down to RemoteWindowHeader');
});

test('the ai-chat-overlay-open-changed event and its payload are registered in ipc.ts', () => {
  assert.match(ipcSource, /aiChatOverlayOpenChanged: 'ai-chat-overlay-open-changed',/);
  assert.match(ipcSource, /export interface AiChatOverlayOpenChangedEvent \{/);
  assert.match(ipcSource, /\[EVENTS\.aiChatOverlayOpenChanged\]: AiChatOverlayOpenChangedEvent;/);
  assert.match(ipcSource, /compositorAiChatOverlayIsOpen: 'compositor_ai_chat_overlay_is_open',/);
});
