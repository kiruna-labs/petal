// Windows Media Foundation H.264 hardware decoder (webrtc::VideoDecoder).
// Windows-only glue compiled via USE_MF_VIDEO_CODEC (build.rs). See
// PETAL_PATCH.md for rationale; structure mirrors src/nvidia/h264_decoder_impl.cpp.

#include "h264_decoder_impl.h"

#include <windows.h>
#include <codecapi.h>
#include <strmif.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>

#include <algorithm>
#include <mutex>

#include "api/scoped_refptr.h"
#include "api/video/i420_buffer.h"
#include "api/video/video_frame.h"
#include "api/video_codecs/video_codec.h"
#include "mf_common.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "rtc_base/logging.h"

namespace webrtc {

namespace {

// Minimal H.264 SPS bit reader + frame_cropping parser (H.264 7.3.2.1.1).
// Mirrors what Chromium's H264 SpsParser does: the encoder MFT codes the
// 16-aligned picture and writes frame_cropping for the visible region; the
// decoder delivers the coded frame, so the receiver must crop. Returns
// margins in luma pixels; valid=false when the AU has no SPS or no crop.
struct SpsCrop {
  DWORD left = 0;
  DWORD right = 0;
  DWORD top = 0;
  DWORD bottom = 0;
  bool valid = false;
};

class SpsBitReader {
 public:
  SpsBitReader(const uint8_t* data, size_t size) : data_(data), size_(size) {}

  bool bit() {
    if (bit_pos_ >= size_ * 8) {
      return false;
    }
    bool value = (data_[bit_pos_ / 8] >> (7 - (bit_pos_ % 8))) & 1;
    ++bit_pos_;
    return value;
  }

  uint32_t ue() {
    uint32_t zeros = 0;
    while (!bit()) {
      ++zeros;
    }
    if (zeros > 31) {
      return 0;
    }
    uint32_t value = 1;
    for (uint32_t i = 0; i < zeros; ++i) {
      value = (value << 1) | (bit() ? 1 : 0);
    }
    return value - 1;
  }

  int32_t se() {
    uint32_t code = ue();
    return (code & 1) ? static_cast<int32_t>((code + 1) / 2)
                      : -static_cast<int32_t>(code / 2);
  }

 private:
  const uint8_t* data_;
  size_t size_;
  size_t bit_pos_ = 0;
};

SpsCrop ParseSpsCrop(const uint8_t* data, size_t size) {
  SpsCrop crop;
  if (!data || size < 8) {
    return crop;
  }
  // Locate the SPS NAL (type 7) inside the Annex B access unit.
  const uint8_t* sps = nullptr;
  size_t sps_size = 0;
  size_t i = 0;
  while (i + 4 <= size) {
    size_t start = 0;
    if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1) {
      start = i + 3;
    } else if (i + 5 <= size && data[i] == 0 && data[i + 1] == 0 &&
               data[i + 2] == 0 && data[i + 3] == 1) {
      start = i + 4;
    } else {
      ++i;
      continue;
    }
    if ((data[start] & 0x1F) == 7) {
      size_t j = start + 1;
      while (j + 4 <= size) {
        if (data[j] == 0 && data[j + 1] == 0 &&
            (data[j + 2] == 1 ||
             (j + 3 <= size && data[j + 2] == 0 && data[j + 3] == 1))) {
          break;
        }
        ++j;
      }
      sps = data + start + 1;  // skip the NAL header byte
      sps_size = j - start - 1;
      break;
    }
    i = start;
  }
  if (!sps || sps_size < 4) {
    return crop;
  }

