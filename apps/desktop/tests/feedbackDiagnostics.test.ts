import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { normalizedFeedbackMessage, FEEDBACK_MAX_MESSAGE_CHARS } from '../src/lib/feedback/messageSanitizer.ts';
import { feedbackSubmission } from '../src/lib/feedback/userDispatch.ts';

const userDispatchSource = readFileSync(
  new URL('../src/lib/feedback/userDispatch.ts', import.meta.url),
  'utf8'
);
const feedbackModalSource = readFileSync(
  new URL('../src/lib/components/FeedbackModal.svelte', import.meta.url),
  'utf8'
);
const feedbackRs = readFileSync(new URL('../src-tauri/src/feedback.rs', import.meta.url), 'utf8');
const loggingRs = readFileSync(new URL('../src-tauri/src/logging.rs', import.meta.url), 'utf8');
const ipcSource = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');

test('normalizedFeedbackMessage trims, collapses whitespace, strips control chars, and bounds length', () => {
  assert.equal(normalizedFeedbackMessage('  hello   world  '), 'hello world');
  assert.equal(normalizedFeedbackMessage('line one\n\nline two'), 'line one line two');
  assert.equal(normalizedFeedbackMessage('a\x00b\x1fc\x7fd'), 'a b c d');
  assert.equal(normalizedFeedbackMessage(''), '');
  assert.equal(normalizedFeedbackMessage('   '), '');

  const long = 'x'.repeat(FEEDBACK_MAX_MESSAGE_CHARS + 500);
  assert.equal(normalizedFeedbackMessage(long).length, FEEDBACK_MAX_MESSAGE_CHARS);
});

test('the UserDispatch SDK is dynamically imported, never a top-level/eager import (#292: no widget script, no startup-path SDK)', () => {
  assert.doesNotMatch(userDispatchSource, /^import.*@userdispatch\/sdk/m);
  assert.match(userDispatchSource, /await import\('@userdispatch\/sdk'\)/);
});

test('submitFeedback sends only public key, fixed subject, sanitized message, and an opt-in attachment -- no room/identity/session metadata', () => {
  assert.match(userDispatchSource, /type: 'feedback'/);
  assert.match(userDispatchSource, /subject: 'Petal feedback'/);
  assert.doesNotMatch(userDispatchSource, /roomName:|identity:|joinUrl:|accessCode:/);
});

test('#245: the payload carries the trimmed email as the SDK field, beside the fixed subject and sanitized message', () => {
  const plain = feedbackSubmission({ message: '  audio   dropped ', email: ' riley@example.org ' });
  assert.deepEqual(Object.keys(plain).sort(), ['email', 'message', 'subject', 'type']);
  assert.deepEqual(plain, {
    type: 'feedback',
    subject: 'Petal feedback',
    email: 'riley@example.org',
    message: 'audio dropped'
  });
  // The NORMALIZED address is what is sent, never the raw field text.
  assert.equal(feedbackSubmission({ message: 'audio dropped', email: ' <mailto:riley@example.org> ' }).email, 'riley@example.org');

  const blob = new Blob(['PK'], { type: 'application/zip' });
  const withFile = feedbackSubmission({
    message: 'audio dropped',
    email: 'riley@example.org',
    attachment: { filename: 'petal-feedback-diagnostics.zip', mimeType: 'application/zip', blob, byteCount: 2 }
  });
  assert.deepEqual(Object.keys(withFile).sort(), ['email', 'files', 'message', 'subject', 'type']);
  assert.equal(withFile.email, 'riley@example.org');
  assert.deepEqual(withFile.files, [{ name: 'petal-feedback-diagnostics.zip', content: blob, type: 'application/zip' }]);
});

test('#245: a missing or malformed email is refused before the SDK, and the error never echoes it', () => {
  for (const email of ['', '   ', 'riley', 'riley@example', 'riley@@example.org', 'ri ley@example.org']) {
    assert.throws(
      () => feedbackSubmission({ message: 'audio dropped', email }),
      (error: unknown) => error instanceof Error && error.message === 'feedback email is not a well-formed address',
      JSON.stringify(email)
    );
  }
  assert.throws(() => feedbackSubmission({ message: '   ', email: 'riley@example.org' }), /feedback message is empty/);
  // submitFeedback builds through the pure function, so the SDK only ever sees its output.
  assert.match(userDispatchSource, /const submission = feedbackSubmission\(options\);[\s\S]*await client\.submit\(submission\);/);
});

