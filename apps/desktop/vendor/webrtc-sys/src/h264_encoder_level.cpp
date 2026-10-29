/*
 * PETAL PATCH (#410): H.264 encoder level that fits the frame.
 *
 * The SFU answers every H.264 track with Level 3.1 (`42e01f`, max frame 3600
 * macroblocks ~= 1280x720). libwebrtc's ObjC RTCVideoEncoderH264 reads the
 * level from the codec info it is CREATED with and
 *   1. clamps the encoder frame rate to MaxMBPS / frame-macroblocks
 *      ("Encoder frame rate setting 29 is larger than the maximal allowed
 *      frame rate 5" for a 2560x1820 share, 13 for a 1080p camera), and
 *   2. asks VideoToolbox for an explicit Level 3.1 bitstream for realtime
 *      (camera) content -- 1920x1080 is ~2.3x that level's max frame size.
 *
 * The negotiated SDP stays untouched (the SFU codec registry and browsers
 * keep seeing exactly what they negotiated, #181). Only the encoder is built
 * with a level token that makes RTCVideoEncoderH264 use
 * kVTProfileLevel_H264_*_AutoLevel -- VideoToolbox then signals the smallest
 * level that fits the real frame size and rate, which is what the bitstreams
 * of window shares (screen content already maps to Baseline AutoLevel) have
 * always carried to every receiver -- and skip the level-derived frame-rate
 * clamp (upstream's documented "zero means auto rate setting" path).
 */

#include "livekit/h264_encoder_level.h"

#include "api/video_codecs/h264_profile_level_id.h"

namespace livekit_ffi {

namespace {

// Level whose upstream ExtractProfile() mapping is *_AutoLevel for every
// profile family (Baseline, Main, High) and whose GetMaxSampleRate() is 0.
constexpr webrtc::H264Level kEncoderAutoLevel = webrtc::H264Level::kLevel2_2;

// Upstream RTCVideoEncoderH264.mm GetMaxSampleRate(): MaxMBPS * 256.
uint64_t MaxSampleRate(webrtc::H264Level level) {
  switch (level) {
    case webrtc::H264Level::kLevel3:
      return 10368000;
    case webrtc::H264Level::kLevel3_1:
      return 27648000;
    case webrtc::H264Level::kLevel3_2:
      return 55296000;
    case webrtc::H264Level::kLevel4:
    case webrtc::H264Level::kLevel4_1:
      return 62914560;
    case webrtc::H264Level::kLevel4_2:
      return 133693440;
    case webrtc::H264Level::kLevel5:
      return 150994944;
    case webrtc::H264Level::kLevel5_1:
      return 251658240;
    case webrtc::H264Level::kLevel5_2:
      return 530841600;
    default:
      return 0;
  }
}

}  // namespace

std::string EncoderH264ProfileLevelIdFor(const std::string& negotiated) {
  auto parsed = webrtc::ParseH264ProfileLevelId(negotiated.c_str());
  if (!parsed) {
    return negotiated;
  }
  auto rewritten = webrtc::H264ProfileLevelIdToString(
      webrtc::H264ProfileLevelId(parsed->profile, kEncoderAutoLevel));
  return rewritten ? *rewritten : negotiated;
}

uint32_t H264EncoderMaxFrameRateFor(const std::string& profile_level_id,
                                    uint32_t width,
                                    uint32_t height) {
  auto parsed = webrtc::ParseH264ProfileLevelId(profile_level_id.c_str());
  if (!parsed || width == 0 || height == 0) {
    return 0;
  }
  const uint64_t aligned_width = ((uint64_t{width} + 15) >> 4) << 4;
  const uint64_t aligned_height = ((uint64_t{height} + 15) >> 4) << 4;
  return static_cast<uint32_t>(MaxSampleRate(parsed->level) /
                               (aligned_width * aligned_height));
}

rust::String EncoderH264ProfileLevelId(rust::Str negotiated) {
  return rust::String(
      EncoderH264ProfileLevelIdFor(std::string(negotiated)));
}

uint32_t H264EncoderMaxFrameRate(rust::Str profile_level_id,
                                 uint32_t width,
                                 uint32_t height) {
  return H264EncoderMaxFrameRateFor(std::string(profile_level_id), width,
                                    height);
}

}  // namespace livekit_ffi
