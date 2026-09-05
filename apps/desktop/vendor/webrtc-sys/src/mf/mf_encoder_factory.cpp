#include "mf_encoder_factory.h"

#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>

#include <map>
#include <memory>
#include <string>

#include "h264_encoder_impl.h"
#include "mf_common.h"
#include "rtc_base/logging.h"

namespace webrtc {

namespace {

// Probe for a Media Foundation H.264 encoder MFT. Must run after
// EnsureMediaFoundationStarted/EnsureComInitialized (MFTEnumEx is a COM
// API); IsSupported() below calls them first.
bool FindH264EncoderMftOnce(bool prefer_hardware_only) {
  EnsureMediaFoundationStarted();
  EnsureComInitialized();
  MFT_REGISTER_TYPE_INFO input_type = {MFMediaType_Video, MFVideoFormat_NV12};
  MFT_REGISTER_TYPE_INFO output_type = {MFMediaType_Video, MFVideoFormat_H264};
  DWORD flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT |
                MFT_ENUM_FLAG_SORTANDFILTER;
  if (prefer_hardware_only) {
    flags |= MFT_ENUM_FLAG_HARDWARE;
  }
  IMFActivate** activates = nullptr;
  UINT32 count = 0;
  HRESULT hr = MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, &input_type,
                         &output_type, &activates, &count);
  if (activates) {
    for (UINT32 i = 0; i < count; ++i) {
      activates[i]->Release();
    }
    CoTaskMemFree(activates);
  }
  const bool supported = SUCCEEDED(hr) && count > 0;
  return supported;
}

}  // namespace

MfVideoEncoderFactory::MfVideoEncoderFactory() {
  // Mirror the NVIDIA factory's baseline H.264 parameters (constrained
  // baseline). Advertise BOTH packetization modes: webrtc's IsSameCodec
  // compares packetization-mode strictly, and the SFU may answer with
  // packetization-mode=0 (the OpenH264/webrtc default), which would make the
  // hardware format unmatchable and silently fall back to software.
  for (const auto& pm : {"1", "0"}) {
    std::map<std::string, std::string> baseline_parameters = {
        {"profile-level-id", "42e01f"},
        {"level-asymmetry-allowed", "1"},
        {"packetization-mode", pm},
    };
    supported_formats_.push_back(SdpVideoFormat("H264", baseline_parameters));
  }
}

MfVideoEncoderFactory::~MfVideoEncoderFactory() {}

bool MfVideoEncoderFactory::IsSupported() {
  // Register only when a REAL hardware encoder MFT is present. Without a
  // hardware MFT the encoder factory simply doesn't register and the existing
  // OpenH264 path is used.
  return FindH264EncoderMftOnce(/*prefer_hardware_only=*/true);
}

std::unique_ptr<VideoEncoder> MfVideoEncoderFactory::Create(
    const Environment& /*env*/, const SdpVideoFormat& format) {
  for (const auto& supported_format : supported_formats_) {
    if (format.IsSameCodec(supported_format)) {
      RTC_LOG(LS_INFO) << "Using MF hardware encoder (H264)";
      return std::make_unique<MfH264EncoderImpl>(format);
    }
  }
  return nullptr;
}

std::vector<SdpVideoFormat> MfVideoEncoderFactory::GetSupportedFormats() const {
  return supported_formats_;
}

std::vector<SdpVideoFormat> MfVideoEncoderFactory::GetImplementations() const {
  return supported_formats_;
}

}  // namespace webrtc
