use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::mem;

use crate::bytes::BytesReader;
use crate::error::Error;
use crate::rtmp_chunk::{MessageHeaderFormat, RtmpChunk, RtmpChunkSize, RtmpChunkStreamId};
use crate::rtmp_message::{RtmpMessageStreamId, RtmpMessageType};
use crate::rtmp_timestamp::RtmpTimestamp;

#[derive(Debug, Default)]
pub struct RtmpChunkDecoder {
    chunk_size: RtmpChunkSize,
    chunk_streams: BTreeMap<RtmpChunkStreamId, RtmpChunkStream>,
}

impl RtmpChunkDecoder {
    pub fn set_chunk_size(&mut self, size: RtmpChunkSize) {
        self.chunk_size = size;
    }

    pub fn reset_chunk_stream(&mut self, chunk_stream_id: RtmpChunkStreamId) {
        self.chunk_streams.remove(&chunk_stream_id);
    }

    pub fn decode(&mut self, mut buf: &[u8]) -> Result<(usize, Option<RtmpChunk>), Error> {
        let original_buf_len = buf.len();

        let (format, chunk_stream_id) = self.decode_chunk_basic_header(&mut buf)?;
        let mut chunk_stream = match self.chunk_streams.get(&chunk_stream_id).cloned() {
            Some(v) => v,
            None if format == MessageHeaderFormat::F0 => RtmpChunkStream::new(),
            None => {
                return Err(Error::invalid_data(
                    "format ID must be 0 for the first chunk",
                ));
            }
        };
        self.decode_message_header(&mut buf, format, &mut chunk_stream)?;
        let maybe_payload = self.decode_message_payload(&mut buf, &mut chunk_stream)?;

        let maybe_chunk = if let Some(payload) = maybe_payload {
            Some(RtmpChunk {
                chunk_stream_id,
                message_stream_id: chunk_stream.message_stream_id,
                message_type: chunk_stream.message_type,
                timestamp: chunk_stream.timestamp,
                payload,
            })
        } else {
            None
        };
        self.chunk_streams.insert(chunk_stream_id, chunk_stream);

        let bytes_consumed = original_buf_len - buf.len();
        Ok((bytes_consumed, maybe_chunk))
    }

    fn decode_message_payload(
        &self,
        buf: &mut &[u8],
        chunk_stream: &mut RtmpChunkStream,
    ) -> Result<Option<Vec<u8>>, Error> {
        let chunk_size = self.chunk_size.get();
        let remaining_message = chunk_stream
            .message_size
            .checked_sub(chunk_stream.acc_payload.len())
            .ok_or_else(|| Error::invalid_data("accumulated payload exceeds message size"))?;
        let chunk_payload_size = remaining_message.min(chunk_size);

        let payload = buf.read_bytes(chunk_payload_size)?;
        chunk_stream.acc_payload.extend_from_slice(&payload);

        if chunk_stream.acc_payload.len() == chunk_stream.message_size {
            chunk_stream.timestamp = chunk_stream
                .timestamp
                .wrapping_add(chunk_stream.timestamp_delta);
            let complete_payload = mem::take(&mut chunk_stream.acc_payload);
            Ok(Some(complete_payload))
        } else {
            Ok(None)
        }
    }

