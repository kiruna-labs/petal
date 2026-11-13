use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::bytes::BytesWriter;
use crate::rtmp_chunk::{MessageHeaderFormat, RtmpChunk, RtmpChunkSize, RtmpChunkStreamId};
use crate::rtmp_message::{RtmpMessageStreamId, RtmpMessageType};
use crate::rtmp_timestamp::RtmpTimestamp;

#[derive(Debug, Default)]
pub struct RtmpChunkEncoder {
    chunk_size: RtmpChunkSize,
    chunk_streams: BTreeMap<RtmpChunkStreamId, RtmpChunkStream>,
}

impl RtmpChunkEncoder {
    pub fn set_chunk_size(&mut self, size: RtmpChunkSize) {
        self.chunk_size = size;
    }

    pub fn encode(&mut self, buf: &mut Vec<u8>, chunk: &RtmpChunk) {
        let mut chunk_stream = self.resolve_chunk_stream(chunk);
        self.encode_message(buf, &mut chunk_stream, &chunk.payload);
        self.chunk_streams
            .insert(chunk.chunk_stream_id, chunk_stream);
    }

    fn encode_message(
        &self,
        buf: &mut Vec<u8>,
        chunk_stream: &mut RtmpChunkStream,
        message_payload: &[u8],
    ) {
        let mut format = chunk_stream.format;
        let mut offset = 0;
        let mut first = true;
        let chunk_size = self.chunk_size.get();

        while first || offset < message_payload.len() {
            self.encode_chunk_basic_header(buf, format, chunk_stream);
            self.encode_message_header(buf, format, chunk_stream);

            let remaining = message_payload.len() - offset;
            let chunk_payload_size = remaining.min(chunk_size);

            buf.write_bytes(&message_payload[offset..offset + chunk_payload_size]);
            offset += chunk_payload_size;

            format = MessageHeaderFormat::F3;
            first = false;
        }
    }

    fn encode_chunk_basic_header(
        &self,
        buf: &mut Vec<u8>,
        format: MessageHeaderFormat,
        chunk_stream: &RtmpChunkStream,
    ) {
        let fmt = format as u8;
        let id = chunk_stream.chunk_stream_id.get();

        if id < 64 {
            buf.write_u8((fmt << 6) | (id as u8));
        } else if id < 320 {
            buf.write_u8(fmt << 6);
            buf.write_u8((id - 64) as u8);
        } else {
            assert!(id < 65600);
            buf.write_u8((fmt << 6) | 1);

            // リトルエンディアンとして扱うために swap_bytes() を呼んでいる
            buf.write_u16(((id - 64) as u16).swap_bytes());
        }
    }

    fn encode_message_header(
        &self,
        buf: &mut Vec<u8>,
        format: MessageHeaderFormat,
        chunk_stream: &mut RtmpChunkStream,
    ) {
        match format {
            MessageHeaderFormat::F0 => {
                let timestamp = chunk_stream.timestamp.as_millis();
                let is_extended = timestamp >= 0xFFFFFF;
                if is_extended {
                    buf.write_u24(0xFFFFFF);
                } else {
                    buf.write_u24(timestamp);
                }
                buf.write_u24(chunk_stream.message_size as u32);
                buf.write_u8(chunk_stream.message_type as u8);
                buf.write_u32(chunk_stream.message_stream_id.get().swap_bytes()); // little endian
                if is_extended {
                    buf.write_u32(timestamp);
                }
                chunk_stream.enable_f3_extended_timestamp = is_extended;
                chunk_stream.timestamp_delta = chunk_stream.timestamp; // ここでの delta は「0 から timestamp 分増えた」と解釈される
            }
            MessageHeaderFormat::F1 => {
                let timestamp_delta = chunk_stream.timestamp_delta.as_millis();
                let is_extended = timestamp_delta >= 0xFFFFFF;
                if is_extended {
                    buf.write_u24(0xFFFFFF);
                } else {
                    buf.write_u24(timestamp_delta);
                }
                buf.write_u24(chunk_stream.message_size as u32);
                buf.write_u8(chunk_stream.message_type as u8);
                if is_extended {
                    buf.write_u32(timestamp_delta);
                }
                chunk_stream.enable_f3_extended_timestamp = is_extended;
            }
            MessageHeaderFormat::F2 => {
                let timestamp_delta = chunk_stream.timestamp_delta.as_millis();
                let is_extended = timestamp_delta >= 0xFFFFFF;
                if is_extended {
                    buf.write_u24(0xFFFFFF);
                } else {
                    buf.write_u24(timestamp_delta);
                }
                if is_extended {
                    buf.write_u32(timestamp_delta);
                }
                chunk_stream.enable_f3_extended_timestamp = is_extended;
            }
            MessageHeaderFormat::F3 => {
                if chunk_stream.enable_f3_extended_timestamp {
                    let timestamp_delta = chunk_stream.timestamp_delta.as_millis();
                    buf.write_u32(timestamp_delta);
                }
            }
        }
    }

