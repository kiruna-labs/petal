//! Transport: LiveKit Cloud room connect + publish/subscribe (SPEC.md §3, §10).
//!
//! Reality check for SPEC.md §10: Petal is LiveKit-coupled today. There is no
//! `Transport` trait, and provider swapping is NOT a contained change yet.
//! `RoomConnection::room()` intentionally exposes the raw `livekit::Room`, and
//! room-event watchers (presence, telepointers, remote control, resilience,
//! diagnostics, audio, subscriber) subscribe to `livekit::RoomEvent` directly.
//!
//! This module is still the right home for LiveKit-specific connect/publish/
//! subscribe/token code, but a future SFU swap would first need a real
//! transport-owned façade: a `RoomHandle`, neutral room/data/media events, and
//! watcher APIs that never pattern-match LiveKit types. Until that exists,
//! treat LiveKit as an explicit architectural dependency rather than a hidden
//! implementation detail.
//!
//! ## Frame metadata / SPEC.md §7 measurement hooks
//!
//! LiveKit's own `FrameMetadataFeatures` (user_timestamp + frame_id) is used
//! instead of hand-rolling a pixel-burned counter: the SDK carries a
//! `capture_timestamp_us` (we set it to the wall-clock time we read the
//! frame from ScreenCaptureKit) and a monotonic `frame_id` through its RTP
//! packet trailer end-to-end, decodable on the subscriber side via
//! `frame.frame_metadata`. This *is* "the embedded timestamp/counter in
//! frames" SPEC.md §7 asks M0 to design in -- it rides in the transport
//! layer's own metadata channel rather than being visually burned into
//! pixels, which is more precise (no OCR/decode step) and exactly what the
//! `local_video` example in livekit/rust-sdks demonstrates for this exact
//! purpose (its `--attach-timestamp`/`--attach-frame-id` flags).

pub mod audio;
// Shared backend HTTP client + error decoder (#143), used by token + rooms.
pub mod backend_http;
// Native webcam capture: shared core + the thin platform adapters
// (`camera::mf` Windows Media Foundation, `camera::avf` macOS AVFoundation —
// each carries its own #![cfg] gate).
pub mod camera;
pub mod publisher;
// Receiver-side authoritative publication reconciliation (#298).
pub mod reconcile;
pub mod room_directory;
pub mod subscriber;
pub mod token;

pub use audio::{enable_managed_playout, AudioError, MicTrack};
pub use publisher::{CaptureResolution, PublishedTrack, RoomConnection, ShareQuality};
pub use subscriber::Subscriber;
#[cfg(any(test, debug_assertions))]
pub use token::mint_access_token;

// Historical M0 linker trap, now fixed: LiveKit requires `-ObjC`, which used
// to expose duplicate `CoreMediaBridge` Swift symbols between `apple-cf` and
// `screencapturekit`. The repo now vendors a patched `screencapturekit` and
// Cargo patches crates.io to it; see `vendor/screencapturekit/PETAL_PATCH.md`.
// Keep this short here so nobody "rediscovers" an already-resolved blocker.