test('#245: the email never reaches the native side, so it cannot land in the log, the diagnostics zip, or Sentry', () => {
  // The only native calls are the argument-free share check and diagnostics
  // build; the address stays in the webview and leaves only through the SDK.
  assert.match(userDispatchSource, /invoke<FeedbackDiagnostics>\(COMMANDS\.prepareFeedbackDiagnostics\);/);
  assert.match(feedbackModalSource, /prepareDiagnosticsAttachment\(\)/);
  assert.deepEqual(
    [...feedbackModalSource.matchAll(/invoke(?:<[^>]*>)?\(([^)]*)\)/g)].map((m) => m[1]),
    ['COMMANDS.sharedWindowIds']
  );
  assert.match(feedbackRs, /pub async fn prepare_feedback_diagnostics\(\s*state: tauri::State<'_, crate::session::SessionState>,\s*\)/);
  // Backstop: the export scrubber masks any bare address anyway.
  assert.match(loggingRs, /fn redact_email_addresses/);
});

test('#245: the FeedbackModal email input is a required email field validated only by the shared rule (behaviour: feedbackModalRendered.test.ts)', () => {
  assert.match(feedbackModalSource, /<form class="feedback-form" onsubmit=\{handleSubmit\} novalidate>/);
  const input = feedbackModalSource.match(/<input\s+id="feedback-email"[\s\S]*?\/>/)?.[0] ?? '';
  for (const attribute of ['type="email"', 'required', 'autocomplete="email"', 'inputmode="email"', 'bind:value={email}']) {
    assert.ok(input.includes(attribute), `the email input must carry ${attribute}`);
  }
  assert.doesNotMatch(input, /maxlength/, 'a cap would silently truncate a pasted address into a different one');
});

test('feedback submission errors are never logged (message text must not reach the console)', () => {
  assert.doesNotMatch(userDispatchSource, /console\.(log|error|warn)/);
  assert.doesNotMatch(feedbackModalSource, /console\.(log|error|warn)/);
});

test('diagnostics attachment checkbox starts unchecked by default -- opt-in per submission, never remembered/automatic', () => {
  assert.match(feedbackModalSource, /let attachDiagnostics = \$state\(false\)/);
  assert.match(feedbackModalSource, /bind:checked=\{attachDiagnostics\}/);
});

test('FeedbackModal discloses that an attachment may be sent off-device and links the UserDispatch privacy policy', () => {
  assert.match(feedbackModalSource, /Your message and email address are sent to UserDispatch/);
  assert.match(feedbackModalSource, /userdispatch\.com\/privacy/);
});

test('FeedbackModal never throws/crashes on a submit failure -- the SDK call is inside try/catch with a generic status message', () => {
  assert.match(feedbackModalSource, /try \{[\s\S]*?await submitFeedback/);
  assert.match(feedbackModalSource, /\} catch \{[\s\S]*?status = 'error';/);
  assert.match(feedbackModalSource, /Could not send feedback\. Please try again later\./);
});

test('FeedbackModal guards against submitting while a share is active (#292 point 6): checked on open, polled, and rechecked before both diagnostics prep and submit', () => {
  assert.match(feedbackModalSource, /COMMANDS\.sharedWindowIds/);
  assert.match(feedbackModalSource, /setInterval\(\(\) => void refreshSharing\(\), 2000\)/);
  // Rechecked immediately before preparing the attachment...
  assert.match(feedbackModalSource, /if \(await checkSharing\(\)\) \{[\s\S]*?attachDiagnostics/);
  // ...and again immediately before the final submit call.
  const submitCallIndex = feedbackModalSource.indexOf('await submitFeedback(');
  const lastSharingCheckBeforeSubmit = feedbackModalSource.lastIndexOf('await checkSharing()', submitCallIndex);
  assert.ok(
    lastSharingCheckBeforeSubmit > -1 && lastSharingCheckBeforeSubmit < submitCallIndex,
    'must recheck sharing state immediately before the SDK submit call'
  );
});

test('active-share rejection from the native diagnostics command is treated as a typed, user-visible case (not a generic crash)', () => {
  assert.match(feedbackRs, /return Err\("sharing_active"\.to_string\(\)\)/);
  assert.match(feedbackModalSource, /sharing_active/);
});

test('the native feedback command returns bytes/base64, never a filesystem path (#292 point 3)', () => {
  assert.match(feedbackRs, /pub bytes_base64: String/);
  assert.doesNotMatch(feedbackRs, /pub .*path.*: String/i);
  assert.match(ipcSource, /bytesBase64: string/);
});

test('the feedback attachment README is distinguishable from the local export README (#292 point 4)', () => {
  assert.match(loggingRs, /FEEDBACK_ATTACHMENT_README/);
  assert.match(loggingRs, /LOCAL_EXPORT_README/);
  assert.match(loggingRs, /No data was sent off this machine/);
  assert.match(loggingRs, /may be sent off this machine/);
});

test('the feedback diagnostics archive is size-bounded (fails closed, never partial) -- #292 boundedness requirement', () => {
  assert.match(loggingRs, /FEEDBACK_ATTACHMENT_MAX_ZIP_BYTES/);
  assert.match(loggingRs, /FEEDBACK_ATTACHMENT_LOG_TAIL_BYTES/);
  assert.match(loggingRs, /too large/);
});

test('screenshare-recursion (#292 point 6) is documented as structurally moot for the main window, with a share-active guard kept as a privacy courtesy', () => {
  assert.match(
    feedbackRs,
    /own_process_windows_are_excluded_from_share_source_enumeration/,
    'feedback.rs must cite the window_source.rs test that makes capture-recursion moot'
  );
});
