import { test } from 'node:test';
import assert from 'node:assert/strict';

import { shareErrorDisplay } from '../src/lib/data/shareErrors.ts';

test('permission denied share errors open Screen Recording recovery', () => {
  assert.deepEqual(shareErrorDisplay({ kind: 'permissionDenied' }), {
    message: 'Screen Recording is off - enable Petal in Privacy & Security, then relaunch',
    openScreenRecordingSettings: true
  });
});

test('too many shares includes the backend limit', () => {
  assert.deepEqual(shareErrorDisplay({ kind: 'tooManyShares', message: 4 }), {
    message: 'You can share up to 4 windows - stop one before sharing another',
    openScreenRecordingSettings: false
  });
});

test('unknown share errors still produce a visible message', () => {
  assert.deepEqual(shareErrorDisplay('native bridge failed'), {
    message: 'Could not share window - native bridge failed',
    openScreenRecordingSettings: false
  });
});
