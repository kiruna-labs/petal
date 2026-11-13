use alloc::borrow::ToOwned;
use alloc::collections::VecDeque;
use alloc::string::String;
use core::fmt;

use crate::bytes::Buf;
use crate::error::Error;
use crate::media::{AudioFrame, VideoFrame};
use crate::rtmp_chunk::{RtmpChunkSize, RtmpChunkStreamId};
use crate::rtmp_command::RtmpCommand;
use crate::rtmp_message::RtmpMessage;
use crate::rtmp_message_decoder::RtmpMessageDecoder;
use crate::rtmp_message_encoder::RtmpMessageEncoder;
#[cfg(doc)]
use crate::rtmp_server_connection::RtmpServerConnection;
use crate::rtmp_user_control_event::RtmpUserControlEvent;

/// RTMP 接続のオプション
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtmpConnectionOptions {
    /// チャンクサイズ（デフォルト: 4096）
    pub chunk_size: RtmpChunkSize,

    /// 確認応答ウィンドウサイズ（デフォルト: 5MB）
    pub ack_window_size: u32,
}

impl Default for RtmpConnectionOptions {
    fn default() -> Self {
        Self {
            chunk_size: RtmpChunkSize::saturating_new(4096),
            ack_window_size: 5_000_000, // およそ 5MB のデータを受信する度に ack を送信する
        }
    }
}

/// RTMP コネクションで発生するイベント
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtmpConnectionEvent {
    /// クライアントから配信コマンドを受信した
    PublishRequested {
        /// アプリケーション名
        app: String,

        /// 接続 URL
        tc_url: String,

        /// ストリーム名
        stream_name: String,
    },

    /// クライアントから再生リクエストを受信した
    PlayRequested {
        /// アプリケーション名
        app: String,

        /// 接続 URL
        tc_url: String,

        /// ストリーム名
        stream_name: String,
    },

    /// 音声フレームを受信した
    AudioReceived(AudioFrame),

    /// 映像フレームを受信した
    VideoReceived(VideoFrame),

    /// コネクションの状態が変更された
    StateChanged(RtmpConnectionState),

    /// 相手から切断された
    DisconnectedByPeer {
        /// 切断理由
        reason: String,
    },

    /// RTMP のコマンドが無視された（デバッグ用イベント）
    ///
    /// サーバーおよびクライアントコネクションは明示的なハンドリングが不要なコマンドを受信した場合には
    /// 無視をするので、その通知 （通常は利用者がこのイベントを明示的にハンドリングする必要はない）
    CommandIgnored {
        /// コマンド名
        name: String,

        /// 詳細情報
        detail: String,
    },

    /// RTMP のメッセージが無視された（デバッグ用イベント）
    ///
    /// サーバーおよびクライアントコネクションは明示的なハンドリングが不要なイベントを受信した場合には
    /// 無視をするので、その通知 （通常は利用者がこのイベントを明示的にハンドリングする必要はない）
    MessageIgnored {
        /// メッセージ名
        name: String,

        /// 詳細情報
        detail: String,
    },

    /// RTMP のユーザーコントロールイベントが無視された（デバッグ用イベント）
    ///
    /// サーバーおよびクライアントコネクションは明示的なハンドリングが不要なユーザーコントロールイベントを受信した場合には
    /// 無視をするので、その通知（通常は利用者がこのイベントを明示的にハンドリングする必要はない）
    UserControlEventIgnored {
        /// イベント名
        name: String,

        /// 詳細情報
        detail: String,
    },
}

impl RtmpConnectionEvent {
    pub(crate) fn command_ignored(command: &RtmpCommand) -> Self {
        Self::CommandIgnored {
            name: command.name().to_owned(),
            detail: format!("{command:?}"),
        }
    }

    pub(crate) fn message_ignored(message: &RtmpMessage) -> Self {
        Self::MessageIgnored {
            name: format!("{:?}", message.message_type()),
            detail: format!("{message:?}"),
        }
    }

    pub(crate) fn user_control_event_ignored(event: &RtmpUserControlEvent) -> Self {
        Self::UserControlEventIgnored {
            name: event.name().to_owned(),
            detail: format!("{event:?}"),
        }
    }
}

