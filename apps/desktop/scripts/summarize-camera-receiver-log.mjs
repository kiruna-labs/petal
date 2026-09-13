#!/usr/bin/env node
// Summarize the durable camera-receiver diagnostic lines from petal.log.
//
// Why this exists: the live acceptance for the receiver diagnostics gate
// ("one stable SID, progressing decode/presentation counters, no ~1 Hz probe
// churn, terminal cleanup") is otherwise a hand count over thousands of log
// lines. Hand counts are where acceptance claims go wrong.
//
// Privacy: this never prints participant identities, track names, free-form
// `detail`, or decoder implementation strings. It prints bounded enums
// (phase, stream state, stall cause, path protocol/candidate types) plus
// counters. SIDs are relabelled by first-appearance order so two runs can be
// compared without carrying the identifier.
//
// Usage:
//   node scripts/summarize-camera-receiver-log.mjs <petal.log>
//   node scripts/summarize-camera-receiver-log.mjs - < petal.log
//   node scripts/summarize-camera-receiver-log.mjs --json < petal.log

const LIFECYCLE_PREFIX = 'diagnostics: camera receiver lifecycle ';
const INTERVAL_PREFIX = 'diagnostics: camera receiver interval ';

/** Terminal phases: after one of these the SID must not produce more work. */
export const TERMINAL_PHASES = new Set([
  'unsubscribed',
  'bridge_disconnected',
  'lifecycle_failed'
]);

/** Bounded enums only. Free-form fields are deliberately never emitted. */
const INTERVAL_KEYS = [
  'route',
  't_ms',
  'trial_id',
  'track_sid',
  'track_name',
  'participant',
  'stream_state',
  'stall_cause',
  'interval_seq',
  'interval_ms',
  'decoded_dimensions',
  'frames_decoded',
  'decoded_fps',
  'frames_received',
  'frames_rendered',
  'frames_dropped',
  'freeze_count',
  'freeze_ms',
  'key_frames_decoded',
  'bytes_received',
  'packets_received',
  'packets_lost',
  'packets_discarded',
  'retransmitted_packets',
  'nack',
  'pli',
  'fir',
  'jitter_ms',
  'jitter_buffer_ms',
  'jitter_buffer_emitted',
  'decode_ms',
  'loss_pct',
  'presented_frames',
  'presented_fps',
  'gap_since_last_frame_ms',
  'decoder',
  'presentation',
  'path'
];

const PRESENTATION_KEYS = [
  'rvfc',
  'observing',
  'paused',
  'hidden',
  'ready_state',
  'probe_starts',
  'gaps_100ms',
  'gaps_250ms',
  'max_gap_ms',
  'excess_ms',
  'current_gap_ms'
];

const PATH_KEYS = [
  'protocol',
  'local',
  'remote',
  'relay',
  'selected_pair_changes',
  'rtt_ms',
  'available_in_kbps'
];

const LIFECYCLE_KEYS = [
  'route',
  't_ms',
  'phase',
  'participant',
  'track_sid',
  'track_name',
  'bridge_age_ms',
  'detail'
];

/** Split a `key=value key=value` tail on the KNOWN keys, in order.
 *
 * Values can contain spaces (lifecycle `detail` is free text), so a naive
 * `split(' ')` is wrong. Anchoring on the known key sequence keeps one
 * free-text field from shifting every later field. */
function readFields(text, keys) {
  const positions = [];
  for (const key of keys) {
    const match = new RegExp(`(?:^| )${key}=`).exec(text);
    if (match) positions.push({ key, start: match.index + match[0].length });
  }
  const out = {};
  for (let i = 0; i < positions.length; i += 1) {
    const end = i + 1 < positions.length ? positions[i + 1].start - positions[i + 1].key.length - 1 : text.length;
    const value = text.slice(positions[i].start, i + 1 < positions.length ? end - 1 : text.length);
    out[positions[i].key] = value;
  }
  return out;
}

function readPairs(text, keys) {
  const out = {};
  for (const key of keys) out[key] = 'unknown';
  const pattern = new RegExp(`(?:^| )(${keys.join('|')})=(\\S*)`, 'g');
  let match;
  while ((match = pattern.exec(text)) !== null) out[match[1]] = match[2];
  return out;
}

const numeric = (value) => {
  if (value === undefined || value === 'unknown') return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
};

/** `petal-camera-alice` -> `camera`; never returns the identity suffix. */
function trackKind(trackName) {
  if (trackName.startsWith('petal-camera-')) return 'camera';
  if (trackName.startsWith('petal-window-')) return 'window';
  return 'other';
}

