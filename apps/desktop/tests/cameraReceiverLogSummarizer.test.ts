import assert from 'node:assert/strict';
import test from 'node:test';

import {
  parseCameraReceiverLines,
  renderSummary,
  summarizeCameraReceiverLog
} from '../scripts/summarize-camera-receiver-log.mjs';

// Fixtures mirror the exact format strings emitted by
// camera_receiver_lifecycle_line / camera_receiver_interval_line in
// apps/desktop/src-tauri/src/diagnostics.rs, so a formatter change that would
// silently break the analyzer fails here first.
//
// The analyzer exists because the receiver-diagnostics live gate ("one stable
// SID, progressing decode/presentation counters, no ~1 Hz probe churn, terminal
// cleanup") is otherwise a hand count over thousands of log lines.

const INTERVAL_DEFAULTS: Record<string, string | number> = {
  route: 'gallery-webview',
  t_ms: 0,
  trial_id: 't1',
  track_sid: 'TR_1',
  track_name: 'petal-camera-alice',
  participant: 'alice',
  stream_state: 'healthy',
  stall_cause: 'none',
  interval_seq: 1,
  interval_ms: 15000,
  decoded_dimensions: '1280x720',
  frames_decoded: 0,
  decoded_fps: 0,
  frames_received: 0,
  frames_rendered: 0,
  frames_dropped: 0,
  freeze_count: 0,
  freeze_ms: 0,
  key_frames_decoded: 0,
  bytes_received: 0,
  packets_received: 0,
  packets_lost: 'unknown',
  packets_discarded: 0,
  retransmitted_packets: 0,
  nack: 0,
  pli: 0,
  fir: 0,
  jitter_ms: 0,
  jitter_buffer_ms: 0,
  jitter_buffer_emitted: 0,
  decode_ms: 0,
  loss_pct: 0,
  presented_frames: 0,
  presented_fps: 0,
  gap_since_last_frame_ms: 0
};

const PRESENTATION_DEFAULTS: Record<string, string | number> = {
  rvfc: 'true',
  observing: 'true',
  paused: 'false',
  hidden: 'false',
  ready_state: 4,
  probe_starts: 1,
  gaps_100ms: 0,
  gaps_250ms: 0,
  max_gap_ms: 0,
  excess_ms: 0,
  current_gap_ms: 0
};

const PATH_DEFAULTS: Record<string, string | number> = {
  protocol: 'udp',
  local: 'host',
  remote: 'srflx',
  relay: 'unknown',
  selected_pair_changes: 1,
  rtt_ms: 12.5,
  available_in_kbps: 4000
};

function intervalLine(
  overrides: Record<string, string | number> = {},
  presentation: Record<string, string | number> = {},
  path: Record<string, string | number> = {}
): string {
  const fields = { ...INTERVAL_DEFAULTS, ...overrides };
  const keys = Object.keys(INTERVAL_DEFAULTS)
    .filter((key) => key !== 'route' && key !== 't_ms')
    .map((key) => `${key}=${fields[key]}`);
  const p = { ...PRESENTATION_DEFAULTS, ...presentation };
  const q = { ...PATH_DEFAULTS, ...path };
  return `diagnostics: camera receiver interval route=${fields.route} t_ms=${fields.t_ms} ${keys.join(' ')} decoder=libvpx presentation=${Object.entries(
    p
  )
    .map(([key, value]) => `${key}=${value}`)
    .join(' ')} path=${Object.entries(q)
    .map(([key, value]) => `${key}=${value}`)
    .join(' ')}`;
}

function lifecycleLine(overrides: Record<string, string | number> = {}): string {
  const fields = {
    route: 'gallery-webview',
    t_ms: 0,
    phase: 'subscribed',
    participant: 'alice',
    track_sid: 'TR_1',
    track_name: 'petal-camera-alice',
    bridge_age_ms: 100,
    detail: 'unknown',
    ...overrides
  };
  return `diagnostics: camera receiver lifecycle route=${fields.route} t_ms=${fields.t_ms} phase=${fields.phase} participant=${fields.participant} track_sid=${fields.track_sid} track_name=${fields.track_name} bridge_age_ms=${fields.bridge_age_ms} detail=${fields.detail}`;
}

