use crate::sync_ext::MutexExt;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use crate::transport::publisher::RoomConnection;

use super::{
    stop_camera_publish, stop_share_explained, SessionState, ShareSessionError, StopShareAnalytics,
};

/// One terminal deadline starts immediately before the external room connect.
/// The final second is reserved for publication-timeout cleanup (#569).
///
/// Sized for a BAD network, not a good one: the pieces under it are each
/// bounded and retried on their own (metadata preflight 8s worst,
/// `backend_http` token retries ~22s worst, `meeting_core` connect attempts
/// ~33s worst) and their worst cases overlap only on a genuinely lossy link.
/// At the old 30s, worst-case token retries alone left the LiveKit connect
/// zero attempts before the deadline fired -- the user saw "could not join
/// room" on a network where a retry would have succeeded (2026-08-11).
/// Still a hard bound: the UI never waits on this path for more than 45s.
const JOIN_TERMINAL_BUDGET: Duration = Duration::from_secs(45);
const JOIN_CLEANUP_RESERVE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
struct JoinBudget {
    terminal_deadline: tokio::time::Instant,
}

impl JoinBudget {
    fn start() -> Self {
        Self {
            terminal_deadline: tokio::time::Instant::now() + JOIN_TERMINAL_BUDGET,
        }
    }

    fn work_deadline(self) -> tokio::time::Instant {
        self.terminal_deadline - JOIN_CLEANUP_RESERVE
    }
}

async fn await_join_work<F, T>(budget: JoinBudget, future: F) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    tokio::time::timeout_at(budget.work_deadline(), future)
        .await
        .map_err(|_| ())
}

enum JoinWorkWithCleanup<T, C> {
    Completed(T),
    TimedOut(Option<C>),
}

async fn await_join_work_with_cleanup<F, M, C, T, O>(
    budget: JoinBudget,
    future: F,
    before_cleanup: M,
    cleanup: C,
) -> JoinWorkWithCleanup<T, O>
where
    F: Future<Output = T>,
    M: FnOnce(),
    C: Future<Output = O>,
{
    match tokio::time::timeout_at(budget.work_deadline(), future).await {
        Ok(output) => JoinWorkWithCleanup::Completed(output),
        Err(_) => {
            before_cleanup();
            JoinWorkWithCleanup::TimedOut(
                tokio::time::timeout_at(budget.terminal_deadline, cleanup)
                    .await
                    .ok(),
            )
        }
    }
}

fn session_commit_is_current(generation_current: bool, current_room_matches: bool) -> bool {
    generation_current && current_room_matches
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MicrophonePublishDisposition {
    Commit,
    ContinueWithoutAudio,
    Superseded,
}

fn microphone_publish_disposition(
    publish_succeeded: bool,
    session_current: bool,
) -> MicrophonePublishDisposition {
    if !publish_succeeded {
        MicrophonePublishDisposition::ContinueWithoutAudio
    } else if session_current {
        MicrophonePublishDisposition::Commit
    } else {
        MicrophonePublishDisposition::Superseded
    }
}

// ===========================================================================
// #787 -- audio failures on the join path must be bounded, retried, and loud
// ===========================================================================
//
// Before this, every audio failure in the block below was a bare `log::warn!`
// and the meeting carried on looking completely normal. A user whose speaker
// playout failed to enable heard nobody for an entire call with one warn line
// to show for it. Two things change:
//
// 1. **Level.** `logging.rs` chains `sentry_log::default_filter`, which maps
//    Error -> a Sentry EVENT and Warn/Info -> a breadcrumb. A breadcrumb only
//    ships if some *later* error happens in the same session, so a warn-only
//    audio failure is invisible off-device -- the incident report never
//    exists. Raising these to `error!` is what makes them reportable.
//    These are join-path *hard* fails (mic/speaker never came up), not the
//    remote-track quality watchdog in `transport/audio.rs` (that one is
//    `warn!` -- a silent subscribed track is a PostHog rate later, not a
//    crash). Raising the level is also strictly safe against COURSE_CORRECTION
//    §4.2's `RUST_LOG` trap, which is about markers being *hidden*: any filter
//    that admits `warn` admits `error` too, and `desktop::*` targets are never
//    in `logging.rs`'s third-party denylist.
//
// 2. **Reach.** These ride the existing `resilience-event` channel, whose
//    `micDeviceFailed`/`speakerDeviceFailed` variants `ToastHost.svelte`
//    already renders as a dismissible "degraded" toast. No new event kind, no
//    frontend change -- the user is simply told, instead of discovering it
//    when nobody answers.

/// One attempt, plus two retries. `enable_managed_playout` is synchronous CoreAudio
/// call; the failure this is defending against is a device that is briefly
/// busy or mid-transition, not one that is missing (which fails all three the
/// same way, quickly).
const PLAYOUT_ENABLE_ATTEMPTS: u32 = 3;
const PLAYOUT_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// Whether attempt `attempt` (1-based) gets another try. Pure so the retry
/// budget is pinned by a test rather than by reading the loop.
fn should_retry_playout(attempt: u32, attempts: u32) -> bool {
    attempt < attempts
}

/// Delay before attempt `attempt` (1-based): none before the first, then a
/// linear back-off. Deliberately tiny -- the whole retry sequence has to fit
/// inside the join budget alongside everything else, and a device that needs
/// more than ~750 ms is not coming back within this join.
fn playout_retry_backoff(attempt: u32) -> Duration {
    PLAYOUT_RETRY_BACKOFF * attempt.saturating_sub(1)
}

/// Why the speaker side never came up. Separated from a bare string so the
/// caller can say "timed out" without parsing a message.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlayoutEnableFailure {
    Failed(String),
    TimedOut,
}

impl PlayoutEnableFailure {
    fn detail(&self) -> String {
        match self {
            Self::Failed(error) => error.clone(),
            Self::TimedOut => "exhausted the join budget".to_string(),
        }
    }
}

/// The two join-path audio failures that a person in the meeting actually
/// experiences, and the copy for each. Kept as an enum with pure accessors so
/// the user-visible strings are pinned by a test -- CLAUDE.md's hard rule is
/// that UI text must never truncate, and these ride a toast whose established
/// house style (`"Microphone disconnected — check input device"`, 43 chars) is
/// the length budget to stay inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioJoinFailure {
    Microphone,
    Playout,
}

impl AudioJoinFailure {
    /// The message the user sees. Says what they lost, not which API failed.
    fn notice(self) -> &'static str {
        match self {
            Self::Microphone => "Microphone unavailable — you can't be heard",
            Self::Playout => "Speaker unavailable — you can't hear others",
        }
    }

    fn event(self, message: String) -> crate::resilience_event::ResilienceEvent {
        match self {
            Self::Microphone => {
                crate::resilience_event::ResilienceEvent::MicDeviceFailed { message }
            }
            Self::Playout => {
                crate::resilience_event::ResilienceEvent::SpeakerDeviceFailed { message }
            }
        }
    }
}

/// Log at `error!` (Sentry event, see the section comment) and tell the user.
/// `context` is the developer-facing detail; it never reaches the toast,
/// which shows only `failure.notice()`.
fn report_audio_join_failure(app: &tauri::AppHandle, failure: AudioJoinFailure, context: &str) {
    log::error!("session: {context} -- {} (#787)", failure.notice());
    // Global `emit`, NOT `emit_to` -- see `resilience::emit`'s comment for
    // why a label-targeted emit never reaches the frontend's plain
    // `listen()`.
    let _ = tauri::Emitter::emit(
        app,
        "resilience-event",
        failure.event(failure.notice().to_string()),
    );
}

/// `PETAL_DISABLE_AUDIO`: skip mic capture AND speaker playout for this run
/// (headless/automated video-only testing). Extracted so the idempotent-
/// rejoin repair below honors exactly the same opt-out as the join path.
///
/// `0`/`false`/`no`/`off` mean ENABLED. This used to treat any non-empty
/// value as "disable", so the one incantation every doc, the cockpit
/// launcher's own error message, and every audio scenario tell you to use --
/// `PETAL_DISABLE_AUDIO=0` -- silently did the exact opposite and skipped the
/// mic. AUD-N2W caught it live: the web listener waited 15s for a track the
/// native side never published, in a run explicitly configured for audio.
/// Anything else non-empty still disables, so existing `=1` callers are
/// unchanged.
fn audio_disabled_by_env() -> bool {
    crate::transport::audio::audio_is_disabled(std::env::var("PETAL_DISABLE_AUDIO").ok().as_deref())
}

/// `enable_managed_playout` bounded and retried, mirroring the microphone path above.
///
/// The call it wraps is synchronous CoreAudio work. Running it directly on
/// the async runtime thread -- which is what #787 found at `room.rs:708` --
/// means a wedged audio device wedges the whole join with no deadline, and a
/// single transient failure costs the user every remote voice for the rest of
/// the meeting. `spawn_blocking` gets it off the runtime thread and
/// `await_join_work` gives it the same terminal deadline everything else on
/// this path already respects.
/// #787: hard cap for the pre-connect playout enable. Small on purpose --
/// see the call site in `join_room`; a wedge costs audio, never the join.
const EARLY_PLAYOUT_ENABLE_CAP: std::time::Duration = std::time::Duration::from_secs(8);

