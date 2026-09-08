// #73: participant metadata is one whole-blob field, and `livekit-client` only
// updates `localParticipant.metadata` on the SERVER echo. These tests pin the
// behaviour that makes concurrent writers safe: every merge reads the last
// LOCALLY-known blob, and keys we own are re-applied (boundedly) if an echo
// comes back without them.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { createMetadataOwner, MetadataMergeError } from '../src/participantMetadata.ts';
import { mergeIdentityPaletteIndexMetadata, mergeSharedSourceMetadata } from '../src/trackNames.ts';
import { mergePluginMetadata, pluginsFromMetadata } from '@petal/shared/plugin-host/metadata';

const tick = () => new Promise((r) => setImmediate(r));

/**
 * A local participant with livekit's echo semantics: `metadata` only changes
 * when the server echoes a write back, `echoMs` later. That delay is the whole
 * bug -- a second writer reading `metadata` in the meantime sees a stale base.
 */
class FakeLocalParticipant {
  metadata: string | undefined;
  readonly writes: string[] = [];
  private readonly echoMs: number;
  private readonly onEcho: ((metadata: string) => void) | undefined;

  constructor(options: { metadata?: string; echoMs?: number; onEcho?: (metadata: string) => void } = {}) {
    this.metadata = options.metadata;
    this.echoMs = options.echoMs ?? 5;
    this.onEcho = options.onEcho;
  }

  setMetadata = (metadata: string): Promise<void> => {
    this.writes.push(metadata);
    return new Promise<void>((resolve) => {
      setTimeout(() => {
        // Last writer wins on the server, exactly like the real thing.
        this.metadata = metadata;
        this.onEcho?.(metadata);
        resolve();
      }, this.echoMs);
    });
  };
}

test('two writers racing on different keys keep both (#73)', async () => {
  const participant = new FakeLocalParticipant({ echoMs: 20 });
  const owner = createMetadataOwner({ echoWaitMs: 5 });
  owner.attach(participant);

  // The real race: the plugin host advertises on RoomEvent.Connected while
  // connection.ts merges the palette index right after connect. Neither write
  // has echoed when the other one merges.
  const advert = owner.update((current) =>
    mergePluginMetadata(current, 'petal.reactions', { v: '1.0.0', src: 'builtin' }),
  );
  const palette = owner.update((current) => mergeIdentityPaletteIndexMetadata(current, 3));
  await Promise.all([advert, palette]);
  // Let every queued write and its echo land.
  await new Promise((r) => setTimeout(r, 80));
  owner.onEcho(participant.metadata);
  await new Promise((r) => setTimeout(r, 80));

  const landed = participant.metadata;
  assert.ok(landed, 'a blob reached the server');
  const root = JSON.parse(landed!) as Record<string, unknown>;
  assert.deepEqual(
    pluginsFromMetadata(landed)['petal.reactions'],
    { v: '1.0.0', src: 'builtin' },
    'the plugin advert survived the concurrent palette write',
  );
  assert.equal(root.petalIdentityPaletteIndex, 3, 'the palette index survived the concurrent advert write');
});

test('a share start racing a plugin advert keeps both keys', async () => {
  const participant = new FakeLocalParticipant({ echoMs: 30 });
  const owner = createMetadataOwner({ echoWaitMs: 5 });
  owner.attach(participant);

  const share = owner.update((current) => mergeSharedSourceMetadata(current, 7, 'window'));
  const advert = owner.update((current) => mergePluginMetadata(current, 'petal.notes', { v: '1.0.0', src: 'builtin' }));
  await Promise.all([share, advert]);
  await new Promise((r) => setTimeout(r, 120));

  const root = JSON.parse(participant.metadata!) as Record<string, Record<string, unknown>>;
  assert.deepEqual(root.petalWindowKinds, { '7': 'window' }, 'the share kind survived');
  assert.ok(pluginsFromMetadata(participant.metadata)['petal.notes'], 'the plugin advert survived');
});

test('the old read-modify-write shape is what drops a key', async () => {
  // The defect this fix removes, reproduced against the same fake: both
  // writers read `participant.metadata` (livekit's echo-lagged value), so the
  // later write goes out without the earlier one's key.
  const participant = new FakeLocalParticipant({ echoMs: 20 });
  await Promise.all([
    participant.setMetadata(mergePluginMetadata(participant.metadata, 'petal.reactions', { v: '1.0.0', src: 'builtin' })),
    participant.setMetadata(mergeIdentityPaletteIndexMetadata(participant.metadata, 3)),
  ]);
  assert.equal(
    pluginsFromMetadata(participant.metadata)['petal.reactions'],
    undefined,
    'read-modify-write from the echoed blob loses the concurrent key',
  );
});

