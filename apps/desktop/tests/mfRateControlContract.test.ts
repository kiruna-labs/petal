import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Source contract for the vendored Media Foundation H.264 encoder's
// rate-control policy.
//
// This is a source-shape test on purpose: the policy lives in C++ inside a
// `[patch.crates-io]` dependency (`apps/desktop/vendor/webrtc-sys`), which no
// Rust unit test in this repo can call directly. What it pins is the exact
// regression that shipped: one global `QUALITY` default applied to screen
// content, which ignores WebRTC's bitrate target and starved a screen share to
// a fraction of its configured cadence while capture was perfectly healthy.
// Measured: QUALITY rendered a median ~5.6 fps where the driver default
// rendered ~29.5 fps against the same 30 fps ceiling.
//
// Both sources now default to CBR, and the QUALITY policy is gone: the driver
// rejected the controls it set (`quality_hr` = 0x80004001, E_NOTIMPL), so it
// could never have taken effect. Screen shares moved off the driver default
// once the real startup defect was identified as the ALLOCATION rather than the
// rate-control mode (`TrackPublishOptions::min_bitrate`); CBR is kept because
// "driver default" is whatever each GPU vendor decides, which a cross-vendor
// validation cannot reason about.

const encoderCpp = readFileSync(
  new URL('../vendor/webrtc-sys/src/mf/h264_encoder_impl.cpp', import.meta.url),
  'utf8'
);
const encoderHeader = readFileSync(
  new URL('../vendor/webrtc-sys/src/mf/h264_encoder_impl.h', import.meta.url),
  'utf8'
);

const resolvePolicy = (() => {
  const start = encoderCpp.indexOf(
    'MfH264EncoderImpl::ResolveRateControlPolicy(const char** source) const {'
  );
  assert.notEqual(start, -1, 'ResolveRateControlPolicy must exist');
  const end = encoderCpp.indexOf(
    'MfH264EncoderImpl::ApplyRateControlPolicy(',
    start
  );
  assert.notEqual(end, -1, 'ApplyRateControlPolicy must follow the resolver');
  return encoderCpp.slice(start, end);
})();

test('the codec mode is captured from InitEncode, not guessed', () => {
  assert.match(encoderCpp, /codec_mode_ = codec_settings->mode;/);
  assert.match(encoderHeader, /VideoCodecMode codec_mode_/);
});

