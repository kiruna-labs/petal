import assert from 'node:assert/strict';
import test from 'node:test';
import {
  LIVE_STREAM_MENU_ITEM_ID,
  LIVE_STREAM_MENU_LABELS,
  buildHoverTabMenuEntries
} from '../src/lib/data/hoverTabMenu.ts';
import { dispatchShareOptionsMenuEntry } from '../src/lib/shareOptionsPopup.ts';
import { readFileSync } from 'node:fs';
import {
  HIDDEN_MEETING_LABEL,
  LIVE_MEETING_LABEL,
  formatStreamElapsed,
  isShareOnAir,
  meetingLabelForDisplay,
  parseMaxBitrateMbps,
  recentRoomLabelForDisplay,
  showsOptOutToggle,
  streamEndedMessage,
  streamIsDownscaled,
  streamPreviewLine,
  streamStatusLine
} from '../src/lib/data/liveStream.ts';
import {
  CUSTOM_STREAM_SERVICE_ID,
  STREAM_SERVICES,
  STREAM_SERVICES_VERIFIED_ON,
  streamServiceById,
  streamServiceForUrl
} from '../src/lib/data/streamServices.ts';
import type {
  LiveMenuAction,
  StreamLiveEntry,
  StreamLiveState,
  StreamPreview,
  StreamSessionSnapshot
} from '../src/lib/ipc.ts';

function entriesWith(shared: boolean, liveAction: LiveMenuAction | null) {
  return buildHoverTabMenuEntries(
    'automatic', shared, false, 'cursorPreserving', true, false, false, false, false, true,
    'right', false, false, undefined, liveAction
  );
}

test('the live stream entry appears only on a shared window with an action', () => {
  assert.ok(!entriesWith(true, null).some((e) => e.kind === 'live-stream'));
  assert.ok(!entriesWith(false, 'goLive').some((e) => e.kind === 'live-stream'));
  for (const action of ['goLive', 'stop', 'resume'] as const) {
    const entries = entriesWith(true, action);
    const entry = entries.at(-1);
    assert.deepEqual(entry, {
      kind: 'live-stream',
      id: LIVE_STREAM_MENU_ITEM_ID,
      text: LIVE_STREAM_MENU_LABELS[action],
      action
    });
    assert.deepEqual(entries.at(-2), { kind: 'separator' });
  }
});

test('the live stream entry dispatches its action', () => {
  const entry = entriesWith(true, 'stop').at(-1)!;
  const seen: LiveMenuAction[] = [];
  const noop = () => {};
  const action = dispatchShareOptionsMenuEntry(entry, {
    onPriority: noop,
    onControlMode: noop,
    onDebug: noop,
    onLiveStream: (a) => seen.push(a)
  } as never);
  action?.();
  assert.deepEqual(seen, ['stop']);
});

const preview: StreamPreview = {
  captureWidth: 2560,
  captureHeight: 1440,
  shareFps: 30,
  plan: { width: 1920, height: 1080, fps: 30, bitrateBps: 6_000_000, maxBitrateBps: 7_200_000, resolution: '1080p' },
  clientAvailable: true
};

test('preview line and downscale flag', () => {
  assert.equal(streamPreviewLine(preview), '2560×1440 source → 1920×1080 @ 30 fps, ~6 Mbps');
  assert.equal(streamIsDownscaled(preview), true);
  assert.equal(
    streamIsDownscaled({ ...preview, captureWidth: 1920, captureHeight: 1080 }),
    false
  );
  // Wider than 16:9 but fitting: scaled up, letterboxed, not downscaled.
  assert.equal(streamIsDownscaled({ ...preview, captureWidth: 1708, captureHeight: 732 }), false);
  assert.equal(streamIsDownscaled({ ...preview, captureWidth: 2000, captureHeight: 800 }), true);
});

test('max bitrate parsing', () => {
  assert.equal(parseMaxBitrateMbps(''), null);
  assert.equal(parseMaxBitrateMbps(' Auto '), null);
  assert.equal(parseMaxBitrateMbps('8'), 8_000_000);
  assert.equal(parseMaxBitrateMbps('2.5'), 2_500_000);
  assert.equal(parseMaxBitrateMbps('0.5'), undefined);
  assert.equal(parseMaxBitrateMbps('51'), undefined);
  assert.equal(parseMaxBitrateMbps('8 Mbps'), undefined);
  assert.equal(parseMaxBitrateMbps('-3'), undefined);
});

test('elapsed formatting', () => {
  assert.equal(formatStreamElapsed(-5), '0:00');
  assert.equal(formatStreamElapsed(245_000), '4:05');
  assert.equal(formatStreamElapsed(3_723_000), '1:02:03');
});

test('status line per phase', () => {
  const base: StreamSessionSnapshot = {
    phase: 'live',
    windowId: 7,
    startedAt: 1_000,
    width: 1920,
    height: 1080,
    fps: 30,
    targetBitrateBps: 6_000_000,
    actualBitrateBps: 5_800_000,
    droppedFrames: 0,
    slateRemainingS: null,
    reconnectAttempt: null,
    hardwareEncoder: true
  };
  const now = 1_000 + 245_000;
  assert.equal(streamStatusLine(base, now), 'LIVE 4:05 · 5.8 Mbps');
  assert.equal(streamStatusLine({ ...base, droppedFrames: 3 }, now), 'LIVE 4:05 · 5.8 Mbps · 3 dropped');
  assert.equal(
    streamStatusLine({ ...base, phase: 'paused', slateRemainingS: 97 }, now),
    'LIVE 4:05 · Be right back · ends in 97s'
  );
  assert.equal(
    streamStatusLine({ ...base, phase: 'reconnecting', reconnectAttempt: 2 }, now),
    'Reconnecting (attempt 2 of 3)…'
  );
  assert.equal(streamStatusLine({ ...base, phase: 'connecting' }, now), 'Connecting to the streaming server…');
});

