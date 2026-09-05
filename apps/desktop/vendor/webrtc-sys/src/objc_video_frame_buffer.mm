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

#include "livekit/video_frame_buffer.h"

#import <CoreVideo/CoreVideo.h>
#import <sdk/objc/components/video_frame_buffer/RTCCVPixelBuffer.h>
#include "sdk/objc/native/api/video_frame_buffer.h"

namespace livekit_ffi {

std::unique_ptr<VideoFrameBuffer> new_native_buffer_from_platform_image_buffer(
    CVPixelBufferRef pixelBuffer
) {
    // PETAL PATCH (#886): same MRC/no-pool hazard as
    // native_buffer_to_platform_image_buffer below -- this runs once per
    // CAPTURED frame on pool-less Rust threads, and
    // ObjCToNativeVideoFrameBuffer may autorelease intermediates. Drain a
    // local pool per call; the returned native buffer holds its own
    // C++-side reference and is pool-independent.
    @autoreleasepool {
        RTC_OBJC_TYPE(RTCCVPixelBuffer) *buffer = [[RTC_OBJC_TYPE(RTCCVPixelBuffer) alloc] initWithPixelBuffer:pixelBuffer];
        webrtc::scoped_refptr<webrtc::VideoFrameBuffer> frame_buffer = webrtc::ObjCToNativeVideoFrameBuffer(buffer);
        [buffer release];
        CVPixelBufferRelease(pixelBuffer);
        return std::make_unique<VideoFrameBuffer>(frame_buffer);
    }
}

CVPixelBufferRef native_buffer_to_platform_image_buffer(
    const std::unique_ptr<VideoFrameBuffer> &buffer
) {
    // PETAL PATCH (#886): this file compiles under MRC, and this function is
    // called once per decoded frame from Rust/tokio decode threads that have
    // NO autorelease pool. `NativeToObjCVideoFrameBuffer` autoreleases an
    // ObjC wrapper that retains the frame buffer (and with it the
    // CVPixelBuffer -> IOSurface); with no pool on the thread, that wrapper
    // was never released -- measured live as exactly one leaked IOSurface
    // per rendered frame (+29.8/s), unbounded for the process lifetime, the
    // #878 field-death mechanism. Draining a local pool per call is safe
    // for the returned +0 CVPixelBufferRef: it is owned by the caller-held
    // frame's buffer chain, not by the autoreleased intermediate.
    @autoreleasepool {
        id<RTC_OBJC_TYPE(RTCVideoFrameBuffer)> rtc_pixel_buffer = webrtc::NativeToObjCVideoFrameBuffer(buffer->get());

        if ([rtc_pixel_buffer isKindOfClass:[RTC_OBJC_TYPE(RTCCVPixelBuffer) class]]) {
            RTC_OBJC_TYPE(RTCCVPixelBuffer) *cv_pixel_buffer = (RTC_OBJC_TYPE(RTCCVPixelBuffer) *)rtc_pixel_buffer;
            return [cv_pixel_buffer pixelBuffer];
        } else {
            return nullptr;
        }
    }
}

}  // namespace livekit_ffi
