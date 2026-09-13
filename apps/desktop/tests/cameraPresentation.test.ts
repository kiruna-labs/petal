import assert from 'node:assert/strict';
import test from 'node:test';

import {
  startCameraPresentationProbe,
  takeCameraPresentationSnapshot,
  clearAllCameraPresentations,
  type CameraPresentationVideo
} from '../src/lib/data/cameraPresentation.ts';

// Generation-owned WebView presentation probe. The fake element exposes a
// controllable `requestVideoFrameCallback` so a test can present frames at
// chosen timestamps and observe the gap/FPS counters without a DOM.

interface FakeVideo extends CameraPresentationVideo {
  present(now: number): void;
  emit(type: string): void;
  readonly outstandingCallbacks: number;
  readonly maxOutstandingCallbacks: number;
  setPaused(paused: boolean): void;
}

function createFakeVideo(options: { rvfc?: boolean } = {}): FakeVideo {
  const listeners = new Map<string, Set<() => void>>();
  let pending: ((now: number) => void) | null = null;
  let outstanding = 0;
  let maxOutstanding = 0;
  const video: FakeVideo = {
    paused: false,
    readyState: 4,
    addEventListener(type, listener) {
      const set = listeners.get(type) ?? new Set();
      set.add(listener);
      listeners.set(type, set);
    },
    removeEventListener(type, listener) {
      listeners.get(type)?.delete(listener);
    },
    present(now) {
      const callback = pending;
      if (!callback) return;
      pending = null;
      outstanding = Math.max(0, outstanding - 1);
      callback(now);
    },
    emit(type) {
      for (const listener of listeners.get(type) ?? []) listener();
    },
    setPaused(paused) {
      video.paused = paused;
    },
    get outstandingCallbacks() {
      return outstanding;
    },
    get maxOutstandingCallbacks() {
      return maxOutstanding;
    }
  };
  if (options.rvfc !== false) {
    video.requestVideoFrameCallback = (callback) => {
      pending = callback;
      outstanding += 1;
      maxOutstanding = Math.max(maxOutstanding, outstanding);
      return outstanding;
    };
    video.cancelVideoFrameCallback = () => {
      pending = null;
      outstanding = Math.max(0, outstanding - 1);
    };
  }
  return video;
}

function withDocumentHidden(hidden: boolean): void {
  const listeners = new Set<() => void>();
  documentListeners = listeners;
  (globalThis as { document?: unknown }).document = {
    hidden,
    addEventListener(type: string, listener: () => void) {
      if (type === 'visibilitychange') listeners.add(listener);
    },
    removeEventListener(type: string, listener: () => void) {
      if (type === 'visibilitychange') listeners.delete(listener);
    }
  };
}

let documentListeners = new Set<() => void>();

function setDocumentHidden(hidden: boolean): void {
  const doc = (globalThis as { document?: { hidden: boolean } }).document;
  if (doc) doc.hidden = hidden;
}

function emitDocument(type: string): void {
  if (type !== 'visibilitychange') return;
  for (const listener of documentListeners) listener();
}

function freshProbe(identity: string, video: FakeVideo, now: () => number) {
  clearAllCameraPresentations();
  return startCameraPresentationProbe({ identity, video, now });
}

test('probe registers synchronously and reports an immediate honest snapshot', () => {
  withDocumentHidden(false);
  const video = createFakeVideo();
  const clock = { value: 1_000 };
  freshProbe('alice', video, () => clock.value);

  const snapshot = takeCameraPresentationSnapshot('alice');
  assert.ok(snapshot);
  assert.equal(snapshot.presentedFrames, 0);
  assert.equal(snapshot.presentedFps, 0);
  assert.equal(snapshot.rvfcAvailable, true);
  assert.equal(snapshot.readyState, 4);
  assert.equal(snapshot.paused, false);
  assert.equal(snapshot.observing, true);
  // No frame has been presented yet: silence is unknown, not a zero gap.
  assert.equal(snapshot.currentGapMs, null);
  assert.equal(snapshot.gapCount100Ms, 0);
  clearAllCameraPresentations();
});

test('an element without requestVideoFrameCallback is unsupported, never a healthy zero', () => {
  withDocumentHidden(false);
  const video = createFakeVideo({ rvfc: false });
  freshProbe('bob', video, () => 0);

  const snapshot = takeCameraPresentationSnapshot('bob');
  assert.ok(snapshot);
  assert.equal(snapshot.rvfcAvailable, false);
  assert.equal(snapshot.presentedFrames, 0);
  // The readiness fallback still fires so the tile can fade in.
  video.emit('loadeddata');
  assert.equal(takeCameraPresentationSnapshot('bob')?.rvfcAvailable, false);
  clearAllCameraPresentations();
});

