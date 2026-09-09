//! Host-side Rust surface of the plugin system (plugins/README.md).
//!
//! M3 adds the registry client (`registry`, `store`): fetch + verify the signed
//! index, install verified bundles, keep installed state on disk.
//!
//! M2 scope: the data bus -- one catch-all receiver for `plugin/*` topics that
//! emits a single global `plugin-data` event, a `plugin_publish_data` command,
//! and the `plugins` participant-metadata key (advertisement + shared state:
//! `plugin_set_state` outbound, `plugin-state-changed` inbound). Permission checks live in the trusted main webview's broker
//! (shared/plugin-host); this layer independently re-checks the two things it
//! can: topic shape (a plugin may only publish under its own id) and size /
//! rate limits, so a bug in the frontend cannot flood the room.

pub mod bus;
pub mod registry;
pub mod store;

// Commands are registered in lib.rs by their defining path (`plugins::bus::...`):
// `tauri::generate_handler!` needs the macro-generated `__cmd__*` items, which
// a `pub use` re-export would not carry.
pub use bus::start_receiver_for_room;

/// Lowercase hex SHA-256, shared by the registry verify chain and the store's re-hash on read.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
