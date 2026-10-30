// AMF0 の詳細は仕様書を参照（このモジュールは `missing_docs` を抑制する）
#![allow(missing_docs)]

use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use crate::amf::Pair;
use crate::amf3::Amf3Value;
use crate::bytes::{BytesReader, BytesWriter};
use crate::error::Error;

const MARKER_NUMBER: u8 = 0x00;
const MARKER_BOOLEAN: u8 = 0x01;
const MARKER_STRING: u8 = 0x02;
const MARKER_OBJECT: u8 = 0x03;
const MARKER_MOVIECLIP: u8 = 0x04;
const MARKER_NULL: u8 = 0x05;
const MARKER_UNDEFINED: u8 = 0x06;
const MARKER_REFERENCE: u8 = 0x07;
const MARKER_ECMA_ARRAY: u8 = 0x08;
const MARKER_OBJECT_END_MARKER: u8 = 0x09;
const MARKER_STRICT_ARRAY: u8 = 0x0A;
const MARKER_DATE: u8 = 0x0B;
const MARKER_LONG_STRING: u8 = 0x0C;
const MARKER_UNSUPPORTED: u8 = 0x0D;
const MARKER_RECORDSET: u8 = 0x0E;
const MARKER_XML_DOCUMENT: u8 = 0x0F;
const MARKER_TYPED_OBJECT: u8 = 0x10;
const MARKER_AVMPLUS_OBJECT: u8 = 0x11;

#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub enum Amf0Value {
    Number(f64),
    Boolean(bool),
    String(String),
    Object {
        // `None` は無名オブジェクトであることを示す
        class_name: Option<String>,
        entries: Vec<Pair<String, Self>>,
    },
    Null,
    Undefined,
    EcmaArray {
        entries: Vec<Pair<String, Self>>,
    },
    Array {
        entries: Vec<Self>,
    },
    Date {
        unix_time: Duration,
    },
    XmlDocument(String),
    AvmPlus(Amf3Value),
}

impl Amf0Value {
    pub fn decode(buf: &[u8]) -> Result<(usize, Self), Error> {
        let original_buf_len = buf.len();
        let mut decoder = Decoder {
            buf,
            complexes: Vec::new(),
        };
        let value = decoder.decode_value()?;
        Ok((original_buf_len - decoder.buf.len(), value))
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        let mut encoder = Encoder { buf };
        encoder.encode_value(self);
    }
}

#[derive(Debug)]
struct Decoder<'a> {
    buf: &'a [u8],
    complexes: Vec<Amf0Value>,
}

