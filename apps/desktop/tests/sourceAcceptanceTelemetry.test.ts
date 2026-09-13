import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Source contract for the S0 source-acceptance telemetry seam.
//
// WebRTC's adapted source can legally REJECT a frame it will not encode, and
// the C++ side already reports that through `on_captured_frame`. Both Rust
// wrappers used to drop the boolean, so "the encoder never received this
// frame" and "the frame converted fine but the source declined it" were
// indistinguishable — which is what made a low-cadence camera read as an
// encoder problem during the #173 investigation.
//
// These are source-shape assertions on purpose: the wrappers live in a
// `[patch.crates-io]` vendor tree and the publisher paths are behind a live
// capture device, so neither is reachable from a unit test here.

const reader = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');

const nativeWrapper = reader('../vendor/libwebrtc/src/native/video_source.rs');
const safeWrapper = reader('../vendor/libwebrtc/src/video_source.rs');
const publisher = reader('../src-tauri/src/transport/publisher.rs');
const mfReader = reader('../src-tauri/src/transport/camera/mf.rs');
const subscriber = reader('../src-tauri/src/transport/subscriber.rs');

test('both wrappers return the C++ acceptance boolean instead of discarding it', () => {
  // `-> bool` on the signature is the whole seam; a `()` here silently makes
  // every downstream `source_accepted` a constant.
  assert.match(
    nativeWrapper,
    /pub fn capture_frame<T: AsRef<dyn VideoBuffer>>\(&self, frame: &VideoFrame<T>\) -> bool \{/
  );
  assert.match(
    safeWrapper,
    /pub fn capture_frame<T: AsRef<dyn VideoBuffer>>\(&self, frame: &VideoFrame<T>\) -> bool \{/
  );
  // The native wrapper must RETURN the call. A trailing semicolon would return
  // `()` from a `-> bool` fn and fail to compile, but it could also be
  // reintroduced as a discarded statement plus `true`/`false`, so pin the shape.
  assert.match(
    nativeWrapper,
    /self\.sys_handle\.on_captured_frame\(\s*&builder\.pin_mut\(\)\.build\(\),\s*&vt_sys::ffi::FrameMetadata \{[\s\S]*?\},\s*\)\s*\n\s*\}/,
    'the native wrapper must return on_captured_frame(...) as its tail expression'
  );
});

test('the publisher timing struct carries acceptance, capture age and frame id', () => {
  assert.match(
    publisher,
    /pub struct PublishedFrameTiming \{[\s\S]*?pub capture_age_ms: f64,[\s\S]*?pub frame_id: u32,[\s\S]*?pub source_accepted: bool,[\s\S]*?\}/
  );
  assert.match(publisher, /fn capture_age_ms\(capture_wall_time_us: u64\) -> f64 \{/);
  // Every CONSTRUCTION site must initialise all three, or a field quietly
  // defaults and the evidence lies. Match only constructions (`Ok(…)` /
  // `Some(…)`); a bare `-> Option<PublishedFrameTiming> {` signature would
  // otherwise match the type name and look like a field-less literal.
  const constructions = publisher.match(/(?:Ok|Some)\(PublishedFrameTiming \{[\s\S]*?\n\s*\}\)/g) ?? [];
  assert.equal(constructions.length, 3, `expected the three push paths, saw ${constructions.length}`);
  for (const site of constructions) {
    // Field shorthand (`frame_id,`) is used in some paths, so accept both forms.
    assert.match(site, /\bcapture_age_ms\s*[:,]/, `missing capture_age_ms in ${site.slice(0, 40)}`);
    assert.match(site, /\bframe_id\s*[:,]/, 'missing frame_id');
    assert.match(site, /\bsource_accepted\s*[:,]/, 'missing source_accepted');
  }
});

test('every video capture_frame call site states what it does with acceptance', () => {
  // A bare `capture_frame(...)` statement is now an unused-result site; S0 is
  // explicit everywhere rather than relying on the compiler being quiet.
  const barePublisher = publisher
    .split('\n')
    .filter((line) => /^\s*self\.rtc_source\.capture_frame\(/.test(line))
    .filter((line) => !/let source_accepted =/.test(line) && !/\)$/.test(line.trim()));
  assert.deepEqual(barePublisher, [], 'publisher push paths must bind acceptance');
  // The two non-S0 sites discard it deliberately and say why.
  assert.match(mfReader, /let _ = source\.capture_frame\(/);
  assert.match(subscriber, /let _ = source\.capture_frame\(/);
});

test('the boundary log is sampled and observational', () => {
  assert.match(publisher, /fn log_source_boundary\(&self, timing: PublishedFrameTiming\) \{/);
  // Rate limiting is the difference between a readable log and a flood at 60 FPS.
  assert.match(publisher, /last_source_boundary_log/);
  assert.match(publisher, /Duration::from_secs\(5\)/);
  // It must only log: no queue/retry/selection policy may hide in here.
  const body = publisher.slice(publisher.indexOf('fn log_source_boundary'));
  const fnBody = body.slice(0, body.indexOf('\n    }\n'));
  for (const forbidden of ['return Err', 'push_frame', 'capture_frame(', '.set_', 'retry']) {
    assert.ok(!fnBody.includes(forbidden), `log_source_boundary must not contain ${forbidden}`);
  }
});

test('S0 did not import the rejected donor experiments', () => {
  // The donor commit fbecc98 mixed S0 with the experimental screencast FPS
  // bypass, a synthetic camera source and its own bitrate experiment. None of
  // THOSE may appear here. (`CAMERA_MIN_BITRATE_BPS` / `CAMERA_MAX_CEILING_BPS`
  // are NOT in this list: the accepted camera CBR formula already exists on
  // main and the donor merely reused its names.)
  for (const [name, source] of [
    ['publisher', publisher],
    ['mf reader', mfReader],
    ['native wrapper', nativeWrapper]
  ] as const) {
    for (const symbol of [
      'PETAL_EXPERIMENTAL_VIDEO_FPS',
      'experimental_share_fps',
      'synthetic_camera_fps',
      'SYNTHETIC_CAMERA_LOW_BITRATE_BPS'
    ]) {
      assert.ok(!source.includes(symbol), `${name} must not carry the rejected ${symbol}`);
    }
  }
  // ...and the accepted camera CBR policy must still be exactly where it was.
  assert.match(publisher, /const CAMERA_MAX_CEILING_BPS: u64 = 16_000_000;/);
  // And the C++ adaptation bypass must stay out of the vendor tree.
  const trackCpp = reader('../vendor/webrtc-sys/src/video_track.cpp');
  assert.ok(
    !trackCpp.includes('PETAL_EXPERIMENTAL_VIDEO_FPS'),
    'the screencast AdaptFrame bypass must not be imported'
  );
});
