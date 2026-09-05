#ifndef WEBRTC_MF_VIDEO_DECODER_FACTORY_H_
#define WEBRTC_MF_VIDEO_DECODER_FACTORY_H_

// Media Foundation H.264 decoder factory (Windows-only, USE_MF_VIDEO_CODEC).
// Mirrors the shape of NvidiaVideoDecoderFactory.

#include <vector>

#include "api/environment/environment.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_decoder_factory.h"

namespace webrtc {

class MfVideoDecoderFactory : public VideoDecoderFactory {
 public:
  MfVideoDecoderFactory();
  ~MfVideoDecoderFactory() override;

  // True when a Media Foundation H.264 decoder MFT exists on this host.
  static bool IsSupported();

  std::unique_ptr<VideoDecoder> Create(const Environment& env,
                                       const SdpVideoFormat& format) override;

  std::vector<SdpVideoFormat> GetSupportedFormats() const override;

 private:
  std::vector<SdpVideoFormat> supported_formats_;
};

}  // namespace webrtc

#endif  // WEBRTC_MF_VIDEO_DECODER_FACTORY_H_
