// PETAL patch: read the H.264 profile/level the MF encoder actually emitted.
//
// The MFT derives the LEVEL from the frame geometry and writes it into the SPS;
// nothing here sets or interprets MF_MT_MPEG2_LEVEL. Measured on an NVIDIA
// H.264 Encoder MFT this crate configures:
//
//   1280x720 @30  -> SPS 42 c0 1f  Level 3.1
//   1920x1080@30  -> SPS 42 c0 28  Level 4.0
//   1920x1080@60  -> SPS 42 c0 2a  Level 4.2
//   2560x1440@30  -> SPS 42 c0 32  Level 5.0
//   3840x2160@30  -> SPS 42 c0 33  Level 5.1
//   3840x2160@60  -> SPS 42 c0 34  Level 5.2
//
// Why scan the bitstream at all: the MFT's own media-type attributes are NOT
// evidence of what it encodes (measured: GetOutputAvailableType reports
// `profile=Main`, `frame=0x0`, and a 0xFFFFFFFF level sentinel). A hardware MFT
// that silently writes a different profile than the SDP advertises is the one
// failure nothing else can see, so the encoder reads the triple out of its own
// output and reports it once per distinct value.
//
// Pure by design (no logging, no WebRTC headers) so the mapping can be compiled
// and checked on its own.
#ifndef PETAL_MF_H264_SPS_H_
#define PETAL_MF_H264_SPS_H_

#include <cstdint>
#include <cstdio>
#include <string>

namespace petal_sps {

/// The profile/level an encoder actually wrote into its bitstream. Read from
/// the SPS (nal_unit_type 7), which is the ground truth a decoder sees.
struct SpsProfileLevel {
  bool found = false;
  uint8_t profile_idc = 0;
  uint8_t constraints = 0;
  uint8_t level_idc = 0;
};

/// Scan Annex-B bytes for the SPS and read its first three payload bytes:
/// profile_idc, constraint flags, level_idc. All three sit directly after the
/// NAL header, before any field that could carry an emulation-prevention byte,
/// so they can be read without a bit reader.
inline SpsProfileLevel FindSpsProfileLevel(const uint8_t* data, size_t size) {
  SpsProfileLevel out;
  if (data == nullptr) return out;
  for (size_t i = 0; i + 4 < size; ++i) {
    size_t nal = 0;
    if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1) {
      nal = i + 3;
    } else if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 &&
               data[i + 3] == 1) {
      nal = i + 4;
    } else {
      continue;
    }
    if (nal + 3 >= size) return out;
    if ((data[nal] & 0x1F) == 7) {
      out.found = true;
      out.profile_idc = data[nal + 1];
      out.constraints = data[nal + 2];
      out.level_idc = data[nal + 3];
      return out;
    }
    i = nal;  // keep scanning past this NAL header
  }
  return out;
}

/// "Baseline" / "Main" / "High" / "profile_idc N" -- for logs.
inline std::string ProfileNameForIdc(uint8_t profile_idc) {
  switch (profile_idc) {
    case 66: return "Baseline";
    case 77: return "Main";
    case 100: return "High";
    case 110: return "High 10";
    case 122: return "High 4:2:2";
    default: {
      char buf[32];
      std::snprintf(buf, sizeof(buf), "profile_idc %u",
                    static_cast<unsigned>(profile_idc));
      return buf;
    }
  }
}

/// "3.1" / "5.1" / "4.0" -- for logs. The suffix form the spec uses is the
/// decimal idc split in two (51 -> 5.1), and whole levels keep the ".0" because
/// that is how levels are spoken (40 is "4.0", never "4").
inline std::string LevelNameForIdc(uint8_t level_idc) {
  char buf[16];
  std::snprintf(buf, sizeof(buf), "%u.%u", static_cast<unsigned>(level_idc / 10),
                static_cast<unsigned>(level_idc % 10));
  return buf;
}

}  // namespace petal_sps

#endif  // PETAL_MF_H264_SPS_H_
