//! LiveKit plumbing for `petal.ai-chat` (#657): publish the host's view of a
//! session, and act on what peers send.
//!
//! `wire.rs` owns the contract and `room.rs` owns the coordination rules; this
//! module is only the transport and the dispatch, so the parts worth testing
//! stay testable without a socket.
//!
//! Every inbound message passes [`wire::authorize`] BEFORE it can affect
//! anything. Sender identity comes from the authenticated LiveKit participant,
//! never the payload.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use livekit::Room;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::room::{Claim, Floor, RemoteSessions, RequestLimiter, SessionReport};
use super::session::PttSource;
use super::state::EndReason;
use super::wire::{
    self, Body, Message, TranscriptRole, WindowKey, MAX_PTT_STARTS_PER_SENDER_PER_MINUTE,
    MAX_TEXT_SENDS_PER_SENDER_PER_MINUTE, TOPIC, VERSION,
};
use crate::session::{RoomGeneration, SessionState};
use crate::transport::publisher::RoomConnection;

/// How often the safety timers are evaluated. Well under the shortest deadline
/// they enforce (`room::SILENCE_TIMEOUT`, 5s) so an expiry fires promptly rather
/// than up to a whole period late.
const SAFETY_TICK: Duration = Duration::from_millis(500);

/// A remote host's session, pushed to every local surface that renders one.
pub const EVENT_REMOTE_STATE: &str = "ai-chat-remote-state";
/// A transcript delta from a session hosted elsewhere (#664).
pub const EVENT_REMOTE_TRANSCRIPT: &str = "ai-chat-remote-transcript";

/// Per-room coordination state, owned by this module.
struct Coordination {
    floor: Floor,
    remote: RemoteSessions,
    /// start/stop requests.
    limiter: RequestLimiter,
    /// Push-to-talk claims, on their own much larger budget. One shared bucket
    /// would either leave start/stop wide open or silence a real conversation
    /// — see [`MAX_PTT_STARTS_PER_SENDER_PER_MINUTE`].
    ptt_limiter: RequestLimiter,
    /// Typed turns, on their own budget too — see
    /// [`MAX_TEXT_SENDS_PER_SENDER_PER_MINUTE`].
    text_limiter: RequestLimiter,
}

impl Default for Coordination {
    fn default() -> Self {
        Self {
            floor: Floor::default(),
            remote: RemoteSessions::default(),
            limiter: RequestLimiter::default(),
            ptt_limiter: RequestLimiter::new(MAX_PTT_STARTS_PER_SENDER_PER_MINUTE),
            text_limiter: RequestLimiter::new(MAX_TEXT_SENDS_PER_SENDER_PER_MINUTE),
        }
    }
}

/// A session hosted by SOMEONE ELSE, as this client sees it.
///
/// The payload of [`EVENT_REMOTE_STATE`] and the return of [`remote_session`],
/// deliberately the same shape: a surface asks once on mount and listens
/// thereafter, and must not have to reconcile two vocabularies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteState {
    pub window_id: u32,
    pub owner_identity: String,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds_left: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_speaker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<EndReason>,
}

impl RemoteState {
    fn new(key: &WindowKey, report: &SessionReport) -> Self {
        Self {
            window_id: key.window_id,
            owner_identity: key.owner_identity.clone(),
            active: report.active,
            started_by: report.started_by.clone(),
            seconds_left: report.seconds_left,
            active_speaker: report.active_speaker.clone(),
            error: report.error,
        }
    }

    /// The "this session is gone" form. Every path that clears a remote session
    /// publishes it: a receiver that only ever hears "live" is exactly the
    /// phantom badge the heartbeat exists to prevent.
    fn cleared(key: &WindowKey) -> Self {
        Self::new(key, &SessionReport::default())
    }
}

/// The current view of a remote host's session, for a surface that mounted
/// AFTER it started and so never heard the event.
pub fn remote_session(key: &WindowKey) -> Option<RemoteState> {
    let coordination = coordination().lock().ok()?;
    let session = coordination.remote.get(key)?;
    Some(RemoteState::new(key, &session.report))
}

fn emit_remote_state(app: &AppHandle, state: &RemoteState) {
    let _ = app.emit(EVENT_REMOTE_STATE, state);
    push_remote_state_to_overlay(app, state);
}

fn emit_remote_transcript(app: &AppHandle, delta: &RemoteTranscriptDelta) {
    let _ = app.emit(EVENT_REMOTE_TRANSCRIPT, delta);
    push_remote_transcript_to_overlay(app, delta);
}

/// #844: `app.emit` above reaches the receiver panel's own surface webview
/// fine (that's how the header's session badge/PTT already worked before
/// this), but the AI-chat transcript/input overlay is a CHILD webview built
/// via `compositor::create_chrome_webview` -- like the control/pointer
/// overlays, Tauri's event bus does not reliably reach it on macOS (see
/// `routes/compositor/pointer/+page.svelte`'s doc comment). Push directly via
/// `webview.eval`, the same already-proven workaround `telepointer.rs` uses
/// for the pointer overlay.
#[cfg(target_os = "macos")]
fn push_remote_state_to_overlay(app: &AppHandle, state: &RemoteState) {
    let Some(label) =
        crate::compositor::ai_chat_overlay_label_for_window(state.window_id, &state.owner_identity)
    else {
        return;
    };
    let Some(win) = app.get_webview_window(&label) else {
        return;
    };
    let Ok(json) = serde_json::to_string(state) else {
        return;
    };
    if let Err(e) = win.eval(format!(
        "window.__petalAiChatRemoteState && window.__petalAiChatRemoteState({json})"
    )) {
        log::warn!("ai_chat: failed to push remote state to overlay '{label}': {e}");
    }
}

#[cfg(not(target_os = "macos"))]
fn push_remote_state_to_overlay(_app: &AppHandle, _state: &RemoteState) {}

#[cfg(target_os = "macos")]
fn push_remote_transcript_to_overlay(app: &AppHandle, delta: &RemoteTranscriptDelta) {
    let Some(label) = crate::compositor::ai_chat_overlay_label_for_window(
        delta.window_id,
        &delta.owner_identity,
    ) else {
        return;
    };
    let Some(win) = app.get_webview_window(&label) else {
        return;
    };
    let Ok(json) = serde_json::to_string(delta) else {
        return;
    };
    if let Err(e) = win.eval(format!(
        "window.__petalAiChatRemoteTranscript && window.__petalAiChatRemoteTranscript({json})"
    )) {
        log::warn!("ai_chat: failed to push remote transcript to overlay '{label}': {e}");
    }
}

