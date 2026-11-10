/*
 * PETAL PATCH (#500): create every macOS VideoToolbox H.264 encoder session
 * WITHOUT kVTVideoEncoderSpecification_EnableLowLatencyRateControl.
 * See vendor/webrtc-sys/PETAL_PATCH.md "Patch 7".
 *
 * The prebuilt libwebrtc's RTCVideoEncoderH264 adds that key to its encoder
 * specification unconditionally on macOS >= 11.3, inside
 * -resetCompressionSessionWithPixelFormat:, and libwebrtc is downloaded, not
 * built, so the call site cannot be edited. In low-latency mode, after an
 * H.264 keyframe at a low bitrate followed by a bitrate ramp (a simulcast
 * layer that starts behind the bandwidth estimate and takes the SFU's
 * layer-lock PLIs), VideoToolbox drops most of the next ~8 submitted frames
 * (kVTEncodeInfo_FrameDropped). For a sparse source -- an idle window that
 * redraws only when remote-control input changes it -- that is up to a second
 * of input that never reaches the viewer. With the key absent the same
 * sequence drops nothing.
 *
 * libwebrtc.a is linked statically into this image, so its undefined
 * reference to VTCompressionSessionCreate binds to the definition below at
 * static-link time (an object-file definition always takes precedence over a
 * dylib export). The override removes that one key from H.264 encoder
 * specifications and forwards everything else, unchanged, to VideoToolbox's
 * own function. Nothing else in this process creates compression sessions.
 */

#include "livekit/vt_h264_encoder_spec.h"

#import <CoreFoundation/CoreFoundation.h>
#import <VideoToolbox/VideoToolbox.h>
#import <sdk/objc/base/RTCVideoCodecInfo.h>
#import <sdk/objc/base/RTCVideoEncoderSettings.h>
#import <sdk/objc/components/video_codec/RTCVideoEncoderH264.h>
#include <dlfcn.h>

#include <atomic>

#include "rtc_base/logging.h"
#include "webrtc-sys/src/vt_h264_encoder_spec.rs.h"

namespace {

using VtCompressionSessionCreateFn = OSStatus (*)(CFAllocatorRef,
                                                  int32_t,
                                                  int32_t,
                                                  CMVideoCodecType,
                                                  CFDictionaryRef,
                                                  CFDictionaryRef,
                                                  CFAllocatorRef,
                                                  VTCompressionOutputCallback,
                                                  void*,
                                                  VTCompressionSessionRef*);

std::atomic<uint64_t> g_sessions{0};
std::atomic<uint64_t> g_low_latency_requested{0};
std::atomic<uint64_t> g_low_latency_forwarded{0};
std::atomic<uint64_t> g_failed{0};

// The last H.264 creation on THIS thread, so the probe reads its own call even
// while other threads (parallel tests) create sessions.
struct ThreadObservation {
  uint64_t sessions = 0;
  bool requested = false;
  bool forwarded = false;
  int32_t status = 0;
};
thread_local ThreadObservation t_observation;

VtCompressionSessionCreateFn RealVtCompressionSessionCreate() {
  // RTLD_NEXT skips this image (where the override lives) and finds
  // VideoToolbox's export.
  static const VtCompressionSessionCreateFn real =
      reinterpret_cast<VtCompressionSessionCreateFn>(
          dlsym(RTLD_NEXT, "VTCompressionSessionCreate"));
  return real;
}

// The key is weak-imported by the SDK (macOS 11.3+); nullptr on older systems.
CFStringRef LowLatencyRateControlKey() {
  if (@available(macOS 11.3, *)) {
    return kVTVideoEncoderSpecification_EnableLowLatencyRateControl;
  }
  return nullptr;
}

}  // namespace

