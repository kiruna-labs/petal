//! FLV packaging for client mode: H.264 access units to AVCC NAL units, AVC
//! and AAC sequence headers (re-sent whenever the encoder's SPS/PPS change),
//! monotonic millisecond timestamps, and audio/video interleaving.
//!
//! Encoder-agnostic: Media Foundation emits Annex B, VideoToolbox emits AVCC
//! plus a format description; both reduce to a list of NAL units here.

use std::collections::VecDeque;

use shiguredo_rtmp::{
    AudioFormat, AudioFrame, AvcPacketType, AvcSequenceHeader, RtmpTimestamp, RtmpTimestampDelta,
    VideoCodec, VideoFrame, VideoFrameType,
};

/// AudioSpecificConfig for AAC-LC, 48 kHz, stereo: object type 2, frequency
/// index 3, channel configuration 2.
pub const AAC_LC_48K_STEREO_ASC: [u8; 2] = [0x11, 0x90];

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;
const NAL_IDR: u8 = 5;

/// One packet on its way to the RTMP connection, timestamped in stream
/// milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPacket {
    Video {
        ts_ms: u32,
        keyframe: bool,
        sequence_header: bool,
        data: Vec<u8>,
    },
    Audio {
        ts_ms: u32,
        sequence_header: bool,
        data: Vec<u8>,
    },
}

impl MediaPacket {
    pub fn ts_ms(&self) -> u32 {
        match self {
            Self::Video { ts_ms, .. } | Self::Audio { ts_ms, .. } => *ts_ms,
        }
    }

    pub fn is_video(&self) -> bool {
        matches!(self, Self::Video { .. })
    }

    pub fn is_sequence_header(&self) -> bool {
        match self {
            Self::Video { sequence_header, .. } | Self::Audio { sequence_header, .. } => {
                *sequence_header
            }
        }
    }

    pub fn is_video_keyframe(&self) -> bool {
        matches!(self, Self::Video { keyframe: true, sequence_header: false, .. })
    }

    pub fn with_ts(mut self, new_ts: u32) -> Self {
        match &mut self {
            Self::Video { ts_ms, .. } | Self::Audio { ts_ms, .. } => *ts_ms = new_ts,
        }
        self
    }

    pub fn into_video_frame(self) -> Option<VideoFrame> {
        let Self::Video { ts_ms, keyframe, sequence_header, data } = self else {
            return None;
        };
        Some(VideoFrame {
            timestamp: RtmpTimestamp::from_millis(ts_ms),
            composition_timestamp_offset: RtmpTimestampDelta::ZERO,
            frame_type: if keyframe || sequence_header {
                VideoFrameType::KeyFrame
            } else {
                VideoFrameType::InterFrame
            },
            codec: VideoCodec::Avc,
            avc_packet_type: Some(if sequence_header {
                AvcPacketType::SequenceHeader
            } else {
                AvcPacketType::NalUnit
            }),
            data,
        })
    }

    pub fn into_audio_frame(self) -> Option<AudioFrame> {
        let Self::Audio { ts_ms, sequence_header, data } = self else {
            return None;
        };
        Some(AudioFrame {
            timestamp: RtmpTimestamp::from_millis(ts_ms),
            format: AudioFormat::Aac,
            sample_rate: AudioFrame::AAC_SAMPLE_RATE,
            is_8bit_sample: false,
            is_stereo: AudioFrame::AAC_STEREO,
            is_aac_sequence_header: sequence_header,
            data,
        })
    }
}

/// NAL units of an Annex B byte stream (3- or 4-byte start codes).
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nalus = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let mut end = starts.get(index + 1).map_or(data.len(), |next| next - 3);
        // A 4-byte start code leaves its leading zero on the previous unit.
        while end > start && data[end - 1] == 0 && index + 1 < starts.len() {
            end -= 1;
        }
        if end > start {
            nalus.push(&data[start..end]);
        }
    }
    nalus
}

/// NAL units of a length-prefixed (AVCC) access unit.
pub fn split_avcc(data: &[u8], length_size: usize) -> Vec<&[u8]> {
    let mut nalus = Vec::new();
    let mut i = 0;
    while i + length_size <= data.len() {
        let len = data[i..i + length_size]
            .iter()
            .fold(0usize, |acc, byte| (acc << 8) | usize::from(*byte));
        i += length_size;
        if len == 0 || i + len > data.len() {
            break;
        }
        nalus.push(&data[i..i + len]);
        i += len;
    }
    nalus
}

