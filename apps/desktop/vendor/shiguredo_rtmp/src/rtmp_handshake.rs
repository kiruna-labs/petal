use alloc::vec::Vec;
use core::mem;

use crate::bytes::{BytesReader, BytesWriter};
use crate::error::Error;

// [NOTE]
// Flash Player が廃止された今となっては、
// ハンドシェイク時のパラメーターを細かく制御する意義はないと思われるので、
// 問題が発生するまでは固定値を使用しておく
const RTMP_VERSION: u8 = 3;
const HANDSHAKE_PACKET_SIZE: usize = 1536;
const APP_VERSION: [u8; 4] = [0, 0, 0, 0]; // 一番小さな値を使うことで、途中から導入されたダイジェスト形式のサポートを不要にしている
const RANDOM_DATA: [u8; HANDSHAKE_PACKET_SIZE - 8] = [0; HANDSHAKE_PACKET_SIZE - 8]; // 固定値で別に困らないので固定値にしておく
const TIMESTAMP: u32 = 0; // 同上

#[derive(Debug, Clone)]
struct RtmpHandshakeOptions {
    app_version: [u8; 4],
    timestamp: u32,
    random_data: [u8; HANDSHAKE_PACKET_SIZE - 8],
}

impl RtmpHandshakeOptions {
    fn phase1_packet(&self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(HANDSHAKE_PACKET_SIZE);

        packet.write_u32(self.timestamp);
        packet.write_u32(u32::from_be_bytes(self.app_version));
        packet.write_bytes(&self.random_data);

        packet
    }
}

