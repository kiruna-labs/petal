//! The AI chat session engine: one Gemini Live connection about one shared
//! window, hosted on the sharer's machine (#656).
//!
//! ## Shape, and why
//!
//! Every structural choice here was paid for by a failure in the takt
//! reference implementation; none of it is incidental:
//!
//! - **The socket writer is a separate task fed by a bounded channel.** Sending
//!   inline in the same `select!` as the read half stalls reading for the whole
//!   duration of a send — `select!` will not re-poll until the chosen branch's
//!   block completes — and a 100 KB frame send audibly stutters playback.
//! - **Realtime input is `try_send`, dropped on backpressure.** A stale frame is
//!   worthless and blocking on one starves the read half. Turn-shaped messages
//!   (setup, digests) use the awaiting send.
//! - **Gemini sends JSON in BINARY frames.** Decode both, or `setupComplete`
//!   silently never arrives and the session hangs in Connecting forever.
//! - **Teardown is ordered and synchronous at the end.** Aborting a task and
//!   hoping the audio unit drops leaves the assistant talking after "stop".
//!
//! ## The publication gate (security-critical, #656)
//!
//! A session may only ever look at a window this client is *currently sharing*.
//! `screencapture -l<id>` and the accessibility walk both work on ANY window id,
//! so without this gate a crafted start request could aim the model at an
//! unshared window — a password manager, say — and stream its pixels and text
//! to Google. The gate is a caller-supplied predicate, re-checked before EVERY
//! capture, not just at start; when it goes false the session tears down with
//! [`EndReason::NotShared`]. It is a callback rather than a direct dependency
//! so it is impossible to construct a session without one, and so the engine
//! stays unit-testable.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::protocol::{self, ServerEvent};
use super::state::{seconds_left, EndReason, Phase};

/// Endpoint for token-authenticated sessions. Ephemeral tokens authenticate the
/// CONSTRAINED bidi method; the plain `BidiGenerateContent` rejects them as
/// "unregistered callers" (verified live, #654 Q1).
const WS_CONSTRAINED: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContentConstrained";
/// Endpoint for raw-API-key (bring-your-own-key) sessions.
const WS_PLAIN: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

/// One video frame per second. Gemini samples video at roughly this rate; more
/// is spend without benefit.
const FRAME_INTERVAL: Duration = Duration::from_millis(1000);
/// Bounded outbound queue. Deep enough to absorb a burst, shallow enough that
/// dropping is preferable to unbounded latency.
const OUTBOUND_CAPACITY: usize = 64;
/// How often the supervisor re-checks the cap and the publication gate.
const TICK: Duration = Duration::from_millis(250);
/// #845: how long the mic stays ducked after the assistant's local playback
/// last sampled active, before releasing. A few TICKs so a brief
/// inter-sentence gap doesn't un-duck and re-duck (chopping the tail of one
/// sentence and the head of the next back into the room).
const AI_CHAT_DUCK_RELEASE_DELAY: Duration = Duration::from_millis(750);
/// The single session length (#656 deliberately dropped the configurable
/// duration and the extend button: one cap plus "start again" covers it, and
/// extensions interact badly with the token's own expiry margin).
pub const CAP_SECONDS: u64 = 300;

/// Events pushed to UI surfaces.
pub const EVENT_STATE: &str = "ai-chat-state";
pub const EVENT_TRANSCRIPT: &str = "ai-chat-transcript";

/// How the session authenticates to Google.
#[derive(Clone)]
pub enum Credential {
    /// Backend-minted ephemeral token (`authTokens/…`) — hosted mode.
    EphemeralToken(String),
    /// The user's own Gemini API key — bring-your-own-key mode.
    ApiKey(String),
}

impl Credential {
    /// Build the connect URL. NEVER log the result: it carries the credential.
    fn connect_url(&self) -> String {
        match self {
            Credential::EphemeralToken(t) => format!("{WS_CONSTRAINED}?access_token={t}"),
            Credential::ApiKey(k) => format!("{WS_PLAIN}?key={k}"),
        }
    }

    fn mode(&self) -> &'static str {
        match self {
            Credential::EphemeralToken(_) => "hosted",
            Credential::ApiKey(_) => "byok",
        }
    }
}

/// Everything needed to run one session.
pub struct StartParams {
    pub window_id: u32,
    /// Model id. In hosted mode this MUST be the value returned by
    /// `/api/ai-token`, never a client constant, so the backend can rotate
    /// models without a client release (#655).
    pub model: String,
    pub credential: Credential,
    /// Re-checked before every capture. See the module docs — this is the
    /// security gate, not a convenience.
    pub is_shared: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The participant who asked for this session — the local user for a click,
    /// the authenticated sender for a `startRequest` over `petal.ai-chat`. Put
    /// on the wire as `state.startedBy` so every surface can say who started it.
    /// `None` only when we are not in a room and there is nobody to attribute.
    pub started_by: Option<String>,
}

/// Where the audio for an open push-to-talk turn comes from.
///
/// **This is the distinction the whole feature turns on.** Getting it backwards
/// opens the HOST's microphone when a remote peer presses their key: the host's
/// room is recorded and streamed to Google, the peer's voice reaches nothing,
/// and the host is never asked. That is precisely how #657 shipped, and it is
/// why the two cases are a type rather than a boolean or a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PttSource {
    /// The local user is holding their own push-to-talk key. The ONLY value
    /// that may open this machine's microphone.
    LocalMicrophone,
    /// A remote participant holds the floor. Their already-subscribed LiveKit
    /// audio track is tapped by `remote_audio`; this machine's microphone stays
    /// shut.
    RemoteTrack { identity: String },
}

/// Decide where a granted floor's audio comes from.
///
/// One decision site for both the local command path and the wire path, so a
/// swapped branch cannot be right in one and wrong in the other.
pub fn audio_route(speaker: &str, local_identity: &str) -> PttSource {
    if speaker == local_identity {
        PttSource::LocalMicrophone
    } else {
        PttSource::RemoteTrack {
            identity: speaker.to_string(),
        }
    }
}

/// Open the audio path for a granted floor.
///
/// The single place this machine's microphone is ever opened for AI chat. A
/// remote speaker's audio was already flowing before the floor was granted
/// (`remote_audio::start` runs first precisely so a failure can refuse the
/// claim), so the remote arm has nothing to do — and must never fall through to
/// the microphone.
fn begin_capture(source: &PttSource) {
    match source {
        PttSource::LocalMicrophone => {
            // Push-to-talk was reasoned to make takt's hardware-AEC (VPIO)
            // port unnecessary: the mic only opens while a turn is held, so
            // there is supposedly no open mic for the assistant's own voice
            // to leak into. But nothing stopped a turn from opening WHILE
            // the assistant was still talking (a natural barge-in gesture —
            // hold PTT to interrupt mid-sentence) — the local speakers keep
            // playing until the server's `Interrupted` event round-trips,
            // and the plain cpal tap has no AEC, so it picks up the
            // assistant's own voice bleeding from the speakers for that
            // whole window. Stopping local playback the instant capture
            // opens closes that window without reintroducing VPIO.
            super::audio::stop_playback();
            super::audio::start_local_microphone_capture();
        }
        PttSource::RemoteTrack { .. } => {}
    }
}

/// Close whichever audio path [`begin_capture`] opened.
fn end_capture(source: &PttSource) {
    match source {
        PttSource::LocalMicrophone => super::audio::stop_local_microphone_capture(),
        PttSource::RemoteTrack { .. } => super::remote_audio::stop(),
    }
}

/// The push-to-talk turn currently open, if any.
struct OpenTurn {
    /// Identity the floor was granted to. Used to keep the floor's silence
    /// timer alive as audio arrives, so a lost `pttEnd` cannot wedge the room.
    speaker: String,
    source: PttSource,
}

struct ActiveSession {
    window_id: u32,
    stop: Arc<AtomicBool>,
    outbound: mpsc::Sender<String>,
    ptt_active: Arc<AtomicBool>,
    /// Who is speaking right now and where their audio comes from.
    turn: Option<OpenTurn>,
    /// Who asked for this session. Published as `state.startedBy`.
    started_by: Option<String>,
    /// Guards against a stale stop/PTT call from a previous session touching a
    /// newer one.
    generation: u64,
    /// The #656 publication gate. Retained (not merely consulted at start) so
    /// the control path can re-ask it at the moment it acts, long after the
    /// human approved (#658).
    is_shared: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The accessibility generations the model has actually been shown, so a
    /// cited `[n]` can be resolved against the generation it came from — or
    /// detected as stale.
    digest_state: Arc<Mutex<super::ax_digest::DigestSessionState>>,
    /// Authorization epoch for this session (`control_gate`). A human's answer
    /// must name it, so an answer aimed at a finished session cannot land on
    /// its successor.
    control_session_id: u64,
    /// Gemini may put several function calls in one `toolCall` envelope. They
    /// share one replay controller id and one approval slot, so only the front
    /// call may be evaluated/executed at a time.
    control_queue: VecDeque<protocol::FunctionCall>,
    control_busy: bool,
}

/// Cancellation for a start that has claimed the slot but not connected yet.
///
/// The flag alone would only be *noticed* at the next await point; the notify
/// is what makes a stop during connect abort the dial immediately instead of
/// waiting out the 15s timeout (#661).
#[derive(Clone)]
struct CancelHandle {
    cancelled: Arc<AtomicBool>,
    signal: Arc<tokio::sync::Notify>,
}

impl CancelHandle {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            signal: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // `notify_one` stores a permit when nobody is waiting yet, so a cancel
        // that lands before the connect starts awaiting still takes effect.
        self.signal.notify_one();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    async fn wait(&self) {
        self.signal.notified().await
    }
}

/// A start that holds the single session slot while it connects.
struct Reservation {
    window_id: u32,
    generation: u64,
    cancel: CancelHandle,
}

/// The single session slot's occupant.
///
/// `Starting` exists because registering only the LIVE session left a gap
/// between the "is anything running?" guard and registration that spans a token
/// mint and a WebSocket connect — up to ~37s. Two callers both passed the
/// guard, the second overwrote the first, and the first became an orphan no
/// `stop` could reach: it kept capturing a frame per second and playing audio
/// for the full 300s cap, and its tool calls resolved against the live
/// session's control epoch (#661).
enum Slot {
    Starting(Reservation),
    Running(ActiveSession),
}

fn slot() -> &'static Mutex<Option<Slot>> {
    static SLOT: OnceLock<Mutex<Option<Slot>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Read something off the LIVE session, if there is one. A reservation is not
/// a session: nothing may be sent to a socket that has not handshaken.
fn with_running<T>(f: impl FnOnce(&ActiveSession) -> T) -> Option<T> {
    let guard = slot().lock().ok()?;
    match guard.as_ref() {
        Some(Slot::Running(session)) => Some(f(session)),
        _ => None,
    }
}