fn nal_type(nalu: &[u8]) -> u8 {
    nalu.first().map_or(0, |byte| byte & 0x1f)
}

/// Turns encoder access units into FLV video packets, emitting a fresh AVC
/// sequence header before the first keyframe and whenever SPS/PPS change.
#[derive(Debug, Default)]
pub struct AvcPackager {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    last_ts_ms: Option<u32>,
    sent_header: bool,
}

impl AvcPackager {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current sequence header, for resending after a reconnect.
    pub fn sequence_header(&self, ts_ms: u32) -> Option<MediaPacket> {
        let (sps, pps) = (self.sps.as_ref()?, self.pps.as_ref()?);
        let header = AvcSequenceHeader {
            avc_profile_indication: sps.get(1).copied().unwrap_or(100),
            profile_compatibility: sps.get(2).copied().unwrap_or(0),
            avc_level_indication: sps.get(3).copied().unwrap_or(52),
            length_size_minus_one: 3,
            sps_list: vec![sps.clone()],
            pps_list: vec![pps.clone()],
        };
        Some(MediaPacket::Video {
            ts_ms,
            keyframe: true,
            sequence_header: true,
            data: header.to_bytes().ok()?,
        })
    }

    /// Supply SPS/PPS out of band (VideoToolbox format descriptions).
    pub fn set_parameter_sets(&mut self, sps: Vec<u8>, pps: Vec<u8>) {
        if self.sps.as_ref() != Some(&sps) || self.pps.as_ref() != Some(&pps) {
            self.sent_header = false;
        }
        self.sps = Some(sps);
        self.pps = Some(pps);
    }

    /// Package one access unit. Parameter-set NAL units are lifted into the
    /// sequence header; access unit delimiters are dropped. Timestamps are
    /// forced monotonic. Returns zero packets when nothing decodable remains
    /// (for example, inter frames before the first keyframe).
    pub fn package(&mut self, nalus: &[&[u8]], ts_ms: u32, keyframe_hint: bool) -> Vec<MediaPacket> {
        let ts_ms = match self.last_ts_ms {
            Some(last) if ts_ms < last => last,
            _ => ts_ms,
        };
        let mut sps = None;
        let mut pps = None;
        let mut has_idr = false;
        let mut payload = Vec::new();
        for nalu in nalus {
            match nal_type(nalu) {
                NAL_SPS => sps = Some(nalu.to_vec()),
                NAL_PPS => pps = Some(nalu.to_vec()),
                NAL_AUD => {}
                kind => {
                    has_idr |= kind == NAL_IDR;
                    payload.extend_from_slice(&u32::try_from(nalu.len()).unwrap_or(0).to_be_bytes());
                    payload.extend_from_slice(nalu);
                }
            }
        }
        if let (Some(sps), Some(pps)) = (sps, pps) {
            self.set_parameter_sets(sps, pps);
        }
        let keyframe = has_idr || keyframe_hint;
        let mut out = Vec::new();
        if keyframe && !self.sent_header {
            if let Some(header) = self.sequence_header(ts_ms) {
                out.push(header);
                self.sent_header = true;
            }
        }
        if !self.sent_header || payload.is_empty() {
            return out;
        }
        self.last_ts_ms = Some(ts_ms);
        out.push(MediaPacket::Video {
            ts_ms,
            keyframe,
            sequence_header: false,
            data: payload,
        });
        out
    }
}

/// AAC packets with a sequence header up front and monotonic timestamps.
#[derive(Debug, Default)]
pub struct AacPackager {
    sent_header: bool,
    last_ts_ms: Option<u32>,
}

impl AacPackager {
    pub fn sequence_header(ts_ms: u32) -> MediaPacket {
        MediaPacket::Audio {
            ts_ms,
            sequence_header: true,
            data: AAC_LC_48K_STEREO_ASC.to_vec(),
        }
    }

    /// Package one raw AAC frame (no ADTS header).
    pub fn package(&mut self, raw_aac: Vec<u8>, ts_ms: u32) -> Vec<MediaPacket> {
        let ts_ms = match self.last_ts_ms {
            Some(last) if ts_ms < last => last,
            _ => ts_ms,
        };
        self.last_ts_ms = Some(ts_ms);
        let mut out = Vec::with_capacity(2);
        if !self.sent_header {
            out.push(Self::sequence_header(ts_ms));
            self.sent_header = true;
        }
        if !raw_aac.is_empty() {
            out.push(MediaPacket::Audio { ts_ms, sequence_header: false, data: raw_aac });
        }
        out
    }
}

