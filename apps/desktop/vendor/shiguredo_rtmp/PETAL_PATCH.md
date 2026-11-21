# Petal patch to `shiguredo_rtmp`

Vendored from crates.io `shiguredo_rtmp` **2026.1.0-canary.6**
(https://github.com/shiguredo/rtmp-rs, Apache-2.0) with the one fix described
below. Pinned via `[patch.crates-io]` in `apps/desktop/src-tauri/Cargo.toml`
(the pin and the only caller arrive with live streaming, kiruna-labs/petal#487;
until then nothing builds this directory).
The copy is the published `.crate` tarball extracted as-is (normalized
`Cargo.toml` plus `Cargo.toml.orig`, `LICENSE`, `README.md`, `src/`; the
tarball's generated `Cargo.lock` is left out); only
`src/rtmp_client_connection.rs` is modified. Tarball SHA-256:
`44a3ddf3112e8cfd27e5fd69f8b34d58dc4b7d62291a314a43cb1212b88a68bb`
(`https://static.crates.io/crates/shiguredo_rtmp/shiguredo_rtmp-2026.1.0-canary.6.crate`);
`diff -r` against it shows only that file and this one.

The server side (`src/rtmp_server_connection.rs`) is kept although the app
never runs an RTMP server: the publisher's tests drive the patched client
against an in-process `RtmpServerConnection` (including the regression test
below), and removing it would make this copy a fork rather than a vendor.

Petal uses this crate for live streaming's client-mode RTMP(S) publisher
(`apps/desktop/src-tauri/src/streaming/rtmp.rs`), as a sans-I/O library: Petal
owns the socket, TLS, timeouts and the reconnect schedule.

## Healthy streams dropped with "ACK not received within expected interval"

### The bug

After every write the client checks how many bytes the server has not yet
acknowledged, and disconnects once that exceeds twice its
`local_ack_window_size` (`RtmpMessageChannel::advance_send_buf` returns
`false`; `RtmpClientConnection::advance_send_buf` then queues
`DisconnectedByPeer { reason: "ACK not received within expected interval" }`
and moves to `Disconnecting`).

`local_ack_window_size` starts as the window the client itself advertised
(Window Acknowledgement Size, 5 MB by default, which is what governs how
often the server acknowledges), but the `SetPeerBandwidth` handler overwrites
it with the server's Set Peer Bandwidth size. That message limits the
client's output; it says nothing about the server's acknowledgement cadence.
YouTube's ingest advertises a Set Peer Bandwidth smaller than the interval it
actually acknowledges at, so the client gave up on healthy connections: a
live test to `rtmp://a.rtmp.youtube.com/live2` dropped eight times in five
minutes, after anything from 3 s to 100 s, with no network or capture trouble
in the log.

OBS (librtmp) and ffmpeg do not enforce acknowledgements at all.

### The fix

The client no longer disconnects over late acknowledgements; it still counts
the bytes (the return value is ignored). A dead connection is detected by
Petal instead: a 10 s write timeout and a 2 s media backlog limit
(`streaming/rtmp.rs`, `streaming/queue.rs`). The server side
(`rtmp_server_connection.rs`) is untouched.

Regression test: `streaming::rtmp::tests::keeps_publishing_when_the_server_acknowledges_less_often_than_it_advertised`
(an in-process server sends a 4 KB Set Peer Bandwidth and acknowledges every
2.5 MB; the unpatched client reconnects, the patched one keeps publishing).

### Upstream status

Unfixed in canary.6 (the latest release) and on upstream `develop` as of
2026-10-12; no upstream issue filed yet. When upgrading, check whether
upstream changed `RtmpClientConnection::advance_send_buf` or the
`SetPeerBandwidth` handler. If it no longer disconnects, drop this copy and
the `[patch.crates-io]` entry; otherwise re-vendor and reapply the diff below.

```diff
--- a/src/rtmp_client_connection.rs
+++ b/src/rtmp_client_connection.rs
@@ -473,18 +473,13 @@
     fn advance_send_buf(&mut self, n: usize) {
         if !self.handshake.is_send_complete() {
             self.handshake.advance_send_buf(n);
-        } else if !self.message_channel.advance_send_buf(n) {
-            // 相手から ACK が期待通りに届いていないので切断する
-            self.event_queue
-                .push_back(RtmpConnectionEvent::DisconnectedByPeer {
-                    reason: "ACK not received within expected interval".to_owned(),
-                });
-
-            // [NOTE]
-            // Disconnecting への遷移は常に成功するので、ここでは expect() を使ってしまう。
-            // 利用側の利便性を考えると、返り値を不必要に Result にしたくないため。
-            self.change_state(RtmpConnectionState::Disconnecting)
-                .expect("infallible");
+        } else {
+            // PETAL PATCH (see PETAL_PATCH.md): never disconnect over late
+            // acknowledgements. Servers such as YouTube's ingest advertise a
+            // Set Peer Bandwidth smaller than the interval they acknowledge
+            // at, so this check dropped healthy streams. Like OBS and ffmpeg,
+            // a client relies on its own write timeouts to detect a dead link.
+            let _ = self.message_channel.advance_send_buf(n);
         }
     }

```
