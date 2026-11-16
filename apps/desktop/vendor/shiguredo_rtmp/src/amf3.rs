// AMF3 の詳細は仕様書を参照（このモジュールは `missing_docs` を抑制する）
#![allow(missing_docs)]

use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use crate::amf::Pair;
use crate::bytes::{BytesReader, BytesWriter};
use crate::error::Error;

const MARKER_UNDEFINED: u8 = 0x00;
const MARKER_NULL: u8 = 0x01;
const MARKER_FALSE: u8 = 0x02;
const MARKER_TRUE: u8 = 0x03;
const MARKER_INTEGER: u8 = 0x04;
const MARKER_DOUBLE: u8 = 0x05;
const MARKER_STRING: u8 = 0x06;
const MARKER_XML_DOCUMENT: u8 = 0x07;
const MARKER_DATE: u8 = 0x08;
const MARKER_ARRAY: u8 = 0x09;
const MARKER_OBJECT: u8 = 0x0A;
const MARKER_XML: u8 = 0x0B;
const MARKER_BYTE_ARRAY: u8 = 0x0C;
const MARKER_INT_VECTOR: u8 = 0x0D;
const MARKER_UINT_VECTOR: u8 = 0x0E;
const MARKER_DOUBLE_VECTOR: u8 = 0x0F;
const MARKER_OBJECT_VECTOR: u8 = 0x10;
const MARKER_DICTIONARY: u8 = 0x11;

#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub enum Amf3Value {
    Undefined,
    Null,
    Boolean(bool),
    Integer(i32),
    Double(f64),
    String(String),
    XmlDocument(String),
    Date {
        unix_time: Duration,
    },
    Array {
        assoc_entries: Vec<Pair<String, Self>>,
        dense_entries: Vec<Self>,
    },
    Object {
        // `None` なら無名オブジェクト
        class_name: Option<String>,

        // この数の "sealed member" が entries の先頭に配置されている
        sealed_count: usize,

        entries: Vec<Pair<String, Self>>,
    },
    Xml(String),
    ByteArray(Vec<u8>),
    IntVector {
        is_fixed: bool,
        entries: Vec<i32>,
    },
    UintVector {
        is_fixed: bool,
        entries: Vec<u32>,
    },
    DoubleVector {
        is_fixed: bool,
        entries: Vec<f64>,
    },
    ObjectVector {
        // `None` なら任意の型を要素に含むことを意味する
        class_name: Option<String>,
        is_fixed: bool,
        entries: Vec<Self>,
    },
    Dictionary {
        is_weak: bool,
        entries: Vec<Pair<Self, Self>>,
    },
}

impl Amf3Value {
    pub fn decode(buf: &[u8]) -> Result<(usize, Self), Error> {
        let original_len = buf.len();
        let mut decoder = Decoder {
            buf,
            complexes: Vec::new(),
            strings: Vec::new(),
            traits: Vec::new(),
        };
        let value = decoder.decode_value()?;
        Ok((original_len - decoder.buf.len(), value))
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        let mut encoder = Encoder { buf };
        encoder.encode_value(self);
    }
}

#[derive(Debug)]
struct Decoder<'a> {
    buf: &'a [u8],
    complexes: Vec<Amf3Value>,
    strings: Vec<String>,
    traits: Vec<Trait>,
}

#[derive(Debug, Clone)]
struct Trait {
    class_name: Option<String>,
    is_dynamic: bool,
    fields: Vec<String>,
}

#[derive(Debug)]
enum SizeOrIndex {
    Size(usize),
    Index(usize),
}