/// Releases audio and video in non-decreasing timestamp order. A track with
/// nothing queued holds the other back for at most `max_wait_ms`, so a
/// stalled encoder can't stall the stream.
#[derive(Debug)]
pub struct Interleaver {
    video: VecDeque<MediaPacket>,
    audio: VecDeque<MediaPacket>,
    max_wait_ms: u32,
    last_out_ms: u32,
}

impl Interleaver {
    pub fn new(max_wait_ms: u32) -> Self {
        Self { video: VecDeque::new(), audio: VecDeque::new(), max_wait_ms, last_out_ms: 0 }
    }

    pub fn push(&mut self, packet: MediaPacket) {
        if packet.is_video() {
            self.video.push_back(packet);
        } else {
            self.audio.push_back(packet);
        }
    }

    /// Packets now safe to send, in order.
    pub fn drain_ready(&mut self) -> Vec<MediaPacket> {
        let mut out = Vec::new();
        loop {
            let next = match (self.video.front(), self.audio.front()) {
                (Some(v), Some(a)) => {
                    if v.ts_ms() <= a.ts_ms() {
                        self.video.pop_front()
                    } else {
                        self.audio.pop_front()
                    }
                }
                (Some(_), None) => {
                    let newest = self.video.back().map_or(0, MediaPacket::ts_ms);
                    let oldest = self.video.front().map_or(0, MediaPacket::ts_ms);
                    if newest.saturating_sub(oldest) >= self.max_wait_ms {
                        self.video.pop_front()
                    } else {
                        None
                    }
                }
                (None, Some(_)) => {
                    let newest = self.audio.back().map_or(0, MediaPacket::ts_ms);
                    let oldest = self.audio.front().map_or(0, MediaPacket::ts_ms);
                    if newest.saturating_sub(oldest) >= self.max_wait_ms {
                        self.audio.pop_front()
                    } else {
                        None
                    }
                }
                (None, None) => None,
            };
            let Some(packet) = next else { break };
            // Never step backwards on the wire, whatever arrived late.
            let ts = packet.ts_ms().max(self.last_out_ms);
            self.last_out_ms = ts;
            out.push(packet.with_ts(ts));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00];
    const NON_IDR: &[u8] = &[0x41, 0x9a, 0x02];

    fn annex_b(nalus: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for (index, nalu) in nalus.iter().enumerate() {
            if index % 2 == 0 {
                out.extend_from_slice(&[0, 0, 0, 1]);
            } else {
                out.extend_from_slice(&[0, 0, 1]);
            }
            out.extend_from_slice(nalu);
        }
        out
    }

    #[test]
    fn splits_annex_b_with_mixed_start_codes() {
        let stream = annex_b(&[&[0x09, 0xf0], SPS, PPS, IDR]);
        let nalus = split_annex_b(&stream);
        assert_eq!(nalus, vec![&[0x09, 0xf0][..], SPS, PPS, IDR]);
    }

    #[test]
    fn splits_avcc() {
        let mut au = Vec::new();
        for nalu in [IDR, NON_IDR] {
            au.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
            au.extend_from_slice(nalu);
        }
        assert_eq!(split_avcc(&au, 4), vec![IDR, NON_IDR]);
    }

    #[test]
    fn avc_sequence_header_precedes_first_keyframe() {
        let mut packager = AvcPackager::new();
        // Inter frames before any keyframe are undecodable: dropped.
        assert!(packager.package(&[NON_IDR], 0, false).is_empty());
        let out = packager.package(&[&[0x09, 0xf0], SPS, PPS, IDR], 33, false);
        assert_eq!(out.len(), 2);
        assert!(out[0].is_sequence_header());
        let MediaPacket::Video { data, .. } = &out[0] else { panic!() };
        let header = AvcSequenceHeader::from_bytes(data).unwrap();
        assert_eq!(header.sps_list, vec![SPS.to_vec()]);
        assert_eq!(header.pps_list, vec![PPS.to_vec()]);
        assert_eq!(header.avc_profile_indication, 0x64);
        assert_eq!(header.length_size_minus_one, 3);
        assert!(out[1].is_video_keyframe());
        // The NAL payload is AVCC: SPS/PPS/AUD lifted out, length-prefixed IDR.
        let MediaPacket::Video { data, .. } = &out[1] else { panic!() };
        assert_eq!(split_avcc(data, 4), vec![IDR]);
        // Steady state: no header repeat.
        let out = packager.package(&[NON_IDR], 66, false);
        assert_eq!(out.len(), 1);
        assert!(!out[0].is_sequence_header());
        assert!(!out[0].is_video_keyframe());
    }

    #[test]
    fn new_parameter_sets_resend_the_sequence_header() {
        let mut packager = AvcPackager::new();
        packager.package(&[SPS, PPS, IDR], 0, false);
        // Same parameter sets on the next IDR: no new header.
        assert_eq!(packager.package(&[SPS, PPS, IDR], 2000, false).len(), 1);
        // Follow the source: a resize changes the SPS.
        let resized_sps: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40];
        let out = packager.package(&[resized_sps, PPS, IDR], 2500, false);
        assert_eq!(out.len(), 2);
        assert!(out[0].is_sequence_header());
        let MediaPacket::Video { data, .. } = &out[0] else { panic!() };
        assert_eq!(AvcSequenceHeader::from_bytes(data).unwrap().sps_list[0], resized_sps);
    }