  SpsBitReader r(sps, sps_size);
  // profile_idc / constraint flags / level_idc are three whole bytes.
  const uint8_t profile_idc = sps[0];
  for (int b = 0; b < 24; ++b) {
    r.bit();
  }
  r.ue();  // seq_parameter_set_id
  if (profile_idc == 100 || profile_idc == 110 || profile_idc == 122 ||
      profile_idc == 244 || profile_idc == 44 || profile_idc == 83 ||
      profile_idc == 86 || profile_idc == 118 || profile_idc == 128 ||
      profile_idc == 138 || profile_idc == 139 || profile_idc == 134 ||
      profile_idc == 135) {
    const uint32_t chroma_format_idc = r.ue();
    if (chroma_format_idc == 3) {
      r.bit();  // separate_colour_plane_flag
    }
    r.ue();  // bit_depth_luma_minus8
    r.ue();  // bit_depth_chroma_minus8
    r.bit();  // qpprime_y_zero_transform_bypass_flag
    if (r.bit()) {  // seq_scaling_matrix_present_flag
      const uint32_t count = (chroma_format_idc != 3) ? 8 : 12;
      for (uint32_t k = 0; k < count; ++k) {
        if (r.bit()) {  // seq_scaling_list_present_flag
          int32_t last_scale = 8;
          int32_t next_scale = 8;
          const int32_t size_of_scaling_list = (k < 6) ? 16 : 64;
          for (int32_t j = 0; j < size_of_scaling_list; ++j) {
            if (next_scale != 0) {
              next_scale = (last_scale + r.se() + 256) % 256;
            }
            last_scale = (next_scale == 0) ? last_scale : next_scale;
          }
        }
      }
    }
  }
  r.ue();  // log2_max_frame_num_minus4
  const uint32_t poc_type = r.ue();
  if (poc_type == 0) {
    r.ue();  // log2_max_pic_order_cnt_lsb_minus4
  } else if (poc_type == 1) {
    r.bit();  // delta_pic_order_always_zero_flag
    r.se();  // offset_for_non_ref_pic
    r.se();  // offset_for_top_to_bottom_field
    const uint32_t num_ref = r.ue();
    for (uint32_t k = 0; k < num_ref; ++k) {
      r.se();  // offset_for_ref_frame
    }
  }
  r.ue();  // max_num_ref_frames
  r.bit();  // gaps_in_frame_num_value_allowed_flag
  r.ue();  // pic_width_in_mbs_minus1
  r.ue();  // pic_height_in_map_units_minus1
  const uint32_t frame_mbs_only = r.bit();
  if (!frame_mbs_only) {
    r.bit();  // mb_adaptive_frame_field_flag
  }
  r.bit();  // direct_8x8_inference_flag
  if (r.bit()) {  // frame_cropping_flag
    // Crop units are chroma samples for 4:2:0 (2 luma px each).
    crop.left = r.ue() * 2;
    crop.right = r.ue() * 2;
    crop.top = r.ue() * 2;
    crop.bottom = r.ue() * 2;
    crop.valid = true;
  }
  return crop;
}

// Find a Media Foundation H.264 video decoder MFT. Prefers a hardware MFT.
// Returns the CLSID or GUID_NULL when no decoder exists at all.
CLSID FindH264DecoderMft(bool* hardware) {
  EnsureMediaFoundationStarted();
  EnsureComInitialized();

  MFT_REGISTER_TYPE_INFO input_type = {MFMediaType_Video, MFVideoFormat_H264};
  MFT_REGISTER_TYPE_INFO output_type = {MFMediaType_Video, MFVideoFormat_NV12};

  for (DWORD flags :
       {MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT |
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT |
            MFT_ENUM_FLAG_SORTANDFILTER}) {
    IMFActivate** activates = nullptr;
    UINT32 count = 0;
    HRESULT hr = MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, flags, &input_type,
                           &output_type, &activates, &count);
    if (FAILED(hr) || count == 0) {
      if (activates) {
        CoTaskMemFree(activates);
      }
      continue;
    }
    CLSID clsid = GUID_NULL;
    for (UINT32 i = 0; i < count; ++i) {
      if (SUCCEEDED(activates[i]->GetGUID(MFT_TRANSFORM_CLSID_Attribute,
                                          &clsid))) {
        break;
      }
      clsid = GUID_NULL;
    }
    for (UINT32 i = 0; i < count; ++i) {
      activates[i]->Release();
    }
    CoTaskMemFree(activates);
    if (clsid != GUID_NULL) {
      if (hardware) {
        *hardware = (flags & MFT_ENUM_FLAG_HARDWARE) != 0;
      }
      return clsid;
    }
  }
  return GUID_NULL;
}

}  // namespace