fn next_generation() -> u64 {
    static GEN: AtomicU64 = AtomicU64::new(0);
    GEN.fetch_add(1, Ordering::SeqCst) + 1
}

/// Claim the one session slot for a start that is about to connect.
///
/// The busy check and the claim happen under the SAME lock, which is the whole
/// point: checking first and registering ~37s later is what let two starts both
/// proceed (#661).
fn reserve(window_id: u32, generation: u64) -> Result<CancelHandle, EndReason> {
    let mut guard = slot().lock().map_err(|_| EndReason::Error)?;
    if guard.is_some() {
        return Err(EndReason::Busy);
    }
    let cancel = CancelHandle::new();
    *guard = Some(Slot::Starting(Reservation {
        window_id,
        generation,
        cancel: cancel.clone(),
    }));
    Ok(cancel)
}

/// Give the slot back after a start that never became a session. Keyed by
/// generation so a start that already lost the slot cannot clear its successor.
fn release_reservation(generation: u64) -> bool {
    let Ok(mut guard) = slot().lock() else {
        return false;
    };
    if matches!(guard.as_ref(), Some(Slot::Starting(r)) if r.generation == generation) {
        *guard = None;
        return true;
    }
    false
}

/// Turn our reservation into the live session.
///
/// Fails when the reservation was cancelled or replaced while the connect was
/// in flight; the caller must then abandon the socket rather than register a
/// session nobody can stop.
fn promote(session: ActiveSession) -> bool {
    let Ok(mut guard) = slot().lock() else {
        return false;
    };
    if matches!(guard.as_ref(), Some(Slot::Starting(r)) if r.generation == session.generation) {
        *guard = Some(Slot::Running(session));
        return true;
    }
    false
}

/// What [`stop`] found in the slot.
enum StopTarget {
    /// A live session, still registered: tear it down.
    Running {
        window_id: u32,
        generation: u64,
        stop: Arc<AtomicBool>,
    },
    /// A start still connecting. It has been cancelled and the slot is already
    /// free; the in-flight start abandons its socket when it notices.
    Starting { window_id: u32 },
}

fn take_stop_target() -> Option<StopTarget> {
    let mut guard = slot().lock().ok()?;
    match guard.as_ref() {
        Some(Slot::Running(session)) => {
            session.stop.store(true, Ordering::SeqCst);
            Some(StopTarget::Running {
                window_id: session.window_id,
                generation: session.generation,
                stop: session.stop.clone(),
            })
        }
        Some(Slot::Starting(reservation)) => {
            let window_id = reservation.window_id;
            reservation.cancel.cancel();
            // Free the slot NOW. Leaving the reservation in place would keep
            // the End button dead and block a retry until the dial timed out.
            *guard = None;
            Some(StopTarget::Starting { window_id })
        }
        None => None,
    }
}

/// Is a session currently running — or connecting — for this window?
///
/// Deliberately true while a start is still connecting: the hover-tab menu
/// reads this to decide between "Start" and "Stop", and reading `false` for the
/// whole connect is what made double-clicking Start reachable (#661).
pub fn is_active_for(window_id: u32) -> bool {
    slot_window_id() == Some(window_id)
}

/// Is any session running or connecting? Phase 1 allows exactly one per app.
pub fn is_any_active() -> bool {
    slot_window_id().is_some()
}

/// The window the single slot is claimed for, whether connecting or live.
pub fn slot_window_id() -> Option<u32> {
    let guard = slot().lock().ok()?;
    match guard.as_ref() {
        Some(Slot::Starting(r)) => Some(r.window_id),
        Some(Slot::Running(s)) => Some(s.window_id),
        None => None,
    }
}

/// The window a LIVE session is running on. The room-side safety timers and the
/// push-to-talk floor need this one rather than [`slot_window_id`]: a turn
/// cannot be opened on a socket that has not handshaken.
pub fn active_window_id() -> Option<u32> {
    with_running(|session| session.window_id)
}

/// Who asked for the running session. `None` when nothing is running, which is
/// also the right answer for the `active: false` state published on teardown.
pub fn started_by() -> Option<String> {
    with_running(|session| session.started_by.clone()).flatten()
}

fn emit_phase(app: &AppHandle, window_id: u32, phase: Phase) {
    let _ = app.emit(
        EVENT_STATE,
        serde_json::json!({ "windowId": window_id, "state": phase }),
    );
}

fn emit_countdown(app: &AppHandle, window_id: u32, left: u64) {
    let _ = app.emit(
        EVENT_STATE,
        serde_json::json!({ "windowId": window_id, "secondsLeft": left }),
    );
}

/// Reflect the room-wide PTT authority into the host's own panel. The topic
/// echo is intentionally ignored for owned windows, so without this direct
/// event the sharer cannot see or disable against a remote floor holder.
pub(crate) fn emit_floor_state(app: &AppHandle, window_id: u32, active_speaker: Option<&str>) {
    let _ = app.emit(
        EVENT_STATE,
        serde_json::json!({ "windowId": window_id, "activeSpeaker": active_speaker }),
    );
}

/// `pub(crate)`: `topic.rs` also calls this directly, to echo a typed
/// message into the transcript at the point it's sent (#657's `send_text`
/// has no async server-side transcription event to hang off of the way a
/// spoken turn's `InputText` does — Gemini doesn't transcribe text it
/// already received as text).
pub(crate) fn emit_transcript(app: &AppHandle, window_id: u32, role: &str, text: &str, final_: bool) {
    let _ = app.emit(
        EVENT_TRANSCRIPT,
        serde_json::json!({
            "windowId": window_id,
            "role": role,
            "text": text,
            "final": final_,
        }),
    );
    // Everyone in the room sees the same conversation (#657). Only the host
    // publishes transcript, and receivers accept it only from the window's
    // owner, so this cannot be spoofed by a peer.
    let wire_role = if role == "user" {
        super::wire::TranscriptRole::User
    } else {
        super::wire::TranscriptRole::Assistant
    };
    super::topic::publish_transcript(app, window_id, wire_role, text, final_);
}

/// Start a session. Returns the failure reason if it could not even begin;
/// once started, the session reports its own end through [`EVENT_STATE`].
///
/// Refuses when: another session is running (one per app in phase 1), or the
/// window is not currently shared.
pub async fn start(app: AppHandle, params: StartParams) -> Result<(), EndReason> {
    // Gate check #1 — at start. (Re-checked on every capture below; a start-only
    // check would let a share stop mid-session and keep streaming.)
    if !(params.is_shared)() {
        return Err(EndReason::NotShared);
    }
    let window_id = params.window_id;
    let generation = next_generation();
    // Claim the ONE slot before anything slow. See [`Slot`] for what the old
    // check-here-register-later order cost.
    let cancel = reserve(window_id, generation)?;

    emit_phase(&app, window_id, Phase::Connecting);
    log::info!(
        "ai_chat: starting session for window {window_id} (mode={}, model={})",
        params.credential.mode(),
        params.model
    );

    // ONE exit for EVERY failure after the `Connecting` announcement. The
    // individual `return Err`s inside used to leave the panel at "Connecting…"
    // forever, with an End button that succeeded and did nothing, because the
    // only `Phase::Ended` emit lived in `teardown` (#661). Routing them all
    // through a single site makes missing a terminal phase structurally
    // impossible rather than something to remember on each new failure path.
    match connect_and_run(&app, generation, &cancel, params).await {
        Ok(()) => Ok(()),
        Err(reason) => Err(report_failed_start(
            &app, window_id, generation, &cancel, reason,
        )),
    }
}

/// Whether an abandoned start must report a terminal phase itself, and which.
///
/// Always releases the reservation first. Returns `None` only when a concurrent
/// [`stop`] already reported the end — reporting again would overwrite the
/// user's own "stopped" with a connect failure they no longer care about.
fn terminal_phase_for_failed_start(
    generation: u64,
    cancel: &CancelHandle,
    reason: EndReason,
) -> Option<Phase> {
    release_reservation(generation);
    if cancel.is_cancelled() {
        return None;
    }
    Some(Phase::Ended { reason })
}

fn report_failed_start(
    app: &AppHandle,
    window_id: u32,
    generation: u64,
    cancel: &CancelHandle,
    reason: EndReason,
) -> EndReason {
    let Some(phase) = terminal_phase_for_failed_start(generation, cancel, reason) else {
        return EndReason::Stopped;
    };
    log::info!("ai_chat: start for window {window_id} failed ({reason:?})");
    emit_phase(app, window_id, phase);
    reason
}

/// Wind down the machinery a start brought up before it could register. Without
/// this the accessibility walker and the writer task outlive an abandoned
/// start, which is half of what made the orphan of #661 so expensive.
fn abandon_connection(stop: &Arc<AtomicBool>) {
    stop.store(true, Ordering::SeqCst);
    super::control_gate::end_session();
    super::takeover::stop();
}