/// #787: at most one rejoin playout re-assert per window, process-wide.
/// The re-assert audibly stops/starts live playout, and the meeting route
/// calls `join_room` on every mount.
fn reassert_rate_limit_permits() -> bool {
    use std::sync::atomic::AtomicU64;
    static LAST_REASSERT_MS: AtomicU64 = AtomicU64::new(0);
    const WINDOW_MS: u64 = 30_000;
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let epoch = *EPOCH.get_or_init(std::time::Instant::now);
    // +1 so the very first call (elapsed 0) is distinguishable from the
    // never-fired sentinel 0.
    let now_ms = epoch.elapsed().as_millis() as u64 + 1;
    let last = LAST_REASSERT_MS.load(Ordering::SeqCst);
    if last != 0 && now_ms.saturating_sub(last) < WINDOW_MS {
        return false;
    }
    LAST_REASSERT_MS
        .compare_exchange(last, now_ms, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

async fn enable_playout_bounded(
    join_budget: JoinBudget,
    preferred_playout_device: Option<String>,
) -> Result<Arc<crate::transport::audio::SpeakerPlayout>, PlayoutEnableFailure> {
    let mut last_error = "no attempt completed".to_string();
    for attempt in 1..=PLAYOUT_ENABLE_ATTEMPTS {
        let backoff = playout_retry_backoff(attempt);
        if !backoff.is_zero()
            && tokio::time::timeout_at(join_budget.work_deadline(), tokio::time::sleep(backoff))
                .await
                .is_err()
        {
            return Err(PlayoutEnableFailure::TimedOut);
        }

        let device = preferred_playout_device.clone();
        let task = tokio::task::spawn_blocking(move || {
            crate::transport::audio::enable_managed_playout(device)
        });
        match await_join_work(join_budget, task).await {
            Ok(Ok(Ok(playout))) => {
                if attempt > 1 {
                    log::warn!(
                        "session: speaker playout enabled on attempt {attempt}/{PLAYOUT_ENABLE_ATTEMPTS}"
                    );
                }
                return Ok(Arc::new(playout));
            }
            Ok(Ok(Err(error))) => last_error = error.to_string(),
            Ok(Err(error)) => last_error = format!("playout task failed: {error}"),
            // Dropping a `spawn_blocking` JoinHandle detaches the closure; it
            // owns only its own `SpeakerPlayout` handle and drops it when it
            // eventually returns, with no session/room state access -- the
            // same reasoning as the microphone preparation timeout above.
            Err(()) => return Err(PlayoutEnableFailure::TimedOut),
        }

        if !should_retry_playout(attempt, PLAYOUT_ENABLE_ATTEMPTS) {
            break;
        }
        log::warn!(
            "session: speaker playout enable attempt {attempt}/{PLAYOUT_ENABLE_ATTEMPTS} failed, retrying: {last_error}"
        );
    }
    Err(PlayoutEnableFailure::Failed(last_error))
}

/// The one thing an idempotent rejoin repairs (#787). See the rejoin
/// short-circuit's comment for why this is a targeted retry of a single
/// missing resource rather than a change to what "rejoin" means.
///
/// Commits only if the session is still joined and still has no playout: a
/// real join running concurrently may have committed one while this was
/// blocked on the device, and two live `PlatformAudio` handles fighting over
/// the same session slot is exactly the stale-completion class `#569` closed.
async fn repair_missing_playout_on_rejoin(app: &tauri::AppHandle, state: &SessionState) {
    let preferred_playout_device = {
        let audio_preferences = app.try_state::<crate::transport::audio::AudioDevicePreferences>();
        audio_preferences
            .as_ref()
            .and_then(|preferences| preferences.playout_device())
    };

    match enable_playout_bounded(JoinBudget::start(), preferred_playout_device).await {
        Ok(playout) => {
            let mut guard = state.inner.lock_unpoisoned();
            if guard.joined.is_some() && guard.playout.is_none() {
                guard.playout = Some(playout);
                drop(guard);
                // `warn!`, not `info!`: this only ever runs because a
                // previous join left the user unable to hear anyone, and the
                // fact that a rejoin was needed to recover is the
                // interesting part of the log.
                log::warn!(
                    "session: rejoin repaired speaker playout that a previous join failed to enable (#787)"
                );
            } else {
                drop(guard);
                drop(playout);
                log::info!(
                    "session: rejoin playout repair discarded -- the session changed underneath it"
                );
            }
        }
        Err(failure) => report_audio_join_failure(
            app,
            AudioJoinFailure::Playout,
            &format!(
                "rejoin could not enable speaker playout either: {}",
                failure.detail()
            ),
        ),
    }
}

/// Privacy-safe, ordered markers for the tiny post-audio tail of `join_room`.
/// Keep these stable until #561's live diagnostic identifies the blocking seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinTailStage {
    PostAudio,
    ResilienceStartBegin,
    ResilienceStartReturned,
    JoinedTerminal,
}

impl JoinTailStage {
    const fn label(self) -> &'static str {
        match self {
            Self::PostAudio => "post_audio",
            Self::ResilienceStartBegin => "resilience_start_begin",
            Self::ResilienceStartReturned => "resilience_start_returned",
            Self::JoinedTerminal => "joined_terminal",
        }
    }
}

const JOIN_TAIL_STAGE_ORDER: [JoinTailStage; 4] = [
    JoinTailStage::PostAudio,
    JoinTailStage::ResilienceStartBegin,
    JoinTailStage::ResilienceStartReturned,
    JoinTailStage::JoinedTerminal,
];

fn log_join_tail_stage(stage: JoinTailStage, started_at: std::time::Instant) {
    log::info!(
        "session: join tail stage={} elapsed_ms={}",
        stage.label(),
        started_at.elapsed().as_millis()
    );
}

/// Which real, durable room (per `rooms::RoomRecord`) this process is
/// currently joined to, plus the real identity it joined under. Set once by
/// `join_room`, cleared by `leave_room`. Replaces the old lazy
/// "connect to `DEV_ROOM_NAME` on first share" stand-in entirely -- there is
/// no path left in this module that connects a room without going through
/// `join_room`.
#[derive(Clone)]
pub(super) struct RoomJoinInfo {
    /// The local durable room record this connection belongs to (SPEC.md
    /// §4.6) -- `room_name` is what the frontend/`RoomRow` know the room as;
    /// the actual LiveKit room name is derived from `room_record.id` (see
    /// `rooms::livekit_room_name`), not stored redundantly here.
    pub(super) room_record: crate::rooms::RoomRecord,
    pub(super) room_connection: Arc<RoomConnection>,
    /// The real identity this process joined under (from onboarding, passed
    /// in to `join_room` -- see that function's doc comment). Threaded
    /// through to `telepointer.rs` (replacing its own `DEV_USER_ID` stand-in)
    /// and `presence.rs`.
    pub(super) identity: String,
    pub(super) presence: Arc<crate::presence::PresenceState>,
    /// #259/#264: held for exactly as long as this process is joined to a
    /// room (dropped whenever every clone of this `RoomJoinInfo` is, i.e. on
    /// `leave_room` or a forced-disconnect cleanup -- see
    /// `cleanup_left_room`'s `guard.joined.take()`). `Arc`-wrapped (not a
    /// bare `DisplaySleepAssertion`) purely so `RoomJoinInfo` can keep
    /// deriving `Clone` -- there is exactly one held assertion per room join
    /// either way. `None` if IOKit refused the assertion (logged, non-fatal
    /// -- see `DisplaySleepAssertion::acquire`).
    #[allow(dead_code)]
    pub(super) display_sleep_assertion: Option<Arc<crate::platform::power::DisplaySleepAssertion>>,
}

impl SessionState {
    /// Snapshot of the room data channel and local identity for lightweight
    /// per-room metadata features that do not depend on this process sharing
    /// a window (e.g. controlling a remote compositor window).
    pub(crate) fn control_channel_snapshot(&self) -> Option<(Arc<RoomConnection>, String)> {
        let guard = self.inner.lock_unpoisoned();
        guard
            .joined
            .as_ref()
            .map(|j| (j.room_connection.clone(), j.identity.clone()))
    }

    /// The real durable room name (`RoomRecord.name`) this process is
    /// currently joined to, if any -- read by the frontend-facing
    /// `current_room` command and by `presence.rs`'s event payload.
    pub(crate) fn current_room_name(&self) -> Option<String> {
        let guard = self.inner.lock_unpoisoned();
        guard.joined.as_ref().map(|j| j.room_record.name.clone())
    }

    /// Full durable room record this process is currently joined to. Debug
    /// harnesses use this to derive the exact credential and LiveKit room
    /// without guessing a human label.
    pub(crate) fn current_room_record(&self) -> Option<crate::rooms::RoomRecord> {
        let guard = self.inner.lock_unpoisoned();
        guard.joined.as_ref().map(|j| j.room_record.clone())
    }

    /// Snapshot of who's currently present in the joined room (SPEC.md
    /// §4.6), or an empty list if not currently joined to any room.
    pub(crate) fn presence_snapshot(&self) -> Vec<crate::presence::PresentParticipant> {
        let guard = self.inner.lock_unpoisoned();
        guard
            .joined
            .as_ref()
            .map(|j| j.presence.snapshot())
            .unwrap_or_default()
    }
}