test('screensharing defaults to CBR, never to QUALITY', () => {
  const screenBranchAt = resolvePolicy.indexOf(
    'codec_mode_ == VideoCodecMode::kScreensharing'
  );
  assert.notEqual(screenBranchAt, -1, 'the resolver must branch on the codec mode');
  // The screen arm runs until the camera selector is read; slice to that
  // boundary rather than a fixed width so prose in the resolver cannot
  // silently push an assertion out of range.
  const cameraArmAt = resolvePolicy.indexOf('PETAL_MF_CAMERA_RATE_CONTROL', screenBranchAt);
  assert.notEqual(cameraArmAt, -1, 'the camera arm must close the screen branch');
  // Slice to the screen branch's own closing brace, so the assertion below is
  // about the arm's statements and not about the prose or the camera arm that
  // follow it.
  const branchEnd = resolvePolicy.indexOf('\n  }\n', screenBranchAt);
  assert.notEqual(branchEnd, -1, 'the screen branch must be a block');
  const branchBody = resolvePolicy.slice(screenBranchAt, branchEnd);
  assert.match(
    branchBody,
    /return RateControlPolicy::Cbr;/,
    'a screen share must pin an explicit mode so the result is vendor-independent'
  );
  assert.doesNotMatch(
    branchBody,
    /RateControlPolicy::Quality/,
    'QUALITY must not be reachable as a screen-share default'
  );
  assert.doesNotMatch(
    branchBody,
    /getenv\(/,
    'the screen arm must resolve without reading any environment variable'
  );
});

test('the QUALITY policy is removed outright', () => {
  assert.doesNotMatch(
    encoderCpp,
    /RateControlPolicy::Quality/,
    'QUALITY is unreachable and must not linger: the driver rejects its controls'
  );
  assert.doesNotMatch(
    encoderHeader,
    /Quality,/,
    'the enum variant must be gone, not merely unused'
  );
  for (const retired of [
    'PETAL_MF_QUALITY_MODE',
    'PETAL_MF_SCREEN_QUALITY',
    'PETAL_MF_SCREEN_QUALITY_VS_SPEED',
    'PETAL_MF_SCREEN_RATE_CONTROL',
  ]) {
    // Assert the knob is no longer READ, not that the identifier never appears:
    // the removal is worth documenting in a comment, and that must not look
    // like a live configuration path.
    assert.doesNotMatch(
      encoderCpp,
      new RegExp('getenv\\("' + retired + '"'),
      `${retired} must not be read`
    );
  }
});

test('the camera selector cannot run for a screen encoder', () => {
  const cameraEnvAt = resolvePolicy.indexOf('PETAL_MF_CAMERA_RATE_CONTROL');
  const screenBranchAt = resolvePolicy.indexOf(
    'codec_mode_ == VideoCodecMode::kScreensharing'
  );
  assert.notEqual(cameraEnvAt, -1, 'the camera arm must exist');
  assert.notEqual(screenBranchAt, -1);
  assert.ok(
    screenBranchAt < cameraEnvAt,
    'the screensharing branch must return before the camera selector is read; ' +
      'reading the camera env first is the coupling bug that applied camera CBR to screen shares'
  );
});

test('the realtime camera default stays CBR', () => {
  const cameraArm = resolvePolicy.slice(
    resolvePolicy.indexOf('PETAL_MF_CAMERA_RATE_CONTROL')
  );
  const fallthrough = cameraArm.slice(cameraArm.lastIndexOf('if (source != nullptr)'));
  assert.match(
    fallthrough,
    /return RateControlPolicy::Cbr;/,
    'an unset or unrecognized camera selector keeps the measured CBR default'
  );
  assert.match(cameraArm, /std::strcmp\(camera_arm, "default"\) == 0/);
  assert.match(cameraArm, /std::strcmp\(camera_arm, "peak-vbr"\) == 0/);
});

test('one resolution is applied in one place, before the media types are set', () => {
  const applyCallAt = encoderCpp.indexOf('ApplyRateControlPolicy(policy, policy_source)');
  assert.notEqual(applyCallAt, -1, 'InitMft must resolve and apply the policy');
  const configureAt = encoderCpp.indexOf(
    'ConfigureMft(width, height, max_framerate_, target_bps_)'
  );
  assert.notEqual(configureAt, -1);
  assert.ok(
    applyCallAt < configureAt,
    'rate control is a static MFT property; it must be set before ConfigureMft negotiates the media types'
  );
  assert.equal(
    (encoderCpp.match(/ApplyRateControlPolicy\(/g) ?? []).length,
    2,
    'the policy is applied at exactly one call site (plus its definition)'
  );
  // The resolver result must be bound to a named local BEFORE the out-parameter
  // is read. Passing both as arguments of one call leaves the evaluation order
  // unspecified: the log then prints the pre-call value and the resolved one can
  // be dead-store-eliminated (observed on MSVC -- the provenance string literals
  // vanished from the binary).
  assert.match(
    encoderCpp,
    /const RateControlPolicy policy = ResolveRateControlPolicy\(&policy_source\);\s*\n\s*ApplyRateControlPolicy\(policy, policy_source\);/
  );
  assert.doesNotMatch(
    encoderCpp,
    /ApplyRateControlPolicy\(ResolveRateControlPolicy\(/,
    'the resolver must not be nested inside the apply call'
  );
});

test('the retired quality-for-all default is gone', () => {
  assert.doesNotMatch(
    encoderCpp,
    /qm == nullptr \|\| std::strcmp\(qm, "0"\) != 0/,
    'the old "QUALITY unless PETAL_MF_QUALITY_MODE=0" default must not return'
  );
  assert.doesNotMatch(
    encoderCpp,
    /DEFAULT to QUALITY rate-control mode/,
    'the documented quality-first default is what starved screen shares'
  );
});

test('ApplyRateControlPolicy sets at most one policy per call', () => {
  const start = encoderCpp.indexOf(
    'MfH264EncoderImpl::ApplyRateControlPolicy(RateControlPolicy policy,'
  );
  assert.notEqual(start, -1);
  const body = encoderCpp.slice(
    start,
    encoderCpp.indexOf('VideoEncoder::EncoderInfo', start)
  );
  assert.match(body, /case RateControlPolicy::DriverDefault:/);
  assert.match(body, /case RateControlPolicy::Cbr:/);
  assert.match(body, /case RateControlPolicy::PeakVbr: \{/);
  // DriverDefault's case body must be empty: it means "set nothing".
  assert.match(
    body,
    /case RateControlPolicy::DriverDefault:\s*\n\s*break;/,
    'the driver-default arm must set no ICodecAPI property at all'
  );
  // The bring-up line must survive the app's log filter, which drops INFO.
  assert.match(body, /RTC_LOG\(LS_WARNING\)[\s\S]*rate control selected=/);
  assert.doesNotMatch(
    body,
    /RTC_LOG\(LS_INFO\)[\s\S]*rate control selected=/,
    'an INFO bring-up line is invisible in petal.log (the app sink drops libwebrtc INFO)'
  );
});

test('SetRates reports a rejected target instead of discarding it', () => {
  const start = encoderCpp.indexOf('void MfH264EncoderImpl::SetRates(');
  assert.notEqual(start, -1);
  const body = encoderCpp.slice(
    start,
    encoderCpp.indexOf('MfH264EncoderImpl::ResolveRateControlPolicy(', start)
  );
  assert.match(
    body,
    /const HRESULT mean_hr =\s*\n?\s*codec_api_->SetValue\(&CODECAPI_AVEncCommonMeanBitRate/,
    'the mean-bitrate HRESULT must be captured, not dropped'
  );
  assert.match(body, /set_rates_calls_/);
  assert.match(
    body,
    /set_rates_calls_ % 60 == 0/,
    'the diagnostic must be bounded, not per-call'
  );
});

test('a screen share gets the low-latency MFT settings unconditionally', () => {
  // The shallow MFT is the production shape for an interactive share: a stale
  // frame is less useful than the newest one. There is no opt-out.
  assert.match(
    encoderCpp,
    /low_latency_ = codec_mode_ == VideoCodecMode::kScreensharing;/
  );
  assert.doesNotMatch(
    encoderCpp,
    /PETAL_MF_LOW_LATENCY/,
    'the low-latency A/B knob is retired; the screen policy is fixed'
  );
  const lowLatencyAt = encoderCpp.indexOf('MfH264EncoderImpl::ApplyLowLatencySettings() {');
  assert.notEqual(lowLatencyAt, -1);
  const body = encoderCpp.slice(lowLatencyAt, encoderCpp.indexOf('\n}\n', lowLatencyAt));
  assert.match(body, /MF_LOW_LATENCY, TRUE/);
  assert.match(body, /CODECAPI_AVEncCommonRealTime, true/);
  assert.match(body, /CODECAPI_AVEncMPVDefaultBPictureCount, 0/);
  assert.match(body, /CODECAPI_AVEncVideoMaxNumRefFrame, 1/);
  // The shallow input window is what keeps the latency promise real.
  assert.match(encoderCpp, /kMaxPendingLowLatencyInputs = 2/);
});

test('the keyframe interval is sized from a duty cycle, not left to the driver', () => {
  // A single intra frame costs many frame-times of drain, so the interval is
  // what sets the fraction of the time a receiver spends waiting for one.
  assert.match(encoderCpp, /kIntraBytesPerPixelDefault = 0\.45/);
  assert.match(encoderCpp, /kAutoGopDutyTarget = 0\.05/);
  assert.match(encoderCpp, /kAutoGopMinSeconds = 2\.0/);
  assert.match(encoderCpp, /kAutoGopMaxSeconds = 10\.0/);
  assert.match(encoderCpp, /CODECAPI_AVEncMPVGOPSize/);
  // The measured intra size replaces the estimate, and the refinement must be
  // an EMA seeded by the first refresh rather than a running average that the
  // opening intra frame would dominate.
  assert.match(encoderCpp, /kIntraEmaAlpha = 0\.5/);
  assert.match(encoderCpp, /intra_bytes_ema_/);
  assert.match(encoderCpp, /ResolveIntraBytes\(intra_bytes_ema_/);
});

test('no measurement-only knob is left in the encoder', () => {
  for (const retired of [
    'PETAL_MF_GOP_FRAMES',
    'PETAL_MF_INTRA_BYTES_PER_PIXEL',
    'PETAL_MF_PROFILE',
    'PETAL_MF_LOW_LATENCY',
    'PETAL_MF_SCREEN_RATE_CONTROL',
  ]) {
    assert.doesNotMatch(
      encoderCpp,
      new RegExp('getenv\\("' + retired + '"'),
      `${retired} must not be read`
    );
  }
  // The output-cadence / size-distribution probe is gone with them.
  assert.doesNotMatch(encoderCpp, /kOutputGapLogEvery|output_frame_bytes_/);
});

test('the encoder reports the SPS it actually emits, read from the bitstream', () => {
  // The MFT's own media-type attributes are not evidence of what it encodes, so
  // the one-shot SPS report is the only thing that can catch a mismatch.
  assert.match(encoderCpp, /MaybeLogEmittedSps\(\)/);
  assert.match(encoderCpp, /FindSpsProfileLevel\(/);
  assert.match(encoderCpp, /emitted_sps_logged_/);
  // Baseline stays what the SDP advertises; the level is left to the MFT.
  assert.match(encoderCpp, /SetUINT32\(MF_MT_MPEG2_PROFILE, 66\)/);
  assert.doesNotMatch(
    encoderCpp,
    /MF_MT_MPEG2_LEVEL/,
    'the MFT derives the level from geometry; pinning one could only make it wrong'
  );
});
