use alloc::string::String;
use alloc::vec::Vec;

use crate::amf::{AmfValue, AmfVersion};
use crate::error::Error;
use crate::media::{AudioFrame, VideoFrame};
use crate::rtmp_chunk::{RtmpChunkSize, RtmpChunkStreamId};
use crate::rtmp_command::TransactionId;
use crate::rtmp_timestamp::RtmpTimestamp;
use crate::rtmp_user_control_event::RtmpUserControlEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RtmpMessageStreamId(u32);

impl RtmpMessageStreamId {
    // 制御用ストリームは 0 と仕様で決まっている
    pub const PCM: Self = Self(0);

    // サーバーが最初に発行するストリームの ID
    // このストリームの用途は不明だが、値は何でもいいので 1 固定にしておく
    pub const FIRST: Self = Self(1);

    // サーバーがメディアストリーム用に発行する ID
    // この crate 内では、一つの接続で複数ストリームを扱うことはないので固定値を使っている
    pub const MEDIA: Self = Self(2);

    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtmpMessageType {
    // Protocol Control Messages
    SetChunkSize = 1,
    Abort = 2,
    Ack = 3,
    UserControl = 4,
    WinAckSize = 5,
    SetPeerBandwidth = 6,

    // Media Messages
    Audio = 8,
    Video = 9,

    // Data/Command Messages
    DataAmf3 = 15,
    CommandAmf3 = 17,
    DataAmf0 = 18,
    CommandAmf0 = 20,
}

impl RtmpMessageType {
    pub fn from_type_id(type_id: u8) -> Result<Self, Error> {
        match type_id {
            1 => Ok(RtmpMessageType::SetChunkSize),
            2 => Ok(RtmpMessageType::Abort),
            3 => Ok(RtmpMessageType::Ack),
            4 => Ok(RtmpMessageType::UserControl),
            5 => Ok(RtmpMessageType::WinAckSize),
            6 => Ok(RtmpMessageType::SetPeerBandwidth),
            8 => Ok(RtmpMessageType::Audio),
            9 => Ok(RtmpMessageType::Video),
            15 => Ok(RtmpMessageType::DataAmf3),
            17 => Ok(RtmpMessageType::CommandAmf3),
            18 => Ok(RtmpMessageType::DataAmf0),
            20 => Ok(RtmpMessageType::CommandAmf0),
            _ => Err(Error::invalid_data(format!(
                "unknown message type: {type_id}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtmpMessageHeader {
    pub stream_id: RtmpMessageStreamId,
    pub timestamp: RtmpTimestamp,
}

impl RtmpMessageHeader {
    pub const PCM: Self = Self {
        stream_id: RtmpMessageStreamId::PCM,
        timestamp: RtmpTimestamp::ZERO,
    };
}

#[derive(Debug, Clone, PartialEq)]
pub enum RtmpMessage {
    SetChunkSize {
        header: RtmpMessageHeader,
        size: RtmpChunkSize,
    },
    Abort {
        header: RtmpMessageHeader,
        chunk_stream_id: RtmpChunkStreamId,
    },
    Ack {
        header: RtmpMessageHeader,
        sequence_number: u32, // 仕様書での名前的にはシーケンス番号だけど、実際には累積受信バイト数
    },
    WinAckSize {
        header: RtmpMessageHeader,
        size: u32,
    },
    SetPeerBandwidth {
        header: RtmpMessageHeader,
        size: u32,
        limit_type: SetPeerBandwidthLimitType,
    },
    UserControl {
        header: RtmpMessageHeader,
        event: RtmpUserControlEvent,
    },
    Audio {
        header: RtmpMessageHeader,
        frame: AudioFrame,
    },
    Video {
        header: RtmpMessageHeader,
        frame: VideoFrame,
    },
    Command {
        header: RtmpMessageHeader,
        amf_version: AmfVersion,
        name: String,
        transaction_id: TransactionId,
        object: AmfValue,
        args: Vec<AmfValue>,
    },
    Data {
        header: RtmpMessageHeader,
        amf_version: AmfVersion,
        values: Vec<AmfValue>,
    },
}

impl RtmpMessage {
    pub fn header(&self) -> RtmpMessageHeader {
        match self {
            RtmpMessage::SetChunkSize { header, .. }
            | RtmpMessage::Abort { header, .. }
            | RtmpMessage::Ack { header, .. }
            | RtmpMessage::WinAckSize { header, .. }
            | RtmpMessage::SetPeerBandwidth { header, .. }
            | RtmpMessage::UserControl { header, .. }
            | RtmpMessage::Audio { header, .. }
            | RtmpMessage::Video { header, .. }
            | RtmpMessage::Command { header, .. }
            | RtmpMessage::Data { header, .. } => *header,
        }
    }

    pub fn message_type(&self) -> RtmpMessageType {
        match self {
            RtmpMessage::SetChunkSize { .. } => RtmpMessageType::SetChunkSize,
            RtmpMessage::Abort { .. } => RtmpMessageType::Abort,
            RtmpMessage::Ack { .. } => RtmpMessageType::Ack,
            RtmpMessage::WinAckSize { .. } => RtmpMessageType::WinAckSize,
            RtmpMessage::SetPeerBandwidth { .. } => RtmpMessageType::SetPeerBandwidth,
            RtmpMessage::UserControl { .. } => RtmpMessageType::UserControl,
            RtmpMessage::Audio { .. } => RtmpMessageType::Audio,
            RtmpMessage::Video { .. } => RtmpMessageType::Video,
            RtmpMessage::Command { amf_version, .. } => match amf_version {
                AmfVersion::Amf0 => RtmpMessageType::CommandAmf0,
                AmfVersion::Amf3 => RtmpMessageType::CommandAmf3,
            },
            RtmpMessage::Data { amf_version, .. } => match amf_version {
                AmfVersion::Amf0 => RtmpMessageType::DataAmf0,
                AmfVersion::Amf3 => RtmpMessageType::DataAmf3,
            },
        }
    }

    pub fn stream_begin(stream_id: RtmpMessageStreamId) -> Self {
        Self::UserControl {
            header: RtmpMessageHeader::PCM,
            event: RtmpUserControlEvent::StreamBegin { stream_id },
        }
    }

    pub fn win_ack_size(size: u32) -> Self {
        Self::WinAckSize {
            header: RtmpMessageHeader::PCM,
            size,
        }
    }

    pub fn set_peer_bandwidth(size: u32) -> Self {
        Self::SetPeerBandwidth {
            header: RtmpMessageHeader::PCM,
            size,

            // いったん挙動が一番シンプルな Hard 固定にしておく
            limit_type: SetPeerBandwidthLimitType::Hard,
        }
    }

    pub fn ack(total_bytes_received: u32) -> Self {
        Self::Ack {
            header: RtmpMessageHeader::PCM,
            sequence_number: total_bytes_received,
        }
    }

    pub fn set_chunk_size(size: RtmpChunkSize) -> Self {
        Self::SetChunkSize {
            header: RtmpMessageHeader::PCM,
            size,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetPeerBandwidthLimitType {
    Hard = 0,
    Soft,
    Dynamic,
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::string::ToString;

    use crate::amf::Pair;
    use crate::amf0::Amf0Value;
    use crate::amf3::Amf3Value;
    use crate::rtmp_message_decoder::RtmpMessageDecoder;
    use crate::rtmp_message_encoder::RtmpMessageEncoder;
    use crate::rtmp_timestamp::RtmpTimestampDelta;
    use crate::{AudioFormat, AudioSampleRate};

    fn header(stream_id: u32, timestamp_ms: u32) -> RtmpMessageHeader {
        RtmpMessageHeader {
            stream_id: RtmpMessageStreamId::new(stream_id),
            timestamp: RtmpTimestamp::from_millis(timestamp_ms),
        }
    }

    fn pcm_header(timestamp_ms: u32) -> RtmpMessageHeader {
        RtmpMessageHeader {
            stream_id: RtmpMessageStreamId::PCM,
            timestamp: RtmpTimestamp::from_millis(timestamp_ms),
        }
    }

    fn encode_decode_roundtrip(message: RtmpMessage) -> RtmpMessage {
        let chunk_stream_id = RtmpChunkStreamId::new(3).unwrap();
        let mut encoder = RtmpMessageEncoder::default();
        let mut buf = Vec::new();

        encoder.encode(&mut buf, chunk_stream_id, message);

        let mut decoder = RtmpMessageDecoder::default();

        decoder.feed_buf(&buf);
        decoder.decode().unwrap().unwrap()
    }

    #[test]
    fn test_set_chunk_size_decode_encode() {
        let msg = RtmpMessage::SetChunkSize {
            header: pcm_header(0),
            size: RtmpChunkSize::new(1234).unwrap(),
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_abort_decode_encode() {
        let msg = RtmpMessage::Abort {
            header: pcm_header(0),
            chunk_stream_id: RtmpChunkStreamId::new(10).unwrap(),
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_ack_decode_encode() {
        let msg = RtmpMessage::Ack {
            header: pcm_header(0),
            sequence_number: 56789,
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_win_ack_size_decode_encode() {
        let msg = RtmpMessage::WinAckSize {
            header: pcm_header(0),
            size: 45678,
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_set_peer_bandwidth_decode_encode() {
        let msg = RtmpMessage::SetPeerBandwidth {
            header: pcm_header(0),
            size: 4567,
            limit_type: SetPeerBandwidthLimitType::Soft,
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_user_control_stream_begin_decode_encode() {
        let msg = RtmpMessage::UserControl {
            header: pcm_header(0),
            event: RtmpUserControlEvent::StreamBegin {
                stream_id: RtmpMessageStreamId::new(10),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_user_control_stream_eof_decode_encode() {
        let msg = RtmpMessage::UserControl {
            header: pcm_header(0),
            event: RtmpUserControlEvent::StreamEof {
                stream_id: RtmpMessageStreamId::new(10),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_user_control_stream_dry_decode_encode() {
        let msg = RtmpMessage::UserControl {
            header: pcm_header(0),
            event: RtmpUserControlEvent::StreamDry {
                stream_id: RtmpMessageStreamId::new(10),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_user_control_set_buffer_length_decode_encode() {
        let msg = RtmpMessage::UserControl {
            header: pcm_header(0),
            event: RtmpUserControlEvent::SetBufferLength {
                stream_id: RtmpMessageStreamId::new(10),
                length: 1234,
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_user_control_stream_is_recorded_decode_encode() {
        let msg = RtmpMessage::UserControl {
            header: pcm_header(0),
            event: RtmpUserControlEvent::StreamIsRecorded {
                stream_id: RtmpMessageStreamId::new(10),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_user_control_ping_request_decode_encode() {
        let msg = RtmpMessage::UserControl {
            header: pcm_header(0),
            event: RtmpUserControlEvent::PingRequest {
                timestamp: RtmpTimestamp::from_millis(3456),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_user_control_ping_response_decode_encode() {
        let msg = RtmpMessage::UserControl {
            header: pcm_header(0),
            event: RtmpUserControlEvent::PingResponse {
                timestamp: RtmpTimestamp::from_millis(3456),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_user_control_buffer_empty_decode_encode() {
        let msg = RtmpMessage::UserControl {
            header: pcm_header(0),
            event: RtmpUserControlEvent::BufferEmpty {
                stream_id: RtmpMessageStreamId::new(10),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_user_control_buffer_ready_decode_encode() {
        let msg = RtmpMessage::UserControl {
            header: pcm_header(0),
            event: RtmpUserControlEvent::BufferReady {
                stream_id: RtmpMessageStreamId::new(10),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_audio_decode_encode() {
        let msg = RtmpMessage::Audio {
            header: header(10, 1234),
            frame: AudioFrame {
                timestamp: RtmpTimestamp::from_millis(1234),
                format: AudioFormat::Aac,
                sample_rate: AudioSampleRate::Khz44,
                is_8bit_sample: true,
                is_stereo: true,
                is_aac_sequence_header: false,
                data: b"dummy_audio_data".to_vec(),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_video_decode_encode() {
        let msg = RtmpMessage::Video {
            header: header(10, 1234),
            frame: VideoFrame {
                timestamp: RtmpTimestamp::from_millis(1234),
                composition_timestamp_offset: RtmpTimestampDelta::from_millis(1234),
                frame_type: crate::media::VideoFrameType::KeyFrame,
                codec: crate::media::VideoCodec::Avc,
                avc_packet_type: Some(crate::media::AvcPacketType::NalUnit),
                data: b"dummy_video_data".to_vec(),
            },
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_command_amf0_decode_encode() {
        let msg = RtmpMessage::Command {
            header: header(10, 0),
            amf_version: AmfVersion::Amf0,
            name: "connect".to_string(),
            transaction_id: TransactionId::CONNECT,
            object: AmfValue::Amf0(Amf0Value::Object {
                class_name: None,
                entries: vec![Pair {
                    key: "a".to_string(),
                    value: Amf0Value::String("b".to_string()),
                }],
            }),
            args: vec![
                AmfValue::Amf0(Amf0Value::String("string".to_string())),
                AmfValue::Amf0(Amf0Value::Array {
                    entries: vec![
                        Amf0Value::Number(1.0),
                        Amf0Value::Number(2.0),
                        Amf0Value::Number(3.0),
                    ],
                }),
            ],
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_command_amf3_decode_encode() {
        let msg = RtmpMessage::Command {
            header: header(10, 0),
            amf_version: AmfVersion::Amf3,
            name: "connect".to_string(),
            transaction_id: TransactionId::CONNECT,
            object: AmfValue::Amf3(Amf3Value::Object {
                class_name: None,
                sealed_count: 1,
                entries: vec![Pair {
                    key: "a".to_string(),
                    value: Amf3Value::String("b".to_string()),
                }],
            }),
            args: vec![
                AmfValue::Amf3(Amf3Value::String("string".to_string())),
                AmfValue::Amf3(Amf3Value::Array {
                    assoc_entries: vec![],
                    dense_entries: vec![
                        Amf3Value::Double(1.0),
                        Amf3Value::Double(2.0),
                        Amf3Value::Double(3.0),
                    ],
                }),
            ],
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_data_amf0_decode_encode() {
        let msg = RtmpMessage::Data {
            header: header(10, 0),
            amf_version: AmfVersion::Amf0,
            values: vec![AmfValue::Amf0(Amf0Value::Object {
                class_name: None,
                entries: vec![
                    Pair {
                        key: "a".to_string(),
                        value: Amf0Value::String("b".to_string()),
                    },
                    Pair {
                        key: "c".to_string(),
                        value: Amf0Value::Number(10.4),
                    },
                ],
            })],
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_data_amf3_decode_encode() {
        let msg = RtmpMessage::Data {
            header: header(10, 0),
            amf_version: AmfVersion::Amf3,
            values: vec![AmfValue::Amf3(Amf3Value::Object {
                class_name: None,
                sealed_count: 2,
                entries: vec![
                    Pair {
                        key: "a".to_string(),
                        value: Amf3Value::String("b".to_string()),
                    },
                    Pair {
                        key: "c".to_string(),
                        value: Amf3Value::Double(10.4),
                    },
                ],
            })],
        };

        let decoded = encode_decode_roundtrip(msg.clone());
        assert_eq!(msg, decoded);
    }
}
