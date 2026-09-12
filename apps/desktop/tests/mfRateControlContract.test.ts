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

test('screensharing defaults to the driver mode, never to QUALITY', () => {
  const screenBranch = resolvePolicy.indexOf(
    'codec_mode_ == VideoCodecMode::kScreensharing'
  );
  assert.notEqual(screenBranch, -1, 'the resolver must branch on the codec mode');
  const branchBody = resolvePolicy.slice(screenBranch, screenBranch + 400);
  assert.match(
    branchBody,
    /return RateControlPolicy::DriverDefault;/,
    'a screen share must leave the driver default standing so the bitrate target is honoured'
  );
  assert.doesNotMatch(
    branchBody,
    /RateControlPolicy::Quality/,
    'QUALITY must not be reachable as a screen-share default'
  );
});

test('QUALITY is reachable only through the explicit opt-in', () => {
  const qualityUses = resolvePolicy.match(/RateControlPolicy::Quality/g) ?? [];
  assert.equal(
    qualityUses.length,
    1,
    'exactly one return may select QUALITY, and it must be the env opt-in'
  );
  const qualityAt = resolvePolicy.indexOf('RateControlPolicy::Quality');
  const overrideGuard = resolvePolicy.lastIndexOf(
    'PETAL_MF_QUALITY_MODE',
    qualityAt
  );
  assert.notEqual(overrideGuard, -1);
  assert.match(
    resolvePolicy.slice(overrideGuard, qualityAt),
    /std::strcmp\(quality_override, "1"\) == 0/,
    'QUALITY must require PETAL_MF_QUALITY_MODE=1'
  );
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
  assert.match(body, /case RateControlPolicy::Quality: \{/);
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
