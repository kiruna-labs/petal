import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createTopicDispatcher } from '../src/dataTopics.ts';

test('exact and prefix handlers both fire; unknown topics warn once', () => {
  const seen: string[] = [];
  const unknown: string[] = [];
  const d = createTopicDispatcher((t) => unknown.push(t));
  d.on('petal.draw', (p, _pt, t) => seen.push(`draw:${t}:${p.length}`));
  d.on('petal.draw', () => seen.push('draw-second'));
  d.onPrefix('plugin/', (_p, _pt, t) => seen.push(`plugin:${t}`));

  assert.equal(d.dispatch(new Uint8Array(3), undefined, 'petal.draw'), true);
  assert.equal(d.dispatch(new Uint8Array(0), undefined, 'plugin/petal.reactions/emoji'), true);
  assert.equal(d.dispatch(new Uint8Array(0), undefined, 'petal.unknown'), false);
  assert.equal(d.dispatch(new Uint8Array(0), undefined, 'petal.unknown'), false);
  assert.equal(d.dispatch(new Uint8Array(0), undefined, undefined), false);
  assert.deepEqual(seen, ['draw:petal.draw:3', 'draw-second', 'plugin:plugin/petal.reactions/emoji']);
  assert.deepEqual(unknown, ['petal.unknown'], 'warned exactly once');
});