export function parseCameraReceiverLines(text) {
  const lifecycles = [];
  const intervals = [];
  let malformed = 0;

  for (const rawLine of text.split(/\r?\n/)) {
    if (rawLine.startsWith(LIFECYCLE_PREFIX)) {
      const fields = readFields(rawLine.slice(LIFECYCLE_PREFIX.length), LIFECYCLE_KEYS);
      if (!fields.t_ms || !fields.phase) {
        malformed += 1;
        continue;
      }
      lifecycles.push({
        tMs: numeric(fields.t_ms),
        phase: fields.phase,
        trackSid: fields.track_sid ?? 'unknown',
        kind: trackKind(fields.track_name ?? ''),
        bridgeAgeMs: numeric(fields.bridge_age_ms)
      });
      continue;
    }
    if (rawLine.startsWith(INTERVAL_PREFIX)) {
      const fields = readFields(rawLine.slice(INTERVAL_PREFIX.length), INTERVAL_KEYS);
      if (!fields.t_ms || !fields.track_sid) {
        malformed += 1;
        continue;
      }
      const presentation = readPairs(fields.presentation ?? '', PRESENTATION_KEYS);
      const path = readPairs(fields.path ?? '', PATH_KEYS);
      intervals.push({
        tMs: numeric(fields.t_ms),
        sequence: numeric(fields.interval_seq),
        intervalMs: numeric(fields.interval_ms),
        trackSid: fields.track_sid,
        kind: trackKind(fields.track_name ?? ''),
        streamState: fields.stream_state,
        stallCause: fields.stall_cause,
        framesDecoded: numeric(fields.frames_decoded),
        decodedFps: numeric(fields.decoded_fps),
        framesReceived: numeric(fields.frames_received),
        framesDropped: numeric(fields.frames_dropped),
        freezeCount: numeric(fields.freeze_count),
        producedFrames: numeric(fields.presented_frames),
        presentedFps: numeric(fields.presented_fps),
        probeStarts: numeric(presentation.probe_starts),
        rvfc: presentation.rvfc,
        observing: presentation.observing,
        paused: presentation.paused,
        hidden: presentation.hidden,
        gaps100: numeric(presentation.gaps_100ms),
        gaps250: numeric(presentation.gaps_250ms),
        maxGapMs: numeric(presentation.max_gap_ms),
        protocol: path.protocol,
        localCandidate: path.local,
        remoteCandidate: path.remote,
        relay: path.relay
      });
      continue;
    }
  }
  return { lifecycles, intervals, malformed };
}

/** Growth per interval above this reads as restart churn, not a frame handoff. */
const PROBE_CHURN_PER_INTERVAL = 3;

export function summarizeCameraReceiverLog(text) {
  const { lifecycles, intervals, malformed } = parseCameraReceiverLines(text);
  const labels = new Map();
  const labelFor = (sid) => {
    if (!labels.has(sid)) labels.set(sid, `sid#${labels.size + 1}`);
    return labels.get(sid);
  };

  const sids = [...new Set([...lifecycles.map((l) => l.trackSid), ...intervals.map((i) => i.trackSid)])];
  const summaries = sids.map((sid) => {
    const ownLifecycles = lifecycles.filter((l) => l.trackSid === sid);
    const ownIntervals = intervals.filter((i) => i.trackSid === sid);
    const first = ownIntervals[0] ?? null;
    const last = ownIntervals.at(-1) ?? null;
    const probeDeltas = ownIntervals
      .map((interval) => interval.probeStarts)
      .filter((value) => value !== null)
      .map((value, index, values) => (index === 0 ? 0 : value - values[index - 1]));
    const sequences = ownIntervals.map((i) => i.sequence).filter((value) => value !== null);
    const kind = ownLifecycles[0]?.kind ?? first?.kind ?? 'other';

    return {
      label: labelFor(sid),
      kind,
      lifecycle: ownLifecycles.map((l) => l.phase),
      terminalPhase: ownLifecycles.map((l) => l.phase).filter((phase) => TERMINAL_PHASES.has(phase)).at(-1) ?? null,
      intervals: ownIntervals.length,
      windowMs: first && last ? { first: first.tMs, last: last.tMs } : null,
      decodedProgress: first && last ? { first: first.framesDecoded, last: last.framesDecoded } : null,
      presentedProgress: first && last ? { first: first.producedFrames, last: last.producedFrames } : null,
      decodedFps: ownIntervals.map((i) => i.decodedFps).filter((value) => value !== null),
      presentedFps: ownIntervals.map((i) => i.presentedFps).filter((value) => value !== null),
      probeStarts: ownIntervals.map((i) => i.probeStarts).filter((value) => value !== null),
      probeGrowthMax: probeDeltas.length ? Math.max(...probeDeltas) : null,
      probeChurn: probeDeltas.some((delta) => delta > PROBE_CHURN_PER_INTERVAL),
      sequenceMonotonic: sequences.every((value, index) => index === 0 || value >= sequences[index - 1]),
      rvfcAvailable: ownIntervals.some((i) => i.rvfc === 'true'),
      observedUnknownPresentation: ownIntervals.filter((i) => i.probeStarts === null).length,
      streamStates: [...new Set(ownIntervals.map((i) => i.streamState))].filter((v) => v && v !== 'unknown'),
      stallCauses: [...new Set(ownIntervals.map((i) => i.stallCause))].filter((v) => v && v !== 'unknown' && v !== 'none'),
      path: {
        protocols: [...new Set(ownIntervals.map((i) => i.protocol))].filter((v) => v && v !== 'unknown'),
        local: [...new Set(ownIntervals.map((i) => i.localCandidate))].filter((v) => v && v !== 'unknown'),
        remote: [...new Set(ownIntervals.map((i) => i.remoteCandidate))].filter((v) => v && v !== 'unknown'),
        relay: [...new Set(ownIntervals.map((i) => i.relay))].filter((v) => v && v !== 'unknown')
      },
      lastGaps: last ? { gaps100: last.gaps100, gaps250: last.gaps250, maxGapMs: last.maxGapMs } : null
    };
  });

  const withTerminal = summaries.filter((s) => s.terminalPhase !== null).length;
  return {
    malformedLines: malformed,
    lifecycleLines: lifecycles.length,
    intervalLines: intervals.length,
    sids: summaries,
    verdicts: {
      hasTerminalPhase: withTerminal > 0,
      anyProbeChurn: summaries.some((s) => s.probeChurn),
      allSequencesMonotonic: summaries.every((s) => s.sequenceMonotonic),
      decodedProgressed: summaries.some((s) => (s.decodedProgress?.last ?? 0) > (s.decodedProgress?.first ?? 0)),
      presentedProgressed: summaries.some((s) => (s.presentedProgress?.last ?? 0) > (s.presentedProgress?.first ?? 0)),
      // The durable line has no `generation` field: a true remount is evidenced
      // by a new SID or a probe-start step, never by a generation counter.
      remountEvidence: summaries.some((s) => (s.probeStarts[0] ?? 0) > 0)
    }
  };
}

