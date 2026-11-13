use alloc::borrow::ToOwned;
use alloc::collections::VecDeque;
use alloc::string::{String, ToString};

use crate::error::Error;
use crate::media::{AudioFrame, VideoFrame};
use crate::rtmp_command::{RtmpCommand, RtmpConnectCommand, RtmpResultCommand, TransactionId};
use crate::rtmp_connection::{
    RtmpConnectionEvent, RtmpConnectionOptions, RtmpConnectionState, RtmpMessageChannel,
};
use crate::rtmp_handshake::RtmpClientHandshake;
use crate::rtmp_message::{RtmpMessage, RtmpMessageHeader, RtmpMessageStreamId};
use crate::rtmp_timestamp::RtmpTimestamp;
use crate::rtmp_url::RtmpUrl;
use crate::rtmp_user_control_event::RtmpUserControlEvent;

const FLASH_VER: &str = "FMLE/3.0 (compatible; FME/3.0)";

/// RTMP で配信を行うクライアントの接続を管理するための構造体
///
/// なお、この構造体自体は I/O 操作を行わないため、
/// サーバーとの接続やソケットの読み書き操作などは利用側で行う必要がある
#[derive(Debug)]
pub struct RtmpPublishClientConnection {
    inner: RtmpClientConnection,
}

impl RtmpPublishClientConnection {
    /// 新しい RTMP 配信クライアント接続を作成する
    ///
    /// # 引数
    ///
    /// * `url` - 接続先の URL (例: "rtmp://localhost/app/stream")
    pub fn new(url: RtmpUrl) -> Self {
        Self {
            inner: RtmpClientConnection::new(url),
        }
    }

    /// サーバーから受信したデータを処理する
    pub fn feed_recv_buf(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.inner.feed_recv_buf(buf)?;
        if self.inner.state == RtmpConnectionState::MediaStreamCreated {
            self.publish()?;
        }
        Ok(())
    }

    /// サーバーに送信待ちのデータを取得する
    ///
    /// サーバーへのデータ送信が成功した場合には、
    /// その送信バイト数を `RtmpPublishClientConnection::advance_send_buf()` を呼び出して反映する必要がある
    pub fn send_buf(&self) -> &[u8] {
        self.inner.send_buf()
    }

    /// 送信バッファから指定バイト数を送信済みとしてマークする
    pub fn advance_send_buf(&mut self, n: usize) {
        self.inner.advance_send_buf(n)
    }

    /// コネクションの現在の状態を返す
    pub fn state(&self) -> RtmpConnectionState {
        self.inner.state
    }

    /// 音声フレームを送信する（送信バッファに追加する）
    pub fn send_audio(&mut self, frame: AudioFrame) -> Result<(), Error> {
        self.inner.state.expect(RtmpConnectionState::Publishing)?;

        let message = RtmpMessage::Audio {
            header: RtmpMessageHeader {
                stream_id: RtmpMessageStreamId::MEDIA,
                timestamp: frame.timestamp,
            },
            frame,
        };

        self.inner.message_channel.feed_send_message(message);
        Ok(())
    }

    /// 映像フレームを送信する（送信バッファに追加する）
    pub fn send_video(&mut self, frame: VideoFrame) -> Result<(), Error> {
        self.inner.state.expect(RtmpConnectionState::Publishing)?;

        let message = RtmpMessage::Video {
            header: RtmpMessageHeader {
                stream_id: RtmpMessageStreamId::MEDIA,
                timestamp: frame.timestamp,
            },
            frame,
        };

        self.inner.message_channel.feed_send_message(message);
        Ok(())
    }

    /// 次のイベントを取得する
    pub fn next_event(&mut self) -> Option<RtmpConnectionEvent> {
        self.inner.next_event()
    }

    fn publish(&mut self) -> Result<(), Error> {
        self.inner
            .change_state(RtmpConnectionState::PublishPending)?;

        let command = RtmpCommand::Publish(crate::rtmp_command::RtmpPublishCommand {
            transaction_id: self.inner.next_transaction_id,
            stream_name: self.inner.url.stream_name.clone(),
        });

        let message = command.into_message(RtmpMessageHeader {
            stream_id: RtmpMessageStreamId::MEDIA,
            timestamp: RtmpTimestamp::ZERO,
        })?;

        self.inner.message_channel.feed_send_message(message);
        self.inner.next_transaction_id.increment();

        Ok(())
    }
}