impl<'a> Decoder<'a> {
    fn decode_value(&mut self) -> Result<Amf3Value, Error> {
        let marker = self.buf.read_u8()?;
        match marker {
            MARKER_UNDEFINED => Ok(Amf3Value::Undefined),
            MARKER_NULL => Ok(Amf3Value::Null),
            MARKER_FALSE => Ok(Amf3Value::Boolean(false)),
            MARKER_TRUE => Ok(Amf3Value::Boolean(true)),
            MARKER_INTEGER => self.decode_integer(),
            MARKER_DOUBLE => self.decode_double(),
            MARKER_STRING => self.decode_string(),
            MARKER_XML_DOCUMENT => self.decode_xml_document(),
            MARKER_DATE => self.decode_date(),
            MARKER_ARRAY => self.decode_array(),
            MARKER_OBJECT => self.decode_object(),
            MARKER_XML => self.decode_xml(),
            MARKER_BYTE_ARRAY => self.decode_byte_array(),
            MARKER_INT_VECTOR => self.decode_int_vector(),
            MARKER_UINT_VECTOR => self.decode_uint_vector(),
            MARKER_DOUBLE_VECTOR => self.decode_double_vector(),
            MARKER_OBJECT_VECTOR => self.decode_object_vector(),
            MARKER_DICTIONARY => self.decode_dictionary(),
            _ => Err(Error::invalid_data(format!(
                "unknown AMF3 marker: {}",
                marker
            ))),
        }
    }

    fn decode_integer(&mut self) -> Result<Amf3Value, Error> {
        let n = self.decode_u29()? as i32;
        let n = if n >= (1 << 28) { n - (1 << 29) } else { n };
        Ok(Amf3Value::Integer(n))
    }

    fn decode_double(&mut self) -> Result<Amf3Value, Error> {
        let n = self.buf.read_f64()?;
        Ok(Amf3Value::Double(n))
    }

    fn decode_string(&mut self) -> Result<Amf3Value, Error> {
        let s = self.decode_utf8()?;
        Ok(Amf3Value::String(s))
    }

