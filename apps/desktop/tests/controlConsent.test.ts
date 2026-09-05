// Remote-control CONSENT flow (ask policy, the default). Source-grep wiring
// tests in the shareNotice.test.ts house pattern: the Rust unit tests in
// remote_control.rs (awaiting_consent_gate_parks_the_request_without_authorizing
// & co.) prove the gate/park/answer LOGIC given inputs; they do not prove the
// Request arm actually routes an `ask` request into the park path, that the
// revoke paths actually deny parked requests, that the panel + route + layout
// + command registration are wired, or that the controller actually extends
// its timeout on `awaitingConsent`. THIS file asserts that wiring (CLAUDE.md
// "Native window-lifecycle changes need a live-exercising test").
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { COMMANDS, EVENTS } from '../src/lib/ipc.ts';
import {
  REMOTE_CONTROL_CONSENT_TIMEOUT_MS,
  REMOTE_CONTROL_REQUEST_TIMEOUT_MS,
  remoteControlFeedbackIsNeutral,
  remoteControlFeedbackLabel,
  remoteControlStatusEffect
} from '../src/lib/remoteControlFeedback.ts';
import {
  REMOTE_CONTROL_POLICY_DESCRIPTION,
  REMOTE_CONTROL_POLICY_OPTIONS,
  REMOTE_CONTROL_POLICY_TITLE
} from '../src/lib/remoteControlPolicyCopy.ts';

const read = (rel: string) => readFileSync(new URL(rel, import.meta.url), 'utf8');
const remoteControlRs = read('../src-tauri/src/remote_control.rs');
const coreRs = read('../src-tauri/src/remote_control_core.rs');
const consentRs = read('../src-tauri/src/control_consent.rs');
const consentWindowsRs = read('../src-tauri/src/control_consent_windows.rs');
const lib = read('../src-tauri/src/lib.rs');
const sessionMac = read('../src-tauri/src/session/mod.rs');
const sessionCommands = read('../src-tauri/src/session/commands.rs');
const sessionStub = read('../src-tauri/src/session_stub.rs');
const autotestRs = read('../src-tauri/src/autotest.rs');
const layout = read('../src/routes/+layout.svelte');
const routeSvelte = read('../src/routes/control-consent/+page.svelte');
const routePageTs = read('../src/routes/control-consent/+page.ts');
const panelCloseGuard = read('../scripts/check-panel-close.mjs');
const surfaceRoute = read('../src/routes/compositor/surface/+page.svelte');
const headerComponent = read('../src/lib/components/RemoteWindowHeader.svelte');
const settings = read('../src/lib/components/Settings.svelte');
const sessionStore = read('../src/lib/stores/session.svelte.ts');
const meetingSession = read('../src/lib/meeting/meetingSession.svelte.ts');
const hoverTab = read('../src/routes/hover-tab/+page.svelte');
const webHarnessHeader = read('../../../web-harness/src/remoteWindowHeader.ts');
const webHarnessValidator = read('../../../web-harness/src/remoteControl.ts');
const webHarnessUi = read('../../../web-harness/src/remoteControlUi.ts');
const scenario = read('../scripts/remote-control-scenario.mjs');
const contracts = JSON.parse(read('../../../contracts/petal-contracts.json')) as {
  remoteControlMessages: Array<{ name: string; message: Record<string, unknown> }>;
};
const defaultCapability = JSON.parse(read('../src-tauri/capabilities/default.json')) as {
  windows?: string[];
};

