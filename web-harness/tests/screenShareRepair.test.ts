import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { LocalVideoTrack } from 'livekit-client';

import { setupControls } from '../src/controls.ts';
import type { HarnessContext } from '../src/context.ts';

// `LocalVideoTrack`'s constructor calls `setMediaStreamTrack`, which does
// `new MediaStream([track])` and reads `track.getConstraints()` -- neither
// exists in this bare `node --test` environment (no browser globals). This
// is the SAME real `LocalVideoTrack` `publishScreenShareTrack` constructs in
// production; polyfilling just enough of the constructor's own dependencies
// (not a fake of our code under test) is what lets this test exercise the
// real class instead of a stand-in.
class FakeMediaStream {
  private tracks: FakeMediaStreamTrack[];
  constructor(tracks: FakeMediaStreamTrack[] = []) {
    this.tracks = tracks;
  }
  getTracks() {
    return this.tracks;
  }
  getVideoTracks() {
    return this.tracks.filter((t) => t.kind === 'video');
  }
  getAudioTracks(): FakeMediaStreamTrack[] {
    return [];
  }
  addTrack(track: FakeMediaStreamTrack) {
    this.tracks.push(track);
  }
  removeTrack(track: FakeMediaStreamTrack) {
    this.tracks = this.tracks.filter((t) => t !== track);
  }
}
(globalThis as unknown as { MediaStream: unknown }).MediaStream = FakeMediaStream;

class FakeMediaStreamTrack {
  readonly kind = 'video';
  readonly id: string;
  label = 'fake-screen-capture';
  enabled = true;
  readyState: 'live' | 'ended' = 'live';
  contentHint = '';
  private listeners = new Map<string, Array<() => void>>();

  constructor(id: string) {
    this.id = id;
  }

  addEventListener(type: string, handler: () => void) {
    const list = this.listeners.get(type) ?? [];
    list.push(handler);
    this.listeners.set(type, list);
  }
  removeEventListener() {}
  getSettings() {
    return { width: 2560, height: 1600 };
  }
  getConstraints() {
    return {};
  }
  stop() {
    this.readyState = 'ended';
  }
  clone() {
    return this;
  }
}

interface PublishCall {
  track: LocalVideoTrack;
  options: Record<string, unknown>;
}
interface UnpublishCall {
  track: LocalVideoTrack;
  stopOnUnpublish: boolean | undefined;
}

function makeScreenShareHarness() {
  const publishCalls: PublishCall[] = [];
  const unpublishCalls: UnpublishCall[] = [];
  const logs: Array<{ line: string; kind?: string }> = [];
  const mediaTrack = new FakeMediaStreamTrack('capture-1');

  const state: Record<string, unknown> = {
    room: {
      localParticipant: {
        identity: 'web-riley',
        publishTrack: async (track: LocalVideoTrack, options: Record<string, unknown>) => {
          publishCalls.push({ track, options });
          return {};
        },
        unpublishTrack: async (track: LocalVideoTrack, stopOnUnpublish?: boolean) => {
          unpublishCalls.push({ track, stopOnUnpublish });
        },
      },
    },
    screenSharing: true,
    screenWindowId: 42,
    // A plain stand-in with the one field `repairScreenShareForWindow`
    // actually reads (`mediaStreamTrack`) -- not a real `LocalVideoTrack`,
    // since the ORIGINAL publish (the getDisplayMedia click path, covered
    // by verifyH264Negotiated elsewhere) is not what this test exercises.
    screenTrack: { mediaStreamTrack: mediaTrack } as unknown as LocalVideoTrack,
  };

  const ctx = {
    dom: {},
    state,
    ui: {
      logEvent: (line: string, kind?: string) => logs.push({ line, kind }),
      clearError: () => {},
      showError: () => {},
      showToast: () => {},
      setScreenShareState: () => {},
      setShareState: () => {},
      setMicState: () => {},
      setRealMicState: () => {},
      setWebcamState: () => {},
      setAudioControl: () => {},
      setVideoControl: () => {},
      setShareControl: () => {},
    },
    cb: {},
  } as unknown as HarnessContext;

  const controls = setupControls(ctx);
  return { controls, state, publishCalls, unpublishCalls, logs, mediaTrack };
}

test('a repair request for the currently-published window republishes exactly once', async () => {
  const { controls, state, publishCalls, unpublishCalls, mediaTrack } = makeScreenShareHarness();
  const originalTrack = (state as { screenTrack: LocalVideoTrack }).screenTrack;

  const repaired = await controls.repairScreenShareForWindow(42, 'native-quinn');

  assert.equal(repaired, true);
  assert.equal(unpublishCalls.length, 1);
  assert.equal(unpublishCalls[0]!.track, originalTrack, 'must unpublish the OLD LiveKit publication');
  assert.equal(
    unpublishCalls[0]!.stopOnUnpublish,
    false,
    'must not stop the underlying getDisplayMedia capture -- that would need a new picker prompt'
  );
  assert.equal(publishCalls.length, 1);
  const { options } = publishCalls[0]!;
  assert.equal(options.name, 'petal-window-42');
  assert.equal(options.videoCodec, 'h264');
  assert.equal(options.degradationPreference, 'maintain-resolution');
  assert.deepEqual(options.frameMetadata, { timestamp: true, frameId: true });
  assert.ok(options.screenShareEncoding, 'must carry an explicit encoding, not the livekit default');
  // Same underlying capture, not a re-prompted new capture.
  assert.equal(
    (publishCalls[0]!.track as unknown as { mediaStreamTrack: FakeMediaStreamTrack }).mediaStreamTrack,
    mediaTrack
  );
  assert.notEqual(
    (state as { screenTrack: LocalVideoTrack }).screenTrack,
    originalTrack,
    'state.screenTrack must point at the NEW publication'
  );
});

test('a second repair request for the same window inside the rate-limit window does nothing', async () => {
  const { controls, publishCalls } = makeScreenShareHarness();

  const first = await controls.repairScreenShareForWindow(42, 'native-quinn');
  const second = await controls.repairScreenShareForWindow(42, 'native-quinn');

  assert.equal(first, true);
  assert.equal(second, false);
  assert.equal(publishCalls.length, 1, 'the rate-limited request must not trigger a second republish');
});

test('a repair request for a window this client is not publishing does nothing', async () => {
  const { controls, publishCalls, unpublishCalls } = makeScreenShareHarness();

  const repaired = await controls.repairScreenShareForWindow(99, 'native-quinn');

  assert.equal(repaired, false);
  assert.equal(publishCalls.length, 0);
  assert.equal(unpublishCalls.length, 0);
});

test('a repair request while not screen-sharing at all does nothing', async () => {
  const { controls, state, publishCalls } = makeScreenShareHarness();
  (state as { screenSharing: boolean }).screenSharing = false;

  const repaired = await controls.repairScreenShareForWindow(42, 'native-quinn');

  assert.equal(repaired, false);
  assert.equal(publishCalls.length, 0);
});
