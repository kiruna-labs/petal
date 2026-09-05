import assert from 'node:assert/strict';
import test from 'node:test';

import { cameraPublishSyncPlan } from '../src/lib/ipc';

// Every platform has a webview self-view now (Windows: native-fed canvas
// stream; macOS: getUserMedia), so ANY camera intent — publishing OR still
// retrying — acquires the preview. `previewRequired` is gone from the
// snapshot; Windows now uses the native-fed canvas self-view instead of an
// SFU loopback.

test('camera publish sync activates nothing and previews nothing when fully off', () => {
  assert.deepEqual(cameraPublishSyncPlan({ publishing: false, intended: false }), {
    activate: false,
    acquirePreview: false
  });
});

test('camera publish sync shows retrying intent as needing the preview', () => {
  assert.deepEqual(cameraPublishSyncPlan({ publishing: false, intended: true }), {
    activate: false,
    acquirePreview: true
  });
});

test('camera publish sync restores a live publication with its preview', () => {
  assert.deepEqual(cameraPublishSyncPlan({ publishing: true, intended: true }), {
    activate: true,
    acquirePreview: true
  });
});

test('camera publish sync restores a live publication even if intent reads off', () => {
  assert.deepEqual(cameraPublishSyncPlan({ publishing: true, intended: false }), {
    activate: true,
    acquirePreview: true
  });
});