extern "C" OSStatus VTCompressionSessionCreate(
    CFAllocatorRef allocator,
    int32_t width,
    int32_t height,
    CMVideoCodecType codec_type,
    CFDictionaryRef encoder_specification,
    CFDictionaryRef source_image_buffer_attributes,
    CFAllocatorRef compressed_data_allocator,
    VTCompressionOutputCallback output_callback,
    void* output_callback_ref_con,
    VTCompressionSessionRef* compression_session_out) {
  const VtCompressionSessionCreateFn real = RealVtCompressionSessionCreate();
  if (real == nullptr) {
    RTC_LOG(LS_ERROR) << "PETAL #500: VideoToolbox VTCompressionSessionCreate not found";
    return kVTCouldNotFindVideoEncoderErr;
  }
  const CFStringRef key = LowLatencyRateControlKey();
  const bool is_h264 = codec_type == kCMVideoCodecType_H264;
  const bool requested = is_h264 && key != nullptr && encoder_specification != nullptr &&
                         CFDictionaryContainsKey(encoder_specification, key);
  CFMutableDictionaryRef stripped = nullptr;
  CFDictionaryRef forwarded = encoder_specification;
  if (requested) {
    stripped = CFDictionaryCreateMutableCopy(kCFAllocatorDefault, 0, encoder_specification);
    if (stripped != nullptr) {
      CFDictionaryRemoveValue(stripped, key);
      forwarded = stripped;
    }
  }
  const OSStatus status =
      real(allocator, width, height, codec_type, forwarded, source_image_buffer_attributes,
           compressed_data_allocator, output_callback, output_callback_ref_con,
           compression_session_out);
  if (is_h264) {
    const bool kept = key != nullptr && forwarded != nullptr && CFDictionaryContainsKey(forwarded, key);
    g_sessions.fetch_add(1, std::memory_order_relaxed);
    if (requested) {
      g_low_latency_requested.fetch_add(1, std::memory_order_relaxed);
    }
    if (kept) {
      g_low_latency_forwarded.fetch_add(1, std::memory_order_relaxed);
    }
    if (status != noErr) {
      g_failed.fetch_add(1, std::memory_order_relaxed);
    }
    t_observation.sessions += 1;
    t_observation.requested = requested;
    t_observation.forwarded = kept;
    t_observation.status = status;
    // WARNING, not INFO: the app's log sink drops libwebrtc INFO lines. One
    // line per session creation (encoder start / resolution change), like #410.
    RTC_LOG(LS_WARNING) << "PETAL #500: H.264 VTCompressionSession " << width << "x" << height
                        << " created without low-latency rate control (requested="
                        << requested << ", forwarded=" << kept << ", status=" << status << ")";
  }
  if (stripped != nullptr) {
    CFRelease(stripped);
  }
  return status;
}

namespace livekit_ffi {

void EnsureVtH264EncoderSpecOverrideLinked() {}

VtH264SessionCounters VtH264SessionCountersSnapshot() {
  VtH264SessionCounters counters;
  counters.sessions = g_sessions.load(std::memory_order_relaxed);
  counters.low_latency_requested = g_low_latency_requested.load(std::memory_order_relaxed);
  counters.low_latency_forwarded = g_low_latency_forwarded.load(std::memory_order_relaxed);
  counters.failed = g_failed.load(std::memory_order_relaxed);
  return counters;
}

VtH264EncoderProbe VtH264StartEncoderProbe(uint32_t width, uint32_t height) {
  VtH264EncoderProbe probe;
  @autoreleasepool {
    // The same class RTCDefaultVideoEncoderFactory creates for every H.264
    // simulcast layer, with the codec info the #410 factory hands it.
    NSDictionary<NSString*, NSString*>* parameters = @{
      @"profile-level-id" : @"42e016",
      @"packetization-mode" : @"1",
      @"level-asymmetry-allowed" : @"1",
    };
    RTC_OBJC_TYPE(RTCVideoCodecInfo)* info =
        [[RTC_OBJC_TYPE(RTCVideoCodecInfo) alloc] initWithName:@"H264" parameters:parameters];
    RTC_OBJC_TYPE(RTCVideoEncoderH264)* encoder =
        [[RTC_OBJC_TYPE(RTCVideoEncoderH264) alloc] initWithCodecInfo:info];
    RTC_OBJC_TYPE(RTCVideoEncoderSettings)* settings =
        [[RTC_OBJC_TYPE(RTCVideoEncoderSettings) alloc] init];
    settings.name = @"H264";
    settings.width = static_cast<unsigned short>(width);
    settings.height = static_cast<unsigned short>(height);
    settings.startBitrate = 300;
    settings.minBitrate = 30;
    settings.maxBitrate = 4000;
    settings.maxFramerate = 30;
    settings.qpMax = 51;
    settings.mode = RTC_OBJC_TYPE(RTCVideoCodecModeScreensharing);
    // startEncodeWithSettings: creates the session synchronously on this
    // thread, so t_observation describes exactly this encoder's call.
    t_observation = ThreadObservation{};
    probe.start_result = [encoder startEncodeWithSettings:settings numberOfCores:1];
    probe.sessions = t_observation.sessions;
    probe.low_latency_requested = t_observation.requested;
    probe.low_latency_forwarded = t_observation.forwarded;
    probe.create_status = t_observation.status;
    [encoder releaseEncoder];
    [settings release];
    [encoder release];
    [info release];
  }
  return probe;
}

}  // namespace livekit_ffi
