//! Tauri command surface for AI chat (#656) — the glue between UI and engine.
//!
//! Every entry point enforces the same two preconditions before anything can
//! reach Google:
//! 1. the master switch is on (the sharer's consent), and
//! 2. the target window is a live publication owned by this client.
//!
//! Both are checked here AND again inside the engine (continuously, for the
//! second). That duplication is deliberate: this layer can be bypassed by a
//! future caller, the engine's gate cannot.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::session::{self, Credential, StartParams};
use super::settings::{self, Redacted};
use super::state::EndReason;

/// Response from the backend's `/api/ai-token` (#655). `model` is authoritative:
/// the client must use whatever the backend says so a preview-model rotation
/// never needs a client release.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AiTokenResponse {
    token: String,
    #[serde(default)]
    model: Option<String>,
}

/// Model used when nothing else specifies one (bring-your-own-key mode).
const FALLBACK_MODEL: &str = super::protocol::DEFAULT_MODEL_ID;

/// Read the current settings, redacted for the frontend (never the key).
#[tauri::command]
pub fn ai_chat_settings() -> Redacted {
    Redacted::from(&settings::current())
}

/// Toggle the master switch. Turning it OFF must also stop anything running —
/// a setting that leaves a live session streaming would be a consent bug, not
/// merely a UI inconsistency.
#[tauri::command]
pub fn ai_chat_set_enabled(app: AppHandle, enabled: bool) -> Result<Redacted, String> {
    let redacted = settings::update(|s| s.enabled = enabled)?;
    if !enabled {
        session::stop(&app, EndReason::Disabled);
    }
    Ok(redacted)
}

/// Set or clear the bring-your-own Gemini key. The value is never echoed back;
/// the response only reports whether one is now configured.
#[tauri::command]
pub fn ai_chat_set_api_key(key: Option<String>) -> Result<Redacted, String> {
    let cleaned = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    settings::update(|s| s.api_key = cleaned)
}

/// Whether a session is currently running for this window.
#[tauri::command]
pub fn ai_chat_is_active(window_id: u32) -> bool {
    session::is_active_for(window_id)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartOutcome {
    pub started: bool,
    /// Present when `started` is false: why, as a taxonomy token the UI maps to
    /// copy. Never a freeform string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<EndReason>,
}

impl StartOutcome {
    fn refused(reason: EndReason) -> Self {
        StartOutcome {
            started: false,
            reason: Some(reason),
        }
    }
}

/// Start an AI chat session for one of this client's shared windows.
///
/// Returns a structured outcome rather than an error string so every refusal
/// renders as a distinct, legible UI state instead of a silent dead button.
#[tauri::command]
pub async fn ai_chat_start(app: AppHandle, window_id: u32) -> Result<StartOutcome, String> {
    // `None` = "this client asked"; `start_inner` resolves our own identity.
    Ok(start_inner(app, window_id, None).await)
}

/// A start arriving over `petal.ai-chat` from another participant (#657).
///
/// Deliberately routes through the SAME guarded path as a local click: the
/// settings check, the one-session limit and the publication gate must not be
/// bypassable by arriving over the wire instead of from a button.
///
/// `requester` is the AUTHENTICATED sender, carried through so every surface's
/// "Started by …" names the person who actually asked rather than the host who
/// happens to run the socket.
pub(crate) async fn start_for_request(app: AppHandle, window_id: u32, requester: String) {
    let outcome = start_inner(app.clone(), window_id, Some(requester)).await;
    if let Some(reason) = outcome.reason {
        // The requester is remote, so the refusal has to travel back over the
        // topic; a local toast would only tell the wrong person.
        super::topic::publish_state(&app, window_id, false, None, Some(reason));
    }
}

async fn start_inner(
    app: AppHandle,
    window_id: u32,
    requested_by: Option<String>,
) -> StartOutcome {
    let config = settings::current();
    if !config.enabled {
        return StartOutcome::refused(EndReason::Disabled);
    }
    let Some(state) = app.try_state::<crate::session::SessionState>() else {
        return StartOutcome::refused(EndReason::Error);
    };
    // Precondition check. The engine re-checks continuously; this one gives the
    // caller an immediate, specific refusal.
    if !state.is_share_active(window_id) {
        return StartOutcome::refused(EndReason::NotShared);
    }
    if session::is_any_active() {
        return StartOutcome::refused(EndReason::Busy);
    }

    // Resolve a credential: hosted (backend-minted ephemeral token) preferred,
    // the user's own key otherwise. Third-party OSS builds have no baked
    // backend and therefore only ever take the second path.
    let (credential, model) = match acquire_credential(&state, &config).await {
        Ok(pair) => pair,
        Err(reason) => return StartOutcome::refused(reason),
    };

    // Who to attribute the session to. A remote `startRequest` names its
    // authenticated sender; a local click is us. `None` only when we are not in
    // a room, in which case there is nobody to tell and the field is omitted.
    let started_by = requested_by.or_else(|| {
        state
            .control_channel_snapshot()
            .map(|(_, identity)| identity)
    });

    // The continuous publication gate. `SessionState` is not cloneable and a
    // Tauri `State` borrow cannot outlive the command, so the predicate holds
    // an `AppHandle` and resolves the managed state on each call — which also
    // means it reports "not shared" if the state ever goes away.
    let gate_app = app.clone();
    let is_shared: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        gate_app
            .try_state::<crate::session::SessionState>()
            .is_some_and(|s| s.is_share_active(window_id))
    });

    match session::start(
        app,
        StartParams {
            window_id,
            model,
            credential,
            is_shared,
            started_by,
        },
    )
    .await
    {
        Ok(()) => StartOutcome {
            started: true,
            reason: None,
        },
        Err(reason) => StartOutcome::refused(reason),
    }
}

