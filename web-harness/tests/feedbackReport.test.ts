import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  buildFeedbackAttachment,
  buildFeedbackDiagnostics,
  buildScrubbedLogTail,
  FEEDBACK_DIAGNOSTICS_FILENAME,
  FEEDBACK_DIAGNOSTICS_TYPE,
  FEEDBACK_LOG_SECTION_HEADING,
  FEEDBACK_MAX_ATTACHMENT_BYTES,
  FeedbackReportController,
  isValidUserDispatchPublicKey,
  submitFeedbackReport,
  userDispatchAdapter,
  type FeedbackAdapter,
  type FeedbackRuntimeState,
} from '../src/feedbackReport.ts';
import type { FeedbackEmailStorage } from '@petal/shared/logic/feedbackEmail';
import { HARNESS_FEEDBACK_EMAIL_STORAGE_KEY } from '../src/constants.ts';
import { inviteLinkForCredential } from '../src/controls.ts';
import { registerMeetingAliases, SensitiveStringRegistry } from '../src/sensitiveStrings.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import { sessionLogCollector } from '../src/ui/sessionLogCollector.ts';

const validKey = 'pk_abcdefghijk_123456';
const validEmail = 'riley@example.org';
const connected: FeedbackRuntimeState = { connected: true, sharing: false, screenSharing: false };

const indexHtml = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const styleSource = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
const feedbackReportSource = readFileSync(new URL('../src/feedbackReport.ts', import.meta.url), 'utf8');
const tileLayoutSource = readFileSync(new URL('../src/tileLayout.ts', import.meta.url), 'utf8');

/** The static `.topbar-right` markup, before tileLayout.ts inserts the picker. */
function topbarRightMarkup(): string {
  const start = indexHtml.indexOf('<div class="topbar-right">');
  assert.notEqual(start, -1, 'missing the meeting topbar-right container');
  const end = indexHtml.indexOf('<div id="tiles"', start);
  assert.notEqual(end, -1, 'topbar-right is expected to precede the tile grid');
  return indexHtml.slice(start, end);
}

function elementMarkup(html: string, id: string, tag: string): string {
  const idAt = html.indexOf(`id="${id}"`);
  assert.notEqual(idAt, -1, `missing #${id}`);
  const start = html.lastIndexOf(`<${tag}`, idAt);
  const end = html.indexOf(`</${tag}>`, idAt);
  assert.ok(start !== -1 && end !== -1, `#${id} is not a well-formed <${tag}>`);
  return html.slice(start, end);
}

function cssBlock(source: string, selector: string): string {
  const marker = `${selector} {`;
  const start = source.indexOf(marker);
  assert.notEqual(start, -1, `missing CSS block for ${selector}`);
  const bodyStart = start + marker.length;
  const end = source.indexOf('}', bodyStart);
  assert.notEqual(end, -1, `unterminated CSS block for ${selector}`);
  return source.slice(bodyStart, end);
}

function recordingAdapter() {
  const calls: Parameters<FeedbackAdapter['submit']>[] = [];
  const adapter: FeedbackAdapter = {
    async submit(...args) {
      calls.push(args);
    },
  };
  return { adapter, calls };
}

function fakeElement<T extends object>(initial: T) {
  const listeners = new Map<string, Array<(event: Event) => void>>();
  // #895: the controller now toggles a `blocked` class (aria-disabled swap,
  // not the `disabled` property) -- a minimal classList so that call doesn't
  // throw against these fakes.
  const classes = new Set<string>();
  return Object.assign(initial, {
    classList: {
      toggle(name: string, force?: boolean) {
        const shouldAdd = force ?? !classes.has(name);
        if (shouldAdd) classes.add(name);
        else classes.delete(name);
        return shouldAdd;
      },
      contains(name: string) {
        return classes.has(name);
      },
    },
    addEventListener(type: string, listener: (event: Event) => void) {
      listeners.set(type, [...(listeners.get(type) ?? []), listener]);
    },
    dispatch(type: string, event: Event = new Event(type)) {
      for (const listener of listeners.get(type) ?? []) listener(event);
    },
  });
}

/** The `.some-class { ... }`-wrapping `<div>` block, matched the same way
 * `elementMarkup` matches a tag by id -- used for the #895 tooltip cell,
 * which has no id of its own. */
function divBlock(html: string, className: string): string {
  const marker = `class="${className}"`;
  const at = html.indexOf(marker);
  assert.notEqual(at, -1, `missing div.${className}`);
  const start = html.lastIndexOf('<div', at);
  const end = html.indexOf('</div>', at);
  assert.ok(start !== -1 && end !== -1, `div.${className} is not a well-formed <div>`);
  return html.slice(start, end + '</div>'.length);
}

test('public key validation accepts only the UserDispatch public pk_ contract', () => {
  assert.equal(isValidUserDispatchPublicKey(validKey), true);
  assert.equal(isValidUserDispatchPublicKey('sk_secret-value-123456'), false);
  assert.equal(isValidUserDispatchPublicKey('pk_short'), false);
  assert.equal(isValidUserDispatchPublicKey(undefined), false);
});

test('missing key removes both UI triggers and does not initialize a provider client', () => {
  const homeTrigger = { hidden: false } as HTMLButtonElement;
  const meetingTrigger = { hidden: false } as HTMLButtonElement;
  let removed = false;
  const controller = new FeedbackReportController({
    publicKey: undefined,
    dom: {
      homeTrigger,
      meetingTrigger,
      dialog: { remove: () => { removed = true; } } as unknown as HTMLDialogElement,
      form: {} as HTMLFormElement,
      message: {} as HTMLTextAreaElement,
      email: {} as HTMLInputElement,
      emailError: {} as HTMLElement,
      consent: {} as HTMLInputElement,
      submit: {} as HTMLButtonElement,
      cancel: {} as HTMLButtonElement,
      status: {} as HTMLElement,
      shareReason: {} as HTMLElement,
    },
    getState: () => connected,
    registry: new SensitiveStringRegistry(),
    adapter: { async submit() { throw new Error('adapter must not be called'); } },
  });
  controller.install();
  assert.equal(homeTrigger.hidden, true);
  assert.equal(meetingTrigger.hidden, true);
  assert.equal(removed, true);
});

