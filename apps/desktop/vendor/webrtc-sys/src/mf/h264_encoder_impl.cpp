// Windows Media Foundation H.264 hardware encoder (webrtc::VideoEncoder).
//
// Modeled on Chromium's MediaFoundationVideoEncodeAccelerator
// (media/gpu/windows/media_foundation_video_encode_accelerator_win.cc): a
// dedicated encoder thread drives the MFT through the generic asynchronous
// contract — IMFMediaEventGenerator events (METransformNeedInput /
// METransformHaveOutput) feed a pending input queue and pull output samples.
// Keyframes use CODECAPI_AVEncVideoForceKeyFrame; drain is never issued per
// frame. This is the vendor-neutral contract that works on NVIDIA, Intel, and
// AMD hardware MFTs alike.

#include "h264_encoder_impl.h"

#include <windows.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <wrl/client.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <thread>

#include "api/video/encoded_image.h"
#include "api/video/video_frame.h"
#include "api/video_codecs/video_codec.h"
#include "common_video/h264/h264_common.h"
#include "mf_common.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "rtc_base/logging.h"
#include "third_party/libyuv/include/libyuv/convert.h"

namespace webrtc {

namespace {

// Routes IMFMediaEventGenerator events from the OS callback thread to the
// encoder thread via a shared AsyncState. Never touches the encoder directly,
// so the encoder may be destroyed while an event is in flight.
class MfAsyncCallbackProxy
    : public Microsoft::WRL::RuntimeClass<
          Microsoft::WRL::RuntimeClassFlags<Microsoft::WRL::ClassicCom>,
          IMFAsyncCallback> {
 public:
  explicit MfAsyncCallbackProxy(
      std::shared_ptr<MfH264EncoderImpl::AsyncState> state)
      : state_(std::move(state)) {}

  IFACEMETHODIMP GetParameters(DWORD* pdwFlags, DWORD* pdwQueue) override {
    *pdwFlags = MFASYNC_FAST_IO_PROCESSING_CALLBACK;
    *pdwQueue = MFASYNC_CALLBACK_QUEUE_TIMER;
    return S_OK;
  }

  IFACEMETHODIMP Invoke(IMFAsyncResult* pAsyncResult) override {
    EnsureComInitialized();
    MediaEventType event_type = MEUnknown;
    HRESULT status = S_OK;
    Microsoft::WRL::ComPtr<IUnknown> state;
    HRESULT hr = pAsyncResult->GetState(&state);
    Microsoft::WRL::ComPtr<IMFMediaEventGenerator> event_generator;
    if (SUCCEEDED(hr)) {
      hr = state.As(&event_generator);
    }
    Microsoft::WRL::ComPtr<IMFMediaEvent> media_event;
    if (SUCCEEDED(hr)) {
      hr = event_generator->EndGetEvent(pAsyncResult, &media_event);
    }
    if (SUCCEEDED(hr)) {
      hr = media_event->GetType(&event_type);
    }
    if (SUCCEEDED(hr)) {
      media_event->GetStatus(&status);
    }
    {
      std::lock_guard<std::mutex> lock(state_->mutex);
      state_->events.emplace_back(event_type, status);
    }
    state_->cv.notify_one();
    return S_OK;
  }

 private:
  std::shared_ptr<MfH264EncoderImpl::AsyncState> state_;
};

}  // namespace

MfH264EncoderImpl::MfH264EncoderImpl(const SdpVideoFormat& format)
    : format_(format) {}

MfH264EncoderImpl::~MfH264EncoderImpl() {
  // Release() deliberately KEEPS the MFT alive so webrtc's
  // Release()-then-InitEncode() reuse handshake can reconfigure the SAME
  // instance in place (no new NVENC session per re-anchor). The destructor
  // therefore performs the FINAL teardown so the encoder's session is
  // returned when the wrapper object actually dies.
  Release();
  TeardownMft();
  event_generator_ = nullptr;
  codec_api_ = nullptr;
  async_callback_ = nullptr;
  mft_ = nullptr;
}

bool MfH264EncoderImpl::IsSupported() {
  EnsureComInitialized();
  Microsoft::WRL::ComPtr<IMFTransform> mft;
  HRESULT hr = FindAndActivateH264Mft(mf_guids::kCategoryVideoEncoder,
                                      /*hardware_only=*/true, nullptr, &mft);
  return SUCCEEDED(hr) && mft != nullptr;
}

int32_t MfH264EncoderImpl::InitEncode(const VideoCodec* codec_settings,
                                      const Settings& /*settings*/) {
  if (!codec_settings || codec_settings->codecType != kVideoCodecH264) {
    return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
  }
  EnsureComInitialized();

  width_ = codec_settings->width;
  height_ = codec_settings->height;
  max_framerate_ = codec_settings->maxFramerate > 0 ? codec_settings->maxFramerate : 30;
  target_bps_ = codec_settings->startBitrate * 1000;

  if (width_ <= 0 || height_ <= 0) {
    RTC_LOG(LS_ERROR) << "MF H264 encoder: unsupported dimensions " << width_
                      << "x" << height_;
    return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
  }
  // The MFT is negotiated at the exact visible size (odd included), exactly
  // like Chromium's MFVEA (input_visible_size_). The MFT codes the 16-aligned
  // picture internally and writes SPS frame_cropping for the remainder, so
  // receivers render the exact visible size with no padding borders.

  int32_t init_result = InitMft(width_, height_);
  if (init_result != WEBRTC_VIDEO_CODEC_OK) {
    return init_result;
  }
  configured_.store(true);
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MfH264EncoderImpl::InitMft(int width, int height) {
  EnsureComInitialized();

  const bool reuse = (mft_ != nullptr);
  if (!reuse) {
    HRESULT hr = FindAndActivateH264Mft(mf_guids::kCategoryVideoEncoder,
                                        /*hardware_only=*/true, nullptr, &mft_);
    if (FAILED(hr) || !mft_) {
      RTC_LOG(LS_ERROR) << "MF H264 encoder: no hardware encoder MFT available";
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }

    hr = UnlockAsyncTransform(mft_.Get());
    if (FAILED(hr)) {
      RTC_LOG(LS_ERROR) << "MF H264 encoder: failed to unlock async transform";
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }

    hr = mft_->QueryInterface(IID_PPV_ARGS(&event_generator_));
    if (FAILED(hr)) {
      // Chromium requires an asynchronous MFT with an event generator; a
      // synchronous MFT (e.g. some software MF encoders) is handled by the
      // OpenH264 fallback instead.
      RTC_LOG(LS_ERROR) << "MF H264 encoder: MFT is not asynchronous";
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }

    // ICodecAPI for keyframe + bitrate control (optional; some MFTs lack it).
    mft_.As(&codec_api_);

    // Window-share quality (Petal): DEFAULT to QUALITY rate-control mode.
    // The bitrate-driven default settles at QP 26 under the BWE-bound target
    // (~3.4Mbps on a static share), which softens remote text on every host
    // (measured: QP 26; fuzzy). Quality mode ignores the bitrate target and
    // minimizes QP (measured live: QP 26 -> 16, crisp text — the fix).
    // Simulcast safety: a constrained receiver still downgrades to the
    // bitrate-capped q rung via the SFU, so the h rung's higher bitrate only
    // costs bandwidth when it is actually being watched. Set
    // PETAL_MF_QUALITY_MODE=0 to opt back into bitrate-driven encoding.
    if (codec_api_) {
      const char* qm = std::getenv("PETAL_MF_QUALITY_MODE");
      const bool quality_mode = qm == nullptr || std::strcmp(qm, "0") != 0;
      if (quality_mode) {
        VARIANT mode;
        mode.vt = VT_UI4;
        mode.ulVal = eAVEncCommonRateControlMode_Quality;
        codec_api_->SetValue(&CODECAPI_AVEncCommonRateControlMode, &mode);
        VARIANT quality;
        quality.vt = VT_UI4;
        quality.ulVal = 100;
        codec_api_->SetValue(&CODECAPI_AVEncCommonQuality, &quality);
        VARIANT qvs;
        qvs.vt = VT_UI4;
        qvs.ulVal = 100;
        codec_api_->SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &qvs);
        RTC_LOG(LS_INFO)
            << "MF H264 encoder: quality rate control enabled (QP minimized; "
               "set PETAL_MF_QUALITY_MODE=0 to disable)";
      }
    }

    if (!ResolveStreamIds(mft_.Get(), &input_stream_id_, &output_stream_id_)) {
      RTC_LOG(LS_ERROR) << "MF H264 encoder: could not resolve stream ids";
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
  } else {
    // REUSE (webrtc Release()s and then InitEncode()s the SAME object at
    // every re-anchor): reconfigure the existing MFT in place so NO new
    // NVENC session is created. FLUSH discards pending samples; DRAIN then
    // forces the MFT to raise an event that the stopped pump thread's
    // callback consumes via EndGetEvent — completing its still-pending
    // BeginGetEvent registration. We re-arm IMMEDIATELY (bounded retry) and
    // use the registration's completion as the drain-settle barrier: the
    // retry only succeeds once the drain-complete has been delivered and
    // the old registration freed, which proves the MFT has finished
    // draining. Only THEN do we end the stream and renegotiate the media
    // types. (A07's build waited 5s after the drain and survived 10+
    // re-anchors; A09's fast EOS/SetOutputType while the MFT was still
    // draining corrupted its internal state over ~10 cycles -> heap
    // corruption detected in webrtc's SdpVideoFormat teardown. NO GetEvent
    // polling: the drain-complete goes to the old callback, never to
    // GetEvent, so a pump just burns its timeout — the A07 ~5s q-rung
    // stall.)
    mft_->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
    mft_->ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
    async_state_ = std::make_shared<AsyncState>();
    auto callback = Microsoft::WRL::Make<MfAsyncCallbackProxy>(async_state_);
    async_callback_ = callback;  // keep the proxy alive for the MFT's lifetime
    HRESULT hr = E_FAIL;
    for (int attempt = 0; attempt < 40; ++attempt) {
      hr = event_generator_->BeginGetEvent(callback.Get(), event_generator_.Get());
      if (SUCCEEDED(hr)) {
        break;
      }
      std::this_thread::sleep_for(std::chrono::milliseconds(50));
    }
    if (FAILED(hr)) {
      RTC_LOG(LS_ERROR) << "MF H264 encoder: BeginGetEvent failed " << hr;
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
    // Drain settled; now end the stream and renegotiate the types.
    mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
    mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
  }

  int32_t config_result =
      ConfigureMft(width, height, max_framerate_, target_bps_);
  if (config_result != WEBRTC_VIDEO_CODEC_OK) {
    return config_result;
  }

  if (!reuse) {
    // Fresh instance: start the asynchronous processing model (Chromium's
    // InitializeMFT). A fresh MFT has no pending registration, so the first
    // BeginGetEvent succeeds.
    async_state_ = std::make_shared<AsyncState>();
    auto callback = Microsoft::WRL::Make<MfAsyncCallbackProxy>(async_state_);
    async_callback_ = callback;  // keep the proxy alive for the MFT's lifetime
    HRESULT hr = event_generator_->BeginGetEvent(callback.Get(), event_generator_.Get());
    if (FAILED(hr)) {
      RTC_LOG(LS_ERROR) << "MF H264 encoder: BeginGetEvent failed " << hr;
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
  }
  mft_->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
  mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
  mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

  encoder_thread_ = std::thread(&MfH264EncoderImpl::EncoderThreadMain, this);
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MfH264EncoderImpl::ReconfigureMft(int new_width, int new_height) {
  RTC_LOG(LS_INFO) << "MF H264 encoder: in-place resolution change to "
                   << new_width << "x" << new_height;
  // Stop the encoder thread (it only touches the MFT inside HandleEvent
  // dispatch; setting stopped + notifying lets it finish any in-flight
  // dispatch and exit the loop, so by the time we join, no thread
  // references the MFT).
  {
    std::lock_guard<std::mutex> lock(async_state_->mutex);
    async_state_->stopped = true;
  }
  async_state_->cv.notify_all();
  if (encoder_thread_.joinable()) {
    encoder_thread_.join();
  }
  {
    std::lock_guard<std::mutex> lock(input_mutex_);
    pending_input_queue_.clear();
    output_metadata_queue_.clear();
    pending_input_count_.store(0, std::memory_order_release);
  }
  need_input_counter_ = 0;
  width_ = new_width;
  height_ = new_height;
  // Reuse the SAME MFT in place (see InitMft): FLUSH + DRAIN + media-type
  // renegotiation at the new size. No new MFT -> no new NVENC session, so
  // the re-anchor never approaches the GeForce 12-session cap.
  int32_t rc = InitMft(new_width, new_height);
  if (rc != WEBRTC_VIDEO_CODEC_OK) {
    RTC_LOG(LS_ERROR) << "MF H264 encoder: reconfiguration to " << new_width
                      << "x" << new_height << " failed (" << rc << ")";
    return rc;
  }
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MfH264EncoderImpl::ConfigureMft(int width, int height,
                                        int max_framerate,
                                        uint32_t target_bps) {
  HRESULT hr = S_OK;

  // Output type FIRST: transcode-style async MFTs reject an input type set
  // before the output type (MF_E_TRANSFORM_TYPE_NOT_SET).
  Microsoft::WRL::ComPtr<IMFMediaType> output_type;
  hr = MFCreateMediaType(&output_type);
  if (SUCCEEDED(hr)) {
    hr = output_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
  }
  if (SUCCEEDED(hr)) {
    hr = output_type->SetGUID(MF_MT_SUBTYPE, mf_guids::kH264);
  }
  if (SUCCEEDED(hr) && target_bps > 0) {
    hr = output_type->SetUINT32(MF_MT_AVG_BITRATE, target_bps);
  }
  if (SUCCEEDED(hr)) {
    hr = MFSetAttributeRatio(output_type.Get(), MF_MT_FRAME_RATE,
                             max_framerate, 1);
  }
  if (SUCCEEDED(hr)) {
    hr = MFSetAttributeSize(output_type.Get(), MF_MT_FRAME_SIZE, width,
                            height);
  }
  if (SUCCEEDED(hr)) {
    hr = output_type->SetUINT32(MF_MT_INTERLACE_MODE,
                                MFVideoInterlace_Progressive);
  }
  if (SUCCEEDED(hr)) {
    // Baseline profile (42e01f in SDP). The H.264 encoder MFT derives the
    // SPS from the configured size (verified by probe at 720p/960p/976p), so
    // no padding or fixed-size workaround is needed.
    hr = output_type->SetUINT32(MF_MT_MPEG2_PROFILE, 66);
  }
  if (SUCCEEDED(hr)) {
    hr = mft_->SetOutputType(output_stream_id_, output_type.Get(), 0);
  }
  if (FAILED(hr)) {
    RTC_LOG(LS_ERROR) << "MF H264 encoder: SetOutputType failed " << hr;
    return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
  }

  // Input type SECOND: NV12 at the same size/rate.
  Microsoft::WRL::ComPtr<IMFMediaType> input_type;
  hr = MFCreateMediaType(&input_type);
  if (SUCCEEDED(hr)) {
    hr = input_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
  }
  if (SUCCEEDED(hr)) {
    hr = input_type->SetGUID(MF_MT_SUBTYPE, mf_guids::kNv12);
  }
  if (SUCCEEDED(hr)) {
    hr = MFSetAttributeRatio(input_type.Get(), MF_MT_FRAME_RATE,
                             max_framerate, 1);
  }
  if (SUCCEEDED(hr)) {
    hr = MFSetAttributeSize(input_type.Get(), MF_MT_FRAME_SIZE, width,
                            height);
  }
  if (SUCCEEDED(hr)) {
    hr = input_type->SetUINT32(MF_MT_INTERLACE_MODE,
                                MFVideoInterlace_Progressive);
  }
  if (SUCCEEDED(hr)) {
    hr = mft_->SetInputType(input_stream_id_, input_type.Get(), 0);
  }
  if (FAILED(hr)) {
    RTC_LOG(LS_ERROR) << "MF H264 encoder: SetInputType failed " << hr;
    return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
  }

  // Does the MFT provide its own output samples, or must we allocate them?
  // (Chromium reads these flags; ProcessOutput passes pSample=NULL only when
  // MFT_OUTPUT_STREAM_PROVIDES_SAMPLES is set.)
  MFT_OUTPUT_STREAM_INFO out_info = {};
  mft_provides_output_samples_ = false;
  if (SUCCEEDED(mft_->GetOutputStreamInfo(output_stream_id_, &out_info))) {
    mft_provides_output_samples_ =
        (out_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES) != 0;
  }
  if (!mft_provides_output_samples_) {
    // Pre-allocate one reusable output sample for the caller-allocated path.
    const DWORD out_capacity = static_cast<DWORD>(width) * height * 3 / 2;
    hr = MFCreateAlignedMemoryBuffer(out_capacity, 0, &output_buffer_storage_);
    if (SUCCEEDED(hr)) {
      hr = MFCreateSample(&output_sample_storage_);
    }
    if (SUCCEEDED(hr)) {
      hr = output_sample_storage_->AddBuffer(output_buffer_storage_.Get());
    }
    if (FAILED(hr)) {
      RTC_LOG(LS_ERROR) << "MF H264 encoder: output sample allocation failed "
                        << hr;
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
  }
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MfH264EncoderImpl::RegisterEncodeCompleteCallback(
    EncodedImageCallback* callback) {
  encoded_image_callback_ = callback;
  return WEBRTC_VIDEO_CODEC_OK;
}

// Complete async-MFT teardown (Chromium MFVEA Reset() sequence). FLUSH
// discards pending samples; END_OF_STREAM + END_STREAMING end the streaming
// state; clearing the media types lets the MFT release its internal
// streaming self-reference and return its NVENC session. The earlier
// FLUSH+END_OF_STREAM-only sequence was measured to LEAK the old MFT's
// session on every re-anchor (probe A06 2026-08-06: live_encoders stayed 2
// through 5 in-place ReconfigureMft swaps while the GPU sessions
// accumulated to the GeForce 12-session cap -> the 6th MFT creation failed
// -> OpenH264 fallback -> 0 RTP -> receiver freeze).
void MfH264EncoderImpl::TeardownMft() {
  if (!mft_) {
    return;
  }
  mft_->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
  mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
  mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
  mft_->SetOutputType(output_stream_id_, nullptr, 0);
  mft_->SetInputType(input_stream_id_, nullptr, 0);
}

int32_t MfH264EncoderImpl::Release() {
  if (encoder_thread_.joinable()) {
    {
      std::lock_guard<std::mutex> lock(async_state_->mutex);
      async_state_->stopped = true;
    }
    async_state_->cv.notify_all();
    encoder_thread_.join();
  }
  configured_.store(false);
  encode_failed_.store(false);
  {
    std::lock_guard<std::mutex> lock(input_mutex_);
    pending_input_queue_.clear();
    output_metadata_queue_.clear();
    pending_input_count_.store(0, std::memory_order_release);
  }
  need_input_counter_ = 0;
  // The MFT is deliberately KEPT: at every re-anchor webrtc calls Release()
  // and then InitEncode() again on the SAME object, and the reused InitMft
  // reconfigures this instance in place (FLUSH + DRAIN + media-type
  // renegotiation at the new size) instead of creating a new MFT. Creating a
  // new MFT per resize was the accumulation: 2 simulcast rungs + 5 resizes x
  // 2 = 12 NVENC sessions = the GeForce 12-session cap -> the 6th re-anchor
  // failed -> OpenH264 fallback -> 0 RTP -> receiver freeze. ~MfH264EncoderImpl
  // performs the final teardown.
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MfH264EncoderImpl::Encode(
    const VideoFrame& input_frame,
    const std::vector<VideoFrameType>* frame_types) {
  if (!configured_.load()) {
    return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  }
  if (encode_failed_.load()) {
    return WEBRTC_VIDEO_CODEC_ENCODER_FAILURE;
  }
  EnsureComInitialized();

  webrtc::scoped_refptr<I420BufferInterface> frame_buffer =
      input_frame.video_frame_buffer()->ToI420();
  if (!frame_buffer) {
    RTC_LOG(LS_ERROR) << "MF H264 encoder: failed to convert frame to I420";
    return WEBRTC_VIDEO_CODEC_ENCODER_FAILURE;
  }

  if (frame_buffer->width() != width_ || frame_buffer->height() != height_) {
    // In-place resolution change (Chromium-style, NO track republish): the
    // encoder renegotiates at the new size and the receiver's decoder
    // handles the format change via the new SPS (MF_E_TRANSFORM_STREAM_CHANGE
    // path). Recreating the track on resize is what exhausted the hardware
    // encoder (~7 republishes -> MFT creation failure -> OpenH264 fallback
    // -> 0 RTP to the receiver).
    int32_t rc = ReconfigureMft(frame_buffer->width(), frame_buffer->height());
    if (rc != WEBRTC_VIDEO_CODEC_OK) {
      RTC_LOG(LS_ERROR) << "MF H264 encoder: in-place reconfiguration to "
                        << frame_buffer->width() << "x"
                        << frame_buffer->height() << " failed (" << rc
                        << "); dropping frame";
      return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    }
  }

  const bool send_key_frame =
      frame_types != nullptr && !frame_types->empty() &&
      (*frame_types)[0] == VideoFrameType::kVideoFrameKey;

  // Build a packed NV12 input sample at the exact visible size width_ x
  // height_ (odd allowed), exactly like Chromium's MFVEA: MFCreateAligned-
  // MemoryBuffer sized by the NV12 allocation, I420->NV12 via libyuv. The
  // MFT codes the 16-aligned picture internally with SPS frame_cropping, so
  // no manual padding is needed and receivers see no borders.
  MFT_INPUT_STREAM_INFO stream_info = {};
  mft_->GetInputStreamInfo(input_stream_id_, &stream_info);
  // NV12 allocation: Y plane = w*h, UV plane = uv_w * ceil(h/2) where the UV
  // rows are interleaved U/V pairs. libyuv's I420ToNV12 (MergeUVPlane) writes
  // (w+1)/2 pairs = w+1 bytes per UV row for an ODD w — allocating only
  // w bytes per row was a 1-byte OOB write per chroma row -> heap corruption
  // 0xC0000374 after a few resize cycles (A09/A10; page-heap fault in
  // FeedInputs touching the output_metadata_queue_'s block). Always use an
  // even UV stride (correct NV12 semantics; odd widths are padded).
  const size_t src_w = static_cast<size_t>(width_);
  const size_t src_h = static_cast<size_t>(height_);
  const size_t uv_w = (src_w + 1) & ~static_cast<size_t>(1);
  const DWORD payload_size = static_cast<DWORD>(src_w * src_h +
                                               uv_w * ((src_h + 1) / 2));
  const DWORD allocation_size =
      std::max(stream_info.cbSize, payload_size);
  Microsoft::WRL::ComPtr<IMFMediaBuffer> input_buffer;
  HRESULT hr = MFCreateAlignedMemoryBuffer(allocation_size, 0, &input_buffer);
  if (SUCCEEDED(hr)) {
    hr = input_buffer->SetCurrentLength(payload_size);
  }
  BYTE* data = nullptr;
  DWORD max_length = 0;
  DWORD current_length = 0;
  if (SUCCEEDED(hr)) {
    hr = input_buffer->Lock(&data, &max_length, &current_length);
  }
  if (SUCCEEDED(hr) && data) {
    // I420 -> packed NV12 (dst strides = width_, exactly Chromium's
    // ConvertAndScale layout for the CPU path).
    libyuv::I420ToNV12(
        frame_buffer->DataY(), frame_buffer->StrideY(),
        frame_buffer->DataU(), frame_buffer->StrideU(),
        frame_buffer->DataV(), frame_buffer->StrideV(),
        data, static_cast<int>(src_w),
        data + src_w * src_h, static_cast<int>(uv_w),
        static_cast<int>(src_w), static_cast<int>(src_h));
    input_buffer->Unlock();
  }

  Microsoft::WRL::ComPtr<IMFSample> sample;
  if (SUCCEEDED(hr)) {
    hr = MFCreateSample(&sample);
  }
  if (SUCCEEDED(hr)) {
    hr = sample->AddBuffer(input_buffer.Get());
  }
  if (SUCCEEDED(hr)) {
    // SetSampleTime in 100 ns units; convert from the 90 kHz RTP timestamp.
    LONGLONG sample_time =
        static_cast<LONGLONG>(input_frame.rtp_timestamp()) * 10000 / 90;
    hr = sample->SetSampleTime(sample_time);
  }
  if (SUCCEEDED(hr)) {
    UINT64 duration = 0;
    if (SUCCEEDED(MFFrameRateToAverageTimePerFrame(max_framerate_, 1,
                                                   &duration))) {
      sample->SetSampleDuration(duration);
    }
  }
  if (FAILED(hr)) {
    RTC_LOG(LS_ERROR) << "MF H264 encoder: sample construction failed " << hr;
    return WEBRTC_VIDEO_CODEC_ENCODER_FAILURE;
  }

  {
    std::lock_guard<std::mutex> lock(input_mutex_);
    pending_input_queue_.push_back(
        PendingInput{sample, input_frame.rtp_timestamp(), send_key_frame});
    pending_input_count_.fetch_add(1, std::memory_order_release);
  }
  // Wake the encoder thread: if the MFT is already waiting for input (a
  // NeedInput event was consumed with an empty queue), feed immediately
  // instead of waiting for a fresh event (Chromium's FeedInputs kick).
  async_state_->cv.notify_all();
  return WEBRTC_VIDEO_CODEC_OK;
}

void MfH264EncoderImpl::EncoderThreadMain() {
  EnsureComInitialized();
  for (;;) {
    MediaEventType event_type = MEUnknown;
    HRESULT status = S_OK;
    bool has_event = false;
    {
      std::unique_lock<std::mutex> lock(async_state_->mutex);
      async_state_->cv.wait(lock, [this] {
        return !async_state_->events.empty() || async_state_->stopped ||
               (need_input_counter_ > 0 &&
                pending_input_count_.load(std::memory_order_acquire) > 0);
      });
      if (async_state_->stopped) {
        break;
      }
      if (!async_state_->events.empty()) {
        event_type = async_state_->events.front().first;
        status = async_state_->events.front().second;
        async_state_->events.pop_front();
        has_event = true;
      }
    }
    if (has_event) {
      HandleEvent(event_type, status);
    } else if (need_input_counter_ > 0) {
      // Kick: the MFT signaled NeedInput earlier but the queue was empty;
      // feed the frame(s) queued since then.
      FeedInputs();
    }
  }
}

void MfH264EncoderImpl::HandleEvent(MediaEventType event_type,
                                    HRESULT status) {
  if (FAILED(status)) {
    RTC_LOG(LS_ERROR) << "MF H264 encoder: async event error " << status;
    encode_failed_.store(true);
    return;
  }
  switch (event_type) {
    case METransformNeedInput: {
      ++need_input_counter_;
      FeedInputs();
      break;
    }
    case METransformHaveOutput: {
      ProcessOutput();
      break;
    }
    case METransformDrainComplete: {
      // Only reachable from an explicit drain, which we never issue per
      // frame. Nothing to do.
      break;
    }
    case MEError: {
      RTC_LOG(LS_ERROR) << "MF H264 encoder: MEError from MFT";
      encode_failed_.store(true);
      return;
    }
    default: {
      break;
    }
  }
  // Re-arm the event request after handling (Chromium does the same). A
  // transient MF_E_INVALIDREQUEST (re-arm raced an in-flight EndGetEvent) is
  // retried once; a persistent failure is fatal so WebRTC tears down and
  // recreates the encoder.
  HRESULT hr = event_generator_->BeginGetEvent(async_callback_.Get(),
                                               event_generator_.Get());
  if (FAILED(hr)) {
    hr = event_generator_->BeginGetEvent(async_callback_.Get(),
                                         event_generator_.Get());
  }
  if (FAILED(hr)) {
    RTC_LOG(LS_ERROR) << "MF H264 encoder: BeginGetEvent re-arm failed " << hr;
    encode_failed_.store(true);
  }
}

void MfH264EncoderImpl::FeedInputs() {
  PendingInput input;
  {
    std::lock_guard<std::mutex> lock(input_mutex_);
    if (pending_input_queue_.empty()) {
      return;
    }
    input = pending_input_queue_.front();  // peek; popped only on success
  }
  if (input.keyframe && codec_api_) {
    VARIANT var;
    var.vt = VT_UI4;
    var.ulVal = 1;
    codec_api_->SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var);
  }
  HRESULT hr = mft_->ProcessInput(input_stream_id_, input.sample.Get(), 0);
  if (hr == MF_E_NOTACCEPTING) {
    // The MFT published NeedInput but rejected the sample (crbug.com/377749373
    // workaround); leave it queued for the next NeedInput event.
    return;
  }
  if (FAILED(hr)) {
    RTC_LOG(LS_ERROR) << "MF H264 encoder: ProcessInput failed " << hr;
    encode_failed_.store(true);
    return;
  }
  {
    std::lock_guard<std::mutex> lock(input_mutex_);
    pending_input_queue_.pop_front();
    output_metadata_queue_.push_back(
        OutputMetadata{input.rtp_timestamp, input.keyframe});
    pending_input_count_.fetch_sub(1, std::memory_order_release);
  }
  if (need_input_counter_ > 0) {
    --need_input_counter_;
  }
}

void MfH264EncoderImpl::ProcessOutput() {
  MFT_OUTPUT_DATA_BUFFER output_data_buffer = {};
  output_data_buffer.dwStreamID = output_stream_id_;
  if (mft_provides_output_samples_) {
    output_data_buffer.pSample = nullptr;  // the MFT provides the sample
  } else {
    // Caller-allocated output sample (reused across frames).
    output_data_buffer.pSample = output_sample_storage_.Get();
    output_buffer_storage_->SetCurrentLength(0);
  }
  DWORD status = 0;
  HRESULT hr = mft_->ProcessOutput(0, 1, &output_data_buffer, &status);
  if (output_data_buffer.pEvents != nullptr) {
    output_data_buffer.pEvents->Release();
  }
  if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
    // Renegotiate the output type (e.g. after a resolution change).
    Microsoft::WRL::ComPtr<IMFMediaType> media_type;
    hr = MF_E_NOT_FOUND;
    for (DWORD i = 0; i < 32; ++i) {
      hr = mft_->GetOutputAvailableType(output_stream_id_, i, &media_type);
      if (SUCCEEDED(hr)) {
        break;
      }
    }
    if (SUCCEEDED(hr)) {
      mft_->SetOutputType(output_stream_id_, media_type.Get(), 0);
    }
    // On the MFT-provides path the MFT may have allocated a sample even on
    // the stream-change result; release it so it does not leak.
    if (mft_provides_output_samples_ && output_data_buffer.pSample != nullptr) {
      output_data_buffer.pSample->Release();
    }
    return;
  }
  if (FAILED(hr)) {
    // Not fatal: e.g. MF_E_TRANSFORM_NEED_MORE_INPUT on a sync MFT.
    RTC_LOG(LS_WARNING) << "MF H264 encoder: ProcessOutput " << hr;
    if (mft_provides_output_samples_ && output_data_buffer.pSample != nullptr) {
      output_data_buffer.pSample->Release();
    }
    return;
  }

  Microsoft::WRL::ComPtr<IMFSample> output_sample;
  if (mft_provides_output_samples_) {
    // The MFT allocated the sample and handed it to us: take ownership and
    // release it when this scope ends.
    output_sample.Attach(output_data_buffer.pSample);
  } else {
    // Caller-allocated sample owned by output_sample_storage_: AddRef so the
    // local's Release() at scope end does NOT free the member's sample.
    // (Attach would steal ownership without AddRef and leave
    // output_sample_storage_ holding a dangling pointer -> use-after-free on
    // the next ProcessOutput, plus a double-Release at destruction.)
    output_sample = output_data_buffer.pSample;
  }
  Microsoft::WRL::ComPtr<IMFMediaBuffer> output_buffer;
  hr = output_sample->GetBufferByIndex(0, &output_buffer);
  if (FAILED(hr)) {
    return;
  }

  OutputMetadata metadata;
  {
    std::lock_guard<std::mutex> lock(input_mutex_);
    if (output_metadata_queue_.empty()) {
      return;
    }
    metadata = output_metadata_queue_.front();
    output_metadata_queue_.pop_front();
  }

  BYTE* data = nullptr;
  DWORD max_length = 0;
  DWORD current_length = 0;
  hr = output_buffer->Lock(&data, &max_length, &current_length);
  if (FAILED(hr) || !data || current_length == 0) {
    if (SUCCEEDED(hr)) {
      output_buffer->Unlock();
    }
    return;
  }

  // Copy the encoded bytes out of the MFT-owned buffer before unlocking.
  // The MFT emits Annex B (00 00 00 01 start codes); the RTP packetizer
  // accepts Annex B directly, no conversion needed.
  encoded_image_.SetEncodedData(
      EncodedImageBuffer::Create(data, current_length));
  output_buffer->Unlock();

  encoded_image_._encodedWidth = width_;
  encoded_image_._encodedHeight = height_;
  encoded_image_.SetRtpTimestamp(metadata.rtp_timestamp);
  encoded_image_.SetSimulcastIndex(0);
  encoded_image_.ntp_time_ms_ = 0;
  encoded_image_.capture_time_ms_ = 0;
  encoded_image_.rotation_ = kVideoRotation_0;
  encoded_image_.content_type_ = VideoContentType::UNSPECIFIED;
  encoded_image_.timing_.flags = VideoSendTiming::kInvalid;
  encoded_image_._frameType = metadata.keyframe
                                  ? VideoFrameType::kVideoFrameKey
                                  : VideoFrameType::kVideoFrameDelta;

  CodecSpecificInfo codec_info;
  codec_info.codecType = kVideoCodecH264;
  codec_info.codecSpecific.H264.packetization_mode =
      H264PacketizationMode::NonInterleaved;

  if (encoded_image_callback_) {
    const auto result =
        encoded_image_callback_->OnEncodedImage(encoded_image_, &codec_info);
    if (result.error != EncodedImageCallback::Result::OK) {
      RTC_LOG(LS_ERROR) << "MF H264 encoder: OnEncodedImage failed "
                        << result.error;
    }
  }
}

void MfH264EncoderImpl::SetRates(const RateControlParameters& parameters) {
  if (parameters.framerate_fps >= 1.0) {
    max_framerate_ = static_cast<int>(parameters.framerate_fps);
  }
  const uint32_t target_bps = parameters.bitrate.get_sum_bps();
  if (target_bps > 0) {
    target_bps_ = target_bps;
    if (codec_api_) {
      VARIANT var;
      var.vt = VT_UI4;
      var.ulVal = target_bps;
      // Best-effort; some MFTs reject mid-stream bitrate changes.
      codec_api_->SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var);
    }
  }
}

VideoEncoder::EncoderInfo MfH264EncoderImpl::GetEncoderInfo() const {
  EncoderInfo info;
  info.supports_native_handle = false;
  info.implementation_name = "MF H264 Encoder (hardware)";
  info.scaling_settings = VideoEncoder::ScalingSettings::kOff;
  info.is_hardware_accelerated = true;
  info.supports_simulcast = false;
  info.preferred_pixel_formats = {VideoFrameBuffer::Type::kI420};
  return info;
}

}  // namespace webrtc
