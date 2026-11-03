use alloc::borrow::ToOwned;
use alloc::collections::VecDeque;

use crate::error::Error;
use crate::media::{AudioFrame, VideoFrame};
use crate::rtmp_command::{
    RtmpCommand, RtmpConnectCommand, RtmpOnStatusCommand, RtmpPlayCommand, RtmpPublishCommand,
    RtmpResultCommand, TransactionId,
};
use crate::rtmp_connection::{
    RtmpConnectionEvent, RtmpConnectionOptions, RtmpConnectionState, RtmpMessageChannel,
};
use crate::rtmp_handshake::RtmpServerHandshake;
use crate::rtmp_message::{RtmpMessage, RtmpMessageHeader, RtmpMessageStreamId};
use crate::rtmp_timestamp::RtmpTimestamp;
use crate::rtmp_user_control_event::RtmpUserControlEvent;

/// RTMP サーバーの接続を管理するための構造体
///
/// この構造体は RTMP サーバー側のコネクションを表し、クライアントからの接続要求を処理し、
/// メディアストリームの配信（Publish）や再生（Play）をハンドリングする。
/// なお、この構造体自体は I/O 操作を行わないため、
/// ソケットの読み書き操作などは利用側で行う必要がある。
#[derive(Debug)]
pub struct RtmpServerConnection {
    options: RtmpConnectionOptions,
    state: RtmpConnectionState,
    event_queue: VecDeque<RtmpConnectionEvent>,
    handshake: RtmpServerHandshake,
    message_channel: RtmpMessageChannel,
    connect_command: Option<RtmpConnectCommand>,
    pending_transaction_id: Option<TransactionId>,
}

impl RtmpServerConnection {
    /// 新しい RTMP サーバー接続を作成する
    pub fn new() -> Self {
        let mut this = Self {
            options: RtmpConnectionOptions::default(),
            state: RtmpConnectionState::default(),
            event_queue: VecDeque::new(),
            handshake: RtmpServerHandshake::new(),
            message_channel: RtmpMessageChannel::default(),
            connect_command: None,
            pending_transaction_id: None,
        };

        this.change_state(RtmpConnectionState::Handshaking);

        this
    }

    fn change_state(&mut self, new_state: RtmpConnectionState) {
        self.state = new_state;
        self.event_queue
            .push_back(RtmpConnectionEvent::StateChanged(new_state));
    }