    #[test]
    fn video_timestamps_never_go_backwards() {
        let mut packager = AvcPackager::new();
        packager.package(&[SPS, PPS, IDR], 100, false);
        let out = packager.package(&[NON_IDR], 90, false);
        assert_eq!(out[0].ts_ms(), 100);
    }

    #[test]
    fn aac_sequence_header_is_sent_once() {
        let mut packager = AacPackager::default();
        let out = packager.package(vec![1, 2, 3], 0);
        assert_eq!(out.len(), 2);
        let MediaPacket::Audio { sequence_header: true, data, .. } = &out[0] else { panic!() };
        assert_eq!(data, &AAC_LC_48K_STEREO_ASC);
        assert_eq!(packager.package(vec![4], 21).len(), 1);
        let frame = packager.package(vec![5], 42).remove(0).into_audio_frame().unwrap();
        assert_eq!(frame.format, AudioFormat::Aac);
        assert!(!frame.is_aac_sequence_header);
    }

    #[test]
    fn interleaver_orders_tracks_and_stays_monotonic() {
        let mut interleaver = Interleaver::new(500);
        let video = |ts| MediaPacket::Video { ts_ms: ts, keyframe: false, sequence_header: false, data: vec![] };
        let audio = |ts| MediaPacket::Audio { ts_ms: ts, sequence_header: false, data: vec![] };
        interleaver.push(video(0));
        interleaver.push(video(33));
        interleaver.push(video(66));
        // Video alone waits for audio.
        assert!(interleaver.drain_ready().is_empty());
        interleaver.push(audio(0));
        interleaver.push(audio(21));
        interleaver.push(audio(42));
        let order: Vec<(bool, u32)> =
            interleaver.drain_ready().iter().map(|p| (p.is_video(), p.ts_ms())).collect();
        assert_eq!(order, vec![(true, 0), (false, 0), (false, 21), (true, 33), (false, 42)]);
        // Audio stalls: video is released once it is max_wait ahead.
        for ts in [99, 133, 166, 199, 233, 266, 299, 333, 366, 399, 433, 466, 499, 533, 566] {
            interleaver.push(video(ts));
        }
        let out = interleaver.drain_ready();
        assert!(!out.is_empty());
        assert!(out.windows(2).all(|pair| pair[0].ts_ms() <= pair[1].ts_ms()));
        // A late audio packet is clamped rather than sent backwards.
        interleaver.push(audio(50));
        interleaver.push(video(600));
        let late = interleaver.drain_ready();
        let last_before = out.last().unwrap().ts_ms();
        assert!(late.iter().all(|p| p.ts_ms() >= last_before));
    }

    #[test]
    fn converts_to_rtmp_frames() {
        let header = MediaPacket::Video { ts_ms: 5, keyframe: true, sequence_header: true, data: vec![1] };
        let frame = header.into_video_frame().unwrap();
        assert_eq!(frame.avc_packet_type, Some(AvcPacketType::SequenceHeader));
        assert_eq!(frame.frame_type, VideoFrameType::KeyFrame);
        assert_eq!(frame.codec, VideoCodec::Avc);
        let inter = MediaPacket::Video { ts_ms: 6, keyframe: false, sequence_header: false, data: vec![1] };
        let frame = inter.into_video_frame().unwrap();
        assert_eq!(frame.frame_type, VideoFrameType::InterFrame);
        assert_eq!(frame.timestamp.as_millis(), 6);
    }
}