test('a healthy SID parses into lifecycle, counters and categorical path', () => {
  const happy = [
    lifecycleLine({ t_ms: 1000, phase: 'bridge_connected' }),
    lifecycleLine({ t_ms: 1100, phase: 'subscribe_requested' }),
    lifecycleLine({ t_ms: 1200, phase: 'subscribed' }),
    lifecycleLine({ t_ms: 1500, phase: 'first_decode' }),
    intervalLine(
      { t_ms: 16000, interval_seq: 1, frames_decoded: 440, presented_frames: 430, decoded_fps: 29.4 },
      { probe_starts: 1 }
    ),
    intervalLine(
      { t_ms: 31000, interval_seq: 2, frames_decoded: 880, presented_frames: 870, decoded_fps: 29.5 },
      { probe_starts: 1 }
    ),
    intervalLine(
      { t_ms: 46000, interval_seq: 3, frames_decoded: 1320, presented_frames: 1300, decoded_fps: 29.3 },
      { probe_starts: 2 }
    ),
    lifecycleLine({ t_ms: 47000, phase: 'unsubscribed' })
  ].join('\n');

  const parsed = parseCameraReceiverLines(happy);
  assert.equal(parsed.malformed, 0);
  assert.equal(parsed.lifecycles.length, 5);
  assert.equal(parsed.intervals.length, 3);

  const summary = summarizeCameraReceiverLog(happy);
  assert.equal(summary.sids.length, 1);
  const sid = summary.sids[0]!;
  assert.equal(sid.kind, 'camera');
  assert.equal(sid.intervals, 3);
  assert.deepEqual(sid.lifecycle, [
    'bridge_connected',
    'subscribe_requested',
    'subscribed',
    'first_decode',
    'unsubscribed'
  ]);
  assert.equal(sid.terminalPhase, 'unsubscribed');
  assert.equal(sid.sequenceMonotonic, true);
  assert.deepEqual(sid.probeStarts, [1, 1, 2]);
  assert.equal(sid.probeChurn, false, 'a single step is a handoff, not churn');
  assert.deepEqual(sid.path.protocols, ['udp']);
  assert.deepEqual(sid.path.local, ['host']);
  assert.deepEqual(sid.path.remote, ['srflx']);
  assert.equal(sid.path.relay.length, 0, 'an unknown relay is not reported as a value');
  assert.equal(summary.verdicts.decodedProgressed, true);
  assert.equal(summary.verdicts.presentedProgressed, true);
  assert.equal(summary.verdicts.remountEvidence, true);
  assert.equal(summary.verdicts.anyProbeChurn, false);
});

test('probe restart churn is detected, because that is the failure the gate targets', () => {
  const churn = [1, 2, 3, 4]
    .map((seq) =>
      intervalLine(
        { t_ms: seq * 15000, interval_seq: seq, frames_decoded: seq * 440 },
        { probe_starts: seq * 15 }
      )
    )
    .join('\n');
  const summary = summarizeCameraReceiverLog(churn);
  assert.equal(summary.sids[0]!.probeChurn, true, '15 starts per interval is churn');
  assert.equal(summary.verdicts.anyProbeChurn, true);
  assert.equal(summary.sids[0]!.probeGrowthMax, 15);
});

test('identities, track names and free text never reach the output', () => {
  const secretIdentity = 'alice@example.com';
  const secretDetail = 'subscribe rejected for https://app.petal.live/?token=super-secret';
  const text = [
    lifecycleLine({
      participant: secretIdentity,
      track_name: `petal-camera-${secretIdentity}`,
      detail: secretDetail
    }),
    intervalLine({ participant: secretIdentity, track_name: `petal-camera-${secretIdentity}` })
  ].join('\n');

  const summary = summarizeCameraReceiverLog(text);
  const rendered = renderSummary(summary);
  const json = JSON.stringify(summary);
  for (const haystack of [rendered, json]) {
    assert.ok(!haystack.includes(secretIdentity), 'identity must not appear');
    assert.ok(!haystack.includes('super-secret'), 'free-text detail must not appear');
    assert.ok(!haystack.includes('petal-camera-alice'), 'full track name must not appear');
  }
  assert.ok(rendered.includes('kind=camera'), 'the bounded kind is still reported');
});