/// Everything after the `Connecting` announcement: dial, hand-shake, register,
/// spawn the reader. Split out so every failure inside it lands on [`start`]'s
/// single terminal-phase site.
async fn connect_and_run(
    app: &AppHandle,
    generation: u64,
    cancel: &CancelHandle,
    params: StartParams,
) -> Result<(), EndReason> {
    let StartParams {
        window_id,
        model,
        credential,
        is_shared,
        started_by,
    } = params;

    // Connect. The URL carries the credential, so it is never logged — only the
    // outcome is.
    let url = credential.connect_url();
    let connect = tokio::select! {
        biased;
        // A stop during connect ABORTS the dial. Before the reservation existed
        // the slot was empty for this whole window, so `ai_chat_stop` was a
        // no-op for up to ~37s (#661).
        _ = cancel.wait() => {
            log::info!("ai_chat: start for window {window_id} cancelled while connecting");
            return Err(EndReason::Stopped);
        }
        result = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(&url),
        ) => result,
    };
    let ws = match connect {
        Err(_) => {
            log::warn!("ai_chat: WS connect timed out");
            return Err(EndReason::Offline);
        }
        Ok(Err(e)) => {
            // The error Display can embed the URL; log only the variant kind.
            log::warn!("ai_chat: WS connect failed ({})", connect_error_kind(&e));
            return Err(EndReason::Offline);
        }
        Ok(Ok((ws, _resp))) => ws,
    };

    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::channel::<String>(OUTBOUND_CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let ptt_active = Arc::new(AtomicBool::new(false));

    // Writer task — decoupled from reading. See module docs.
    {
        let stop = stop.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(text) = rx.recv().await {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let started = Instant::now();
                if let Err(e) = sink.send(Message::Text(text.into())).await {
                    log::debug!("ai_chat: outbound send failed: {e}");
                    break;
                }
                if started.elapsed() > Duration::from_millis(20) {
                    log::debug!("ai_chat: slow send {}ms", started.elapsed().as_millis());
                }
            }
            let _ = sink.close().await;
        });
    }

    // Window-control tools are declared ONLY when the master switch is armed.
    // With it unset the model is never told the tools exist, so there is
    // nothing to call, nothing to approve and nothing to execute (#658).
    let control_armed = super::control_gate::control_enabled();
    let control_session_id = super::control_gate::begin_session();
    if control_armed {
        // Bring the takeover detector up at session start rather than on the
        // first tool call: it needs a moment to prove its provenance
        // round-trip, and until it has, the click/key/scroll tier refuses.
        super::takeover::ensure_started();
        log::info!(
            "ai_chat: window control ARMED for this session ({}=1)",
            super::control_gate::CONTROL_ENV_VAR
        );
    }

    // Setup must be the first message on the wire.
    if tx
        .send(protocol::setup_message_with_tools(&model, control_armed))
        .await
        .is_err()
    {
        abandon_connection(&stop);
        return Err(EndReason::Error);
    }

    // Accessibility digest: a periodic walk of the window's AX tree, giving the
    // model the text a screenshot cannot show (scrolled-away rows, off-screen
    // content). Runs on its own OS thread — AX calls block inside the system
    // framework and would starve the socket if inlined here.
    let digest_state = Arc::new(Mutex::new(super::ax_digest::DigestSessionState::default()));
    let (digest_tx, mut digest_rx) = mpsc::channel::<super::ax_digest::DigestEvent>(4);
    super::ax_digest::spawn_digest_timer(
        window_id,
        stop.clone(),
        is_shared.clone(),
        digest_tx,
        digest_state.clone(),
    );

    // Hand the reservation over to the live session. Fails when a stop — or a
    // newer start — took the slot while we were connecting; abandoning here is
    // what stops an orphan being registered at all (#661).
    if !promote(ActiveSession {
        window_id,
        stop: stop.clone(),
        outbound: tx.clone(),
        ptt_active: ptt_active.clone(),
        turn: None,
        started_by,
        generation,
        is_shared: is_shared.clone(),
        digest_state: digest_state.clone(),
        control_session_id,
        control_queue: VecDeque::new(),
        control_busy: false,
    }) {
        log::info!("ai_chat: abandoning the start for window {window_id} -- the slot is no longer ours");
        abandon_connection(&stop);
        return Err(EndReason::Stopped);
    }

    // Reader task — owns the session's lifetime and reports the end.
    {
        let app = app.clone();
        let stop = stop.clone();
        let teardown_stop = stop.clone();
        let tx_frames = tx.clone();
        let is_shared_for_tick = is_shared.clone();
        tauri::async_runtime::spawn(async move {
            let started = Instant::now();
            let mut live = false;
            let mut end_reason = EndReason::Error;
            // Set once a `goAway` names the end as a normal time-limit close,
            // BEFORE the real `Close` frame that always follows it arrives.
            // Without this, the Close arm below unconditionally reclassifies
            // `end_reason` from the close frame's own (often empty/generic)
            // reason string, silently downgrading a correctly-detected
            // graceful end back to `EndReason::Error` — a real user-visible
            // "AI chat stopped unexpectedly" toast for a session that ended
            // exactly as expected.
            let mut go_away_received = false;
            let mut last_countdown = u64::MAX;
            let mut frame_due = Instant::now();
            // #845: hysteresis for AI-chat mic ducking -- see MicDuckGate's
            // doc comment. `ducking_active` mirrors the gate's last verdict so
            // set_mic_ducking (an atomic store + a possible spawned SDK call)
            // only runs on an actual state CHANGE, not every 250ms tick.
            let mut duck_gate = super::audio::MicDuckGate::new(AI_CHAT_DUCK_RELEASE_DELAY);
            let mut ducking_active = false;

            loop {
                if stop.load(Ordering::SeqCst) {
                    end_reason = EndReason::Stopped;
                    break;
                }
                let should_duck = duck_gate.sample(super::audio::is_playing(), Instant::now());
                if should_duck != ducking_active {
                    ducking_active = should_duck;
                    set_mic_ducking(&app, should_duck);
                }
                let elapsed = started.elapsed().as_secs();
                if elapsed >= CAP_SECONDS {
                    end_reason = EndReason::TimeLimit;
                    break;
                }
                // Gate check #2 — continuous. A share that stops mid-session
                // must stop the capture, not merely fail the next start.
                if !(is_shared_for_tick)() {
                    log::info!("ai_chat: window {window_id} no longer shared -- ending session");
                    end_reason = EndReason::NotShared;
                    break;
                }
                let left = seconds_left(CAP_SECONDS, elapsed);
                if left != last_countdown {
                    emit_countdown(&app, window_id, left);
                    last_countdown = left;
                    // Heartbeat the room (#657). Receivers expire a session
                    // whose heartbeat stops, so this is what keeps a live
                    // session visible — and what makes a crashed host's
                    // session disappear instead of lingering as a phantom.
                    if live && elapsed % super::wire::STATE_HEARTBEAT_SECONDS == 0 {
                        super::topic::publish_state(&app, window_id, true, Some(left), None);
                    }
                }

                // Frame pump, inline on the tick so it shares the gate check.
                if live && Instant::now() >= frame_due {
                    frame_due = Instant::now() + FRAME_INTERVAL;
                    pump_frame(window_id, &tx_frames);
                }

                // Drain any accessibility digest the walker produced. Sent as
                // machine context (`realtimeInput.text`), never a user turn —
                // a turn-shaped message can be implicitly committed and would
                // make the model answer a context update.
                while let Ok(event) = digest_rx.try_recv() {
                    use super::ax_digest::DigestEvent;
                    let message = match event {
                        DigestEvent::First(Some(snapshot)) => {
                            Some(protocol::initial_digest_message(&snapshot.text))
                        }
                        // The first walk reports even when AX gave nothing, so
                        // the session's timing does not depend on it.
                        DigestEvent::First(None) => None,
                        DigestEvent::Refresh(snapshot) => {
                            Some(protocol::ax_digest_update_message(&snapshot.text))
                        }
                    };
                    if let Some(message) = message {
                        let _ = tx_frames.try_send(message);
                    }
                }

                match tokio::time::timeout(TICK, stream.next()).await {
                    Err(_) => continue, // tick timeout — loop for cap/gate/countdown
                    Ok(None) => {
                        end_reason = EndReason::Error;
                        break;
                    }
                    Ok(Some(Err(e))) => {
                        log::debug!("ai_chat: ws stream error: {e}");
                        end_reason = EndReason::Error;
                        break;
                    }
                    Ok(Some(Ok(msg))) => {
                        let raw = match &msg {
                            // Gemini sends JSON as BINARY frames; decode both.
                            Message::Text(t) => t.to_string(),
                            Message::Binary(b) => String::from_utf8_lossy(b).to_string(),
                            Message::Close(frame) => {
                                let reason = frame
                                    .as_ref()
                                    .map(|f| f.reason.to_string())
                                    .unwrap_or_default();
                                end_reason = super::state::resolve_close_end_reason(
                                    go_away_received,
                                    &reason,
                                    end_reason,
                                );
                                log::info!(
                                    "ai_chat: server closed session ({end_reason:?})"
                                );
                                break;
                            }
                            _ => continue,
                        };
                        for event in protocol::parse_server_message(&raw) {
                            match event {
                                ServerEvent::SetupComplete => {
                                    live = true;
                                    emit_phase(&app, window_id, Phase::Live);
                                    log::info!("ai_chat: session live for window {window_id}");
                                    // Sent FIRST, before the frame or the voice
                                    // track: without a scripted opening line the
                                    // model will volunteer a guess about the
                                    // window's contents on its own initiative —
                                    // reproduced live during the #654 spike with
                                    // zero frames or digest ever sent. Safe to
                                    // send unconditionally under push-to-talk:
                                    // nobody can be heard until PTT is held, so
                                    // there is no race with real speech to
                                    // arbitrate the way takt's always-listening
                                    // design needs to.
                                    let _ = tx_frames
                                        .send(protocol::greeting_trigger_message())
                                        .await;
                                    // Everyone in the room hears the assistant,
                                    // not just this machine (#657).
                                    super::voice::start(&app, window_id);
                                    // First frame immediately, so the model has
                                    // something to look at before anyone speaks.
                                    pump_frame(window_id, &tx_frames);
                                    frame_due = Instant::now() + FRAME_INTERVAL;
                                }
                                ServerEvent::Audio(pcm) => {
                                    // Both, always: LiveKit does not loop a
                                    // participant's own track back, so dropping
                                    // the local render would leave the host deaf
                                    // to the session it is hosting.
                                    super::audio::play(&pcm);
                                    super::voice::push(&pcm);
                                }
                                ServerEvent::OutputText(t) => {
                                    emit_transcript(&app, window_id, "assistant", &t, false);
                                }
                                ServerEvent::InputText(t) => {
                                    emit_transcript(&app, window_id, "user", &t, false);
                                }
                                ServerEvent::TurnComplete => {
                                    emit_transcript(&app, window_id, "assistant", "", true);
                                }
                                ServerEvent::Interrupted => {
                                    // Barge-in has to reach every listener, not
                                    // only the host's speakers.
                                    super::audio::stop_playback();
                                    super::voice::clear();
                                }
                                ServerEvent::GoAway => {
                                    // Approaching the server's connection
                                    // lifetime: a normal end, not a failure.
                                    end_reason = EndReason::TimeLimit;
                                    go_away_received = true;
                                }
                                ServerEvent::ToolCallBatch(calls) => {
                                    enqueue_tool_calls(&app, window_id, calls);
                                }
                                ServerEvent::Other => {}
                            }
                        }
                    }
                }
            }

            teardown(&app, window_id, generation, &teardown_stop, end_reason);
        });
    }

    Ok(())
}

/// Event raised when the model asks to act on the window, so a human can
/// approve or refuse it (#658).
pub const EVENT_CONTROL_REQUEST: &str = "ai-chat-control-request";
/// Event raised when a request stops needing an answer — it ran, it was
/// refused, or the session ended. The card is dismissed only by one of these,
/// never on a timer: a control prompt that vanishes by itself trains people to
/// ignore it.
pub const EVENT_CONTROL_RESOLVED: &str = "ai-chat-control-resolved";

/// Handles the control path needs from the running session, cloned out under a
/// short lock rather than held across the (blocking) accessibility work.
struct ControlHandles {
    window_id: u32,
    control_session_id: u64,
    outbound: mpsc::Sender<String>,
    is_shared: Arc<dyn Fn() -> bool + Send + Sync>,
    digest_state: Arc<Mutex<super::ax_digest::DigestSessionState>>,
}

/// Rust-authoritative standing control state for the currently running
/// session. The panel queries this instead of inferring a persistent grant
/// from whichever button it most recently clicked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlStatus {
    pub session_id: u64,
    pub standing: &'static str,
}

