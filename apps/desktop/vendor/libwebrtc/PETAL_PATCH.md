# Petal patches to libwebrtc

Vendored from `libwebrtc` 0.3.38 (crates.io), pinned via
`[patch.crates-io]` in `apps/desktop/src-tauri/Cargo.toml`.

## Receiver jitter-buffer minimum playout delay

### Why this exists

The underlying FFI already exposes the receiver jitter-buffer minimum-delay
setter, but the safe Rust wrapper did not expose it. Petal needs to set the
receiver playout delay during controlled latency measurements without changing
the default behavior.

### The fix

This patch adds the purely additive public
`RtpReceiver::set_jitter_buffer_minimum_delay(Option<f64>) -> bool` method.
`None` clears the explicit minimum; `Some(seconds)` sets one. Its boolean
return makes the native-handle outcome observable: `true` means the request was
passed to the native receiver, while `false` means no native receiver handle was
available.

### Updating

Drop this patch once upstream exposes an equivalent safe receiver
playout-delay API with an observable outcome.

## Native video sources emit only captured frames

### Why this exists

The upstream `NativeVideoSource` injects a zero-filled I420 frame every 100 ms
until its first real capture. Petal deliberately publishes the camera track
before starting its frame pump so subscribers can negotiate before the first
H.264 keyframe; the injected frames therefore reached every receiver as a green
camera image during startup.

### The fix

This patch removes the synthetic-frame loop and its bookkeeping. A native
source now stays idle until its owner supplies a real frame, so the first
encoded keyframe always contains captured content.

### Updating

Keep this patch until upstream native sources no longer generate placeholder
frames before capture.

## The native WebRTC log sink preserves severity (#787)

### Why this exists

`PeerConnectionFactory::default()` installs the one process-wide sink that every
`RTC_LOG` line from native WebRTC — including the vendored C++ Petal compiles
itself, `webrtc-sys/src/adm_proxy.cpp` — passes through. Upstream's callback
discarded the `LoggingSeverity` argument (`|msg, _|`) and emitted everything at
`log::debug!` on target `libwebrtc`.

`logging.rs` denylists `libwebrtc` to `warn`, and the app's default level is
`info`, so *every* native line was dropped before reaching `petal.log` and none
could ever become a Sentry event. Measured on a real 8.5 MB `~/Library/Logs/
Petal/petal.log` spanning many sessions: zero records with target `libwebrtc`,
zero containing `AdmProxy`. That made `adm_proxy.cpp`'s `RTC_LOG(LS_ERROR)`
playout-failure lines — added for #787 precisely so a silent meeting would name
its own mechanism — unreachable at any default configuration.

### The fix

Severity is mapped instead of discarded: `Error` → `log::Level::Error`,
`Warning` → `Warn`, `Info` → `Debug`, everything else → `Trace`. `Info`/
`Verbose` stay below the default filter deliberately (native WebRTC emits those
per packet on some paths); disk use of the newly-admitted `warn`/`error` volume
is bounded by `logging.rs`'s 10 MB rotation. The callback is a named `fn`
(`emit_webrtc_log`) rather than a closure so the real path is callable from a
test, and `webrtc_log_level` is public for the same reason.

Pinned from the app side by `logging.rs`'s
`native_webrtc_error_lines_survive_the_default_filter`, which fails if a vendor
bump restores the severity-discarding form.

### Updating

Drop this patch once upstream maps severity onto `log` levels itself.