/// RTMP コネクションの状態
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RtmpConnectionState {
    /// ハンドシェイク中
    #[default]
    Handshaking,

    /// 接続中
    Connecting,

    /// 接続済み
    Connected,

    /// メディアストリーム作成済み
    MediaStreamCreated,

    /// 配信待機中
    ///
    /// [`RtmpServerConnection::accept()`] が呼ばれると `Publishing` 状態に遷移する
    PublishPending,

    /// 配信中
    ///
    /// [`RtmpServerConnection::accept()`] が呼ばれると `Playing` 状態に遷移する
    Publishing,

    /// 再生待機中
    PlayPending,

    /// 再生中
    Playing,

    /// 切断中
    ///
    /// この状態に遷移したら「利用側はその TCP 接続を切断すべき」であることを意味する
    Disconnecting,
}

impl RtmpConnectionState {
    #[track_caller]
    pub(crate) fn expect(self, expected: Self) -> Result<(), Error> {
        if self == expected {
            Ok(())
        } else {
            Err(Error::invalid_state(format!(
                "expected connection state {expected}, but current state is {self}"
            )))
        }
    }
}

impl fmt::Display for RtmpConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RtmpConnectionState::Handshaking => write!(f, "HANDSHAKING"),
            RtmpConnectionState::Connecting => write!(f, "CONNECTING"),
            RtmpConnectionState::Connected => write!(f, "CONNECTED"),
            RtmpConnectionState::MediaStreamCreated => write!(f, "MEDIA_STREAM_CREATED"),
            RtmpConnectionState::PublishPending => write!(f, "PUBLISH_PENDING"),
            RtmpConnectionState::Publishing => write!(f, "PUBLISHING"),
            RtmpConnectionState::PlayPending => write!(f, "PLAY_PENDING"),
            RtmpConnectionState::Playing => write!(f, "PLAYING"),
            RtmpConnectionState::Disconnecting => write!(f, "DISCONNECTING"),
        }
    }
}

#[derive(Debug, Default)]
pub struct RtmpMessageChannel {
    decoder: RtmpMessageDecoder,
    encoder: RtmpMessageEncoder,
    decoded_messages: VecDeque<RtmpMessage>,
    send_buf: Buf,

    // ACK ハンドリング用のフィールド群
    // 32 bit で表現しているのは RTMP 仕様に合わせているため

    // [NOTE]
    // 受信側は last_ack_sent が total_bytes_received と peer_ack_window_size 以上乖離する前に次の ack を送る必要がある
    // （送信側の場合はその逆）
    peer_ack_window_size: u32,
    total_bytes_received: u32, // 累計受信バイト数
    last_ack_sent: u32,        // 最後に ack を送ったバイト地点

    local_ack_window_size: u32,
    total_bytes_sent: u32,  // 累計送信バイト数
    last_ack_received: u32, // 最後に ack を受け取ったバイト地点
}

impl RtmpMessageChannel {
    pub fn feed_recv_buf(&mut self, buf: &[u8]) -> Result<Option<u32>, Error> {
        self.decoder.feed_buf(buf);
        self.total_bytes_received = self.total_bytes_received.wrapping_add(buf.len() as u32);

        while let Some(message) = self.decoder.decode()? {
            self.decoded_messages.push_back(message);
        }

        // ACK を送る必要があるなら送るように呼び出し元に指示する
        //
        // 念のために peer_ack_window_size の半分を超過したら送るようにする
        //
        // 仕様的には peer_ack_window_size 分だけ受信したら送れば十分そうだけど、
        // RTMP は仕様が不明瞭なところがあり、クライアントが厳密にそれに準拠している保証もないため、
        // 安全側に倒して、早めに送信するようにしている
        // （この部分のオーバーヘッドもほぼ無視できる範囲のため）
        let unacked_bytes = self.total_bytes_received.wrapping_sub(self.last_ack_sent);
        if unacked_bytes > self.peer_ack_window_size / 2 {
            self.last_ack_sent = self.total_bytes_received;
            Ok(Some(self.total_bytes_received))
        } else {
            Ok(None)
        }
    }