test('gap thresholds count at 100ms and 250ms boundaries, never below', () => {
  withDocumentHidden(false);
  const video = createFakeVideo();
  let now = 0;
  freshProbe('carol', video, () => now);

  // Integer timestamps keep the boundary arithmetic exact.
  video.present((now = 0)); // baseline
  video.present((now = 99)); // below the 100ms bucket
  let snapshot = takeCameraPresentationSnapshot('carol');
  assert.equal(snapshot?.gapCount100Ms, 0);
  assert.equal(snapshot?.gapCount250Ms, 0);
  assert.equal(snapshot?.maxGapMs, 99);

  video.present((now = 199)); // exactly 100ms
  snapshot = takeCameraPresentationSnapshot('carol');
  assert.equal(snapshot?.gapCount100Ms, 1);
  assert.equal(snapshot?.gapCount250Ms, 0);
  assert.equal(snapshot?.excessGapMs, 0);

  video.present((now = 448)); // 249ms, below the 250ms bucket
  snapshot = takeCameraPresentationSnapshot('carol');
  assert.equal(snapshot?.gapCount100Ms, 2);
  assert.equal(snapshot?.gapCount250Ms, 0);
  assert.equal(snapshot?.excessGapMs, 149);

  video.present((now = 698)); // exactly 250ms
  snapshot = takeCameraPresentationSnapshot('carol');
  assert.equal(snapshot?.gapCount100Ms, 3);
  assert.equal(snapshot?.gapCount250Ms, 1);
  assert.equal(snapshot?.maxGapMs, 250);
  assert.equal(snapshot?.excessGapMs, 299);
  clearAllCameraPresentations();
});

test('current silence is folded into max gap and excess beyond 100ms', () => {
  withDocumentHidden(false);
  const video = createFakeVideo();
  let now = 0;
  freshProbe('dave', video, () => now);

  video.present((now = 0));
  video.present((now = 40));
  now = 40 + 300; // 300ms of silence with no new frame
  const snapshot = takeCameraPresentationSnapshot('dave');
  assert.equal(snapshot?.currentGapMs, 300);
  assert.equal(snapshot?.maxGapMs, 300);
  assert.equal(snapshot?.excessGapMs, 200);
  // The open gap is not counted as a completed gap until a frame arrives.
  assert.equal(snapshot?.gapCount100Ms, 0);
  clearAllCameraPresentations();
});

test('presented fps is computed at snapshot time from frame/time deltas', () => {
  withDocumentHidden(false);
  const video = createFakeVideo();
  let now = 0;
  freshProbe('erin', video, () => now);
  for (let frame = 0; frame < 10; frame += 1) {
    video.present((now = frame * 100));
  }
  const snapshot = takeCameraPresentationSnapshot('erin');
  assert.equal(snapshot?.presentedFrames, 10);
  // 10 frames over the 900ms window; the first frame only anchors the window.
  assert.ok(
    (snapshot?.presentedFps ?? 0) > 9 && (snapshot?.presentedFps ?? 0) < 12,
    `fps=${snapshot?.presentedFps}`
  );
  clearAllCameraPresentations();
});

test('the probe keeps exactly one callback outstanding', () => {
  withDocumentHidden(false);
  const video = createFakeVideo();
  let now = 0;
  freshProbe('frank', video, () => now);
  assert.equal(video.outstandingCallbacks, 1);
  for (let frame = 0; frame < 5; frame += 1) video.present((now = frame * 16));
  assert.equal(video.outstandingCallbacks, 1);
  assert.equal(video.maxOutstandingCallbacks, 1);
  clearAllCameraPresentations();
});

test('hidden and paused stretches reset the gap baseline', () => {
  withDocumentHidden(false);
  const video = createFakeVideo();
  let now = 0;
  freshProbe('grace', video, () => now);
  video.present((now = 0));

  // Backgrounding, then returning: the 5s away must not be a media freeze.
  setDocumentHidden(true);
  emitDocument('visibilitychange');
  setDocumentHidden(false);
  assert.equal(takeCameraPresentationSnapshot('grace')?.observing, true);
  video.present((now = 5_000));
  let snapshot = takeCameraPresentationSnapshot('grace');
  assert.equal(snapshot?.gapCount100Ms, 0);
  assert.equal(snapshot?.maxGapMs, 0);

  // Pausing is the same: baseline drops, resume does not count the pause.
  video.setPaused(true);
  video.emit('pause');
  assert.equal(takeCameraPresentationSnapshot('grace')?.observing, false);
  video.setPaused(false);
  video.emit('play');
  video.present((now = 9_000));
  snapshot = takeCameraPresentationSnapshot('grace');
  assert.equal(snapshot?.gapCount100Ms, 0);
  assert.equal(snapshot?.maxGapMs, 0);
  clearAllCameraPresentations();
});

