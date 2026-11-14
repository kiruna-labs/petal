use alloc::borrow::ToOwned;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::iter;

use crate::amf::{AmfValue, AmfVersion};
use crate::amf0::Amf0Value;
use crate::error::Error;
use crate::rtmp_message::{RtmpMessage, RtmpMessageHeader, RtmpMessageStreamId};
use crate::rtmp_timestamp::RtmpTimestamp;

// [NOTE]
// フォーマット（AMF）の表現力的には f64 にするのが正しいが、
// RTMP の仕様的に小数は推奨されておらず、また実際に使われることもないため、
// 内部的には整数で保持している
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransactionId(i64);

impl TransactionId {
    pub const ON_STATUS: Self = Self(0);
    pub const CONNECT: Self = Self(1);
    pub const NON_RESERVED_START: Self = Self(2);

    pub fn from_f64(id: f64) -> Self {
        // `no_std` では `f64::round` が使えない。RTMP では実質整数のみのため切り捨てでよい
        Self(id as i64)
    }

    pub const fn get(self) -> i64 {
        self.0
    }

    pub fn increment(&mut self) {
        self.0 += 1
    }
}

#[derive(Debug, Clone)]
pub enum RtmpCommand {
    Connect(RtmpConnectCommand),
    CreateStream(RtmpCreateStreamCommand),
    Publish(RtmpPublishCommand),
    Play(RtmpPlayCommand),
    DeleteStream(RtmpDeleteStreamCommand),
    GetStreamLength(RtmpGetStreamLengthCommand),
    Result(RtmpResultCommand),
    OnStatus(RtmpOnStatusCommand),
    Ignore {
        name: String,

        // 以降のフィールドはデバッグ表示用で、クレート内のコードからは参照されない
        // （`tests` モジュール経由で PBT / fuzzing から参照されるため dead_code にはならない）
        object: AmfValue,
        args: Vec<AmfValue>,
    },
}

impl RtmpCommand {
    pub fn name(&self) -> &str {
        match self {
            RtmpCommand::Connect(_) => "connect",
            RtmpCommand::CreateStream(_) => "createStream",
            RtmpCommand::Publish(_) => "publish",
            RtmpCommand::Play(_) => "play",
            RtmpCommand::DeleteStream(_) => "deleteStream",
            RtmpCommand::GetStreamLength(_) => "getStreamLength",
            RtmpCommand::Result(_) => "_result",
            RtmpCommand::OnStatus(_) => "onStatus",
            RtmpCommand::Ignore { name, .. } => name,
        }
    }

