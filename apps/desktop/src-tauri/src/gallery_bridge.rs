//! issue #26 -- in-webview gallery video bridge, Rust side.
//!
//! Remote participants' camera feeds must render inside the main webview's
//! gallery tiles (`ParticipantTile`), but all remote media is decoded by the
//! NATIVE LiveKit connection (`transport/subscriber.rs` -> the compositor's
//! zero-copy CVPixelBuffer path), which has no way to hand frames to a
//! WKWebView `<video>` element.
//!
//! Mechanism chosen (issue #26's sketch option (a), which it calls "almost
//! certainly the pragmatic answer" -- decided explicitly here):
//! the webview joins the same LiveKit room a SECOND time with a lightweight
//! **hidden, subscribe-only** participant via `livekit-client` JS
//! (`src/lib/data/galleryBridge.ts`), and attaches remote camera tracks to
//! tile `<video>`s the same way the web client already does. Option (b)
//! (native decode -> frame push into the webview via eval/base64 or a
//! localhost MJPEG/WebSocket stream) was rejected: a base64 `eval` push is a
//! full CPU copy + JPEG/base64 encode per frame per tile (the exact CPU-copy
//! path SPEC.md §4.4 exists to avoid), and a localhost stream is a second
//! bespoke media transport next to the real one.
//!
//! The native connection now ignores `petal-camera-*` in its compositor feed;
//! those tracks belong only in gallery tiles. Window-share tracks stay
//! native-compositor-only, THE high-fidelity path per SPEC.md.
//!
//! This module's job is only the credential plumbing: mint the bridge's
//! token. The grants are least-privilege and, critically, **hidden** --
//! a hidden participant is never announced to other participants (LiveKit
//! server semantics), so it can't pollute anyone's roster/presence
//! (`presence.rs` counts `ParticipantConnected` events, which never fire
//! for hidden participants) and publishes nothing, so it can't trigger
//! compositor windows on other machines.

use serde::Serialize;

/// Appended to the caller's real room identity so logs/server inspection can
/// tell the hidden bridge participant apart from the real native one.
pub const GALLERY_IDENTITY_SUFFIX: &str = "-gallery";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GalleryBridgeConfig {
    /// LiveKit server URL (same one the native connection uses).
    pub url: String,
    /// Hidden, subscribe-only room-join JWT for the bridge participant.
    pub token: String,
    /// The derived LiveKit room name (`petal-room-<slug>`), informational.
    pub livekit_room: String,
    /// The bridge participant's identity (`<identity>-gallery`).
    pub identity: String,
}

/// Pure core of the command (testable without a Tauri handle).
pub async fn config_for(
    room_name: &str,
    base_identity: &str,
) -> Result<GalleryBridgeConfig, String> {
    let identity = format!("{base_identity}{GALLERY_IDENTITY_SUFFIX}");
    // #109: the public /api/token endpoint clamps hidden/grant fields (#100)
    // and rejects non-generated identities, so a `-gallery`-suffixed identity
    // asking for hidden+subscribe-only there is unconditionally rejected in
    // production. `fetch_gallery_access_token` hits the trusted, purpose-built
    // endpoint instead: it sends the CALLER'S OWN base identity (unsuffixed),
    // and the backend derives the bridge identity + verifies base_identity is
    // a real current participant before minting anything.
    let token_response = crate::transport::token::fetch_gallery_access_token(
        room_name,
        base_identity,
        Some(&identity),
    )
    .await
    .map_err(|e| e.to_string())?;
    log::info!(
        "gallery-bridge: fetched hidden subscribe-only token for '{}' in '{}'",
        crate::logging::log_safe_quoted(&identity),
        crate::logging::log_safe_quoted(&token_response.room)
    );
    Ok(GalleryBridgeConfig {
        url: token_response.url,
        token: token_response.token,
        livekit_room: token_response.room,
        identity,
    })
}

/// Tauri command: the meeting route calls this right after `join_room`
/// succeeds and hands the result to `galleryBridge.ts`'s livekit-client
/// connect. `room_name` is the human room code (the same one `join_room`
/// takes) -- the LiveKit room name is derived here via the same
/// `rooms::livekit_room_name_for` the native join uses, so the bridge lands
/// in the identical room by construction.
#[tauri::command]
pub async fn gallery_bridge_config(
    room_name: String,
    identity: String,
) -> Result<GalleryBridgeConfig, String> {
    config_for(&room_name, &identity).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gallery_identity_uses_unambiguous_suffix() {
        assert_eq!(
            format!("participant-abc{GALLERY_IDENTITY_SUFFIX}"),
            "participant-abc-gallery"
        );
    }
}
