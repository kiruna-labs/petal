#ifndef WEBRTC_MF_H264_DECODER_IMPL_H_
#define WEBRTC_MF_H264_DECODER_IMPL_H_

// Windows Media Foundation H.264 hardware decoder for libwebrtc.
//
// Wraps the Media Foundation H.264 decoder MFT (hardware MFT preferred)
// behind the webrtc::VideoDecoder interface. Compiled only on Windows
// (USE_MF_VIDEO_CODEC in build.rs). See PETAL_PATCH.md for the rationale.

#include <mfobjects.h>
#include <mftransform.h>

#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_decoder.h"

namespace webrtc {

class MfH264DecoderImpl : public VideoDecoder {
 public:
  explicit MfH264DecoderImpl(const SdpVideoFormat& format);
  ~MfH264DecoderImpl() override;

  bool Configure(const Settings& settings) override;

  int32_t Decode(const EncodedImage& input_image,
                 bool missing_frames,
                 int64_t render_time_ms) override;

  int32_t RegisterDecodeCompleteCallback(
      DecodedImageCallback* callback) override;

  int32_t Release() override;

  DecoderInfo GetDecoderInfo() const override;

 private:
  // Find + create the MF H.264 decoder MFT and negotiate input/output types.
  // Returns true on success.
  bool CreateDecoder();

  // Convert an NV12 frame (as produced by the decoder MFT) into an I420
  // VideoFrame and deliver it through the decode callback.
  void DeliverDecodedFrame(const uint8_t* data, LONG stride, DWORD width,
                           DWORD height, const EncodedImage& input_image,
                           int64_t render_time_ms);

  const SdpVideoFormat format_;
  DecodedImageCallback* decoded_complete_callback_ = nullptr;

  bool hardware_mft_ = false;
  IMFTransform* mft_ = nullptr;
  // Async decoder MFTs signal output via media events even after
  // MF_TRANSFORM_ASYNC_UNLOCK; drive them event-driven (feed one frame, send
  // MFT_MESSAGE_COMMAND_DRAIN, consume METransformHaveOutput events).
  IMFMediaEventGenerator* event_generator_ = nullptr;
  bool mft_provides_output_samples_ = false;

  // Negotiated output layout (filled on the first stream-change renegotiation).
  DWORD output_width_ = 0;
  DWORD output_height_ = 0;
  LONG output_stride_ = 0;

  // SPS frame_cropping of the most recent access unit, in luma pixels. The
  // encoder MFT codes the 16-aligned picture and crops (frame_cropping) to
  // the visible size; the decoder delivers the coded frame, so the NV12->I420
  // copy in DeliverDecodedFrame must drop the crop margins or the receiver
  // shows the encoder's internal pad as a border.
  DWORD crop_left_ = 0;
  DWORD crop_right_ = 0;
  DWORD crop_top_ = 0;
  DWORD crop_bottom_ = 0;

  // Pull one decoded sample from the MFT (ProcessOutput + stream-change
  // renegotiation + delivery via DeliverDecodedFrame). Returns false when no
  // complete sample was available.
  bool FetchDecodedSample(const EncodedImage& input_image,
                          int64_t render_time_ms);
};

}  // namespace webrtc

#endif  // WEBRTC_MF_H264_DECODER_IMPL_H_