/// Join a real, durable room (SPEC.md §4.6) by its human-readable name:
/// looks up (or creates, if missing) the local `rooms::RoomRecord`, derives
/// the durable LiveKit room name from it (`rooms::livekit_room_name` -- NOT
/// the literal `"petal-dev-room"` this module used to hardcode), mints a
/// real access token for `identity`/`display_name` (passed in from the
/// frontend's onboarding store -- see this command's call site in
/// `lib.rs`/the frontend `join_room` invocation), and connects.
///
/// ## Idempotent rejoin (SPEC.md §4.6: "Idempotent join... never duplicates
/// membership")
///
/// If this process is ALREADY joined to the same room (by durable room id,
/// not display name -- so renaming wouldn't defeat this check, though no
/// rename UI exists yet), this is a clean no-op: returns the existing
/// `RoomJoinInfo` without reconnecting, republishing, or touching any active
/// share. There is exactly one membership concept in this local model --
/// "this process has one live `RoomConnection` connected under one identity to
/// one room at a time" -- so "already a member" is simply "already the
/// currently-joined room," and rejoining can't duplicate anything because
/// there's nothing list-shaped to duplicate (no membership list is appended
/// to; the single `joined` slot is just confirmed to already hold the right
/// value). If this process is joined to a DIFFERENT room, that room is left
/// first (`leave_room_inner`) before joining the new one -- a process is
/// only ever a member of one room at a time in this model, matching how a
/// real single-window desktop app's meeting concept works (you're in one
/// meeting, or none).
///
/// Starts telepointers' receiver, audio (mic publish + playout), presence
/// tracking, and connection-resilience watchers exactly once per room
/// connection -- all moved here from the old `ensure_room_connected` (which
/// used to run this lazily on first share); this is the same one-time-per-
/// connection seam, just triggered by an explicit join instead of an
/// implicit first share.
pub async fn join_room(
    app: &tauri::AppHandle,
    rooms: &crate::rooms::RoomsState,
    state: &SessionState,
    room_name: String,
    identity: String,
    display_name: String,
    remote_control_policy: crate::remote_control_core::RemoteControlPolicy,
    identity_palette_index: Option<u8>,
) -> Result<crate::rooms::RoomRecord, ShareSessionError> {
    let remote_control_allowed = remote_control_policy.as_wire();
    log::info!(
        "session: join_room('{}') begin (identity '{}')",
        crate::logging::log_safe_quoted(&room_name),
        crate::logging::log_safe_quoted(&identity)
    );
    log::info!(
        "session: join_room('{}') permission snapshot -- Screen Recording {}, Accessibility {}, remote_control_policy={remote_control_allowed}",
        crate::logging::log_safe_quoted(&room_name),
        if crate::window_source::has_screen_recording_access() {
            "GRANTED"
        } else {
            "DENIED"
        },
        if crate::permissions::check_accessibility() {
            "GRANTED"
        } else {
            "DENIED"
        }
    );

    let room_record = crate::meeting_core::persist_joined_room_record(rooms, &room_name)?;

    // Idempotent rejoin: already joined to this exact room -> no-op.
    //
    // #787 deliberately does NOT change that. The user in #787 rejoined
    // natively and nothing changed, because this returns before the audio
    // block and the playout enable at the bottom of it is never re-run. The
    // tempting "fix" -- make a rejoin tear down and reconnect -- is a real
    // change to join semantics: it would drop every share, re-run token
    // minting, and re-enter the room, all as a side effect of a button that
    // has always meant "you are already here." That is not something to
    // change while chasing a silent-audio bug.
    //
    // What it does instead is repair exactly the resource that is knowably
    // missing. `playout` is `None` only if `enable_managed_playout` failed at join
    // (or was skipped); a `Some` is never torn down while joined. So a
    // no-op rejoin now retries just that one call, in place, holding the same
    // room connection -- no reconnect, no republish, same return value. The
    // rejoin also states the audio state it found, so a log from an affected
    // machine says whether the retry was even applicable instead of leaving
    // "no-op rejoin" to be read as "nothing was wrong."
    let already_joined = {
        let guard = state.inner.lock_unpoisoned();
        guard
            .joined
            .as_ref()
            .filter(|joined| joined.room_record.id == room_record.id)
            .map(|joined| (joined.room_record.clone(), guard.playout.is_some()))
    };
    if let Some((joined_record, playout_present)) = already_joined {
        log::info!(
            "session: join_room('{}') -- already joined, no-op rejoin (speaker playout {})",
            crate::logging::log_safe_quoted(&room_name),
            if playout_present {
                "already enabled"
            } else {
                "MISSING"
            }
        );
        if !audio_disabled_by_env() {
            if playout_present {
                // #787: a playout HANDLE proves nothing about the ADM's
                // actual mode -- the proxy's platform switch can fail
                // silently and no Rust-side call could previously reach it.
                // Re-drive the full Init+Start pair so a user rejoin is a
                // real recovery action, not a no-op. Rate-limited: the
                // meeting route calls join_room on every mount, and the
                // toggle stops/starts live playout -- an audible blip that
                // must not fire on every remount, and whose (true) leg could
                // itself hit a transient failure. A troubled user's repeated
                // deliberate rejoin still gets through after the window.
                let playout = if reassert_rate_limit_permits() {
                    let guard = state.inner.lock_unpoisoned();
                    guard.playout.clone()
                } else {
                    log::info!("session: rejoin playout re-assert skipped (rate-limited)");
                    None
                };
                if let Some(playout) = playout {
                    let reassert = tokio::task::spawn_blocking(move || playout.reassert_playout());
                    if tokio::time::timeout(std::time::Duration::from_secs(10), reassert)
                        .await
                        .is_err()
                    {
                        log::warn!("session: rejoin playout re-assert timed out");
                    }
                }
            } else {
                repair_missing_playout_on_rejoin(app, state).await;
            }
        }
        return Ok(joined_record);
    }

    // Joined to a DIFFERENT room -- leave it first (a process is a member of
    // at most one room at a time in this model).
    leave_room(app, state).await;

    // `can_subscribe: true` -- telepointers (SPEC.md §4.5) need this process
    // to *receive* other participants' cursor data-channel messages on this
    // same room connection, not just publish video. `RoomEvent::DataReceived`
    // is delivered independent of `auto_subscribe`/video-track subscription
    // (that flag only gates automatic *track* subscription, checked directly
    // against the `livekit` 0.7.49 source -- there is no separate
    // data-channel auto-subscribe knob), but `can_subscribe` is a
    // server-enforced room-join grant, and this process is both a potential
    // sharer AND a potential viewer of others' telepointers, so it needs the
    // grant. Video tracks are auto-subscribed by `RoomConnection`'s
    // `RoomOptions::auto_subscribe = true`; the compositor consumes the
    // connect-time event receiver below so pre-existing shares are retained.

    // #569: one generation and one terminal deadline cover every external
    // await from room connect through palette/microphone publication.
    let room_generation = state.begin_room_generation();
    let join_budget = JoinBudget::start();

    // #787: enable speaker playout BEFORE connecting. `auto_subscribe` wires
    // any already-published remote audio DURING connect, and the voice
    // engine's InitPlayout/StartPlayout race a post-connect enable -- an
    // enable landing between them would leave the platform ADM started-but-
    // never-initialized and the meeting silent until the next fresh audio
    // publication (structurally possible; never reproduced live). Acquire
    // the ADM first so those calls run against real speakers from the start.
    //
    // This deliberately does NOT draw on `join_budget`: a wedged CoreAudio
    // call here must cost the user audio (rejoin-repair is the retry), never
    // the join itself -- so it gets its own small cap instead of eating the
    // connect deadline.
    let early_playout = if audio_disabled_by_env() {
        None
    } else {
        let preferred_playout_device = app
            .try_state::<crate::transport::audio::AudioDevicePreferences>()
            .as_ref()
            .and_then(|preferences| preferences.playout_device());
        let enable = enable_playout_bounded(JoinBudget::start(), preferred_playout_device);
        match tokio::time::timeout(EARLY_PLAYOUT_ENABLE_CAP, enable).await {
            Ok(Ok(playout)) => Some(playout),
            Ok(Err(failure)) => {
                report_audio_join_failure(
                    app,
                    AudioJoinFailure::Playout,
                    &format!(
                        "speaker playout enable failed after {PLAYOUT_ENABLE_ATTEMPTS} attempt(s), continuing without audio: {}",
                        failure.detail()
                    ),
                );
                None
            }
            Err(_) => {
                report_audio_join_failure(
                    app,
                    AudioJoinFailure::Playout,
                    "speaker playout enable exceeded its pre-connect cap, continuing without audio",
                );
                None
            }
        }
    };

    let connected = match await_join_work(
        join_budget,
        crate::meeting_core::connect_room(rooms, room_record, &identity, &display_name),
    )
    .await
    {
        Ok(Ok(connected)) => connected,
        Ok(Err(error)) => {
            state.invalidate_room_generation();
            log::error!(
                "session: join_room('{}') failed -- portable room connect error: {error}",
                crate::logging::log_safe_quoted(&room_name)
            );
            return Err(error.into());
        }
        Err(()) => {
            state.invalidate_room_generation();
            log::error!(
                "session: join_room('{}') failed -- portable room connect timed out",
                crate::logging::log_safe_quoted(&room_name)
            );
            crate::analytics::join_failed_from_connect_timeout();
            return Err(ShareSessionError::JoinTimeout);
        }
    };
    let room_record = connected.room_record;
    let room_connection = connected.room_connection;
    let livekit_room_name = connected.livekit_room_name;
    let url = connected.url;
    match await_join_work(
        join_budget,
        room_connection.publish_identity_palette_index(identity_palette_index),
    )
    .await
    {
        Ok(Ok(palette_index)) if room_generation.is_current() => {
            room_connection.commit_identity_palette_index(palette_index);
        }
        Ok(Ok(_)) => {
            log::warn!("session: skipped stale identity palette commit");
        }
        Ok(Err(error)) => {
            log::warn!("publisher: failed to publish identity color metadata: {error}");
        }
        Err(()) => {
            log::warn!("publisher: identity color metadata timed out; continuing join");
        }
    }
    let presence = Arc::new(crate::presence::PresenceState::default());

    // #259/#264: prevent idle display sleep for the duration of this meeting
    // (see `platform::power::DisplaySleepAssertion`'s doc comment for why
    // this alone isn't the full fix -- `resilience.rs`'s screensDidSleep/
    // Wake pause/resume is the real safety net for a user-forced sleep).
    let display_sleep_assertion = crate::platform::power::DisplaySleepAssertion::acquire(&format!(
        "Petal meeting: {}",
        room_record.name
    ))
    .map(Arc::new);

    let room_commit_is_current = {
        let mut guard = state.inner.lock_unpoisoned();
        if room_generation.is_current() {
            guard.joined = Some(RoomJoinInfo {
                room_record: room_record.clone(),
                room_connection: room_connection.clone(),
                identity: identity.clone(),
                presence: presence.clone(),
                display_sleep_assertion,
            });
            true
        } else {
            false
        }
    };
    if !room_commit_is_current {
        let _ = tokio::time::timeout_at(
            join_budget.terminal_deadline,
            room_connection.room().close(),
        )
        .await;
        return Err(ShareSessionError::RoomConnect(
            "join attempt was superseded".to_string(),
        ));
    }
    let remote_control_commit_is_current = {
        let guard = state.inner.lock_unpoisoned();
        let current_room_matches = guard
            .joined
            .as_ref()
            .map(|joined| Arc::ptr_eq(&joined.room_connection, &room_connection))
            .unwrap_or(false);
        let current = session_commit_is_current(room_generation.is_current(), current_room_matches);
        if current {
            // Commit the meeting-scoped switch while the exact joined-room
            // slot used for validation is still locked.
            state.seed_remote_control_policy(remote_control_policy);
        }
        current
    };
    if !remote_control_commit_is_current {
        let _ = tokio::time::timeout_at(
            join_budget.terminal_deadline,
            room_connection.room().close(),
        )
        .await;
        return Err(ShareSessionError::RoomConnect(
            "join attempt was superseded".to_string(),
        ));
    }

    // Telepointers (SPEC.md §4.5): start this process's receiver task on the
    // room connection we just made, exactly once per connection (not once
    // per share) -- see `telepointer::start_receiver_for_room`'s doc comment
    // for what it does with received data.
    crate::telepointer::start_receiver_for_room(
        app,
        room_connection.room(),
        room_generation.clone(),
    );

    // Remote control: data-channel receiver for viewer-originated input
    // against windows this process is sharing. Same one-room-connection seam
    // as telepointers; the receiver validates `targetUserId` and active local
    // shares before any replay side effect.
    crate::remote_control::start_receiver_for_room(
        app,
        room_connection.room(),
        identity.clone(),
        room_generation.clone(),
    );

    // Drawing/annotation: reliable batched stroke messages for remote
    // compositor windows. Same one-room-connection seam as telepointers and
    // remote control; receiver derives the drawer from the authenticated
    // LiveKit sender instead of trusting payload identity.
    crate::draw::start_receiver_for_room(app, room_connection.room(), room_generation.clone());

    // AI chat (#657): start/stop requests, push-to-talk floor claims, and
    // remote session state. Every inbound message is authorized against the
    // per-kind matrix in `ai_chat::wire` before it can affect anything, and a
    // start routes through the same guarded path as a local click.
    #[cfg(target_os = "macos")]
    crate::ai_chat::topic::start_receiver_for_room(
        app,
        room_connection.room(),
        room_generation.clone(),
    );

    // Crisp mode (#384 Phase 1 spike): decodes + stores each received still
    // (versioned, per window). Same one-room-connection seam as the
    // receivers above. NOT yet wired to any native blit -- see
    // crisp_still.rs's module doc comment for exactly what remains.
    crate::crisp_still::start_receiver_for_room(
        app,
        room_connection.room(),
        room_generation.clone(),
    );

    // Data-channel RTT probe for the network cockpit. This is intentionally
    // peer-to-peer data-channel RTT only, gated by the cockpit-open flag in
    // diagnostics so a closed cockpit burns no probe bandwidth.
    if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
        crate::latency_probe::start_receiver_for_room(
            app,
            room_connection.room(),
            identity.clone(),
            room_generation.clone(),
            diagnostics.inner().clone(),
        );
        crate::pipeline_stats::start_receiver_for_room(
            app,
            room_connection.room(),
            identity.clone(),
            room_generation.clone(),
            diagnostics.inner().clone(),
        );
        // Test-cockpit walking skeleton (#254): log/journal any web-peer
        // self-report over `petal.cockpit`. Read-only; no verdict consumption
        // yet (that's Phase 3+).
        crate::cockpit_topic::start_receiver_for_room(
            app,
            room_connection.room(),
            room_generation.clone(),
            diagnostics.inner().clone(),
        );
    } else {
        log::warn!(
            "session: DiagnosticsState not managed -- latency probe and pipeline stats disabled this run"
        );
    }

    // Passive viewer demand: receivers publish "this remote compositor
    // window is open/visible" over LiveKit data. Sharers use it to keep
    // watched non-focused shares at Full quality.
    crate::viewer_demand::start_for_room(
        app,
        room_connection.room(),
        identity.clone(),
        room_generation.clone(),
    );

    // Receiver-side compositor (SPEC.md §4.4): the real "shared windows
    // render as real native windows on every other participant's machine"
    // path -- see `transport::subscriber::start_compositor_feed`'s doc
    // comment. Started once per room connection, same seam as telepointers/
    // presence/audio/resilience below.
    crate::transport::subscriber::start_compositor_feed(
        app,
        room_connection
            .take_compositor_events()
            .expect("RoomConnection compositor event receiver already taken"),
        // #298: the same live room object the feed reconciles its receiver
        // state against, so "is this share actually backed by a publication
        // right now" is answered from the SDK rather than an event replay.
        room_connection.room(),
        identity.clone(),
        room_generation.clone(),
    );

    // Network/system diagnostics (issue #19 Phase A): stats poller +
    // event journal, exactly once per room connection -- same seam as the
    // watchers above/below. Deliberately self-terminating (its own event
    // loop breaks on RoomEvent::Disconnected and stops its poller via a
    // generation counter), so leave_room needs no diagnostics teardown call
    // -- see diagnostics.rs's module doc comment.
    crate::diagnostics::start_for_room(
        app,
        room_connection.room(),
        room_record.name.clone(),
        url.clone(),
        identity.clone(),
    );

    // Presence (SPEC.md §4.6): watch join/leave events on this room
    // connection, exactly once per connection -- see `presence.rs` for why
    // this reuses LiveKit's own participant-connect/disconnect events rather
    // than building a second heartbeat mechanism.
    crate::presence::start_for_room(
        app,
        room_connection.room(),
        presence,
        room_record.name.clone(),
        identity.clone(),
        display_name,
        room_generation.clone(),
    );

    // Audio (SPEC.md §4.9): mic publish + speaker playout, exactly once per
    // room connection -- see module doc comment "Audio lifecycle" for why
    // this lives in `join_room` (tied to "joined the room") rather than
    // per-window-share. A missing/denied microphone or audio device must NOT
    // prevent joining the room -- log and continue rather than failing
    // `join_room` outright.
    //
    // Opt-out escape hatch (`PETAL_DISABLE_AUDIO`): when set, skip mic capture
    // AND speaker playout entirely for this run. This exists for headless/
    // automated testing of the *video* screenshare loop where grabbing the
    // mic/speaker devices is undesirable -- e.g. when the tester is on a
    // parallel voice call and the app opening the mic causes feedback. It is
    // env-gated and OFF by default, so normal launches are unaffected. Screen
    // sharing works fine without audio (the two are independent, per the
    // module doc comment).
    if audio_disabled_by_env() {
        log::warn!(
            "session: PETAL_DISABLE_AUDIO set -- skipping mic publish + speaker playout (video-only run)"
        );
    } else {
        let audio_preferences = app.try_state::<crate::transport::audio::AudioDevicePreferences>();
        let preferred_recording_device = audio_preferences
            .as_ref()
            .and_then(|p| p.recording_device());
        let initial_mic_muted = state.desired_mic_muted.load(Ordering::SeqCst);

        let prepare_task = tokio::task::spawn_blocking(move || {
            crate::transport::audio::prepare_microphone(
                preferred_recording_device,
                initial_mic_muted,
            )
        });
        let prepared = match await_join_work(join_budget, prepare_task).await {
            Ok(Ok(Ok(prepared))) => Some(prepared),
            Ok(Ok(Err(e))) => {
                report_audio_join_failure(
                    app,
                    AudioJoinFailure::Microphone,
                    &format!("microphone preparation failed, continuing without audio: {e}"),
                );
                None
            }
            Ok(Err(e)) => {
                report_audio_join_failure(
                    app,
                    AudioJoinFailure::Microphone,
                    &format!("microphone preparation task failed, continuing without audio: {e}"),
                );
                None
            }
            Err(()) => {
                // Dropping a spawn_blocking JoinHandle detaches the closure.
                // It owns every prepared resource and drops them when it
                // eventually returns; it has no session/room state access.
                report_audio_join_failure(
                    app,
                    AudioJoinFailure::Microphone,
                    "microphone preparation timed out, continuing without audio",
                );
                None
            }
        };

        if let Some(prepared) = prepared {
            let mut prepared = Some(prepared);
            let room = room_connection.room();
            match await_join_work_with_cleanup(
                join_budget,
                crate::transport::audio::publish_prepared_microphone(
                    &room,
                    prepared.as_ref().expect("prepared microphone present"),
                ),
                || {
                    prepared
                        .as_ref()
                        .expect("prepared microphone present")
                        .mute_for_cleanup();
                },
                crate::transport::audio::unpublish_prepared_microphone(
                    &room,
                    prepared.as_ref().expect("prepared microphone present"),
                ),
            )
            .await
            {
                JoinWorkWithCleanup::Completed(Ok(())) => {
                    let committed = {
                        let mut guard = state.inner.lock_unpoisoned();
                        let current_room_matches = guard
                            .joined
                            .as_ref()
                            .map(|joined| Arc::ptr_eq(&joined.room_connection, &room_connection))
                            .unwrap_or(false);
                        let disposition = microphone_publish_disposition(
                            true,
                            session_commit_is_current(
                                room_generation.is_current(),
                                current_room_matches,
                            ),
                        );
                        match disposition {
                            MicrophonePublishDisposition::Commit => {
                                let mic = Arc::new(
                                    prepared
                                        .take()
                                        .expect("prepared microphone present")
                                        .into_mic_track(),
                                );
                                guard.mic = Some(mic);
                                disposition
                            }
                            _ => disposition,
                        }
                    };
                    match committed {
                        MicrophonePublishDisposition::Commit => {
                            state.set_mic_muted(state.desired_mic_muted.load(Ordering::SeqCst));
                        }
                        MicrophonePublishDisposition::Superseded => {
                            let prepared = prepared.as_ref().expect("prepared microphone present");
                            prepared.mute_for_cleanup();
                            let _ = tokio::time::timeout_at(
                                join_budget.terminal_deadline,
                                crate::transport::audio::unpublish_prepared_microphone(
                                    &room, prepared,
                                ),
                            )
                            .await;
                            return Err(ShareSessionError::RoomConnect(
                                "join attempt was superseded".to_string(),
                            ));
                        }
                        MicrophonePublishDisposition::ContinueWithoutAudio => {
                            unreachable!("successful publication has a terminal disposition")
                        }
                    }
                }
                JoinWorkWithCleanup::Completed(Err(e)) => {
                    report_audio_join_failure(
                        app,
                        AudioJoinFailure::Microphone,
                        &format!("microphone publish failed, continuing without audio: {e}"),
                    );
                }
                JoinWorkWithCleanup::TimedOut(cleanup_result) => {
                    report_audio_join_failure(
                        app,
                        AudioJoinFailure::Microphone,
                        "microphone publish timed out, continuing without audio",
                    );
                    match cleanup_result {
                        Some(Ok(())) => {}
                        Some(Err(e)) => log::warn!(
                            "session: timed-out microphone cleanup could not unpublish: {e}"
                        ),
                        None => log::warn!(
                            "session: timed-out microphone cleanup exhausted join deadline"
                        ),
                    }
                }
            }
        }
        // #787: playout was enabled BEFORE connect (see `early_playout`
        // above); this is only the generation-guarded commit of that handle.
        // A `None` here means the pre-connect enable already failed and
        // reported; the rejoin repair path remains the retry.
        if let Some(playout) = early_playout {
            let mut guard = state.inner.lock_unpoisoned();
            let current_room_matches = guard
                .joined
                .as_ref()
                .map(|joined| Arc::ptr_eq(&joined.room_connection, &room_connection))
                .unwrap_or(false);
            if session_commit_is_current(room_generation.is_current(), current_room_matches) {
                guard.playout = Some(playout);
            } else {
                drop(guard);
                drop(playout);
                return Err(ShareSessionError::RoomConnect(
                    "join attempt was superseded".to_string(),
                ));
            }
        }
        crate::transport::audio::start_audio_track_logger(
            room_connection.room(),
            room_generation.clone(),
        );
    }

    let join_tail_started_at = std::time::Instant::now();
    log_join_tail_stage(JoinTailStage::PostAudio, join_tail_started_at);

    // Connection resilience (SPEC.md §4.8): start the reconnect-toast
    // bridge for this room connection -- same seam as telepointers/audio/
    // presence above. Issue #18: only the room-event bridge is per-room (it
    // self-terminates when this room's event channel closes); the
    // network-change monitor and audio device-hot-swap poll inside
    // `start_for_room` are app-level singletons that look the CURRENT room/
    // audio handles at fire time, so repeated join/leave cycles never accumulate
    // watchers (no teardown needed here in `leave_room` either).
    // `MicWatchHandle` closes over the `AppHandle` and
    // re-reads `SessionState.mic` via Tauri's managed-state lookup on every
    // poll tick (cheap: a mutex lock + `Arc` clone), rather than capturing
    // today's `mic` snapshot by value -- this way the poll correctly sees
    // `None` if mic publish just failed above, and would also pick up a mic
    // published later if that ever becomes possible (it isn't, today: mic
    // publish is exactly the block above, once per room connection).
    let app_for_mic_watch = app.clone();
    let app_for_speaker_watch = app.clone();
    log_join_tail_stage(JoinTailStage::ResilienceStartBegin, join_tail_started_at);
    crate::resilience::start_for_room(
        app,
        room_connection
            .take_resilience_events()
            .expect("RoomConnection resilience event receiver already taken"),
        room_generation,
        Some(crate::resilience::MicWatchHandle::new(move || {
            app_for_mic_watch
                .try_state::<SessionState>()
                .and_then(|state| state.inner.lock_unpoisoned().mic.clone())
        })),
        Some(crate::resilience::SpeakerWatchHandle::new(move || {
            app_for_speaker_watch
                .try_state::<SessionState>()
                .and_then(|state| state.inner.lock_unpoisoned().playout.clone())
        })),
    );
    log_join_tail_stage(JoinTailStage::ResilienceStartReturned, join_tail_started_at);

    log::info!(
        "session: joined room '{}' (livekit room '{}') as identity '{}'",
        crate::logging::log_safe_quoted(&room_record.name),
        crate::logging::log_safe_quoted(&livekit_room_name),
        crate::logging::log_safe_quoted(&identity)
    );
    crate::analytics::meeting_joined();
    log_join_tail_stage(JoinTailStage::JoinedTerminal, join_tail_started_at);

    // #680: manufacture the full border + draw-overlay capacity only after a
    // room is joined, so a first/second share can consume already-realized
    // retired handles instead of racing into CreateFresh. Spawned rather than
    // awaited -- an adversarial review caught that awaiting here blocks the
    // user's "joined" transition on up to 8 hidden NSPanel+WKWebView builds
    // across 2 main-thread round-trips on the very first join after every app
    // launch (empty pool), an unmeasured latency add to a hot UX path for a
    // defense-in-depth fix to a rare race. Correctness only needs prewarm to
    // START before a share can happen, not complete before join_room returns
    // -- matches spawn_local_publish_reconcile's identical reasoning below.
    spawn_share_panel_prewarm(app);

    // Reconcile the rest of the user's local publish state against intent
    // (camera + window shares; the mic was already republished by the audio
    // tail above). Spawned so a slow camera start (its bounded first-frame
    // wait is 5s per attempt) never delays `join_room`'s return; the room
    // generation cancels it if this room is left/superseded meanwhile.
    spawn_local_publish_reconcile(app, room_record.id.clone());

    // NOTE: the main window is deliberately NOT hidden on join anymore --
    // it stays visible and hosts the in-meeting UI (MeetingChrome on
    // /meeting/[room]: large Gallery view <-> compact pill bar).

    Ok(room_record)
}