impl Decoder<'_> {
    fn decode_value(&mut self) -> Result<Amf0Value, Error> {
        let marker = self.buf.read_u8()?;
        match marker {
            MARKER_NUMBER => self.decode_number(),
            MARKER_BOOLEAN => self.decode_boolean(),
            MARKER_STRING => self.decode_string(),
            MARKER_OBJECT => self.decode_object(),
            MARKER_NULL => Ok(Amf0Value::Null),
            MARKER_UNDEFINED => Ok(Amf0Value::Undefined),
            MARKER_REFERENCE => self.decode_reference(),
            MARKER_ECMA_ARRAY => self.decode_ecma_array(),
            MARKER_STRICT_ARRAY => self.decode_strict_array(),
            MARKER_DATE => self.decode_date(),
            MARKER_LONG_STRING => self.decode_long_string(),
            MARKER_XML_DOCUMENT => self.decode_xml_document(),
            MARKER_TYPED_OBJECT => self.decode_typed_object(),
            MARKER_AVMPLUS_OBJECT => self.decode_avmplus(),
            MARKER_MOVIECLIP | MARKER_RECORDSET | MARKER_UNSUPPORTED => Err(Error::unsupported(
                format!("unsupported AMF0 marker {marker}"),
            )),
            _ => Err(Error::invalid_data(format!(
                "unexpected AMF0 marker {marker}"
            ))),
        }
    }

    fn decode_avmplus(&mut self) -> Result<Amf0Value, Error> {
        let (bytes_read, amf3_value) = Amf3Value::decode(self.buf)?;
        self.buf = &self.buf[bytes_read..];
        Ok(Amf0Value::AvmPlus(amf3_value))
    }

    fn decode_xml_document(&mut self) -> Result<Amf0Value, Error> {
        let len = self.buf.read_u32()? as usize;
        let s = self.buf.read_utf8(len)?;
        Ok(Amf0Value::XmlDocument(s))
    }

    fn decode_strict_array(&mut self) -> Result<Amf0Value, Error> {
        self.decode_complex_type(|this| {
            let count = this.buf.read_u32()? as usize;
            let entries = (0..count)
                .map(|_| this.decode_value())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Amf0Value::Array { entries })
        })
    }

    fn decode_ecma_array(&mut self) -> Result<Amf0Value, Error> {
        self.decode_complex_type(|this| {
            let _count = this.buf.read_u32()?;
            let entries = this.decode_pairs()?;
            Ok(Amf0Value::EcmaArray { entries })
        })
    }

    fn decode_date(&mut self) -> Result<Amf0Value, Error> {
        let millis = self.buf.read_f64()?;
        let time_zone = self.buf.read_u16()?;

        if !millis.is_finite() || millis.is_sign_negative() {
            return Err(Error::invalid_data(format!(
                "invalid date: millis must be finite and non-negative, got {millis}"
            )));
        }
        if time_zone != 0 {
            return Err(Error::invalid_data(
                "time zone is a reserved field and must be set to zero",
            ));
        }

        Ok(Amf0Value::Date {
            unix_time: Duration::from_millis(millis as u64),
        })
    }

    fn decode_typed_object(&mut self) -> Result<Amf0Value, Error> {
        self.decode_complex_type(|this| {
            let len = this.buf.read_u16()? as usize;
            let class_name = this.buf.read_utf8(len)?;
            let entries = this.decode_pairs()?;
            Ok(Amf0Value::Object {
                class_name: Some(class_name),
                entries,
            })
        })
    }

    fn decode_reference(&mut self) -> Result<Amf0Value, Error> {
        let index = self.buf.read_u16()? as usize;
        let v = self
            .complexes
            .get(index)
            .ok_or_else(|| Error::invalid_data(format!("reference index out of range: {index}")))?;

        if *v == Amf0Value::Null {
            // [NOTE] この crate では循環参照は未対応（実用的にはこれでまず問題ない）
            return Err(Error::unsupported(format!(
                "circular reference at index {index}"
            )));
        }

        Ok(v.clone())
    }

    fn decode_number(&mut self) -> Result<Amf0Value, Error> {
        let n = self.buf.read_f64()?;
        Ok(Amf0Value::Number(n))
    }

    fn decode_boolean(&mut self) -> Result<Amf0Value, Error> {
        let b = self.buf.read_u8()? != 0;
        Ok(Amf0Value::Boolean(b))
    }

    fn decode_string(&mut self) -> Result<Amf0Value, Error> {
        let len = self.buf.read_u16()? as usize;
        let s = self.buf.read_utf8(len)?;
        Ok(Amf0Value::String(s))
    }

    fn decode_long_string(&mut self) -> Result<Amf0Value, Error> {
        let len = self.buf.read_u32()? as usize;
        let s = self.buf.read_utf8(len)?;
        Ok(Amf0Value::String(s))
    }

    fn decode_object(&mut self) -> Result<Amf0Value, Error> {
        self.decode_complex_type(|this| {
            let entries = this.decode_pairs()?;
            Ok(Amf0Value::Object {
                class_name: None,
                entries,
            })
        })
    }

    fn decode_pairs(&mut self) -> Result<Vec<Pair<String, Amf0Value>>, Error> {
        let mut entries = Vec::new();
        loop {
            let key_len = self.buf.read_u16()? as usize;
            let key = self.buf.read_utf8(key_len)?;

            let marker = self.buf.first().copied();
            if marker == Some(MARKER_OBJECT_END_MARKER) {
                let _ = self.buf.read_u8()?;
                break;
            }

            let value = self.decode_value()?;
            entries.push(Pair { key, value });
        }
        Ok(entries)
    }

    fn decode_complex_type<F>(&mut self, f: F) -> Result<Amf0Value, Error>
    where
        F: FnOnce(&mut Self) -> Result<Amf0Value, Error>,
    {
        let index = self.complexes.len();
        self.complexes.push(Amf0Value::Null);
        let value = f(self)?;
        self.complexes[index] = value.clone();
        Ok(value)
    }
}

