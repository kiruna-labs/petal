import assert from 'node:assert/strict';
import { test } from 'node:test';

import { scrubSensitiveStrings, SensitiveStringRegistry } from '../src/sensitiveStrings.ts';

test('scrubSensitiveStrings replaces every registered value and leaves everything else alone', () => {
  const values = new Map([
    ['fake-room-9x2', '<redacted:room>'],
    ['web-riley-participant', '<redacted:participant-1>'],
  ]);

  const message =
    'connecting to meeting "fake-room-9x2" as "web-riley-participant"... meeting "fake-room-9x2" -> livekit room "petal-room-fake-room-9x2"';

  const scrubbed = scrubSensitiveStrings(message, values);

  assert.equal(
    scrubbed,
    'connecting to meeting "<redacted:room>" as "<redacted:participant-1>"... meeting "<redacted:room>" -> livekit room "petal-room-<redacted:room>"'
  );
  assert.doesNotMatch(scrubbed, /fake-room-9x2|web-riley-participant/);
  // Unrelated text is untouched.
  assert.match(scrubbed, /connecting to meeting/);
});

test('scrubSensitiveStrings is a no-op when nothing is registered', () => {
  const message = 'track subscribed: someone / some-track (video)';
  assert.equal(scrubSensitiveStrings(message, new Map()), message);
});

test('scrubSensitiveStrings replaces the longest match first so a shorter value cannot partially clobber a longer one', () => {
  const values = new Map([
    ['room', '<redacted:room>'],
    ['room-extended-name', '<redacted:participant-1>'],
  ]);
  const scrubbed = scrubSensitiveStrings('joined room-extended-name today', values);
  assert.equal(scrubbed, 'joined <redacted:participant-1> today');
});

test('SensitiveStringRegistry: register a fake room name + identity, scrub a message containing both inline', () => {
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('acme-standup-77');
  registry.registerParticipant('web-alex-9f2');

  const message = 'connected to "acme-standup-77" as "web-alex-9f2"';
  const scrubbed = registry.scrub(message);

  assert.equal(scrubbed, 'connected to "<redacted:room>" as "<redacted:participant-1>"');
  assert.doesNotMatch(scrubbed, /acme-standup-77|web-alex-9f2/);
});

test('SensitiveStringRegistry keeps every room-name variant scrubbed within one session (access code, wire room, backend room)', () => {
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('abc-defg-hjk');
  registry.registerRoom('petal-room-abc-defg-hjk');
  registry.registerRoom('petal-room-abc-defg-hjk');

  const message = 'meeting "abc-defg-hjk" -> livekit room "petal-room-abc-defg-hjk"';
  assert.equal(registry.scrub(message), 'meeting "<redacted:room>" -> livekit room "<redacted:room>"');
});

test('SensitiveStringRegistry assigns stable, distinct labels per participant', () => {
  const registry = new SensitiveStringRegistry();
  registry.registerParticipant('web-riley');
  registry.registerParticipant('web-sam');
  registry.registerParticipant('web-riley'); // re-registering is idempotent

  assert.equal(registry.scrub('web-riley and web-sam joined'), '<redacted:participant-1> and <redacted:participant-2> joined');
});

test('SensitiveStringRegistry.unregisterParticipant stops scrubbing a departed participant', () => {
  const registry = new SensitiveStringRegistry();
  registry.registerParticipant('web-riley');
  registry.unregisterParticipant('web-riley');

  assert.equal(registry.scrub('web-riley left'), 'web-riley left');
});

test('SensitiveStringRegistry: registerReportingValue is scrubbed by scrub() too, not only scrubForReporting() (#709)', () => {
  // #709: registerReportingValue() used to write only to the local-log
  // `reportingValues` map, leaving every registered display name invisible
  // to the Sentry-facing `scrub()` path (`beforeBreadcrumb`/`beforeSend`).
  // Real display names reached Sentry unredacted as a result.
  const registry = new SensitiveStringRegistry();
  registry.registerReportingValue('Riley Example');
  registry.registerReportingValue('Payroll window');

  const raw = 'Riley Example / Payroll window';
  assert.equal(registry.scrub(raw), '<redacted:session-value> / <redacted:session-value>');
  assert.doesNotMatch(registry.scrub(raw), /Riley Example|Payroll window/);
});

test('SensitiveStringRegistry: an unregistered (departed) participant identity is still retained for reporting but no longer by scrub()', () => {
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('room-alias-9');
  registry.registerParticipant('web-riley');
  registry.registerReportingValue('Riley Example');
  registry.unregisterParticipant('web-riley');

  const raw = 'room-alias-9 / web-riley / Riley Example';
  // unregisterParticipant deliberately only removes the identity from the
  // live Sentry-facing map -- the room and the display name (never
  // unregistered) remain scrubbed in both paths for the rest of the session.
  assert.equal(registry.scrub(raw), '<redacted:room> / web-riley / <redacted:session-value>');
  const scrubbed = registry.scrubForReporting(raw);
  assert.doesNotMatch(scrubbed, /room-alias-9|web-riley|Riley Example/);
});

test('SensitiveStringRegistry reproduces the PETAL-WEB-HARNESS-3 leak shape: a "participant left" breadcrumb and a raw-identity latency-probe log line, both scrubbed by scrub()', () => {
  const registry = new SensitiveStringRegistry();
  // Pre-existing participant, registered the way the post-connect()
  // enumeration now does (identity only known ahead of any display name).
  registry.registerParticipant('1ab294e1-7ed8-4a11-9c2e-abcdef012345');
  registry.registerReportingValue('Till');

  const leftBreadcrumb = 'participant left: Till';
  const latencyProbeLine = 'latency probe: peer RTT to 1ab294e1-7ed8-4a11-9c2e-abcdef012345 42.3 ms';

  assert.equal(registry.scrub(leftBreadcrumb), 'participant left: <redacted:session-value>');
  assert.equal(
    registry.scrub(latencyProbeLine),
    'latency probe: peer RTT to <redacted:participant-1> 42.3 ms'
  );
  assert.doesNotMatch(registry.scrub(leftBreadcrumb), /Till/);
  assert.doesNotMatch(registry.scrub(latencyProbeLine), /1ab294e1-7ed8-4a11-9c2e-abcdef012345/);
});

test('SensitiveStringRegistry.reset clears rooms and participants', () => {
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('acme-standup-77');
  registry.registerParticipant('web-alex-9f2');
  registry.reset();

  assert.equal(registry.scrub('acme-standup-77 / web-alex-9f2'), 'acme-standup-77 / web-alex-9f2');
  assert.equal(registry.size, 0);
});

test('SensitiveStringRegistry ignores empty/blank room and participant values', () => {
  const registry = new SensitiveStringRegistry();
  registry.registerRoom('');
  registry.registerRoom('   ');
  registry.registerRoom(undefined);
  registry.registerParticipant('');
  registry.registerParticipant(undefined);

  assert.equal(registry.size, 0);
});
