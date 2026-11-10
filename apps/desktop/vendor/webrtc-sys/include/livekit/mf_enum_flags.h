/*
 * PETAL PATCH (#504): MF H.264 encoder enumeration policy, exposed over cxx.
 * See vendor/webrtc-sys/PETAL_PATCH.md and src/mf/mf_enum_flags.h.
 */

#pragma once

#include <cstdint>

namespace livekit_ffi {

uint32_t MfH264EncoderEnumFlags(bool hardware_only);
bool MfEnumIncludesSyncMfts(uint32_t flags);

}  // namespace livekit_ffi