/// RTMP で再生を行うクライアントの接続を管理するための構造体
///
/// なお、この構造体自体は I/O 操作を行わないため、
/// サーバーとの接続やソケットの読み書き操作などは利用側で行う必要がある
#[derive(Debug)]
pub struct RtmpPlayClientConnection {
    inner: RtmpClientConnection,
}

impl RtmpPlayClientConnection {
    /// 新しい RTMP 再生クライアント接続を作成する
    ///
    /// # 引数
    ///
    /// * `url` - 接続先の URL (例: "rtmp://localhost/app/stream")
    pub fn new(url: RtmpUrl) -> Self {
        Self {
            inner: RtmpClientConnection::new(url),
        }
    }

    /// サーバーから受信したデータを処理する
    pub fn feed_recv_buf(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.inner.feed_recv_buf(buf)?;
        if self.inner.state == RtmpConnectionState::MediaStreamCreated {
            self.play()?;
        }
        Ok(())
    }

    /// サーバーに送信待ちのデータを取得する
    ///
    /// サーバーへのデータ送信が成功した場合には、
    /// その送信バイト数を `RtmpPlayClientConnection::advance_send_buf()` を呼び出して反映する必要がある
    pub fn send_buf(&self) -> &[u8] {
        self.inner.send_buf()
    }

    /// 送信バッファから指定バイト数を送信済みとしてマークする
    pub fn advance_send_buf(&mut self, n: usize) {
        self.inner.advance_send_buf(n)
    }

    /// コネクションの現在の状態を返す
    pub fn state(&self) -> RtmpConnectionState {
        self.inner.state
    }

    /// 次のイベントを取得する
    pub fn next_event(&mut self) -> Option<RtmpConnectionEvent> {
        self.inner.next_event()
    }

    fn play(&mut self) -> Result<(), Error> {
        self.inner.change_state(RtmpConnectionState::PlayPending)?;

        let command = RtmpCommand::Play(crate::rtmp_command::RtmpPlayCommand {
            transaction_id: self.inner.next_transaction_id,
            stream_name: self.inner.url.stream_name.clone(),
            start: -1.0, // 「常にライブストリームを再生する」と言うことを意味する値
        });

        let message = command.into_message(RtmpMessageHeader {
            stream_id: RtmpMessageStreamId::MEDIA,
            timestamp: RtmpTimestamp::ZERO,
        })?;

        self.inner.message_channel.feed_send_message(message);
        self.inner.next_transaction_id.increment();

        Ok(())
    }
}

// publish / play の共通部分をまとめた構造体
#[derive(Debug)]
struct RtmpClientConnection {
    options: RtmpConnectionOptions,
    state: RtmpConnectionState,
    event_queue: VecDeque<RtmpConnectionEvent>,
    handshake: RtmpClientHandshake,
    message_channel: RtmpMessageChannel,
    next_transaction_id: TransactionId,
    url: RtmpUrl,
}

impl RtmpClientConnection {
    fn new(url: RtmpUrl) -> Self {
        let initial_state = RtmpConnectionState::Handshaking;
        let mut event_queue = VecDeque::new();
        event_queue.push_back(RtmpConnectionEvent::StateChanged(initial_state));

        Self {
            options: RtmpConnectionOptions::default(),
            state: initial_state,
            event_queue,
            handshake: RtmpClientHandshake::new(),
            message_channel: RtmpMessageChannel::default(),
            next_transaction_id: TransactionId::NON_RESERVED_START,
            url,
        }
    }

    fn change_state(&mut self, new_state: RtmpConnectionState) -> Result<(), Error> {
        self.state = new_state;
        self.event_queue
            .push_back(RtmpConnectionEvent::StateChanged(new_state));
        match new_state {
            RtmpConnectionState::Connecting => self.connect(),
            RtmpConnectionState::Connected => self.create_stream(),
            _ => Ok(()),
        }
    }