/// Obtain a credential + the model to use with it.
async fn acquire_credential(
    state: &tauri::State<'_, crate::session::SessionState>,
    config: &settings::AiChatSettings,
) -> Result<(Credential, String), EndReason> {
    // Hosted path: only possible when a backend is configured AND we are in a
    // room (the endpoint mints only for live participants). The backend expects
    // the durable room credential — the same value `fetch_gallery_access_token`
    // sends — not the derived LiveKit room name.
    let backend = crate::transport::token::backend_base_url().ok();
    let room = state.current_room_record().map(|record| record.name);
    let identity = state
        .control_channel_snapshot()
        .map(|(_, identity)| identity);

    if let (Some(base), Some(room), Some(identity)) = (backend, room, identity) {
        match fetch_ai_token(&base, &room, &identity).await {
            Ok(resp) => {
                let model = resp.model.unwrap_or_else(|| FALLBACK_MODEL.to_string());
                return Ok((Credential::EphemeralToken(resp.token), model));
            }
            Err(reason) => {
                // Fall back to a user key if they have one; otherwise surface
                // the hosted failure as-is so the UI can distinguish
                // "rate limited" from "switched off" from "offline".
                if !config.has_own_key() {
                    return Err(reason);
                }
                log::info!("ai_chat: hosted token unavailable ({reason:?}) -- using user key");
            }
        }
    }

    match config.api_key.as_ref() {
        Some(key) if !key.trim().is_empty() => Ok((
            Credential::ApiKey(key.trim().to_string()),
            FALLBACK_MODEL.to_string(),
        )),
        _ => Err(EndReason::HostedUnavailable),
    }
}

/// How long ONE `/api/ai-token` attempt may take. Mirrors the backend's
/// `AI_TOKEN_CLIENT_ATTEMPT_TIMEOUT_MS` (backend/lib/handlers.ts) and must stay
/// at or above it — plus `backend/vercel.json`'s 10s function ceiling, so even
/// a platform kill is observed as a response rather than raced.
///
/// The mint is NOT idempotent: every completed request is a Gemini token
/// Google bills for and an hourly slot spent. `transport::backend_http`'s
/// retrying send helper is therefore deliberately NOT used on this route — it
/// caps each attempt at 5s and retries three times on timeout AND on 5xx, so a
/// request the backend answered at 6s turned one click into four paid tokens,
/// four of the user's six hourly slots, ~21s of dead UI, and then "Could not
/// reach the AI chat service" for four requests that had all succeeded
/// server-side. One attempt, waited out, is the only safe shape here; the user
/// re-clicking is the retry. The test below greps for a relapse, so keep that
/// helper's name out of this function.
const AI_TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

