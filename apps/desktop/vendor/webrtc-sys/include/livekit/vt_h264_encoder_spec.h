/*
 * PETAL PATCH (#500): macOS VideoToolbox H.264 encoder specification.
 * See vendor/webrtc-sys/PETAL_PATCH.md "Patch 7".
 */

#pragma once

#include <cstdint>

#include "rust/cxx.h"

namespace livekit_ffi {

struct VtH264SessionCounters;
struct VtH264EncoderProbe;

// Called from CreateObjCVideoEncoderFactory so the object file carrying the
// VTCompressionSessionCreate override is always linked into the binary.
void EnsureVtH264EncoderSpecOverrideLinked();

// cxx bridge (src/vt_h264_encoder_spec.rs).
VtH264SessionCounters VtH264SessionCountersSnapshot();
VtH264EncoderProbe VtH264StartEncoderProbe(uint32_t width, uint32_t height);

}  // namespace livekit_ffi