struct Encoder<'a> {
    buf: &'a mut Vec<u8>,
}

impl<'a> Encoder<'a> {
    fn encode_value(&mut self, v: &Amf0Value) {
        match v {
            Amf0Value::Number(v) => self.encode_number(*v),
            Amf0Value::Boolean(v) => self.encode_boolean(*v),
            Amf0Value::String(v) => self.encode_string(v),
            Amf0Value::Object {
                class_name,
                entries,
            } => self.encode_object(class_name, entries),
            Amf0Value::Null => self.buf.write_u8(MARKER_NULL),
            Amf0Value::Undefined => self.buf.write_u8(MARKER_UNDEFINED),
            Amf0Value::EcmaArray { entries } => self.encode_ecma_array(entries),
            Amf0Value::Array { entries } => self.encode_strict_array(entries),
            Amf0Value::Date { unix_time } => self.encode_date(*unix_time),
            Amf0Value::XmlDocument(v) => self.encode_xml_document(v),
            Amf0Value::AvmPlus(v) => self.encode_avmplus(v),
        }
    }

    fn encode_avmplus(&mut self, v: &Amf3Value) {
        self.buf.write_u8(MARKER_AVMPLUS_OBJECT);
        v.encode(self.buf);
    }

    fn encode_xml_document(&mut self, xml: &str) {
        self.buf.write_u8(MARKER_XML_DOCUMENT);
        self.buf.write_u32(xml.len() as u32);
        self.buf.write_bytes(xml.as_bytes());
    }

    fn encode_date(&mut self, unix_time: Duration) {
        self.buf.write_u8(MARKER_DATE);
        self.buf.write_f64(unix_time.as_millis() as f64);
        self.buf.write_u16(0); // time_zone は常に 0 (reserved field)
    }

    fn encode_strict_array(&mut self, entries: &[Amf0Value]) {
        self.buf.write_u8(MARKER_STRICT_ARRAY);
        self.buf.write_u32(entries.len() as u32);
        for entry in entries {
            self.encode_value(entry);
        }
    }

    fn encode_ecma_array(&mut self, entries: &[Pair<String, Amf0Value>]) {
        self.buf.write_u8(MARKER_ECMA_ARRAY);
        self.buf.write_u32(entries.len() as u32);
        self.encode_pairs(entries);
    }

    fn encode_number(&mut self, n: f64) {
        self.buf.write_u8(MARKER_NUMBER);
        self.buf.write_f64(n);
    }

    fn encode_boolean(&mut self, b: bool) {
        self.buf.write_u8(MARKER_BOOLEAN);
        self.buf.write_u8(if b { 1 } else { 0 });
    }

    fn encode_string(&mut self, s: &str) {
        if s.len() <= 0xFFFF {
            self.buf.write_u8(MARKER_STRING);
            self.buf.write_u16(s.len() as u16);
        } else {
            self.buf.write_u8(MARKER_LONG_STRING);
            self.buf.write_u32(s.len() as u32);
        }
        self.buf.write_bytes(s.as_bytes());
    }

    fn encode_object(&mut self, class_name: &Option<String>, entries: &[Pair<String, Amf0Value>]) {
        if let Some(class_name) = class_name {
            self.buf.write_u8(MARKER_TYPED_OBJECT);
            self.buf.write_u16(class_name.len() as u16);
            self.buf.write_bytes(class_name.as_bytes());
        } else {
            self.buf.write_u8(MARKER_OBJECT);
        }
        self.encode_pairs(entries);
    }

    fn encode_pairs(&mut self, entries: &[Pair<String, Amf0Value>]) {
        for pair in entries {
            self.buf.write_u16(pair.key.len() as u16);
            self.buf.write_bytes(pair.key.as_bytes());
            self.encode_value(&pair.value);
        }
        self.buf.write_u16(0); // 空のキーは終端を意味する
        self.buf.write_u8(MARKER_OBJECT_END_MARKER);
    }
}