test('ended messages', () => {
  assert.match(streamEndedMessage('slate-timeout'), /2 minutes/);
  assert.equal(streamEndedMessage('failed', 'connection refused'), 'Live stream failed: connection refused');
  assert.equal(streamEndedMessage('failed'), 'Live stream failed');
});

test('service presets are well-formed RTMP(S) URLs with no key or credentials', () => {
  assert.match(STREAM_SERVICES_VERIFIED_ON, /^\d{4}-\d{2}-\d{2}$/);
  const ids = new Set<string>();
  for (const service of STREAM_SERVICES) {
    assert.ok(!ids.has(service.id), `duplicate id ${service.id}`);
    ids.add(service.id);
    if (service.id === CUSTOM_STREAM_SERVICE_ID) {
      assert.equal(service.serverUrl, '');
      continue;
    }
    assert.match(service.serverUrl, /^rtmps:\/\/[a-z0-9.-]+(:\d+)?\/[a-z0-9]+\/?$/);
    assert.ok(service.helpUrl?.startsWith('https://'), `${service.id} help link`);
  }
  assert.equal(STREAM_SERVICES.at(-1)?.id, CUSTOM_STREAM_SERVICE_ID);
});

test('every live share in the room carries the badge, not only the earliest', () => {
  // The streams Rust derives from the shared contract fixture, earliest first.
  const contracts = JSON.parse(
    readFileSync(new URL('../../../contracts/petal-contracts.json', import.meta.url), 'utf8')
  ) as { streamLiveDerivation: { streams: StreamLiveEntry[] } };
  const streams = contracts.streamLiveDerivation.streams;
  assert.ok(streams.length >= 3);
  const [first] = streams;
  const state: StreamLiveState = {
    live: true,
    ownerIdentity: first.ownerIdentity,
    windowId: first.windowId,
    paused: first.paused,
    startedAt: first.startedAt,
    streams
  };
  for (const stream of streams) {
    assert.equal(
      isShareOnAir(state, stream.ownerIdentity, stream.windowId),
      !stream.paused,
      `${stream.ownerIdentity}/${stream.windowId}`
    );
  }
  // Same window id, someone else's share: no badge.
  assert.equal(isShareOnAir(state, 'mallory', first.windowId), false);
  assert.equal(isShareOnAir({ ...state, live: false }, first.ownerIdentity, first.windowId), false);
  assert.equal(isShareOnAir(null, first.ownerIdentity, first.windowId), false);
});

test('streamer mode masks meeting names and codes', () => {
  assert.equal(meetingLabelForDisplay('Design review', false), 'Design review');
  assert.equal(meetingLabelForDisplay('Design review', true), LIVE_MEETING_LABEL);
  assert.equal(LIVE_MEETING_LABEL, 'Live meeting');
  assert.equal(recentRoomLabelForDisplay('Standup', false), 'Standup');
  assert.equal(recentRoomLabelForDisplay('Standup', true), HIDDEN_MEETING_LABEL);
  assert.equal(HIDDEN_MEETING_LABEL, 'Hidden meeting');
});

test('the opt-out toggle is offered to everyone but the streamer, only while live', () => {
  assert.equal(showsOptOutToggle(true, false), true);
  assert.equal(showsOptOutToggle(true, true), false, "the streamer's setting never applies to their own stream");
  assert.equal(showsOptOutToggle(false, false), false);
});

test('the LIVE banner writes the same setting as Settings, and only for non-streamers', () => {
  const banner = readFileSync(new URL('../src/lib/components/LiveStreamBanner.svelte', import.meta.url), 'utf8');
  assert.match(banner, /COMMANDS\.streamSettingsSet, \{\s*settings: \{ \.\.\.settings, keepOffLiveStreams: keepOff \}/);
  assert.match(banner, /showsOptOutToggle\(roomLive, own !== null\)/);
  assert.match(banner, /\{:else if optOutVisible && settings\}[\s\S]*Keep me off live streams/);
});

test('service lookup', () => {
  assert.equal(streamServiceById('twitch').name, 'Twitch');
  assert.equal(streamServiceById('nope').id, CUSTOM_STREAM_SERVICE_ID);
  assert.equal(streamServiceForUrl(' rtmps://live-api-s.facebook.com:443/rtmp ')?.id, 'facebook');
  assert.equal(streamServiceForUrl('rtmp://example.com/app'), undefined);
  // Any of YouTube's ingest addresses, not only the preset's.
  for (const url of [
    'rtmp://a.rtmp.youtube.com/live2',
    'rtmps://b.rtmps.youtube.com:443/live2?backup=1',
    'RTMP://A.RTMP.YOUTUBE.COM/live2/'
  ]) {
    assert.equal(streamServiceForUrl(url)?.id, 'youtube', url);
  }
  assert.equal(streamServiceForUrl('rtmp://notyoutube.com/live2'), undefined);
});