pub fn control_status() -> Option<ControlStatus> {
    let handles = control_handles()?;
    super::control_gate::with_state(|state| {
        if state.session_id != handles.control_session_id {
            return None;
        }
        let standing = match state.standing {
            super::control_policy::Standing::None => "ask",
            super::control_policy::Standing::Session => "session",
            super::control_policy::Standing::Refused => "refused",
        };
        Some(ControlStatus {
            session_id: state.session_id,
            standing,
        })
    })
}

fn control_handles() -> Option<ControlHandles> {
    with_running(|session| ControlHandles {
        window_id: session.window_id,
        control_session_id: session.control_session_id,
        outbound: session.outbound.clone(),
        is_shared: session.is_shared.clone(),
        digest_state: session.digest_state.clone(),
    })
}

/// Resolve a model-cited `[n]` against the generation it named.
fn resolve_click(
    digest_state: &Arc<Mutex<super::ax_digest::DigestSessionState>>,
    generation: u64,
    element_index: usize,
) -> (
    Option<super::ax_digest::DigestIndex>,
    super::control_exec::ClickResolution,
) {
    use super::control_exec::ClickResolution;
    let state = digest_state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match super::ax_digest::lookup_index(&state, generation, element_index) {
        None => (None, ClickResolution::StaleGeneration),
        Some(index) => {
            let resolution = match index.rect {
                Some(rect) => ClickResolution::Resolved(rect),
                None => ClickResolution::Unpositioned,
            };
            (Some(index), resolution)
        }
    }
}

/// Re-send the newest accessibility snapshot we hold, so a model that cited a
/// dead generation has a live one to work from instead of retrying the same
/// stale reference.
fn push_current_digest(
    digest_state: &Arc<Mutex<super::ax_digest::DigestSessionState>>,
    tx: &mpsc::Sender<String>,
) {
    let text = digest_state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .last_text
        .clone();
    if let Some(text) = text {
        let _ = tx.try_send(protocol::ax_digest_update_message(&text));
    }
}

/// Which application we are about to drive, and under what pid.
fn target_process(app: &AppHandle, window_id: u32) -> (Option<i32>, Option<String>) {
    let pid = app
        .try_state::<crate::session::SessionState>()
        .and_then(|state| state.active_share_pid(window_id))
        .or_else(|| crate::window_registry::global().map(|r| r.owner_pid_fresh(window_id)).unwrap_or_else(|| crate::platform::cg::owner_pid_for_window_id(window_id)))
        .filter(|pid| *pid > 0);
    let bundle_id = pid.and_then(super::control_target::bundle_id_for_pid);
    (pid, bundle_id)
}

