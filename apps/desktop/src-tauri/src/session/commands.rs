use crate::remote_control_core::RemoteControlPolicy;
use super::share::set_share_resolution as set_share_resolution_impl;
use super::{join_room, leave_room, SessionState, ShareSessionError};

// =============================================================================
// Tauri commands (SPEC.md §4.6 join flow)
// =============================================================================

/// Join a room by name (SPEC.md §4.6). `identity`/`display_name` come from
/// the frontend's real onboarding identity store (`session.svelte.ts`) --
/// see that store's `session.name`/a stable per-install identity derived
/// from it, threaded through by the frontend's `join_room` call site
/// (`/meeting/[room]/+page.svelte`). Idempotent: rejoining the same room is
/// a clean no-op (see `join_room`'s own doc comment).
#[tauri::command]
pub async fn join_room_command(
    app: tauri::AppHandle,
    rooms: tauri::State<'_, crate::rooms::RoomsState>,
    state: tauri::State<'_, SessionState>,
    room_name: String,
    identity: String,
    display_name: String,
    remote_control_allowed: bool,
    remote_control_policy: Option<RemoteControlPolicy>,
    identity_palette_index: Option<u8>,
) -> Result<crate::rooms::RoomRecord, ShareSessionError> {
    // `remote_control_policy` is the additive, authoritative field; the
    // legacy boolean is kept so an older frontend bundle still joins
    // (`true` -> Ask, never Auto -- consent is the default).
    let policy = remote_control_policy
        .unwrap_or_else(|| RemoteControlPolicy::from_allowed(remote_control_allowed, RemoteControlPolicy::Ask));
    join_room(
        &app,
        &rooms,
        &state,
        room_name,
        identity,
        display_name,
        policy,
        identity_palette_index,
    )
    .await
}

/// Current meeting's remote-control allow state. This is not the persisted
/// global default; it is the live in-memory session gate used by
/// `remote_control.rs`.
#[tauri::command]
pub fn remote_control_allowed(state: tauri::State<'_, SessionState>) -> bool {
    state.remote_control_allowed()
}

/// Set the current meeting's remote-control allow state. Turning it off
/// immediately revokes any active controller sessions; turning it on simply
/// allows future Request packets to auto-authorize.
#[tauri::command]
pub fn set_remote_control_allowed(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    allowed: bool,
) -> bool {
    state.set_remote_control_allowed(allowed);
    if !allowed {
        crate::remote_control::revoke_all(&app);
    }
    allowed
}

/// Current meeting's remote-control policy (`off` / `ask` / `auto`) -- the
/// live in-memory gate `remote_control.rs` consults on every Request.
#[tauri::command]
pub fn remote_control_policy(state: tauri::State<'_, SessionState>) -> RemoteControlPolicy {
    state.remote_control_policy()
}

/// Set the meeting's remote-control policy AND the default it restores to
/// (Settings changes mid-meeting). `off` revokes every active controller and
/// denies every parked consent request immediately.
#[tauri::command]
pub fn set_remote_control_policy(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    policy: RemoteControlPolicy,
) -> RemoteControlPolicy {
    state.set_remote_control_policy(policy);
    if !policy.allows_requests() {
        crate::remote_control::revoke_all(&app);
    }
    policy
}

/// Whether remote peers may control ONE shared window. Per-share, and
/// independent of the meeting-wide policy -- both must allow.
#[tauri::command]
pub fn share_remote_control_allowed(
    state: tauri::State<'_, SessionState>,
    window_id: u32,
) -> bool {
    state.share_allows_remote_control(window_id)
}

/// Set the per-share remote-control lock.
///
/// Three things have to happen together, and the order matters:
///  1. flip the host-side authorization (this is what actually enforces it);
///  2. revoke any controller currently driving THIS window, so turning the
///     lock off stops input already in flight rather than only refusing the
///     next request;
///  3. publish the metadata hint so peers hide the affordance.
///
/// Step 3 is last and is best-effort: if it fails, peers keep showing a
/// button whose requests the host now refuses -- degraded, but never unsafe.
#[tauri::command]
pub async fn set_share_remote_control_allowed(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
    window_id: u32,
    allowed: bool,
) -> Result<bool, String> {
    let Some(previous) = state.set_share_allows_remote_control(window_id, allowed) else {
        // No such live share: nothing to lock, and nothing to advertise.
        return Ok(false);
    };
    if previous == allowed {
        return Ok(allowed);
    }
    if !allowed {
        crate::remote_control::revoke_window(&app, window_id, "share-remote-control-locked");
    }
    if let Some(connection) = state.room_connection() {
        connection
            .set_shared_remote_control_allowed(window_id, allowed)
            .await;
    }
    log::info!(
        "session: share {window_id} remote control {} by the sharer",
        if allowed { "ALLOWED" } else { "LOCKED" }
    );
    Ok(allowed)
}

/// Set the capture-resolution cap for an active share. This republishes the track if dimensions
/// change and leaves focus-driven Full/Reduced quality policy intact.
#[tauri::command]
pub async fn set_share_resolution(
    state: tauri::State<'_, SessionState>,
    window_id: u32,
    res: crate::transport::publisher::CaptureResolution,
) -> Result<(), ShareSessionError> {
    set_share_resolution_impl(&state, window_id, res).await
}

/// Leave the currently-joined room, if any (SPEC.md §4.6). Idempotent.
#[tauri::command]
pub async fn leave_room_command(
    app: tauri::AppHandle,
    state: tauri::State<'_, SessionState>,
) -> Result<(), ()> {
    leave_room(&app, &state).await;
    Ok(())
}

/// The real durable room name this process is currently joined to, if any --
/// lets the frontend confirm/display which room is active (e.g. on a
/// `/meeting/[room]` reload) without re-deriving it from navigation state
/// alone.
#[tauri::command]
pub fn current_room(state: tauri::State<'_, SessionState>) -> Option<String> {
    state.current_room_name()
}

/// Live presence snapshot for the currently-joined room (SPEC.md §4.6) --
/// who's actually in it right now, per this process's own LiveKit
/// connection. Empty if not currently joined to any room. See `presence.rs`
/// for the `presence-update` event that pushes changes live, rather than
/// requiring the frontend to poll this command.
#[tauri::command]
pub fn room_presence(
    state: tauri::State<'_, SessionState>,
) -> Vec<crate::presence::PresentParticipant> {
    state.presence_snapshot()
}