#[cfg(not(target_os = "macos"))]
fn push_remote_transcript_to_overlay(_app: &AppHandle, _delta: &RemoteTranscriptDelta) {}

fn coordination() -> &'static Mutex<Coordination> {
    static STATE: OnceLock<Mutex<Coordination>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Coordination::default()))
}

/// Who currently holds the push-to-talk floor, for UI.
pub fn floor_holder() -> Option<String> {
    coordination()
        .lock()
        .ok()
        .and_then(|c| c.floor.holder().map(str::to_string))
}

/// Note that audio arrived from `speaker`, keeping their turn alive.
///
/// Called from `session::push_audio` for BOTH capture paths, so the silence
/// timeout means "no audio from the holder" rather than "no audio from a
/// microphone". Never call this while holding the session lock — that inverts
/// the order the disconnect path takes the two locks in.
pub fn note_floor_audio(speaker: &str) {
    if let Ok(mut c) = coordination().lock() {
        c.floor.note_audio(speaker, Instant::now());
    }
}

/// Release the floor if `who` holds it. Returns whether it changed.
fn release_floor(who: &str) -> bool {
    coordination()
        .lock()
        .map(|mut c| c.floor.release(who))
        .unwrap_or(false)
}

/// Publish a message on the topic. Reliable: session state and transcript
/// lines must not be dropped.
pub fn publish(room_connection: &Arc<RoomConnection>, message: Message) {
    let room = room_connection.room();
    tauri::async_runtime::spawn(async move {
        let Ok(payload) = serde_json::to_vec(&message) else {
            return;
        };
        let packet = livekit::DataPacket {
            payload,
            topic: Some(TOPIC.to_string()),
            reliable: true,
            destination_identities: Vec::new(),
        };
        if let Err(e) = room.local_participant().publish_data(packet).await {
            log::debug!("ai_chat: publish_data failed: {e}");
        }
    });
}

/// Announce this host's view of a window's session. Doubles as the liveness
/// heartbeat; receivers expire a session whose heartbeat stops.
pub fn publish_state(
    app: &AppHandle,
    window_id: u32,
    active: bool,
    seconds_left: Option<u64>,
    error: Option<EndReason>,
) {
    let Some((connection, identity)) = control_channel(app) else {
        return;
    };
    let active_speaker = floor_holder();
    super::session::emit_floor_state(app, window_id, active_speaker.as_deref());
    // Read from the running session rather than threaded through every call
    // site: the engine records who asked for it at start (a local click, or the
    // authenticated sender of a `startRequest`), so there is exactly one source
    // of truth and a teardown's `active: false` correctly reports nobody.
    let started_by = super::session::started_by();
    publish(
        &connection,
        Message {
            v: VERSION,
            key: WindowKey {
                window_id,
                owner_identity: identity,
            },
            body: Body::State {
                active,
                started_by,
                seconds_left,
                active_speaker,
                error,
            },
        },
    );
}

/// Broadcast a transcript delta so every participant sees the same conversation.
pub fn publish_transcript(app: &AppHandle, window_id: u32, role: TranscriptRole, text: &str, final_: bool) {
    let Some((connection, identity)) = control_channel(app) else {
        return;
    };
    publish(
        &connection,
        Message {
            v: VERSION,
            key: WindowKey {
                window_id,
                owner_identity: identity,
            },
            body: Body::Transcript {
                role,
                text: text.to_string(),
                final_,
            },
        },
    );
}

fn control_channel(app: &AppHandle) -> Option<(Arc<RoomConnection>, String)> {
    app.try_state::<SessionState>()
        .and_then(|state| state.control_channel_snapshot())
}

/// Subscribe to the topic for one room generation. Registered alongside the
/// other per-room receivers in `session::room`.
pub(crate) fn start_receiver_for_room(app: &AppHandle, room: Arc<Room>, generation: RoomGeneration) {
    let mut events = room.subscribe();
    // A fresh room means a fresh floor and no remembered remote sessions. Reset
    // BEFORE the safety timer starts, or its first tick could measure a
    // previous room's timestamps.
    if let Ok(mut c) = coordination().lock() {
        *c = Coordination::default();
    }
    start_safety_timer(app, generation.clone());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("ai_chat: receiver exiting for stale room generation");
                break;
            }
            match event {
                livekit::RoomEvent::DataReceived {
                    payload,
                    topic,
                    participant,
                    ..
                } => {
                    if topic.as_deref() != Some(TOPIC) {
                        continue;
                    }
                    let Ok(message) = serde_json::from_slice::<Message>(&payload) else {
                        log::warn!("ai_chat: ignored malformed payload");
                        continue;
                    };
                    // Identity ALWAYS from the authenticated sender.
                    let Some(sender) = participant.as_ref().map(|p| p.identity().to_string())
                    else {
                        log::warn!("ai_chat: ignored message without authenticated sender");
                        continue;
                    };
                    if let Err(rejection) = wire::authorize(&message, &sender) {
                        log::warn!("ai_chat: rejected message from '{sender}': {rejection:?}");
                        continue;
                    }
                    handle(&app, message, &sender).await;
                }
                // A participant leaving must not leave their floor held or
                // their sessions displayed.
                livekit::RoomEvent::ParticipantDisconnected(participant) => {
                    let gone = participant.identity().to_string();
                    // Decide under the coordination lock, act after releasing
                    // it: `session::ptt_end` takes the session lock, and
                    // `push_audio` takes them the other way round.
                    let (released, cleared) = match coordination().lock() {
                        Ok(mut c) => (
                            c.floor.release_on_disconnect(&gone),
                            c.remote.forget_owner(&gone),
                        ),
                        Err(_) => (false, Vec::new()),
                    };
                    if released {
                        log::info!("ai_chat: floor released -- '{gone}' disconnected");
                        super::session::ptt_end();
                        if let Some(window_id) = super::session::active_window_id() {
                            publish_state(&app, window_id, true, None, None);
                        }
                    }
                    for key in cleared {
                        log::info!(
                            "ai_chat: cleared session for window {} -- host left",
                            key.window_id
                        );
                        // A receiver that never hears "gone" leaves the badge
                        // up forever — the phantom the heartbeat exists to
                        // prevent, arriving by the other door.
                        emit_remote_state(&app, &RemoteState::cleared(&key));
                    }
                }
                _ => {}
            }
        }
    });
}

/// This host's own session, as [`decide`] needs to see it.
///
/// Passed in rather than read inside `decide` so a test can drive a message at
/// a host whose session is on a DIFFERENT window than the message names — which
/// is the whole of the window-id blocker (#661), and was untestable while the
/// dispatcher read the engine's globals directly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LocalSession {
    /// The window a LIVE session is running on.
    pub live_window: Option<u32>,
    /// The window the single session slot is claimed for — connecting OR live.
    /// A stop must be able to reach a session that has not finished connecting.
    pub slot_window: Option<u32>,
}

