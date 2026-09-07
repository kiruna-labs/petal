//! Host-side Rust surface of the plugin system (plugins/README.md).
//!
//! M2 scope: the data bus -- one catch-all receiver for `plugin/*` topics that
//! emits a single global `plugin-data` event, a `plugin_publish_data` command,
//! and the `plugins` participant-metadata key (advertisement + shared state:
//! `plugin_set_state` outbound, `plugin-state-changed` inbound). Permission checks live in the trusted main webview's broker
//! (shared/plugin-host); this layer independently re-checks the two things it
//! can: topic shape (a plugin may only publish under its own id) and size /
//! rate limits, so a bug in the frontend cannot flood the room.

pub mod bus;

pub use bus::{plugin_publish_data, plugin_set_state, start_receiver_for_room};