/// The single, explicit "rejoin restores what the user was publishing" seam
/// (see `LeavePublishCarryover` in mod.rs for the full inventory -- add any
/// future locally-published track type THERE and consume it HERE):
/// - **mic**: join-driven; `join_room`'s audio tail already republished it.
/// - **camera**: if the carried-over intent was ON, restore it through the
///   bounded self-heal loop (immediate attempt + backed-off retries, then a
///   terminal `camera-publish-state` event -- never a silent drop, which is
///   exactly what the 2026-07-30 leave→rejoin incident shipped).
/// - **window shares**: re-share via the REAL UI path
///   (`hover_tab::toggle_share_for_window` -- border bookkeeping,
///   optimistic-then-rollback, `share-error` emission), so hover-tab state
///   cannot go stale the way bypassing it once did (issue #13).
/// #680: build the full border + draw-overlay retired-pool capacity off the
/// join_room critical path. A first/second share must still consume these
/// already-realized handles instead of racing into CreateFresh, but nothing
/// about that requires join_room's own return to wait for it -- only that
/// prewarm has STARTED, since a share can't happen before the frontend sees
/// the join complete and the user takes an action afterward anyway.
fn spawn_share_panel_prewarm(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        crate::share_border::prewarm_share_borders(&app).await;
        crate::share_overlay::prewarm_share_overlays(&app).await;
    });
}

