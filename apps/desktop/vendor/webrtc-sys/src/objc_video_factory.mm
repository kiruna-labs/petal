/*
 * Copyright 2025 LiveKit, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include "livekit/objc_video_factory.h"

#import <sdk/objc/components/video_codec/RTCDefaultVideoDecoderFactory.h>
#import <sdk/objc/components/video_codec/RTCDefaultVideoEncoderFactory.h>
#import <sdk/objc/components/video_codec/RTCVideoEncoderFactorySimulcast.h>
#include "sdk/objc/native/api/video_decoder_factory.h"
#include "sdk/objc/native/api/video_encoder_factory.h"

#include <string>

#include "livekit/h264_encoder_level.h"
#include "rtc_base/logging.h"

// PETAL PATCH (#410): build every H.264 encoder with a level that fits the
// frame instead of the negotiated Level 3.1 -- see h264_encoder_level.cpp
// and PETAL_PATCH.md. Wraps the default factory (the simulcast factory below
// creates each layer's encoder through it), forwards everything else
// untouched, and never changes `supportedCodecs`, so SDP is unaffected.
@interface PetalLevelFittingEncoderFactory : NSObject <RTC_OBJC_TYPE(RTCVideoEncoderFactory)>
- (instancetype)initWithFactory:(id<RTC_OBJC_TYPE(RTCVideoEncoderFactory)>)inner;
@end

@implementation PetalLevelFittingEncoderFactory {
  id<RTC_OBJC_TYPE(RTCVideoEncoderFactory)> _inner;  // retained (MRC)
}

- (instancetype)initWithFactory:(id<RTC_OBJC_TYPE(RTCVideoEncoderFactory)>)inner {
  self = [super init];
  if (self) {
    _inner = [inner retain];
  }
  return self;
}

- (void)dealloc {
  [_inner release];
  [super dealloc];
}

- (NSArray<RTC_OBJC_TYPE(RTCVideoCodecInfo) *> *)supportedCodecs {
  return [_inner supportedCodecs];
}

- (nullable id<RTC_OBJC_TYPE(RTCVideoEncoder)>)createEncoder:(RTC_OBJC_TYPE(RTCVideoCodecInfo) *)info {
  NSString *negotiated = info.parameters[@"profile-level-id"];
  if (![info.name isEqualToString:@"H264"] || negotiated == nil) {
    return [_inner createEncoder:info];
  }
  const std::string negotiatedId(negotiated.UTF8String);
  const std::string fitted = livekit_ffi::EncoderH264ProfileLevelIdFor(negotiatedId);
  if (fitted == negotiatedId) {
    return [_inner createEncoder:info];
  }
  // Explicit +1/release, no autorelease: this runs on WebRTC's encoder
  // thread, which has no autorelease pool of its own (see #889 above).
  NSMutableDictionary<NSString *, NSString *> *parameters = [info.parameters mutableCopy];
  NSString *fittedId = [[NSString alloc] initWithUTF8String:fitted.c_str()];
  parameters[@"profile-level-id"] = fittedId;
  [fittedId release];
  RTC_OBJC_TYPE(RTCVideoCodecInfo) *fittedInfo =
      [[RTC_OBJC_TYPE(RTCVideoCodecInfo) alloc] initWithName:info.name parameters:parameters];
  [parameters release];
  // WARNING, not INFO: the app's log sink drops libwebrtc INFO lines.
  RTC_LOG(LS_WARNING) << "PETAL #410: H.264 encoder created with profile-level-id "
                      << fitted << " (VideoToolbox AutoLevel, no level frame-rate clamp) for negotiated "
                      << negotiatedId;
  id<RTC_OBJC_TYPE(RTCVideoEncoder)> encoder = [_inner createEncoder:fittedInfo];
  [fittedInfo release];
  return encoder;
}

// Optional protocol methods (implementations, encoderSelector,
// queryCodecSupport...) go to the wrapped factory exactly as before.
- (BOOL)respondsToSelector:(SEL)selector {
  return [super respondsToSelector:selector] || [_inner respondsToSelector:selector];
}

- (id)forwardingTargetForSelector:(SEL)selector {
  if ([_inner respondsToSelector:selector]) {
    return _inner;
  }
  return [super forwardingTargetForSelector:selector];
}

@end

namespace livekit_ffi {

// PETAL PATCH (#889): this file compiles under MRC (build.rs never passes
// -fobjc-arc), so every `[[X alloc] init]` here is a +1 the caller owns.
// Upstream returned the native wrapper without ever releasing those locals,
// and `ObjCToNative*Factory` takes its OWN reference -- so each call leaked
// the factory objects outright. `leaks(1)` on a live 2.4GB session named
// them: 10 orphaned `webrtc::ObjCVideoEncoderFactory` roots, one per
// factory creation, all in the encode path -- which is why memory grows
// when a share STARTS and never while frames merely flow. Release the
// locals after the conversion retains them, and drain a pool around the
// ObjC work since these run on pool-less Rust threads (same hazard as
// objc_video_frame_buffer.mm's #886 patch).
std::unique_ptr<webrtc::VideoEncoderFactory> CreateObjCVideoEncoderFactory() {
  @autoreleasepool {
    RTC_OBJC_TYPE(RTCDefaultVideoEncoderFactory)* defaultFactory = [[RTC_OBJC_TYPE(RTCDefaultVideoEncoderFactory) alloc] init];
    // PETAL PATCH (#410): every encoder (each simulcast layer included) is
    // created through the level-fitting wrapper.
    PetalLevelFittingEncoderFactory* encoderFactory =
        [[PetalLevelFittingEncoderFactory alloc] initWithFactory:defaultFactory];
    RTC_OBJC_TYPE(RTCVideoEncoderFactorySimulcast)* simulcastFactory =
        [[RTC_OBJC_TYPE(RTCVideoEncoderFactorySimulcast) alloc] initWithPrimary:encoderFactory fallback:encoderFactory];
    std::unique_ptr<webrtc::VideoEncoderFactory> native =
        webrtc::ObjCToNativeVideoEncoderFactory(simulcastFactory);
    // The simulcast factory retains primary+fallback; the native wrapper
    // retains the simulcast factory; the wrapper retains the default
    // factory. All three locals are ours to release.
    [simulcastFactory release];
    [encoderFactory release];
    [defaultFactory release];
    return native;
  }
}

std::unique_ptr<webrtc::VideoDecoderFactory> CreateObjCVideoDecoderFactory() {
  @autoreleasepool {
    RTC_OBJC_TYPE(RTCDefaultVideoDecoderFactory)* decoderFactory = [[RTC_OBJC_TYPE(RTCDefaultVideoDecoderFactory) alloc] init];
    std::unique_ptr<webrtc::VideoDecoderFactory> native =
        webrtc::ObjCToNativeVideoDecoderFactory(decoderFactory);
    [decoderFactory release];
    return native;
  }
}

}  // namespace livekit_ffi