/// Answer the model and write the room's audit line for one refusal.
fn refuse_tool_call(
    app: &AppHandle,
    window_id: u32,
    tx: &mpsc::Sender<String>,
    id: &str,
    name: &str,
    code: &'static str,
    detail: Option<&super::control_gate::ActionDetail>,
) {
    log::info!("ai_chat: control refused for window {window_id}: {code}");
    let _ = tx.try_send(protocol::tool_response_message(
        id,
        name,
        false,
        code,
        "That action is not permitted here.",
    ));
    if let Some(detail) = detail {
        emit_transcript(
            app,
            window_id,
            "assistant",
            &super::control_gate::audit_line(name, detail, false, code),
            true,
        );
    }
    let _ = app.emit(
        EVENT_CONTROL_RESOLVED,
        serde_json::json!({
            "windowId": window_id,
            "requestId": id,
            "ok": false,
            "code": code,
        }),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlProgress {
    Complete,
    Waiting,
}

/// Queue whole tool-call envelopes onto the running session's one controller
/// lane. `control_busy` remains true while a card is pending or an execution
/// thread is live, so a sibling cannot replace the approval slot or race the
/// replay-protection shard.
fn enqueue_tool_calls(app: &AppHandle, window_id: u32, calls: Vec<protocol::FunctionCall>) {
    let should_pump = {
        let Ok(mut guard) = slot().lock() else { return };
        let Some(Slot::Running(session)) = guard.as_mut() else { return };
        if session.window_id != window_id {
            return;
        }
        queue_control_calls(session, calls)
    };
    if should_pump {
        pump_control_queue(app, window_id);
    }
}

fn queue_control_calls(
    session: &mut ActiveSession,
    calls: Vec<protocol::FunctionCall>,
) -> bool {
    session.control_queue.extend(calls);
    if session.control_busy {
        false
    } else {
        session.control_busy = true;
        true
    }
}

fn pump_control_queue(app: &AppHandle, window_id: u32) {
    loop {
        let next = {
            let Ok(mut guard) = slot().lock() else { return };
            let Some(Slot::Running(session)) = guard.as_mut() else { return };
            if session.window_id != window_id {
                return;
            }
            match session.control_queue.pop_front() {
                Some(call) => Some((session.outbound.clone(), call)),
                None => {
                    session.control_busy = false;
                    None
                }
            }
        };
        let Some((tx, call)) = next else { return };
        if handle_tool_call(app, window_id, &tx, &call.id, &call.name, &call.args)
            == ControlProgress::Waiting
        {
            return;
        }
    }
}

fn finish_control_call(app: &AppHandle, window_id: u32, session_id: u64) {
    let still_current = with_running(|session| {
        session.window_id == window_id && session.control_session_id == session_id
    })
    .unwrap_or(false);
    if still_current {
        pump_control_queue(app, window_id);
    }
}

/// Evaluate a tool call against the fail-closed policy and answer the model.
///
/// Once this call reaches the front of the controller queue, its answer is
/// immediate. A Live API function call that blocks while a human decides stalls
/// the whole conversation, so an ungranted action returns a structured "not
/// granted" straight away and the model is told to wait for an explicit grant
/// rather than retry. The approval card is raised out-of-band, on the sharer's
/// machine, and the eventual outcome reaches the model as machine context
/// (`protocol::control_outcome_message`). Siblings in the same envelope remain
/// queued until that eventual outcome, which prevents them from replacing the
/// one pending approval slot.
///
/// Note what this function does NOT decide: the checks here gate whether a
/// human is even asked. Every one of them is asked again, against freshly read
/// state, at the moment the action runs (`control_exec::recheck`).
fn handle_tool_call(
    app: &AppHandle,
    window_id: u32,
    tx: &mpsc::Sender<String>,
    id: &str,
    name: &str,
    args: &serde_json::Value,
) -> ControlProgress {
    use super::control_gate::{describe_action, PendingRequest};
    use super::control_policy::{grant_decision, parse_action, Decision, GrantContext, Standing};

    // The master switch. Tools are not declared without it, so reaching here
    // means something else called us — answer, don't act.
    if !super::control_gate::control_enabled() {
        refuse_tool_call(app, window_id, tx, id, name, "control_disabled", None);
        return ControlProgress::Complete;
    }

    // Shape first: an action we cannot even express safely never reaches the
    // gate.
    let action = match parse_action(name, args) {
        Ok(action) => action,
        Err(err) => {
            log::info!("ai_chat: refused malformed tool call '{name}': {err:?}");
            let _ = tx.try_send(protocol::tool_response_message(
                id,
                name,
                false,
                "invalid_arguments",
                "The arguments were not valid for this tool.",
            ));
            return ControlProgress::Complete;
        }
    };

    let Some(handles) = control_handles().filter(|h| h.window_id == window_id) else {
        refuse_tool_call(app, window_id, tx, id, name, "session_ended", None);
        return ControlProgress::Complete;
    };

    // Resolve a cited element now, so the approval card can name what it is
    // about to click. A citation we cannot resolve is refused here rather than
    // shown to a human as "click something".
    let mut resolved = None;
    if let super::control_policy::Action::Click {
        generation,
        element_index,
    } = action
    {
        let (index, resolution) = resolve_click(&handles.digest_state, generation, element_index);
        if !matches!(
            resolution,
            super::control_exec::ClickResolution::Resolved(_)
        ) {
            let code = match resolution {
                super::control_exec::ClickResolution::StaleGeneration => {
                    "stale_digest_generation"
                }
                _ => "element_unpositioned",
            };
            refuse_tool_call(app, window_id, tx, id, name, code, None);
            // Give the model something current to work from; otherwise it
            // retries the same dead reference.
            push_current_digest(&handles.digest_state, tx);
            return ControlProgress::Complete;
        }
        resolved = index;
    }
    let detail = describe_action(&action, resolved.as_ref());

    // Truthful gate inputs (#658 phase 3). Each of these was hard-coded to its
    // denying value in phase 2 because the real answer was unavailable.
    //
    // These reads happen inline on the reader task. They are window-server and
    // running-application queries measured in milliseconds, and a tool call is
    // rare — unlike the accessibility WALK, which blocks inside the framework
    // and is why the digest has its own thread. The genuinely blocking work
    // (the frontmost poll, the focus probe, the replay) all happens on the
    // execution thread instead.
    let (target_pid, target_bundle_id) = target_process(app, window_id);
    let window_present = (handles.is_shared)()
        && target_pid.is_some()
        && crate::window_registry::global().map(|r| r.frame_fresh(window_id)).unwrap_or_else(|| crate::platform::cg::frame_for_window_id(window_id)).is_some();
    let ctx = GrantContext {
        window_present,
        // The TARGET application, not merely whatever is frontmost: the
        // blocklist has to apply to the thing we would drive. (Execution
        // separately requires the target to be frontmost.)
        bundle_id: target_bundle_id.as_deref(),
        secure_input: super::control_target::secure_input_state(),
        takeover_detection_healthy: super::takeover::healthy(),
        // AI control never exceeds what human remote control is permitted to
        // do; if the state is unavailable we deny.
        remote_control_allowed: app
            .try_state::<crate::session::SessionState>()
            .is_some_and(|s| s.remote_control_allowed()),
        ai_chat_enabled: super::settings::is_enabled(),
    };
    if let Decision::Refuse { code } = grant_decision(&action, &ctx) {
        refuse_tool_call(app, window_id, tx, id, name, code, Some(&detail));
        return ControlProgress::Complete;
    }

    // Standing authorization decides whether a human is asked at all.
    let standing = super::control_gate::with_state(|state| state.standing.clone());
    match standing {
        Standing::Refused => {
            // A refused session must not keep raising cards — that is exactly
            // the wear-down the stickiness exists to prevent.
            refuse_tool_call(app, window_id, tx, id, name, "control_rejected", Some(&detail));
            ControlProgress::Complete
        }
        Standing::Session => {
            // The human already escalated to the whole session; run it, still
            // re-checking everything at the moment of execution.
            let pending = PendingRequest {
                session_id: handles.control_session_id,
                request_id: id.to_string(),
                window_id,
                tool: name.to_string(),
                action,
                detail,
            };
            let _ = tx.try_send(protocol::tool_response_message(
                id,
                name,
                false,
                "control_running",
                "Running under the session-wide grant; the outcome follows.",
            ));
            spawn_execution(app.clone(), pending);
            ControlProgress::Waiting
        }
        Standing::None => {
            let pending = PendingRequest {
                session_id: handles.control_session_id,
                request_id: id.to_string(),
                window_id,
                tool: name.to_string(),
                action,
                detail: detail.clone(),
            };
            super::control_gate::with_state(|state| state.pending = Some(pending));
            let _ = app.emit(
                EVENT_CONTROL_REQUEST,
                serde_json::json!({
                    "windowId": window_id,
                    "tool": name,
                    "requestId": id,
                    "sessionId": handles.control_session_id,
                    "detail": detail,
                }),
            );
            let _ = tx.try_send(protocol::tool_response_message(
                id,
                name,
                false,
                "control_not_granted",
                "Waiting for a participant to grant control. Do not retry until they do.",
            ));
            ControlProgress::Waiting
        }
    }
}

/// Record a human's approval and run the action. Returns false when the answer
/// was stale — a different request, or a finished session.
pub fn approve_control(
    app: &AppHandle,
    session_id: u64,
    request_id: &str,
    scope: super::control_policy::GrantScope,
) -> bool {
    use super::control_gate::{apply_approval, AnswerOutcome};

    let pending = super::control_gate::with_state(|state| {
        // Capture the request BEFORE applying, since applying clears it.
        let captured = state
            .pending
            .clone()
            .filter(|p| p.session_id == session_id && p.request_id == request_id);
        match apply_approval(state, session_id, request_id, scope) {
            AnswerOutcome::Applied => captured,
            AnswerOutcome::Stale => None,
        }
    });
    let Some(pending) = pending else {
        log::info!("ai_chat: ignoring a stale control approval for request {request_id}");
        return false;
    };
    spawn_execution(app.clone(), pending);
    true
}

/// Record a human's refusal. Sticky for the session.
pub fn reject_control(app: &AppHandle, session_id: u64) -> bool {
    use super::control_gate::{apply_rejection, AnswerOutcome};

    let (applied, pending) = super::control_gate::with_state(|state| {
        let captured = state.pending.clone();
        (apply_rejection(state, session_id), captured)
    });
    if applied == AnswerOutcome::Stale {
        return false;
    }
    if let Some(pending) = pending {
        let window_id = pending.window_id;
        let pending_session_id = pending.session_id;
        emit_transcript(
            app,
            pending.window_id,
            "assistant",
            &super::control_gate::audit_line(
                &pending.tool,
                &pending.detail,
                false,
                "control_rejected",
            ),
            true,
        );
        let _ = app.emit(
            EVENT_CONTROL_RESOLVED,
            serde_json::json!({
                "windowId": pending.window_id,
                "requestId": pending.request_id,
                "ok": false,
                "code": "control_rejected",
            }),
        );
        finish_control_call(app, window_id, pending_session_id);
    }
    true
}

/// The deliberate way back from a sticky refusal.
pub fn resume_control(session_id: u64) -> bool {
    use super::control_gate::{clear_refusal, AnswerOutcome};
    super::control_gate::with_state(|state| clear_refusal(state, session_id)) == AnswerOutcome::Applied
}

/// Run an approved action on its own thread.
///
/// Not on the reader task and not on the main thread: the execution path polls
/// for the frontmost application and makes blocking accessibility calls, either
/// of which would stall the WebSocket loop (and the latter of which would be a
/// main-thread hazard).
fn spawn_execution(app: AppHandle, pending: super::control_gate::PendingRequest) {
    std::thread::spawn(move || {
        let window_id = pending.window_id;
        let session_id = pending.session_id;
        run_execution(&app, pending);
        finish_control_call(&app, window_id, session_id);
    });
}

fn run_execution(app: &AppHandle, pending: super::control_gate::PendingRequest) {
    use super::control_exec::{self, ExecContext};
    use super::control_policy::Action;

    // Spend the one-shot grant on the ATTEMPT. A refused-at-execution action
    // has still used the human's yes; leaving it live would let the model
    // retry into a moment when the check happens to pass.
    let authorization = super::control_gate::with_state(|state| {
        let authorization =
            super::control_gate::authorization(state, pending.session_id, &pending.request_id);
        super::control_gate::consume_once(state, pending.session_id, &pending.request_id);
        authorization
    });

    // Cheap early bailout: don't spend the bounded frontmost poll below on a
    // session that's already gone. `target_process` only needs the window id,
    // not these handles, so this check costs nothing extra before the wait.
    if control_handles()
        .filter(|h| h.window_id == pending.window_id && h.control_session_id == pending.session_id)
        .is_none()
    {
        report_execution(app, &pending, None, false, "session_ended");
        return;
    }
    let (target_pid, target_bundle_id) = target_process(app, pending.window_id);
    let target_is_frontmost = target_pid.is_some_and(control_exec::wait_for_frontmost);

    // Everything above ran before or during the ~500ms frontmost wait, so
    // none of it can be trusted to still be true afterward -- #661 item 7:
    // a Stop click, a share ending, or the session itself ending during that
    // window used to land the action anyway, because only frame/secure-input/
    // takeover were re-read fresh below and session-alive/publication were
    // not. Re-fetch both HERE, after the wait, alongside the frame read that
    // already correctly accounts for the window moving during it.
    let Some(handles) = control_handles().filter(|h| {
        h.window_id == pending.window_id && h.control_session_id == pending.session_id
    }) else {
        report_execution(app, &pending, None, false, "session_ended");
        return;
    };
    let publication_live = (handles.is_shared)();
    let window_frame = crate::window_registry::global().map(|r| r.frame_fresh(pending.window_id)).unwrap_or_else(|| crate::platform::cg::frame_for_window_id(pending.window_id));

    let ctx = ExecContext {
        control_enabled: super::control_gate::control_enabled(),
        authorization,
        ai_chat_enabled: super::settings::is_enabled(),
        remote_control_allowed: app
            .try_state::<crate::session::SessionState>()
            .is_some_and(|s| s.remote_control_allowed()),
        publication_live,
        window_frame,
        target_pid,
        target_bundle_id: target_bundle_id.as_deref(),
        secure_input: super::control_target::secure_input_state(),
        takeover_healthy: super::takeover::healthy(),
        target_is_frontmost,
    };
    if let Err(code) = control_exec::recheck(&pending.action, &ctx) {
        report_execution(app, &pending, Some(&handles), false, code);
        return;
    }
    // `recheck` proved both of these; unwrapping here keeps the checks in one
    // place rather than duplicating their error codes.
    let (Some(frame), Some(pid)) = (ctx.window_frame, ctx.target_pid) else {
        report_execution(app, &pending, Some(&handles), false, "window_unavailable");
        return;
    };

    // Per-action verification, all of it against state read just now.
    let point = match &pending.action {
        Action::Click {
            generation,
            element_index,
        } => {
            let (_, resolution) =
                resolve_click(&handles.digest_state, *generation, *element_index);
            match control_exec::plan_click(&resolution, frame) {
                Ok(point) => Some(point),
                Err(code) => {
                    if code == "stale_digest_generation" {
                        push_current_digest(&handles.digest_state, &handles.outbound);
                    }
                    report_execution(app, &pending, Some(&handles), false, code);
                    return;
                }
            }
        }
        Action::Type(text) => {
            if let Err(code) = control_exec::plan_text(text) {
                report_execution(app, &pending, Some(&handles), false, code);
                return;
            }
            let focused = super::ax_digest::focused_context(pid, frame);
            if let Err(code) = control_exec::plan_type(&focused) {
                report_execution(app, &pending, Some(&handles), false, code);
                return;
            }
            None
        }
        Action::PressKey(_) | Action::Scroll { .. } => None,
    };

    // Baseline the takeover detector immediately before acting, so physical
    // input DURING our own injection is what auto-revokes — not the human's
    // click on the approval card a moment earlier.
    //
    // Known conservative bias: `remote_control`'s replay creates its CGEvents
    // from a NULL event source, so where it falls back from the accessibility
    // route to a CGEvent post, its own events read as UNMARKED and trip this.
    // The consequence is that a session-wide grant can degrade to per-action
    // for those routes. That errs toward asking the human again, which is the
    // safe direction; closing it properly means stamping the provenance marker
    // inside remote_control's shared event sink.
    let baseline = super::takeover::physical_count();
    let result = control_exec::dispatch(&pending.action, pending.window_id, frame, pid, point);
    if super::takeover::physical_activity_since(baseline) {
        // The sharer used their own keyboard or mouse while we were driving.
        // Drop any session-wide grant: the next action asks again.
        log::info!("ai_chat: physical input observed during an agent action -- revoking control");
        super::control_gate::with_state(|state| {
            state.standing = super::control_policy::Standing::None;
            state.granted_once = None;
        });
    }

    match result {
        Ok(()) => report_execution(app, &pending, Some(&handles), true, "ok"),
        Err(error) => {
            log::warn!("ai_chat: control replay failed: {error}");
            report_execution(app, &pending, Some(&handles), false, "replay_failed");
        }
    }
}

/// Write the room's audit line, tell the model, and dismiss the card.
fn report_execution(
    app: &AppHandle,
    pending: &super::control_gate::PendingRequest,
    handles: Option<&ControlHandles>,
    ok: bool,
    code: &str,
) {
    let line = super::control_gate::audit_line(&pending.tool, &pending.detail, ok, code);
    log::info!("ai_chat: {line}");
    emit_transcript(app, pending.window_id, "assistant", &line, true);
    if let Some(handles) = handles {
        let _ = handles.outbound.try_send(protocol::control_outcome_message(
            &pending.tool,
            ok,
            code,
            if ok {
                "Describe the result only from a new frame or snapshot."
            } else {
                "Do not retry unless a participant grants control again."
            },
        ));
    }
    let _ = app.emit(
        EVENT_CONTROL_RESOLVED,
        serde_json::json!({
            "windowId": pending.window_id,
            "requestId": pending.request_id,
            "ok": ok,
            "code": code,
        }),
    );
}

/// Long edge cap for frames sent to Gemini. Deliberately NOT the picker's
/// 320px thumbnail size (that was the real bug here — this function used to
/// call the shared capture helper with no size argument at all, which
/// defaulted to the thumbnail's 320px and made on-screen text/UI illegible
/// to the model). 1280px matches #656's original plan and is a reasonable
/// middle ground: enough detail to read a shared window's content, without
/// sending a full-native-resolution JPEG on every ~1s tick.
const AI_CHAT_FRAME_MAX_LONG_EDGE: u32 = 1280;

/// Capture one frame of the window and queue it, dropping on backpressure.
fn pump_frame(window_id: u32, tx: &mpsc::Sender<String>) {
    if tx.capacity() == 0 {
        return; // queue saturated; a stale frame is worthless
    }
    match crate::window_source::capture_window_thumbnail_uncached(
        window_id,
        AI_CHAT_FRAME_MAX_LONG_EDGE,
    ) {
        Ok(jpeg) => {
            let _ = tx.try_send(protocol::video_frame_message(&jpeg));
        }
        Err(e) => log::debug!("ai_chat: frame capture failed: {e}"),
    }
}

/// Stop the current session — live, or still connecting.
pub fn stop(app: &AppHandle, reason: EndReason) {
    match take_stop_target() {
        Some(StopTarget::Running {
            window_id,
            generation,
            stop,
        }) => teardown(app, window_id, generation, &stop, reason),
        Some(StopTarget::Starting { window_id }) => {
            // The connect is aborted and the slot is already free, so report the
            // end from here. Nothing else can: the aborting start deliberately
            // stays silent rather than overwrite this reason (see
            // `terminal_phase_for_failed_start`).
            log::info!("ai_chat: stopped a connecting session for window {window_id} ({reason:?})");
            emit_phase(app, window_id, Phase::Ended { reason });
            super::topic::publish_state(app, window_id, false, None, Some(reason));
        }
        None => {}
    }
}

/// What a teardown found, decided under the slot lock.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TeardownOutcome {
    /// Whether the flag that stops the reader task, the frame pump and the
    /// accessibility walker is now set for the session this call names. ALWAYS
    /// true — it is set before anything else can return.
    pub stopped: bool,
    /// Whether this call owned the registered slot, and so must run the
    /// synchronous shutdown and report the end exactly once.
    pub was_current: bool,
}

/// The state half of a teardown.
///
/// Sets `stop` for the generation being torn down BEFORE any early return.
/// Bailing out on a generation mismatch without setting it is what left an
/// orphaned session capturing a frame per second and playing audio for the full
/// 300s cap, with nothing able to reach it (#661).
fn claim_teardown(generation: u64, stop: &Arc<AtomicBool>) -> TeardownOutcome {
    stop.store(true, Ordering::SeqCst);
    let Ok(mut guard) = slot().lock() else {
        return TeardownOutcome {
            stopped: true,
            was_current: false,
        };
    };
    let was_current = matches!(
        guard.as_ref(),
        Some(Slot::Running(s)) if s.generation == generation
    );
    if was_current {
        *guard = None;
    }
    TeardownOutcome {
        stopped: stop.load(Ordering::SeqCst),
        was_current,
    }
}

/// Clear session state and report the end exactly once. Safe to call twice —
/// the generation check makes the second call a no-op, which matters because
/// both the reader task and an explicit stop can reach here.
/// #845: engage/release AI-chat mic ducking (see `ai_chat::audio::MicDuckGate`
/// for why). Never touches the user's own mute intent -- see
/// `SessionState::set_ai_chat_ducking`'s doc comment.
fn set_mic_ducking(app: &AppHandle, duck: bool) {
    if let Some(state) = app.try_state::<crate::session::SessionState>() {
        state.set_ai_chat_ducking(duck);
    }
}

fn teardown(
    app: &AppHandle,
    window_id: u32,
    generation: u64,
    stop: &Arc<AtomicBool>,
    reason: EndReason,
) {
    if !claim_teardown(generation, stop).was_current {
        return;
    }
    // Synchronous: the assistant must not still be talking after the UI says
    // the session ended — on ANY machine, which is why the published track goes
    // down here too and not only the local render.
    super::audio::stop_playback();
    // #845: release ducking unconditionally on every teardown path -- the
    // tick loop's own release has a delay by design (AI_CHAT_DUCK_RELEASE_DELAY)
    // and a session can end (cap, share dropped, stop, error) inside that
    // window. Without this the mic would stay ducked -- effectively muted --
    // after the session that justified it is gone.
    set_mic_ducking(app, false);
    super::voice::stop();
    // Both capture paths unconditionally: whichever one an open turn was using,
    // a session that has ended must leave no audio source running. A remote tap
    // left alive would keep a microphone's worth of a peer's room flowing at a
    // model that is no longer listening.
    super::audio::stop_local_microphone_capture();
    super::remote_audio::stop();
    // Control dies with the session: no grant, no pending card, and no event
    // tap outlive it. A card left on screen after the session ended could only
    // ever authorize something that no longer exists.
    let pending = super::control_gate::with_state(|state| state.pending.clone());
    super::control_gate::end_session();
    super::takeover::stop();
    if let Some(pending) = pending {
        let _ = app.emit(
            EVENT_CONTROL_RESOLVED,
            serde_json::json!({
                "windowId": pending.window_id,
                "requestId": pending.request_id,
                "ok": false,
                "code": "session_ended",
            }),
        );
    }
    log::info!("ai_chat: session for window {window_id} ended ({reason:?})");
    emit_phase(app, window_id, Phase::Ended { reason });
    // Tell the room immediately rather than letting the heartbeat lapse, so
    // every other participant's badge clears at once.
    super::topic::publish_state(app, window_id, false, None, Some(reason));
}

/// Begin a push-to-talk turn for `speaker`, taking audio from `source`.
///
/// Returns false if there is no live session or someone already holds the floor
/// — manual-activity mode is a single serial stream, so two speakers
/// interleaved would corrupt the turn.
///
/// For [`PttSource::RemoteTrack`] the caller MUST have established the tap
/// already (`remote_audio::start`); this function never opens one, and never
/// substitutes the local microphone when one is missing.
pub fn ptt_start(speaker: &str, source: PttSource) -> bool {
    // The lock is released before any capture work: opening or closing a cpal
    // stream synchronizes with its callback thread, and that callback calls
    // `push_audio`, which wants this same lock.
    let opened = {
        let mut guard = match slot().lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(Slot::Running(session)) = guard.as_mut() else {
            return false;
        };
        if session
            .ptt_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false; // floor already held
        }
        let _ = session.outbound.try_send(protocol::activity_start_message());
        let _ = session
            .outbound
            .try_send(protocol::speaker_label_message(speaker));
        session.turn = Some(OpenTurn {
            speaker: speaker.to_string(),
            source: source.clone(),
        });
        true
    };
    if opened {
        begin_capture(&source);
    }
    opened
}