    fn resolve_chunk_stream(&self, chunk: &RtmpChunk) -> RtmpChunkStream {
        let mut chunk_stream = RtmpChunkStream {
            chunk_stream_id: chunk.chunk_stream_id,
            timestamp: chunk.timestamp,
            timestamp_delta: chunk.timestamp,
            message_type: chunk.message_type,
            message_stream_id: chunk.message_stream_id,
            message_size: chunk.payload.len(),
            format: MessageHeaderFormat::F0,
            enable_f3_extended_timestamp: false,
        };

        // 同じストリームの最後の状態を考慮して `chunk_stream` の更新やエンコードフォーマットの決定を行う
        let maybe_last_chunk_stream = self.chunk_streams.get(&chunk.chunk_stream_id);
        let Some(last_chunk_stream) = maybe_last_chunk_stream else {
            // 初めてのチャンクなら何もしない
            return chunk_stream;
        };
        chunk_stream.enable_f3_extended_timestamp = last_chunk_stream.enable_f3_extended_timestamp;

        let Some(timestamp_delta) = chunk_stream
            .timestamp
            .checked_sub(last_chunk_stream.timestamp)
        else {
            // タイムスタンプがラップアラウンドしている場合も何もしない
            return chunk_stream;
        };
        chunk_stream.timestamp_delta = timestamp_delta;

        // 前回からの差分を考慮してフォーマットを決定する
        chunk_stream.format = match (
            chunk_stream.message_stream_id == last_chunk_stream.message_stream_id,
            chunk_stream.message_type == last_chunk_stream.message_type,
            chunk_stream.message_size == last_chunk_stream.message_size,
            chunk_stream.timestamp_delta == last_chunk_stream.timestamp_delta,
        ) {
            (true, true, true, true) => {
                // 全て一致している
                MessageHeaderFormat::F3
            }
            (true, true, true, _) => {
                // `timestamp_delta` 以外が一致している
                MessageHeaderFormat::F2
            }
            (true, _, _, _) => {
                // `message_stream_id` だけが一致している
                MessageHeaderFormat::F1
            }
            _ => {
                // 何も一致していない
                MessageHeaderFormat::F0
            }
        };

        chunk_stream
    }
}