test('an echo that dropped one of our keys is re-applied', async () => {
  const participant = new FakeLocalParticipant({ echoMs: 1 });
  const owner = createMetadataOwner({ echoWaitMs: 5 });
  owner.attach(participant);
  await owner.update((current) => mergePluginMetadata(current, 'petal.reactions', { v: '1.0.0', src: 'builtin' }));
  await tick();

  // Someone else's whole-blob write landed on the server without our key.
  owner.onEcho(JSON.stringify({ petalIdentityPaletteIndex: 2 }));
  await new Promise((r) => setTimeout(r, 20));

  const root = JSON.parse(participant.metadata!) as Record<string, unknown>;
  assert.ok(pluginsFromMetadata(participant.metadata)['petal.reactions'], 'our key was re-applied');
  assert.equal(root.petalIdentityPaletteIndex, 2, 'a key we never wrote is adopted from the echo, not clobbered');
});

test('re-applies are bounded when the key can never land', async () => {
  const participant = new FakeLocalParticipant({ echoMs: 1 });
  const warnings: string[] = [];
  const owner = createMetadataOwner({
    echoWaitMs: 5,
    maxReapplyAttempts: 3,
    log: (line, kind) => {
      if (kind === 'warn') warnings.push(line);
    },
  });
  owner.attach(participant);
  await owner.update((current) => mergePluginMetadata(current, 'petal.reactions', { v: '1.0.0', src: 'builtin' }));
  await new Promise((r) => setTimeout(r, 10));
  const beforeWrites = participant.writes.length;

  // A server that refuses the key: every echo comes back without it.
  for (let i = 0; i < 10; i += 1) {
    owner.onEcho('{}');
    await new Promise((r) => setTimeout(r, 5));
  }
  assert.equal(participant.writes.length - beforeWrites, 3, 're-applies stop at the cap');
  assert.equal(warnings.length, 1, 'the cap is reported once, not per echo');

  // A NEW write is a new intent and gets its own budget.
  await owner.update((current) => mergeIdentityPaletteIndexMetadata(current, 1));
  await new Promise((r) => setTimeout(r, 10));
  owner.onEcho('{}');
  await new Promise((r) => setTimeout(r, 10));
  assert.ok(participant.writes.length - beforeWrites > 4, 'the budget resets on the next explicit write');
});

test('a rejected merge does not touch the locally-known blob', async () => {
  const participant = new FakeLocalParticipant({ echoMs: 1 });
  const owner = createMetadataOwner({ echoWaitMs: 5 });
  owner.attach(participant);
  await owner.update((current) => mergeIdentityPaletteIndexMetadata(current, 4));
  const before = owner.current();
  await assert.rejects(
    owner.update(() => {
      throw new Error('plugin state exceeds 2048 bytes');
    }),
    (err: unknown) => err instanceof MetadataMergeError && /2048/.test((err as Error).message),
  );
  assert.equal(owner.current(), before, 'the failed merge left the state alone');
});

test('attach seeds from the participant and detach forgets it', async () => {
  const participant = new FakeLocalParticipant({ metadata: JSON.stringify({ fromToken: 'x' }), echoMs: 1 });
  const owner = createMetadataOwner({ echoWaitMs: 5 });
  owner.attach(participant);
  assert.deepEqual(JSON.parse(owner.current()), { fromToken: 'x' });
  await owner.update((current) => mergeIdentityPaletteIndexMetadata(current, 5));
  await new Promise((r) => setTimeout(r, 10));
  assert.deepEqual(JSON.parse(participant.metadata!), { fromToken: 'x', petalIdentityPaletteIndex: 5 }, 'token metadata is preserved');

  owner.detach();
  assert.equal(owner.current(), '{}');
  const writes = participant.writes.length;
  await owner.update((current) => mergeIdentityPaletteIndexMetadata(current, 6));
  assert.equal(participant.writes.length, writes, 'a detached owner writes to nobody');
});

test('no web call site does its own metadata read-modify-write', () => {
  // The owner is only worth having if nothing routes around it. Both files
  // used to hold their own `setLocalParticipantMetadata(participant, merge(participant.metadata, ...))`.
  for (const file of ['../src/controls.ts', '../src/connection.ts', '../src/plugins/webAdapter.ts']) {
    const source = readFileSync(new URL(file, import.meta.url), 'utf8');
    assert.equal(source.includes('setLocalParticipantMetadata'), false, `${file} still has its own metadata writer`);
    assert.equal(
      /merge\w*Metadata\(\s*[\w.]*localParticipant\.metadata/.test(source),
      false,
      `${file} still merges from the echo-lagged localParticipant.metadata`,
    );
    assert.equal(
      /localParticipant\s*(as[^)]*)?\)?\.setMetadata\(/.test(source),
      false,
      `${file} still calls setMetadata directly instead of going through the owner`,
    );
  }
});
