use alloc::string::String;
use alloc::vec::Vec;

use crate::error::Error;

pub trait BytesReader {
    fn read_u8(&mut self) -> Result<u8, Error>;
    fn read_u16(&mut self) -> Result<u16, Error>;
    fn read_u24(&mut self) -> Result<u32, Error>;
    fn read_i24(&mut self) -> Result<i32, Error>;
    fn read_u32(&mut self) -> Result<u32, Error>;
    fn read_i32(&mut self) -> Result<i32, Error>;
    fn read_f64(&mut self) -> Result<f64, Error>;
    fn read_bytes(&mut self, len: usize) -> Result<Vec<u8>, Error>;
    fn read_utf8(&mut self, len: usize) -> Result<String, Error>;
}

impl BytesReader for &[u8] {
    #[track_caller]
    fn read_u8(&mut self) -> Result<u8, Error> {
        Error::check_buffer_size(1, self)?;
        let v = self[0];
        *self = &self[1..];
        Ok(v)
    }

    #[track_caller]
    fn read_u16(&mut self) -> Result<u16, Error> {
        Error::check_buffer_size(2, self)?;
        let bytes = [self[0], self[1]];
        *self = &self[2..];
        Ok(u16::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_u24(&mut self) -> Result<u32, Error> {
        Error::check_buffer_size(3, self)?;
        let bytes = [0, self[0], self[1], self[2]];
        *self = &self[3..];
        Ok(u32::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_i24(&mut self) -> Result<i32, Error> {
        Error::check_buffer_size(3, self)?;
        let bytes = [
            if self[0] & 0x80 != 0 { 0xFF } else { 0x00 },
            self[0],
            self[1],
            self[2],
        ];
        *self = &self[3..];
        Ok(i32::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_u32(&mut self) -> Result<u32, Error> {
        Error::check_buffer_size(4, self)?;
        let bytes = [self[0], self[1], self[2], self[3]];
        *self = &self[4..];
        Ok(u32::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_i32(&mut self) -> Result<i32, Error> {
        Error::check_buffer_size(4, self)?;
        let bytes = [self[0], self[1], self[2], self[3]];
        *self = &self[4..];
        Ok(i32::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_f64(&mut self) -> Result<f64, Error> {
        Error::check_buffer_size(8, self)?;
        let bytes = [
            self[0], self[1], self[2], self[3], self[4], self[5], self[6], self[7],
        ];
        *self = &self[8..];
        Ok(f64::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_bytes(&mut self, len: usize) -> Result<Vec<u8>, Error> {
        Error::check_buffer_size(len, self)?;
        let buf = self[..len].to_vec();
        *self = &self[len..];
        Ok(buf)
    }

    #[track_caller]
    fn read_utf8(&mut self, len: usize) -> Result<String, Error> {
        let buf = self.read_bytes(len)?;
        String::from_utf8(buf).map_err(|e| Error::invalid_data(format!("invalid UTF-8 bytes: {e}")))
    }
}

impl BytesReader for Vec<u8> {
    #[track_caller]
    fn read_u8(&mut self) -> Result<u8, Error> {
        Error::check_buffer_size(1, self)?;
        let v = self[0];
        // drain() で読み込んだ部分を削除する
        // 効率はよくないが、パフォーマンスが重要でないユースケース用
        self.drain(0..1);
        Ok(v)
    }

    #[track_caller]
    fn read_u16(&mut self) -> Result<u16, Error> {
        Error::check_buffer_size(2, self)?;
        let bytes = [self[0], self[1]];
        // 読み込んだ2バイトを削除
        self.drain(0..2);
        Ok(u16::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_u24(&mut self) -> Result<u32, Error> {
        Error::check_buffer_size(3, self)?;
        let bytes = [0, self[0], self[1], self[2]];
        // 読み込んだ3バイトを削除
        self.drain(0..3);
        Ok(u32::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_i24(&mut self) -> Result<i32, Error> {
        Error::check_buffer_size(3, self)?;
        let bytes = [
            if self[0] & 0x80 != 0 { 0xFF } else { 0x00 },
            self[0],
            self[1],
            self[2],
        ];
        // 読み込んだ3バイトを削除
        self.drain(0..3);
        Ok(i32::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_u32(&mut self) -> Result<u32, Error> {
        Error::check_buffer_size(4, self)?;
        let bytes = [self[0], self[1], self[2], self[3]];
        // 読み込んだ4バイトを削除
        self.drain(0..4);
        Ok(u32::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_i32(&mut self) -> Result<i32, Error> {
        Error::check_buffer_size(4, self)?;
        let bytes = [self[0], self[1], self[2], self[3]];
        // 読み込んだ4バイトを削除
        self.drain(0..4);
        Ok(i32::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_f64(&mut self) -> Result<f64, Error> {
        Error::check_buffer_size(8, self)?;
        let bytes = [
            self[0], self[1], self[2], self[3], self[4], self[5], self[6], self[7],
        ];
        // 読み込んだ8バイトを削除
        self.drain(0..8);
        Ok(f64::from_be_bytes(bytes))
    }

    #[track_caller]
    fn read_bytes(&mut self, len: usize) -> Result<Vec<u8>, Error> {
        Error::check_buffer_size(len, self)?;
        // drain() は イテレータを返すため、collect() で Vec に変換
        let buf: Vec<u8> = self.drain(0..len).collect();
        Ok(buf)
    }

    #[track_caller]
    fn read_utf8(&mut self, len: usize) -> Result<String, Error> {
        let buf = self.read_bytes(len)?;
        String::from_utf8(buf).map_err(|e| Error::invalid_data(format!("invalid UTF-8 bytes: {e}")))
    }
}

pub trait BytesWriter {
    fn write_u8(&mut self, v: u8);
    fn write_u16(&mut self, v: u16);
    fn write_u24(&mut self, v: u32);
    fn write_i24(&mut self, v: i32);
    fn write_u32(&mut self, v: u32);
    fn write_i32(&mut self, v: i32);
    fn write_f64(&mut self, v: f64);
    fn write_bytes(&mut self, v: &[u8]);
}

impl BytesWriter for Vec<u8> {
    fn write_u8(&mut self, v: u8) {
        self.push(v);
    }

    fn write_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_u24(&mut self, v: u32) {
        let bytes = v.to_be_bytes();
        self.extend_from_slice(&bytes[1..4]);
    }

    fn write_i24(&mut self, v: i32) {
        let bytes = v.to_be_bytes();
        self.extend_from_slice(&bytes[1..4]);
    }

    fn write_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_i32(&mut self, v: i32) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_f64(&mut self, v: f64) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_bytes(&mut self, v: &[u8]) {
        self.extend_from_slice(v);
    }
}

#[derive(Debug, Default)]
pub struct Buf {
    bytes: Vec<u8>,
    offset: usize,
}

impl Buf {
    pub fn get(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }

    pub fn inner_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    pub fn feed(&mut self, buf: &[u8]) {
        self.bytes.extend_from_slice(buf);

        // タイミングが悪くて bytes のサイズが延々と増えるのを防ぐために、
        // 1 MB を超えた場合には、データを先頭に移動して offset を 0 にする
        // （基本的にこの状況は発生しない想定だけど、保険のための処理）
        const MAX_SIZE: usize = 1024 * 1024; // 1 MB
        if self.bytes.len() > MAX_SIZE {
            let remaining = self.bytes.len() - self.offset;
            self.bytes.copy_within(self.offset.., 0);
            self.bytes.truncate(remaining);
            self.offset = 0;
        }
    }

    // [NOTE]
    // 呼び出しが `Buf::get().len()` よりも大きい値を n に指定した場合には、
    // 後続の処理でパニックなどをする可能性がある
    // （`Buf` は内部的な構造体なので、このメソッドを正しく使うのは呼び出し元の責務として、ここではエラーチェックをしていない）
    pub fn advance(&mut self, n: usize) {
        self.offset += n;
        if self.offset == self.bytes.len() {
            self.offset = 0;
            self.bytes.clear(); // [NOTE] bytes 自体の capacity は変わらない
        }
    }
}