fn spawn_local_publish_reconcile(app: &tauri::AppHandle, room_id: String) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(state) = tauri::Manager::try_state::<SessionState>(&app) else {
            return;
        };
        let generation = state.current_room_generation();
        let plan = state.take_leave_publish_carryover(&room_id);
        if !plan.camera_on && plan.shares.is_empty() {
            return;
        }
        log::info!(
            "session: reconciling local publish state after rejoin (camera_on={}, shares={:?})",
            plan.camera_on,
            plan.shares.iter().map(|(id, _)| *id).collect::<Vec<_>>()
        );

        if plan.camera_on && !state.camera_publishing() {
            state.set_camera_intent(true);
            let camera_app = app.clone();
            tauri::async_runtime::spawn(async move {
                let Some(state) = tauri::Manager::try_state::<SessionState>(&camera_app) else {
                    return;
                };
                crate::session::ensure_camera_published(
                    &camera_app,
                    &state,
                    crate::camera_session::CAMERA_REJOIN_ATTEMPT_SCHEDULE,
                )
                .await;
            });
        }

        for (window_id, frame) in plan.shares {
            if !generation.is_current() {
                log::info!(
                    "session: rejoin publish reconcile cancelled before window {window_id} (room left again)"
                );
                return;
            }
            if state.is_share_active(window_id) {
                continue;
            }
            let shared =
                crate::hover_tab::toggle_share_for_window(&app, &state, window_id, frame).await;
            if !shared {
                // toggle_share_for_window already rolled back its border and
                // emitted `share-error` (visible in the UI); just record why
                // this window didn't come back.
                log::warn!(
                    "session: rejoin re-share of window {window_id} failed (window closed during the gap?)"
                );
            }
        }
    });
}

