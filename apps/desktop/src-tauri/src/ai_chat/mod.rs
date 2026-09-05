//! AI chat (Gemini Live on a shared window) — issue #656 feature, #654 spike.
//!
//! The session runs on the SHARER's machine: it has pixel-perfect frames, the
//! accessibility tree, and (later) the input-replay machinery for agent control.
//! This module is being built spike-first (#654); `protocol.rs` — the pure
//! `BidiGenerateContent` JSON builders + parser — lands first because it is
//! I/O-free and unit-testable without a live socket or a Gemini key.
//!
//! Adapted from the single-user reference implementation in the sibling `takt`
//! repo (`src-tauri/src/gemini_live/`, same author). Petal differences baked in
//! here from the start:
//! - **Manual activity (push-to-talk) mode**, not always-open-mic server VAD:
//!   `setup.realtimeInputConfig.automaticActivityDetection.disabled = true`, and
//!   each PTT hold is bracketed by `activityStart` / `activityEnd`. This is what
//!   keeps the model from hearing humans talk amongst themselves.
//! - **No tools in phase 1** — window-control tools arrive in #658 behind a
//!   fail-closed gate; the phase-1 setup declares none.
//! - **Model id is passed in**, never a baked constant: hosted mode uses the
//!   `model` field returned by `/api/ai-token` so preview-model rotation is a
//!   backend change, not a client release (#655).
//!
//! Compile-time platform split: the pure/serde half (`protocol`, `wire`,
//! `state`, `settings`, `room_auth`, `audio`, `room`) compiles on every
//! platform — the #654 spike probe examples drive it. The session engine and
//! most of its surfaces (`commands`, `session`, `topic`, `voice`,
//! `remote_audio`, `takeover`, `ax_digest`, `control_*`) are now also
//! compiled on every platform: their macOS-only primitives live behind
//! in-module `#[cfg]` splits with honest non-macOS fallbacks (the digest walk
//! is a no-op off macOS, the takeover detector is permanently unhealthy, the
//! control tier stays refused). Only the floating panel (`panel`) is still
//! macOS-gated — Windows gets its own WebviewWindow panel. Gate new native
//! surfaces here, not by stubbing behavior on Windows.

pub mod audio;
pub mod ax_digest;
pub mod commands;
pub(crate) mod control_exec;
pub(crate) mod control_gate;
pub mod control_policy;
pub(crate) mod control_target;
#[cfg(target_os = "macos")]
pub mod panel;
#[cfg(target_os = "windows")]
#[path = "panel_windows.rs"]
pub mod panel;
pub mod protocol;
pub mod remote_audio;
pub mod room;
pub mod room_auth;
pub mod session;
pub mod settings;
pub mod state;
pub(crate) mod takeover;
pub mod topic;
pub mod voice;
pub mod wire;