    pub fn send_buf(&self) -> &[u8] {
        self.send_buf.get()
    }

    pub fn advance_send_buf(&mut self, n: usize) -> bool {
        self.total_bytes_sent = self.total_bytes_sent.wrapping_add(n as u32);
        self.send_buf.advance(n);

        let unacked_bytes = self.total_bytes_sent.wrapping_sub(self.last_ack_received);
        if unacked_bytes > self.local_ack_window_size * 2 {
            // 必要な間隔で相手から ACK が送られて来ていない（* 2 は念のために若干のバッファを持たせている）
            //
            // 呼び出し元で、送信を止めるなり、切断するなりの対処が必要
            false
        } else {
            true
        }
    }

    pub fn next_recv_message(&mut self) -> Option<RtmpMessage> {
        self.decoded_messages.pop_front()
    }

    pub fn feed_send_message(&mut self, message: RtmpMessage) {
        let chunk_stream_id = RtmpChunkStreamId::from_message_stream_id(message.header().stream_id);
        self.encoder
            .encode(self.send_buf.inner_mut(), chunk_stream_id, message);
    }

    pub fn set_local_ack_window_size(&mut self, size: u32) {
        self.local_ack_window_size = size;
    }

    pub fn set_peer_ack_window_size(&mut self, size: u32) {
        self.peer_ack_window_size = size;
    }

    // 相手から送られてきた ACK の情報で更新する
    pub fn notify_ack_received(&mut self, acked_bytes: u32) {
        self.last_ack_received = acked_bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec::Vec;

    use crate::media::{
        AudioFormat, AudioFrame, AudioSampleRate, VideoCodec, VideoFrame, VideoFrameType,
    };
    use crate::rtmp_message::{RtmpMessageHeader, RtmpMessageStreamId};
    use crate::rtmp_message_decoder::RtmpMessageDecoder;
    use crate::rtmp_timestamp::RtmpTimestamp;
    use crate::rtmp_timestamp::RtmpTimestampDelta;

    #[test]
    fn message_channel_send_roundtrip_regression_pbt() {
        // PBT で観測した最新の最小ケースを単体テスト化
        // proptest-regressions: e00fb29aad1ea669d11ef622d70b499d650a9750e1b912e1eefb257471e55e60
        let messages = vec![
            RtmpMessage::SetChunkSize {
                header: RtmpMessageHeader {
                    stream_id: RtmpMessageStreamId::PCM,
                    timestamp: RtmpTimestamp::ZERO,
                },
                size: RtmpChunkSize::new(1).unwrap(),
            },
            RtmpMessage::Video {
                header: RtmpMessageHeader {
                    stream_id: RtmpMessageStreamId::new(65_597),
                    timestamp: RtmpTimestamp::from_millis(1),
                },
                frame: VideoFrame {
                    timestamp: RtmpTimestamp::from_millis(1),
                    composition_timestamp_offset: RtmpTimestampDelta::ZERO,
                    frame_type: VideoFrameType::KeyFrame,
                    codec: VideoCodec::Jpeg,
                    avc_packet_type: None,
                    data: vec![],
                },
            },
            RtmpMessage::Audio {
                header: RtmpMessageHeader {
                    stream_id: RtmpMessageStreamId::new(65_598),
                    timestamp: RtmpTimestamp::from_millis(16_777_215),
                },
                frame: AudioFrame {
                    timestamp: RtmpTimestamp::from_millis(16_777_215),
                    format: AudioFormat::Mp3,
                    sample_rate: AudioSampleRate::Khz5,
                    is_8bit_sample: false,
                    is_stereo: false,
                    is_aac_sequence_header: false,
                    data: vec![0],
                },
            },
        ];

        let mut channel = RtmpMessageChannel::default();
        for message in &messages {
            channel.feed_send_message(message.clone());
        }

        let buf = channel.send_buf().to_vec();
        let mut decoder = RtmpMessageDecoder::default();
        decoder.feed_buf(&buf);

        let mut decoded = Vec::new();
        while let Some(message) = decoder.decode().unwrap() {
            decoded.push(message);
        }

        assert_eq!(decoded, messages);
    }
}
