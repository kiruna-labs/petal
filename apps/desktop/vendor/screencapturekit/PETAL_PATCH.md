# Petal patches to `screencapturekit` 8.0.0

Vendored from `screencapturekit` 8.0.0 (crates.io), with the two patches
described below. Pinned via `[patch.crates-io]` in
`apps/desktop/src-tauri/Cargo.toml`.

## Final-owner `SCStream` teardown (#548)

`SCStream::clone` previously retained the native stream and `StreamContext`,
while every clone's `Drop` called the Swift bridge's `sc_stream_release`.
That bridge removes the stream's sole `StreamState`, which owns its callback
delegate and output handler, before releasing one native retain. Dropping any
temporary clone therefore stopped frame callbacks even while other
`SCStream` owners remained alive.

Petal wraps the native stream pointer and callback context in one
`Arc<SCStreamInner>`. Clones share that owner, and only the inner object's final
drop calls `sc_stream_release` followed by `StreamContext::release`. This keeps
the Swift callback state alive until the last Rust stream handle is gone.

## Duplicate `CoreMediaBridge` Swift module

### Why this exists

`livekit`/`webrtc-sys` require the `-ObjC` linker flag on macOS (both already
emit `cargo:rustc-link-arg=-ObjC` from their own `build.rs` as of the
currently-pinned `livekit 0.7.49` / `webrtc-sys 0.3.35` — no manual RUSTFLAGS
needed, upstream issue livekit/rust-sdks#795 was already fixed by #847 before
this version). `-ObjC` forces the linker to whole-archive-load every static
archive containing any Objective-C/Swift class metadata.

Once forced, two archives collide:
- `apple-cf` 0.9.3's Swift Package target `CoreMediaBridge`
  (`swift-bridge/Sources/CoreMediaBridge/CoreMedia.swift`)
- `screencapturekit` 8.0.0's own Swift Package target, *also* literally named
  `CoreMediaBridge` (`swift-bridge/Sources/CoreMedia/CoreMedia.swift` — source
  dir differs, target name does not)

Both declare byte-identical `public struct AudioBufferBridge { ... }` and
`public struct AudioBufferListRaw { ... }`. `screencapturekit`'s own
`Package.swift` comment confirms this is a leftover, half-finished migration:
other bridge targets (CoreGraphics/CoreVideo/IOSurface/Dispatch) were already
"extracted into apple-cf-rs's bridge"; CoreMedia's generic accessors were
supposed to follow but the two duplicate audio-buffer structs were never
removed from `screencapturekit`'s copy. Under normal linking this is
invisible (dead-stripping never loads both full `.o` files); `-ObjC` defeats
that and the linker reports ~32 duplicate Swift type-metadata symbols.

Confirmed this is exactly this pair (not a `apple-cf` version mismatch):
`cargo tree -i apple-cf` shows a single `apple-cf` version in the whole
dependency graph.

### The fix

Renamed, in this vendored copy only, in
`swift-bridge/Sources/CoreMedia/CoreMedia.swift`:
- `AudioBufferBridge` -> `SCKAudioBufferBridge`
- `AudioBufferListRaw` -> `SCKAudioBufferListRaw`

Nothing else changed. This is safe because:
- The two `@_cdecl` C functions that build/consume these structs
  (`cm_sample_buffer_get_audio_buffer_list`,
  `cm_sample_buffer_get_audio_buffer_list_num_buffers`) keep their exact
  `@_cdecl` symbol names and C-callable signatures.
- Rust (`src/cm/audio.rs`, `src/cm/ffi.rs`) only crosses the FFI boundary
  through those C symbol names and its own `#[repr(C)]` `AudioBuffer`/
  `AudioBufferList` types (matched by field layout, not by Swift type name)
  — it never references the Swift struct names `AudioBufferBridge`/
  `AudioBufferListRaw` directly, so renaming them on the Swift side has zero
  effect on the Rust API surface (`window_source.rs` and friends are
  unaffected; window enumeration doesn't touch this code path at all — it's
  audio-sample-buffer only).
- Field layout (`number_channels: UInt32`, `data_bytes_size: UInt32`,
  `data_ptr: UnsafeMutableRawPointer?`, in that order) is untouched, so the
  memory layout Rust reads via `from_raw_parts` is unchanged.

## Re-vendoring / updating

If `screencapturekit` publishes a new version that fixes this upstream, drop
this vendor dir and the `[patch.crates-io]` entry, and bump the version
requirement in `src-tauri/Cargo.toml` instead. To re-vendor a newer version
with the same patches, re-apply the final-owner `SCStream` ownership change and
diff this crate's `CoreMedia.swift` against the new release's copy before
re-applying the same two-symbol rename.
