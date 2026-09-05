import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { EVENTS } from '../src/lib/ipc.ts';

const galleryBridge = readFileSync(
  new URL('../src/lib/data/galleryBridge.ts', import.meta.url),
  'utf8'
);
const participantTile = readFileSync(
  new URL('../src/lib/components/ParticipantTile.svelte', import.meta.url),
  'utf8'
);
const gallery = readFileSync(
  new URL('../src/lib/components/Gallery.svelte', import.meta.url),
  'utf8'
);
const meetingRoute = readFileSync(
  new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
  'utf8'
);
const drawRs = readFileSync(new URL('../src-tauri/src/draw.rs', import.meta.url), 'utf8');

function cameraWindowId(trackName: string): number {
  let hash = 0x811c_9dc5;
  for (let index = 0; index < trackName.length; index += 1) {
    hash ^= trackName.charCodeAt(index) & 0xff;
    hash = Math.imul(hash, 0x0100_0193) >>> 0;
  }
  return (hash | 0x8000_0000) >>> 0;
}

test('desktop camera draw ids match the shared high-bit camera track contract', () => {
  assert.match(galleryBridge, /export function cameraTrackNameForIdentity\(identity: string\): string/);
  assert.match(galleryBridge, /export function cameraWindowId\(trackName: string\): number/);
  assert.match(galleryBridge, /hash = Math\.imul\(hash, 0x0100_0193\) >>> 0;/);
  assert.match(galleryBridge, /return \(hash \| 0x8000_0000\) >>> 0;/);
  const trackName = 'petal-camera-web-tester';
  assert.equal(trackName, 'petal-camera-web-tester');
  assert.equal(cameraWindowId(trackName), cameraWindowId(trackName));
  assert.notEqual(cameraWindowId(trackName), cameraWindowId('petal-camera-other'));
  assert.equal((cameraWindowId(trackName) & 0x8000_0000) >>> 0, 0x8000_0000);
});

test('camera draw updates use a dedicated event and stay out of remote-window ids', () => {
  assert.equal(EVENTS.drawUpdate, 'draw-update');
  assert.match(drawRs, /const DRAW_UPDATE_EVENT: &str = "draw-update";/);
  assert.match(drawRs, /if is_camera_window_id\(update\.window_id\) \{[\s\S]*app\.emit\(DRAW_UPDATE_EVENT, update\)/);
  assert.doesNotMatch(gallery, /data-window-id=\{.*drawWindowId/);
  assert.doesNotMatch(participantTile, /data-window-id/);
});

test('participant tiles render matched camera draw strokes without payload colors', () => {
  assert.match(participantTile, /ownerIdentity\?: string;/);
  assert.match(participantTile, /drawWindowId\?: number;/);
  assert.match(participantTile, /update\.ownerIdentity !== ownerIdentity \|\| update\.windowId !== drawWindowId/);
  assert.match(participantTile, /identityColorFromPaletteIndex\(update\.drawerPaletteIndex\) \?\? colorForIdentity\(update\.drawerIdentity\)/);
  assert.doesNotMatch(participantTile, /update\.color/);
  assert.match(participantTile, /<svg class="draw-layer" viewBox="0 0 1 1" preserveAspectRatio="none"/);
});

test('meeting route forwards only high-bit camera draw updates to gallery', () => {
  assert.match(meetingRoute, /listen<DrawUpdate>\(EVENTS\.drawUpdate/);
  assert.match(meetingRoute, /if \(\(event\.payload\.windowId & 0x8000_0000\) === 0\) return;/);
  assert.match(meetingRoute, /cameraDrawUpdates = \[\.\.\.cameraDrawUpdates\.slice\(-240\), event\.payload\]/);
  assert.match(meetingRoute, /<MeetingChrome[\s\S]*\{cameraDrawUpdates\}/);
  assert.match(gallery, /<ParticipantTile[\s\S]*drawWindowId=\{p\.drawWindowId\}[\s\S]*drawUpdates=\{cameraDrawUpdates\}/);
});