test('the Request arm routes an `ask` request into the park path BEFORE the authorize tail', () => {
  const arm = remoteControlRs.slice(
    remoteControlRs.indexOf('RemoteControlType::Request => {'),
    remoteControlRs.indexOf('RemoteControlType::Release => {')
  );
  assert.match(arm, /let policy = state\.remote_control_policy\(\);/);
  assert.match(arm, /policy == RemoteControlPolicy::Ask/);
  // Idempotent re-request: an already-granted controller is never re-prompted.
  assert.match(arm, /&& !is_authorized\(message\.window_id, &message\.controller_id\)/);
  const parkIndex = arm.indexOf('park_consent_request(app, state, local_identity, message);');
  const grantIndex = arm.indexOf('complete_granted_request(app, state, local_identity, message, grant_token);');
  assert.ok(parkIndex > 0, 'park call not found in the Request arm');
  assert.ok(grantIndex > parkIndex, 'the authorize tail must come AFTER the park early-return');
  // Windows envelope validation still runs before the prompt.
  assert.ok(arm.indexOf('validate_host_request') < parkIndex, 'validate_host_request must precede the park');
});

test('parking never mints, prompts once, arms a 30 s deny timer keyed to the parked seq', () => {
  assert.match(remoteControlRs, /pub\(crate\) const CONSENT_TIMEOUT: Duration = Duration::from_secs\(30\);/);
  const park = remoteControlRs.slice(
    remoteControlRs.indexOf('fn park_consent_request('),
    remoteControlRs.indexOf('pub(crate) fn answer_consent(')
  );
  assert.match(park, /engine\.store_pending_request\(key, message\);/);
  assert.match(park, /status: "awaitingConsent"/);
  assert.match(park, /if already_pending \{\s*return;\s*\}/, 'a repeat request must not re-prompt');
  // P1: a repeat must KEEP the original parked message so the timer armed
  // for its seq still fires and denies; replacing it would orphan the entry.
  assert.match(park, /if !already_pending \{\s*engine\.store_pending_request\(key, message\);\s*\}/);
  assert.match(remoteControlRs, /fn repeat_request_while_pending_keeps_original_timer_and_denies\(\)/);
  assert.match(park, /kind: ControlConsentPromptKind::Control/);
  assert.match(park, /app\.emit\("control-consent-requested", payload\)/);
  assert.match(park, /tokio::time::sleep\(CONSENT_TIMEOUT\)\.await;/);
  assert.match(park, /pending_request_seq\(window_id, &controller_id\) != Some\(seq\)/, 'timer must be seq-keyed');
  assert.match(park, /RemoteControlReason::ConsentTimedOut/);
  assert.doesNotMatch(park, /authorize_shared|apply_request_gate_for_message/, 'parking must never authorize');
});

test('answer_consent re-checks the gate on Allow and reuses the auto authorize tail; Deny never grants', () => {
  const answer = remoteControlRs.slice(
    remoteControlRs.indexOf('pub(crate) fn answer_consent('),
    remoteControlRs.indexOf('fn deny_pending_requests_where(')
  );
  assert.match(answer, /take_pending_request\(window_id, controller_id\)/);
  assert.match(answer, /status: "denied"/);
  assert.match(answer, /reason: Some\(reason\)/);
  assert.match(answer, /if !state\.remote_control_allowed\(\)[\s\S]*requester_is_present_in_room\(state, controller_id\)/);
  assert.match(answer, /apply_request_gate_for_message\(&message, gate\)/);
  assert.match(answer, /complete_granted_request\(app, state, &local_identity, message, grant_token\);/);
  const denyBranch = answer.slice(answer.indexOf('if !approve {'), answer.indexOf('// Re-run the gate'));
  assert.doesNotMatch(denyBranch, /authorize|complete_granted_request/, 'deny branch must not authorize');
});

