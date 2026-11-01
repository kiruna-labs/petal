//! Live streaming ("Go live" with a share to an RTMP(S) destination).
//!
//! * [`plan`] and [`wire`]: the output rules and room-level wire shapes,
//!   pinned to contracts/petal-contracts.json.
//! * Encoding on this computer: [`capability`] (which encode path this
//!   computer has, probed once), [`clock`] (constant frame rate, follow
//!   the source), [`mixer`] (meeting audio), [`mic`] (the sharer's mic),
//!   [`flv`] (packaging), [`queue`]
//!   (backpressure, reconnect schedule), [`rtmp`] (the socket), [`slate`],
//!   and the encode thread in `pipeline`, over the platform encoders in
//!   `windows_encode` (Media Foundation) or `macos_encode` (VideoToolbox and
//!   AudioToolbox).
//! * Privacy: [`streamer_mode`] (masking and capture exclusion while live)
//!   and [`settings`] ("Always use streamer mode", "Keep me off live
//!   streams").

pub mod audio_tap;
pub mod capability;
pub mod clock;
pub mod commands;
pub mod destinations;
pub mod flv;
pub mod keystore;
pub mod live_state;
pub mod mic;
pub mod mixer;
pub mod plan;
pub mod queue;
pub mod region_lock;
pub mod rtmp;
pub mod session;
pub mod settings;
pub mod slate;
pub mod streamer_mode;
pub mod tap;
pub mod wire;
#[cfg(target_os = "macos")]
pub mod macos_encode;
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub mod pipeline;
#[cfg(target_os = "windows")]
pub mod gpu_tap;
#[cfg(target_os = "windows")]
pub mod windows_encode;
