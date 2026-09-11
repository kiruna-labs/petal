import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Source-contract tests for the Windows Media Foundation H.264 encoder's
// rate-control policy. The C++ cannot be invoked from this suite, so these
// assert the invariants that would otherwise regress silently -- above all the
// ORDERING rule, which is a real Windows API requirement: a rate-control MODE
// is a static property, so an encoder is allowed to ignore one applied after
// the media types have been negotiated.
const source = readFileSync(
  new URL('../vendor/webrtc-sys/src/mf/h264_encoder_impl.cpp', import.meta.url),
  'utf8'
);
const header = readFileSync(
  new URL('../vendor/webrtc-sys/src/mf/h264_encoder_impl.h', import.meta.url),
  'utf8'
);

function bodyOf(signature: string): string {
  const start = source.indexOf(signature);
  assert.ok(start >= 0, `could not locate ${signature}`);
  // Brace-match from the first '{' after the signature so the slice is exactly
  // this function and cannot leak assertions into a neighbour.
  const open = source.indexOf('{', start);
  let depth = 0;
  for (let index = open; index < source.length; index += 1) {
    if (source[index] === '{') depth += 1;
    else if (source[index] === '}') {
      depth -= 1;
      if (depth === 0) return source.slice(open, index + 1);
    }
  }
  throw new Error(`unterminated body for ${signature}`);
}

test('the camera rate-control mode is applied before media types are negotiated', () => {
  const initMft = bodyOf('int32_t MfH264EncoderImpl::InitMft(int width, int height)');
  const rateControl = initMft.indexOf('ApplyCameraRateControlMode()');
  const configure = initMft.indexOf('ConfigureMft(width, height');
  assert.ok(rateControl >= 0, 'InitMft must apply the camera rate-control mode');
  assert.ok(configure >= 0, 'InitMft must negotiate the media types');
  assert.ok(
    rateControl < configure,
    'a static rate-control mode must be set BEFORE ConfigureMft, or an encoder may ignore it'
  );
});

test('only the exact documented camera rate-control arms are honored', () => {
  const apply = bodyOf('void MfH264EncoderImpl::ApplyCameraRateControlMode()');
  assert.match(apply, /std::strcmp\(value, "cbr"\) == 0/);
  assert.match(apply, /std::strcmp\(value, "peak-vbr"\) == 0/);
  // Anything else must fall through to the default rather than being passed to
  // the encoder as an unvalidated mode.
  assert.equal(apply.includes('else {'), false, 'strict parsing: no catch-all arm');
  assert.match(apply, /selected = "default"/);
});

test('the camera arm leaves the screenshare quality policy alone', () => {
  const apply = bodyOf('void MfH264EncoderImpl::ApplyCameraRateControlMode()');
  const qualityGate = apply.indexOf('if (QualityModeEnabled(nullptr))');
  const firstCameraMode = apply.indexOf('eAVEncCommonRateControlMode_CBR');
  assert.ok(qualityGate >= 0, 'the camera arm must check the quality policy first');
  assert.ok(firstCameraMode >= 0, 'the camera arm must set a camera mode');
  assert.ok(
    qualityGate < firstCameraMode,
    'quality mode (screenshare) must short-circuit before any camera mode is set'
  );
  // The screenshare policy itself must remain quality-first.
  const initMft = bodyOf('int32_t MfH264EncoderImpl::InitMft(int width, int height)');
  assert.match(initMft, /eAVEncCommonRateControlMode_Quality/);
  assert.match(initMft, /CODECAPI_AVEncCommonQuality, &quality/);
});

test('the rate-control policy is reported at a level the app log sink keeps', () => {
  const apply = bodyOf('void MfH264EncoderImpl::ApplyCameraRateControlMode()');
  // Measured: the app's log sink keeps libwebrtc WARN/ERROR but NOT INFO, so an
  // INFO line here is invisible in petal.log -- which silently makes every
  // rate-control A/B unreadable while appearing to be instrumented.
  assert.equal(
    apply.includes('RTC_LOG(LS_INFO)'),
    false,
    'libwebrtc INFO does not reach petal.log; the selected arm would be unreadable'
  );
  assert.match(apply, /RTC_LOG\(LS_WARNING\)/);
  // Every early return must still name the effective mode, or an absent arm
  // line is ambiguous between "quality" and "no ICodecAPI".
  assert.match(apply, /selected=unavailable/);
  assert.match(apply, /selected=quality/);
});

test('the single quality-mode rule is shared by both call sites', () => {
  // Two independent copies of the same env rule is how a policy silently
  // diverges, so InitMft must delegate to the one helper.
  const initMft = bodyOf('int32_t MfH264EncoderImpl::InitMft(int width, int height)');
  assert.match(initMft, /QualityModeEnabled\(&quality_source\)/);
  assert.equal(
    initMft.includes('getenv("PETAL_MF_QUALITY_MODE")'),
    false,
    'InitMft must not re-derive the quality rule itself (the name may still appear in prose)'
  );
  const helper = bodyOf('bool MfH264EncoderImpl::QualityModeEnabled(const char** source) const');
  assert.match(helper, /PETAL_MF_QUALITY_MODE/);
});

test('cbr selects the mode and pins the mean bitrate', () => {
  const apply = bodyOf('void MfH264EncoderImpl::ApplyCameraRateControlMode()');
  assert.match(apply, /eAVEncCommonRateControlMode_CBR/);
  assert.match(apply, /CODECAPI_AVEncCommonMeanBitRate, &mean/);
});

test('peak-vbr adds a bounded ceiling rather than an unbounded peak', () => {
  const apply = bodyOf('void MfH264EncoderImpl::ApplyCameraRateControlMode()');
  assert.match(apply, /eAVEncCommonRateControlMode_PeakConstrainedVBR/);
  assert.match(apply, /peak\.ulVal = target_bps_ \* 2/);
  assert.match(apply, /CODECAPI_AVEncCommonMaxBitRate, &peak/);
});

test('a rejected mean-bitrate update is observed instead of assumed', () => {
  const setRates = bodyOf('void MfH264EncoderImpl::SetRates(const RateControlParameters& parameters)');
  // The old code discarded this HRESULT entirely, which is exactly what let a
  // silently ignored target look like a healthy encoder.
  assert.match(setRates, /mean_hr = codec_api_->SetValue\(&CODECAPI_AVEncCommonMeanBitRate/);
  assert.match(setRates, /mean_bitrate_hr=0x/);
});

test('set_rates diagnostics stay bounded', () => {
  const setRates = bodyOf('void MfH264EncoderImpl::SetRates(const RateControlParameters& parameters)');
  assert.match(
    setRates,
    /set_rates_calls_ <= 1 \|\| set_rates_calls_ % 60 == 0/,
    'rate control can churn sub-second, so the log must be sampled, not per-call'
  );
  assert.match(setRates, /requested_bps=/);
  assert.ok(header.includes('uint64_t set_rates_calls_ = 0;'));
});