    fn decode_message_header(
        &self,
        buf: &mut &[u8],
        format: MessageHeaderFormat,
        chunk_stream: &mut RtmpChunkStream,
    ) -> Result<(), Error> {
        match format {
            MessageHeaderFormat::F0 => {
                let timestamp = buf.read_u24()?;
                let message_size = buf.read_u24()? as usize;
                let message_type = buf.read_u8()?;
                let message_stream_id = buf.read_u32().map(|v| v.swap_bytes())?; // little endian

                let is_extended = timestamp == 0xFFFFFF;
                chunk_stream.enable_f3_extended_timestamp = is_extended;
                chunk_stream.timestamp_delta = if is_extended {
                    RtmpTimestamp::from_millis(buf.read_u32()?)
                } else {
                    RtmpTimestamp::from_millis(timestamp)
                };
                chunk_stream.timestamp = RtmpTimestamp::ZERO;
                chunk_stream.message_size = message_size;
                chunk_stream.message_type = RtmpMessageType::from_type_id(message_type)?;
                chunk_stream.message_stream_id = RtmpMessageStreamId::new(message_stream_id);
            }
            MessageHeaderFormat::F1 => {
                let timestamp_delta = buf.read_u24()?;
                let message_size = buf.read_u24()? as usize;
                let message_type = buf.read_u8()?;

                let is_extended = timestamp_delta == 0xFFFFFF;
                chunk_stream.enable_f3_extended_timestamp = is_extended;
                chunk_stream.timestamp_delta = if is_extended {
                    RtmpTimestamp::from_millis(buf.read_u32()?)
                } else {
                    RtmpTimestamp::from_millis(timestamp_delta)
                };
                chunk_stream.message_size = message_size;
                chunk_stream.message_type = RtmpMessageType::from_type_id(message_type)?;
            }
            MessageHeaderFormat::F2 => {
                let timestamp_delta = buf.read_u24()?;

                let is_extended = timestamp_delta == 0xFFFFFF;
                chunk_stream.enable_f3_extended_timestamp = is_extended;
                chunk_stream.timestamp_delta = if is_extended {
                    RtmpTimestamp::from_millis(buf.read_u32()?)
                } else {
                    RtmpTimestamp::from_millis(timestamp_delta)
                };
            }
            MessageHeaderFormat::F3 => {
                if chunk_stream.enable_f3_extended_timestamp {
                    chunk_stream.timestamp_delta = RtmpTimestamp::from_millis(buf.read_u32()?);
                }
            }
        }
        Ok(())
    }

    fn decode_chunk_basic_header(
        &self,
        buf: &mut &[u8],
    ) -> Result<(MessageHeaderFormat, RtmpChunkStreamId), Error> {
        let first_byte = buf.read_u8()?;

        let format = match first_byte >> 6 {
            0 => MessageHeaderFormat::F0,
            1 => MessageHeaderFormat::F1,
            2 => MessageHeaderFormat::F2,
            3 => MessageHeaderFormat::F3,
            _ => unreachable!(),
        };

        let id_bits = first_byte & 0b0011_1111;
        let chunk_stream_id = if id_bits == 0 {
            buf.read_u8()? as u32 + 64
        } else if id_bits == 1 {
            // リトルエンディアンとして扱うために swap_bytes() を呼んでいる
            (buf.read_u16()?).swap_bytes() as u32 + 64
        } else {
            id_bits as u32
        };

        // RtmpChunkStreamId::new() が許可する範囲は 2..=65599 だけど、
        // 上の if 分岐の条件的に chunk_stream_id の最小値は 2 で、かつ、
        // 最大値は 0xFFFF + 64 = 65599 なので、以下の new() は常に成功する
        let chunk_stream_id = RtmpChunkStreamId::new(chunk_stream_id).expect("bug");

        Ok((format, chunk_stream_id))
    }
}

#[derive(Debug, Clone)]
struct RtmpChunkStream {
    timestamp: RtmpTimestamp,
    timestamp_delta: RtmpTimestamp, // [NOTE] ここでは負数は扱わないので RtmpTimestampDelta は使わない
    message_type: RtmpMessageType,
    message_stream_id: RtmpMessageStreamId,
    message_size: usize,
    acc_payload: Vec<u8>,
    enable_f3_extended_timestamp: bool,
}