    fn decode_xml_document(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, len| {
            let s = this.buf.read_utf8(len)?;
            Ok(Amf3Value::XmlDocument(s))
        })
    }

    fn decode_date(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, _len| {
            let millis = this.buf.read_f64()?;
            if !millis.is_finite() || millis.is_sign_negative() {
                return Err(Error::invalid_data(format!(
                    "invalid date millis: {}",
                    millis
                )));
            }
            Ok(Amf3Value::Date {
                unix_time: Duration::from_millis(millis as u64),
            })
        })
    }

    fn decode_array(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, count| {
            let assoc_entries = this.decode_pairs()?;
            let dense_entries = (0..count)
                .map(|_| this.decode_value())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Amf3Value::Array {
                assoc_entries,
                dense_entries,
            })
        })
    }

    fn decode_object(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, u28| {
            let trait_def = this.decode_trait(u28)?;
            let mut entries = trait_def
                .fields
                .iter()
                .map(|key| {
                    Ok(Pair {
                        key: key.clone(),
                        value: this.decode_value()?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            if trait_def.is_dynamic {
                entries.extend(this.decode_pairs()?);
            }

            Ok(Amf3Value::Object {
                class_name: trait_def.class_name,
                sealed_count: trait_def.fields.len(),
                entries,
            })
        })
    }

    fn decode_xml(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, len| {
            let s = this.buf.read_utf8(len)?;
            Ok(Amf3Value::Xml(s))
        })
    }

    fn decode_byte_array(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, len| {
            let bytes = this.buf.read_bytes(len)?;
            Ok(Amf3Value::ByteArray(bytes))
        })
    }

    fn decode_int_vector(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, count| {
            let is_fixed = this.buf.read_u8()? != 0;
            let entries = (0..count)
                .map(|_| this.buf.read_i32())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Amf3Value::IntVector { is_fixed, entries })
        })
    }

    fn decode_uint_vector(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, count| {
            let is_fixed = this.buf.read_u8()? != 0;
            let entries = (0..count)
                .map(|_| this.buf.read_u32())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Amf3Value::UintVector { is_fixed, entries })
        })
    }

    fn decode_double_vector(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, count| {
            let is_fixed = this.buf.read_u8()? != 0;
            let entries = (0..count)
                .map(|_| this.buf.read_f64())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Amf3Value::DoubleVector { is_fixed, entries })
        })
    }

    fn decode_object_vector(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, count| {
            let is_fixed = this.buf.read_u8()? != 0;
            let class_name = this.decode_utf8()?;
            let entries = (0..count)
                .map(|_| this.decode_value())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Amf3Value::ObjectVector {
                class_name: if class_name == "*" {
                    None
                } else {
                    Some(class_name)
                },
                is_fixed,
                entries,
            })
        })
    }

    fn decode_dictionary(&mut self) -> Result<Amf3Value, Error> {
        self.decode_complex_type(|this, count| {
            let is_weak = this.buf.read_u8()? == 1;
            let entries = (0..count)
                .map(|_| {
                    Ok(Pair {
                        key: this.decode_value()?,
                        value: this.decode_value()?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Amf3Value::Dictionary { is_weak, entries })
        })
    }

    fn decode_utf8(&mut self) -> Result<String, Error> {
        match self.decode_size_or_index()? {
            SizeOrIndex::Size(len) => {
                let s = self.buf.read_utf8(len)?;
                if !s.is_empty() {
                    self.strings.push(s.clone());
                }
                Ok(s)
            }
            SizeOrIndex::Index(idx) => self.strings.get(idx).cloned().ok_or_else(|| {
                Error::invalid_data(format!("string reference out of range: {idx}"))
            }),
        }
    }

    fn decode_u29(&mut self) -> Result<u32, Error> {
        let mut n = 0;
        for _ in 0..3 {
            let b = self.buf.read_u8()? as u32;
            n = (n << 7) | (b & 0b0111_1111);
            if (b & 0b1000_0000) == 0 {
                return Ok(n);
            }
        }
        let b = self.buf.read_u8()? as u32;
        n = (n << 8) | b;
        Ok(n)
    }

    fn decode_size_or_index(&mut self) -> Result<SizeOrIndex, Error> {
        let u29 = self.decode_u29()? as usize;
        let is_reference = (u29 & 1) == 0;
        let value = u29 >> 1;
        Ok(if is_reference {
            SizeOrIndex::Index(value)
        } else {
            SizeOrIndex::Size(value)
        })
    }

    fn decode_complex_type<F>(&mut self, f: F) -> Result<Amf3Value, Error>
    where
        F: FnOnce(&mut Self, usize) -> Result<Amf3Value, Error>,
    {
        match self.decode_size_or_index()? {
            SizeOrIndex::Index(idx) => {
                let val = self.complexes.get(idx).ok_or_else(|| {
                    Error::invalid_data(format!("complex reference out of range: {}", idx))
                })?;
                if *val == Amf3Value::Null {
                    Err(Error::unsupported(format!(
                        "circular reference at index: {idx}",
                    )))
                } else {
                    Ok(val.clone())
                }
            }
            SizeOrIndex::Size(len) => {
                let idx = self.complexes.len();
                self.complexes.push(Amf3Value::Null);
                let value = f(self, len)?;
                self.complexes[idx] = value.clone();
                Ok(value)
            }
        }
    }

    fn decode_pairs(&mut self) -> Result<Vec<Pair<String, Amf3Value>>, Error> {
        let mut pairs = Vec::new();
        loop {
            let key = self.decode_utf8()?;
            if key.is_empty() {
                break;
            }
            let value = self.decode_value()?;
            pairs.push(Pair { key, value });
        }
        Ok(pairs)
    }

    fn decode_trait(&mut self, u28: usize) -> Result<Trait, Error> {
        if (u28 & 1) == 0 {
            let idx = u28 >> 1;
            self.traits.get(idx).cloned().ok_or_else(|| {
                Error::invalid_data(format!("trait reference out of range: {}", idx))
            })
        } else if (u28 & 2) != 0 {
            Err(Error::unsupported("externalizable types not supported"))
        } else {
            let is_dynamic = (u28 & 4) != 0;
            let field_count = u28 >> 3;
            let class_name = self.decode_utf8()?;
            let fields = (0..field_count)
                .map(|_| self.decode_utf8())
                .collect::<Result<Vec<_>, _>>()?;

            let trait_def = Trait {
                class_name: if class_name.is_empty() {
                    None
                } else {
                    Some(class_name)
                },
                is_dynamic,
                fields,
            };
            self.traits.push(trait_def.clone());
            Ok(trait_def)
        }
    }
}

struct Encoder<'a> {
    buf: &'a mut Vec<u8>,
}

impl<'a> Encoder<'a> {
    fn encode_value(&mut self, value: &Amf3Value) {
        match value {
            Amf3Value::Undefined => self.buf.write_u8(MARKER_UNDEFINED),
            Amf3Value::Null => self.buf.write_u8(MARKER_NULL),
            Amf3Value::Boolean(b) => self
                .buf
                .write_u8(if *b { MARKER_TRUE } else { MARKER_FALSE }),
            Amf3Value::Integer(i) => self.encode_integer(*i),
            Amf3Value::Double(d) => self.encode_double(*d),
            Amf3Value::String(s) => self.encode_string(s),
            Amf3Value::XmlDocument(s) => self.encode_xml_document(s),
            Amf3Value::Date { unix_time } => self.encode_date(*unix_time),
            Amf3Value::Array {
                assoc_entries,
                dense_entries,
            } => self.encode_array(assoc_entries, dense_entries),
            Amf3Value::Object {
                class_name,
                sealed_count,
                entries,
            } => self.encode_object(class_name, *sealed_count, entries),
            Amf3Value::Xml(s) => self.encode_xml(s),
            Amf3Value::ByteArray(bytes) => self.encode_byte_array(bytes),
            Amf3Value::IntVector { is_fixed, entries } => {
                self.encode_int_vector(*is_fixed, entries)
            }
            Amf3Value::UintVector { is_fixed, entries } => {
                self.encode_uint_vector(*is_fixed, entries)
            }
            Amf3Value::DoubleVector { is_fixed, entries } => {
                self.encode_double_vector(*is_fixed, entries)
            }
            Amf3Value::ObjectVector {
                class_name,
                is_fixed,
                entries,
            } => self.encode_object_vector(class_name, *is_fixed, entries),
            Amf3Value::Dictionary { is_weak, entries } => self.encode_dictionary(*is_weak, entries),
        }
    }

    fn encode_integer(&mut self, i: i32) {
        // i が 29 bit の範囲内に収まっているかどうかを assert! でチェックする
        //
        // [NOTE]
        // AMF のエンコードは crate 内部のみで行われ、外部には公開されていないので、
        // これを正しく呼ぶのは呼び出し側の責務として、ここでは条件に違反した場合は単純にパニックしている
        assert!(
            (-(1i32 << 28)..(1i32 << 28)).contains(&i),
            "integer must fit in 29-bit signed range, got: {}",
            i
        );

        self.buf.write_u8(MARKER_INTEGER);
        let u29 = if i >= 0 {
            i as u32
        } else {
            ((1u32 << 29) as i32 + i) as u32
        };
        self.encode_u29(u29);
    }

    fn encode_double(&mut self, d: f64) {
        self.buf.write_u8(MARKER_DOUBLE);
        self.buf.write_f64(d);
    }

    fn encode_string(&mut self, s: &str) {
        self.buf.write_u8(MARKER_STRING);
        self.encode_utf8(s);
    }

    fn encode_xml_document(&mut self, xml: &str) {
        self.buf.write_u8(MARKER_XML_DOCUMENT);
        self.encode_utf8(xml);
    }

    fn encode_date(&mut self, unix_time: Duration) {
        self.buf.write_u8(MARKER_DATE);
        self.encode_size(0);
        self.buf.write_f64(unix_time.as_millis() as f64);
    }

    fn encode_array(
        &mut self,
        assoc_entries: &[Pair<String, Amf3Value>],
        dense_entries: &[Amf3Value],
    ) {
        self.buf.write_u8(MARKER_ARRAY);
        self.encode_size(dense_entries.len());
        self.encode_pairs(assoc_entries);
        for entry in dense_entries {
            self.encode_value(entry);
        }
    }

    fn encode_object(
        &mut self,
        class_name: &Option<String>,
        sealed_count: usize,
        entries: &[Pair<String, Amf3Value>],
    ) {
        self.buf.write_u8(MARKER_OBJECT);
        self.encode_trait(class_name, sealed_count, entries);
        for entry in entries.iter().take(sealed_count) {
            self.encode_value(&entry.value);
        }
        if entries.len() > sealed_count {
            self.encode_pairs(&entries[sealed_count..]);
        }
    }

    fn encode_xml(&mut self, xml: &str) {
        self.buf.write_u8(MARKER_XML);
        self.encode_utf8(xml);
    }

    fn encode_byte_array(&mut self, bytes: &[u8]) {
        self.buf.write_u8(MARKER_BYTE_ARRAY);
        self.encode_size(bytes.len());
        self.buf.write_bytes(bytes);
    }

    fn encode_int_vector(&mut self, is_fixed: bool, entries: &[i32]) {
        self.buf.write_u8(MARKER_INT_VECTOR);
        self.encode_size(entries.len());
        self.buf.write_u8(if is_fixed { 1 } else { 0 });
        for &entry in entries {
            self.buf.write_i32(entry);
        }
    }

    fn encode_uint_vector(&mut self, is_fixed: bool, entries: &[u32]) {
        self.buf.write_u8(MARKER_UINT_VECTOR);
        self.encode_size(entries.len());
        self.buf.write_u8(if is_fixed { 1 } else { 0 });
        for &entry in entries {
            self.buf.write_u32(entry);
        }
    }

    fn encode_double_vector(&mut self, is_fixed: bool, entries: &[f64]) {
        self.buf.write_u8(MARKER_DOUBLE_VECTOR);
        self.encode_size(entries.len());
        self.buf.write_u8(if is_fixed { 1 } else { 0 });
        for &entry in entries {
            self.buf.write_f64(entry);
        }
    }

    fn encode_object_vector(
        &mut self,
        class_name: &Option<String>,
        is_fixed: bool,
        entries: &[Amf3Value],
    ) {
        self.buf.write_u8(MARKER_OBJECT_VECTOR);
        self.encode_size(entries.len());
        self.buf.write_u8(if is_fixed { 1 } else { 0 });
        self.encode_utf8(class_name.as_ref().map_or("*", |s| s));
        for entry in entries {
            self.encode_value(entry);
        }
    }

    fn encode_dictionary(&mut self, is_weak: bool, entries: &[Pair<Amf3Value, Amf3Value>]) {
        self.buf.write_u8(MARKER_DICTIONARY);
        self.encode_size(entries.len());
        self.buf.write_u8(if is_weak { 1 } else { 0 });
        for entry in entries {
            self.encode_value(&entry.key);
            self.encode_value(&entry.value);
        }
    }

    fn encode_utf8(&mut self, s: &str) {
        self.encode_size(s.len());
        self.buf.write_bytes(s.as_bytes());
    }

    fn encode_size(&mut self, size: usize) {
        let u29 = ((size << 1) | 1) as u32;
        self.encode_u29(u29);
    }

    fn encode_u29(&mut self, u29: u32) {
        if u29 < 0x80 {
            self.buf.write_u8(u29 as u8);
        } else if u29 < 0x4000 {
            let b1 = (u29 & 0x7F) as u8;
            let b2 = ((u29 >> 7) | 0x80) as u8;
            self.buf.write_u8(b2);
            self.buf.write_u8(b1);
        } else if u29 < 0x200000 {
            let b1 = (u29 & 0x7F) as u8;
            let b2 = ((u29 >> 7) | 0x80) as u8;
            let b3 = ((u29 >> 14) | 0x80) as u8;
            self.buf.write_u8(b3);
            self.buf.write_u8(b2);
            self.buf.write_u8(b1);
        } else {
            let b1 = (u29 & 0xFF) as u8;
            let b2 = ((u29 >> 8) | 0x80) as u8;
            let b3 = ((u29 >> 15) | 0x80) as u8;
            let b4 = ((u29 >> 22) | 0x80) as u8;
            self.buf.write_u8(b4);
            self.buf.write_u8(b3);
            self.buf.write_u8(b2);
            self.buf.write_u8(b1);
        }
    }

    fn encode_pairs(&mut self, pairs: &[Pair<String, Amf3Value>]) {
        for pair in pairs {
            self.encode_utf8(&pair.key);
            self.encode_value(&pair.value);
        }
        self.encode_utf8("");
    }

    fn encode_trait(
        &mut self,
        class_name: &Option<String>,
        sealed_count: usize,
        entries: &[Pair<String, Amf3Value>],
    ) {
        let is_dynamic = (sealed_count < entries.len()) as usize;
        let u28 = (sealed_count << 3) | (is_dynamic << 2) | 1;
        self.encode_size(u28);

        self.encode_utf8(class_name.as_ref().map_or("", |s| s));
        for entry in entries.iter().take(sealed_count) {
            self.encode_utf8(&entry.key);
        }
    }
}