/// Leave the currently-joined room, if any (SPEC.md §4.6's explicit meeting
/// lifecycle). Tears down every active share's capture/publish first (a
/// left room has no connection left to publish tracks on), every open
/// receiver-side compositor window (SPEC.md §4.4 -- this process is no
/// longer receiving anyone's shared windows once it's left), then
/// unpublishes audio and closes the LiveKit room connection. Idempotent:
/// leaving when not currently joined to anything is a clean no-op.
pub async fn leave_room(app: &tauri::AppHandle, state: &SessionState) {
    cleanup_left_room(app, state, true, "leave_room").await;
}

/// Cleanup shared by explicit leaves and forced LiveKit disconnects.
///
/// `close_room_connection=false` is for `RoomEvent::Disconnected` paths where
/// the SDK already considers the room gone; we still must clear every local
/// UI/native surface that explicit leave clears.
pub(crate) async fn cleanup_for_forced_disconnect(app: &tauri::AppHandle, state: &SessionState) {
    cleanup_left_room(app, state, false, "forced_disconnect").await;
}

async fn cleanup_left_room(
    app: &tauri::AppHandle,
    state: &SessionState,
    close_room_connection: bool,
    reason: &str,
) {
    crate::hover_tab::cancel_drag_for_lifecycle();
    crate::remote_control::revoke_all(app);
    // The picker is a meeting-scoped surface: it must not remain on the
    // desktop after the user exits the meeting (hide, don't destroy — a
    // re-open re-shows the hidden singleton; the window-change watcher
    // self-terminates once the picker is no longer visible).
    crate::window_picker::hide_picker_on_meeting_exit(app);
    // Any AI chat session belongs to a window shared in the room being left,
    // and its credential must not survive the room (#655/#656). Both explicit
    // leaves and forced disconnects come through here.
    #[cfg(target_os = "macos")]
    {
        crate::ai_chat::session::stop(app, crate::ai_chat::state::EndReason::Stopped);
        crate::ai_chat::room_auth::forget();
    }
    state.invalidate_room_generation();

    // Snapshot the user's local-publish intent BEFORE tearing anything down,
    // so a rejoin of the SAME room can reconcile publishes back to what the
    // user wanted (see `LeavePublishCarryover` in mod.rs -- the fix for the
    // live 2026-07-30 incident where rejoin restored the mic but silently
    // dropped the camera). The mic needs no carryover: `join_room`'s audio
    // tail republishes it on every join.
    {
        let room_id = {
            let guard = state.inner.lock_unpoisoned();
            guard
                .joined
                .as_ref()
                .map(|joined| joined.room_record.id.clone())
        };
        if let Some(room_id) = room_id {
            let shares: Vec<(u32, crate::hover_tab::WindowFrame)> = state
                .active_share_restart_plan()
                .into_iter()
                .map(
                    |(window_id, frame, _started_seq, _resolution, _source_kind, _border_color)| {
                        (window_id, frame)
                    },
                )
                .collect();
            let camera_on = state.camera_intent();
            log::info!(
                "session: {reason} recording publish carryover (camera_on={camera_on}, shares={:?})",
                shares.iter().map(|(id, _)| *id).collect::<Vec<_>>()
            );
            state.record_leave_publish_carryover(room_id, camera_on, shares);
        }
        // Leaving turns everything off; the carryover above (not this live
        // flag) is what a same-room rejoin consumes.
        state.set_camera_intent(false);
    }

    // Stop every active share first -- reuses `stop_share`'s own
    // capture-stop/pump-abort/unpublish logic exactly, rather than
    // duplicating it here, so there's one code path for "a share's capture
    // pipeline tears down," not two that could drift apart.
    let window_ids: Vec<u32> = {
        let guard = state.inner.lock_unpoisoned();
        guard.shares.keys().copied().collect()
    };
    if !window_ids.is_empty() {
        log::info!(
            "session: {reason} stopping {} active share(s): {window_ids:?}",
            window_ids.len()
        );
    }
    for window_id in window_ids {
        if let Err(e) =
            stop_share_explained(app, state, window_id, StopShareAnalytics::Silent).await
        {
            log::warn!("session: {reason}: error stopping share {window_id}: {e}");
        }
    }

    // Issue #13 anomaly root cause: the loop above calls `session::stop_share`
    // DIRECTLY, bypassing `hover_tab::toggle_share_for_window` -- so before
    // this call existed, hover_tab's own `SHARE_STATE` (its `shared` HashSet
    // + `borders` map) went STALE across a leave: the pill kept rendering
    // "unshare" for windows the session no longer tracked, the colored border
    // panels stayed on screen, and clicking the stale pill after a rejoin
    // fired a `stop_share(...) begin` with no matching `start_share` (the
    // exact orphan line in the crash evidence) plus a border-panel hide/close
    // for a share that didn't exist. Clear hover_tab's bookkeeping (and hide
    // its border panels -- hide + retire, never destroy) in the same seam
    // that tears the shares down.
    crate::hover_tab::clear_share_state_on_leave(app);
    // #872: session leave must retire overlays absent from hover-tab's map.
    crate::share_overlay::retire_all_overlays(app);

    // Close every remote compositor window this process had open -- unlike
    // `RoomEvent::ParticipantDisconnected` (fired for OTHER participants
    // leaving), OUR OWN `Room::close()` below does not synthesize per-remote-
    // participant disconnect events back to `compositor.rs`'s own state, so
    // this has to be done explicitly rather than relying on
    // `subscriber::start_compositor_feed`'s event loop to notice.
    crate::compositor::remove_all_windows(app);
    // Petal View selectors are meeting-scoped too: close them so no hollow
    // window outlives the session (each close stops its share + unregisters).
    crate::region_window::close_all_region_windows(app).await;

    // Stop the published webcam, if on. BEFORE Room::close(): the
    // AVCaptureSession (and its green camera light) is
    // ours to stop, not LiveKit's -- closing the room alone would keep the
    // camera capturing into a dead track.
    stop_camera_publish(state).await;

    let (joined, mic, playout) = {
        let mut guard = state.inner.lock_unpoisoned();
        (guard.joined.take(), guard.mic.take(), guard.playout.take())
    };
    // Explicitly unmute/unpublish isn't needed -- `Room::close()` below tears
    // down every locally published track along with the room. `mic`/
    // `playout` are dropped here (releasing the platform ADM's recording/
    // playout handles) purely so their `Drop` runs promptly rather than
    // lingering as long as `SessionInner` itself.
    drop(mic);
    drop(playout);
    if let Some(joined) = joined {
        if close_room_connection {
            if let Err(e) = joined.room_connection.room().close().await {
                log::warn!("session: error closing room on {reason}: {e}");
            }
        }
        log::info!(
            "session: left room '{}' via {reason}",
            crate::logging::log_safe_quoted(&joined.room_record.name)
        );
        crate::analytics::meeting_left();
        // Main window is never hidden on join anymore (it hosts the
        // in-meeting UI), so there's nothing to re-show here.

        // Tell the frontend the room was left (issue #5): the meeting route
        // navigates back to /main and the menubar popover clears its roster.
        // Emitted from HERE (not from each leave trigger) so every leave
        // path -- the popover's Leave button, the menubar pill's leave
        // circle, and joining a different room -- shares one seam. Global
        // `emit` (not `emit_to("main", ...)`) deliberately: the popover
        // webview needs it too, same broadcast pattern as `presence-update`.
        let _ = tauri::Emitter::emit(
            app,
            "room-left",
            RoomLeftEvent {
                room_name: joined.room_record.name.clone(),
            },
        );

        // Reset the menubar pill to its not-in-meeting rendering (issue #4).
        // Belt-and-suspenders with presence.rs's own Disconnected-event
        // update: our own Room::close() should fire RoomEvent::Disconnected,
        // but the pill must not stay green if that event is ever missed.
        crate::menubar::update_meeting_state(app, false, 0);
    }
}

