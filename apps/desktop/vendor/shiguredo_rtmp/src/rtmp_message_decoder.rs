use alloc::borrow::ToOwned;
use alloc::vec::Vec;

use crate::amf::{AmfValue, AmfVersion};
use crate::bytes::{Buf, BytesReader};
use crate::error::{Error, ErrorKind};
use crate::rtmp_chunk::{RtmpChunk, RtmpChunkSize, RtmpChunkStreamId};
use crate::rtmp_chunk_decoder::RtmpChunkDecoder;
use crate::rtmp_command::TransactionId;
use crate::rtmp_message::{
    RtmpMessage, RtmpMessageHeader, RtmpMessageType, SetPeerBandwidthLimitType,
};
use crate::rtmp_user_control_event::RtmpUserControlEvent;

#[derive(Debug, Default)]
pub struct RtmpMessageDecoder {
    chunk_decoder: RtmpChunkDecoder,
    buf: Buf,
}

impl RtmpMessageDecoder {
    pub fn feed_buf(&mut self, buf: &[u8]) {
        self.buf.feed(buf);
    }

    pub fn decode(&mut self) -> Result<Option<RtmpMessage>, Error> {
        loop {
            let chunk = match self.chunk_decoder.decode(self.buf.get()) {
                Ok((size, maybe_chunk)) => {
                    self.buf.advance(size);

                    if let Some(chunk) = maybe_chunk {
                        chunk
                    } else {
                        continue;
                    }
                }
                Err(e) if e.kind == ErrorKind::InsufficientBuffer => {
                    // まだ先頭のチャンクをデコードするためのデータが溜まっていない
                    // バッファ管理は内部で行っているので、呼び出し元には `Ok(None)` を返す
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };

            let message = self.chunk_to_message(chunk)?;
            match &message {
                RtmpMessage::SetChunkSize { size, .. } => self.chunk_decoder.set_chunk_size(*size),
                RtmpMessage::Abort {
                    chunk_stream_id, ..
                } => self.chunk_decoder.reset_chunk_stream(*chunk_stream_id),
                _ => {}
            }

            return Ok(Some(message));
        }
    }

    fn chunk_to_message(&self, chunk: RtmpChunk) -> Result<RtmpMessage, Error> {
        let header = RtmpMessageHeader {
            stream_id: chunk.message_stream_id,
            timestamp: chunk.timestamp,
        };

        let mut payload = chunk.payload.as_slice();

        let message = match chunk.message_type {
            RtmpMessageType::SetChunkSize => {
                let size = payload.read_u32()? as usize;
                let size = RtmpChunkSize::new(size)
                    .ok_or_else(|| Error::invalid_data(format!("invalid chunk size: {size}")))?;
                RtmpMessage::SetChunkSize { header, size }
            }
            RtmpMessageType::Abort => {
                let chunk_stream_id = RtmpChunkStreamId::new(payload.read_u32()?)
                    .ok_or_else(|| Error::invalid_data("invalid chunk stream ID"))?;
                RtmpMessage::Abort {
                    header,
                    chunk_stream_id,
                }
            }
            RtmpMessageType::Ack => {
                let sequence_number = payload.read_u32()?;
                RtmpMessage::Ack {
                    header,
                    sequence_number,
                }
            }
            RtmpMessageType::WinAckSize => {
                let size = payload.read_u32()?;
                RtmpMessage::WinAckSize { header, size }
            }
            RtmpMessageType::SetPeerBandwidth => {
                let size = payload.read_u32()?;
                let limit_type = match payload.read_u8()? {
                    0 => SetPeerBandwidthLimitType::Hard,
                    1 => SetPeerBandwidthLimitType::Soft,
                    2 => SetPeerBandwidthLimitType::Dynamic,
                    t => {
                        return Err(Error::invalid_data(format!("invalid limit type: {t}")));
                    }
                };
                RtmpMessage::SetPeerBandwidth {
                    header,
                    size,
                    limit_type,
                }
            }
            RtmpMessageType::UserControl => {
                let event = RtmpUserControlEvent::decode(payload)?;
                RtmpMessage::UserControl { header, event }
            }
            RtmpMessageType::Audio => {
                let frame = crate::flv::decode_audio_frame(&chunk.payload, header.timestamp)?;
                RtmpMessage::Audio { header, frame }
            }
            RtmpMessageType::Video => {
                let frame = crate::flv::decode_video_frame(&chunk.payload, header.timestamp)?;
                RtmpMessage::Video { header, frame }
            }
            RtmpMessageType::DataAmf0 => {
                let values = self.decode_amf_values(AmfVersion::Amf0, payload)?;
                RtmpMessage::Data {
                    header,
                    amf_version: AmfVersion::Amf0,
                    values,
                }
            }
            RtmpMessageType::DataAmf3 => {
                let values = self.decode_amf_values(AmfVersion::Amf3, payload)?;
                RtmpMessage::Data {
                    header,
                    amf_version: AmfVersion::Amf3,
                    values,
                }
            }
            RtmpMessageType::CommandAmf0 => {
                self.decode_command(AmfVersion::Amf0, header, chunk.payload.as_slice())?
            }
            RtmpMessageType::CommandAmf3 => {
                self.decode_command(AmfVersion::Amf3, header, chunk.payload.as_slice())?
            }
        };

        Ok(message)
    }

    fn decode_command(
        &self,
        mut amf_version: AmfVersion,
        header: RtmpMessageHeader,
        payload: &[u8],
    ) -> Result<RtmpMessage, Error> {
        let mut buf = payload;

        // AMF3 が 0x00 で始まる場合、AMF0 として扱う特別なケースに対応
        if amf_version == AmfVersion::Amf3 && buf.first() == Some(&0) {
            buf = &buf[1..];
            amf_version = AmfVersion::Amf0;
        }

        // コマンド名をデコード
        let (size, name) = AmfValue::decode(buf, amf_version)?;
        let name = name.expect_str()?.to_owned();
        buf = &buf[size..];

        // トランザクション ID をデコード
        let (size, transaction_id) = AmfValue::decode(buf, amf_version)?;
        let transaction_id = TransactionId::from_f64(transaction_id.expect_number()?);
        buf = &buf[size..];

        // コマンドオブジェクトをデコード
        let (size, object) = AmfValue::decode(buf, amf_version)?;
        buf = &buf[size..];

        // オプショナルな引数をデコード
        let args = self.decode_amf_values(amf_version, buf)?;

        Ok(RtmpMessage::Command {
            header,
            amf_version,
            name,
            transaction_id,
            object,
            args,
        })
    }

    fn decode_amf_values(
        &self,
        amf_version: AmfVersion,
        mut buf: &[u8],
    ) -> Result<Vec<AmfValue>, Error> {
        let mut values = Vec::new();

        while !buf.is_empty() {
            let (size, value) = AmfValue::decode(buf, amf_version)?;
            buf = &buf[size..];
            values.push(value);
        }

        Ok(values)
    }
}