    fn connect(&mut self) -> Result<(), Error> {
        // こちらから送信するデータに対する（相手からの） ACK の間隔を指定する
        let message = RtmpMessage::win_ack_size(self.options.ack_window_size);
        self.message_channel.feed_send_message(message);
        self.message_channel
            .set_local_ack_window_size(self.options.ack_window_size);

        // 相手から送信してくるデータに対する（こちらからの） ACK の間隔の要望（ヒント）を指定する
        //
        // [NOTE] これへの返信として、相手からの win ack size を受信したタイミングで、こちらからの ACK の間隔が確定する
        let message = RtmpMessage::set_peer_bandwidth(self.options.ack_window_size);
        self.message_channel.feed_send_message(message);

        // SetChunkSize を送信する
        self.message_channel
            .feed_send_message(RtmpMessage::set_chunk_size(self.options.chunk_size));

        // Connect コマンドを送信する
        let tc_url = format!(
            "{}://{}:{}/{}",
            if self.url.tls { "rtmps" } else { "rtmp" },
            self.url.host,
            self.url.port,
            self.url.app
        );

        let command = RtmpCommand::Connect(RtmpConnectCommand {
            app: self.url.app.clone(),
            flash_ver: FLASH_VER.to_owned(),
            tc_url,
        });
        let message = command.into_pcm_message()?;
        self.message_channel.feed_send_message(message);

        Ok(())
    }

    fn feed_recv_buf(&mut self, buf: &[u8]) -> Result<(), Error> {
        if self.state == RtmpConnectionState::Handshaking {
            self.handshake.feed_recv_buf(buf)?;
            if self.handshake.is_recv_complete() {
                self.change_state(RtmpConnectionState::Connecting)?;
                let remaining_recv_buf = self.handshake.take_recv_buf();
                self.message_channel.feed_recv_buf(&remaining_recv_buf)?;
            }
        } else {
            if let Some(total_bytes_received) = self.message_channel.feed_recv_buf(buf)? {
                // ACK メッセージを送信
                let ack_message = RtmpMessage::ack(total_bytes_received);
                self.message_channel.feed_send_message(ack_message);
            }
            while let Some(message) = self.message_channel.next_recv_message() {
                self.handle_recv_message(message)?;
            }
        }
        Ok(())
    }

