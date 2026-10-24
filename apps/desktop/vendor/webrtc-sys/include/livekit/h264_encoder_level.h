/*
 * PETAL PATCH (#410): H.264 encoder level that fits the frame.
 * See vendor/webrtc-sys/PETAL_PATCH.md.
 */

#pragma once

#include <cstdint>
#include <string>

#include "rust/cxx.h"

namespace livekit_ffi {

// C++ entry points used by objc_video_factory.mm.
std::string EncoderH264ProfileLevelIdFor(const std::string& negotiated);
uint32_t H264EncoderMaxFrameRateFor(const std::string& profile_level_id,
                                    uint32_t width,
                                    uint32_t height);

// cxx bridge wrappers (src/h264_encoder_level.rs).
rust::String EncoderH264ProfileLevelId(rust::Str negotiated);
uint32_t H264EncoderMaxFrameRate(rust::Str profile_level_id,
                                 uint32_t width,
                                 uint32_t height);

}  // namespace livekit_ffi