/// Payload of the `room-left` event (see `leave_room`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomLeftEvent {
    pub room_name: String,
}

#[cfg(test)]
mod tests {
    use super::{
        await_join_work, await_join_work_with_cleanup, microphone_publish_disposition,
        playout_retry_backoff, session_commit_is_current, should_retry_playout, AudioJoinFailure,
        JoinBudget, JoinTailStage, JoinWorkWithCleanup, MicrophonePublishDisposition,
        PlayoutEnableFailure, JOIN_CLEANUP_RESERVE, JOIN_TAIL_STAGE_ORDER, JOIN_TERMINAL_BUDGET,
        PLAYOUT_ENABLE_ATTEMPTS, PLAYOUT_RETRY_BACKOFF,
    };
    use crate::meeting_core::{
        learned_display_name, persist_joined_room_record, room_record_with_learned_display_name,
    };
    use crate::rooms::RoomsState;
    use crate::session::{SessionState, ShareSessionError};
    use crate::time_util::now_ms;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// The leave→rejoin publish carryover (the B1 fix's source of truth) is
    /// room-scoped and one-shot: rejoining the SAME room consumes the
    /// recorded intent exactly once; joining a DIFFERENT room discards it so
    /// the camera/window shares can never leak into an audience that never
    /// saw them.
    #[test]
    fn leave_publish_carryover_is_room_scoped_and_one_shot() {
        let frame = crate::hover_tab::WindowFrame {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        };

        // Joining a DIFFERENT room discards (and still clears) the snapshot.
        let state = SessionState::default();
        state.record_leave_publish_carryover("room-a".to_string(), true, vec![(157, frame)]);
        let other = state.take_leave_publish_carryover("room-b");
        assert!(!other.camera_on);
        assert!(other.shares.is_empty());
        let after_discard = state.take_leave_publish_carryover("room-a");
        assert!(
            !after_discard.camera_on && after_discard.shares.is_empty(),
            "a discarded carryover must not linger for a later same-room join"
        );

        // Rejoining the SAME room consumes it exactly once.
        state.record_leave_publish_carryover("room-a".to_string(), true, vec![(157, frame)]);
        let plan = state.take_leave_publish_carryover("room-a");
        assert!(plan.camera_on);
        assert_eq!(plan.shares.len(), 1);
        assert_eq!(plan.shares[0].0, 157);
        let second = state.take_leave_publish_carryover("room-a");
        assert!(
            !second.camera_on && second.shares.is_empty(),
            "the carryover is one-shot"
        );
    }