test('#895: home feedback works while disconnected, and share intent marks both controls aria-disabled+blocked (not disabled) with an aria reason', () => {
  let state: FeedbackRuntimeState = { ...connected, connected: false };
  const homeTrigger = fakeElement({ hidden: true, disabled: false, attributes: new Map<string, string>(), focused: false,
    setAttribute(name: string, value: string) { this.attributes.set(name, value); }, focus() { this.focused = true; } }) as unknown as HTMLButtonElement;
  const meetingTrigger = fakeElement({ hidden: true, disabled: false, attributes: new Map<string, string>(),
    setAttribute(name: string, value: string) { this.attributes.set(name, value); }, focus() {} }) as unknown as HTMLButtonElement;
  const dialog = fakeElement({ open: false, removed: false, showModal() { this.open = true; }, close() { this.open = false; }, remove() { this.removed = true; } }) as unknown as HTMLDialogElement;
  const message = fakeElement({ value: '', focus() {} }) as unknown as HTMLTextAreaElement;
  const email = fakeElement({ value: '', setAttribute() {} }) as unknown as HTMLInputElement;
  const emailError = { hidden: true, textContent: '' } as HTMLElement;
  const consent = fakeElement({ checked: false }) as unknown as HTMLInputElement;
  const submit = fakeElement({ disabled: false }) as unknown as HTMLButtonElement;
  const cancel = fakeElement({}) as unknown as HTMLButtonElement;
  const form = fakeElement({}) as unknown as HTMLFormElement;
  const status = { textContent: '' } as HTMLElement;
  const shareReason = { id: 'feedback-share-reason', hidden: true } as HTMLElement;
  const controller = new FeedbackReportController({
    publicKey: validKey,
    dom: { homeTrigger, meetingTrigger, dialog, form, message, email, emailError, consent, submit, cancel, status, shareReason },
    getState: () => state,
    registry: new SensitiveStringRegistry(),
    adapter: { async submit() {} },
    storage: null,
  });

  controller.install();
  assert.equal(homeTrigger.hidden, false);
  assert.equal(meetingTrigger.hidden, false);
  assert.equal(homeTrigger.disabled, false);
  (homeTrigger as unknown as { dispatch(type: string): void }).dispatch('click');
  assert.equal(dialog.open, true);
  controller.onDisconnect();

  // #786: the in-meeting bug-report trigger opens the same dialog.
  (meetingTrigger as unknown as { dispatch(type: string): void }).dispatch('click');
  assert.equal(dialog.open, true);

  controller.onShareStartIntent();
  assert.equal(dialog.open, false);
  // #895: never the `disabled` property -- a real disabled button can't fire
  // hover/focus, so it could never reveal the reason tooltip. aria-disabled
  // + the `blocked` class carry the blocked state instead, and the control
  // stays genuinely focusable/hoverable/clickable at the DOM level.
  assert.equal(homeTrigger.disabled, false);
  assert.equal(meetingTrigger.disabled, false);
  assert.equal((homeTrigger as unknown as { classList: { contains(n: string): boolean } }).classList.contains('blocked'), true);
  assert.equal((meetingTrigger as unknown as { classList: { contains(n: string): boolean } }).classList.contains('blocked'), true);
  assert.equal((homeTrigger as unknown as { attributes: Map<string, string> }).attributes.get('aria-disabled'), 'true');
  assert.equal((meetingTrigger as unknown as { attributes: Map<string, string> }).attributes.get('aria-disabled'), 'true');
  assert.equal(shareReason.hidden, false);
  assert.equal((homeTrigger as unknown as { attributes: Map<string, string> }).attributes.get('aria-describedby'), 'feedback-share-reason');
  // Both triggers point at the same (now sr-only) explanation, and neither
  // can reopen the dialog while the share is live -- even though the click
  // handler DOES fire (the button was never actually disabled), open()'s own
  // isShareActive() guard (feedbackReport.ts:286) refuses it. This is the
  // real safety net the aria-disabled swap depends on.
  assert.equal((meetingTrigger as unknown as { attributes: Map<string, string> }).attributes.get('aria-describedby'), 'feedback-share-reason');
  (meetingTrigger as unknown as { dispatch(type: string): void }).dispatch('click');
  assert.equal(dialog.open, false);

  state = { ...state, sharing: false, screenSharing: false };
  controller.onShareEnded();
  assert.equal(homeTrigger.disabled, false);
  assert.equal(meetingTrigger.disabled, false);
  assert.equal((meetingTrigger as unknown as { classList: { contains(n: string): boolean } }).classList.contains('blocked'), false);
  assert.equal((meetingTrigger as unknown as { attributes: Map<string, string> }).attributes.get('aria-disabled'), 'false');
  assert.equal(shareReason.hidden, true);
  assert.equal((meetingTrigger as unknown as { attributes: Map<string, string> }).attributes.get('aria-describedby'), '');
});

test('#786: the in-meeting trigger is a bug-report affordance sitting immediately right of the spotlight/layout toggle', () => {
  const topbar = topbarRightMarkup();
  const trigger = elementMarkup(topbar, 'feedback-meeting-trigger', 'button');

  // Live DOM order is [layout picker][bug report][conn state][count chip]:
  // the picker is inserted as topbarRight's first child, and the static
  // markup below puts the trigger ahead of the two chips.
  assert.match(tileLayoutSource, /topbarRight\.insertBefore\(picker, topbarRight\.firstChild\)/);
  const triggerAt = topbar.indexOf('id="feedback-meeting-trigger"');
  const connAt = topbar.indexOf('id="conn-state"');
  const countAt = topbar.indexOf('class="count-chip"');
  assert.ok(triggerAt !== -1 && connAt !== -1 && countAt !== -1, 'topbar-right lost one of its members');
  assert.ok(triggerAt < connAt && connAt < countAt, 'the bug-report trigger must lead the topbar-right chips');

  // Icon + accessible name, and a name that reads as a bug report rather
  // than the old generic "Feedback" pill.
  assert.match(trigger, /aria-label="Report a bug"/);
  assert.match(trigger, /title="Report a bug"/);
  assert.match(trigger, /<svg[\s\S]*<\/svg>/);
  assert.match(trigger, /aria-haspopup="dialog"/);
  assert.match(trigger, /aria-controls="feedback-dialog"/);
});