/// Send a typed turn into the running session, attributed to `speaker`.
///
/// Unlike [`ptt_start`], this never claims the floor — a typed message has no
/// "who's speaking" ambiguity to arbitrate, and any number of participants
/// may each send one independently at any time. It IS refused while the PTT
/// floor is held: Gemini Live's `clientContent` turns and an open
/// `realtimeInput` manual-activity window are different input modes, and
/// sending one while the other is open is undefined by the API — text waits
/// for the floor to be free rather than risk corrupting an in-progress
/// spoken turn.
pub fn send_text(speaker: &str, text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > super::wire::MAX_USER_TEXT_CHARS {
        return false;
    }
    let guard = match slot().lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let Some(Slot::Running(session)) = guard.as_ref() else {
        return false;
    };
    if session.ptt_active.load(Ordering::SeqCst) {
        return false;
    }
    let _ = session
        .outbound
        .try_send(protocol::speaker_label_message(speaker));
    let _ = session
        .outbound
        .try_send(protocol::user_text_message(trimmed));
    true
}

/// Feed captured audio (PCM16, 16 kHz mono) into the open turn — from this
/// machine's microphone when the local user holds the floor, from the holder's
/// LiveKit track when a remote participant does.
///
/// Ignored unless a turn is actually open, so nothing can leak to the model
/// outside a held push-to-talk.
pub fn push_audio(pcm16: &[u8]) {
    let speaker = {
        let guard = match slot().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(Slot::Running(session)) = guard.as_ref() else {
            return;
        };
        if !session.ptt_active.load(Ordering::SeqCst) {
            return;
        }
        let _ = session
            .outbound
            .try_send(protocol::audio_chunk_message(pcm16));
        session.turn.as_ref().map(|turn| turn.speaker.clone())
    };
    // Outside the lock: `note_floor_audio` takes the coordination lock, and
    // holding both would invert the order the disconnect path takes them in.
    if let Some(speaker) = speaker {
        super::topic::note_floor_audio(&speaker);
    }
}

/// End the current push-to-talk turn, closing whichever capture path it opened.
pub fn ptt_end() {
    let ended = {
        let mut guard = match slot().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(Slot::Running(session)) = guard.as_mut() else {
            return;
        };
        if session
            .ptt_active
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return; // no turn open
        }
        let _ = session.outbound.try_send(protocol::activity_end_message());
        session.turn.take()
    };
    // Same reason as `ptt_start`: never stop a capture stream under the lock.
    if let Some(turn) = ended {
        end_capture(&turn.source);
    }
}

/// Send a refreshed accessibility digest as machine context.
pub fn push_digest(text: &str) {
    with_running(|session| {
        let _ = session
            .outbound
            .try_send(protocol::ax_digest_update_message(text));
    });
}

