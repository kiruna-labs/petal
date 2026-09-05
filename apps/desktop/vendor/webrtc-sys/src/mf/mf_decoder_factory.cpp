#include "mf_decoder_factory.h"

#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>

#include <map>
#include <memory>

#include "h264_decoder_impl.h"
#include "mf_common.h"
#include "rtc_base/logging.h"

namespace webrtc {

namespace {

// Probe for a Media Foundation H.264 decoder MFT. Must run after
// EnsureMediaFoundationStarted/EnsureComInitialized (MFTEnumEx is a COM
// API); IsSupported() below calls them first.
bool FindH264DecoderMftOnce(bool prefer_hardware_only) {
  EnsureMediaFoundationStarted();
  EnsureComInitialized();
  MFT_REGISTER_TYPE_INFO input_type = {MFMediaType_Video, MFVideoFormat_H264};
  MFT_REGISTER_TYPE_INFO output_type = {MFMediaType_Video, MFVideoFormat_NV12};
  DWORD flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT |
                MFT_ENUM_FLAG_SORTANDFILTER;
  if (prefer_hardware_only) {
    flags |= MFT_ENUM_FLAG_HARDWARE;
  }
  IMFActivate** activates = nullptr;
  UINT32 count = 0;
  HRESULT hr = MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, flags, &input_type,
                         &output_type, &activates, &count);
  if (activates) {
    for (UINT32 i = 0; i < count; ++i) {
      activates[i]->Release();
    }
    CoTaskMemFree(activates);
  }
  return SUCCEEDED(hr) && count > 0;
}

}  // namespace

MfVideoDecoderFactory::MfVideoDecoderFactory() {
  // H.264 baseline variants the stack negotiates (constrained baseline).
  // Browsers commonly offer 42e01f / 42001f; our own MF encoder emits 42c01f.
  // Include all three AND both packetization modes: webrtc's IsSameCodec
  // compares packetization-mode strictly, so a sender that negotiated
  // packetization-mode=0 must still match the hardware decoder.
  const std::vector<std::string> profile_level_ids = {"42e01f", "42001f",
                                                      "42c01f"};
  for (const auto& pli : profile_level_ids) {
    for (const auto& pm : {"1", "0"}) {
      std::map<std::string, std::string> baseline_parameters = {
          {"profile-level-id", pli},
          {"level-asymmetry-allowed", "1"},
          {"packetization-mode", pm},
      };
      supported_formats_.push_back(SdpVideoFormat("H264", baseline_parameters));
    }
  }
}

MfVideoDecoderFactory::~MfVideoDecoderFactory() {}

bool MfVideoDecoderFactory::IsSupported() {
  // Register only when a REAL hardware decoder MFT is present (see the
  // encoder factory's comment for the full rationale — same reasoning).
  // Without a hardware MFT the decoder factory doesn't register and the
  // built-in software decoder is used, exactly as before this patch.
  const bool supported = FindH264DecoderMftOnce(/*prefer_hardware_only=*/true);
  return supported;
}

std::unique_ptr<VideoDecoder> MfVideoDecoderFactory::Create(
    const Environment& /*env*/, const SdpVideoFormat& format) {
  for (const auto& supported_format : supported_formats_) {
    if (format.IsSameCodec(supported_format)) {
      RTC_LOG(LS_INFO) << "Using MF hardware decoder (H264)";
      return std::make_unique<MfH264DecoderImpl>(format);
    }
  }
  return nullptr;
}

std::vector<SdpVideoFormat> MfVideoDecoderFactory::GetSupportedFormats() const {
  return supported_formats_;
}

}  // namespace webrtc
