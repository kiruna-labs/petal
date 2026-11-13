# rtmp-rs

[![shiguredo_rtmp](https://img.shields.io/crates/v/shiguredo_rtmp.svg)](https://crates.io/crates/shiguredo_rtmp)
[![Documentation](https://docs.rs/shiguredo_rtmp/badge.svg)](https://docs.rs/shiguredo_rtmp)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

## About Shiguredo's open source software

We will not respond to PRs or issues that have not been discussed on Discord. Also, Discord is only available in Japanese.

Please read <https://github.com/shiguredo/oss> before use.

## 時雨堂のオープンソースソフトウェアについて

利用前に <https://github.com/shiguredo/oss> をお読みください。

## 概要

Rust で実装された依存 0 かつ `no_std` に対応した Sans I/O な RTMP ライブラリです。

## 特徴

- Sans I/O
- 依存ライブラリ 0
- `no_std` 対応

## 使い方

### クライアント (配信)

```rust
use shiguredo_rtmp::{RtmpPublishClientConnection, VideoFrame, AudioFrame};

// 配信用クライアント接続を作成
let mut connection = RtmpPublishClientConnection::new(
    "rtmp://localhost:1935/live",
    "stream"
);

// 送信バッファを取得してソケットに書き込む
let send_data = connection.send_buf();
// send_data を送信...
connection.advance_send_buf(send_data.len());

// 受信データを feed
// connection.feed_recv_buf(&received_data)?;

// イベントを処理
// while let Some(event) = connection.next_event() { ... }

// 映像/音声フレームを送信
// connection.send_video(video_frame)?;
// connection.send_audio(audio_frame)?;
```

### クライアント (再生)

```rust
use shiguredo_rtmp::RtmpPlayClientConnection;

// 再生用クライアント接続を作成
let mut connection = RtmpPlayClientConnection::new(
    "rtmp://localhost:1935/live",
    "stream"
);

// 送信バッファを取得してソケットに書き込む
let send_data = connection.send_buf();
// send_data を送信...
connection.advance_send_buf(send_data.len());

// 受信データを feed
// connection.feed_recv_buf(&received_data)?;

// イベントを処理 (映像/音声フレーム受信など)
// while let Some(event) = connection.next_event() { ... }
```

### サーバー

```rust
use shiguredo_rtmp::{RtmpServerConnection, RtmpConnectionEvent};

// サーバー接続を作成
let mut connection = RtmpServerConnection::new();

// 受信データを feed
// connection.feed_recv_buf(&received_data)?;

// イベントを処理
while let Some(event) = connection.next_event() {
    match event {
        RtmpConnectionEvent::PublishRequested { app, stream_name, .. } => {
            // 配信要求を許可/拒否
            // connection.accept()?;
            // connection.reject("reason")?;
        }
        RtmpConnectionEvent::PlayRequested { app, stream_name, .. } => {
            // 再生要求を許可/拒否
            // connection.accept()?;
        }
        RtmpConnectionEvent::VideoReceived(frame) => {
            // 映像フレームを受信
        }
        RtmpConnectionEvent::AudioReceived(frame) => {
            // 音声フレームを受信
        }
        _ => {}
    }
}

// 送信バッファを取得してソケットに書き込む
let send_data = connection.send_buf();
// send_data を送信...
connection.advance_send_buf(send_data.len());
```

## 公開 API

### 接続

| 構造体 | 説明 |
|--------|------|
| `RtmpPublishClientConnection` | 配信用クライアント接続 |
| `RtmpPlayClientConnection` | 再生用クライアント接続 |
| `RtmpServerConnection` | サーバー接続 |

### イベント

| 列挙型 | 説明 |
|--------|------|
| `RtmpConnectionEvent` | 接続イベント (配信/再生要求、フレーム受信など) |
| `RtmpConnectionState` | 接続状態 (Handshaking, Connecting, Publishing など) |

### メディア

| 型 | 説明 |
|--------|------|
| `VideoFrame` | 映像フレーム |
| `AudioFrame` | 音声フレーム |
| `MediaFrame` | メディアフレーム (Audio/Video の enum) |
| `VideoCodec` | 映像コーデック |
| `VideoFrameType` | 映像フレームタイプ (Keyframe, Inter など) |
| `AudioFormat` | 音声フォーマット |
| `AudioSampleRate` | 音声サンプルレート |
| `AvcPacketType` | AVC パケットタイプ |

### タイムスタンプ

| 型 | 説明 |
|--------|------|
| `RtmpTimestamp` | タイムスタンプ (ミリ秒単位、符号なし 32 ビット) |
| `RtmpTimestampDelta` | タイムスタンプ差分 (ミリ秒単位、符号付き 32 ビット) |

### エラー

| 型 | 説明 |
|--------|------|
| `Error` | エラー型 (kind, reason, location を含む) |
| `ErrorKind` | エラー種別 (InvalidInput, InvalidData, InvalidState, Unsupported) |

## サンプル

サンプルは [Tokio](https://github.com/tokio-rs/tokio) と [Rustls](https://github.com/rustls/rustls) を利用しています。引数のライブラリには [noargs](https://github.com/sile/noargs) を利用しています。

### publish

MP4 ファイルを RTMP/RTMPS サーバーに配信するクライアントの例です。

```bash
# RTMP
cargo run -p publish -- -H 127.0.0.1 -p 1935 -a live -s stream input.mp4

# RTMPS (--tls フラグ)
cargo run -p publish -- -H example.com -p 443 -a live -s stream --tls input.mp4

# RTMPS (URL スキーム)
cargo run -p publish -- -u rtmps://example.com/live/stream input.mp4
```

**オプション:**

- `-H, --host <HOST>`: サーバーホスト (デフォルト: 127.0.0.1)
- `-p, --port <PORT>`: サーバーポート (デフォルト: 1935、TLS 時: 443)
- `-a, --app <APP>`: アプリケーション名 (デフォルト: live)
- `-s, --stream <STREAM>`: ストリーム名 (デフォルト: stream)
- `-u, --url <URL>`: RTMP/RTMPS URL (例: rtmps://example.com/live/stream)
- `--tls`: TLS (RTMPS) 有効化
- `--verbose`: 詳細出力

### server

RTMP/RTMPS サーバーの例です。

```bash
# RTMP
cargo run -p server -- -H 0.0.0.0 -p 1935

# RTMPS
cargo run -p server -- --tls --cert cert.pem --key key.pem -p 443
```

**オプション:**

- `-H, --host <HOST>`: リッスンホスト (デフォルト: 0.0.0.0)
- `-p, --port <PORT>`: リッスンポート (デフォルト: 1935、TLS 時: 443)
- `--tls`: TLS (RTMPS) 有効化
- `--cert <PATH>`: 証明書ファイル (PEM 形式)
- `--key <PATH>`: 秘密鍵ファイル (PEM 形式)
- `--verbose`: 詳細出力

## ライセンス

Apache License 2.0

```text
Copyright 2026-2026, Takeru Ohta (Original Author)
Copyright 2026-2026, Shiguredo Inc.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```