MfH264DecoderImpl::MfH264DecoderImpl(const SdpVideoFormat& format)
    : format_(format) {}

MfH264DecoderImpl::~MfH264DecoderImpl() {
  Release();
}

VideoDecoder::DecoderInfo MfH264DecoderImpl::GetDecoderInfo() const {
  VideoDecoder::DecoderInfo info;
  info.implementation_name =
      hardware_mft_ ? "MF H264 Decoder (hardware)" : "MF H264 Decoder";
  info.is_hardware_accelerated = hardware_mft_;
  return info;
}

bool MfH264DecoderImpl::Configure(const Settings& settings) {
  if (settings.codec_type() != kVideoCodecH264) {
    RTC_LOG(LS_ERROR) << "MF H264 decoder: Configure called for non-H264 codec";
    return false;
  }
  return CreateDecoder();
}

bool MfH264DecoderImpl::CreateDecoder() {
  EnsureMediaFoundationStarted();
  EnsureComInitialized();

  const CLSID clsid = FindH264DecoderMft(&hardware_mft_);
  if (clsid == GUID_NULL) {
    RTC_LOG(LS_ERROR)
        << "MF H264 decoder: no Media Foundation H.264 decoder MFT found";
    return false;
  }

  IMFTransform* mft = nullptr;
  HRESULT hr = CoCreateInstance(clsid, nullptr, CLSCTX_INPROC_SERVER,
                                IID_PPV_ARGS(&mft));
  if (FAILED(hr)) {
    RTC_LOG(LS_ERROR) << "MF H264 decoder: CoCreateInstance failed (0x"
                      << std::hex << hr << std::dec << ")";
    return false;
  }
  // Hardware H.264 decoder MFTs may be asynchronous; unlock for sync use or
  // SetInputType fails with MF_E_TRANSFORM_ASYNC_LOCKED.
  UnlockAsyncTransform(mft);
  IMFMediaEventGenerator* event_generator = nullptr;
  const HRESULT hr_qeg = mft->QueryInterface(IID_PPV_ARGS(&event_generator));

  // CRITICAL: the MS H.264 decoder MFT (Msmpeg2vdec.dll) loops forever on
  // MF_E_TRANSFORM_STREAM_CHANGE in its default mode. Setting
  // CODECAPI_AVLowLatencyMode=1 makes it signal the format change correctly
  // (MF_E_TRANSFORM_NEED_MORE_INPUT + MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE)
  // and then decode. Established empirically with an MFT probe; without this
  // the first ProcessOutput never completes and frames_decoded stays 0.
  {
    ICodecAPI* codec_api = nullptr;
    if (SUCCEEDED(mft->QueryInterface(IID_PPV_ARGS(&codec_api)))) {
      VARIANT var = {};
      var.vt = VT_UI4;
      var.ulVal = 1;
      HRESULT hr_ll = codec_api->SetValue(&CODECAPI_AVLowLatencyMode, &var);
      codec_api->Release();
    }
  }

  // Input type: H.264 (Annex B; the decoder MFT scans the byte stream for
  // SPS/PPS). Deliberately no frame size — the decoder derives it from SPS
  // and signals the output size via a stream change on the first
  // ProcessOutput.
  IMFMediaType* input_type = nullptr;
  hr = MFCreateMediaType(&input_type);
  if (SUCCEEDED(hr)) {
    hr = input_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
  }
  if (SUCCEEDED(hr)) {
    hr = input_type->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_H264);
  }
  if (SUCCEEDED(hr)) {
    hr = mft->SetInputType(0, input_type, 0);
  }
  if (input_type) {
    input_type->Release();
  }
  if (FAILED(hr)) {
    RTC_LOG(LS_ERROR) << "MF H264 decoder: input type setup failed (0x"
                      << std::hex << hr << std::dec << ")";
    event_generator->Release();
    mft->Release();
    return false;
  }

  // Output type: NV12.
  IMFMediaType* output_type = nullptr;
  bool output_set = false;
  for (DWORD i = 0;; ++i) {
    IMFMediaType* type = nullptr;
    hr = mft->GetOutputAvailableType(0, i, &type);
    if (hr == MF_E_NO_MORE_TYPES) {
      break;
    }
    if (FAILED(hr)) {
      continue;
    }
    GUID subtype = GUID_NULL;
    type->GetGUID(MF_MT_SUBTYPE, &subtype);
    if (subtype == MFVideoFormat_NV12) {
      hr = mft->SetOutputType(0, type, 0);
      output_set = SUCCEEDED(hr);
      type->Release();
      if (output_set) {
        break;
      }
    } else {
      type->Release();
    }
  }
  if (!output_set) {
    RTC_LOG(LS_ERROR) << "MF H264 decoder: no NV12 output type";
    event_generator->Release();
    mft->Release();
    return false;
  }

  // Output stream info: hardware decoder MFTs may provide their own output
  // samples (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES), in which case ProcessOutput
  // must be called with pSample = NULL.
  {
    MFT_OUTPUT_STREAM_INFO out_info = {};
    if (SUCCEEDED(mft->GetOutputStreamInfo(0, &out_info))) {
      mft_provides_output_samples_ =
          (out_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES) != 0;
    }
  }

  // Streaming messages are required even for the unlocked async MFT (an MFT
  // probe established that ProcessInput fails without them).
  mft->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
  mft->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

  mft_ = mft;
  event_generator_ = event_generator;
  RTC_LOG(LS_INFO) << "MF H264 decoder initialized"
                   << (hardware_mft_ ? " (hardware MFT)" : " (software MFT)");
  return true;
}

