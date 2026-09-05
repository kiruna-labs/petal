#ifndef WEBRTC_MF_H264_ENCODER_IMPL_H_
#define WEBRTC_MF_H264_ENCODER_IMPL_H_

// Windows Media Foundation H.264 hardware encoder for libwebrtc, modeled on
// Chromium's MediaFoundationVideoEncodeAccelerator (media/gpu/windows/):
// a dedicated encoder thread drives the MFT through the generic asynchronous
// contract — IMFMediaEventGenerator events (METransformNeedInput /
// METransformHaveOutput) feed a pending input queue and pull output samples.
// No per-frame drain, no vendor-specific workarounds.
//
// Compiled only on Windows (USE_MF_VIDEO_CODEC in build.rs).

#include <codecapi.h>
#include <mfobjects.h>
#include <mftransform.h>
#include <strmif.h>
#include <wrl/client.h>
#include <wrl/implements.h>

#include <atomic>
#include <condition_variable>
#include <deque>
#include <memory>
#include <mutex>
#include <thread>
#include <vector>

#include "api/video/i420_buffer.h"
#include "api/video/video_codec_constants.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_encoder.h"

namespace webrtc {

class MfH264EncoderImpl : public VideoEncoder {
 public:
  explicit MfH264EncoderImpl(const SdpVideoFormat& format);
  ~MfH264EncoderImpl() override;

  static bool IsSupported();

  int32_t InitEncode(const VideoCodec* codec_settings,
                     const Settings& settings) override;

  int32_t RegisterEncodeCompleteCallback(
      EncodedImageCallback* callback) override;

  int32_t Release() override;

  int32_t Encode(const VideoFrame& frame,
                 const std::vector<VideoFrameType>* frame_types) override;

  void SetRates(const RateControlParameters& rc_parameters) override;

  EncoderInfo GetEncoderInfo() const override;

 private:
  // One queued input frame, waiting for a METransformNeedInput event.
  struct PendingInput {
    Microsoft::WRL::ComPtr<IMFSample> sample;
    uint32_t rtp_timestamp = 0;  // 90 kHz units, echoed onto the output
    bool keyframe = false;
  };

  // Metadata echoed to ProcessOutput to tag the encoded image.
  struct OutputMetadata {
    uint32_t rtp_timestamp = 0;
    bool keyframe = false;
  };

 public:
  // Shared with the IMFAsyncCallback proxy so an OS-thread event callback
  // never touches `this` (avoids use-after-free on shutdown). The encoder
  // thread owns the MFT and all dispatch. Public because the proxy (defined
  // in the anonymous namespace of the .cpp) must construct it.
  struct AsyncState {
    std::mutex mutex;
    std::condition_variable cv;
    std::deque<std::pair<MediaEventType, HRESULT>> events;
    bool stopped = false;
  };

 private:

  int32_t ConfigureMft(int width, int height, int max_framerate,
                       uint32_t target_bps);
  // Shared MFT bring-up: find+activate a hardware encoder, unlock the async
  // contract, negotiate types, start the event loop + encoder thread. Used by
  // both InitEncode and ReconfigureMft.
  int32_t InitMft(int width, int height);
  // Rebuild the MFT + event loop at a new resolution, IN PLACE (Chromium's
  // MFVEA re-init on resolution change; no track republish, no encoder churn).
  // Called from Encode when the source frame size changes; the receiver
  // renegotiates via the new SPS (format-change path).
  int32_t ReconfigureMft(int new_width, int new_height);
  void EncoderThreadMain();
  void HandleEvent(MediaEventType event_type, HRESULT status);
  void FeedInputs();
  void ProcessOutput();
  // Complete async-MFT teardown (Chromium MFVEA Reset() sequence).
  void TeardownMft();

  Microsoft::WRL::ComPtr<IMFTransform> mft_;
  Microsoft::WRL::ComPtr<IMFMediaEventGenerator> event_generator_;
  Microsoft::WRL::ComPtr<ICodecAPI> codec_api_;
  Microsoft::WRL::ComPtr<IMFAsyncCallback> async_callback_;
  DWORD input_stream_id_ = 0;
  DWORD output_stream_id_ = 0;

  // Encoder thread + queues (Chromium's pending_input_queue_ equivalent).
  std::thread encoder_thread_;
  std::shared_ptr<AsyncState> async_state_;
  std::mutex input_mutex_;
  std::deque<PendingInput> pending_input_queue_;
  std::deque<OutputMetadata> output_metadata_queue_;
  // Atomic count of queued inputs so the encoder thread can be woken to feed
  // the MFT immediately when it is known to be waiting (Chromium's
  // 'if (encoder_needs_input_counter_ > 0) FeedInputs()' kick), without
  // racing on the deque itself.
  std::atomic<int> pending_input_count_{0};

  // True when the MFT provides its own output samples (Chromium reads
  // GetOutputStreamInfo flags; caller-allocated samples are used otherwise).
  bool mft_provides_output_samples_ = false;
  Microsoft::WRL::ComPtr<IMFSample> output_sample_storage_;
  Microsoft::WRL::ComPtr<IMFMediaBuffer> output_buffer_storage_;

  const SdpVideoFormat format_;
  EncodedImageCallback* encoded_image_callback_ = nullptr;

  std::atomic<bool> configured_{false};
  std::atomic<bool> encode_failed_{false};
  int need_input_counter_ = 0;

  int width_ = 0;
  int height_ = 0;
  int max_framerate_ = 0;
  uint32_t target_bps_ = 0;

  // Reused encoded-image scaffolding (only touched on the encoder thread).
  EncodedImage encoded_image_;
};

}  // namespace webrtc

#endif  // WEBRTC_MF_H264_ENCODER_IMPL_H_