#[derive(Debug, Clone)]
struct RtmpChunkStream {
    pub chunk_stream_id: RtmpChunkStreamId,
    timestamp: RtmpTimestamp,
    timestamp_delta: RtmpTimestamp, // [NOTE] ここでは負数は扱わないので RtmpTimestampDelta は使わない
    message_type: RtmpMessageType,
    message_stream_id: RtmpMessageStreamId,
    message_size: usize,
    format: MessageHeaderFormat,
    enable_f3_extended_timestamp: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_basic_chunk() {
        let input_chunk = input_chunk();
        let expected = fmt_0(4, 300, 3, 2, b"abc");
        let encoded = encode_chunks(RtmpChunkEncoder::default(), &[input_chunk]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_consecutive_chunks_fmt0_to_fmt1() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            message_type: RtmpMessageType::CommandAmf0,
            ..chunk0.clone()
        };
        let expected = [fmt_0(4, 300, 3, 2, b"abc"), fmt_1(4, 0, 20, b"abc")].concat();
        let encoded = encode_chunks(RtmpChunkEncoder::default(), &[chunk0, chunk1]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_consecutive_chunks_fmt0_to_fmt2() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(700),
            ..chunk0.clone()
        };
        let expected = [fmt_0(4, 300, 3, 2, b"abc"), fmt_2(4, 400, b"abc")].concat();
        let encoded = encode_chunks(RtmpChunkEncoder::default(), &[chunk0, chunk1]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_consecutive_chunks_fmt0_to_fmt2_to_fmt3_to_fmt3() {
        let chunk = input_chunk();
        let expected = [
            fmt_0(4, 300, 3, 2, b"abc"),
            fmt_2(4, 0, b"abc"),
            fmt_3(4, b"abc"),
            fmt_3(4, b"abc"),
        ]
        .concat();
        let encoded = encode_chunks(
            RtmpChunkEncoder::default(),
            &[chunk.clone(), chunk.clone(), chunk.clone(), chunk],
        );
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_consecutive_chunks_fmt0_to_fmt3_to_fmt3() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            timestamp: chunk0.timestamp.wrapping_add(chunk0.timestamp),
            ..chunk0.clone()
        };
        let chunk2 = RtmpChunk {
            timestamp: chunk1.timestamp.wrapping_add(chunk0.timestamp),
            ..chunk1.clone()
        };
        let expected = [
            fmt_0(4, 300, 3, 2, b"abc"),
            fmt_3(4, b"abc"),
            fmt_3(4, b"abc"),
        ]
        .concat();
        let encoded = encode_chunks(RtmpChunkEncoder::default(), &[chunk0, chunk1, chunk2]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_consecutive_chunks_fmt0_to_fmt1_to_fmt2_to_fmt3() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            message_type: RtmpMessageType::CommandAmf0,
            ..chunk0.clone()
        };
        let chunk2 = RtmpChunk {
            timestamp: chunk1
                .timestamp
                .wrapping_add(RtmpTimestamp::from_millis(400)),
            ..chunk1.clone()
        };
        let chunk3 = RtmpChunk {
            timestamp: chunk2
                .timestamp
                .wrapping_add(RtmpTimestamp::from_millis(400)),
            ..chunk2.clone()
        };
        let expected = [
            fmt_0(4, 300, 3, 2, b"abc"),
            fmt_1(4, 0, 20, b"abc"),
            fmt_2(4, 400, b"abc"),
            fmt_3(4, b"abc"),
        ]
        .concat();
        let encoded = encode_chunks(
            RtmpChunkEncoder::default(),
            &[chunk0, chunk1, chunk2, chunk3],
        );
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_consecutive_chunks_fmt0_to_fmt2_to_fmt0_to_fmt1() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(700),
            ..chunk0.clone()
        };
        let chunk2 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(100),
            ..chunk1.clone()
        };
        let chunk3 = RtmpChunk {
            payload: b"a".to_vec(),
            ..chunk2.clone()
        };
        let expected = [
            fmt_0(4, 300, 3, 2, b"abc"),
            fmt_2(4, 400, b"abc"),
            fmt_0(4, 100, 3, 2, b"abc"),
            fmt_1(4, 0, 3, b"a"),
        ]
        .concat();
        let encoded = encode_chunks(
            RtmpChunkEncoder::default(),
            &[chunk0, chunk1, chunk2, chunk3],
        );
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_extended_timestamp_fmt0_to_fmt3() {
        let timestamp = 0x12345678u32; // >= 0xFFFFFF
        let chunk0 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp),
            ..input_chunk()
        };
        let chunk1 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp.wrapping_add(timestamp)),
            ..chunk0.clone()
        };
        let expected = [
            fmt_ext_0(4, timestamp, 3, 2, b"abc"),
            fmt_ext_3(4, timestamp, b"abc"),
        ]
        .concat();
        let encoded = encode_chunks(RtmpChunkEncoder::default(), &[chunk0, chunk1]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_extended_timestamp_boundary_fmt0_to_fmt3() {
        // Test case 1: exactly 0xFFFFFF
        let timestamp0 = 0xFFFFFFu32;
        let chunk0_0 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp0),
            ..input_chunk()
        };
        let chunk0_1 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp0.wrapping_add(timestamp0)),
            ..chunk0_0.clone()
        };
        let expected0 = [
            fmt_ext_0(4, timestamp0, 3, 2, b"abc"),
            fmt_ext_3(4, timestamp0, b"abc"),
        ]
        .concat();
        let encoded0 = encode_chunks(RtmpChunkEncoder::default(), &[chunk0_0, chunk0_1]);
        assert_eq!(encoded0, expected0);

        // Test case 2: 0xFFFFFF - 1
        let timestamp1 = 0xFFFFFEu32;
        let chunk1_0 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp1),
            ..input_chunk()
        };
        let chunk1_1 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp1.wrapping_add(timestamp1)),
            ..chunk1_0.clone()
        };
        let expected1 = [fmt_0(4, timestamp1, 3, 2, b"abc"), fmt_3(4, b"abc")].concat();
        let encoded1 = encode_chunks(RtmpChunkEncoder::default(), &[chunk1_0, chunk1_1]);
        assert_eq!(encoded1, expected1);

        // Test case 3: 0xFFFFFF + 1
        let timestamp2 = 0x1000000u32;
        let chunk2_0 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp2),
            ..input_chunk()
        };
        let chunk2_1 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp2.wrapping_add(timestamp2)),
            ..chunk2_0.clone()
        };
        let expected2 = [
            fmt_ext_0(4, timestamp2, 3, 2, b"abc"),
            fmt_ext_3(4, timestamp2, b"abc"),
        ]
        .concat();
        let encoded2 = encode_chunks(RtmpChunkEncoder::default(), &[chunk2_0, chunk2_1]);
        assert_eq!(encoded2, expected2);
    }

    #[test]
    fn encode_extended_timestamp_fmt0_to_fmt1_to_fmt3() {
        let timestamp_base = 300u32;
        let timestamp_delta = 0x12345678u32; // >= 0xFFFFFF

        let chunk0 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp_base),
            ..input_chunk()
        };
        let chunk1 = RtmpChunk {
            message_type: RtmpMessageType::Video,
            timestamp: RtmpTimestamp::from_millis(timestamp_base.wrapping_add(timestamp_delta)),
            ..chunk0.clone()
        };
        let chunk2 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(
                timestamp_base
                    .wrapping_add(timestamp_delta)
                    .wrapping_add(timestamp_delta),
            ),
            ..chunk1.clone()
        };

        let expected = [
            fmt_0(4, timestamp_base, 3, 2, b"abc"),
            fmt_ext_1(4, timestamp_delta, 9, b"abc"),
            fmt_ext_3(4, timestamp_delta, b"abc"),
        ]
        .concat();
        let encoded = encode_chunks(RtmpChunkEncoder::default(), &[chunk0, chunk1, chunk2]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_extended_timestamp_boundary_fmt0_to_fmt1_to_fmt3() {
        let timestamp_base = 300u32;

        // Test case 1: exactly 0xFFFFFF
        let timestamp_delta0 = 0xFFFFFFu32;
        let chunk0_0 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp_base),
            ..input_chunk()
        };
        let chunk0_1 = RtmpChunk {
            message_type: RtmpMessageType::Video,
            timestamp: RtmpTimestamp::from_millis(timestamp_base.wrapping_add(timestamp_delta0)),
            ..chunk0_0.clone()
        };
        let chunk0_2 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(
                timestamp_base
                    .wrapping_add(timestamp_delta0)
                    .wrapping_add(timestamp_delta0),
            ),
            ..chunk0_1.clone()
        };
        let expected0 = [
            fmt_0(4, timestamp_base, 3, 2, b"abc"),
            fmt_ext_1(4, timestamp_delta0, 9, b"abc"),
            fmt_ext_3(4, timestamp_delta0, b"abc"),
        ]
        .concat();
        let encoded0 = encode_chunks(RtmpChunkEncoder::default(), &[chunk0_0, chunk0_1, chunk0_2]);
        assert_eq!(encoded0, expected0);

        // Test case 2: 0xFFFFFF - 1
        let timestamp_delta1 = 0xFFFFFEu32;
        let chunk1_0 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp_base),
            ..input_chunk()
        };
        let chunk1_1 = RtmpChunk {
            message_type: RtmpMessageType::Video,
            timestamp: RtmpTimestamp::from_millis(timestamp_base.wrapping_add(timestamp_delta1)),
            ..chunk1_0.clone()
        };
        let chunk1_2 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(
                timestamp_base
                    .wrapping_add(timestamp_delta1)
                    .wrapping_add(timestamp_delta1),
            ),
            ..chunk1_1.clone()
        };
        let expected1 = [
            fmt_0(4, timestamp_base, 3, 2, b"abc"),
            fmt_1(4, timestamp_delta1, 9, b"abc"),
            fmt_3(4, b"abc"),
        ]
        .concat();
        let encoded1 = encode_chunks(RtmpChunkEncoder::default(), &[chunk1_0, chunk1_1, chunk1_2]);
        assert_eq!(encoded1, expected1);

        // Test case 3: 0xFFFFFF + 1
        let timestamp_delta2 = 0x1000000u32;
        let chunk2_0 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp_base),
            ..input_chunk()
        };
        let chunk2_1 = RtmpChunk {
            message_type: RtmpMessageType::Video,
            timestamp: RtmpTimestamp::from_millis(timestamp_base.wrapping_add(timestamp_delta2)),
            ..chunk2_0.clone()
        };
        let chunk2_2 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(
                timestamp_base
                    .wrapping_add(timestamp_delta2)
                    .wrapping_add(timestamp_delta2),
            ),
            ..chunk2_1.clone()
        };
        let expected2 = [
            fmt_0(4, timestamp_base, 3, 2, b"abc"),
            fmt_ext_1(4, timestamp_delta2, 9, b"abc"),
            fmt_ext_3(4, timestamp_delta2, b"abc"),
        ]
        .concat();
        let encoded2 = encode_chunks(RtmpChunkEncoder::default(), &[chunk2_0, chunk2_1, chunk2_2]);
        assert_eq!(encoded2, expected2);
    }

    #[test]
    fn encode_chunk_id_over_320_uses_little_endian() {
        // チャンク ID 320 は (320 - 64) = 256 = 0x0100 としてエンコードされる
        // リトルエンディアン: 0x00, 0x01
        // ビッグエンディアン（間違い）: 0x01, 0x00
        let chunk = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(320).expect("infallible"),
            ..input_chunk()
        };

        let encoded = encode_chunks(RtmpChunkEncoder::default(), &[chunk]);

        // 基本ヘッダーは以下のようになるべき:
        // - fmt_flag | 1 = 0b0000_0001 (fmt=0, ID は 2 バイトエンコーディングを使用)
        // - delta_id[0] = 0x00 (256 のリトルエンディアン下位バイト)
        // - delta_id[1] = 0x01 (256 のリトルエンディアン上位バイト)
        assert_eq!(encoded[0], 0x01);
        assert_eq!(encoded[1], 0x00);
        assert_eq!(encoded[2], 0x01);
    }

    #[test]
    fn encode_chunk_id_65599_uses_little_endian() {
        // チャンク ID 65599 は (65599 - 64) = 65535 = 0xFFFF としてエンコードされる
        // リトルエンディアン: 0xFF, 0xFF
        let chunk = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(65599).expect("infallible"),
            ..input_chunk()
        };

        let encoded = encode_chunks(RtmpChunkEncoder::default(), &[chunk]);

        // 基本ヘッダーは以下のようになるべき:
        // - fmt_flag | 1 = 0b0000_0001 (fmt=0, ID は 2 バイトエンコーディングを使用)
        // - delta_id[0] = 0xFF (65535 のリトルエンディアン下位バイト)
        // - delta_id[1] = 0xFF (65535 のリトルエンディアン上位バイト)
        assert_eq!(encoded[0], 0x01);
        assert_eq!(encoded[1], 0xFF);
        assert_eq!(encoded[2], 0xFF);
    }

    fn input_chunk() -> RtmpChunk {
        RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(4).expect("infallible"),
            message_stream_id: RtmpMessageStreamId::new(2),
            message_type: RtmpMessageType::Ack,
            timestamp: RtmpTimestamp::from_millis(300),
            payload: b"abc".to_vec(),
        }
    }

    fn encode_chunks(mut encoder: RtmpChunkEncoder, chunks: &[RtmpChunk]) -> Vec<u8> {
        let mut buf = Vec::new();
        for chunk in chunks {
            encoder.encode(&mut buf, chunk);
        }
        buf
    }

    fn fmt_0(chunk_id: u8, timestamp: u32, type_id: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.write_u8(chunk_id);
        buf.write_u24(timestamp);
        buf.write_u24(payload.len() as u32);
        buf.write_u8(type_id);
        buf.write_u32(stream_id.swap_bytes());
        buf.write_bytes(payload);

        buf
    }

    fn fmt_1(chunk_id: u8, timestamp_delta: u32, type_id: u8, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.write_u8(0b0100_0000 | chunk_id);
        buf.write_u24(timestamp_delta);
        buf.write_u24(payload.len() as u32);
        buf.write_u8(type_id);
        buf.write_bytes(payload);

        buf
    }

    fn fmt_2(chunk_id: u8, timestamp_delta: u32, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.write_u8(0b1000_0000 | chunk_id);
        buf.write_u24(timestamp_delta);
        buf.write_bytes(payload);

        buf
    }

    fn fmt_3(chunk_id: u8, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.write_u8(0b1100_0000 | chunk_id);
        buf.write_bytes(payload);

        buf
    }

    fn fmt_ext_0(
        chunk_id: u8,
        timestamp: u32,
        type_id: u8,
        stream_id: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.write_u8(chunk_id);
        buf.write_u24(0xFFFFFF);
        buf.write_u24(payload.len() as u32);
        buf.write_u8(type_id);
        buf.write_u32(stream_id.swap_bytes());
        buf.write_u32(timestamp);
        buf.write_bytes(payload);

        buf
    }

    fn fmt_ext_1(chunk_id: u8, timestamp_delta: u32, type_id: u8, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.write_u8(0b0100_0000 | chunk_id);
        buf.write_u24(0xFFFFFF);
        buf.write_u24(payload.len() as u32);
        buf.write_u8(type_id);
        buf.write_u32(timestamp_delta);
        buf.write_bytes(payload);

        buf
    }

    fn fmt_ext_3(chunk_id: u8, timestamp_delta: u32, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.write_u8(0b1100_0000 | chunk_id);
        buf.write_u32(timestamp_delta);
        buf.write_bytes(payload);

        buf
    }
}