impl LocalSession {
    fn current() -> Self {
        Self {
            live_window: super::session::active_window_id(),
            slot_window: super::session::slot_window_id(),
        }
    }
}

/// What an inbound message resolves to, decided without touching a socket, an
/// audio device or an `AppHandle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dispatch {
    Ignore(Ignored),
    Start { window_id: u32, requester: String },
    Stop { window_id: u32 },
    /// The floor is now the sender's: open their audio path and tell the room.
    OpenTurn {
        window_id: u32,
        speaker: String,
        source: PttSource,
    },
    /// Somebody else holds the floor; the asker is told who.
    FloorBusy { window_id: u32, holder: String },
    /// The sender held the floor and gave it up.
    CloseTurn { window_id: u32, speaker: String },
    /// A typed turn to send into the session, attributed to the sender.
    /// Distinct from `OpenTurn`/`CloseTurn`: text never touches the floor.
    SendText {
        window_id: u32,
        speaker: String,
        text: String,
    },
    /// A remote host's session state was recorded; push it to local surfaces.
    RecordedRemoteState(RemoteState),
    /// A transcript delta from a session hosted elsewhere (#664): unlike
    /// `RecordedRemoteState`, nothing is accumulated in Rust -- this just
    /// relays the raw delta, the same way `session::emit_transcript` does
    /// for a LOCAL session's own transcript. The frontend's existing
    /// coalescing logic (`appendTranscriptDelta`) already knows how to fold
    /// a stream of these into bubbles; duplicating that in Rust would be a
    /// second implementation of the same rules to keep in sync.
    RemoteTranscript(RemoteTranscriptDelta),
}

/// One transcript delta from a session hosted elsewhere, keyed the same way
/// [`RemoteState`] is so a receiver watching multiple remote windows can
/// tell them apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTranscriptDelta {
    pub window_id: u32,
    pub owner_identity: String,
    pub role: TranscriptRole,
    pub text: String,
    #[serde(rename = "final")]
    pub final_: bool,
}

/// Why a message changed nothing.
///
/// Carried rather than collapsed to a bare "no" so a test can tell "we do not
/// own that window" from "we own it but have no session on it" — the second is
/// the case that used to act on the WRONG session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ignored {
    /// Should be unreachable on the real path — the receiver refuses first.
    Unauthorized(wire::Rejection),
    /// Someone else's window — or no room at all, so we own nothing. Its host
    /// acts, not us.
    NotTheOwner,
    /// The message named a window this host has no session on.
    ///
    /// `we_own_it` compared identities ONLY, so a peer's `pttStart {windowId:
    /// 5}` opened a turn on whatever session was running (window 9) and
    /// published `state{windowId: 5, active: true}` — every receiver then
    /// rendered "AI chat live" on a window with nothing on it. `stopRequest`
    /// named window 5 and killed the session on window 9 (#661).
    WrongWindow,
    RateLimited,
    /// A `pttEnd` from someone who does not hold the floor. Releasing on it
    /// would cut the actual speaker off mid-sentence.
    NotTheHolder,
    /// Our own `state` coming back to us.
    OwnEcho,
    /// Nothing for this layer to do (remote transcript is the receiver UI's).
    NoAction,
}

/// Decide what an inbound message means.
///
/// Owns every coordination-state mutation (floor, limiters, remote sessions) so
/// the effects a test needs to observe are real, and leaves only I/O to
/// [`handle`].
pub(crate) fn decide(
    message: &Message,
    sender: &str,
    local_identity: Option<&str>,
    local: &LocalSession,
    now: Instant,
) -> Dispatch {
    // Defence in depth. The receiver loop refuses an unauthorized message
    // before it ever reaches here (that is the real boundary, and it stays
    // there); repeating the check means a future caller of the dispatcher
    // cannot skip it by accident.
    if let Err(rejection) = wire::authorize(message, sender) {
        return Dispatch::Ignore(Ignored::Unauthorized(rejection));
    }
    let we_own_it = local_identity == Some(message.key.owner_identity.as_str());
    let window_id = message.key.window_id;

    match &message.body {
        // Only the window's host acts on a request.
        Body::StartRequest | Body::StopRequest => {
            if !we_own_it {
                return Dispatch::Ignore(Ignored::NotTheOwner);
            }
            if !allow_request(sender, now) {
                return Dispatch::Ignore(Ignored::RateLimited);
            }
            if matches!(message.body, Body::StopRequest) {
                // A stop must name the session it is stopping. The slot window
                // rather than the live one, so a stop DURING connect works.
                if local.slot_window != Some(window_id) {
                    return Dispatch::Ignore(Ignored::WrongWindow);
                }
                return Dispatch::Stop { window_id };
            }
            Dispatch::Start {
                window_id,
                requester: sender.to_string(),
            }
        }

        // Claim/release the floor. The host is the only one that acts.
        Body::PttStart => {
            // The ownership check IS the bind, so the audio-routing decision
            // below can never be made against a missing local identity — which
            // would classify the host's own push-to-talk as remote, and is the
            // shape of #657's original bug.
            let Some(local_identity) =
                local_identity.filter(|id| *id == message.key.owner_identity.as_str())
            else {
                return Dispatch::Ignore(Ignored::NotTheOwner);
            };
            // The floor belongs to ONE session, so a claim naming any other
            // window is not a claim we can honour. Checked BEFORE the floor is
            // touched: claiming and then unwinding would still have locked the
            // real speaker out for the width of the race.
            if local.live_window != Some(window_id) {
                return Dispatch::Ignore(Ignored::WrongWindow);
            }
            if !allow_ptt(sender, now) {
                return Dispatch::Ignore(Ignored::RateLimited);
            }
            match claim_floor(sender, now) {
                Claim::Granted => Dispatch::OpenTurn {
                    window_id,
                    speaker: sender.to_string(),
                    source: super::session::audio_route(sender, local_identity),
                },
                Claim::Busy { holder } => Dispatch::FloorBusy { window_id, holder },
            }
        }
        Body::PttEnd => {
            if !we_own_it {
                return Dispatch::Ignore(Ignored::NotTheOwner);
            }
            if local.live_window != Some(window_id) {
                return Dispatch::Ignore(Ignored::WrongWindow);
            }
            // Deliberately NOT rate-limited: a dropped `pttEnd` wedges the floor
            // until the silence timeout.
            if release_floor(sender) {
                Dispatch::CloseTurn {
                    window_id,
                    speaker: sender.to_string(),
                }
            } else {
                Dispatch::Ignore(Ignored::NotTheHolder)
            }
        }

        // A typed turn. Only the host acts, same as start/stop -- but unlike
        // PTT this never touches the floor: text has no "who's speaking"
        // ambiguity, so it is not arbitrated and does not require the floor
        // to be free.
        Body::SendText { text } => {
            if !we_own_it {
                return Dispatch::Ignore(Ignored::NotTheOwner);
            }
            if local.live_window != Some(window_id) {
                return Dispatch::Ignore(Ignored::WrongWindow);
            }
            if !allow_text(sender, now) {
                return Dispatch::Ignore(Ignored::RateLimited);
            }
            Dispatch::SendText {
                window_id,
                speaker: sender.to_string(),
                text: text.clone(),
            }
        }

        // Remote host telling us about its session.
        Body::State {
            active,
            started_by,
            seconds_left,
            active_speaker,
            error,
        } => {
            if we_own_it {
                return Dispatch::Ignore(Ignored::OwnEcho);
            }
            let report = SessionReport {
                active: *active,
                started_by: started_by.clone(),
                seconds_left: *seconds_left,
                active_speaker: active_speaker.clone(),
                error: *error,
            };
            if let Ok(mut c) = coordination().lock() {
                c.remote.observe(&message.key, report.clone(), now);
            }
            // `message.key` IS the key the state is stored under, and
            // `authorize` already proved the sender owns it — so the window id
            // surfaces carry is never one a peer chose for someone else's
            // window.
            Dispatch::RecordedRemoteState(RemoteState::new(&message.key, &report))
        }

        Body::Transcript { role, text, final_ } => {
            if we_own_it {
                // We already have our own transcript locally
                // (session::emit_transcript's direct EVENT_TRANSCRIPT emit);
                // this is that same message echoed back to us over the room.
                return Dispatch::Ignore(Ignored::OwnEcho);
            }
            // `authorize` already proved the sender owns this window, so the
            // role/text here are trustworthy the same way State's are.
            Dispatch::RemoteTranscript(RemoteTranscriptDelta {
                window_id,
                owner_identity: message.key.owner_identity.clone(),
                role: *role,
                text: text.clone(),
                final_: *final_,
            })
        }
    }
}