int32_t MfH264DecoderImpl::RegisterDecodeCompleteCallback(
    DecodedImageCallback* callback) {
  decoded_complete_callback_ = callback;
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MfH264DecoderImpl::Release() {
  if (mft_) {
    mft_->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
    mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
    mft_->Release();
    mft_ = nullptr;
  }
  if (event_generator_) {
    event_generator_->Release();
    event_generator_ = nullptr;
  }
  output_width_ = output_height_ = 0;
  output_stride_ = 0;
  return WEBRTC_VIDEO_CODEC_OK;
}

int32_t MfH264DecoderImpl::Decode(const EncodedImage& input_image,
                                  bool /*missing_frames*/,
                                  int64_t render_time_ms) {
  if (!mft_) {
    RTC_LOG(LS_ERROR) << "MF H264 decoder: Decode before Configure";
    return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  }
  if (!decoded_complete_callback_) {
    RTC_LOG(LS_ERROR) << "MF H264 decoder: no decode callback registered";
    return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  }
  if (!input_image.data() || input_image.size() == 0) {
    return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
  }

  // Track the SPS frame_cropping of the current access unit so
  // DeliverDecodedFrame can drop the encoder's internal pad margins. The SPS
  // appears only in keyframes; P-frame AUs carry none, so the last parsed
  // crop is CACHED (only updated on a valid parse) — otherwise the delivered
  // size would flip between coded and visible every GOP.
  const SpsCrop crop = ParseSpsCrop(input_image.data(), input_image.size());
  if (crop.valid) {
    crop_left_ = crop.left;
    crop_right_ = crop.right;
    crop_top_ = crop.top;
    crop_bottom_ = crop.bottom;
  }

  EnsureComInitialized();

  // Feed the Annex B data directly. The MS H.264 decoder (MFVideoFormat_H264)
  // requires Annex B with start codes and scans the byte stream for SPS/PPS
  // (MSDN); converting to AVCC breaks that scan and causes the perpetual
  // stream-change loop.
  const uint8_t* data_ptr = input_image.data();
  const size_t data_len = input_image.size();

  IMFMediaBuffer* input_buffer = nullptr;
  HRESULT hr = MFCreateMemoryBuffer(
      static_cast<DWORD>(data_len + 1024), &input_buffer);
  if (SUCCEEDED(hr)) {
    BYTE* data = nullptr;
    hr = input_buffer->Lock(&data, nullptr, nullptr);
    if (SUCCEEDED(hr)) {
      memcpy(data, data_ptr, data_len);
      input_buffer->Unlock();
    }
  }
  if (SUCCEEDED(hr)) {
    hr = input_buffer->SetCurrentLength(static_cast<DWORD>(data_len));
  }
  IMFSample* input_sample = nullptr;
  if (SUCCEEDED(hr)) {
    hr = MFCreateSample(&input_sample);
  }
  if (SUCCEEDED(hr)) {
    hr = input_sample->AddBuffer(input_buffer);
  }
  if (SUCCEEDED(hr)) {
    input_sample->SetSampleTime(render_time_ms * 10000);
  }
  if (SUCCEEDED(hr)) {
    hr = mft_->ProcessInput(0, input_sample, 0);
  }
  if (input_buffer) {
    input_buffer->Release();
  }
  if (input_sample) {
    input_sample->Release();
  }
  if (FAILED(hr)) {
    if (hr != MF_E_NOTACCEPTING) {
      RTC_LOG(LS_WARNING) << "MF H264 decoder: ProcessInput failed (0x"
                          << std::hex << hr << std::dec << ")";
      return WEBRTC_VIDEO_CODEC_ERROR;
    }
    // MF_E_NOTACCEPTING: the decoder is busy draining prior output. The
    // frame is dropped; the next keyframe re-syncs the receiver.
    return WEBRTC_VIDEO_CODEC_OK;
  }

  // The MS H.264 decoder MFT is a SYNC decoder — feed one access unit and pull
  // the decoded frame with ProcessOutput. Do NOT send MFT_MESSAGE_COMMAND_DRAIN
  // per frame: that is the async ENCODER's contract, and draining the sync
  // decoder after every input resets its reference-frame state so P-frames
  // return MF_E_TRANSFORM_STREAM_CHANGE and only keyframes decode (the window
  // then only updates when the sender resizes). START_OF_STREAM is re-armed
  // after a stream-change renegotiation so the decoder stays in streaming mode.
  mft_->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

  if (event_generator_) {
    for (int pump = 0; pump < 16; ++pump) {
      IMFMediaEvent* event = nullptr;
      HRESULT hr_ev = event_generator_->GetEvent(MF_EVENT_FLAG_NO_WAIT,
                                                 &event);
      if (FAILED(hr_ev)) {
        break;  // No more pending events.
      }
      MediaEventType type = MEUnknown;
      event->GetType(&type);
      event->Release();
      if (type == METransformHaveOutput) {
        if (!FetchDecodedSample(input_image, render_time_ms)) {
          break;
        }
      } else if (type == METransformDrainComplete) {
        break;
      }
    }
  } else {
    FetchDecodedSample(input_image, render_time_ms);
  }

  return WEBRTC_VIDEO_CODEC_OK;
}

bool MfH264DecoderImpl::FetchDecodedSample(const EncodedImage& input_image,
                                           int64_t render_time_ms) {
  MFT_OUTPUT_DATA_BUFFER out = {};
  out.dwStreamID = 0;
  if (!mft_provides_output_samples_) {
    HRESULT hr_sample = MFCreateSample(&out.pSample);
    if (FAILED(hr_sample)) {
      RTC_LOG(LS_ERROR) << "MF H264 decoder: MFCreateSample failed (0x"
                        << std::hex << hr_sample << std::dec << ")";
      return false;
    }
  }
  IMFMediaBuffer* out_buffer = nullptr;
  DWORD out_size = std::max<DWORD>(output_width_ * output_height_ * 3 / 2,
                                   1 << 16);
  bool got_output = false;
  HRESULT hr_out = S_OK;
  for (int attempt = 0; attempt < 8; ++attempt) {
    if (!mft_provides_output_samples_) {
      if (out_buffer) {
        out_buffer->Release();
        out_buffer = nullptr;
      }
      hr_out = MFCreateMemoryBuffer(out_size, &out_buffer);
      if (FAILED(hr_out)) {
        RTC_LOG(LS_ERROR) << "MF H264 decoder: MFCreateMemoryBuffer failed (0x"
                          << std::hex << hr_out << std::dec << ")";
        break;
      }
      out_buffer->SetCurrentLength(0);
      out.pSample->RemoveAllBuffers();
      out.pSample->AddBuffer(out_buffer);
    }
    DWORD status = 0;
    hr_out = mft_->ProcessOutput(0, 1, &out, &status);
    if (hr_out == MF_E_TRANSFORM_STREAM_CHANGE ||
        hr_out == MF_E_TRANSFORM_TYPE_NOT_SET ||
        (hr_out == MF_E_TRANSFORM_NEED_MORE_INPUT &&
         (status & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE) != 0)) {
      // Output format changed (SPS-derived resolution). Take the updated type
      // from GetOutputAvailableType(0,0) (the decoder wants the 16-aligned
      // size, e.g. 1920x1088, and the output buffer must fit it). The MS
      // decoder signals the change as MF_E_TRANSFORM_TYPE_NOT_SET +
      // MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE (probe-verified 2026-08-06); the
      // triggering input is NOT consumed by the change, so we REISSUE
      // ProcessOutput below (continue) instead of returning — a plain
      // "return false" drops the still-pending input and the change never
      // converges (B10 wedge: frames_decoded stuck while bytes keep flowing).
      IMFMediaType* type = nullptr;
      if (SUCCEEDED(mft_->GetOutputAvailableType(0, 0, &type))) {
        UINT32 w = 0;
        UINT32 h = 0;
        MFGetAttributeSize(type, MF_MT_FRAME_SIZE, &w, &h);
        output_width_ = w;
        output_height_ = h;
        output_stride_ = static_cast<LONG>(
            MFGetAttributeUINT32(type, MF_MT_DEFAULT_STRIDE, 0));
        if (output_stride_ <= 0) {
          output_stride_ =
              static_cast<LONG>((output_width_ + 31) & ~31u);
        }
        // Size the output buffer for the MFT's padded row layout, not just
        // the visible frame: rows are stride-padded, so stride*height*3/2
        // bytes are written when stride > width.
        out_size = std::max<DWORD>(
            std::max<DWORD>(w * h * 3 / 2, 1 << 16),
            static_cast<DWORD>(output_stride_) * h * 3 / 2);
        if (SUCCEEDED(mft_->SetOutputType(0, type, 0))) {
          // Renegotiated; the reissue below pulls the pending frame.
        }
        type->Release();
      }
      // Keep out.pSample alive on the caller-allocated path (the next attempt
      // reuses it via RemoveAllBuffers + AddBuffer); on the MFT-provides path
      // the reissue must pass pSample=NULL so the MFT allocates a fresh one.
      if (mft_provides_output_samples_ && out.pSample) {
        out.pSample->Release();
        out.pSample = nullptr;
      }
      if (out_buffer) {
        out_buffer->Release();
        out_buffer = nullptr;
      }
      continue;  // REISSUE ProcessOutput with the new type. For a raw
                 // MF_E_TRANSFORM_STREAM_CHANGE the MFT consumed the input,
                 // so the reissue returns NEED_MORE_INPUT -> got_output stays
                 // false and the caller feeds the next AU (correct either way).
    }
    if (hr_out == MF_E_BUFFERTOOSMALL) {
      out_size *= 2;
      continue;
    }
    if (hr_out == S_OK && (status & MFT_OUTPUT_DATA_BUFFER_INCOMPLETE)) {
      // The MFT produced only a partial sample (it needs more input to
      // complete it) — not a complete frame; stop pulling for this input.
      hr_out = MF_E_TRANSFORM_NEED_MORE_INPUT;
      break;
    }
    if (hr_out == S_OK) {
      got_output = true;
    }
    break;
  }
  if (out_buffer) {
    out_buffer->Release();
  }
  if (!got_output || !out.pSample) {
    if (out.pSample) {
      out.pSample->Release();
    }
    if (hr_out != S_OK && hr_out != MF_E_TRANSFORM_NEED_MORE_INPUT &&
        hr_out != S_FALSE) {
      RTC_LOG(LS_WARNING) << "MF H264 decoder: ProcessOutput ended (0x"
                          << std::hex << hr_out << std::dec << ")";
    }
    return false;
  }

  IMFMediaBuffer* decoded_buffer = nullptr;
  HRESULT hr_get = out.pSample->GetBufferByIndex(0, &decoded_buffer);
  if (SUCCEEDED(hr_get)) {
    BYTE* data = nullptr;
    DWORD max_len = 0;
    DWORD cur_len = 0;
    hr_get = decoded_buffer->Lock(&data, &max_len, &cur_len);
    if (SUCCEEDED(hr_get)) {
      DeliverDecodedFrame(data, output_stride_, output_width_, output_height_,
                          input_image, render_time_ms);
      decoded_buffer->Unlock();
    }
    decoded_buffer->Release();
  }
  out.pSample->Release();
  return SUCCEEDED(hr_get);
}

void MfH264DecoderImpl::DeliverDecodedFrame(
    const uint8_t* data, LONG stride, DWORD width, DWORD height,
    const EncodedImage& input_image, int64_t render_time_ms) {
  if (width == 0 || height == 0) {
    return;
  }
  // Drop the SPS-cropped margins: the decoder delivers the coded picture
  // (16-aligned), and the encoder's internal pad outside the visible region
  // would render as a border. Clamp against malformed crops.
  DWORD x0 = crop_left_;
  DWORD y0 = crop_top_;
  DWORD vw = width - (crop_left_ + crop_right_);
  DWORD vh = height - (crop_top_ + crop_bottom_);
  if (crop_left_ + crop_right_ >= width || crop_top_ + crop_bottom_ >= height) {
    x0 = 0;
    y0 = 0;
    vw = width;
    vh = height;
  }
  webrtc::scoped_refptr<I420Buffer> i420 = I420Buffer::Create(vw, vh);
  const uint8_t* y = data;
  const uint8_t* uv = data + stride * height;
  const size_t dst_chroma_width = (vw + 1) / 2;
  const size_t dst_chroma_height = (vh + 1) / 2;
  // Source chroma offsets (NV12 chroma is 2:1 subsampled in both axes).
  const size_t src_chroma_x = x0 / 2;
  const size_t src_chroma_y = y0 / 2;
  uint8_t* dst_y = i420->MutableDataY();
  uint8_t* dst_u = i420->MutableDataU();
  uint8_t* dst_v = i420->MutableDataV();
  for (size_t row = 0; row < vh; ++row) {
    memcpy(dst_y + row * vw, y + (y0 + row) * stride + x0, vw);
  }
  for (size_t row = 0; row < dst_chroma_height; ++row) {
    const uint8_t* src = uv + (src_chroma_y + row) * stride + src_chroma_x * 2;
    for (size_t i = 0; i < dst_chroma_width; ++i) {
      dst_u[row * dst_chroma_width + i] = src[2 * i];
      dst_v[row * dst_chroma_width + i] = src[2 * i + 1];
    }
  }

  VideoFrame frame = VideoFrame::Builder()
                         .set_video_frame_buffer(i420)
                         .set_timestamp_rtp(input_image.RtpTimestamp())
                         .set_timestamp_ms(render_time_ms)
                         .set_rotation(webrtc::kVideoRotation_0)
                         .build();
  decoded_complete_callback_->Decoded(frame);
}

}  // namespace webrtc
