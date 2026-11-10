// MFTEnumEx flag policy for the Windows Media Foundation H.264 encoder path.
//
// Kept free of <windows.h> so the policy is unit-testable on any host
// (src/mf_enum_flags.rs -> the desktop crate's cargo test --lib).
// mf_common.h static_asserts that these mirrors equal the real
// MFT_ENUM_FLAG_* values from mfapi.h.

#pragma once

#include <cstdint>

namespace webrtc {
namespace mf_enum {

inline constexpr uint32_t kSyncMft = 0x00000001;        // MFT_ENUM_FLAG_SYNCMFT
inline constexpr uint32_t kAsyncMft = 0x00000002;       // MFT_ENUM_FLAG_ASYNCMFT
inline constexpr uint32_t kHardware = 0x00000004;       // MFT_ENUM_FLAG_HARDWARE
inline constexpr uint32_t kSortAndFilter = 0x00000040;  // MFT_ENUM_FLAG_SORTANDFILTER

// MFTEnumEx flags are a UNION of MFT kinds, not a filter: SYNCMFT adds every
// synchronous (software) MFT. A "hardware only" enumeration must therefore
// never carry SYNCMFT -- with it, a machine with no hardware encoder gets
// Microsoft's synchronous software H.264 MFT, which the async-only encoder
// cannot drive, and every window share encodes zero frames (#504).
// Hardware MFTs are asynchronous by contract, so HARDWARE|ASYNCMFT loses none.
constexpr uint32_t H264EncoderEnumFlags(bool hardware_only) {
  return hardware_only ? (kHardware | kAsyncMft | kSortAndFilter)
                       : (kSyncMft | kAsyncMft | kHardware | kSortAndFilter);
}

// True when an enumeration with `flags` can return a synchronous software
// MFT.
constexpr bool EnumIncludesSyncMfts(uint32_t flags) {
  return (flags & kSyncMft) != 0;
}

static_assert(!EnumIncludesSyncMfts(H264EncoderEnumFlags(true)),
              "hardware-only encoder enumeration must exclude sync MFTs (#504)");
static_assert((H264EncoderEnumFlags(true) & kHardware) != 0 &&
                  (H264EncoderEnumFlags(true) & kAsyncMft) != 0,
              "hardware-only encoder enumeration must include async hardware MFTs");

}  // namespace mf_enum
}  // namespace webrtc
