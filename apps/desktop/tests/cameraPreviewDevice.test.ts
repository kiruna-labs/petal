import assert from 'node:assert/strict';
import test from 'node:test';

import { previewCameraIdFor } from '../src/lib/data/cameraPreviewDevice';

const native = [
  { id: 'native-builtin', name: 'MacBook Pro Camera' },
  { id: 'native-desk-view', name: 'MacBook Pro Desk View Camera' },
  { id: 'native-camlink', name: 'Cam Link 4K' }
];
const web = [
  { deviceId: 'web-camlink', label: 'Cam Link 4K' },
  { deviceId: 'web-builtin', label: 'MacBook Pro Camera' },
  { deviceId: 'web-desk-view', label: 'MacBook Pro Desk View Camera' }
];

test('the selected native camera maps to the WebKit camera with the same name', () => {
  assert.equal(previewCameraIdFor('native-builtin', native, web), 'web-builtin');
  assert.equal(previewCameraIdFor('native-camlink', native, web), 'web-camlink');
});

test('a name that prefixes another camera name only matches itself', () => {
  assert.equal(previewCameraIdFor('native-desk-view', native, web), 'web-desk-view');
});

test('no selection or an unknown native id falls back to the default camera', () => {
  assert.equal(previewCameraIdFor('', native, web), undefined);
  assert.equal(previewCameraIdFor(undefined, native, web), undefined);
  assert.equal(previewCameraIdFor('native-unplugged', native, web), undefined);
});

test('unlabeled WebKit devices (before a stream is granted) fall back to the default', () => {
  const unlabeled = web.map((c) => ({ ...c, label: '' }));
  assert.equal(previewCameraIdFor('native-builtin', native, unlabeled), undefined);
});

test('two WebKit cameras with the same name are ambiguous and fall back to the default', () => {
  const twins = [...web, { deviceId: 'web-builtin-2', label: 'MacBook Pro Camera' }];
  assert.equal(previewCameraIdFor('native-builtin', native, twins), undefined);
});