    /// クライアントから受信したデータを処理する
    pub fn feed_recv_buf(&mut self, buf: &[u8]) -> Result<(), Error> {
        let feed_result = if self.state == RtmpConnectionState::Handshaking {
            self.handshake.feed_recv_buf(buf)?;
            if !self.handshake.is_recv_complete() {
                return Ok(());
            }
            self.change_state(RtmpConnectionState::Connecting);
            let remaining_recv_buf = self.handshake.take_recv_buf();
            self.message_channel.feed_recv_buf(&remaining_recv_buf)?
        } else {
            self.message_channel.feed_recv_buf(buf)?
        };

        // ACK メッセージを送信する必要があるかを確認
        if let Some(total_bytes_received) = feed_result {
            // ACK メッセージを送信
            let ack_message = RtmpMessage::ack(total_bytes_received);
            self.message_channel.feed_send_message(ack_message);
        }
        while let Some(message) = self.message_channel.next_recv_message() {
            self.handle_recv_message(message)?;
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
                self.handle_command(header, command)?;
            }
            RtmpMessage::Audio { frame, .. } => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::AudioReceived(frame));
            }
            RtmpMessage::Video { frame, .. } => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::VideoReceived(frame));
            }
            RtmpMessage::WinAckSize { size, .. } => {
                // 相手が期待する ACK 送信間隔を設定
                self.message_channel.set_peer_ack_window_size(size);
            }
            RtmpMessage::Ack {
                sequence_number, ..
            } => {
                // 相手から ACK を受け取ったことを通知
                self.message_channel.notify_ack_received(sequence_number);
            }
            RtmpMessage::SetChunkSize { .. } => {
                // message decoder のレイヤーでハンドリングされるのでここでは何もしなくていい
            }
            RtmpMessage::SetPeerBandwidth { size, .. } => {
                // 相手に win ack size を返送する（いったん limit_type は考慮しない）
                let message = RtmpMessage::win_ack_size(self.options.ack_window_size);
                self.message_channel.feed_send_message(message);
                self.message_channel.set_local_ack_window_size(size);
            }
            RtmpMessage::UserControl {
                event: RtmpUserControlEvent::SetBufferLength { .. },
                ..
            } => {
                // SetBufferLength は一方向の通知なので、応答は不要
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
            }
            RtmpMessage::UserControl { event, .. } => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::user_control_event_ignored(&event));
            }
            message => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::message_ignored(&message));
            }
        }
        Ok(())
    }

    fn handle_command(
        &mut self,
        _header: RtmpMessageHeader,
        command: RtmpCommand,
    ) -> Result<(), Error> {
        match command {
            RtmpCommand::Connect(c) => self.handle_connect_command(c),
            RtmpCommand::CreateStream(c) => self.handle_create_stream_command(c),
            RtmpCommand::Publish(c) => self.handle_publish_command(c),
            RtmpCommand::Play(c) => self.handle_play_command(c),
            RtmpCommand::GetStreamLength(c) => self.handle_get_stream_length_command(c),
            _ => {
                self.event_queue
                    .push_back(RtmpConnectionEvent::command_ignored(&command));
                Ok(())
            }
        }
    }

    fn handle_get_stream_length_command(
        &mut self,
        command: crate::rtmp_command::RtmpGetStreamLengthCommand,
    ) -> Result<(), Error> {
        // Always reply with stream length of 0
        let message = RtmpCommand::Result(RtmpResultCommand::get_stream_length_result(
            command.transaction_id,
            0.0,
        ))
        .into_message(RtmpMessageHeader::PCM)?;
        self.message_channel.feed_send_message(message);

        Ok(())
    }

    fn handle_publish_command(
        &mut self,
        command: crate::rtmp_command::RtmpPublishCommand,
    ) -> Result<(), Error> {
        self.state.expect(RtmpConnectionState::MediaStreamCreated)?;
        self.change_state(RtmpConnectionState::PublishPending);
        self.pending_transaction_id = Some(command.transaction_id);

        let connect = self
            .connect_command
            .as_ref()
            .ok_or_else(|| Error::invalid_state("Connect command not set before publish"))?;
        self.event_queue
            .push_back(RtmpConnectionEvent::PublishRequested {
                app: connect.app.clone(),
                tc_url: connect.tc_url.clone(),
                stream_name: command.stream_name,
            });

        Ok(())
    }

    fn handle_play_command(
        &mut self,
        command: crate::rtmp_command::RtmpPlayCommand,
    ) -> Result<(), Error> {
        self.state.expect(RtmpConnectionState::MediaStreamCreated)?;
        self.change_state(RtmpConnectionState::PlayPending);
        self.pending_transaction_id = Some(command.transaction_id);

        let connect = self
            .connect_command
            .as_ref()
            .ok_or_else(|| Error::invalid_state("Connect command not set before play"))?;
        self.event_queue
            .push_back(RtmpConnectionEvent::PlayRequested {
                app: connect.app.clone(),
                tc_url: connect.tc_url.clone(),
                stream_name: command.stream_name,
            });

        Ok(())
    }

    fn handle_create_stream_command(
        &mut self,
        command: crate::rtmp_command::RtmpCreateStreamCommand,
    ) -> Result<(), Error> {
        self.state.expect(RtmpConnectionState::Connected)?;

        let message = RtmpCommand::Result(RtmpResultCommand::create_stream_result(
            command.transaction_id,
            RtmpMessageStreamId::MEDIA,
        ))
        .into_message(RtmpMessageHeader::PCM)?;
        self.message_channel.feed_send_message(message);

        self.message_channel
            .feed_send_message(RtmpMessage::stream_begin(RtmpMessageStreamId::MEDIA));

        self.change_state(RtmpConnectionState::MediaStreamCreated);
        Ok(())
    }

    fn handle_connect_command(&mut self, command: RtmpConnectCommand) -> Result<(), Error> {
        // こちらから送信するデータに対する（相手からの） ACK の間隔を指定する
        self.message_channel
            .feed_send_message(RtmpMessage::win_ack_size(self.options.ack_window_size));
        self.message_channel
            .set_local_ack_window_size(self.options.ack_window_size);

        // SetChunkSize を送信する
        self.message_channel
            .feed_send_message(RtmpMessage::set_chunk_size(self.options.chunk_size));

        // 相手から送信してくるデータに対する（こちらからの） ACK の間隔の要望（ヒント）を指定する
        //
        // [NOTE] これへの返信として、相手からの win ack size を受信したタイミングで、こちらからの ACK の間隔が確定する
        self.message_channel
            .feed_send_message(RtmpMessage::set_peer_bandwidth(
                self.options.ack_window_size,
            ));

        // RTMP の仕様に合わせて、最初のストリームを開始する（用途は不明）
        self.message_channel
            .feed_send_message(RtmpMessage::stream_begin(RtmpMessageStreamId::FIRST));

        // Connect は常に受理する（必要なら Publish / Play コマンドのタイミングで拒否する）
        self.message_channel.feed_send_message(command.accept()?);
        self.change_state(RtmpConnectionState::Connected);
        self.connect_command = Some(command);

        Ok(())
    }

    /// クライアントに送信待ちのデータを取得する
    ///
    /// クライアントへのデータ送信が成功した場合には、
    /// その送信バイト数を `RtmpServerConnection::advance_send_buf()` を呼び出して反映する必要がある
    pub fn send_buf(&self) -> &[u8] {
        if self.state == RtmpConnectionState::Handshaking {
            self.handshake.send_buf()
        } else {
            self.message_channel.send_buf()
        }
    }

    /// 送信バッファから指定バイト数を送信済みとしてマークする
    pub fn advance_send_buf(&mut self, n: usize) {
        if !self.handshake.is_send_complete() {
            self.handshake.advance_send_buf(n);
        } else if !self.message_channel.advance_send_buf(n) {
            // 相手から ACK が期待通りに届いていないので切断する
            self.event_queue
                .push_back(RtmpConnectionEvent::DisconnectedByPeer {
                    reason: "ACK not received within expected interval".to_owned(),
                });
            self.change_state(RtmpConnectionState::Disconnecting);
        }
    }

    /// コネクションの現在の状態を返す
    pub fn state(&self) -> RtmpConnectionState {
        self.state
    }

    /// 配信（Publish）または再生（Play）のリクエストを受理する
    ///
    /// [`RtmpConnectionState::PublishPending`] または [`RtmpConnectionState::PlayPending`] 状態の時に呼び出すことで、
    /// クライアントからのリクエストを承認し、それぞれ [`RtmpConnectionState::Publishing`] または [`RtmpConnectionState::Playing`] 状態に遷移する
    ///
    /// 上記以外の状態でこのメソッドを呼び出した場合にはエラーとなる
    pub fn accept(&mut self) -> Result<(), Error> {
        if !matches!(
            self.state,
            RtmpConnectionState::PublishPending | RtmpConnectionState::PlayPending
        ) {
            return Err(Error::invalid_state(format!(
                "Cannot accept in {} state",
                self.state
            )));
        }

        let transaction_id = self
            .pending_transaction_id
            .ok_or_else(|| Error::invalid_state("Pending transaction ID not set"))?;
        if self.state == RtmpConnectionState::PublishPending {
            self.accept_publish(transaction_id)
        } else {
            self.accept_play(transaction_id)
        }
    }

    fn accept_publish(
        &mut self,
        transaction_id: crate::rtmp_command::TransactionId,
    ) -> Result<(), Error> {
        self.state.expect(RtmpConnectionState::PublishPending)?;

        self.message_channel
            .feed_send_message(RtmpPublishCommand::accept(
                transaction_id,
                RtmpMessageStreamId::MEDIA,
            )?);

        let message = RtmpCommand::OnStatus(RtmpOnStatusCommand::publish_start()).into_message(
            RtmpMessageHeader {
                stream_id: RtmpMessageStreamId::MEDIA,
                timestamp: RtmpTimestamp::ZERO,
            },
        )?;
        self.message_channel.feed_send_message(message);

        self.change_state(RtmpConnectionState::Publishing);
        Ok(())
    }

    fn accept_play(
        &mut self,
        transaction_id: crate::rtmp_command::TransactionId,
    ) -> Result<(), Error> {
        self.state.expect(RtmpConnectionState::PlayPending)?;

        self.message_channel
            .feed_send_message(RtmpPlayCommand::accept(
                transaction_id,
                RtmpMessageStreamId::MEDIA,
            )?);

        let message = RtmpCommand::OnStatus(RtmpOnStatusCommand::play_start()).into_message(
            RtmpMessageHeader {
                stream_id: RtmpMessageStreamId::MEDIA,
                timestamp: RtmpTimestamp::ZERO,
            },
        )?;
        self.message_channel.feed_send_message(message);

        self.change_state(RtmpConnectionState::Playing);
        Ok(())
    }

    /// 配信（Publish）または再生（Play）のリクエストを拒否する
    ///
    /// [`RtmpConnectionState::PublishPending`] または [`RtmpConnectionState::PlayPending`] 状態の時に呼び出すことで、
    /// クライアントからのリクエストを拒否し、[`RtmpConnectionState::Disconnecting`] 状態に遷移する
    ///
    /// 上記以外の状態でこのメソッドを呼び出した場合にはエラーとなる
    pub fn reject(&mut self, reason: &str) -> Result<(), Error> {
        if !matches!(
            self.state,
            RtmpConnectionState::PublishPending | RtmpConnectionState::PlayPending
        ) {
            return Err(Error::invalid_state(format!(
                "Cannot reject in {} state",
                self.state
            )));
        }

        let message = if self.state == RtmpConnectionState::PublishPending {
            RtmpCommand::OnStatus(RtmpOnStatusCommand::publish_bad_name(reason)).into_message(
                RtmpMessageHeader {
                    stream_id: RtmpMessageStreamId::MEDIA,
                    timestamp: RtmpTimestamp::ZERO,
                },
            )?
        } else {
            RtmpCommand::OnStatus(RtmpOnStatusCommand::play_stream_not_found(reason)).into_message(
                RtmpMessageHeader {
                    stream_id: RtmpMessageStreamId::MEDIA,
                    timestamp: RtmpTimestamp::ZERO,
                },
            )?
        };

        self.message_channel.feed_send_message(message);

        self.change_state(RtmpConnectionState::Disconnecting);
        Ok(())
    }

    /// 音声フレームをクライアントに送信する（送信バッファに追加する）
    ///
    /// [`RtmpConnectionState::Playing`] 状態の時のみ呼び出し可能
    pub fn send_audio(&mut self, frame: AudioFrame) -> Result<(), Error> {
        self.state.expect(RtmpConnectionState::Playing)?;

        let message = RtmpMessage::Audio {
            header: RtmpMessageHeader {
                stream_id: RtmpMessageStreamId::MEDIA,
                timestamp: frame.timestamp,
            },
            frame,
        };

        self.message_channel.feed_send_message(message);
        Ok(())
    }

    /// 映像フレームをクライアントに送信する（送信バッファに追加する）
    ///
    /// [`RtmpConnectionState::Playing`] 状態の時のみ呼び出し可能
    pub fn send_video(&mut self, frame: VideoFrame) -> Result<(), Error> {
        self.state.expect(RtmpConnectionState::Playing)?;

        let message = RtmpMessage::Video {
            header: RtmpMessageHeader {
                stream_id: RtmpMessageStreamId::MEDIA,
                timestamp: frame.timestamp,
            },
            frame,
        };

        self.message_channel.feed_send_message(message);
        Ok(())
    }

    /// 次のイベントを取得する
    ///
    /// 接続状態の変化やクライアントからのリクエストなど、
    /// 処理すべきイベントがある場合はそれを返す
    ///
    /// イベントがない場合は `None` を返す
    pub fn next_event(&mut self) -> Option<RtmpConnectionEvent> {
        self.event_queue.pop_front()
    }
}

impl Default for RtmpServerConnection {
    fn default() -> Self {
        Self::new()
    }
}