/// Act on an authorized message.
async fn handle(app: &AppHandle, message: Message, sender: &str) {
    let local_identity = control_channel(app).map(|(_, identity)| identity);
    match decide(
        &message,
        sender,
        local_identity.as_deref(),
        &LocalSession::current(),
        Instant::now(),
    ) {
        Dispatch::Ignore(reason) => {
            log::debug!("ai_chat: nothing to do for a message from '{sender}': {reason:?}");
        }
        Dispatch::Start {
            window_id,
            requester,
        } => {
            // Through the SAME command path a local click takes, so the settings
            // check and the publication gate cannot be bypassed by arriving over
            // the wire.
            log::info!("ai_chat: '{requester}' requested a session on window {window_id}");
            super::commands::start_for_request(app.clone(), window_id, requester).await;
        }
        Dispatch::Stop { window_id } => {
            log::info!("ai_chat: '{sender}' requested a stop on window {window_id}");
            super::session::stop(app, EndReason::Stopped);
        }
        Dispatch::OpenTurn {
            window_id,
            speaker,
            source,
        } => {
            // Establish a remote speaker's tap BEFORE the turn opens. If it
            // cannot be established the claim FAILS and says so — the one thing
            // that must never happen here is falling back to this machine's
            // microphone, which is #657's original bug.
            if let PttSource::RemoteTrack { identity } = &source {
                if let Err(error) = super::remote_audio::start(app, identity) {
                    log::warn!(
                        "ai_chat: refusing '{speaker}' the floor -- cannot reach their audio ({})",
                        error.reason()
                    );
                    release_floor(&speaker);
                    publish_state(app, window_id, true, None, Some(EndReason::Error));
                    return;
                }
            }
            if !super::session::ptt_start(&speaker, source) {
                // The engine already had a turn open. Do not leave the floor
                // claimed for a turn that never began, or the next speaker is
                // locked out.
                log::info!("ai_chat: '{speaker}' claimed the floor but no turn opened");
                super::remote_audio::stop();
                release_floor(&speaker);
                return;
            }
            publish_state(app, window_id, true, None, None);
        }
        Dispatch::FloorBusy { window_id, holder } => {
            log::info!("ai_chat: '{sender}' asked for the floor; '{holder}' has it");
            publish_state(app, window_id, true, None, Some(EndReason::Busy));
        }
        Dispatch::CloseTurn { window_id, .. } => {
            // `ptt_end` closes whichever capture path the turn opened, so a
            // remote tap cannot outlive the turn it served.
            super::session::ptt_end();
            publish_state(app, window_id, true, None, None);
        }
        Dispatch::SendText {
            window_id,
            speaker,
            text,
        } => {
            if super::session::send_text(&speaker, &text) {
                echo_sent_text(app, window_id, &text);
            } else {
                log::info!(
                    "ai_chat: '{speaker}' sent text but it was not accepted (no live session, \
                     empty/too-long text, or the PTT floor is held)"
                );
            }
        }
        Dispatch::RecordedRemoteState(state) => emit_remote_state(app, &state),
        Dispatch::RemoteTranscript(delta) => emit_remote_transcript(app, &delta),
    }
}

fn allow_request(sender: &str, now: Instant) -> bool {
    let allowed = coordination()
        .lock()
        .map(|mut c| c.limiter.allow(sender, now))
        .unwrap_or(false);
    if !allowed {
        log::warn!("ai_chat: rate-limited start/stop requests from '{sender}'");
    }
    allowed
}

fn allow_ptt(sender: &str, now: Instant) -> bool {
    let allowed = coordination()
        .lock()
        .map(|mut c| c.ptt_limiter.allow(sender, now))
        .unwrap_or(false);
    if !allowed {
        log::warn!("ai_chat: rate-limited push-to-talk claims from '{sender}'");
    }
    allowed
}

fn allow_text(sender: &str, now: Instant) -> bool {
    let allowed = coordination()
        .lock()
        .map(|mut c| c.text_limiter.allow(sender, now))
        .unwrap_or(false);
    if !allowed {
        log::warn!("ai_chat: rate-limited typed turns from '{sender}'");
    }
    allowed
}

fn claim_floor(who: &str, now: Instant) -> Claim {
    coordination()
        .lock()
        .map(|mut c| c.floor.claim(who, now))
        .unwrap_or(Claim::Busy {
            holder: String::new(),
        })
}

// ---- the local user's own push-to-talk --------------------------------------