    pub fn into_message(self, header: RtmpMessageHeader) -> Result<RtmpMessage, Error> {
        match self {
            RtmpCommand::Ignore { .. } => {
                // ここに来るのは想定外（実装バグ）
                Err(Error::invalid_state("BUG"))
            }
            RtmpCommand::OnStatus(cmd) => {
                let mut pairs = vec![
                    ("level".to_string(), Amf0Value::String(cmd.level)),
                    ("code".to_string(), Amf0Value::String(cmd.code)),
                ];

                if let Some(description) = cmd.description {
                    pairs.push(("description".to_string(), Amf0Value::String(description)));
                }

                if let Some(details) = cmd.details {
                    pairs.push(("details".to_string(), Amf0Value::String(details)));
                }

                let status_object = AmfValue::Amf0(Amf0Value::Object {
                    class_name: None,
                    entries: pairs
                        .into_iter()
                        .map(|(k, v)| crate::amf::Pair { key: k, value: v })
                        .collect(),
                });

                Ok(RtmpMessage::Command {
                    header,
                    amf_version: AmfVersion::Amf0,
                    name: "onStatus".to_string(),
                    transaction_id: TransactionId::ON_STATUS,
                    object: AmfValue::Amf0(Amf0Value::Null),
                    args: vec![status_object],
                })
            }
            RtmpCommand::Connect(cmd) => {
                let object = AmfValue::amf0_object([
                    ("app", Amf0Value::String(cmd.app)),
                    ("flashVer", Amf0Value::String(cmd.flash_ver)),
                    ("tcUrl", Amf0Value::String(cmd.tc_url)),
                ]);
                Ok(RtmpMessage::Command {
                    header,
                    amf_version: AmfVersion::Amf0,
                    name: "connect".to_string(),
                    transaction_id: TransactionId::CONNECT,
                    object,
                    args: vec![],
                })
            }
            RtmpCommand::CreateStream(cmd) => Ok(RtmpMessage::Command {
                header,
                amf_version: AmfVersion::Amf0,
                name: "createStream".to_string(),
                transaction_id: cmd.transaction_id,
                object: AmfValue::Amf0(Amf0Value::Null),
                args: vec![],
            }),
            RtmpCommand::Publish(cmd) => Ok(RtmpMessage::Command {
                header,
                amf_version: AmfVersion::Amf0,
                name: "publish".to_string(),
                transaction_id: cmd.transaction_id,
                object: AmfValue::Amf0(Amf0Value::Null),
                args: vec![
                    AmfValue::Amf0(Amf0Value::String(cmd.stream_name)),
                    AmfValue::Amf0(Amf0Value::String("live".to_owned())), // publish_type
                ],
            }),
            RtmpCommand::Play(cmd) => Ok(RtmpMessage::Command {
                header,
                amf_version: AmfVersion::Amf0,
                name: "play".to_string(),
                transaction_id: cmd.transaction_id,
                object: AmfValue::Amf0(Amf0Value::Null),
                args: vec![
                    AmfValue::Amf0(Amf0Value::String(cmd.stream_name)),
                    AmfValue::Amf0(Amf0Value::Number(cmd.start)),
                ],
            }),
            RtmpCommand::DeleteStream(cmd) => Ok(RtmpMessage::Command {
                header,
                amf_version: AmfVersion::Amf0,
                name: "deleteStream".to_string(),
                transaction_id: cmd.transaction_id,
                object: AmfValue::Amf0(Amf0Value::Null),
                args: vec![AmfValue::Amf0(
                    Amf0Value::Number(cmd.stream_id.get() as f64),
                )],
            }),
            RtmpCommand::GetStreamLength(cmd) => Ok(RtmpMessage::Command {
                header,
                amf_version: AmfVersion::Amf0,
                name: "getStreamLength".to_string(),
                transaction_id: cmd.transaction_id,
                object: AmfValue::Amf0(Amf0Value::Null),
                args: vec![AmfValue::Amf0(Amf0Value::String(cmd.stream_name))],
            }),
            RtmpCommand::Result(cmd) => Ok(RtmpMessage::Command {
                header,
                amf_version: AmfVersion::Amf0,
                name: "_result".to_string(),
                transaction_id: cmd.transaction_id,
                object: cmd.properties,
                args: vec![cmd.information],
            }),
        }
    }

    pub fn into_pcm_message(self) -> Result<RtmpMessage, Error> {
        self.into_message(RtmpMessageHeader {
            stream_id: RtmpMessageStreamId::PCM,
            timestamp: RtmpTimestamp::ZERO,
        })
    }