/// Ask the backend for a short-lived Gemini token. Mirrors
/// `transport::token::fetch_gallery_access_token`'s trust shape: the caller
/// presents room + identity and the backend verifies that identity is a live
/// participant before minting anything.
async fn fetch_ai_token(
    base: &str,
    room: &str,
    identity: &str,
) -> Result<AiTokenResponse, EndReason> {
    // The endpoint verifies our LiveKit JWT's signature, room and identity
    // before minting anything, so an unauthenticated caller — or one asking in
    // someone else's name — gets nothing. Without a live join there is no token
    // to present, which is itself the correct answer: hosted AI chat is only
    // for participants of a room.
    let Some(bearer) = super::room_auth::current() else {
        log::info!("ai_chat: no room credential to authenticate the token request");
        return Err(EndReason::MintFailed);
    };
    let url = format!("{base}/api/ai-token");
    let payload = serde_json::json!({ "room": room, "identity": identity });
    // Single attempt, no retry — see AI_TOKEN_REQUEST_TIMEOUT above.
    // Do not reintroduce the retrying helper here (#655 cost review).
    let response = crate::transport::backend_http::client()
        .post(&url)
        .timeout(AI_TOKEN_REQUEST_TIMEOUT)
        .bearer_auth(&bearer)
        .json(&payload)
        .send()
        .await
        .map_err(|_| EndReason::Offline)?;

    let status = response.status();
    if !status.is_success() {
        let reason = super::state::classify_mint_status(status.as_u16());
        log::warn!("ai_chat: token mint failed with HTTP {} ({reason:?})", status);
        return Err(reason);
    }
    response
        .json::<AiTokenResponse>()
        .await
        .map_err(|_| EndReason::MintFailed)
}

/// Stop the running session.
#[tauri::command]
pub fn ai_chat_stop(app: AppHandle) {
    session::stop(&app, EndReason::Stopped);
}

/// Begin a push-to-talk turn for the local user. Returns false when the floor
/// is already held or no session is live — manual-activity mode is a single
/// serial stream, so two speakers interleaved would corrupt the turn.
///
/// Goes through `topic::local_ptt_start` rather than straight to the engine so
/// the local user contends for the SAME floor a remote peer claims over the
/// wire, and so the room is told who is speaking. This is also the one entry
/// point permitted to open this machine's microphone (#657).
#[tauri::command]
pub fn ai_chat_ptt_start(app: AppHandle) -> bool {
    super::topic::local_ptt_start(&app)
}

/// End the local user's push-to-talk turn and release the floor.
#[tauri::command]
pub fn ai_chat_ptt_end(app: AppHandle) {
    super::topic::local_ptt_end(&app);
}

/// Send a typed message into the local user's own AI chat session.
///
/// Unlike PTT, this never claims a floor and cannot be "busy" — any number of
/// participants may each send text independently. Returns false when there is
/// no live session, the text is blank/too long, or the PTT floor is currently
/// held (a `clientContent` turn and an open manual-activity window are
/// undefined together, so text waits for the floor to be free).
#[tauri::command]
pub fn ai_chat_send_text(app: AppHandle, text: String) -> bool {
    super::topic::local_send_text(&app, &text)
}

// ---- remote windows (#657 receiver half) ------------------------------------
//
// A receiver never hosts: the session for a window someone else is sharing
// always runs on THEIR machine. These commands ask over `petal.ai-chat` rather
// than touching `session::` directly, and the window's owner is the one who
// decides whether to act (its own settings/publication checks, unchanged).

/// Ask the window's owner to start a session on a window they are sharing.
///
/// Fire-and-forget by design: the owner's decision — and any refusal — comes
/// back as a `state` message over the same topic (see
/// [`ai_chat_remote_session`]), not as this call's return value. A remote
/// participant is never in a position to know the outcome synchronously.
#[tauri::command]
pub fn ai_chat_request_start(app: AppHandle, window_id: u32, owner_identity: String) -> Result<(), String> {
    publish_request(&app, window_id, owner_identity, super::wire::Body::StartRequest)
}