/// Claim the floor for THIS machine's user and open their microphone.
///
/// Routed through the same [`super::session::audio_route`] the wire path uses,
/// and through the same floor, so the local user and a remote peer genuinely
/// contend for one turn instead of talking over each other into a single serial
/// stream the model cannot separate.
pub fn local_ptt_start(app: &AppHandle) -> bool {
    let Some((_, identity)) = control_channel(app) else {
        // Not in a room: nobody else can be holding the floor and there is no
        // state to publish. Purely local push-to-talk.
        return super::session::ptt_start("You", PttSource::LocalMicrophone);
    };
    let claim = coordination()
        .lock()
        .map(|mut c| c.floor.claim(&identity, Instant::now()))
        .unwrap_or(Claim::Busy {
            holder: String::new(),
        });
    match claim {
        Claim::Granted => {
            // Deliberately the shared routing function rather than a literal
            // `LocalMicrophone`: one decision site means a swapped branch
            // cannot be right here and wrong on the wire path.
            let source = super::session::audio_route(&identity, &identity);
            debug_assert_eq!(source, PttSource::LocalMicrophone);
            if !super::session::ptt_start(&identity, source) {
                release_floor(&identity);
                return false;
            }
            if let Some(window_id) = super::session::active_window_id() {
                publish_state(app, window_id, true, None, None);
            }
            true
        }
        Claim::Busy { holder } => {
            log::info!("ai_chat: local push-to-talk refused; '{holder}' has the floor");
            false
        }
    }
}

/// Release the floor this machine's user holds and close their microphone.
pub fn local_ptt_end(app: &AppHandle) {
    let Some((_, identity)) = control_channel(app) else {
        // Not in a room: no floor was ever claimed, so the open turn is
        // unambiguously ours to end.
        super::session::ptt_end();
        return;
    };
    // End the turn ONLY if we are the one holding it. A local key-up while a
    // remote peer has the floor must not cut them off mid-sentence — the same
    // rule `Floor::release` enforces for a stray `pttEnd` on the wire.
    if release_floor(&identity) {
        super::session::ptt_end();
        if let Some(window_id) = super::session::active_window_id() {
            publish_state(app, window_id, true, None, None);
        }
    }
}

/// Send a typed turn into THIS machine's own AI chat session -- the one it is
/// hosting, because it owns the window. No floor claim: see `session::
/// send_text`'s own doc for why text is not arbitrated the way PTT is.
pub fn local_send_text(app: &AppHandle, text: &str) -> bool {
    let speaker = match control_channel(app) {
        // Not in a room: no room-wide attribution needed.
        None => "You".to_string(),
        Some((_, identity)) => identity,
    };
    let sent = super::session::send_text(&speaker, text);
    if sent {
        if let Some(window_id) = super::session::active_window_id() {
            echo_sent_text(app, window_id, text);
        }
    }
    sent
}

/// Put the user's own words in the transcript at the moment they're sent --
/// a typed turn has no async server-side transcription event to hang this
/// off of the way a spoken turn's `InputText` does, so without this the
/// transcript would show the assistant's reply with no visible question.
fn echo_sent_text(app: &AppHandle, window_id: u32, text: &str) {
    super::session::emit_transcript(app, window_id, "user", text, true);
}

// ---- safety timers ----------------------------------------------------------

/// What one safety tick found. Returned rather than acted on inline so the loop
/// that drives it is testable without an `AppHandle` or a socket.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TickOutcome {
    /// The identity whose turn the timers ended, if any.
    pub expired_floor: Option<String>,
    /// Remote sessions whose host stopped heartbeating.
    pub stale_sessions: Vec<WindowKey>,
}

impl TickOutcome {
    fn is_empty(&self) -> bool {
        self.expired_floor.is_none() && self.stale_sessions.is_empty()
    }
}

/// Evaluate both timers against the shared coordination state.
fn tick_coordination(now: Instant) -> TickOutcome {
    let mut c = match coordination().lock() {
        Ok(c) => c,
        Err(_) => return TickOutcome::default(),
    };
    TickOutcome {
        expired_floor: c.floor.expire(now),
        stale_sessions: c.remote.expire_stale(now),
    }
}

/// The safety-timer loop.
///
/// `Floor::expire`, `Floor::note_audio` and `RemoteSessions::expire_stale` all
/// shipped with green unit tests and NO caller, which is worth spelling out
/// because of what that costs: a dropped `pttEnd` wedged the floor for the rest
/// of the meeting and left an audio source running with nothing to stop it.
///
/// The clock and the effects are parameters so a test can drive this loop
/// itself, not merely the pure functions underneath it.
pub(crate) async fn run_safety_ticks<S, N, A>(
    interval: Duration,
    mut still_current: S,
    mut now: N,
    mut apply: A,
) where
    S: FnMut() -> bool,
    N: FnMut() -> Instant,
    A: FnMut(TickOutcome),
{
    loop {
        tokio::time::sleep(interval).await;
        if !still_current() {
            break;
        }
        let outcome = tick_coordination(now());
        if !outcome.is_empty() {
            apply(outcome);
        }
    }
}

fn start_safety_timer(app: &AppHandle, generation: RoomGeneration) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        run_safety_ticks(
            SAFETY_TICK,
            move || generation.is_current(),
            Instant::now,
            move |outcome| apply_safety_tick(&app, outcome),
        )
        .await;
        log::debug!("ai_chat: safety timer exiting for stale room generation");
    });
}