impl RtmpChunkStream {
    fn new() -> Self {
        // 最初はテキトウな値を設定しておく（すぐに受信データによって上書きされるので何でもいい）
        Self {
            timestamp: RtmpTimestamp::ZERO,
            timestamp_delta: RtmpTimestamp::ZERO,
            message_type: RtmpMessageType::SetChunkSize,
            message_stream_id: RtmpMessageStreamId::new(0),
            message_size: 0,
            acc_payload: Vec::new(),
            enable_f3_extended_timestamp: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::rtmp_chunk_encoder::RtmpChunkEncoder;

    #[test]
    fn decode_basic_chunk() {
        let input_chunk = input_chunk();
        let encoded = encode_chunks(core::slice::from_ref(&input_chunk));
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![input_chunk]);
    }

    #[test]
    fn decode_consecutive_chunks_fmt0_to_fmt1() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            message_type: RtmpMessageType::CommandAmf0,
            ..chunk0.clone()
        };
        let encoded = encode_chunks(&[chunk0.clone(), chunk1.clone()]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1]);
    }

    #[test]
    fn decode_consecutive_chunks_fmt0_to_fmt2() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(700),
            ..chunk0.clone()
        };
        let encoded = encode_chunks(&[chunk0.clone(), chunk1.clone()]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1]);
    }

    #[test]
    fn decode_consecutive_chunks_fmt0_to_fmt2_to_fmt3_to_fmt3() {
        let chunk = input_chunk();
        let encoded = encode_chunks(&[chunk.clone(), chunk.clone(), chunk.clone(), chunk.clone()]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(
            decoded,
            vec![chunk.clone(), chunk.clone(), chunk.clone(), chunk]
        );
    }

    #[test]
    fn decode_consecutive_chunks_fmt0_to_fmt3_to_fmt3() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            timestamp: chunk0.timestamp.wrapping_add(chunk0.timestamp),
            ..chunk0.clone()
        };
        let chunk2 = RtmpChunk {
            timestamp: chunk1.timestamp.wrapping_add(chunk0.timestamp),
            ..chunk1.clone()
        };
        let encoded = encode_chunks(&[chunk0.clone(), chunk1.clone(), chunk2.clone()]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1, chunk2]);
    }

    #[test]
    fn decode_consecutive_chunks_fmt0_to_fmt1_to_fmt2_to_fmt3() {
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
        let encoded = encode_chunks(&[
            chunk0.clone(),
            chunk1.clone(),
            chunk2.clone(),
            chunk3.clone(),
        ]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1, chunk2, chunk3]);
    }

    #[test]
    fn decode_consecutive_chunks_fmt0_to_fmt2_to_fmt0_to_fmt1() {
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
        let encoded = encode_chunks(&[
            chunk0.clone(),
            chunk1.clone(),
            chunk2.clone(),
            chunk3.clone(),
        ]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1, chunk2, chunk3]);
    }

    #[test]
    fn decode_extended_timestamp_fmt0_to_fmt3() {
        let timestamp = 0x12345678u32;
        let chunk0 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp),
            ..input_chunk()
        };
        let chunk1 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(timestamp.wrapping_add(timestamp)),
            ..chunk0.clone()
        };
        let encoded = encode_chunks(&[chunk0.clone(), chunk1.clone()]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1]);
    }

    #[test]
    fn decode_extended_timestamp_boundary_fmt0_to_fmt3() {
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
        let encoded0 = encode_chunks(&[chunk0_0.clone(), chunk0_1.clone()]);
        let decoded0 = decode_chunks(&encoded0);
        assert_eq!(decoded0, vec![chunk0_0, chunk0_1]);

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
        let encoded1 = encode_chunks(&[chunk1_0.clone(), chunk1_1.clone()]);
        let decoded1 = decode_chunks(&encoded1);
        assert_eq!(decoded1, vec![chunk1_0, chunk1_1]);

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
        let encoded2 = encode_chunks(&[chunk2_0.clone(), chunk2_1.clone()]);
        let decoded2 = decode_chunks(&encoded2);
        assert_eq!(decoded2, vec![chunk2_0, chunk2_1]);
    }

    #[test]
    fn decode_extended_timestamp_fmt0_to_fmt1_to_fmt3() {
        let timestamp_base = 300u32;
        let timestamp_delta = 0x12345678u32;

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

        let encoded = encode_chunks(&[chunk0.clone(), chunk1.clone(), chunk2.clone()]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1, chunk2]);
    }

    #[test]
    fn decode_extended_timestamp_boundary_fmt0_to_fmt1_to_fmt3() {
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
        let encoded0 = encode_chunks(&[chunk0_0.clone(), chunk0_1.clone(), chunk0_2.clone()]);
        let decoded0 = decode_chunks(&encoded0);
        assert_eq!(decoded0, vec![chunk0_0, chunk0_1, chunk0_2]);

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
        let encoded1 = encode_chunks(&[chunk1_0.clone(), chunk1_1.clone(), chunk1_2.clone()]);
        let decoded1 = decode_chunks(&encoded1);
        assert_eq!(decoded1, vec![chunk1_0, chunk1_1, chunk1_2]);

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
        let encoded2 = encode_chunks(&[chunk2_0.clone(), chunk2_1.clone(), chunk2_2.clone()]);
        let decoded2 = decode_chunks(&encoded2);
        assert_eq!(decoded2, vec![chunk2_0, chunk2_1, chunk2_2]);
    }

    #[test]
    fn decode_multiple_chunk_stream_ids() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(10).unwrap(),
            ..chunk0.clone()
        };
        let chunk2 = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(100).unwrap(),
            ..chunk0.clone()
        };
        let encoded = encode_chunks(&[chunk0.clone(), chunk1.clone(), chunk2.clone()]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1, chunk2]);
    }

    #[test]
    fn decode_chunk_size_change() {
        let chunk0 = RtmpChunk {
            payload: vec![0u8; 512],
            ..input_chunk()
        };
        let chunk1 = RtmpChunk {
            message_type: RtmpMessageType::CommandAmf0,
            ..chunk0.clone()
        };
        let chunk2 = RtmpChunk {
            timestamp: RtmpTimestamp::from_millis(6789),
            ..chunk1.clone()
        };
        let chunk3 = chunk2.clone();

        // Encode with two different chunk sizes
        let mut encoder = RtmpChunkEncoder::default();
        let mut buf = Vec::new();

        encoder.encode(&mut buf, &chunk0);
        encoder.encode(&mut buf, &chunk1);

        encoder.set_chunk_size(RtmpChunkSize::new(256).unwrap());

        encoder.encode(&mut buf, &chunk2);
        encoder.encode(&mut buf, &chunk3);

        let encoded = &buf[..];

        // Decode with matching chunk size changes
        let mut decoder = RtmpChunkDecoder::default();
        let mut remaining = encoded;
        let mut decoded = vec![];

        while decoded.len() < 2 {
            let (size, chunk) = decoder.decode(remaining).expect("bug");
            remaining = &remaining[size..];
            if let Some(chunk) = chunk {
                decoded.push(chunk);
            }
        }

        decoder.set_chunk_size(RtmpChunkSize::new(256).unwrap());

        while decoded.len() < 4 {
            let (size, chunk) = decoder.decode(remaining).expect("bug");
            remaining = &remaining[size..];
            if let Some(chunk) = chunk {
                decoded.push(chunk);
            }
        }

        assert_eq!(decoded, vec![chunk0, chunk1, chunk2, chunk3]);
    }

    #[test]
    fn decode_chunk_id_over_320() {
        let chunk0 = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(320).expect("infallible"),
            ..input_chunk()
        };
        let chunk1 = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(320).expect("infallible"),
            message_type: RtmpMessageType::CommandAmf0,
            ..chunk0.clone()
        };
        let encoded = encode_chunks(&[chunk0.clone(), chunk1.clone()]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1]);
    }

    #[test]
    fn decode_chunk_id_65599() {
        let chunk0 = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(65599).expect("infallible"),
            ..input_chunk()
        };
        let chunk1 = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(65599).expect("infallible"),
            message_type: RtmpMessageType::Video,
            ..chunk0.clone()
        };
        let encoded = encode_chunks(&[chunk0.clone(), chunk1.clone()]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1]);
    }

    #[test]
    fn decode_multiple_chunk_stream_ids_with_large_ids() {
        let chunk0 = input_chunk();
        let chunk1 = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(320).expect("infallible"),
            ..chunk0.clone()
        };
        let chunk2 = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(65599).expect("infallible"),
            ..chunk0.clone()
        };
        let chunk3 = RtmpChunk {
            chunk_stream_id: RtmpChunkStreamId::new(4).expect("infallible"),
            ..chunk0.clone()
        };
        let encoded = encode_chunks(&[
            chunk0.clone(),
            chunk1.clone(),
            chunk2.clone(),
            chunk3.clone(),
        ]);
        let decoded = decode_chunks(&encoded);
        assert_eq!(decoded, vec![chunk0, chunk1, chunk2, chunk3]);
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

    fn encode_chunks(chunks: &[RtmpChunk]) -> Vec<u8> {
        let mut encoder = RtmpChunkEncoder::default();
        let mut buf = Vec::new();

        for chunk in chunks {
            encoder.encode(&mut buf, chunk);
        }

        buf
    }

    fn decode_chunks(encoded: &[u8]) -> Vec<RtmpChunk> {
        let mut decoder = RtmpChunkDecoder::default();
        let mut remaining = encoded;
        let mut chunks = vec![];

        while let Ok((size, Some(chunk))) = decoder.decode(remaining) {
            chunks.push(chunk);
            remaining = &remaining[size..];
        }

        chunks
    }
}
