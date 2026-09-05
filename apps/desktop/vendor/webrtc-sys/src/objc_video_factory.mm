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
    RTC_OBJC_TYPE(RTCDefaultVideoEncoderFactory)* encoderFactory = [[RTC_OBJC_TYPE(RTCDefaultVideoEncoderFactory) alloc] init];
    RTC_OBJC_TYPE(RTCVideoEncoderFactorySimulcast)* simulcastFactory =
        [[RTC_OBJC_TYPE(RTCVideoEncoderFactorySimulcast) alloc] initWithPrimary:encoderFactory fallback:encoderFactory];
    std::unique_ptr<webrtc::VideoEncoderFactory> native =
        webrtc::ObjCToNativeVideoEncoderFactory(simulcastFactory);
    // The simulcast factory retains primary+fallback; the native wrapper
    // retains the simulcast factory. Both locals are ours to release.
    [simulcastFactory release];
    [encoderFactory release];
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