    pub fn from_message(
        name: &str,
        transaction_id: TransactionId,
        object: AmfValue,
        args: Vec<AmfValue>,
    ) -> Result<Self, Error> {
        match name {
            "connect" => {
                RtmpConnectCommand::from_message(transaction_id, object).map(Self::Connect)
            }
            "createStream" => RtmpCreateStreamCommand::from_message(transaction_id, object)
                .map(Self::CreateStream),
            "publish" => {
                RtmpPublishCommand::from_message(transaction_id, object, args).map(Self::Publish)
            }
            "play" => RtmpPlayCommand::from_message(transaction_id, object, args).map(Self::Play),
            "deleteStream" => RtmpDeleteStreamCommand::from_message(transaction_id, object, args)
                .map(Self::DeleteStream),
            "getStreamLength" => {
                RtmpGetStreamLengthCommand::from_message(transaction_id, object, args)
                    .map(Self::GetStreamLength)
            }
            "_result" => {
                RtmpResultCommand::from_message(transaction_id, object, args).map(Self::Result)
            }
            "onStatus" => {
                RtmpOnStatusCommand::from_message(transaction_id, object, args).map(Self::OnStatus)
            }
            _ => {
                // この crate 内で明示的にハンドリングしないものは全て Ignore 扱いにする
                Ok(Self::Ignore {
                    name: name.to_string(),
                    object,
                    args,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtmpConnectCommand {
    pub app: String,
    pub flash_ver: String,
    pub tc_url: String,
}

impl RtmpConnectCommand {
    fn from_message(transaction_id: TransactionId, object: AmfValue) -> Result<Self, Error> {
        if transaction_id != TransactionId::CONNECT {
            return Err(Error::invalid_data(format!(
                "invalid transaction ID for connect command: expected {}, got {}",
                TransactionId::CONNECT.get(),
                transaction_id.get()
            )));
        }

        let app = object
            .expect_object_member("app")?
            .expect_str()?
            .to_string();
        let flash_ver = object
            .expect_object_member("flashVer")?
            .expect_str()?
            .to_string();
        let tc_url = object
            .expect_object_member("tcUrl")?
            .expect_str()?
            .to_string();
        Ok(Self {
            app,
            flash_ver,
            tc_url,
        })
    }

    pub fn accept(&self) -> Result<RtmpMessage, Error> {
        let properties = AmfValue::amf0_object([
            ("fmsVer", Amf0Value::String("FMS/4,5,0,297".to_string())),
            ("capabilities", Amf0Value::Number(255.0)),
            ("mode", Amf0Value::Number(1.0)),
        ]);
        let information = AmfValue::amf0_object([
            ("level", Amf0Value::String("status".to_string())),
            (
                "code",
                Amf0Value::String("NetConnection.Connect.Success".to_string()),
            ),
            (
                "description",
                Amf0Value::String("Connection succeeded.".to_string()),
            ),
            ("objectEncoding", Amf0Value::Number(0.0)),
        ]);
        let command = RtmpResultCommand {
            transaction_id: TransactionId::CONNECT,
            properties,
            information,
        };
        RtmpCommand::Result(command).into_pcm_message()
    }
}

#[derive(Debug, Clone)]
pub struct RtmpCreateStreamCommand {
    pub transaction_id: TransactionId,
}

impl RtmpCreateStreamCommand {
    fn from_message(transaction_id: TransactionId, _object: AmfValue) -> Result<Self, Error> {
        Ok(Self { transaction_id })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtmpPublishCommand {
    pub transaction_id: TransactionId,
    pub stream_name: String,
}

impl RtmpPublishCommand {
    fn from_message(
        transaction_id: TransactionId,
        _object: AmfValue,
        args: Vec<AmfValue>,
    ) -> Result<Self, Error> {
        let stream_name = args
            .first()
            .ok_or_else(|| Error::invalid_data("publish: missing stream name"))?
            .expect_str()?
            .to_string();
        let publish_type = args
            .get(1)
            .ok_or_else(|| Error::invalid_data("publish: missing publish type"))?
            .expect_str()?
            .to_string();
        if publish_type != "live" {
            return Err(Error::unsupported(format!(
                "unsupported publish type: {publish_type} (only 'live' is supported)",
            )));
        }

        Ok(Self {
            transaction_id,
            stream_name,
        })
    }

    pub fn accept(
        transaction_id: TransactionId,
        stream_id: RtmpMessageStreamId,
    ) -> Result<RtmpMessage, Error> {
        let properties = AmfValue::amf0_object(iter::empty());
        let information = AmfValue::amf0_object([
            ("level", Amf0Value::String("status".to_string())),
            (
                "code",
                Amf0Value::String("NetStream.Publish.Start".to_string()),
            ),
            (
                "description",
                Amf0Value::String("Publish succeeded.".to_string()),
            ),
        ]);
        let command = RtmpResultCommand {
            transaction_id,
            properties,
            information,
        };
        RtmpCommand::Result(command).into_message(RtmpMessageHeader {
            stream_id,
            timestamp: RtmpTimestamp::ZERO,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RtmpPlayCommand {
    pub transaction_id: TransactionId,
    pub stream_name: String,
    pub start: f64,
}

impl RtmpPlayCommand {
    fn from_message(
        transaction_id: TransactionId,
        _object: AmfValue,
        args: Vec<AmfValue>,
    ) -> Result<Self, Error> {
        let stream_name = args
            .first()
            .ok_or_else(|| Error::invalid_data("play: missing stream name"))?
            .expect_str()?
            .to_string();
        let start = args
            .get(1)
            .ok_or_else(|| Error::invalid_data("play: missing start position"))?
            .expect_number()?;
        Ok(Self {
            transaction_id,
            stream_name,
            start,
        })
    }

    pub fn accept(
        transaction_id: TransactionId,
        stream_id: RtmpMessageStreamId,
    ) -> Result<RtmpMessage, Error> {
        let properties = AmfValue::amf0_object(iter::empty());
        let information = AmfValue::amf0_object([
            ("level", Amf0Value::String("status".to_string())),
            (
                "code",
                Amf0Value::String("NetStream.Play.Start".to_string()),
            ),
            (
                "description",
                Amf0Value::String("Play succeeded.".to_string()),
            ),
        ]);
        let command = RtmpResultCommand {
            transaction_id,
            properties,
            information,
        };
        RtmpCommand::Result(command).into_message(RtmpMessageHeader {
            stream_id,
            timestamp: RtmpTimestamp::ZERO,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RtmpDeleteStreamCommand {
    pub transaction_id: TransactionId,
    pub stream_id: RtmpMessageStreamId,
}

impl RtmpDeleteStreamCommand {
    fn from_message(
        transaction_id: TransactionId,
        _object: AmfValue,
        args: Vec<AmfValue>,
    ) -> Result<Self, Error> {
        let stream_id = args
            .first()
            .ok_or_else(|| Error::invalid_data("deleteStream: missing stream id"))?
            .expect_number()?;
        Ok(Self {
            transaction_id,
            stream_id: RtmpMessageStreamId::new(stream_id as u32),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RtmpGetStreamLengthCommand {
    pub transaction_id: TransactionId,
    pub stream_name: String,
}

impl RtmpGetStreamLengthCommand {
    fn from_message(
        transaction_id: TransactionId,
        _object: AmfValue,
        args: Vec<AmfValue>,
    ) -> Result<Self, Error> {
        let stream_name = args
            .first()
            .ok_or_else(|| Error::invalid_data("getStreamLength: missing stream name"))?
            .expect_str()?
            .to_string();
        Ok(Self {
            transaction_id,
            stream_name,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RtmpResultCommand {
    pub transaction_id: TransactionId,
    pub properties: AmfValue,
    pub information: AmfValue,
}

impl RtmpResultCommand {
    pub fn get_stream_length_result(transaction_id: TransactionId, length: f64) -> Self {
        Self {
            transaction_id,
            properties: AmfValue::Amf0(Amf0Value::Null),
            information: AmfValue::Amf0(Amf0Value::Number(length)),
        }
    }

    fn from_message(
        transaction_id: TransactionId,
        object: AmfValue,
        args: Vec<AmfValue>,
    ) -> Result<Self, Error> {
        let properties = object;
        let information = args
            .first()
            .cloned()
            .ok_or_else(|| Error::invalid_data("_result: missing information argument"))?;
        Ok(Self {
            transaction_id,
            properties,
            information,
        })
    }

    pub fn is_error(&self) -> bool {
        // ヒューリスティックでエラー応答かどうかを判定する
        self.properties
            .expect_object_member("code")
            .and_then(|code| code.expect_str())
            .map(|code| code.to_lowercase().contains("error"))
            .unwrap_or(false)
    }

    pub fn create_stream_result(
        transaction_id: TransactionId,
        stream_id: RtmpMessageStreamId,
    ) -> Self {
        Self {
            transaction_id,
            properties: AmfValue::Amf0(Amf0Value::Null),
            information: AmfValue::Amf0(Amf0Value::Number(stream_id.get() as f64)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RtmpOnStatusCommand {
    pub level: String,
    pub code: String,
    pub description: Option<String>,
    pub details: Option<String>,
}

impl RtmpOnStatusCommand {
    fn from_message(
        _transaction_id: TransactionId,
        _object: AmfValue,
        args: Vec<AmfValue>,
    ) -> Result<Self, Error> {
        let status_obj = args
            .first()
            .ok_or_else(|| Error::invalid_data("onStatus: missing status object"))?;

        let level = status_obj
            .expect_object_member("level")?
            .expect_str()?
            .to_string();
        let code = status_obj
            .expect_object_member("code")?
            .expect_str()?
            .to_string();

        let description = status_obj
            .expect_object_member("description")
            .ok()
            .and_then(|v| v.expect_str().ok())
            .map(|s| s.to_string());

        let details = status_obj
            .expect_object_member("details")
            .ok()
            .and_then(|v| v.expect_str().ok())
            .map(|s| s.to_string());

        Ok(Self {
            level,
            code,
            description,
            details,
        })
    }

    pub fn is_publish_start(&self) -> bool {
        self.code == "NetStream.Publish.Start"
    }

    pub fn is_play_start(&self) -> bool {
        self.code == "NetStream.Play.Start"
    }

    pub fn publish_start() -> Self {
        Self {
            level: "status".to_string(),
            code: "NetStream.Publish.Start".to_string(),
            description: Some("Publish succeeded.".to_string()),
            details: None,
        }
    }

    pub fn play_start() -> Self {
        Self {
            level: "status".to_string(),
            code: "NetStream.Play.Start".to_string(),
            description: Some("Play succeeded.".to_string()),
            details: None,
        }
    }

    pub fn publish_bad_name(reason: &str) -> Self {
        Self {
            level: "error".to_string(),
            code: "NetStream.Publish.BadName".to_string(),
            description: Some("Stream name already in use.".to_string()),
            details: Some(reason.to_string()),
        }
    }

    pub fn play_stream_not_found(reason: &str) -> Self {
        Self {
            level: "error".to_string(),
            code: "NetStream.Play.StreamNotFound".to_string(),
            description: Some("Stream not found.".to_string()),
            details: Some(reason.to_string()),
        }
    }
}
