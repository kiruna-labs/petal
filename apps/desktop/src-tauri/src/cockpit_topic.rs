//! Read-only receiver for the `petal.cockpit` LiveKit data topic (#254).
//!
//! Test-cockpit web peers (headless Chrome driven by
//! `apps/desktop/scripts/cockpit.mjs`, via `web-harness`'s `?auto=<scenario>`
//! mode) self-report step/liveness results over this topic so a native
//! observer doesn't need CDP to see what the web side thinks happened. This
//! walking-skeleton phase only needs the native side to receive and
//! log/journal the report for visibility -- turning it into part of an
//! automated verdict is Phase 3+ (see the test-cockpit plan, #257).

#![cfg(target_os = "macos")]

use std::sync::Arc;

use livekit::prelude::*;

use crate::diagnostics::DiagnosticsState;
use crate::session::RoomGeneration;

pub const TOPIC: &str = "petal.cockpit";

pub fn start_receiver_for_room(
    app: &tauri::AppHandle,
    room: Arc<Room>,
    generation: RoomGeneration,
    diagnostics: DiagnosticsState,
) {
    let app = app.clone();
    let mut events = room.subscribe();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("cockpit-topic: receiver exiting for stale room generation");
                break;
            }
            let RoomEvent::DataReceived {
                payload,
                topic,
                participant,
                ..
            } = event
            else {
                continue;
            };
            if topic.as_deref() != Some(TOPIC) {
                continue;
            }
            let sender = participant
                .as_ref()
                .map(|p| p.identity().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let text = String::from_utf8_lossy(&payload).to_string();
            log::info!("cockpit-topic: report from '{sender}': {text}");
            diagnostics.journal_append(
                &app,
                "shares",
                format!("test-cockpit report from '{sender}': {text}"),
            );
        }
    });
}