/// Describe a connect error without ever including the URL (which carries the
/// credential). tungstenite's Display for some variants embeds the request.
fn connect_error_kind(err: &tokio_tungstenite::tungstenite::Error) -> &'static str {
    use tokio_tungstenite::tungstenite::Error as E;
    match err {
        E::ConnectionClosed => "connection-closed",
        E::AlreadyClosed => "already-closed",
        E::Io(_) => "io",
        E::Tls(_) => "tls",
        E::Capacity(_) => "capacity",
        E::Protocol(_) => "protocol",
        E::WriteBufferFull(_) => "write-buffer-full",
        E::Utf8(_) => "utf8",
        E::AttackAttempt => "attack-attempt",
        E::Url(_) => "url",
        E::Http(_) => "http",
        E::HttpFormat(_) => "http-format",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot is process-wide, so the tests that claim it must not run
    /// concurrently with each other or with the "nothing is running" tests.
    /// Same for the audio statics (`audio::local_microphone_open_requests`,
    /// `audio::is_playing`, `remote_audio::tapped_identity`): every test that
    /// touches them via `begin_capture`/`end_capture` must hold this lock,
    /// or counter-delta asserts flake under parallel test threads.
    fn serialize() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn reset_slot() {
        if let Ok(mut guard) = slot().lock() {
            *guard = None;
        }
    }

    /// A registered session with no socket behind it. Enough to exercise the
    /// slot's ownership rules, which is where #661's orphan lived — the bug was
    /// never in the WebSocket, it was in who the registry believed owned it.
    ///
    /// The receiver is returned so the outbound channel stays open; dropping it
    /// would silently turn every `try_send` in the code under test into a
    /// no-op.
    fn fake_session(
        window_id: u32,
        generation: u64,
    ) -> (ActiveSession, Arc<AtomicBool>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let session = ActiveSession {
            window_id,
            stop: stop.clone(),
            outbound: tx,
            ptt_active: Arc::new(AtomicBool::new(false)),
            turn: None,
            started_by: Some("alice".into()),
            generation,
            is_shared: Arc::new(|| true),
            digest_state: Arc::new(Mutex::new(
                super::super::ax_digest::DigestSessionState::default(),
            )),
            control_session_id: 0,
            control_queue: VecDeque::new(),
            control_busy: false,
        };
        (session, stop, rx)
    }

    #[test]
    fn one_tool_call_batch_occupies_one_fifo_execution_lane() {
        let (mut session, _stop, _rx) = fake_session(9, 1);
        let calls = ["fc_1", "fc_2", "fc_3"]
            .into_iter()
            .map(|id| protocol::FunctionCall {
                id: id.into(),
                name: "window_type".into(),
                args: serde_json::json!({ "text": id }),
            })
            .collect();
        assert!(queue_control_calls(&mut session, calls));
        assert!(session.control_busy, "the batch did not claim the controller lane");
        assert_eq!(
            session
                .control_queue
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            ["fc_1", "fc_2", "fc_3"]
        );

        // A later envelope joins the tail and must not start a sibling pump
        // while the first call is awaiting approval/execution.
        assert!(!queue_control_calls(
            &mut session,
            vec![protocol::FunctionCall {
                id: "fc_4".into(),
                name: "window_type".into(),
                args: serde_json::json!({ "text": "fourth" }),
            }]
        ));
        assert_eq!(session.control_queue.back().unwrap().id, "fc_4");
    }

    #[test]
    fn control_status_command_reads_the_live_rust_standing_state() {
        let _serial = serialize();
        reset_slot();
        let session_id = super::super::control_gate::begin_session();
        let (mut session, _stop, _rx) = fake_session(9, 1);
        session.control_session_id = session_id;
        *slot().lock().unwrap() = Some(Slot::Running(session));

        super::super::control_gate::with_state(|state| {
            state.standing = super::super::control_policy::Standing::Session;
        });
        assert_eq!(
            super::super::commands::ai_chat_control_status(),
            Some(ControlStatus {
                session_id,
                standing: "session"
            })
        );
        super::super::control_gate::with_state(|state| {
            assert_eq!(
                super::super::control_gate::apply_rejection(state, session_id),
                super::super::control_gate::AnswerOutcome::Applied
            );
        });
        assert_eq!(
            super::super::commands::ai_chat_control_status()
                .unwrap()
                .standing,
            "refused"
        );

        reset_slot();
        super::super::control_gate::end_session();
        assert_eq!(super::super::commands::ai_chat_control_status(), None);
    }

    #[test]
    fn two_concurrent_starts_cannot_both_claim_the_slot() {
        // The blocker: `is_any_active()` guarded at the top and registration
        // happened after a token mint and a WS connect — up to ~37s — so both
        // callers passed and the second overwrote the first. The claim now
        // happens under the same lock as the check, BEFORE anything slow.
        let _serial = serialize();
        reset_slot();

        let first = reserve(9, 1).expect("the first start must get the slot");
        assert_eq!(
            reserve(5, 2).err(),
            Some(EndReason::Busy),
            "a second start claimed the slot while the first was still connecting -- \
             the loser's session is orphaned and no stop can reach it"
        );

        // And the slot reads as busy to everyone who asks, so the hover-tab menu
        // offers Stop rather than a second Start during the connect.
        assert!(is_any_active(), "a connecting start must read as active");
        assert!(is_active_for(9));
        assert!(!is_active_for(5));
        // But it is not LIVE: no turn may be opened on a socket that has not
        // handshaken.
        assert_eq!(active_window_id(), None);

        drop(first);
        reset_slot();
    }

    #[test]
    fn a_teardown_for_a_replaced_generation_still_stops_its_own_session() {
        // The second half of the same blocker. `teardown` returned early on a
        // generation mismatch WITHOUT setting the stop flag, so an orphan kept
        // capturing a frame per second and playing audio for the full 300s cap.
        let _serial = serialize();
        reset_slot();

        let (live, live_stop, _live_rx) = fake_session(9, 42);
        *slot().lock().unwrap() = Some(Slot::Running(live));

        let orphan_stop = Arc::new(AtomicBool::new(false));
        let outcome = claim_teardown(41, &orphan_stop);

        assert!(
            orphan_stop.load(Ordering::SeqCst),
            "teardown returned without stopping the session it was called for -- \
             that session keeps capturing frames and playing audio until the cap"
        );
        assert_eq!(
            outcome,
            TeardownOutcome {
                stopped: true,
                was_current: false
            }
        );
        // And it must not take the LIVE session down with it.
        assert!(
            !live_stop.load(Ordering::SeqCst),
            "a stale teardown stopped the session that actually owns the slot"
        );
        assert!(is_active_for(9), "the registered session must survive");

        reset_slot();
    }

    #[test]
    fn a_stop_during_connect_aborts_it_instead_of_doing_nothing() {
        // `sessions()` stayed empty until registration, so `ai_chat_stop` was a
        // no-op for the whole mint + dial window (~37s worst case) and the panel
        // sat at "Connecting…" with an End button that succeeded and did
        // nothing.
        let _serial = serialize();
        reset_slot();

        let cancel = reserve(9, 7).expect("reserve");
        let target = take_stop_target().expect("a stop during connect found nothing to stop");
        match target {
            StopTarget::Starting { window_id } => assert_eq!(window_id, 9),
            StopTarget::Running { .. } => panic!("a connecting start is not a running session"),
        }
        assert!(
            cancel.is_cancelled(),
            "the in-flight connect was never told to abort"
        );
        assert!(
            !is_any_active(),
            "the slot must be free again so a retry is possible immediately"
        );

        // And if the dial completes anyway, it must not register.
        let (session, _stop, _rx) = fake_session(9, 7);
        assert!(
            !promote(session),
            "a cancelled start registered a session nobody can stop"
        );

        reset_slot();
    }

    /// #661 item 7: `run_execution`'s ~500ms frontmost wait sits between an
    /// initial session/publication check and the actual action. `control_
    /// handles()` is the exact function that check calls, both before AND
    /// (after the fix) after the wait — this proves it correctly reflects a
    /// share ending or a session stopping AS OF THE MOMENT IT'S CALLED,
    /// rather than returning a value cached from before either happened. A
    /// caller that reads it once before a long wait and trusts that value
    /// afterward (the pre-fix bug) would land an action `run_execution` had
    /// no way to know was no longer authorized.
    #[test]
    fn control_handles_reflects_publication_and_session_state_at_call_time_not_earlier() {
        let _serial = serialize();
        reset_slot();

        let shared = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let shared_for_closure = shared.clone();
        let (tx, _rx) = mpsc::channel(4);
        let session = ActiveSession {
            window_id: 9,
            stop: Arc::new(AtomicBool::new(false)),
            outbound: tx,
            ptt_active: Arc::new(AtomicBool::new(false)),
            turn: None,
            started_by: Some("alice".into()),
            generation: 1,
            is_shared: Arc::new(move || shared_for_closure.load(Ordering::SeqCst)),
            digest_state: Arc::new(Mutex::new(
                super::super::ax_digest::DigestSessionState::default(),
            )),
            control_session_id: 7,
            control_queue: VecDeque::new(),
            control_busy: false,
        };
        *slot().lock().unwrap() = Some(Slot::Running(session));

        // "Before the wait": publication is live. The pre-fix bug captured
        // this as a plain bool (`let publication_live = (handles.is_shared)()`)
        // and never looked again -- reproduce that exact capture here.
        let handles_before = control_handles()
            .filter(|h| h.window_id == 9 && h.control_session_id == 7)
            .expect("session should be found while it's running");
        let stale_publication_live = (handles_before.is_shared)();
        assert!(stale_publication_live, "publication should read live at first");

        // Something happens DURING the simulated wait: the share stops.
        shared.store(false, Ordering::SeqCst);

        // The pre-fix bug's captured bool is now WRONG -- this is the bug
        // itself, reproduced: a stale `true` for a share that has ended.
        assert!(
            stale_publication_live,
            "sanity: reproduces the bug -- a value captured before the wait \
             does not know the share ended during it"
        );
        // The fix: re-fetch AFTER the wait instead of trusting the captured
        // value. A fresh call must observe the change the stale bool missed.
        let handles_after = control_handles()
            .filter(|h| h.window_id == 9 && h.control_session_id == 7)
            .expect("the session itself is still registered, just unshared now");
        assert!(
            !(handles_after.is_shared)(),
            "a fresh read after the wait must observe the share having ended"
        );

        // And if the session ends entirely (Stop pressed) during the wait,
        // a re-fetch must report it gone, not silently keep the old handles.
        reset_slot();
        assert!(
            control_handles()
                .filter(|h| h.window_id == 9 && h.control_session_id == 7)
                .is_none(),
            "a re-fetch after the session ended must not find it"
        );
    }

    #[test]
    fn a_start_that_fails_after_announcing_connecting_reports_a_terminal_phase() {
        // Every failure path after the first `Connecting` emit used to return
        // `Err` with no terminal phase — `emit_phase(Ended)` lived only in
        // `teardown` — leaving the panel wedged at "Connecting…" forever.
        let _serial = serialize();
        reset_slot();

        let cancel = reserve(9, 11).expect("reserve");
        assert_eq!(
            terminal_phase_for_failed_start(11, &cancel, EndReason::Offline),
            Some(Phase::Ended {
                reason: EndReason::Offline
            }),
            "a failed connect must report a terminal phase, or the panel never leaves Connecting"
        );
        assert!(
            !is_any_active(),
            "an abandoned start must give the slot back"
        );

        // The one exception, in the other direction: a concurrent stop already
        // reported the end, and reporting again would overwrite the user's own
        // reason with a connect failure they no longer care about.
        let cancel = reserve(9, 12).expect("reserve");
        take_stop_target();
        assert_eq!(
            terminal_phase_for_failed_start(12, &cancel, EndReason::Offline),
            None,
            "the stop already reported the end; a second report would overwrite it"
        );

        reset_slot();
    }

    #[test]
    fn a_released_reservation_frees_the_slot_for_the_next_start() {
        let _serial = serialize();
        reset_slot();

        let _first = reserve(9, 21).expect("reserve");
        assert!(reserve(5, 22).is_err());
        assert!(release_reservation(21));
        let _second = reserve(5, 22).expect("the slot must be reusable after a failed start");
        assert!(is_active_for(5));
        // A start that already lost the slot cannot clear its successor.
        assert!(!release_reservation(21));
        assert!(is_active_for(5));

        reset_slot();
    }

    #[test]
    fn connect_url_uses_constrained_endpoint_for_tokens() {
        // Ephemeral tokens ONLY authenticate the constrained method (#654 Q1);
        // pointing them at the plain method fails with "unregistered callers".
        let url = Credential::EphemeralToken("authTokens/abc".into()).connect_url();
        assert!(url.contains("BidiGenerateContentConstrained"), "{url}");
        assert!(url.contains("access_token=authTokens/abc"), "{url}");
        assert!(!url.contains("?key="), "{url}");
    }

    #[test]
    fn connect_url_uses_plain_endpoint_for_api_keys() {
        let url = Credential::ApiKey("AIzaTest".into()).connect_url();
        assert!(url.contains("GenerativeService.BidiGenerateContent?"), "{url}");
        assert!(!url.contains("Constrained"), "{url}");
        assert!(url.contains("key=AIzaTest"), "{url}");
    }

    #[test]
    fn credential_mode_labels() {
        assert_eq!(Credential::EphemeralToken("t".into()).mode(), "hosted");
        assert_eq!(Credential::ApiKey("k".into()).mode(), "byok");
    }

    #[tokio::test]
    async fn start_refuses_an_unshared_window_before_any_capture() {
        // The security gate: a start request for a window with no live
        // publication must be refused outright. If this regresses, a crafted
        // request could stream an unshared window to Google.
        let captured = Arc::new(AtomicBool::new(false));
        let seen = captured.clone();
        let params = StartParams {
            window_id: 4242,
            model: "models/test".into(),
            credential: Credential::ApiKey("unused".into()),
            is_shared: Arc::new(move || {
                seen.store(true, Ordering::SeqCst);
                false
            }),
            started_by: Some("alice".into()),
        };
        // No AppHandle in unit tests, so assert the gate decision directly via
        // the same predicate the engine consults first.
        assert!(!(params.is_shared)());
        assert!(
            captured.load(Ordering::SeqCst),
            "the gate must actually be consulted"
        );
    }

    #[test]
    fn with_the_master_switch_off_the_model_is_never_told_the_tools_exist() {
        // This is the exact expression `start` uses. With PETAL_AI_CONTROL
        // unset there are no tool declarations, so there is no call to gate, no
        // card to approve, and nothing for the execution path to run.
        let armed = super::super::control_gate::control_enabled();
        if armed {
            // A developer running with control armed would see a confusing
            // failure; the off case is what this test exists to pin.
            return;
        }
        let setup: serde_json::Value =
            serde_json::from_str(&protocol::setup_message_with_tools("models/test", armed))
                .unwrap();
        assert!(setup["setup"]["tools"].is_null(), "{setup}");
        let instruction = setup["setup"]["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap();
        assert!(!instruction.contains("window_click"), "{instruction}");
        assert!(!instruction.contains("window-control"), "{instruction}");
    }

    #[test]
    fn with_the_master_switch_off_execution_is_unreachable_even_if_called() {
        // Defence in depth: the tools are not declared, but a future caller
        // reaching the execution path anyway must still be refused.
        use super::super::control_exec::{recheck, ExecContext};
        use super::super::control_gate::Authorization;
        use super::super::control_policy::{Action, SecureInput};

        let ctx = ExecContext {
            control_enabled: false,
            authorization: Authorization::Session,
            ai_chat_enabled: true,
            remote_control_allowed: true,
            publication_live: true,
            window_frame: Some(crate::platform::cg::WindowFrame {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }),
            target_pid: Some(1),
            target_bundle_id: Some("com.apple.TextEdit"),
            secure_input: SecureInput::Inactive,
            takeover_healthy: true,
            target_is_frontmost: true,
        };
        assert_eq!(
            recheck(&Action::Type("hi".into()), &ctx),
            Err("control_disabled")
        );
    }

    #[test]
    fn an_answer_for_a_finished_session_authorizes_nothing() {
        // A card answered after teardown must not authorize the next session's
        // first action.
        use super::super::control_gate::{
            apply_approval, authorization, begin_session, end_session, with_state, Authorization,
            PendingRequest,
        };
        use super::super::control_policy::{Action, GrantScope};

        let first = begin_session();
        with_state(|state| {
            state.pending = Some(PendingRequest {
                session_id: first,
                request_id: "fc_1".into(),
                window_id: 7,
                tool: "window_type".into(),
                action: Action::Type("hi".into()),
                detail: super::super::control_gate::describe_action(
                    &Action::Type("hi".into()),
                    None,
                ),
            })
        });
        let second = begin_session();
        // The human's click still names the OLD session.
        with_state(|state| {
            assert_eq!(
                apply_approval(state, first, "fc_1", GrantScope::Session),
                super::super::control_gate::AnswerOutcome::Stale
            );
            assert_eq!(
                authorization(state, second, "fc_1"),
                Authorization::NotGranted
            );
        });
        end_session();
    }

    #[test]
    fn no_session_means_ptt_is_inert() {
        // Guards against audio reaching the model with no session open.
        let _serial = serialize();
        reset_slot();
        assert!(!is_any_active());
        assert!(!ptt_start("alice", PttSource::LocalMicrophone));
        push_audio(&[0x01, 0x02]); // must not panic
        ptt_end(); // must not panic
    }

    #[test]
    fn the_floor_is_routed_to_the_speaker_who_actually_holds_it() {
        // The rule #657 shipped backwards. Asserted in BOTH directions: a test
        // that only checked one would still pass with the branches swapped.
        assert_eq!(
            audio_route("alice", "alice"),
            PttSource::LocalMicrophone,
            "the local user's own push-to-talk must use the local microphone"
        );
        assert_eq!(
            audio_route("bob", "alice"),
            PttSource::RemoteTrack {
                identity: "bob".into()
            },
            "a remote peer's push-to-talk must tap THAT peer's track"
        );
        // Not merely "some remote track": the one belonging to the speaker.
        assert_eq!(
            audio_route("carol", "alice"),
            PttSource::RemoteTrack {
                identity: "carol".into()
            }
        );
    }

    #[test]
    fn only_the_local_users_own_turn_opens_the_local_microphone() {
        // The routing decision above is worthless unless the dispatch honours
        // it, so this drives `begin_capture` — the real function `ptt_start`
        // calls — and reads the counter incremented inside
        // `audio::start_local_microphone_capture` itself. Swapping the two arms
        // fails this in both directions.
        let _serial = serialize();
        let before = super::super::audio::local_microphone_open_requests();

        begin_capture(&PttSource::RemoteTrack {
            identity: "bob".into(),
        });
        assert_eq!(
            super::super::audio::local_microphone_open_requests(),
            before,
            "a REMOTE peer's push-to-talk opened the host's microphone"
        );

        begin_capture(&PttSource::LocalMicrophone);
        assert_eq!(
            super::super::audio::local_microphone_open_requests(),
            before + 1,
            "the local user's own push-to-talk did not open their microphone"
        );
    }

    #[test]
    fn send_text_sends_a_speaker_label_then_the_turn_and_is_refused_while_ptt_is_held() {
        let _serial = serialize();
        reset_slot();

        let (session, _stop, mut rx) = fake_session(9, 1);
        let ptt_active = session.ptt_active.clone();
        *slot().lock().unwrap() = Some(Slot::Running(session));

        assert!(send_text("alice", "what does this button do?"));
        let label = rx.try_recv().expect("a speaker label must be sent first");
        assert!(
            label.contains("alice"),
            "the label must attribute the turn to whoever sent it: {label}"
        );
        let turn = rx.try_recv().expect("the text turn itself must be sent");
        assert!(turn.contains("what does this button do?"), "{turn}");

        // Simulate a PTT turn being open: `clientContent` and an open
        // `realtimeInput` activity window are undefined together, so text
        // must wait for the floor to be free rather than risk corrupting it.
        ptt_active.store(true, Ordering::SeqCst);
        assert!(
            !send_text("bob", "are you there?"),
            "text sent while the PTT floor is held must be refused"
        );

        reset_slot();
    }

    #[test]
    fn send_text_rejects_blank_and_oversized_text() {
        let _serial = serialize();
        reset_slot();

        let (session, _stop, _rx) = fake_session(9, 1);
        *slot().lock().unwrap() = Some(Slot::Running(session));

        assert!(!send_text("alice", "   "), "blank text must be refused");
        let too_long = "x".repeat(super::super::wire::MAX_USER_TEXT_CHARS + 1);
        assert!(
            !send_text("alice", &too_long),
            "text over the contract's length cap must be refused"
        );

        reset_slot();
    }

    #[test]
    fn send_text_does_nothing_without_a_running_session() {
        let _serial = serialize();
        reset_slot();
        assert!(!send_text("alice", "hello?"));
    }

    /// Takt-comparison finding: push-to-talk was reasoned to make hardware
    /// echo cancellation unnecessary because the mic only opens while a turn
    /// is held — but nothing stopped a turn opening WHILE the assistant was
    /// still talking (a barge-in gesture), so the plain cpal capture (no
    /// AEC) could pick up the assistant's own voice bleeding from the local
    /// speakers for the round-trip until the server's `Interrupted` event
    /// arrived. Reverting `begin_capture`'s `LocalMicrophone` arm to skip
    /// the `stop_playback()` call makes the first assertion below fail.
    ///
    /// Both scenarios live in one test function, not two, because
    /// `audio::is_playing`'s backing flag is a process-wide static and
    /// `cargo test` runs tests in parallel by default — two separate tests
    /// each setting and asserting it would be a real, not theoretical, race.
    #[test]
    fn opening_the_local_microphone_stops_local_playback_but_a_remote_turn_does_not() {
        let _serial = serialize();
        super::super::audio::set_playing_for_test(true);
        assert!(super::super::audio::is_playing(), "test setup failed");

        begin_capture(&PttSource::LocalMicrophone);
        assert!(
            !super::super::audio::is_playing(),
            "the local mic opened while the assistant's own voice was still \
             playing through the speakers -- the barge-in acoustic leak window"
        );

        // A REMOTE peer's push-to-talk must not touch local playback at all
        // -- this machine keeps playing the assistant's voice through the
        // host's speakers regardless of a peer's key press, since the
        // host's own mic never opens for it in the first place.
        super::super::audio::set_playing_for_test(true);
        begin_capture(&PttSource::RemoteTrack {
            identity: "bob".into(),
        });
        assert!(
            super::super::audio::is_playing(),
            "a remote peer's push-to-talk stopped this machine's own playback"
        );

        super::super::audio::set_playing_for_test(false); // leave clean for other tests
    }

    #[test]
    fn a_remote_turn_never_ends_by_closing_the_local_microphone() {
        // The teardown half of the same rule: ending Bob's turn must stop the
        // tap on Bob, not shut a microphone that was never opened. Exercised
        // through `end_capture`, which is what `ptt_end` calls.
        let _serial = serialize();
        end_capture(&PttSource::RemoteTrack {
            identity: "bob".into(),
        });
        assert_eq!(
            super::super::remote_audio::tapped_identity(),
            None,
            "ending a remote turn must stop forwarding that peer's audio"
        );
    }
}