test('every revoke path denies parked requests so a prompt cannot outlive its share/controller/policy', () => {
  for (const fn of ['pub(crate) fn revoke_all(', 'pub(crate) fn revoke_window(', 'pub(crate) fn revoke_controller(']) {
    const start = remoteControlRs.indexOf(fn);
    assert.ok(start > 0, `${fn} not found`);
    const body = remoteControlRs.slice(start, start + 600);
    assert.match(body, /deny_pending_requests_where\(/, `${fn} must deny parked requests`);
  }
  // The engine half that the Rust unit test covers is what those call.
  assert.match(coreRs, /pub\(crate\) fn take_pending_requests_where\(/);
  assert.match(coreRs, /pub\(crate\) fn pending_request_seq\(/);
});

test('policy lives in BOTH session backends with the same boolean->policy mapping', () => {
  for (const [label, src] of [['session/mod.rs', sessionMac], ['session_stub.rs', sessionStub]] as const) {
    assert.match(src, /remote_control_policy: AtomicU8/, `${label} policy field`);
    assert.match(src, /remote_control_default_policy: AtomicU8/, `${label} default field`);
    assert.match(src, /RemoteControlPolicy::from_allowed\(allowed, self\.remote_control_default_policy\(\)\)/, `${label} legacy setter`);
    assert.match(src, /pub\(crate\) fn set_remote_control_policy\(&self, policy: RemoteControlPolicy\)/, `${label} setter`);
    // P3: a Settings change must not lift a pill-Off live gate; join seeds both.
    assert.match(src, /if self\.remote_control_policy\(\)\.allows_requests\(\) \|\| !policy\.allows_requests\(\)/, `${label} keeps live Off`);
    assert.match(src, /pub\(crate\) fn seed_remote_control_policy\(&self, policy: RemoteControlPolicy\)/, `${label} seed`);
  }
  assert.match(coreRs, /pub enum RemoteControlPolicy \{\s*Off,\s*#\[default\]\s*Ask,\s*Auto,\s*\}/);
  // join_room_command: additive policy field, legacy boolean maps true -> Ask, never Auto.
  for (const src of [sessionCommands, sessionStub]) {
    assert.match(src, /remote_control_policy: Option<RemoteControlPolicy>,/);
    assert.match(src, /RemoteControlPolicy::from_allowed\(remote_control_allowed, RemoteControlPolicy::Ask\)/);
  }
  // The autotest rig keeps the legacy cases auto-granting by seeding `auto` explicitly.
  assert.equal((autotestRs.match(/RemoteControlPolicy::Auto,/g) ?? []).length, 2, 'both autotest joins seed auto');
});

test('lib.rs creates the consent panel and registers the commands in BOTH invoke handlers', () => {
  assert.match(lib, /control_consent::create_control_consent_panel\(&handle\);/);
  assert.match(lib, /control_consent::control_consent_present,/);
  assert.match(lib, /control_consent::control_consent_dismiss,/);
  assert.equal((lib.match(/remote_control::remote_control_answer_consent,/g) ?? []).length, 2);
  assert.equal((lib.match(/remote_control::remote_control_answer_escalation,/g) ?? []).length, 1);
  assert.equal((lib.match(/session::set_remote_control_policy,/g) ?? []).length, 2);
  assert.equal((lib.match(/session::remote_control_policy,/g) ?? []).length, 2);
});

test('the consent panel is a singleton, non-activating, hidden/shown only -- never closed (CLAUDE.md crash class 2)', () => {
  assert.match(consentRs, /panel\.hide\(\);/);
  assert.match(consentRs, /window\.hide\(\)/);
  assert.doesNotMatch(consentRs, /\.close\(\)/, 'must never call .close() on the tauri_nspanel consent panel');
  assert.match(consentRs, /no_activate\(true\)/);
  assert.match(consentRs, /StyleMask::empty\(\)\.nonactivating_panel\(\)/);
  assert.match(consentRs, /can_become_key_window: false/);
  assert.match(consentRs, /set_ignore_cursor_events\(false\)/, 'Allow / Deny must be clickable');
  assert.match(consentRs, /crate::platform::on_main\(&app, "control_consent: present"/);
  assert.match(consentRs, /crate::platform::on_main\(&app, "control_consent: dismiss"/);
  assert.match(consentRs, /PanelLevel::Floating/, 'must float above the app being shared');
  assert.match(panelCloseGuard, /control_consent\.rs/, 'check-panel-close.mjs must scan the new panel module');
});

test('the control-consent route is a prerendered transparent overlay wired to the real commands and events', () => {
  assert.match(routePageTs, /export const prerender = true;/);
  assert.match(routePageTs, /export const ssr = false;/);
  assert.match(layout, /control-consent/);
  assert.match(routeSvelte, /listen<ControlConsentRequestedEvent>\(EVENTS\.controlConsentRequested,/);
  assert.match(routeSvelte, /listen<RemoteControlStatus>\(EVENTS\.remoteControlStatus,/);
  assert.match(routeSvelte, /invoke\(COMMANDS\.remoteControlAnswerConsent,\s*\{\s*windowId: req\.windowId,\s*controllerId: req\.controllerId,\s*approve\s*\}\)/);
  assert.match(routeSvelte, /invoke\(COMMANDS\.controlConsentPresent, \{ height \}\)/);
  // P2: re-measure when the queue/countdown changes the rendered copy.
  assert.match(routeSvelte, /queue\.length >= 0 && secondsLeft >= 0\) void reveal\(\);/);
  assert.match(routeSvelte, /new ResizeObserver\(/);
  assert.match(routeSvelte, /req\.kind === 'fullControlEscalation'/);
  assert.match(routeSvelte, /COMMANDS\.remoteControlAnswerEscalation/);
  assert.match(routeSvelte, /a\.kind === b\.kind/);
  assert.match(routeSvelte, /EVENTS\.shareControlModeChanged/);
  assert.match(routeSvelte, /requested full control of/);
  assert.match(routeSvelte, /never invokes a mode change/);
  assert.match(routeSvelte, /invoke\(COMMANDS\.controlConsentDismiss\)/);
  // Queue, never replace.
  assert.match(routeSvelte, /queue = \[\.\.\.queue, \{ \.\.\.payload/);
  assert.doesNotMatch(routeSvelte, /queue = \[entry\]/, 'a new request must not replace a pending one');
  assert.match(routeSvelte, /if \(existing >= 0\) return;/, 'duplicate prompt events must not extend their deadline');
  // Never truncate: the copy wraps inside a capped column; no nowrap, no text-overflow.
  assert.match(routeSvelte, /overflow-wrap: anywhere;/);
  assert.doesNotMatch(routeSvelte, /white-space:\s*nowrap|text-overflow/);
  assert.equal(COMMANDS.remoteControlAnswerConsent, 'remote_control_answer_consent');
  assert.equal(COMMANDS.remoteControlAnswerEscalation, 'remote_control_answer_escalation');
  assert.equal(COMMANDS.controlConsentPresent, 'control_consent_present');
  assert.equal(COMMANDS.controlConsentDismiss, 'control_consent_dismiss');
  assert.equal(COMMANDS.setRemoteControlPolicy, 'set_remote_control_policy');
  assert.equal(EVENTS.controlConsentRequested, 'control-consent-requested');
});

test('the consent panel is included in the Tauri capability scope', () => {
  assert.ok(
    defaultCapability.windows?.includes('control-consent'),
    'the control-consent webview needs event and command permissions'
  );
});

test('Windows escalation uses the same typed consent event and a revalidated 30-second host record', () => {
  const escalation = remoteControlRs.slice(
    remoteControlRs.indexOf('if message.reason == Some(RemoteControlReason::RequestEscalation)'),
    remoteControlRs.indexOf('let policy = state.remote_control_policy();')
  );
  assert.match(escalation, /requester_is_present_in_room/);
  assert.match(escalation, /is_authorized\(message\.window_id, &message\.controller_id\)/);
  assert.match(escalation, /park_escalation\(message\.window_id, &message\.controller_id\)/);
  assert.match(escalation, /kind: ControlConsentPromptKind::FullControlEscalation/);
  assert.match(escalation, /app\.emit\("control-consent-requested", payload\)/);
  assert.match(remoteControlRs, /const ESCALATION_TIMEOUT: Duration = Duration::from_secs\(30\)/);
  assert.match(remoteControlRs, /pub async fn remote_control_answer_escalation/);
  const answer = remoteControlRs.slice(remoteControlRs.indexOf('pub async fn remote_control_answer_escalation'));
  assert.match(answer, /take_escalation\(window_id, &controller_id\)/);
  assert.match(answer, /state\.active_share_frame\(window_id\)\.is_some\(\)/);
  assert.match(answer, /requester_is_present_in_room\(state, &controller_id\)/);
  assert.match(answer, /is_authorized\(window_id, &controller_id\)/);
  assert.match(answer, /set_share_control_mode_for_window/);
  assert.match(answer, /RemoteControlMode::FullControl/);
  assert.match(sessionStub, /set_share_control_mode_for_window/);
  assert.match(sessionStub, /share-control-mode-changed/);
  assert.match(routeSvelte, /if \(req\.kind === 'fullControlEscalation'\)/);
  assert.match(routeSvelte, /dropEscalationsForWindow/);
});

test('Windows routes consent through the dedicated non-activating singleton, not the hover tab', () => {
  assert.match(consentWindowsRs, /create_control_consent_panel/);
  assert.match(consentWindowsRs, /WebviewUrl::App\("control-consent\.html"/);
  assert.match(consentWindowsRs, /SetWindowPos[\s\S]*SWP_SHOWWINDOW[\s\S]*SWP_NOACTIVATE/);
  assert.match(consentWindowsRs, /always_on_top\(true\)/);
  assert.match(consentWindowsRs, /pub fn control_consent_present/);
  assert.match(consentWindowsRs, /pub async fn control_consent_dismiss/);
  assert.match(consentWindowsRs, /PRESENTATION_GENERATION/);
  assert.match(consentWindowsRs, /tokio::sync::oneshot::channel\(\)/);
  assert.match(consentWindowsRs, /set_ignore_cursor_events\(true\)/);
  assert.match(consentWindowsRs, /run_on_main_thread\(move \|/);
  assert.doesNotMatch(hoverTab, /controlConsentRequested|consentQueue|answerConsent/);
  assert.doesNotMatch(hoverTab, /Remote control request/);
});

test('controller side: awaitingConsent is neutral and extends the request timeout; denied is a warning', () => {
  assert.equal(remoteControlFeedbackLabel('awaitingConsent'), 'Waiting for approval');
  assert.equal(remoteControlFeedbackLabel('denied'), 'Control denied');
  assert.equal(remoteControlFeedbackIsNeutral('awaitingConsent'), true);
  assert.equal(remoteControlFeedbackIsNeutral('denied'), false);
  assert.equal(remoteControlStatusEffect('awaitingConsent'), 'feedback');
  assert.equal(remoteControlStatusEffect('denied'), 'feedback');
  assert.ok(REMOTE_CONTROL_CONSENT_TIMEOUT_MS > 30000, 'must cover the host 30 s consent window');
  assert.ok(REMOTE_CONTROL_CONSENT_TIMEOUT_MS > REMOTE_CONTROL_REQUEST_TIMEOUT_MS);
  assert.match(surfaceRoute, /status\.status === 'awaitingConsent' && remoteControlRequesting/);
  assert.match(surfaceRoute, /startRemoteControlTimeout\(REMOTE_CONTROL_CONSENT_TIMEOUT_MS, REMOTE_CONTROL_CONSENT_TIMEOUT_MESSAGE\)/);
  assert.match(headerComponent, /remoteControlFeedbackIsNeutral\(remoteControlStatus\)/);
  assert.match(headerComponent, /remoteControlRequesting && !remoteControlAwaitingConsent/);
  // web-harness parity.
  assert.match(webHarnessHeader, /case 'awaitingConsent':\s*return 'Waiting for approval';/);
  assert.match(webHarnessHeader, /case 'denied':\s*return 'Control denied';/);
  assert.match(webHarnessHeader, /\['requestUnavailable', 'awaitingConsent'\]\.includes\(/);
  assert.match(webHarnessValidator, /'awaitingConsent',\s*'denied'/);
  assert.match(webHarnessValidator, /'consentDenied', 'consentTimedOut'/);
  // P4: a deny clears the optimistic active record in the web client.
  assert.match(webHarnessUi, /message\.status === 'denied' \|\|/);
  // P5: native deny feedback auto-clears like other transient feedback.
  assert.match(surfaceRoute, /if \(status\.status === 'denied'\) \{[\s\S]*?\}, 3000\);/);
});

test('the contract fixture pins the two consent statuses', () => {
  const byName = new Map(contracts.remoteControlMessages.map((v) => [v.name, v.message]));
  assert.equal(byName.get('status-awaiting-consent')?.status, 'awaitingConsent');
  assert.equal(byName.get('status-awaiting-consent')?.grantToken, undefined);
  assert.equal(byName.get('status-denied')?.status, 'denied');
  assert.equal(byName.get('status-denied')?.reason, 'consentDenied');
});

test('Settings exposes the 3-way policy (default ask), the store migrates the old boolean, join seeds the policy', () => {
  assert.deepEqual(
    REMOTE_CONTROL_POLICY_OPTIONS.map((o) => o.value),
    ['ask', 'auto', 'off']
  );
  assert.match(settings, /remoteControlPolicy = 'ask',/);
  assert.match(settings, /onRemoteControlPolicyChange\?\.\(option\.value\)/);
  assert.match(settings, /name="remote-control-policy"/);
  assert.doesNotMatch(settings, /allowRemoteControlByDefault/, 'the boolean prop must be gone');
  assert.match(sessionStore, /remoteControlPolicy: 'ask',/);
  assert.match(sessionStore, /merged\.remoteControlPolicy = migrateRemoteControlPolicy\(parsed\);/);
  assert.match(sessionStore, /invoke\(COMMANDS\.setRemoteControlPolicy, \{ policy \}\)/);
  assert.match(meetingSession, /const remoteControlPolicy = session\.remoteControlPolicy;/);
});

test('Settings policy copy fits the 400px main window without truncation', () => {
  // Settings' tile: 400px window - page padding (2 x 16) - tile padding
  // (2 x 12) - radio (~14px) - gap (10px) leaves ~320px for a label. At the
  // tile's 13px/600 weight Manrope the widest glyph average is ~7.2px, so a
  // label must stay under ~44 characters to fit on ONE line; everything
  // longer is allowed to wrap (overflow-wrap: anywhere, never nowrap).
  const available = 400 - 2 * 16 - 2 * 12 - 14 - 10;
  const avgGlyph = 7.2;
  for (const option of REMOTE_CONTROL_POLICY_OPTIONS) {
    assert.ok(option.label.length * avgGlyph <= available, `label "${option.label}" must fit on one line`);
    assert.ok(option.hint.length > 0);
  }
  assert.ok(REMOTE_CONTROL_POLICY_TITLE.length * avgGlyph <= 400 - 2 * 16 - 2 * 12, 'title fits one line');
  assert.ok(REMOTE_CONTROL_POLICY_DESCRIPTION.length > 0);
  const policyCss = settings.slice(settings.indexOf('.policy-row {'), settings.indexOf('.policy-option-hint {') + 200);
  assert.doesNotMatch(policyCss, /white-space:\s*nowrap|text-overflow/);
  assert.match(policyCss, /overflow-wrap: anywhere;/);
});

test('the loopback scenario exercises both consent outcomes through the autotest socket', () => {
  assert.match(scenario, /cmd: 'remote-control-policy', policy: 'ask'/);
  assert.match(scenario, /cmd: 'remote-control-consent-answer'/);
  assert.match(scenario, /approve: true,/);
  assert.match(scenario, /approve: false,/);
  assert.match(scenario, /m\.status === 'awaitingConsent'/);
  assert.match(scenario, /m\.status === 'denied'/);
  assert.match(autotestRs, /"remote-control-consent-answer"/);
  assert.match(autotestRs, /"remote-control-policy"/);
  assert.match(remoteControlRs, /"pending": pending/);
});