/// Ask the window's owner to stop the running session.
#[tauri::command]
pub fn ai_chat_request_stop(app: AppHandle, window_id: u32, owner_identity: String) -> Result<(), String> {
    publish_request(&app, window_id, owner_identity, super::wire::Body::StopRequest)
}

/// Send a typed message into a session running on a window someone else is
/// sharing. Fire-and-forget, same as start/stop: the owner's machine is the
/// one that actually calls `session::send_text`, and there is no synchronous
/// outcome to report back — the reply (or the lack of one) shows up in the
/// room's normal audio/transcript, not as this call's return value.
///
/// Length/blank validation happens again on the owner's machine
/// (`session::send_text`) regardless of this client-side check — this is
/// just to avoid a network round trip for an obviously invalid message.
#[tauri::command]
pub fn ai_chat_request_send_text(
    app: AppHandle,
    window_id: u32,
    owner_identity: String,
    text: String,
) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("text is empty".to_string());
    }
    if text.chars().count() > super::wire::MAX_USER_TEXT_CHARS {
        return Err("text is too long".to_string());
    }
    publish_request(&app, window_id, owner_identity, super::wire::Body::SendText { text })
}

/// Claim the push-to-talk floor on a window someone else is sharing (#664:
/// this command never existed, despite #657 being reported landed --
/// `topic::decide()` has always correctly processed an INCOMING `pttStart`
/// on the owner's machine, but nothing ever published one).
///
/// Fire-and-forget like every other remote command: the owner's floor
/// decision (granted, or `state{error:"busy", activeSpeaker}` if someone
/// else already holds it) comes back over the topic, not this call's return
/// value.
#[tauri::command]
pub fn ai_chat_request_ptt_start(app: AppHandle, window_id: u32, owner_identity: String) -> Result<(), String> {
    publish_request(&app, window_id, owner_identity, super::wire::Body::PttStart)
}

/// Release a push-to-talk floor claimed on a window someone else is sharing.
#[tauri::command]
pub fn ai_chat_request_ptt_end(app: AppHandle, window_id: u32, owner_identity: String) -> Result<(), String> {
    publish_request(&app, window_id, owner_identity, super::wire::Body::PttEnd)
}

fn publish_request(
    app: &AppHandle,
    window_id: u32,
    owner_identity: String,
    body: super::wire::Body,
) -> Result<(), String> {
    let Some(state) = app.try_state::<crate::session::SessionState>() else {
        return Err("not in a room".to_string());
    };
    let Some((connection, _)) = state.control_channel_snapshot() else {
        return Err("not in a room".to_string());
    };
    super::topic::publish(
        &connection,
        super::wire::Message {
            v: super::wire::VERSION,
            key: super::wire::WindowKey {
                window_id,
                owner_identity,
            },
            body,
        },
    );
    Ok(())
}

/// What this client currently believes about a window someone else is
/// sharing, or `None` if it has never observed a `state` message for it.
///
/// A receiver's chrome window can mount AFTER a session is already live, so
/// this is the "ask" half of ask-then-listen; `EVENTS.aiChatRemoteState` (see
/// `topic::start_receiver_for_room`) is the "listen" half for everything
/// after. Returns `topic::RemoteState` directly — its own doc comment already
/// promises it is the one shape for both halves, so reshaping it down here
/// (as an earlier version of this command did, dropping `windowId`,
/// `ownerIdentity` and `error`) would make that promise false. A surface that
/// mounts right after the owner refuses a start now sees the refusal
/// immediately from this command, not only from the next event.
#[tauri::command]
pub fn ai_chat_remote_session(window_id: u32, owner_identity: String) -> Option<super::topic::RemoteState> {
    super::topic::remote_session(&super::wire::WindowKey {
        window_id,
        owner_identity,
    })
}