test('cumulative counters survive a same-identity replacement without fabricating a gap', () => {
  withDocumentHidden(false);
  const first = createFakeVideo();
  const second = createFakeVideo();
  let now = 0;
  clearAllCameraPresentations();
  startCameraPresentationProbe({ identity: 'judy', video: first, now: () => now });
  first.present((now = 0));
  first.present((now = 100));
  const before = takeCameraPresentationSnapshot('judy');
  assert.equal(before?.probeStarts, 1);
  assert.equal(before?.presentedFrames, 2);
  assert.equal(before?.gapCount100Ms, 1);

  // A replacement element inherits the cumulative counters but resets the
  // frame baseline, so the handoff itself is never counted as a gap.
  startCameraPresentationProbe({ identity: 'judy', video: second, now: () => now });
  second.present((now = 60_000));
  const after = takeCameraPresentationSnapshot('judy');
  assert.equal(after?.probeStarts, 2);
  assert.equal(after?.presentedFrames, 3);
  assert.equal(after?.gapCount100Ms, 1);
  assert.equal(after?.maxGapMs, 100);
  assert.notEqual(after?.generation, before?.generation);

  // clearAll drops the inherited state too.
  clearAllCameraPresentations();
  const third = createFakeVideo();
  startCameraPresentationProbe({ identity: 'judy', video: third, now: () => now });
  assert.equal(takeCameraPresentationSnapshot('judy')?.presentedFrames, 0);
  clearAllCameraPresentations();
});

test('counters survive a stop-then-start, which is Svelte effect cleanup order', () => {
  withDocumentHidden(false);
  const first = createFakeVideo();
  const second = createFakeVideo();
  let now = 0;
  clearAllCameraPresentations();
  const probeA = startCameraPresentationProbe({
    identity: 'karl',
    video: first,
    now: () => now
  });
  first.present((now = 0));
  first.present((now = 100));
  // Svelte runs the effect's cleanup (stop) BEFORE the replacement effect
  // body, so the active entry is already gone when the next probe starts.
  probeA.stop();
  const probeB = startCameraPresentationProbe({
    identity: 'karl',
    video: second,
    now: () => now
  });
  second.present((now = 50_000));
  const snapshot = takeCameraPresentationSnapshot('karl');
  assert.equal(snapshot?.probeStarts, 2);
  assert.equal(snapshot?.presentedFrames, 3);
  assert.equal(snapshot?.gapCount100Ms, 1, 'the handoff must not add a gap');
  assert.equal(snapshot?.maxGapMs, 100);
  probeB.stop();
  clearAllCameraPresentations();
});

test('stopping a probe retires it without touching its replacement generation', () => {
  withDocumentHidden(false);
  const first = createFakeVideo();
  const second = createFakeVideo();
  let now = 0;
  const nowFn = () => now;
  clearAllCameraPresentations();
  const probeA = startCameraPresentationProbe({ identity: 'heidi', video: first, now: nowFn });
  // Replacing the identity retires A and installs B.
  const probeB = startCameraPresentationProbe({ identity: 'heidi', video: second, now: nowFn });
  probeA.stop();

  // A stale callback on the retired element must not mutate B.
  first.present((now = 100));
  assert.equal(takeCameraPresentationSnapshot('heidi')?.presentedFrames, 0);
  second.present((now = 200));
  assert.equal(takeCameraPresentationSnapshot('heidi')?.presentedFrames, 1);

  // Stopping A again is a no-op; stopping B drops the entry.
  probeA.stop();
  assert.ok(takeCameraPresentationSnapshot('heidi'));
  probeB.stop();
  assert.equal(takeCameraPresentationSnapshot('heidi'), null);
  clearAllCameraPresentations();
});

test('clearAllCameraPresentations retires every probe', () => {
  withDocumentHidden(false);
  const video = createFakeVideo();
  startCameraPresentationProbe({ identity: 'ivan', video, now: () => 0 });
  assert.ok(takeCameraPresentationSnapshot('ivan'));
  clearAllCameraPresentations();
  assert.equal(takeCameraPresentationSnapshot('ivan'), null);
  // Presenting after clear-all cannot throw or resurrect the entry.
  video.present(16);
  assert.equal(takeCameraPresentationSnapshot('ivan'), null);
});