export function renderSummary(summary) {
  const lines = [
    `camera receiver lines: lifecycle=${summary.lifecycleLines} interval=${summary.intervalLines} malformed=${summary.malformedLines}`,
    `sids=${summary.sids.length}`
  ];
  for (const sid of summary.sids) {
    lines.push(`- ${sid.label} kind=${sid.kind}`);
    lines.push(`    lifecycle=${sid.lifecycle.join(' -> ') || 'none'}`);
    lines.push(`    terminal=${sid.terminalPhase ?? 'none'}`);
    lines.push(`    intervals=${sid.intervals} sequenceMonotonic=${sid.sequenceMonotonic}`);
    lines.push(`    decoded=${sid.decodedProgress?.first ?? 'unknown'} -> ${sid.decodedProgress?.last ?? 'unknown'} presented=${sid.presentedProgress?.first ?? 'unknown'} -> ${sid.presentedProgress?.last ?? 'unknown'}`);
    lines.push(`    probeStarts=[${sid.probeStarts.join(',')}] maxGrowthPerInterval=${sid.probeGrowthMax ?? 'unknown'} churn=${sid.probeChurn}`);
    lines.push(`    rvfcAvailable=${sid.rvfcAvailable} unknownPresentationIntervals=${sid.observedUnknownPresentation}`);
    lines.push(`    path protocols=[${sid.path.protocols.join(',')}] local=[${sid.path.local.join(',')}] remote=[${sid.path.remote.join(',')}] relay=[${sid.path.relay.join(',')}]`);
    lines.push(`    streamStates=[${sid.streamStates.join(',')}] stallCauses=[${sid.stallCauses.join(',')}]`);
  }
  lines.push(`verdicts: ${JSON.stringify(summary.verdicts)}`);
  return lines.join('\n');
}

import { realpathSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

/** Only run the CLI when this file is the entry point.
 *
 * A suffix check is not enough: the test file is named
 * `test-summarize-camera-receiver-log.mjs`, so `endsWith` would make importing
 * this module execute main() and print a spurious empty summary. */
function invokedDirectly() {
  if (!process.argv[1]) return false;
  try {
    return realpathSync(process.argv[1]) === fileURLToPath(import.meta.url);
  } catch {
    return false;
  }
}

async function main(argv) {
  const json = argv.includes('--json');
  const path = argv.find((arg) => !arg.startsWith('--'));
  let text;
  if (!path || path === '-') {
    const chunks = [];
    for await (const chunk of process.stdin) chunks.push(chunk);
    text = Buffer.concat(chunks).toString('utf8');
  } else {
    const { readFile } = await import('node:fs/promises');
    text = await readFile(path, 'utf8');
  }
  const summary = summarizeCameraReceiverLog(text);
  process.stdout.write(`${json ? JSON.stringify(summary, null, 2) : renderSummary(summary)}\n`);
}

if (invokedDirectly()) {
  main(process.argv.slice(2)).catch((error) => {
    process.stderr.write(`summarize-camera-receiver-log: ${error.message}\n`);
    process.exitCode = 1;
  });
}