// ---- window control (#658) --------------------------------------------------
//
// Every answer names BOTH the session epoch and the request id it is answering.
// Without the pair, a click on a card the model had already replaced would
// authorize whatever replaced it — the human would have said yes to one action
// and got another. `false` means the answer was stale and nothing happened.

/// A participant allowed the pending action.
///
/// `session_scope` is the explicit escalation: the default (`false`) authorizes
/// exactly this one action, and per-action approval is what the UI offers
/// first. A session-wide grant is a much larger thing to hand out and has to be
/// chosen deliberately.
#[tauri::command]
pub fn ai_chat_control_approve(
    app: AppHandle,
    session_id: u64,
    request_id: String,
    session_scope: bool,
) -> bool {
    let scope = if session_scope {
        super::control_policy::GrantScope::Session
    } else {
        super::control_policy::GrantScope::Once
    };
    session::approve_control(&app, session_id, &request_id, scope)
}

/// A participant refused. Sticky for the rest of the session — a model that
/// keeps asking must not be able to wear anyone down by repetition.
#[tauri::command]
pub fn ai_chat_control_reject(app: AppHandle, session_id: u64) -> bool {
    session::reject_control(&app, session_id)
}

/// The deliberate way back from a sticky refusal, so "no" is reversible by a
/// human decision but never by the model asking again.
#[tauri::command]
pub fn ai_chat_control_resume(session_id: u64) -> bool {
    session::resume_control(session_id)
}

/// Current Rust-owned standing grant/refusal for the live session. `None`
/// means there is no live control epoch to display.
#[tauri::command]
pub fn ai_chat_control_status() -> Option<session::ControlStatus> {
    session::control_status()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_carry_a_taxonomy_token_not_a_string() {
        let outcome = StartOutcome::refused(EndReason::NotShared);
        assert!(!outcome.started);
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"reason\":\"not-shared\""), "{json}");
    }

    #[test]
    fn a_successful_start_carries_no_reason() {
        let json = serde_json::to_string(&StartOutcome {
            started: true,
            reason: None,
        })
        .unwrap();
        assert!(json.contains("\"started\":true"), "{json}");
        assert!(!json.contains("reason"), "{json}");
    }

    #[test]
    fn the_ai_token_mint_is_waited_out_and_never_retried() {
        // A mint is not idempotent, so the only safe retry count is zero and the
        // single attempt has to outlast the backend's worst case: its own 6s
        // shared upstream budget, and vercel.json's 10s function ceiling above
        // that. Shorten this, or route it back through the retrying helper, and
        // one click buys four billable tokens again.
        assert!(
            AI_TOKEN_REQUEST_TIMEOUT >= Duration::from_secs(11),
            "one attempt must outlast the backend's 10s function ceiling, got {AI_TOKEN_REQUEST_TIMEOUT:?}"
        );
        // Built by concatenation so this assertion is not itself the match it
        // is looking for -- the same trick backend_http.rs's diagnostics test
        // uses, and the reason a naive `contains` here can never pass.
        let retrying_helper = ["send", "_with_retry"].concat();
        let source = include_str!("commands.rs");
        let body = source
            .split_once("async fn fetch_ai_token")
            .map(|(_, rest)| rest.split_once("\n}\n").map(|(body, _)| body).unwrap_or(rest))
            .expect("fetch_ai_token must exist");
        assert!(
            !body.contains(&retrying_helper),
            "the ai-token mint must never go through the retrying backend helper"
        );
    }

    #[test]
    fn token_response_tolerates_a_missing_model_field() {
        // The backend should always send `model`, but a client that hard-errors
        // on its absence would be broken by a backend rollback.
        let resp: AiTokenResponse =
            serde_json::from_str(r#"{"token":"authTokens/x"}"#).unwrap();
        assert_eq!(resp.token, "authTokens/x");
        assert!(resp.model.is_none());
    }

    #[test]
    fn token_response_prefers_the_backend_model() {
        let resp: AiTokenResponse = serde_json::from_str(
            r#"{"token":"authTokens/x","model":"models/gemini-future","expireTime":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(resp.model.as_deref(), Some("models/gemini-future"));
    }
}
