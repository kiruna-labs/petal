/*
 * PETAL PATCH (#504): cxx wrappers over src/mf/mf_enum_flags.h.
 */

#include "livekit/mf_enum_flags.h"

#include "mf/mf_enum_flags.h"

namespace livekit_ffi {

uint32_t MfH264EncoderEnumFlags(bool hardware_only) {
  return webrtc::mf_enum::H264EncoderEnumFlags(hardware_only);
}

bool MfEnumIncludesSyncMfts(uint32_t flags) {
  return webrtc::mf_enum::EnumIncludesSyncMfts(flags);
}

}  // namespace livekit_ffi