    fn handle_recv_message(&mut self, message: RtmpMessage) -> Result<(), Error> {
        match message {
            RtmpMessage::Command {
                header,
                name,
                transaction_id,
                object,
                args,
                ..
            } => {
                let command = RtmpCommand::from_message(&name, transaction_id, object, args)?;
                self.handle_command(header, command)
            }
            RtmpMessage::Audio { frame, .. } => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::AudioReceived(frame));
                Ok(())
            }
            RtmpMessage::Video { frame, .. } => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::VideoReceived(frame));
                Ok(())
            }
            RtmpMessage::WinAckSize { size, .. } => {
                // 相手が期待する ACK 送信間隔を設定
                self.message_channel.set_peer_ack_window_size(size);
                Ok(())
            }
            RtmpMessage::Ack {
                sequence_number, ..
            } => {
                // 相手から ACK を受け取ったことを通知
                self.message_channel.notify_ack_received(sequence_number);
                Ok(())
            }
            RtmpMessage::SetPeerBandwidth { size, .. } => {
                // 相手に win ack size を返送する（いったん limit_type は考慮しない）
                let message = RtmpMessage::win_ack_size(self.options.ack_window_size);
                self.message_channel.feed_send_message(message);
                self.message_channel.set_local_ack_window_size(size);
                Ok(())
            }
            RtmpMessage::UserControl {
                event: RtmpUserControlEvent::PingRequest { timestamp },
                ..
            } => {
                let response = RtmpMessage::UserControl {
                    header: RtmpMessageHeader::PCM,
                    event: RtmpUserControlEvent::PingResponse { timestamp },
                };
                self.message_channel.feed_send_message(response);
                Ok(())
            }
            RtmpMessage::UserControl { event, .. } => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::user_control_event_ignored(&event));
                Ok(())
            }
            RtmpMessage::SetChunkSize { .. } => {
                // message decoder のレイヤーでハンドリングされるのでここでは何もしなくていい
                Ok(())
            }
            message => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::message_ignored(&message));
                Ok(())
            }
        }
    }

    fn handle_command(
        &mut self,
        _header: RtmpMessageHeader,
        command: RtmpCommand,
    ) -> Result<(), Error> {
        match command {
            RtmpCommand::Result(result) => self.handle_result_command(result),
            RtmpCommand::OnStatus(status) => self.handle_on_status_command(status),
            _ => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::command_ignored(&command));
                Ok(())
            }
        }
    }

    fn handle_result_command(&mut self, command: RtmpResultCommand) -> Result<(), Error> {
        // 失敗応答を共通でハンドリング
        if command.is_error() {
            self.event_queue
                .push_back(RtmpConnectionEvent::DisconnectedByPeer {
                    reason: format!(
                        "Command response error: {}",
                        self.extract_error_description(&command)
                    ),
                });
            self.change_state(RtmpConnectionState::Disconnecting)?;
            return Ok(());
        }

        // Match transaction ID to determine which result this is
        if command.transaction_id == TransactionId::CONNECT {
            self.state.expect(RtmpConnectionState::Connecting)?;
            self.change_state(RtmpConnectionState::Connected)?;
        } else if command.transaction_id == TransactionId::NON_RESERVED_START {
            // createStream result
            self.state.expect(RtmpConnectionState::Connected)?;
            self.change_state(RtmpConnectionState::MediaStreamCreated)?;
        } else {
            self.event_queue
                .push_back(RtmpConnectionEvent::command_ignored(&RtmpCommand::Result(
                    command,
                )));
        }

        Ok(())
    }

    fn handle_on_status_command(
        &mut self,
        command: crate::rtmp_command::RtmpOnStatusCommand,
    ) -> Result<(), Error> {
        // publish の成功判定
        if command.is_publish_start() && self.state == RtmpConnectionState::PublishPending {
            self.change_state(RtmpConnectionState::Publishing)?;
            return Ok(());
        }

        // play の成功判定
        if command.is_play_start() && self.state == RtmpConnectionState::PlayPending {
            self.change_state(RtmpConnectionState::Playing)?;
            return Ok(());
        }

        // その他のエラー状態を処理
        if command.level == "error" {
            let mut reason = format!("OnStatus error: {}", command.code);
            if let Some(description) = &command.description {
                reason.push_str(&format!(" - {}", description));
            }
            if let Some(details) = &command.details {
                reason.push_str(&format!(" ({})", details));
            }
            self.event_queue
                .push_back(RtmpConnectionEvent::DisconnectedByPeer { reason });
            self.change_state(RtmpConnectionState::Disconnecting)?;
            return Ok(());
        }

        // その他のonStatusメッセージは無視
        self.event_queue
            .push_back(RtmpConnectionEvent::command_ignored(
                &RtmpCommand::OnStatus(command),
            ));

        Ok(())
    }

    // エラーレスポンスから説明文を抽出するヘルパーメソッド
    fn extract_error_description(&self, command: &RtmpResultCommand) -> String {
        command
            .properties
            .expect_object_member("description")
            .and_then(|desc| desc.expect_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "Unknown error".to_string())
    }

    fn send_buf(&self) -> &[u8] {
        if !self.handshake.is_send_complete() {
            self.handshake.send_buf()
        } else {
            self.message_channel.send_buf()
        }
    }

    fn advance_send_buf(&mut self, n: usize) {
        if !self.handshake.is_send_complete() {
            self.handshake.advance_send_buf(n);
        } else {
            // PETAL PATCH (see PETAL_PATCH.md): never disconnect over late
            // acknowledgements. Servers such as YouTube's ingest advertise a
            // Set Peer Bandwidth smaller than the interval they acknowledge
            // at, so this check dropped healthy streams. Like OBS and ffmpeg,
            // a client relies on its own write timeouts to detect a dead link.
            let _ = self.message_channel.advance_send_buf(n);
        }
    }

    fn create_stream(&mut self) -> Result<(), Error> {
        self.state.expect(RtmpConnectionState::Connected)?;

        let command = RtmpCommand::CreateStream(crate::rtmp_command::RtmpCreateStreamCommand {
            transaction_id: self.next_transaction_id,
        });

        let message = command.into_pcm_message()?;
        self.message_channel.feed_send_message(message);
        self.next_transaction_id.increment();

        Ok(())
    }

    fn next_event(&mut self) -> Option<RtmpConnectionEvent> {
        self.event_queue.pop_front()
    }
}
