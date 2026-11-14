import assert from 'node:assert/strict';
import test from 'node:test';

import { readFileSync } from 'node:fs';

import { PhotonCorrelator } from '@petal/shared/logic/photonCorrelator';
import {
  REMOTE_TEXT_CHUNK_CHARS,
  remoteControlInputRttMs,
  remoteTextChunkCount,
  reserveSeqs,
  stampSentAtMs,
  wireMs
} from '@petal/shared/logic/remoteControlTiming';

test('#371: stampSentAtMs adds an integer stamp and keeps an existing one', () => {
  assert.deepEqual(stampSentAtMs({ kind: 'key', seq: 3 }, 1_700_000_000_123.7), {
    kind: 'key',
    seq: 3,
    sentAtMs: 1_700_000_000_123
  });
  const produced = { kind: 'text', sentAtMs: 42 };
  assert.equal(stampSentAtMs(produced, 99), produced, 'production stamp wins');
  assert.equal(stampSentAtMs({ sentAtMs: -1 }, 7).sentAtMs, 7, 'an invalid stamp is replaced');
});

test('#371: input rtt subtracts the host hold and needs it', () => {
  assert.equal(remoteControlInputRttMs(1_000, 1_090, 12), 78);
  assert.equal(remoteControlInputRttMs(1_000, 1_090, undefined), undefined, 'old host');
  assert.equal(remoteControlInputRttMs(undefined, 1_090, 12), undefined, 'unstamped send');
  assert.equal(remoteControlInputRttMs(1_000, 1_005, 10), undefined, 'negative is dropped');
  assert.equal(wireMs(1.5), undefined);
  assert.equal(wireMs('5'), undefined);
  assert.equal(wireMs(0), 0);
});

// #376: Rust numbers an oversized text's chunks seq, seq+1, ... A click after
// it must get a seq no chunk used, or the host's inputFrame for that chunk
// would resolve the click against the wrong frame.
test('#376: a chunked text reserves every chunk seq, so the next click never reuses one', () => {
  assert.equal(remoteTextChunkCount(''), 1);
  assert.equal(remoteTextChunkCount('x'.repeat(REMOTE_TEXT_CHUNK_CHARS)), 1);
  assert.equal(remoteTextChunkCount('x'.repeat(REMOTE_TEXT_CHUNK_CHARS + 1)), 2);
  assert.equal(remoteTextChunkCount('😀'.repeat(REMOTE_TEXT_CHUNK_CHARS)), 1, 'scalar values, not UTF-16 units');

  // The overlay's counter: text first, then a click.
  let seq = 41;
  const text = 'a'.repeat(REMOTE_TEXT_CHUNK_CHARS * 2 + 5);
  const reserved = reserveSeqs(seq, remoteTextChunkCount(text));
  seq = reserved.last;
  const chunkSeqs = [0, 1, 2].map((index) => reserved.first + index); // Rust: seq + index
  seq += 1;
  const clickSeq = seq;
  assert.deepEqual(chunkSeqs, [42, 43, 44]);
  assert.ok(!chunkSeqs.includes(clickSeq), `click seq ${clickSeq} reused a chunk seq`);
  assert.deepEqual(reserveSeqs(Number.MAX_SAFE_INTEGER - 1, 3), { first: 1, last: 3 }, 'wraps like nextSeq');

  // The host announces chunk 2 (the controller never tracked chunks); it
  // must not resolve the click.
  const correlator = new PhotonCorrelator();
  correlator.noteSent(clickSeq, 1_000);
  correlator.notePresented('TR', 7, 1_020);
  assert.deepEqual(correlator.noteInputFrame(chunkSeqs[1], 'TR', 7, 1_030), []);
  assert.deepEqual(correlator.noteInputFrame(clickSeq, 'TR', 7, 1_031), [{ kind: 'photon', seq: clickSeq, ms: 20 }]);
});

test('#376: the desktop overlay reserves text seqs through the shared helper', () => {
  const page = readFileSync(new URL('../src/routes/compositor/control/+page.svelte', import.meta.url), 'utf8');
  const body = page.slice(page.indexOf('function sendComposedText('), page.indexOf('function onCompositionStart('));
  assert.match(body, /reserveSeqs\(seq, remoteTextChunkCount\(text\)\)/);
  assert.match(body, /seq = reserved\.last;/);
  assert.match(body, /seq: reserved\.first,/);
  assert.doesNotMatch(body, /nextSeq\(\)/);
});
