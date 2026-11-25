use alloc::vec::Vec;

use crate::bytes::BytesReader;
use crate::error::Error;
use crate::rtmp_message::RtmpMessageStreamId;
use crate::rtmp_timestamp::RtmpTimestamp;

const EVENT_STREAM_BEGIN: u16 = 0;
const EVENT_STREAM_EOF: u16 = 1;
const EVENT_STREAM_DRY: u16 = 2;
const EVENT_SET_BUFFER_LENGTH: u16 = 3;
const EVENT_STREAM_IS_RECORDED: u16 = 4;
const EVENT_PING_REQUEST: u16 = 6;
const EVENT_PING_RESPONSE: u16 = 7;
const EVENT_BUFFER_EMPTY: u16 = 31;
const EVENT_BUFFER_READY: u16 = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtmpUserControlEvent {
    StreamBegin {
        stream_id: RtmpMessageStreamId,
    },
    StreamEof {
        stream_id: RtmpMessageStreamId,
    },
    StreamDry {
        stream_id: RtmpMessageStreamId,
    },
    SetBufferLength {
        stream_id: RtmpMessageStreamId,
        length: u32,
    },
    StreamIsRecorded {
        stream_id: RtmpMessageStreamId,
    },
    PingRequest {
        timestamp: RtmpTimestamp,
    },
    PingResponse {
        timestamp: RtmpTimestamp,
    },
    BufferEmpty {
        stream_id: RtmpMessageStreamId,
    },
    BufferReady {
        stream_id: RtmpMessageStreamId,
    },
}

impl RtmpUserControlEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::StreamBegin { .. } => "StreamBegin",
            Self::StreamEof { .. } => "StreamEof",
            Self::StreamDry { .. } => "StreamDry",
            Self::SetBufferLength { .. } => "SetBufferLength",
            Self::StreamIsRecorded { .. } => "StreamIsRecorded",
            Self::PingRequest { .. } => "PingRequest",
            Self::PingResponse { .. } => "PingResponse",
            Self::BufferEmpty { .. } => "BufferEmpty",
            Self::BufferReady { .. } => "BufferReady",
        }
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            Self::StreamBegin { stream_id } => {
                buf.extend_from_slice(&EVENT_STREAM_BEGIN.to_be_bytes());
                buf.extend_from_slice(&stream_id.get().to_be_bytes());
            }
            Self::StreamEof { stream_id } => {
                buf.extend_from_slice(&EVENT_STREAM_EOF.to_be_bytes());
                buf.extend_from_slice(&stream_id.get().to_be_bytes());
            }
            Self::StreamDry { stream_id } => {
                buf.extend_from_slice(&EVENT_STREAM_DRY.to_be_bytes());
                buf.extend_from_slice(&stream_id.get().to_be_bytes());
            }
            Self::SetBufferLength { stream_id, length } => {
                buf.extend_from_slice(&EVENT_SET_BUFFER_LENGTH.to_be_bytes());
                buf.extend_from_slice(&stream_id.get().to_be_bytes());
                buf.extend_from_slice(&length.to_be_bytes());
            }
            Self::StreamIsRecorded { stream_id } => {
                buf.extend_from_slice(&EVENT_STREAM_IS_RECORDED.to_be_bytes());
                buf.extend_from_slice(&stream_id.get().to_be_bytes());
            }
            Self::PingRequest { timestamp } => {
                buf.extend_from_slice(&EVENT_PING_REQUEST.to_be_bytes());
                buf.extend_from_slice(&timestamp.as_millis().to_be_bytes());
            }
            Self::PingResponse { timestamp } => {
                buf.extend_from_slice(&EVENT_PING_RESPONSE.to_be_bytes());
                buf.extend_from_slice(&timestamp.as_millis().to_be_bytes());
            }
            Self::BufferEmpty { stream_id } => {
                buf.extend_from_slice(&EVENT_BUFFER_EMPTY.to_be_bytes());
                buf.extend_from_slice(&stream_id.get().to_be_bytes());
            }
            Self::BufferReady { stream_id } => {
                buf.extend_from_slice(&EVENT_BUFFER_READY.to_be_bytes());
                buf.extend_from_slice(&stream_id.get().to_be_bytes());
            }
        }
    }

    pub fn decode(mut buf: &[u8]) -> Result<Self, Error> {
        let event_type = buf.read_u16()?;
        let event = match event_type {
            EVENT_STREAM_BEGIN => {
                let stream_id = RtmpMessageStreamId::new(buf.read_u32()?);
                Self::StreamBegin { stream_id }
            }
            EVENT_STREAM_EOF => {
                let stream_id = RtmpMessageStreamId::new(buf.read_u32()?);
                Self::StreamEof { stream_id }
            }
            EVENT_STREAM_DRY => {
                let stream_id = RtmpMessageStreamId::new(buf.read_u32()?);
                Self::StreamDry { stream_id }
            }
            EVENT_SET_BUFFER_LENGTH => {
                let stream_id = RtmpMessageStreamId::new(buf.read_u32()?);
                let length = buf.read_u32()?;
                Self::SetBufferLength { stream_id, length }
            }
            EVENT_STREAM_IS_RECORDED => {
                let stream_id = RtmpMessageStreamId::new(buf.read_u32()?);
                Self::StreamIsRecorded { stream_id }
            }
            EVENT_PING_REQUEST => {
                let timestamp = RtmpTimestamp::from_millis(buf.read_u32()?);
                Self::PingRequest { timestamp }
            }
            EVENT_PING_RESPONSE => {
                let timestamp = RtmpTimestamp::from_millis(buf.read_u32()?);
                Self::PingResponse { timestamp }
            }
            EVENT_BUFFER_EMPTY => {
                let stream_id = RtmpMessageStreamId::new(buf.read_u32()?);
                Self::BufferEmpty { stream_id }
            }
            EVENT_BUFFER_READY => {
                let stream_id = RtmpMessageStreamId::new(buf.read_u32()?);
                Self::BufferReady { stream_id }
            }
            _ => {
                return Err(Error::invalid_data(format!(
                    "unknown user control event type: {event_type}"
                )));
            }
        };
        Ok(event)
    }
}