impl Default for RtmpHandshakeOptions {
    fn default() -> Self {
        Self {
            app_version: APP_VERSION,
            timestamp: TIMESTAMP,
            random_data: RANDOM_DATA,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    #[default]
    P0,
    P1,
    P2,
    Complete,
}

#[derive(Debug)]
pub struct RtmpServerHandshake {
    options: RtmpHandshakeOptions,
    phase: Phase,
    recv_buf: Vec<u8>,
    send_buf: Vec<u8>,
}

impl RtmpServerHandshake {
    pub fn new() -> Self {
        Self {
            options: RtmpHandshakeOptions::default(),
            phase: Phase::P0,
            recv_buf: Vec::new(),
            send_buf: Vec::new(),
        }
    }

    pub fn feed_recv_buf(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.recv_buf.extend_from_slice(buf);
        self.handle_recv_buf()?;
        Ok(())
    }

    fn handle_recv_buf(&mut self) -> Result<(), Error> {
        match self.phase {
            Phase::P0 => self.handle_phase_p0(),
            Phase::P1 => self.handle_phase_p1(),
            Phase::P2 => self.handle_phase_p2(),
            Phase::Complete => Ok(()),
        }
    }

    fn handle_phase_p0(&mut self) -> Result<(), Error> {
        if self.recv_buf.is_empty() {
            // データが揃っていない
            return Ok(());
        }

        // RECV: C0
        let client_rtmp_version = self.recv_buf.read_u8()?;
        if client_rtmp_version != RTMP_VERSION {
            return Err(Error::invalid_data(format!(
                "invalid RTMP version: expected {RTMP_VERSION}, got {client_rtmp_version}"
            )));
        }

        // SEND: S0
        self.send_buf.write_u8(RTMP_VERSION);

        self.phase = Phase::P1;
        self.handle_phase_p1()
    }

    fn handle_phase_p1(&mut self) -> Result<(), Error> {
        if self.recv_buf.len() < HANDSHAKE_PACKET_SIZE {
            // データが揃っていない
            return Ok(());
        }

        // RECV: C1
        let c1_packet = self.recv_buf.read_bytes(HANDSHAKE_PACKET_SIZE)?;

        // SEND: S1, S2
        self.send_buf.write_bytes(&self.options.phase1_packet());
        self.send_buf.write_bytes(&c1_packet);

        self.phase = Phase::P2;
        self.handle_phase_p2()
    }

    fn handle_phase_p2(&mut self) -> Result<(), Error> {
        if self.recv_buf.len() < HANDSHAKE_PACKET_SIZE {
            // データが揃っていない
            return Ok(());
        }

        // RECV: C2
        let c2_packet = self.recv_buf.read_bytes(HANDSHAKE_PACKET_SIZE)?;
        if self.options.phase1_packet() != c2_packet {
            return Err(Error::invalid_data("C2 packet does not match S1 packet"));
        }

        self.phase = Phase::Complete;
        Ok(())
    }

    pub fn take_recv_buf(&mut self) -> Vec<u8> {
        mem::take(&mut self.recv_buf)
    }

    pub fn send_buf(&self) -> &[u8] {
        &self.send_buf
    }

    pub fn advance_send_buf(&mut self, n: usize) {
        let n = n.min(self.send_buf.len());
        self.send_buf.drain(..n); // NOTE: 非効率だけど性能を気にする箇所ではないのでシンプルさを優先している
    }

    pub fn is_recv_complete(&self) -> bool {
        self.phase == Phase::Complete
    }

    pub fn is_send_complete(&self) -> bool {
        self.phase == Phase::Complete && self.send_buf.is_empty()
    }
}

impl Default for RtmpServerHandshake {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct RtmpClientHandshake {
    options: RtmpHandshakeOptions,
    phase: Phase,
    recv_buf: Vec<u8>,
    send_buf: Vec<u8>,
}

impl RtmpClientHandshake {
    pub fn new() -> Self {
        let options = RtmpHandshakeOptions::default();
        let mut send_buf = Vec::new();

        // SEND: C0, C1
        send_buf.push(RTMP_VERSION);
        send_buf.extend_from_slice(&options.phase1_packet());

        Self {
            options,
            phase: Phase::P0,
            recv_buf: Vec::new(),
            send_buf,
        }
    }

    pub fn feed_recv_buf(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.recv_buf.extend_from_slice(buf);
        self.handle_recv_buf()?;
        Ok(())
    }

    fn handle_recv_buf(&mut self) -> Result<(), Error> {
        match self.phase {
            Phase::P0 => self.handle_phase_p0(),
            Phase::P1 => self.handle_phase_p1(),
            Phase::P2 | Phase::Complete => Ok(()),
        }
    }

    fn handle_phase_p0(&mut self) -> Result<(), Error> {
        if self.recv_buf.is_empty() {
            // データが揃っていない
            return Ok(());
        }

        // RECV: S0
        let server_rtmp_version = self.recv_buf.read_u8()?;
        if server_rtmp_version != RTMP_VERSION {
            return Err(Error::invalid_data(format!(
                "invalid RTMP version: expected {RTMP_VERSION}, got {server_rtmp_version}"
            )));
        }

        self.phase = Phase::P1;
        self.handle_phase_p1()
    }

    fn handle_phase_p1(&mut self) -> Result<(), Error> {
        if self.recv_buf.len() < HANDSHAKE_PACKET_SIZE * 2 {
            // S1とS2の両方を受信するまで待つ
            return Ok(());
        }

        // RECV: S1, S2
        let s1_packet = self.recv_buf.read_bytes(HANDSHAKE_PACKET_SIZE)?;
        let s2_packet = self.recv_buf.read_bytes(HANDSHAKE_PACKET_SIZE)?;
        if s2_packet != self.options.phase1_packet() {
            return Err(Error::invalid_data("S2 packet does not match C1 packet"));
        }

        // SEND: C2
        self.send_buf.extend_from_slice(&s1_packet);

        // クライアント側は明示的な phase2 への移行は不要
        self.phase = Phase::Complete;
        Ok(())
    }

    pub fn take_recv_buf(&mut self) -> Vec<u8> {
        mem::take(&mut self.recv_buf)
    }

    pub fn send_buf(&self) -> &[u8] {
        &self.send_buf
    }

    pub fn advance_send_buf(&mut self, n: usize) {
        let n = n.min(self.send_buf.len());
        self.send_buf.drain(..n); // NOTE: 非効率だけど性能を気にする箇所ではないのでシンプルさを優先している
    }

    pub fn is_recv_complete(&self) -> bool {
        self.phase == Phase::Complete
    }

    pub fn is_send_complete(&self) -> bool {
        self.phase == Phase::Complete && self.send_buf.is_empty()
    }
}

impl Default for RtmpClientHandshake {
    fn default() -> Self {
        Self::new()
    }
}