test('#786: an absent public key still removes the trigger — no CSS display can defeat the hidden attribute', () => {
  const trigger = elementMarkup(topbarRightMarkup(), 'feedback-meeting-trigger', 'button');
  // The static markup ships hidden; only the key gate un-hides it.
  assert.match(trigger, /\shidden(\s|>)/);
  // A bare `display` on the class would outrank the UA's `[hidden]` rule and
  // reveal the feature on an unconfigured deployment.
  assert.match(styleSource, /\.feedback-meeting-trigger:not\(\[hidden\]\) \{/);
  assert.doesNotMatch(cssBlock(styleSource, '.feedback-meeting-trigger'), /display:/);
});

test('#895: the blocked reason is no longer a permanent topbar chip -- it is reachable on hover/focus and via aria-describedby', () => {
  const topbar = topbarRightMarkup();
  const reason = elementMarkup(topbar, 'feedback-share-reason', 'p');
  const cell = divBlock(topbar, 'feedback-meeting-cell');
  const trigger = elementMarkup(cell, 'feedback-meeting-trigger', 'button');
  const tooltip = elementMarkup(cell, 'feedback-meeting-tooltip', 'span');

  // The always-visible chip (#786) is reverted: the reason `<p>` survives
  // only as the aria-describedby target, sr-only and starting hidden.
  assert.ok(topbar.includes('id="feedback-share-reason"'), 'the share reason must still exist for aria-describedby');
  assert.match(reason, /class="[^"]*\bsr-only\b/);
  assert.match(reason, /\shidden(\s|>)/);
  assert.match(indexHtml, /Bug reports pause while you're sharing\./);

  // The visible explanation is now a tooltip: a SIBLING of the button inside
  // a positioned wrapper cell, not a child of the button.
  const buttonCloseAt = cell.indexOf('</button>');
  const tooltipAt = cell.indexOf('id="feedback-meeting-tooltip"');
  assert.ok(buttonCloseAt !== -1 && tooltipAt !== -1 && buttonCloseAt < tooltipAt, 'the tooltip must follow the button as a sibling, not nest inside it');
  assert.match(tooltip, /aria-hidden="true"/, 'the tooltip is decorative; the accessible description comes from aria-describedby');
  assert.match(tooltip, /Bug reports pause while you're sharing\./);

  // Reachable on hover AND keyboard focus, gated on the trigger's blocked
  // state (never shown for an available button, which already has a native
  // title tooltip).
  assert.match(styleSource, /\.feedback-meeting-trigger\.blocked:hover ~ \.feedback-meeting-tooltip/);
  assert.match(styleSource, /\.feedback-meeting-trigger\.blocked:focus-visible ~ \.feedback-meeting-tooltip/);

  // Opens DOWNWARD -- this control sits at the very top of the topbar, so an
  // upward tooltip (like .control-tooltip's `bottom:`) would render off the
  // top of the viewport -- and wraps rather than clips (never-truncate rule).
  const tooltipStyles = cssBlock(styleSource, '.feedback-meeting-tooltip');
  assert.match(tooltipStyles, /top:\s*calc\(100% \+ 8px\);/);
  assert.doesNotMatch(tooltipStyles, /\bbottom:/);
  // Right-anchored, never centered: the trigger sits near the viewport's
  // right edge, so `left: 50%` + translate(-50%) pushes the tooltip past
  // the viewport and clips the copy (caught truncated on prod, #895).
  assert.match(tooltipStyles, /right:\s*0;/);
  assert.doesNotMatch(tooltipStyles, /left:\s*50%/);
  assert.doesNotMatch(tooltipStyles, /translate\(-50%/);
  assert.match(tooltipStyles, /width:\s*max-content;/);
  assert.match(tooltipStyles, /max-width:/);
  assert.match(tooltipStyles, /white-space:\s*normal;/);
  assert.match(tooltipStyles, /overflow-wrap:\s*anywhere;/);
  assert.doesNotMatch(tooltipStyles, /text-overflow:\s*ellipsis;/);
  assert.doesNotMatch(tooltipStyles, /white-space:\s*nowrap;/);

  // Icon-only trigger: the tooltip lives outside the button, so there is
  // still no text node inside it to clip at any topbar width.
  assert.equal(trigger.replace(/<[^>]*>/g, '').trim(), '');
});

test("#786: the web trigger's copy never promises that logs are attached (#293 keeps raw logs local-only)", () => {
  const topbar = topbarRightMarkup();
  const trigger = elementMarkup(topbar, 'feedback-meeting-trigger', 'button');
  const reason = elementMarkup(topbar, 'feedback-share-reason', 'p');
  // Only the user-visible copy — `dialog` in aria-haspopup/aria-controls is
  // wiring, not a promise.
  const triggerCopy = [...trigger.matchAll(/(?:aria-label|title)="([^"]*)"/g)].map((m) => m[1]).join(' ');
  assert.match(triggerCopy, /Report a bug/);
  assert.doesNotMatch(triggerCopy, /log/i);
  assert.doesNotMatch(reason.replace(/<[^>]*>/g, ''), /log/i);
  // The upload stays the fixed diagnostics document, and the raw session log
  // stays a local-only download the user attaches by hand.
  assert.match(indexHtml, /Send the attached redacted diagnostics file to UserDispatch/);
  assert.match(indexHtml, /id="download-session-log"/);
});

test('fixed diagnostics accept only closed event codes and never serialize arbitrary input', () => {
  const text = buildFeedbackDiagnostics(connected, ['feedback_opened', 'room-acme-77', 'unknown', 'feedback_submitted'], '2026-07-11T01:02:03.000Z');
  assert.deepEqual(JSON.parse(text), {
    schema_version: 2,
    connection_state: 'connected',
    timestamp: '2026-07-11T01:02:03.000Z',
    event_codes: ['feedback_opened', 'feedback_submitted'],
  });
  assert.doesNotMatch(text, /room-acme-77|unknown/);
});

// #293's original guard asserted this module could not import the session-log
// collector at all. That policy was reversed deliberately (owner decision,
// 2026-08-13) so incidents are diagnosable -- see #788. The guard is REPLACED,
// not deleted: what must now hold is that no UNSCRUBBED log byte can reach the
// adapter. These three tests are that guarantee.
test('every log byte crosses the upload boundary scrubbed, including values that straddle the tail cut', () => {
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('room-acme-77');
  registry.registerParticipant('web-riley');
  registry.registerReportingValue('Riley Example');

  // `room-acme-77` is positioned so the 512-byte tail cut falls INSIDE it:
  // six of its characters land before the cut and six after. This is the
  // arrangement the ordering actually has to survive. Putting the value
  // wholly before the cut proves nothing -- truncate-then-scrub discards it
  // along with the rest of the prefix and passes too (verified: that ordering
  // leaves the whole file green with the value placed at the start).
  const trailing = 'x'.repeat(506);
  const log = `room-acme-77 joined by web-riley (Riley Example)\n${'filler line\n'.repeat(5_000)}room-acme-77${trailing}`;
  const tail = buildScrubbedLogTail(log, registry, 512);

  assert.ok(tail.length <= 512 + 120, 'tail must be bounded');
  assert.doesNotMatch(tail, /room-acme-77/);
  assert.doesNotMatch(tail, /web-riley/);
  assert.doesNotMatch(tail, /Riley Example/);
  // Under truncate-then-scrub the registry only ever sees `cme-77…`, cannot
  // match the full room name, and ships the orphaned fragment.
  assert.doesNotMatch(tail, /cme-77/, 'no fragment of the credential may survive the cut');
  assert.match(tail, /earlier log lines omitted/, 'truncation must be visible to the reader');
});

test('a non-ASCII log still produces an attachment instead of failing the whole submission', async () => {
  // The tail cap is bytes, not `String.length`. Japanese window titles run 3
  // bytes per code unit, so a length-based cap overshot
  // FEEDBACK_MAX_ATTACHMENT_BYTES and threw -- and the controller swallows that
  // into "Could not send feedback", so these users could never file a
  // diagnostics report at all.
  const registry = new SensitiveStringRegistry();
  const log = '共有ウィンドウが停止しました\n'.repeat(20_000);
  assert.ok(
    new TextEncoder().encode(log).length > FEEDBACK_MAX_ATTACHMENT_BYTES,
    'the fixture must actually exceed the attachment cap, or this proves nothing'
  );

  const blob = buildFeedbackAttachment(connected, ['feedback_opened'], registry, { logText: log });

  assert.ok(blob.size <= FEEDBACK_MAX_ATTACHMENT_BYTES, 'must fit the cap rather than throw');
  const text = await blob.text();
  assert.ok(text.includes(FEEDBACK_LOG_SECTION_HEADING), 'and must still carry a log');
  assert.match(text, /共有ウィンドウが停止しました/, 'legible non-ASCII content survives the byte cut');
});

test('the attachment carries the session log, and it is the scrubbed one', async () => {
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('room-acme-77');
  const blob = buildFeedbackAttachment(
    connected,
    ['feedback_opened'],
    registry,
    { logText: 'connecting to room-acme-77 ...\ntrack subscribed\n', timestamp: '2026-08-13T00:00:00.000Z' }
  );
  const text = await blob.text();

  assert.match(text, /=== session log \(redacted\) ===/, 'the log must actually be attached');
  assert.match(text, /track subscribed/, 'non-sensitive log content survives');
  assert.doesNotMatch(text, /room-acme-77/, 'the room credential must not survive');
});

test('an unscrubbed collector export can never reach the adapter', async () => {
  // Drives the REAL submit path (not the builder in isolation): whatever the
  // collector holds must arrive scrubbed at the SDK boundary.
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('room-acme-77');
  registry.registerParticipant('web-riley');
  sessionLogCollector.record({
    ts: '2026-08-13T00:00:00.000Z',
    kind: 'info',
    message: 'joined room-acme-77 as web-riley'
  });
  const { adapter, calls } = recordingAdapter();

  const sent = await submitFeedbackReport({
    publicKey: validKey,
    getState: () => connected,
    isCurrent: () => true,
    includeDiagnostics: true,
    message: 'audio broke',
    email: validEmail,
    registry,
    eventCodes: ['feedback_opened'],
    adapter,
  });

  assert.equal(sent, true);
  const [, payload] = calls[0];
  const uploaded = await (payload.files?.[0]?.content as Blob).text();
  // Non-vacuity first: a submit path that quietly stopped attaching the log
  // would satisfy both `doesNotMatch` checks below while proving nothing.
  assert.equal(uploaded.includes(FEEDBACK_LOG_SECTION_HEADING), true, 'the log must reach the adapter');
  assert.match(uploaded, /joined <redacted:room> as <redacted:participant-\d+>/);
  assert.doesNotMatch(uploaded, /room-acme-77/);
  assert.doesNotMatch(uploaded, /web-riley/);
});

test('adapter boundary receives only a generic redacted Blob and fixed payload', async () => {
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('room-acme-77');
  registry.registerParticipant('web-riley');
  registry.registerReportingValue('Riley Example');
  registry.unregisterParticipant('web-riley');
  const { adapter, calls } = recordingAdapter();

  const sent = await submitFeedbackReport({
    publicKey: validKey,
    getState: () => connected,
    isCurrent: () => true,
    includeDiagnostics: true,
    message: 'Room room-acme-77 with web-riley and Riley Example failed.',
    email: validEmail,
    registry,
    eventCodes: ['feedback_opened', 'feedback_submitted', 'room-acme-77'],
    adapter,
  });

  assert.equal(sent, true);
  assert.equal(calls.length, 1);
  const [key, payload] = calls[0];
  assert.equal(key, validKey);
  assert.deepEqual(Object.keys(payload).sort(), ['email', 'files', 'message', 'metadata', 'subject', 'type']);
  assert.equal(payload.type, 'feedback');
  assert.equal(payload.subject, 'Petal feedback');
  assert.equal(payload.email, validEmail);
  assert.equal(payload.files?.[0]?.name, FEEDBACK_DIAGNOSTICS_FILENAME);
  assert.equal(payload.files?.[0]?.type, FEEDBACK_DIAGNOSTICS_TYPE);
  const bytes = await (payload.files?.[0]?.content as Blob).text();
  const combined = `${payload.message}\n${bytes}\n${JSON.stringify(payload.metadata)}`;
  assert.doesNotMatch(combined, /room-acme-77|web-riley|Riley Example/);
  // The keyword tripwire still guards the CLOSED part of the document -- the
  // header must stay the fixed schema and never grow a window title, identity,
  // room, console dump or URL field. It deliberately does NOT extend to the log
  // section any more: that section is arbitrary prose whose safety comes from
  // the registry scrub asserted above, and a keyword ban there would fire on
  // the redaction markers themselves (`<redacted:room>`).
  const [header, ...logSection] = bytes.split(`\n\n${FEEDBACK_LOG_SECTION_HEADING}\n`);
  assert.doesNotMatch(header, /window|identity|room|console|url/i);
  assert.equal(logSection.length <= 1, true, 'exactly one log section, or none');
});

test('missing key, invalid key, and active share never call the SDK', async () => {
  for (const [publicKey, state] of [
    [undefined, connected],
    ['sk_secret-value-123456', connected],
    [validKey, { ...connected, sharing: true }],
  ] as const) {
    const { adapter, calls } = recordingAdapter();
    const sent = await submitFeedbackReport({
      publicKey,
      getState: () => state,
      isCurrent: () => true,
      includeDiagnostics: true,
      message: 'A bounded feedback message.',
      email: validEmail,
      registry: new SensitiveStringRegistry(),
      eventCodes: ['feedback_opened'],
      adapter,
    });
    assert.equal(sent, false);
    assert.equal(calls.length, 0);
  }
});

test('unchecked diagnostics still sends the intentional message but attaches no file', async () => {
  const { adapter, calls } = recordingAdapter();
  const sent = await submitFeedbackReport({
    publicKey: validKey,
    getState: () => ({ ...connected, connected: false }),
    isCurrent: () => true,
    includeDiagnostics: false,
    message: 'Feedback is available from the join screen too.',
    email: validEmail,
    registry: new SensitiveStringRegistry(),
    eventCodes: ['feedback_opened'],
    adapter,
  });
  assert.equal(sent, true);
  assert.equal(calls.length, 1);
  assert.equal(calls[0][1].files, undefined);
});

test('share-start race after Blob preparation prevents the adapter invocation', async () => {
  const { adapter, calls } = recordingAdapter();
  let reads = 0;
  const sent = await submitFeedbackReport({
    publicKey: validKey,
    getState: () => {
      reads += 1;
      return reads < 2 ? connected : { ...connected, screenSharing: true };
    },
    isCurrent: () => true,
    includeDiagnostics: true,
    message: 'The picker race should cancel this report.',
    email: validEmail,
    registry: new SensitiveStringRegistry(),
    eventCodes: ['feedback_opened'],
    adapter,
  });
  assert.equal(sent, false);
  assert.equal(calls.length, 0);
});

test('attachment uses the fixed generic content type and cannot inherit a user filename', async () => {
  const blob = buildFeedbackAttachment(connected, ['feedback_opened'], new SensitiveStringRegistry(), { logText: '' });
  assert.equal(blob.type, FEEDBACK_DIAGNOSTICS_TYPE);
  assert.equal(FEEDBACK_DIAGNOSTICS_FILENAME, 'petal-diagnostics.txt');
  assert.ok(blob.size > 0);
});

// ---------------------------------------------------------------------------
// #245: every report carries a required, format-checked (never verified)
// reply address, sent as the SDK's `email` field and nowhere else.
// ---------------------------------------------------------------------------

test('#245: a missing or malformed email never reaches the SDK', async () => {
  for (const email of ['', '   ', 'riley', 'riley@example', 'riley@@example.org', 'ri ley@example.org']) {
    const { adapter, calls } = recordingAdapter();
    const sent = await submitFeedbackReport({
      publicKey: validKey,
      getState: () => connected,
      isCurrent: () => true,
      includeDiagnostics: false,
      message: 'Audio dropped for everyone.',
      email,
      registry: new SensitiveStringRegistry(),
      eventCodes: ['feedback_opened'],
      adapter,
    });
    assert.equal(sent, false, `${JSON.stringify(email)} must be refused`);
    assert.equal(calls.length, 0);
  }
});

test('#245: the bundled SDK carries the trimmed address on both wire paths -- JSON, and multipart when diagnostics are attached', async () => {
  // The real `@userdispatch/sdk` behind the real adapter; only `fetch` is
  // stubbed, so nothing leaves the process. The SDK builds the multipart body
  // field by field, so this is what proves `email` survives an attachment.
  const realFetch = globalThis.fetch;
  const requests: Array<{ url: string; body: RequestInit['body'] }> = [];
  globalThis.fetch = (async (url: RequestInfo | URL, init?: RequestInit) => {
    requests.push({ url: String(url), body: init?.body });
    return new Response(JSON.stringify({ data: { id: 'sub_test', status: 'new', created_at: '2026-09-25T00:00:00Z' } }), { status: 201 });
  }) as typeof fetch;
  try {
    for (const includeDiagnostics of [false, true]) {
      const sent = await submitFeedbackReport({
        publicKey: validKey,
        getState: () => connected,
        isCurrent: () => true,
        includeDiagnostics,
        message: 'Audio dropped for everyone.',
        email: `  ${validEmail} `,
        registry: new SensitiveStringRegistry(),
        eventCodes: ['feedback_opened'],
        adapter: userDispatchAdapter,
      });
      assert.equal(sent, true);
    }
  } finally {
    globalThis.fetch = realFetch;
  }

  assert.equal(requests.length, 2);
  assert.match(requests[0].url, /^https:\/\/userdispatch\.com\/api\/v1\/submissions$/);
  const json = JSON.parse(requests[0].body as string);
  assert.equal(json.email, validEmail);
  assert.equal(json.message, 'Audio dropped for everyone.');

  const form = requests[1].body;
  assert.ok(form instanceof FormData, 'an attachment switches the SDK to multipart');
  assert.equal(form.get('email'), validEmail);
  const file = form.get('files');
  assert.ok(file instanceof Blob);
  assert.doesNotMatch(await file.text(), /riley@example\.org/i);
});

test('#245: the address never reaches the attachment, even when the session log holds it', async () => {
  sessionLogCollector.record({
    ts: '2026-09-25T00:00:00.000Z',
    kind: 'info',
    message: `display name set to ${validEmail}; retrying as RILEY@EXAMPLE.ORG`,
  });
  const { adapter, calls } = recordingAdapter();

  const sent = await submitFeedbackReport({
    publicKey: validKey,
    getState: () => connected,
    isCurrent: () => true,
    includeDiagnostics: true,
    message: 'audio broke',
    email: validEmail,
    registry: new SensitiveStringRegistry(),
    eventCodes: ['feedback_opened'],
    adapter,
  });

  assert.equal(sent, true);
  const [, payload] = calls[0];
  assert.equal(payload.email, validEmail);
  const uploaded = await (payload.files?.[0]?.content as Blob).text();
  // Non-vacuity: the line itself is attached; only the address is gone, in
  // any letter case.
  assert.match(uploaded, /display name set to <redacted:email>; retrying as <redacted:email>/);
  assert.doesNotMatch(uploaded, /riley@example\.org/i);
});

test('#245: the address is scrubbed before the tail cut, so no fragment of it survives the cut', async () => {
  // Same arrangement as the room-name straddle test above: the 128 KiB cut
  // falls inside the address, eight characters before its end.
  const log = `${'filler line\n'.repeat(12_000)}${validEmail}${'x'.repeat(128 * 1024 - 8)}`;
  const blob = buildFeedbackAttachment(connected, ['feedback_opened'], new SensitiveStringRegistry(), {
    logText: log,
    email: validEmail,
  });
  const text = await blob.text();
  assert.match(text, /earlier log lines omitted/, 'the fixture must actually be cut');
  // Scrub-after-cut would ship `mple.org`, the address's last eight characters.
  assert.doesNotMatch(text, /mple\.org/);
});

test('#245: the feedback module never writes to the console or the session log', () => {
  assert.doesNotMatch(feedbackReportSource, /console\.|sessionLogCollector\.record|logEvent\(/);
});

function memoryStorage(initial: Record<string, string> = {}) {
  const values = new Map(Object.entries(initial));
  const storage: FeedbackEmailStorage = {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => void values.set(key, value),
  };
  return { storage, values };
}

/** The controller over minimal DOM fakes, opened from the home trigger. */
function emailHarness(storage: FeedbackEmailStorage | null, adapter: FeedbackAdapter = { async submit() {} }) {
  const trigger = () =>
    fakeElement({ hidden: true, disabled: false, setAttribute() {}, focus() {} }) as unknown as HTMLButtonElement;
  const homeTrigger = trigger();
  const dialog = fakeElement({ open: false, showModal() { this.open = true; }, close() { this.open = false; }, remove() {} }) as unknown as HTMLDialogElement;
  const message = fakeElement({ value: '', focus() {} }) as unknown as HTMLTextAreaElement;
  const email = fakeElement({
    value: '',
    attributes: new Map<string, string>(),
    setAttribute(name: string, value: string) { this.attributes.set(name, value); },
  }) as unknown as HTMLInputElement & { attributes: Map<string, string> };
  const emailError = { hidden: true, textContent: '' } as HTMLElement;
  const consent = fakeElement({ checked: false }) as unknown as HTMLInputElement;
  const submit = fakeElement({ disabled: false }) as unknown as HTMLButtonElement;
  const cancel = fakeElement({}) as unknown as HTMLButtonElement;
  const form = fakeElement({}) as unknown as HTMLFormElement;
  const status = { textContent: '' } as HTMLElement;
  const controller = new FeedbackReportController({
    publicKey: validKey,
    dom: {
      homeTrigger,
      meetingTrigger: trigger(),
      dialog,
      form,
      message,
      email,
      emailError,
      consent,
      submit,
      cancel,
      status,
      shareReason: { id: 'feedback-share-reason', hidden: true } as HTMLElement,
    },
    getState: () => connected,
    registry: new SensitiveStringRegistry(),
    adapter,
    storage,
  });
  controller.install();
  const fire = (element: unknown, type: string) => (element as { dispatch(type: string): void }).dispatch(type);
  const type = (element: HTMLInputElement | HTMLTextAreaElement, value: string) => {
    element.value = value;
    fire(element, 'input');
  };
  const open = () => fire(homeTrigger, 'click');
  const send = async () => {
    fire(form, 'submit');
    await new Promise((resolve) => setTimeout(resolve, 0));
  };
  const press = (element: unknown, key: string) =>
    (element as { dispatch(type: string, event: Event): void }).dispatch('keydown', Object.assign(new Event('keydown'), { key }));
  return { dialog, message, email, emailError, submit, cancel, status, fire, type, open, send, press };
}

test('#245: Send stays disabled until the message AND a well-formed email are present; the inline error waits for blur', () => {
  const ui = emailHarness(null);
  ui.open();
  assert.equal(ui.email.value, '', 'nothing remembered yet');
  ui.type(ui.message, 'Audio dropped for everyone.');
  assert.equal(ui.submit.disabled, true, 'a message alone is not enough');

  ui.type(ui.email, 'riley@example');
  assert.equal(ui.submit.disabled, true);
  assert.equal(ui.emailError.hidden, true, 'no error while the field is still being typed in');

  ui.fire(ui.email, 'blur');
  assert.equal(ui.emailError.hidden, false);
  assert.equal(ui.emailError.textContent, 'Enter a valid email address, like name@example.com.');
  assert.equal(ui.email.attributes.get('aria-invalid'), 'true');

  ui.type(ui.email, ` ${validEmail} `);
  assert.equal(ui.emailError.hidden, true, 'the error clears as soon as the address is fixed');
  assert.equal(ui.emailError.textContent, '');
  assert.equal(ui.email.attributes.get('aria-invalid'), 'false');
  assert.equal(ui.submit.disabled, false);

  ui.type(ui.email, '');
  assert.equal(ui.submit.disabled, true);
  assert.equal(ui.emailError.textContent, 'Enter your email address.');

  // Closing resets the touched state: a reopened form starts without an error.
  ui.fire(ui.cancel, 'click');
  ui.open();
  assert.equal(ui.emailError.hidden, true);
});

test('#245: Enter on a bad address explains itself instead of doing nothing', () => {
  const ui = emailHarness(null);
  ui.open();
  ui.type(ui.message, 'Audio dropped for everyone.');
  ui.type(ui.email, 'riley@example');
  ui.press(ui.email, 'a');
  assert.equal(ui.emailError.hidden, true, 'ordinary typing never shows the error');
  // Send is disabled, so the browser's implicit submission does nothing; the
  // keydown is the only thing that can tell the user why.
  ui.press(ui.email, 'Enter');
  assert.equal(ui.emailError.hidden, false);
  assert.equal(ui.emailError.textContent, 'Enter a valid email address, like name@example.com.');
  assert.equal(ui.email.attributes.get('aria-invalid'), 'true');
});

test('#245: the address last sent is remembered on the device, prefilled on the next open, and still editable', async () => {
  const { storage, values } = memoryStorage();
  const { adapter, calls } = recordingAdapter();
  const ui = emailHarness(storage, adapter);

  ui.open();
  ui.type(ui.message, 'First report.');
  ui.type(ui.email, `  ${validEmail}`);
  ui.fire(ui.email, 'blur');
  assert.equal(values.size, 0, 'typing alone remembers nothing');
  await ui.send();
  assert.equal(ui.status.textContent, 'Feedback sent.');
  assert.equal(calls[0][1].email, validEmail);
  assert.equal(values.get(HARNESS_FEEDBACK_EMAIL_STORAGE_KEY), validEmail, 'stored trimmed');
  assert.equal(ui.email.value, `  ${validEmail}`, 'the field keeps the address after a send');

  ui.fire(ui.cancel, 'click');
  assert.equal(ui.email.value, '');
  ui.open();
  assert.equal(ui.email.value, validEmail, 'prefilled on reopen');
  assert.equal(ui.emailError.hidden, true);
  ui.type(ui.message, 'Second report.');
  assert.equal(ui.submit.disabled, false, 'a remembered address is enough to send');

  // A pasted `<mailto:…>` is unwrapped: the NORMALIZED address is what is
  // sent and what is remembered, never the raw field text.
  ui.type(ui.email, ' <mailto:riley@work.example.com> ');
  await ui.send();
  assert.equal(calls[1][1].email, 'riley@work.example.com');
  assert.equal(values.get(HARNESS_FEEDBACK_EMAIL_STORAGE_KEY), 'riley@work.example.com');
});

test('#245: an address that was not sent is not remembered, and a bad stored value is never prefilled', async () => {
  const { storage, values } = memoryStorage({ [HARNESS_FEEDBACK_EMAIL_STORAGE_KEY]: 'not an address' });
  const failing: FeedbackAdapter = { async submit() { throw new Error('offline'); } };
  const ui = emailHarness(storage, failing);
  ui.open();
  assert.equal(ui.email.value, '');
  ui.type(ui.message, 'Report.');
  ui.type(ui.email, validEmail);
  await ui.send();
  assert.equal(ui.status.textContent, 'Could not send feedback. Please try again later.');
  assert.equal(values.get(HARNESS_FEEDBACK_EMAIL_STORAGE_KEY), 'not an address');
});

test('#245: storage that throws (blocked site data) never breaks the form', async () => {
  const throwing: FeedbackEmailStorage = {
    getItem() { throw new Error('SecurityError'); },
    setItem() { throw new Error('QuotaExceededError'); },
  };
  const { adapter, calls } = recordingAdapter();
  const ui = emailHarness(throwing, adapter);
  ui.open();
  assert.equal(ui.email.value, '');
  ui.type(ui.message, 'Report.');
  ui.type(ui.email, validEmail);
  await ui.send();
  assert.equal(calls.length, 1);
  assert.equal(ui.status.textContent, 'Feedback sent.');
});

test('#245: the dialog asks for a required email with its hint, styled explicitly and validated only by the shared rule', () => {
  const input = indexHtml.match(/<input id="feedback-email"[^>]*>/)?.[0];
  assert.ok(input, 'missing #feedback-email');
  assert.match(input, /\stype="email"/);
  assert.match(input, /\srequired(\s|\/|>)/);
  assert.match(input, /\sautocomplete="email"/);
  assert.match(input, /\sinputmode="email"/);
  assert.match(input, /\saria-describedby="feedback-email-error feedback-email-hint"/);
  assert.doesNotMatch(input, /maxlength/, 'a cap would silently truncate a pasted address into a different one');
  assert.match(indexHtml, /<label class="feedback-field" for="feedback-email">Your email<\/label>/);
  assert.match(
    indexHtml,
    /<p id="feedback-email-hint" class="feedback-disclosure">So we can reply\. We won't verify it\. Remembered on this device\.<\/p>/
  );
  // The error sits inside an ALWAYS-rendered polite live region and only it
  // toggles -- a region that is itself hidden until it speaks is unreliable.
  // aria-describedby still points at the error element.
  const notes = divBlock(indexHtml, 'feedback-email-notes');
  assert.match(notes, /^<div class="feedback-email-notes" aria-live="polite">/);
  assert.match(notes, /<p id="feedback-email-error" class="feedback-field-error" hidden><\/p>/);
  assert.match(notes, /<p id="feedback-email-hint" class="feedback-disclosure">/);
  assert.match(indexHtml, /<form id="feedback-form"[^>]*\snovalidate>/);
  assert.match(indexHtml, /Your message and email address are sent to UserDispatch\./);

  // The shared `input[type='text']` rule does not reach an email input.
  const emailStyles = cssBlock(styleSource, ".feedback-form input[type='email']");
  assert.match(emailStyles, /width:\s*100%;/);
  assert.match(emailStyles, /border:/);
  assert.match(styleSource, /\.feedback-form input\[type='email'\]\[aria-invalid='true'\] \{/);
});

test('#245: the attachment disclosure describes what the attachment really carries', async () => {
  // The old copy promised "only fixed diagnostic codes and connection state
  // ... never ... raw session logs" while the attachment has carried a
  // redacted session-log excerpt since #788. Pin the corrected copy, and pin
  // each claim in it against the real builder.
  assert.match(
    indexHtml,
    /<p class="feedback-disclosure">The attachment is a small fixed header \(connection state, a timestamp, event codes\) plus the end of this tab's Petal log, with the room names, invite codes, identities and display names of every meeting in it redacted\. It never includes screenshots, video, or chat messages\.<\/p>/
  );
  assert.doesNotMatch(indexHtml, /only fixed diagnostic codes|raw session logs/);

  const registry = new SensitiveStringRegistry();
  registry.registerRoom('room-acme-77');
  registry.registerParticipant('web-riley');
  registry.registerReportingValue('Riley Example');
  const text = await buildFeedbackAttachment(connected, ['feedback_opened'], registry, {
    logText: 'room-acme-77: web-riley (Riley Example) joined\n',
    timestamp: '2026-09-25T00:00:00.000Z',
  }).text();
  const [header, log] = text.split(`\n\n${FEEDBACK_LOG_SECTION_HEADING}\n`);
  // "a small fixed header (connection state, a timestamp, event codes)"
  assert.deepEqual(Object.keys(JSON.parse(header)).sort(), ['connection_state', 'event_codes', 'schema_version', 'timestamp']);
  // "plus the end of this tab's Petal log, with ... redacted"
  assert.equal(log, '<redacted:room>: <redacted:participant-1> (<redacted:session-value>) joined\n\n');
});

test('#245: a real invite link and room label never survive into the attachment -- access code and label slug are registered with the room', async () => {
  const accessCode = 'vds-quyc-aqv';
  const credential = internalCredentialForAccessCode(accessCode);
  const label = 'Acme Board Meeting';
  const url = inviteLinkForCredential(credential, 'https://petal.example', label);
  assert.equal(url, `https://petal.example/acme-board-meeting/${accessCode}`, 'non-vacuity: the real link carries both');

  const registry = new SensitiveStringRegistry();
  // What connectToMeeting registers (credential and wire names, then the
  // user-facing aliases).
  registry.registerRoom(credential);
  registerMeetingAliases(registry, credential, label);
  // Registering the same values again (e.g. #244's own access-code
  // registration) is harmless.
  const before = registry.size;
  registerMeetingAliases(registry, credential, label);
  registry.registerRoom(accessCode);
  assert.equal(registry.size, before);

  const text = await buildFeedbackAttachment(connected, ['feedback_opened'], registry, {
    logText: `old build line: invite link copied: ${url}\nrenamed room to "${label}" for ${credential}\n`,
  }).text();
  assert.match(text, /invite link copied: https:\/\/petal\.example\/<redacted:room>\/<redacted:room>/);
  assert.equal(text.includes(accessCode), false);
  assert.equal(text.includes('acme-board-meeting'), false);
  assert.equal(text.includes(label), false);
  assert.equal(text.includes(credential), false);
});

test('#245: the friendly fallback label is Petal wording, not user data, and is never registered', () => {
  const registry = new SensitiveStringRegistry();
  registerMeetingAliases(registry, internalCredentialForAccessCode('abc-defg-hjk'), 'Petal meeting');
  assert.equal(registry.scrubForReporting('Petal meeting / petal-meeting'), 'Petal meeting / petal-meeting');
  assert.equal(registry.scrubForReporting('abc-defg-hjk'), '<redacted:room>');
});

test('#245: a report sent after leaving a meeting still scrubs that meeting -- leaving resets only the live Sentry map', async () => {
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('room-acme-77');
  registry.registerParticipant('web-riley');
  registry.registerReportingValue('Riley Example');
  const log = 'joined room-acme-77 as web-riley (Riley Example)\n';
  // connection.ts calls reset() on disconnect; the user then files feedback
  // from the home screen with the diagnostics box ticked.
  registry.reset();

  assert.equal(registry.scrub(log), log, 'the live Sentry map is cleared as before');
  const text = await buildFeedbackAttachment(connected, ['feedback_opened'], registry, { logText: log }).text();
  assert.match(text, /joined <redacted:room> as <redacted:participant-1> \(<redacted:session-value>\)/);
  assert.doesNotMatch(text, /room-acme-77|web-riley|Riley Example/);
});

test('#245: no web log line interpolates an invite link -- it carries the access code and the label slug', () => {
  const offenders: string[] = [];
  const walk = (dir: URL) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const child = new URL(`${entry.name}${entry.isDirectory() ? '/' : ''}`, dir);
      if (entry.isDirectory()) walk(child);
      else if (/\.(ts|svelte)$/.test(entry.name)) {
        const source = readFileSync(child, 'utf8');
        for (const match of source.matchAll(/logEvent\??\.?\(\s*`[^`]*\$\{(url|inviteUrl|inviteLink|link)\}[^`]*`/g)) {
          offenders.push(`${entry.name}: ${match[0]}`);
        }
      }
    }
  };
  walk(new URL('../src/', import.meta.url));
  assert.deepEqual(offenders, []);
});
