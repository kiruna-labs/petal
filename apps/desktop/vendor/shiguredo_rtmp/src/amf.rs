// AMF の詳細は仕様書を参照（このモジュールは `missing_docs` を抑制する）
#![allow(missing_docs)]

use alloc::borrow::ToOwned;
use alloc::vec::Vec;

use crate::amf0::Amf0Value;
use crate::amf3::Amf3Value;
use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub enum AmfVersion {
    Amf0,
    Amf3,
}

#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub enum AmfValue {
    Amf0(Amf0Value),
    Amf3(Amf3Value),
}

impl AmfValue {
    pub fn decode(buf: &[u8], version: AmfVersion) -> Result<(usize, Self), Error> {
        match version {
            AmfVersion::Amf0 => Amf0Value::decode(buf).map(|(n, v)| (n, Self::Amf0(v))),
            AmfVersion::Amf3 => Amf3Value::decode(buf).map(|(n, v)| (n, Self::Amf3(v))),
        }
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            Self::Amf0(x) => x.encode(buf),
            Self::Amf3(x) => x.encode(buf),
        }
    }

    #[track_caller]
    pub fn expect_object_member(&self, key: &str) -> Result<AmfValueRef<'_>, Error> {
        match self {
            Self::Amf0(Amf0Value::Object { entries, .. }) => entries
                .iter()
                .find(|pair| pair.key == key)
                .map(|pair| AmfValueRef::Amf0(&pair.value))
                .ok_or_else(|| Error::invalid_data(format!("missing required key: {key}"))),
            Self::Amf3(Amf3Value::Object { entries, .. }) => entries
                .iter()
                .find(|pair| pair.key == key)
                .map(|pair| AmfValueRef::Amf3(&pair.value))
                .ok_or_else(|| Error::invalid_data(format!("missing required key: {key}"))),
            _ => Err(Error::invalid_data("value is not an AMF object")),
        }
    }

    #[track_caller]
    pub fn expect_str(&self) -> Result<&str, Error> {
        self.to_ref().expect_str()
    }

    #[track_caller]
    pub fn expect_number(&self) -> Result<f64, Error> {
        self.to_ref().expect_number()
    }

    fn to_ref(&self) -> AmfValueRef<'_> {
        match self {
            Self::Amf0(v) => AmfValueRef::Amf0(v),
            Self::Amf3(v) => AmfValueRef::Amf3(v),
        }
    }

    pub fn amf0_object<'a, I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, Amf0Value)>,
    {
        Self::Amf0(Amf0Value::Object {
            class_name: None,
            entries: entries
                .into_iter()
                .map(|(k, v)| Pair {
                    key: k.to_owned(),
                    value: v,
                })
                .collect(),
        })
    }
}

impl From<(AmfVersion, &str)> for AmfValue {
    fn from((version, value): (AmfVersion, &str)) -> Self {
        match version {
            AmfVersion::Amf0 => Self::Amf0(Amf0Value::String(value.to_owned())),
            AmfVersion::Amf3 => Self::Amf3(Amf3Value::String(value.to_owned())),
        }
    }
}

impl From<(AmfVersion, f64)> for AmfValue {
    fn from((version, value): (AmfVersion, f64)) -> Self {
        match version {
            AmfVersion::Amf0 => Self::Amf0(Amf0Value::Number(value)),
            AmfVersion::Amf3 => Self::Amf3(Amf3Value::Double(value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub enum AmfValueRef<'a> {
    Amf0(&'a Amf0Value),
    Amf3(&'a Amf3Value),
}

impl<'a> AmfValueRef<'a> {
    pub fn expect_str(&self) -> Result<&'a str, Error> {
        match self {
            Self::Amf0(Amf0Value::String(s)) => Ok(s),
            Self::Amf3(Amf3Value::String(s)) => Ok(s),
            _ => Err(Error::invalid_data("value is not an AMF string")),
        }
    }

    pub fn expect_number(&self) -> Result<f64, Error> {
        match self {
            Self::Amf0(Amf0Value::Number(n)) => Ok(*n),
            Self::Amf3(Amf3Value::Integer(n)) => Ok(*n as f64),
            Self::Amf3(Amf3Value::Double(n)) => Ok(*n),
            _ => Err(Error::invalid_data("value is not an AMF number")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pair<K, V> {
    pub key: K,
    pub value: V,
}