test('an absent optional field stays unknown rather than becoming zero', () => {
  const text = intervalLine({ packets_lost: 'unknown' }, { probe_starts: 'unknown' });
  const summary = summarizeCameraReceiverLog(text);
  assert.deepEqual(summary.sids[0]!.probeStarts, [], 'unknown probe starts is missing, not 0');
  assert.equal(summary.sids[0]!.observedUnknownPresentation, 1);
  assert.equal(summary.sids[0]!.probeGrowthMax, null);
  assert.equal(summary.verdicts.remountEvidence, false, 'missing evidence is not a positive claim');
  assert.equal(summary.sids[0]!.decodedFps.length, 1);
});

test('truncated and foreign lines are counted, not parsed as data', () => {
  const text = [
    'diagnostics: camera receiver lifecycle route=gallery-webview t_ms= phase=subscribed track_sid=TR_1 track_name=petal-camera-a',
    'diagnostics: camera receiver interval t_ms=1000',
    'diagnostics: unrelated media line',
    '',
    lifecycleLine({ t_ms: 5, phase: 'bridge_connected' })
  ].join('\n');
  const summary = summarizeCameraReceiverLog(text);
  assert.equal(summary.malformedLines, 2);
  assert.equal(summary.lifecycleLines, 1);
  assert.equal(summary.intervalLines, 0);
  assert.deepEqual(summary.sids[0]!.lifecycle, ['bridge_connected']);
  assert.equal(summary.sids[0]!.terminalPhase, null);
  assert.equal(summary.verdicts.hasTerminalPhase, false);
});

test('multiple SIDs stay separate, which is how a remount is proven', () => {
  const text = [
    lifecycleLine({ track_sid: 'TR_a', t_ms: 10, phase: 'subscribed' }),
    intervalLine({ track_sid: 'TR_a', t_ms: 15000 }),
    lifecycleLine({ track_sid: 'TR_b', t_ms: 20000, phase: 'subscribed' }),
    intervalLine({ track_sid: 'TR_b', t_ms: 35000 }),
    lifecycleLine({ track_sid: 'TR_b', t_ms: 40000, phase: 'unsubscribed' })
  ].join('\n');
  const summary = summarizeCameraReceiverLog(text);
  assert.equal(summary.sids.length, 2);
  assert.equal(summary.sids[0]!.intervals, 1);
  assert.equal(summary.sids[1]!.intervals, 1);
  assert.equal(summary.sids[0]!.terminalPhase, null);
  assert.equal(summary.sids[1]!.terminalPhase, 'unsubscribed');
  assert.deepEqual(
    summary.sids.map((s) => s.label),
    ['sid#1', 'sid#2']
  );
  assert.equal(summary.verdicts.hasTerminalPhase, true);
});

test('free-text detail with spaces cannot shift the fields after it', () => {
  const line = lifecycleLine({
    t_ms: 777,
    phase: 'lifecycle_failed',
    bridge_age_ms: 4242,
    detail: 'one two three four'
  });
  const parsed = parseCameraReceiverLines(line);
  assert.equal(parsed.lifecycles[0]!.tMs, 777);
  assert.equal(parsed.lifecycles[0]!.bridgeAgeMs, 4242, 'bridge_age_ms must not absorb detail text');
  assert.equal(parsed.lifecycles[0]!.phase, 'lifecycle_failed');
});

test('a window share is not misreported as a camera', () => {
  const text = intervalLine({ track_name: 'petal-window-42', track_sid: 'TR_w' });
  const summary = summarizeCameraReceiverLog(text);
  assert.equal(summary.sids[0]!.kind, 'window');
});
