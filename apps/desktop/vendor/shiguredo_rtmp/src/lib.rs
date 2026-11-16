//! RTMP (Real Time Messaging Protocol) の Sans I/O 実装を提供するライブラリ
#![no_std]
// `missing_docs` の警告は公開 API のみに効かせたい。
// 下部の `tests` モジュールは PBT / Fuzzing 用に内部アイテムを大量に再エクスポートするので
// 通常ビルドで `warn(missing_docs)` を全体に効かせると内部 API まで警告対象になってしまう。
// そのためドキュメント生成時 (`cfg(doc)`) に限定して警告を有効化する。
// 併せて `tests` モジュール自体を `cfg(not(doc))` で除外し、公開 API だけをチェック対象にする。
#![cfg_attr(doc, warn(missing_docs))]
#[macro_use]
extern crate alloc;
mod amf;
mod amf0;
mod amf3;
mod bytes;
mod error;
mod flv;
mod media;
mod rtmp_chunk;
mod rtmp_chunk_decoder;
mod rtmp_chunk_encoder;
mod rtmp_client_connection;
mod rtmp_command;
mod rtmp_connection;
mod rtmp_handshake;
mod rtmp_message;
mod rtmp_message_decoder;
mod rtmp_message_encoder;
mod rtmp_server_connection;
mod rtmp_timestamp;
mod rtmp_url;
mod rtmp_user_control_event;

pub use error::{Error, ErrorKind};
pub use media::{
    AudioFormat, AudioFrame, AudioSampleRate, AvcPacketType, AvcSequenceHeader, MediaFrame,
    VideoCodec, VideoFrame, VideoFrameType,
};
pub use rtmp_client_connection::{RtmpPlayClientConnection, RtmpPublishClientConnection};
pub use rtmp_connection::{RtmpConnectionEvent, RtmpConnectionState};
pub use rtmp_server_connection::RtmpServerConnection;
pub use rtmp_timestamp::{RtmpTimestamp, RtmpTimestampDelta};
pub use rtmp_url::RtmpUrl;

// PBT / Fuzzing から内部アイテムを参照するために用意した再エクスポート用モジュール。
// クレート外の通常利用からは見せたくないので `#[doc(hidden)]` でドキュメントから除外し、
// さらにドキュメント生成時 (`cfg(doc)`) には丸ごと除外して公開 API 以外を rustdoc に露出させない。
#[cfg(not(doc))]
#[doc(hidden)]
pub mod tests {
    pub use crate::amf::*;
    pub use crate::amf0::*;
    pub use crate::amf3::*;
    pub use crate::flv::*;
    pub use crate::media::*;
    pub use crate::rtmp_chunk::*;
    pub use crate::rtmp_chunk_decoder::*;
    pub use crate::rtmp_chunk_encoder::*;
    pub use crate::rtmp_client_connection::*;
    pub use crate::rtmp_command::TransactionId;
    pub use crate::rtmp_command::*;
    pub use crate::rtmp_connection::*;
    pub use crate::rtmp_handshake::*;
    pub use crate::rtmp_message::*;
    pub use crate::rtmp_message_decoder::*;
    pub use crate::rtmp_message_encoder::*;
    pub use crate::rtmp_server_connection::*;
    pub use crate::rtmp_timestamp::*;
    pub use crate::rtmp_url::*;
    pub use crate::rtmp_user_control_event::*;
}