/// Act on an expired turn or a dead host.
fn apply_safety_tick(app: &AppHandle, outcome: TickOutcome) {
    if let Some(who) = outcome.expired_floor {
        log::info!("ai_chat: floor expired -- '{who}' held it past a safety limit");
        // Ends the model's activity turn AND closes whichever capture path was
        // open. Without this an unheard `pttEnd` leaves a microphone or a
        // remote tap running for the rest of the session.
        super::session::ptt_end();
        if let Some(window_id) = super::session::active_window_id() {
            publish_state(app, window_id, true, None, None);
        }
    }
    for key in outcome.stale_sessions {
        log::info!(
            "ai_chat: session for window {} on '{}' went stale -- host stopped heartbeating",
            key.window_id,
            key.owner_identity
        );
        // The other clearing path. Both must tell the surfaces, or a crashed
        // host leaves a live-looking badge on every other machine.
        emit_remote_state(app, &RemoteState::cleared(&key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// `coordination()` is process-wide, so the tests that mutate it must not
    /// run concurrently with each other.
    fn serialize() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn reset_coordination() {
        if let Ok(mut c) = coordination().lock() {
            *c = Coordination::default();
        }
    }

    #[test]
    fn topic_matches_the_contract_module() {
        assert_eq!(TOPIC, "petal.ai-chat");
    }

    #[test]
    fn audio_from_the_holder_keeps_their_turn_alive_through_the_public_seam() {
        // `note_floor_audio` is the function `session::push_audio` calls on
        // every chunk, from either capture path. If it stopped reaching the
        // floor, a speaker mid-sentence would lose it after 5s of "silence".
        let _serial = serialize();
        reset_coordination();
        let t0 = Instant::now();
        coordination().lock().unwrap().floor.claim("bob", t0);

        note_floor_audio("bob");
        assert_eq!(
            tick_coordination(Instant::now() + super::super::room::SILENCE_TIMEOUT
                - Duration::from_millis(1))
            .expired_floor,
            None
        );
        assert_eq!(floor_holder().as_deref(), Some("bob"));
        reset_coordination();
    }

    #[tokio::test(start_paused = true)]
    async fn the_safety_tick_loop_expires_a_wedged_floor_and_a_dead_host() {
        // The regression that matters here is NOT that `Floor::expire` is
        // correct — it always was, and its unit tests always passed. It is that
        // something CALLS it. This drives the real loop against the real shared
        // coordination state and asserts the effects reach the caller.
        let _serial = serialize();
        reset_coordination();

        let t0 = Instant::now();
        let stale_key = WindowKey {
            window_id: 7,
            owner_identity: "carol".into(),
        };
        {
            let mut c = coordination().lock().unwrap();
            // Bob grabs the floor and is never heard from again: his `pttEnd`
            // was lost. Carol's session heartbeats once and then her host dies.
            c.floor.claim("bob", t0);
            c.remote.observe(
                &stale_key,
                SessionReport {
                    active: true,
                    started_by: Some("bob".into()),
                    seconds_left: Some(300),
                    ..SessionReport::default()
                },
                t0,
            );
        }

        // A fake clock advancing one tick per iteration, so a 60s max-hold does
        // not need 60 real seconds. `start_paused` lets the sleeps auto-advance.
        let tick = Duration::from_millis(500);
        let ticks = Arc::new(AtomicU64::new(1));
        let clock_ticks = ticks.clone();
        let guard_ticks = ticks.clone();
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let sink = outcomes.clone();

        run_safety_ticks(
            tick,
            // Long enough for both deadlines (5s silence, 15s heartbeat).
            move || guard_ticks.load(Ordering::SeqCst) <= 60,
            move || t0 + tick * clock_ticks.fetch_add(1, Ordering::SeqCst) as u32,
            move |outcome| sink.lock().unwrap().push(outcome),
        )
        .await;

        let outcomes = outcomes.lock().unwrap();
        let expired: Vec<&String> = outcomes
            .iter()
            .filter_map(|o| o.expired_floor.as_ref())
            .collect();
        assert_eq!(
            expired,
            vec![&"bob".to_string()],
            "the loop must expire the wedged floor exactly once"
        );
        let stale: Vec<&WindowKey> = outcomes.iter().flat_map(|o| &o.stale_sessions).collect();
        assert_eq!(
            stale,
            vec![&stale_key],
            "a host that stops heartbeating must be cleared by the same tick"
        );
        drop(outcomes);

        // And the shared state actually changed, not just the report.
        assert_eq!(floor_holder(), None);
        reset_coordination();
    }

    #[tokio::test(start_paused = true)]
    async fn the_safety_tick_loop_stops_with_its_room_generation() {
        // A loop that outlived its room would keep expiring the next room's
        // floor from the previous room's clock.
        let _serial = serialize();
        reset_coordination();
        let t0 = Instant::now();
        let ticks = Arc::new(AtomicU64::new(0));
        let seen = ticks.clone();
        run_safety_ticks(
            Duration::from_millis(500),
            move || seen.fetch_add(1, Ordering::SeqCst) < 3,
            move || t0,
            |_| panic!("nothing should expire"),
        )
        .await;
        assert_eq!(ticks.load(Ordering::SeqCst), 4, "loop did not stop promptly");
    }

    // ---- the dispatch harness ------------------------------------------------
    //
    // Nothing anywhere exercised `handle`. The wire tests parsed the fixture and
    // re-emitted it; no test ever drove a peer message through dispatch, which
    // is exactly why three window-id bugs sat in it (#661). These drive real
    // `Message` values, with a controlled sender identity, against a controlled
    // view of this host's session, and assert what CHANGED: the floor, the
    // remote-session store, and which action would run and publish.

    const OWNER: &str = "alice";

    fn msg(window_id: u32, body: Body) -> Message {
        Message {
            v: VERSION,
            key: WindowKey {
                window_id,
                owner_identity: OWNER.into(),
            },
            body,
        }
    }

    /// This host, running a live session on `window_id`.
    fn running_on(window_id: u32) -> LocalSession {
        LocalSession {
            live_window: Some(window_id),
            slot_window: Some(window_id),
        }
    }

    /// This host, with a start still connecting for `window_id`.
    fn connecting_on(window_id: u32) -> LocalSession {
        LocalSession {
            live_window: None,
            slot_window: Some(window_id),
        }
    }

    fn live_state(started_by: &str) -> Body {
        Body::State {
            active: true,
            started_by: Some(started_by.into()),
            seconds_left: Some(240),
            active_speaker: None,
            error: None,
        }
    }

    #[test]
    fn a_peer_start_request_reaches_the_start_path_only_on_the_windows_host() {
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();

        assert_eq!(
            decide(
                &msg(9, Body::StartRequest),
                "bob",
                Some(OWNER),
                &LocalSession::default(),
                now
            ),
            Dispatch::Start {
                window_id: 9,
                requester: "bob".into()
            },
            "a peer's startRequest must reach the host's own start path, \
             attributed to the AUTHENTICATED sender"
        );
        // Everyone in the room receives the packet; only the owner acts.
        assert_eq!(
            decide(
                &msg(9, Body::StartRequest),
                "bob",
                Some("carol"),
                &LocalSession::default(),
                now
            ),
            Dispatch::Ignore(Ignored::NotTheOwner)
        );
        reset_coordination();
    }

    #[test]
    fn ptt_start_for_a_window_with_no_session_never_touches_the_running_one() {
        // THE blocker: `we_own_it` compared identities only. A peer sending
        // `pttStart {windowId: 5}` while the session ran on window 9 opened a
        // turn on session 9 and published `state{windowId: 5, active: true}`,
        // so every receiver rendered "AI chat live" on a window with nothing
        // running.
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();
        let local = running_on(9);

        assert_eq!(
            decide(&msg(5, Body::PttStart), "bob", Some(OWNER), &local, now),
            Dispatch::Ignore(Ignored::WrongWindow),
            "a pttStart naming window 5 opened a turn on the session running on window 9"
        );
        assert_eq!(
            floor_holder(),
            None,
            "the floor was claimed for a window with no session -- the real \
             speaker is now locked out and a phantom state was published"
        );

        // The other direction: naming the window that IS running works, and
        // routes the audio to the remote speaker's own track.
        assert_eq!(
            decide(&msg(9, Body::PttStart), "bob", Some(OWNER), &local, now),
            Dispatch::OpenTurn {
                window_id: 9,
                speaker: "bob".into(),
                source: PttSource::RemoteTrack {
                    identity: "bob".into()
                },
            }
        );
        assert_eq!(floor_holder().as_deref(), Some("bob"));

        // And a start that has not finished connecting is not a session either:
        // there is no socket to open a turn on.
        reset_coordination();
        assert_eq!(
            decide(
                &msg(9, Body::PttStart),
                "bob",
                Some(OWNER),
                &connecting_on(9),
                now
            ),
            Dispatch::Ignore(Ignored::WrongWindow)
        );
        assert_eq!(floor_holder(), None);
        reset_coordination();
    }

    #[test]
    fn a_stop_request_for_the_wrong_window_does_not_kill_the_running_session() {
        // `stopRequest {windowId: 5}` killed the session on window 9.
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();
        let local = running_on(9);

        assert_eq!(
            decide(&msg(5, Body::StopRequest), "bob", Some(OWNER), &local, now),
            Dispatch::Ignore(Ignored::WrongWindow),
            "a stopRequest naming window 5 killed the session on window 9"
        );
        assert_eq!(
            decide(&msg(9, Body::StopRequest), "bob", Some(OWNER), &local, now),
            Dispatch::Stop { window_id: 9 }
        );
        // A stop must still reach a session that is only connecting — otherwise
        // the End button is dead for the whole connect.
        assert_eq!(
            decide(
                &msg(9, Body::StopRequest),
                "bob",
                Some(OWNER),
                &connecting_on(9),
                now
            ),
            Dispatch::Stop { window_id: 9 }
        );
        reset_coordination();
    }

    #[test]
    fn the_floor_claims_and_releases_round_trip_through_dispatch() {
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();
        let local = running_on(9);

        // Bob takes it.
        assert!(matches!(
            decide(&msg(9, Body::PttStart), "bob", Some(OWNER), &local, now),
            Dispatch::OpenTurn { .. }
        ));
        assert_eq!(floor_holder().as_deref(), Some("bob"));

        // Carol is told who has it rather than talking over him.
        assert_eq!(
            decide(&msg(9, Body::PttStart), "carol", Some(OWNER), &local, now),
            Dispatch::FloorBusy {
                window_id: 9,
                holder: "bob".into()
            }
        );
        assert_eq!(floor_holder().as_deref(), Some("bob"));

        // A stray pttEnd from Carol must not cut Bob off mid-sentence.
        assert_eq!(
            decide(&msg(9, Body::PttEnd), "carol", Some(OWNER), &local, now),
            Dispatch::Ignore(Ignored::NotTheHolder)
        );
        assert_eq!(floor_holder().as_deref(), Some("bob"));

        // Bob gives it up, and Carol can then have it.
        assert_eq!(
            decide(&msg(9, Body::PttEnd), "bob", Some(OWNER), &local, now),
            Dispatch::CloseTurn {
                window_id: 9,
                speaker: "bob".into()
            }
        );
        assert_eq!(floor_holder(), None);
        assert!(matches!(
            decide(&msg(9, Body::PttStart), "carol", Some(OWNER), &local, now),
            Dispatch::OpenTurn { .. }
        ));
        assert_eq!(floor_holder().as_deref(), Some("carol"));

        // And a pttEnd naming the wrong window leaves the turn alone.
        assert_eq!(
            decide(&msg(5, Body::PttEnd), "carol", Some(OWNER), &local, now),
            Dispatch::Ignore(Ignored::WrongWindow)
        );
        assert_eq!(floor_holder().as_deref(), Some("carol"));
        reset_coordination();
    }

    #[test]
    fn push_to_talk_is_rate_limited_on_its_own_budget() {
        // The limiter sat in the start/stop arm only, so pttStart spam was
        // unbounded — each one claims the floor, taps a track and publishes.
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();
        let local = running_on(9);

        for i in 0..MAX_PTT_STARTS_PER_SENDER_PER_MINUTE {
            assert!(
                !matches!(
                    decide(&msg(9, Body::PttStart), "mallory", Some(OWNER), &local, now),
                    Dispatch::Ignore(Ignored::RateLimited)
                ),
                "claim {i} was limited too early"
            );
        }
        assert_eq!(
            decide(&msg(9, Body::PttStart), "mallory", Some(OWNER), &local, now),
            Dispatch::Ignore(Ignored::RateLimited),
            "pttStart spam is unbounded"
        );

        // The budgets are separate: exhausting push-to-talk must not disarm the
        // start/stop path, and vice versa.
        assert_eq!(
            decide(
                &msg(9, Body::StartRequest),
                "mallory",
                Some(OWNER),
                &LocalSession::default(),
                now
            ),
            Dispatch::Start {
                window_id: 9,
                requester: "mallory".into()
            }
        );
        reset_coordination();
    }

    #[test]
    fn send_text_reaches_the_session_only_for_the_right_window_and_owner() {
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();
        let local = running_on(9);

        // Same #661-class check PttStart needed: naming the wrong window must
        // not reach whatever session actually happens to be running.
        assert_eq!(
            decide(
                &msg(5, Body::SendText { text: "hi".into() }),
                "bob",
                Some(OWNER),
                &local,
                now
            ),
            Dispatch::Ignore(Ignored::WrongWindow)
        );

        // Only the owner acts -- everyone in the room receives the packet.
        assert_eq!(
            decide(
                &msg(9, Body::SendText { text: "hi".into() }),
                "bob",
                Some("carol"),
                &local,
                now
            ),
            Dispatch::Ignore(Ignored::NotTheOwner)
        );

        // The right window, acted on by the owner, dispatches to send it --
        // attributed to the AUTHENTICATED sender, not anything in the payload.
        assert_eq!(
            decide(
                &msg(9, Body::SendText { text: "hi".into() }),
                "bob",
                Some(OWNER),
                &local,
                now
            ),
            Dispatch::SendText {
                window_id: 9,
                speaker: "bob".into(),
                text: "hi".into(),
            }
        );
        reset_coordination();
    }

    #[test]
    fn send_text_is_rate_limited_on_its_own_budget() {
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();
        let local = running_on(9);

        for i in 0..super::super::wire::MAX_TEXT_SENDS_PER_SENDER_PER_MINUTE {
            assert!(
                !matches!(
                    decide(
                        &msg(9, Body::SendText { text: "hi".into() }),
                        "mallory",
                        Some(OWNER),
                        &local,
                        now
                    ),
                    Dispatch::Ignore(Ignored::RateLimited)
                ),
                "send {i} was limited too early"
            );
        }
        assert_eq!(
            decide(
                &msg(9, Body::SendText { text: "hi".into() }),
                "mallory",
                Some(OWNER),
                &local,
                now
            ),
            Dispatch::Ignore(Ignored::RateLimited),
            "text-send spam is unbounded"
        );

        // The budgets are separate: exhausting text sends must not disarm PTT.
        assert!(!matches!(
            decide(&msg(9, Body::PttStart), "mallory", Some(OWNER), &local, now),
            Dispatch::Ignore(Ignored::RateLimited)
        ));
        reset_coordination();
    }

    #[test]
    fn start_and_stop_requests_stay_rate_limited() {
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();

        for i in 0..super::super::wire::MAX_REQUESTS_PER_SENDER_PER_MINUTE {
            assert_eq!(
                decide(
                    &msg(9, Body::StartRequest),
                    "mallory",
                    Some(OWNER),
                    &LocalSession::default(),
                    now
                ),
                Dispatch::Start {
                    window_id: 9,
                    requester: "mallory".into()
                },
                "request {i}"
            );
        }
        assert_eq!(
            decide(
                &msg(9, Body::StartRequest),
                "mallory",
                Some(OWNER),
                &LocalSession::default(),
                now
            ),
            Dispatch::Ignore(Ignored::RateLimited)
        );
        reset_coordination();
    }

    #[test]
    fn a_remote_hosts_state_is_recorded_and_our_own_echo_is_not() {
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();
        let carols_window = WindowKey {
            window_id: 3,
            owner_identity: "carol".into(),
        };
        let from_carol = Message {
            v: VERSION,
            key: carols_window.clone(),
            body: live_state("dave"),
        };

        let dispatch = decide(&from_carol, "carol", Some(OWNER), &LocalSession::default(), now);
        assert_eq!(
            dispatch,
            Dispatch::RecordedRemoteState(RemoteState {
                window_id: 3,
                owner_identity: "carol".into(),
                active: true,
                started_by: Some("dave".into()),
                seconds_left: Some(240),
                active_speaker: None,
                error: None,
            })
        );
        assert_eq!(remote_session(&carols_window), Some(match dispatch {
            Dispatch::RecordedRemoteState(state) => state,
            other => panic!("{other:?}"),
        }));

        // Our own heartbeat coming back to us is not news.
        assert_eq!(
            decide(
                &msg(9, live_state(OWNER)),
                OWNER,
                Some(OWNER),
                &running_on(9),
                now
            ),
            Dispatch::Ignore(Ignored::OwnEcho)
        );

        // The fourth message kind (#664: now actually relayed to the
        // receiver UI, not just authorized-and-ignored).
        let line = Message {
            v: VERSION,
            key: carols_window,
            body: Body::Transcript {
                role: TranscriptRole::Assistant,
                text: "the deploy is green".into(),
                final_: true,
            },
        };
        assert_eq!(
            decide(&line, "carol", Some(OWNER), &LocalSession::default(), now),
            Dispatch::RemoteTranscript(RemoteTranscriptDelta {
                window_id: 3,
                owner_identity: "carol".into(),
                role: TranscriptRole::Assistant,
                text: "the deploy is green".into(),
                final_: true,
            })
        );
        // Our own transcript line coming back to us is not news either --
        // we already have it locally via session::emit_transcript's direct
        // EVENT_TRANSCRIPT emit. "Our own" means the LOCAL client owns
        // carol's window, i.e. this client IS carol.
        assert_eq!(
            decide(&line, "carol", Some("carol"), &LocalSession::default(), now),
            Dispatch::Ignore(Ignored::OwnEcho)
        );
        reset_coordination();
    }

    #[test]
    fn an_unauthorized_message_changes_nothing_even_if_dispatch_is_reached() {
        // The receiver refuses first — that is the real boundary and it stays
        // there. This pins the second line of defence, so a future caller of
        // the dispatcher cannot skip authorization by accident.
        let _serial = serialize();
        reset_coordination();
        let now = Instant::now();
        let key = WindowKey {
            window_id: 3,
            owner_identity: "carol".into(),
        };
        let forged = Message {
            v: VERSION,
            key: key.clone(),
            body: live_state("mallory"),
        };
        assert_eq!(
            decide(&forged, "mallory", Some(OWNER), &LocalSession::default(), now),
            Dispatch::Ignore(Ignored::Unauthorized(wire::Rejection::NotWindowOwner))
        );
        assert_eq!(
            remote_session(&key),
            None,
            "a peer faked a running session on someone else's window"
        );
        reset_coordination();
    }

    #[test]
    fn the_remote_state_payload_uses_the_key_names_the_ui_parses() {
        let state = RemoteState {
            window_id: 3,
            owner_identity: "carol".into(),
            active: true,
            started_by: Some("dave".into()),
            seconds_left: Some(240),
            active_speaker: Some("dave".into()),
            error: Some(EndReason::Busy),
        };
        let json = serde_json::to_string(&state).unwrap();
        for key in [
            "\"windowId\":3",
            "\"ownerIdentity\":\"carol\"",
            "\"active\":true",
            "\"startedBy\":\"dave\"",
            "\"secondsLeft\":240",
            "\"activeSpeaker\":\"dave\"",
            "\"error\":\"busy\"",
        ] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
        // Absent optionals must not appear as nulls.
        let cleared = serde_json::to_string(&RemoteState::cleared(&WindowKey {
            window_id: 3,
            owner_identity: "carol".into(),
        }))
        .unwrap();
        assert!(cleared.contains("\"active\":false"), "{cleared}");
        assert!(!cleared.contains("startedBy"), "{cleared}");
        assert!(!cleared.contains("null"), "{cleared}");
    }

    #[test]
    fn a_state_message_from_a_non_owner_never_reaches_handling() {
        // The receiver drops it at `authorize`; this pins the expectation that
        // handling is unreachable for an unauthorized sender.
        let message = Message {
            v: VERSION,
            key: WindowKey {
                window_id: 1,
                owner_identity: "alice".into(),
            },
            body: Body::State {
                active: true,
                started_by: None,
                seconds_left: None,
                active_speaker: None,
                error: None,
            },
        };
        assert!(wire::authorize(&message, "mallory").is_err());
        assert!(wire::authorize(&message, "alice").is_ok());
    }
}
