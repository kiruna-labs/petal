#ifndef WEBRTC_MF_H264_ENCODER_FACTORY_H_
#define WEBRTC_MF_H264_ENCODER_FACTORY_H_

#include <memory>
#include <vector>

#include "api/environment/environment.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_encoder.h"
#include "api/video_codecs/video_encoder_factory.h"

namespace webrtc {

// VideoEncoderFactory that creates MfH264EncoderImpl instances. Registered
// under the Hardware backend only when a real hardware H.264 encoder MFT is
// present, so the OpenH264 software fallback is used otherwise.
class MfVideoEncoderFactory : public VideoEncoderFactory {
 public:
  MfVideoEncoderFactory();
  ~MfVideoEncoderFactory() override;

  // True when a Media Foundation H.264 encoder MFT exists on this host.
  static bool IsSupported();

  std::unique_ptr<VideoEncoder> Create(const Environment& env,
                                       const SdpVideoFormat& format) override;

  // Returns a list of supported codecs in order of preference.
  std::vector<SdpVideoFormat> GetSupportedFormats() const override;

  std::vector<SdpVideoFormat> GetImplementations() const override;

  std::unique_ptr<EncoderSelectorInterface> GetEncoderSelector()
      const override {
    return nullptr;
  }

 private:
  std::vector<SdpVideoFormat> supported_formats_;
};

}  // namespace webrtc

#endif  // WEBRTC_MF_H264_ENCODER_FACTORY_H_