    struct DropSpy(Arc<AtomicUsize>);

    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "petal-session-room-test-{}-{}-{:?}",
            std::process::id(),
            now_ms(),
            std::thread::current().id()
        ))
    }

    fn wait_until_after_ms(timestamp_ms: u64) {
        while now_ms() <= timestamp_ms {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn issue569_connect_timeout_drops_pending_work_before_cleanup_reserve() {
        let budget = JoinBudget::start();
        let drops = Arc::new(AtomicUsize::new(0));
        let drops_for_future = drops.clone();
        let task = tokio::spawn(async move {
            await_join_work(budget, async move {
                let _drop_spy = DropSpy(drops_for_future);
                std::future::pending::<()>().await;
            })
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(JOIN_TERMINAL_BUDGET - JOIN_CLEANUP_RESERVE).await;

        assert_eq!(task.await.expect("join budget task"), Err(()));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn issue569_publication_timeout_runs_cleanup_inside_final_reserve() {
        let budget = JoinBudget::start();
        let publish_drops = Arc::new(AtomicUsize::new(0));
        let cleanup_runs = Arc::new(AtomicUsize::new(0));
        let publish_drops_for_future = publish_drops.clone();
        let cleanup_runs_for_future = cleanup_runs.clone();
        let task = tokio::spawn(async move {
            await_join_work_with_cleanup(
                budget,
                async move {
                    let _drop_spy = DropSpy(publish_drops_for_future);
                    std::future::pending::<()>().await;
                },
                || {},
                async move {
                    cleanup_runs_for_future.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(JOIN_TERMINAL_BUDGET - JOIN_CLEANUP_RESERVE).await;

        assert!(matches!(
            task.await.expect("publication budget task"),
            JoinWorkWithCleanup::TimedOut(Some(()))
        ));
        assert_eq!(publish_drops.load(Ordering::SeqCst), 1);
        assert_eq!(cleanup_runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn issue569_microphone_timeout_joins_without_audio_at_terminal_deadline() {
        let budget = JoinBudget::start();
        let publish_drops = Arc::new(AtomicUsize::new(0));
        let mute_runs = Arc::new(AtomicUsize::new(0));
        let cleanup_runs = Arc::new(AtomicUsize::new(0));
        let publish_drops_for_future = publish_drops.clone();
        let mute_runs_for_cleanup = mute_runs.clone();
        let cleanup_runs_for_future = cleanup_runs.clone();
        let task = tokio::spawn(async move {
            await_join_work_with_cleanup(
                budget,
                async move {
                    let _drop_spy = DropSpy(publish_drops_for_future);
                    std::future::pending::<()>().await;
                },
                move || {
                    mute_runs_for_cleanup.fetch_add(1, Ordering::SeqCst);
                },
                async move {
                    cleanup_runs_for_future.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                },
            )
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(JOIN_TERMINAL_BUDGET - JOIN_CLEANUP_RESERVE).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "cleanup owns the final reserve");

        tokio::time::advance(JOIN_CLEANUP_RESERVE).await;
        assert!(matches!(
            task.await.expect("microphone publication budget task"),
            JoinWorkWithCleanup::TimedOut(None)
        ));
        assert_eq!(publish_drops.load(Ordering::SeqCst), 1);
        assert_eq!(mute_runs.load(Ordering::SeqCst), 1);
        assert_eq!(cleanup_runs.load(Ordering::SeqCst), 1);
        assert_eq!(
            microphone_publish_disposition(false, true),
            MicrophonePublishDisposition::ContinueWithoutAudio
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue569_palette_timeout_is_nonfatal_without_late_commit() {
        let budget = JoinBudget::start();
        let drops = Arc::new(AtomicUsize::new(0));
        let drops_for_future = drops.clone();
        let task = tokio::spawn(async move {
            await_join_work(budget, async move {
                let _drop_spy = DropSpy(drops_for_future);
                std::future::pending::<Option<u8>>().await
            })
            .await
            .ok()
        });

        tokio::task::yield_now().await;
        tokio::time::advance(JOIN_TERMINAL_BUDGET - JOIN_CLEANUP_RESERVE).await;

        assert_eq!(task.await.expect("palette budget task"), None);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn issue569_preparation_timeout_detaches_blocking_raii_work() {
        let budget = JoinBudget::start();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (dropped_tx, mut dropped_rx) = tokio::sync::oneshot::channel();
        let prepare_task = tokio::task::spawn_blocking(move || {
            let _drop_signal = DropSignal(Some(dropped_tx));
            let _ = release_rx.recv();
        });
        let task = tokio::spawn(await_join_work(budget, prepare_task));

        tokio::task::yield_now().await;
        tokio::time::advance(JOIN_TERMINAL_BUDGET - JOIN_CLEANUP_RESERVE).await;

        assert!(matches!(task.await.expect("prepare budget task"), Err(())));
        assert!(matches!(
            dropped_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        release_tx.send(()).expect("release detached preparation");
        dropped_rx
            .await
            .expect("prepared resources eventually drop");
    }

    #[test]
    fn issue569_join_timeout_has_stable_serialized_terminal_kind() {
        let value = serde_json::to_value(ShareSessionError::JoinTimeout)
            .expect("join timeout should serialize");
        assert_eq!(value["kind"], "joinTimeout");
    }

    // ------------------------------------------------------------------
    // #787: the join path's audio failures.
    // ------------------------------------------------------------------

    #[test]
    fn playout_enable_retries_exactly_twice_after_its_first_attempt() {
        // The bug this defends against costs the user every remote voice for
        // the whole meeting, so one attempt is not enough; but the sequence
        // has to fit inside the join budget, so it is not unbounded either.
        assert!(should_retry_playout(1, PLAYOUT_ENABLE_ATTEMPTS));
        assert!(should_retry_playout(2, PLAYOUT_ENABLE_ATTEMPTS));
        assert!(!should_retry_playout(3, PLAYOUT_ENABLE_ATTEMPTS));
        assert!(!should_retry_playout(4, PLAYOUT_ENABLE_ATTEMPTS));
        // A single-attempt budget must never retry -- guards against a future
        // edit to the constant silently turning the loop into a no-retry or
        // an infinite one.
        assert!(!should_retry_playout(1, 1));
    }

    #[test]
    fn playout_backoff_starts_immediately_then_grows_linearly() {
        assert_eq!(playout_retry_backoff(1), Duration::ZERO);
        assert_eq!(playout_retry_backoff(2), PLAYOUT_RETRY_BACKOFF);
        assert_eq!(playout_retry_backoff(3), PLAYOUT_RETRY_BACKOFF * 2);
        // Whole retry sequence must stay small next to the 30s join budget.
        let total: Duration = (1..=PLAYOUT_ENABLE_ATTEMPTS)
            .map(playout_retry_backoff)
            .sum();
        assert!(
            total < Duration::from_secs(1),
            "retry back-off {total:?} must not eat the join budget"
        );
    }

    #[test]
    fn a_timed_out_playout_reports_differently_from_a_failed_one() {
        assert_eq!(
            PlayoutEnableFailure::TimedOut.detail(),
            "exhausted the join budget"
        );
        assert_eq!(
            PlayoutEnableFailure::Failed("no such device".to_string()).detail(),
            "no such device"
        );
    }

    /// The user-facing half of #787: these strings are the only thing that
    /// tells someone their meeting has no audio. Pin the copy, and pin that
    /// it rides the two `resilience-event` kinds `ToastHost.svelte` already
    /// renders -- a typo'd `kind` is a silently dead toast (the #22 lesson).
    #[test]
    fn audio_join_failures_reach_the_user_on_an_existing_toast_channel() {
        let mic = serde_json::to_value(
            AudioJoinFailure::Microphone.event(AudioJoinFailure::Microphone.notice().to_string()),
        )
        .expect("mic failure event serializes");
        assert_eq!(mic["kind"], "micDeviceFailed");
        assert_eq!(
            mic["message"],
            "Microphone unavailable — you can't be heard"
        );

        let speaker = serde_json::to_value(
            AudioJoinFailure::Playout.event(AudioJoinFailure::Playout.notice().to_string()),
        )
        .expect("playout failure event serializes");
        assert_eq!(speaker["kind"], "speakerDeviceFailed");
        assert_eq!(
            speaker["message"],
            "Speaker unavailable — you can't hear others"
        );
    }

    /// CLAUDE.md: UI text must never truncate. These ride the same toast as
    /// the shipped `"Microphone disconnected — check input device"` (43
    /// chars), so stay within that established budget rather than trusting
    /// the toast's overflow defense.
    #[test]
    fn audio_failure_notices_stay_inside_the_shipped_toast_length_budget() {
        const SHIPPED_REFERENCE: &str = "Microphone disconnected — check input device";
        for failure in [AudioJoinFailure::Microphone, AudioJoinFailure::Playout] {
            let notice = failure.notice();
            assert!(
                !notice.is_empty(),
                "a failure with no message tells the user nothing"
            );
            assert!(
                notice.chars().count() <= SHIPPED_REFERENCE.chars().count(),
                "{notice:?} ({} chars) is longer than the shipped toast copy it sits beside",
                notice.chars().count()
            );
        }
    }

    #[test]
    fn issue569_stale_completion_cannot_commit_microphone_or_playout() {
        assert!(session_commit_is_current(true, true));
        assert!(!session_commit_is_current(false, true));
        assert!(!session_commit_is_current(true, false));
        assert!(!session_commit_is_current(false, false));
        assert_eq!(
            microphone_publish_disposition(true, false),
            MicrophonePublishDisposition::Superseded
        );
        assert_eq!(
            microphone_publish_disposition(true, true),
            MicrophonePublishDisposition::Commit
        );
    }

    #[derive(Default)]
    struct RemoteControlJoinModel {
        joined_room_id: Option<&'static str>,
        remote_control_allowed: bool,
    }

    impl RemoteControlJoinModel {
        fn join_room(&mut self, room_id: &'static str, remote_control_allowed: bool) {
            if self.joined_room_id == Some(room_id) {
                return;
            }

            self.joined_room_id = Some(room_id);
            self.remote_control_allowed = remote_control_allowed;
        }

        fn set_remote_control_allowed(&mut self, allowed: bool) {
            self.remote_control_allowed = allowed;
        }
    }

    #[test]
    fn same_room_rejoin_does_not_reapply_remote_control_allowed() {
        let mut model = RemoteControlJoinModel::default();

        model.join_room("room-a", true);
        assert!(model.remote_control_allowed);

        model.set_remote_control_allowed(false);
        model.join_room("room-a", true);

        assert!(!model.remote_control_allowed);
    }

    #[test]
    fn join_tail_terminal_marker_follows_joined_room_log() {
        let source = include_str!("room.rs");
        let production_source = source
            .split_once("\n#[cfg(test)]")
            .map(|(production, _)| production)
            .expect("room source must keep tests after production code");
        let joined_log = production_source
            .find("session: joined room")
            .expect("joined-room log must remain observable");
        let terminal_marker = production_source
            .find("log_join_tail_stage(JoinTailStage::JoinedTerminal")
            .expect("terminal marker must remain observable");
        let successful_return = production_source
            .find("Ok(room_record)")
            .expect("successful room join must return its record");

        assert_eq!(
            JOIN_TAIL_STAGE_ORDER.last(),
            Some(&JoinTailStage::JoinedTerminal),
            "the stage registry must retain the terminal label"
        );
        assert!(
            terminal_marker > joined_log,
            "the terminal marker must be emitted after the existing joined-room log"
        );
        assert!(
            successful_return > terminal_marker,
            "the terminal marker must be emitted before the successful return"
        );
    }

    #[test]
    fn learned_display_name_reads_contract_room_metadata_when_local_label_is_empty() {
        let contracts: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../contracts/petal-contracts.json"
        )))
        .expect("contract fixtures should parse");
        let metadata = contracts["roomMetadataRegistration"]["metadata"]
            .as_str()
            .expect("room metadata fixture should be a string");

        assert_eq!(
            learned_display_name(metadata, None).as_deref(),
            Some("Eng meeting")
        );
    }

    #[test]
    fn learned_display_name_only_fills_empty_or_generic_local_labels() {
        let metadata = r#"{"displayName":"Design sync","open":false}"#;

        assert_eq!(
            learned_display_name(metadata, Some("")).as_deref(),
            Some("Design sync")
        );
        assert_eq!(
            learned_display_name(metadata, Some("  room  ")).as_deref(),
            Some("Design sync")
        );
        assert_eq!(learned_display_name(metadata, Some("Local name")), None);
    }

    #[test]
    fn learned_display_name_ignores_missing_empty_invalid_or_generic_metadata() {
        assert_eq!(learned_display_name("", None), None);
        assert_eq!(learned_display_name("not-json", None), None);
        assert_eq!(learned_display_name(r#"{"open":false}"#, None), None);
        assert_eq!(
            learned_display_name(r#"{"displayName":"   ","open":false}"#, None),
            None
        );
        assert_eq!(
            learned_display_name(r#"{"displayName":"room","open":false}"#, None),
            None
        );
    }

    #[test]
    fn learned_display_name_persistence_returns_updated_record_and_keeps_existing_labels() {
        let dir = temp_dir();
        let rooms = RoomsState::load(dir.clone());
        let created = rooms.create("abc-defg-hjk", true).unwrap();
        let metadata = r#"{"displayName":"Design sync","open":false}"#;

        let learned = room_record_with_learned_display_name(&rooms, created.clone(), metadata);

        assert_eq!(learned.id, created.id);
        assert_eq!(learned.display_name.as_deref(), Some("Design sync"));
        assert_eq!(
            rooms
                .find(&created.name)
                .expect("renamed room should still exist")
                .display_name
                .as_deref(),
            Some("Design sync")
        );

        let preserved = room_record_with_learned_display_name(
            &rooms,
            learned.clone(),
            r#"{"displayName":"Server rename","open":false}"#,
        );
        assert_eq!(preserved.display_name.as_deref(), Some("Design sync"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn join_room_record_resolution_bumps_existing_room_recency() {
        let dir = temp_dir();
        let rooms = RoomsState::load(dir.clone());
        let first = rooms.create("abc-defg-hjk", true).unwrap();
        let first_joined_ms = first.last_joined_ms.expect("first join timestamp");

        wait_until_after_ms(first_joined_ms);

        let joined = persist_joined_room_record(&rooms, "abc-defg-hjk").unwrap();

        assert_eq!(first.id, joined.id);
        assert!(
            joined
                .last_joined_ms
                .expect("rejoin timestamp should be persisted")
                > first_joined_ms,
            "joining an existing room must bump last_joined_ms"
        );
        assert_eq!(rooms.list().len(), 1);

        let reloaded = RoomsState::load(dir.clone());
        let reloaded_rooms = reloaded.list();
        assert_eq!(reloaded_rooms.len(), 1);
        assert_eq!(reloaded_rooms[0].id, joined.id);
        assert_eq!(reloaded_rooms[0].last_joined_ms, joined.last_joined_ms);

        let _ = std::fs::remove_dir_all(dir);
    }
}
