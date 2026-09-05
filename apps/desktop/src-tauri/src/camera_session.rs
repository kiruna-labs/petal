//! Shared camera session orchestration: capture start,
//! publish pump + telemetry, loss monitoring, the bounded self-heal loop,
//! reconnect publication repair (macOS), and every camera Tauri command.
//! Platform-neutral — the platform differences live in the `SessionState`
//! methods it calls (see `session_stub.rs` / `session/mod.rs`) and in
//! `transport::camera`.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::Manager;

use crate::logging::{
    CadenceBucket, CameraDirection, CameraHealthDiagnostic, DecoderRenderHealth, DiagnosticRole,
    QueueBackpressureBucket, SentryDiagnosticEvent,
};
use crate::room_generation::RoomGeneration;
use crate::sync_ext::MutexExt;
use crate::transport::camera::{
    CameraBackend, CameraDeviceInfo, CameraDevicePreferences, CameraFrame, CameraMode,
    CameraStatus, PreferredCameraMode,
};
use crate::transport::publisher::PublishedTrack;

/// A running camera publish: the capture backend + its status handle, the
/// published track, and the two task handles that must be aborted on teardown
/// (the frame pump and the terminal-error loss monitor). Exactly zero or one
/// per session; toggle-driven from the meeting route's Video control.
pub(crate) struct ActiveCamera {
    pub(crate) capture: Box<dyn CameraBackend>,
    pub(crate) status: CameraStatus,
    pub(crate) published: Arc<PublishedTrack>,
    pub(crate) pump: tauri::async_runtime::JoinHandle<()>,
    pub(crate) loss_monitor: tauri::async_runtime::JoinHandle<()>,
}

const CAMERA_TELEMETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Bounded wait for the capture's first delivered frame. `start` can succeed
/// while the device never delivers a frame (held by another process, a
/// Continuity Camera whose phone never starts streaming, ...) — live-observed
/// 2026-07-30: four consecutive `start_camera_publish` attempts reached
/// "capture running" and no frame ever arrived. The wait must end in a REAL
/// terminal error, never a hang.
const CAMERA_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Self-heal backoff for a publish attempt that failed while the user's
/// camera intent is still ON: how long to wait before each retry. Bounded —
/// after the last retry fails the heal loop clears the intent and emits a
/// terminal `camera-publish-state` event so the UI can surface a real error
/// with a working retry affordance instead of a toggle that lies.
pub(crate) const CAMERA_HEAL_RETRY_BACKOFF: &[Duration] =
    &[Duration::from_secs(2), Duration::from_secs(4)];

/// Rejoin-reconcile schedule: attempt immediately, then the standard backoff.
pub(crate) const CAMERA_REJOIN_ATTEMPT_SCHEDULE: &[Duration] = &[
    Duration::ZERO,
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// The first H.264 frame is the decoder bootstrap keyframe. Give existing
/// room peers time to negotiate a replacement publication before sending it;
/// otherwise a camera restart/switch can leave them receiving only
/// undecodable delta frames.
const CAMERA_SUBSCRIBER_NEGOTIATION_DELAY: Duration = Duration::from_millis(500);

/// How often the loss monitor polls the capture's terminal error.
const CAMERA_LOSS_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Default)]
struct CameraPublishTelemetry {
    captured_frames: u64,
    pushed_frames: u64,
    dropped_push_frames: u64,
    overwritten_latest_frames: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct CameraPublishBaseline {
    captured_frames: u64,
    pushed_frames: u64,
    dropped_push_frames: u64,
    overwritten_latest_frames: u64,
}

impl From<&CameraPublishTelemetry> for CameraPublishBaseline {
    fn from(value: &CameraPublishTelemetry) -> Self {
        Self {
            captured_frames: value.captured_frames,
            pushed_frames: value.pushed_frames,
            dropped_push_frames: value.dropped_push_frames,
            overwritten_latest_frames: value.overwritten_latest_frames,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CameraPublishHealth {
    captured_frames: u64,
    pushed_frames: u64,
    dropped_push_frames: u64,
    overwritten_latest_frames: u64,
    capture_fps: f64,
    encode_fps: f64,
}

fn format_camera_publish_health(health: &CameraPublishHealth) -> String {
    format!(
        "session: camera publish health -- captured={} pushed={} dropped_push={} overwritten_latest={} capture_fps={:.1} encode_fps={:.1}",
        health.captured_frames,
        health.pushed_frames,
        health.dropped_push_frames,
        health.overwritten_latest_frames,
        health.capture_fps,
        health.encode_fps
    )
}

fn camera_publish_interval_health(
    telemetry: &CameraPublishTelemetry,
    baseline: CameraPublishBaseline,
    elapsed: f64,
) -> (CameraPublishHealth, u64, u64) {
    let capture_fps = if elapsed > 0.0 {
        telemetry
            .captured_frames
            .saturating_sub(baseline.captured_frames) as f64
            / elapsed
    } else {
        0.0
    };
    let encode_fps = if elapsed > 0.0 {
        telemetry
            .pushed_frames
            .saturating_sub(baseline.pushed_frames) as f64
            / elapsed
    } else {
        0.0
    };
    (
        CameraPublishHealth {
            captured_frames: telemetry.captured_frames,
            pushed_frames: telemetry.pushed_frames,
            dropped_push_frames: telemetry.dropped_push_frames,
            overwritten_latest_frames: telemetry.overwritten_latest_frames,
            capture_fps,
            encode_fps,
        },
        telemetry
            .dropped_push_frames
            .saturating_sub(baseline.dropped_push_frames),
        telemetry
            .overwritten_latest_frames
            .saturating_sub(baseline.overwritten_latest_frames),
    )
}

fn camera_cadence_bucket(fps: f64) -> CadenceBucket {
    if !fps.is_finite() || fps < 0.0 {
        CadenceBucket::Unknown
    } else if fps >= 24.0 {
        CadenceBucket::Healthy
    } else if fps >= 10.0 {
        CadenceBucket::Reduced
    } else if fps > 0.0 {
        CadenceBucket::Severe
    } else {
        CadenceBucket::Stalled
    }
}

fn camera_queue_backpressure_bucket(
    dropped_push_frames: u64,
    overwritten_latest_frames: u64,
) -> QueueBackpressureBucket {
    match dropped_push_frames.saturating_add(overwritten_latest_frames) {
        0 => QueueBackpressureBucket::None,
        1..=2 => QueueBackpressureBucket::Low,
        3..=10 => QueueBackpressureBucket::High,
        _ => QueueBackpressureBucket::Saturated,
    }
}

fn unhealthy_camera_publish_diagnostic(
    health: &CameraPublishHealth,
    dropped_push_frames: u64,
    overwritten_latest_frames: u64,
) -> Option<SentryDiagnosticEvent> {
    let capture_cadence = camera_cadence_bucket(health.capture_fps);
    let encode_cadence = camera_cadence_bucket(health.encode_fps);
    let queue_backpressure =
        camera_queue_backpressure_bucket(dropped_push_frames, overwritten_latest_frames);
    if capture_cadence == CadenceBucket::Healthy
        && encode_cadence == CadenceBucket::Healthy
        && matches!(
            queue_backpressure,
            QueueBackpressureBucket::None | QueueBackpressureBucket::Low
        )
    {
        return None;
    }
    Some(SentryDiagnosticEvent::CameraHealth(
        CameraHealthDiagnostic {
            role: DiagnosticRole::Sharer,
            direction: CameraDirection::Publish,
            capture_cadence,
            encode_cadence,
            queue_backpressure,
            decoder_render: DecoderRenderHealth::NotApplicable,
        },
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CameraPublishOutcome {
    pub used_default_fallback: bool,
}

/// Payload of the `camera-publish-state` event: pushed whenever the camera
/// self-heal loop reaches a terminal outcome so every UI surface (meeting
/// route, menubar popover) can reflect the REAL publish state instead of a
/// stale local toggle.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CameraPublishStateEvent {
    /// Whether the camera track is now publishing.
    pub publishing: bool,
    /// Terminal error after the bounded self-heal retries were exhausted.
    /// `None` when `publishing` is true.
    pub error: Option<String>,
}

/// Result of `start_camera_publish_command`. `published: false` means the
/// immediate attempt failed but the bounded self-heal loop is retrying in
/// the background; the terminal outcome arrives as a `camera-publish-state`
/// event (see [`ensure_camera_published`]).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartCameraPublishResult {
    pub published: bool,
}

/// Snapshot for UI surfaces syncing their camera toggle to reality (e.g. the
/// meeting route re-mounting after a leave→rejoin).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraPublishStateSnapshot {
    /// A camera track is currently publishing.
    pub publishing: bool,
    /// The user's intended camera state (ON even while a publish attempt or
    /// self-heal retry is still in flight).
    pub intended: bool,
}

/// Terminal outcome of one camera self-heal loop.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CameraHealOutcome {
    /// The camera is publishing (either an attempt succeeded, or another
    /// path published it while we were waiting).
    Published,
    /// The room was left/superseded or the user turned the camera off.
    Cancelled,
    /// Every attempt failed; the camera intent has been CLEARED so no
    /// surface keeps claiming ON for a publish that will never happen.
    TerminalFailure(String),
}

/// A running camera capture that can be synchronously stopped + released on
/// a blocking thread. The one production impl is the boxed real backend
/// (whose `Drop` runs the platform teardown); tests substitute a fake to
/// prove the timeout path of [`await_first_frame_or_release`] actually
/// releases before erroring.
pub(super) trait ReleasableCapture: Send + 'static {
    fn release_blocking(self);
}

impl ReleasableCapture for Box<dyn CameraBackend> {
    fn release_blocking(self) {
        drop(self);
    }
}

/// Wait (bounded) for the first captured frame's dimensions. On success the
/// capture is handed back for publishing; on timeout or a disconnected
/// capture the capture is RELEASED before the error is returned — a retry
/// that reuses (or leaks) a wedged capture is exactly the failure mode this
/// exists to prevent.
pub(super) async fn await_first_frame_or_release<C: ReleasableCapture>(
    capture: C,
    size_rx: std::sync::mpsc::Receiver<(u32, u32)>,
    timeout: Duration,
) -> Result<(C, (u32, u32)), String> {
    let timeout_secs = timeout.as_secs_f64();
    let size_result = tokio::task::spawn_blocking(move || size_rx.recv_timeout(timeout))
        .await
        .map_err(|e| format!("camera size wait panicked: {e}"));
    match size_result.and_then(|inner| {
        inner.map_err(|_| {
            format!(
                "no camera frames within {timeout_secs:.0}s (camera held by another app, or the device never started streaming?)"
            )
        })
    }) {
        Ok(size) => Ok((capture, size)),
        Err(e) => {
            // Stop + release the capture we just started before bailing.
            // Platform teardown blocks — run on a blocking thread.
            if tokio::task::spawn_blocking(move || capture.release_blocking())
                .await
                .is_err()
            {
                log::error!(
                    "session: camera capture release task panicked after first-frame timeout"
                );
            }
            Err(e)
        }
    }
}

/// The Windows gate: the user still wants the camera AND the room generation
/// is still current. Anything else means a mid-start leave/toggle must tear
/// the just-opened capture down instead of publishing it.
fn camera_start_is_current(
    state: &crate::session::SessionState,
    generation: &RoomGeneration,
) -> bool {
    state.camera_intent() && generation.is_current()
}

/// Start capturing + publishing the local webcam as
/// `petal-camera-<identity-slug>` (no-op if
/// already publishing); requires a joined room + camera intent.
/// Step-bracketed logging per the issue #13 convention.
///
/// Callers must hold `state.lock_camera_control()` (the commands and the
/// self-heal attempt do) — this fn serializes the intent check, capture
/// open, publish, and store against device switches.
pub(crate) async fn start_camera_publish_with_device(
    app: &tauri::AppHandle,
    state: &crate::session::SessionState,
    preferred_device_id: Option<String>,
    preferred_mode: Option<PreferredCameraMode>,
) -> Result<CameraPublishOutcome, String> {
    let (room_connection, identity, generation) = {
        if !state.camera_intent() {
            return Err("camera start cancelled".to_string());
        }
        if state.camera_publishing() {
            log::info!("session: start_camera_publish -- already publishing, no-op");
            return Ok(CameraPublishOutcome::default());
        }
        let Some((room_connection, identity)) = state.control_channel_snapshot() else {
            return Err("not in room".to_string());
        };
        (
            room_connection,
            identity,
            state.current_room_generation(),
        )
    };
    log::info!(
        "session: start_camera_publish begin (identity '{}')",
        crate::logging::log_safe_quoted(&identity)
    );

    // Frames flow: capture callback -> latest-wins single slot -> tokio pump
    // (NV12->I420 convert + LiveKit push). This bounds queued camera data to
    // one full NV12 frame under encoder backpressure.
    let latest_frame: Arc<Mutex<Option<CameraFrame>>> = Arc::new(Mutex::new(None));
    let frame_ready = Arc::new(tokio::sync::Notify::new());
    let first_frame_sent = Arc::new(AtomicBool::new(false));
    let (first_frame_tx, first_frame_rx) = std::sync::mpsc::sync_channel(1);
    let telemetry = Arc::new(Mutex::new(CameraPublishTelemetry::default()));

    // The capture open blocks for hundreds of ms — spawn_blocking, never
    // directly on the runtime.
    let callback_latest_frame = latest_frame.clone();
    let callback_frame_ready = frame_ready.clone();
    let callback_first_frame_sent = first_frame_sent.clone();
    let callback_first_frame_tx = first_frame_tx;
    let callback_telemetry = telemetry.clone();
    let start_task = tokio::task::spawn_blocking(move || {
        crate::transport::camera::open_camera(
            preferred_device_id.as_deref(),
            preferred_mode,
            move |frame| {
                #[cfg(target_os = "windows")]
                crate::camera_self_view::feed_frame(&frame);
                let size = (frame.width, frame.height);
                let is_first = !callback_first_frame_sent.swap(true, Ordering::SeqCst);
                {
                    let mut telemetry = callback_telemetry.lock_unpoisoned();
                    telemetry.captured_frames = telemetry.captured_frames.saturating_add(1);
                }
                {
                    let mut latest = callback_latest_frame.lock_unpoisoned();
                    if latest.is_some() {
                        let mut telemetry = callback_telemetry.lock_unpoisoned();
                        telemetry.overwritten_latest_frames =
                            telemetry.overwritten_latest_frames.saturating_add(1);
                    }
                    *latest = Some(frame);
                }
                callback_frame_ready.notify_one();
                if is_first {
                    let _ = callback_first_frame_tx.try_send(size);
                }
            },
        )
    });
    let capture = start_task
        .await
        .map_err(|error| format!("camera capture task failed: {error}"))?
        .map_err(|error| error.to_string())?;
    let status = capture.status_handle();
    log::info!("session: start_camera_publish capture running, waiting for first frame");

    // First frame carries the real dimensions the track must be created at.
    // Bounded wait; on timeout the capture is FULLY released (teardown on a
    // blocking thread) before the error returns, so the next attempt starts
    // from a clean capture. A platform terminal error (e.g. Media Foundation
    // stream failure) is preferred over the generic timeout message.
    let (capture, (width, height)) =
        match await_first_frame_or_release(capture, first_frame_rx, CAMERA_FIRST_FRAME_TIMEOUT)
            .await
        {
            Ok(v) => v,
            Err(error) => {
                let error = status.terminal_error().unwrap_or(error);
                // Terminal error in the NATIVE log too — the live 2026-07-30
                // incident's silent 5s hangs were only diagnosable because
                // the error eventually reached petal.log; the wait alone is
                // not enough.
                log::warn!("session: start_camera_publish failed: {error}");
                return Err(error);
            }
        };
    if !camera_start_is_current(state, &generation) {
        drop_camera_capture(capture).await;
        return Err("camera start cancelled".to_string());
    }

    // The encoding ceiling scales with the negotiated capture frame rate
    // (camera_publish_options), so publish with the fps the capture actually
    // negotiated rather than a hardcoded 30.
    let (frame_rate_num, frame_rate_den) = capture.frame_rate();
    let negotiated_frame_rate = frame_rate_num as f64 / frame_rate_den.max(1) as f64;
    let published = match room_connection
        .publish_camera(width, height, negotiated_frame_rate, &identity)
        .await
    {
        Ok(published) => Arc::new(published),
        Err(error) => {
            log::warn!("session: start_camera_publish track publish failed: {error}");
            drop_camera_capture(capture).await;
            return Err(error.to_string());
        }
    };

    tokio::time::sleep(CAMERA_SUBSCRIBER_NEGOTIATION_DELAY).await;

    let pump = start_camera_frame_pump(latest_frame, frame_ready, published.clone(), telemetry);
    let loss_monitor = start_camera_loss_monitor(app.clone(), generation.clone(), status.clone());
    let used_default_fallback = capture.used_default_fallback();
    let mut camera = Some(ActiveCamera {
        capture,
        status,
        published: published.clone(),
        pump,
        loss_monitor,
    });

    // Store — unless the room was left/superseded or a newer camera raced on
    // while we were awaiting (leave_room can't see this in-flight camera), in
    // which case tear THIS one down instead of leaking a running capture into
    // a closed room. The platform `put_active_camera` re-checks joined-ness
    // under its own lock (atomically with the store) and only takes the
    // camera on success — on rejection ownership stays here for teardown.
    let stored = if !generation.is_current() || state.camera_publishing() {
        false
    } else {
        state.put_active_camera(&mut camera)
    };
    if !stored {
        log::warn!(
            "session: start_camera_publish -- room left (or camera raced on) mid-start; tearing down"
        );
        teardown_active_camera(camera.expect("camera not stored"), true).await;
        return Err("not in room".to_string());
    }
    log::info!("session: start_camera_publish succeeded ({width}x{height})");
    Ok(CameraPublishOutcome {
        used_default_fallback,
    })
}

/// Latest-wins frame pump: take the newest captured frame and push it into
/// the published track, with a per-interval telemetry log (captured/pushed/
/// dropped/overwritten cadence) and an unhealthy-interval Sentry diagnostic.
fn start_camera_frame_pump(
    latest_frame: Arc<Mutex<Option<CameraFrame>>>,
    frame_ready: Arc<tokio::sync::Notify>,
    published: Arc<PublishedTrack>,
    telemetry: Arc<Mutex<CameraPublishTelemetry>>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut health_tick = tokio::time::interval(CAMERA_TELEMETRY_INTERVAL);
        health_tick.tick().await;
        let mut last_log_at = Instant::now();
        let mut health_baseline = CameraPublishBaseline::from(&*telemetry.lock_unpoisoned());
        loop {
            tokio::select! {
                _ = health_tick.tick() => {
                    let now = Instant::now();
                    let elapsed = now.duration_since(last_log_at).as_secs_f64();
                    let snapshot = telemetry.lock_unpoisoned();
                    let current_baseline = CameraPublishBaseline::from(&*snapshot);
                    let (health, dropped_push_frames, overwritten_latest_frames) =
                        camera_publish_interval_health(&snapshot, health_baseline, elapsed);
                    drop(snapshot);
                    log::info!("{}", format_camera_publish_health(&health));
                    if let Some(event) = unhealthy_camera_publish_diagnostic(
                        &health,
                        dropped_push_frames,
                        overwritten_latest_frames,
                    ) {
                        crate::logging::capture_sentry_diagnostic(event);
                    }
                    last_log_at = now;
                    health_baseline = current_baseline;
                }
                _ = frame_ready.notified() => {
                    let Some(frame) = latest_frame.lock_unpoisoned().take() else {
                        continue;
                    };
                    let pushed = published
                        .push_nv12(
                            &frame.y,
                            frame.y_stride,
                            &frame.uv,
                            frame.uv_stride,
                            frame.width,
                            frame.height,
                            frame.capture_wall_time_us,
                        )
                        .is_some();
                    let mut telemetry = telemetry.lock_unpoisoned();
                    if pushed {
                        telemetry.pushed_frames = telemetry.pushed_frames.saturating_add(1);
                    } else {
                        telemetry.dropped_push_frames =
                            telemetry.dropped_push_frames.saturating_add(1);
                    }
                }
            }
        }
    })
}

/// Terminal-capture watchdog: stops the publish when the capture reports a
/// terminal error (Media Foundation stream failure, etc.). Runs on both
/// platforms now; macOS's status never errors, so it is a no-op there
/// (runtime AVFoundation failures are not surfaced today — behavior
/// preserved). Self-terminates once the camera is no longer the current
/// capture (generation change or an explicit stop).
fn start_camera_loss_monitor(
    app: tauri::AppHandle,
    generation: RoomGeneration,
    status: CameraStatus,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(CAMERA_LOSS_POLL_INTERVAL);
        loop {
            interval.tick().await;
            if !generation.is_current() {
                break;
            }
            let Some(error) = status.terminal_error() else {
                continue;
            };
            let Some(state) = app.try_state::<crate::session::SessionState>() else {
                break;
            };
            let _control = state.lock_camera_control().await;
            if !generation.is_current() {
                break;
            }
            // Take ONLY the capture this monitor belongs to — a stop+restart
            // since our last poll must leave the newer camera alone.
            let Some(camera) = state.take_active_camera_matching(&status) else {
                break;
            };
            teardown_active_camera(camera, false).await;
            state.set_camera_intent(false);
            emit_camera_publish_state(&app, false, Some(error));
            break;
        }
    })
}

async fn drop_camera_capture(capture: Box<dyn CameraBackend>) {
    if tokio::task::spawn_blocking(move || drop(capture))
        .await
        .is_err()
    {
        log::warn!("session: camera capture cleanup task failed");
    }
}

/// Tear one active camera down: abort the pump (and optionally the loss
/// monitor — the loss monitor calls this from inside itself, so it must NOT
/// abort its own task), release the capture on a blocking thread, unpublish
/// the track.
async fn teardown_active_camera(camera: ActiveCamera, abort_loss_monitor: bool) {
    camera.pump.abort();
    if abort_loss_monitor {
        camera.loss_monitor.abort();
    }
    drop_camera_capture(camera.capture).await;
    if let Err(error) = camera.published.unpublish().await {
        log::warn!("session: camera unpublish failed (room already closed?): {error}");
    }
}

/// Stop the published webcam, releasing the camera (light off) and
/// unpublishing the track. Idempotent no-op when not publishing.
pub(crate) async fn stop_camera_publish(state: &crate::session::SessionState) {
    let Some(camera) = state.take_active_camera() else {
        return;
    };
    log::info!("session: stop_camera_publish begin");
    teardown_active_camera(camera, true).await;
    #[cfg(target_os = "windows")]
    crate::camera_self_view::clear();
    log::info!("session: stop_camera_publish done (camera released)");
}

pub(crate) fn emit_camera_publish_state(
    app: &tauri::AppHandle,
    publishing: bool,
    error: Option<String>,
) {
    if let Err(error) = tauri::Emitter::emit(
        app,
        "camera-publish-state",
        CameraPublishStateEvent { publishing, error },
    ) {
        log::warn!("session: failed to emit camera-publish-state: {error}");
    }
}

/// The camera self-heal loop, decoupled from the platform capture/LiveKit/
/// Tauri so the real gating (intent, room generation, already-publishing) can
/// be driven in tests: for each entry in `schedule`, sleep that long (via the
/// injected `sleep`, so tests don't wall-clock), re-check that the user still
/// wants the camera in this still-current room, and run `attempt`.
pub(crate) async fn drive_camera_publish_attempts<Att, AttFut, Slp, SlpFut>(
    state: &crate::session::SessionState,
    generation: &RoomGeneration,
    schedule: &[Duration],
    mut attempt: Att,
    mut sleep: Slp,
) -> CameraHealOutcome
where
    Att: FnMut() -> AttFut,
    AttFut: Future<Output = Result<(), String>>,
    Slp: FnMut(Duration) -> SlpFut,
    SlpFut: Future<Output = ()>,
{
    let mut last_error = "camera publish failed".to_string();
    for (index, delay) in schedule.iter().enumerate() {
        if !delay.is_zero() {
            sleep(*delay).await;
        }
        if !generation.is_current() || !state.camera_intent() {
            log::info!(
                "session: camera self-heal cancelled before attempt {} (room left or camera turned off)",
                index + 1
            );
            return CameraHealOutcome::Cancelled;
        }
        if state.camera_publishing() {
            return CameraHealOutcome::Published;
        }
        match attempt().await {
            Ok(()) => return CameraHealOutcome::Published,
            Err(error) => {
                last_error = error;
                log::warn!(
                    "session: camera self-heal attempt {}/{} failed: {last_error}",
                    index + 1,
                    schedule.len()
                );
            }
        }
    }
    // Bounded retries exhausted: clear the intent so the toggle cannot keep
    // claiming ON, and report the terminal failure.
    state.set_camera_intent(false);
    CameraHealOutcome::TerminalFailure(last_error)
}

/// Drive the camera toward the user's intent (ON) with bounded, backed-off
/// retries, then emit one terminal `camera-publish-state` event. At most one
/// heal loop runs at a time; a duplicate call returns immediately (the
/// running loop re-reads intent/publish state before every attempt, so it
/// converges on the newest user action).
pub(crate) async fn ensure_camera_published(
    app: &tauri::AppHandle,
    state: &crate::session::SessionState,
    schedule: &[Duration],
) {
    if !state.try_begin_camera_heal() {
        log::info!("session: camera self-heal already running -- not starting a second loop");
        return;
    }
    // Clear the busy flag on every exit path.
    struct HealGuard<'a>(&'a crate::session::SessionState);
    impl Drop for HealGuard<'_> {
        fn drop(&mut self) {
            self.0.end_camera_heal();
        }
    }
    let _guard = HealGuard(state);

    let generation = state.current_room_generation();
    let outcome = drive_camera_publish_attempts(
        state,
        &generation,
        schedule,
        move || async move {
            // Serialize against set_camera_device / the manual toggle, same
            // lock discipline as start_camera_publish_command.
            let _control = state.lock_camera_control().await;
            let (preferred_device, preferred_mode) = app
                .try_state::<CameraDevicePreferences>()
                .map(|preferences| {
                    (
                        preferences.preferred_device(),
                        preferences.preferred_mode(),
                    )
                })
                .unwrap_or((None, None));
            start_camera_publish_with_device(app, state, preferred_device, preferred_mode)
                .await
                .map(|_| ())
        },
        |delay| tokio::time::sleep(delay),
    )
    .await;

    let event = match outcome {
        CameraHealOutcome::Published => {
            // A stop/leave can land between the winning attempt and this
            // emit (the control lock serializes the operations, not the
            // reporting) — never announce a publish that is already gone.
            if !state.camera_publishing() || !state.camera_intent() {
                return;
            }
            CameraPublishStateEvent {
                publishing: true,
                error: None,
            }
        }
        CameraHealOutcome::Cancelled => return,
        CameraHealOutcome::TerminalFailure(error) => {
            log::error!(
                "session: camera publish self-heal exhausted its retries -- camera intent cleared: {error}"
            );
            if !generation.is_current() {
                // The room this heal belonged to is gone; the UI for a NEW
                // room must not receive a stale terminal error.
                return;
            }
            CameraPublishStateEvent {
                publishing: false,
                error: Some(error),
            }
        }
    };
    emit_camera_publish_state(app, event.publishing, event.error);
}

/// #713: camera reconnect publication repair, wiring real LiveKit calls into
/// `share::repair_local_track_publication_after_reconnect`'s shared
/// generation-guarded/bounded-single-retry core (see that function's doc
/// comment). Called from `resilience.rs`'s post-`Reconnected` repair pass,
/// the same seam that already drives
/// `repair_active_share_publications_after_reconnect` for window shares —
/// this only fires when the vendored SDK's own `handle_restarted` republish
/// attempt timed out and left the local participant with no
/// `petal-camera-*` publication at all. Deliberately does NOT reuse
/// `ensure_camera_published`'s self-heal loop: that loop no-ops whenever the
/// session's camera slot is locally `Some`, which stays true here even
/// though the real LiveKit publication is gone — it would silently do
/// nothing. This instead republishes the SAME already-running capture's
/// existing `LocalVideoTrack` in place, no capture churn. macOS-only:
/// Windows has no resilience seam (`resilience.rs` is macOS-gated).
#[cfg(target_os = "macos")]
pub(crate) async fn repair_camera_publication_after_reconnect(
    app: &tauri::AppHandle,
    state: &crate::session::SessionState,
    reconnect_guard: &crate::session::ReconnectRepairGuard,
) {
    let Some((room_connection, identity, published)) =
        state.camera_reconnect_repair_snapshot(reconnect_guard)
    else {
        return;
    };
    let sid = published.sid().to_string();
    let expected_track_name = crate::transport::publisher::camera_track_name(&identity);
    let local_publications: Vec<(String, String)> = room_connection
        .room()
        .local_participant()
        .track_publications()
        .values()
        .map(|publication| (publication.sid().to_string(), publication.name()))
        .collect();
    let published_for_republish = published.clone();
    let app_for_failure = app.clone();
    crate::session::repair_local_track_publication_after_reconnect(
        "camera",
        &sid,
        &expected_track_name,
        &local_publications,
        || state.reconnect_repair_guard_is_current(reconnect_guard),
        move || async move {
            published_for_republish
                .republish_camera_after_reconnect()
                .await
                .map(|_| published_for_republish.sid().to_string())
                .map_err(|e| e.to_string())
        },
        move |message| {
            crate::resilience::emit_camera_publication_repair_failed(
                &app_for_failure,
                format!(
                    "Reconnect could not restore your camera -- try toggling video to reconnect it ({message})"
                ),
            );
        },
    )
    .await;
}

// =============================================================================
// Tauri commands
// =============================================================================

/// Enumerate the platform's camera devices (Settings picker).
#[tauri::command]
pub fn list_camera_devices() -> Result<Vec<CameraDeviceInfo>, String> {
    crate::transport::camera::list_devices().map_err(|error| error.to_string())
}

/// Enumerate the concrete (width, height, frame-rate) modes the selected
/// camera actually supports (Settings resolution/FPS menus). macOS returns
/// an empty list — those menus are disabled there.
#[tauri::command]
pub fn list_camera_modes(preferred_device_id: Option<String>) -> Result<Vec<CameraMode>, String> {
    crate::transport::camera::list_modes(preferred_device_id.as_deref())
        .map_err(|error| error.to_string())
}

/// Normalize a device-id argument the same way [`CameraDevicePreferences::set_preferred_device`]
/// stores it: empty string means "no preference" (`None`).
fn normalize_device_id(device_id: &str) -> Option<String> {
    if device_id.is_empty() {
        None
    } else {
        Some(device_id.to_string())
    }
}

/// #842: whether a camera-preference request is identical to what's already
/// applied, and can therefore be satisfied as a no-op instead of an
/// unconditional stop/restart of a live publish. Pulled out of
/// `set_camera_device`/`set_camera_prefs` (both call this exact function, not
/// a re-implementation) so the decision is unit testable without a live
/// `tauri::AppHandle` (this crate has no `tauri::test` mock-builder yet --
/// same rationale as `dump_metrics_value` in autotest.rs).
fn camera_request_is_unchanged<T: PartialEq>(requested: &T, previously_applied: &T) -> bool {
    requested == previously_applied
}

/// Apply a user-chosen camera capture mode (Settings resolution/FPS menus).
/// `None` width/height/frame_rate resets to Auto (best healthy mode). Restarts
/// the camera when publishing, mirroring [`set_camera_device`].
#[tauri::command]
pub async fn set_camera_prefs(
    app: tauri::AppHandle,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<u32>,
    preferences: tauri::State<'_, CameraDevicePreferences>,
    state: tauri::State<'_, crate::session::SessionState>,
) -> Result<crate::transport::camera::AppliedCameraDevice, String> {
    let _camera_transaction = state.lock_camera_control().await;
    let mode = match (width, height, frame_rate) {
        (Some(width), Some(height), Some(frame_rate)) if width > 0 && height > 0 && frame_rate > 0 => {
            Some(PreferredCameraMode {
                width,
                height,
                frame_rate,
            })
        }
        _ => None,
    };
    let previous_mode = preferences.preferred_mode();
    preferences.set_preferred_mode(mode);

    let in_room = state.is_in_room();
    if !in_room || !state.camera_publishing() {
        return Ok(crate::transport::camera::AppliedCameraDevice {
            in_room,
            ..Default::default()
        });
    }

    // #842: a caller (e.g. the frontend re-seeding preferences on mount) may
    // re-request the mode already applied. Restarting the camera for a
    // request that changes nothing glitches a live publish for no reason —
    // no-op instead of unconditionally stop/restarting.
    if camera_request_is_unchanged(&mode, &previous_mode) {
        return Ok(crate::transport::camera::AppliedCameraDevice {
            applied: true,
            in_room: true,
            used_default_fallback: false,
            error: None,
        });
    }

    stop_camera_publish(&state).await;
    match start_camera_publish_with_device(
        &app,
        &state,
        preferences.preferred_device(),
        preferences.preferred_mode(),
    )
    .await
    {
        Ok(outcome) => Ok(crate::transport::camera::AppliedCameraDevice {
            applied: true,
            in_room: true,
            used_default_fallback: outcome.used_default_fallback,
            error: None,
        }),
        Err(error) => {
            state.set_camera_intent(false);
            emit_camera_publish_state(&app, false, Some(error.clone()));
            Ok(crate::transport::camera::AppliedCameraDevice {
                applied: false,
                in_room: true,
                used_default_fallback: false,
                error: Some(error),
            })
        }
    }
}

/// Apply a user-chosen camera device (Settings picker). Restarts the camera
/// when publishing, mirroring [`set_camera_prefs`].
#[tauri::command]
pub async fn set_camera_device(
    app: tauri::AppHandle,
    device_id: String,
    preferences: tauri::State<'_, CameraDevicePreferences>,
    state: tauri::State<'_, crate::session::SessionState>,
) -> Result<crate::transport::camera::AppliedCameraDevice, String> {
    let _camera_transaction = state.lock_camera_control().await;
    let devices = crate::transport::camera::list_devices().map_err(|error| error.to_string())?;
    if !device_id.is_empty() && !devices.iter().any(|device| device.id == device_id) {
        if state.is_in_room() {
            crate::analytics::device_changed(
                crate::analytics::DeviceKind::Camera,
                crate::analytics::DeviceChange::Failed,
            );
        }
        return Ok(crate::transport::camera::AppliedCameraDevice {
            in_room: state.is_in_room(),
            error: Some("Selected camera is no longer available".into()),
            ..Default::default()
        });
    }

    let previous_device = preferences.preferred_device();
    let requested_device = normalize_device_id(&device_id);
    preferences.set_preferred_device(device_id);
    let in_room = state.is_in_room();
    if !in_room || !state.camera_publishing() {
        return Ok(crate::transport::camera::AppliedCameraDevice {
            in_room,
            ..Default::default()
        });
    }

    // #842: no-op when the requested device is already the applied one --
    // see the identical guard in set_camera_prefs above. This is what a
    // secondary window (network-cockpit, window-picker) re-seeding the same
    // preference on mount would otherwise turn into an unconditional
    // stop/restart of a live camera publish.
    if camera_request_is_unchanged(&requested_device, &previous_device) {
        return Ok(crate::transport::camera::AppliedCameraDevice {
            applied: true,
            in_room: true,
            used_default_fallback: false,
            error: None,
        });
    }

    stop_camera_publish(&state).await;
    match start_camera_publish_with_device(
        &app,
        &state,
        preferences.preferred_device(),
        preferences.preferred_mode(),
    )
    .await
    {
        Ok(outcome) => {
            crate::analytics::device_changed(
                crate::analytics::DeviceKind::Camera,
                crate::analytics::DeviceChange::Switched,
            );
            Ok(crate::transport::camera::AppliedCameraDevice {
                applied: true,
                in_room: true,
                used_default_fallback: outcome.used_default_fallback,
                error: None,
            })
        }
        Err(error) => {
            crate::analytics::device_changed(
                crate::analytics::DeviceKind::Camera,
                crate::analytics::DeviceChange::Failed,
            );
            state.set_camera_intent(false);
            emit_camera_publish_state(&app, false, Some(error.clone()));
            Ok(crate::transport::camera::AppliedCameraDevice {
                applied: false,
                in_room: true,
                used_default_fallback: false,
                error: Some(error),
            })
        }
    }
}

/// Turn ON the native webcam publish — called by the
/// meeting route's Video control AFTER its local self-view succeeds, and by
/// the menubar popover. Records the user's camera INTENT first (the leave→
/// rejoin reconcile's source of truth); a retryable failure hands off to the
/// bounded self-heal loop instead of leaving a dead ON toggle.
#[tauri::command]
pub async fn start_camera_publish_command(
    app: tauri::AppHandle,
    preferences: tauri::State<'_, CameraDevicePreferences>,
    state: tauri::State<'_, crate::session::SessionState>,
) -> Result<StartCameraPublishResult, String> {
    let _control = state.lock_camera_control().await;
    state.set_camera_intent(true);
    match start_camera_publish_with_device(
        &app,
        &state,
        preferences.preferred_device(),
        preferences.preferred_mode(),
    )
    .await
    {
        Ok(_) => Ok(StartCameraPublishResult { published: true }),
        Err(error) if error == "not in room" => {
            // Not retryable — there is no room to publish into. Intent is
            // cleared so a later join doesn't surprise-start the camera.
            state.set_camera_intent(false);
            Err(error)
        }
        Err(error) => {
            log::warn!(
                "session: start_camera_publish_command immediate attempt failed, starting self-heal: {error}"
            );
            let heal_app = app.clone();
            tauri::async_runtime::spawn(async move {
                let Some(state) =
                    tauri::Manager::try_state::<crate::session::SessionState>(&heal_app)
                else {
                    return;
                };
                ensure_camera_published(&heal_app, &state, CAMERA_HEAL_RETRY_BACKOFF).await;
            });
            Ok(StartCameraPublishResult { published: false })
        }
    }
}

/// Turn OFF the native webcam publish. Idempotent. Clears the user's camera
/// intent (which also cancels any in-flight self-heal loop before its next
/// attempt).
#[tauri::command]
pub async fn stop_camera_publish_command(
    state: tauri::State<'_, crate::session::SessionState>,
) -> Result<(), ()> {
    let _control = state.lock_camera_control().await;
    state.set_camera_intent(false);
    stop_camera_publish(&state).await;
    Ok(())
}

/// Real camera publish/intent snapshot — lets a (re)mounting UI surface
/// sync its Video toggle + self-view to reality instead of defaulting OFF
/// after a leave→rejoin restored the camera natively.
#[tauri::command]
pub fn camera_publish_state(
    state: tauri::State<'_, crate::session::SessionState>,
) -> CameraPublishStateSnapshot {
    CameraPublishStateSnapshot {
        publishing: state.camera_publishing(),
        intended: state.camera_intent(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Fake capture for `await_first_frame_or_release`: records whether the
    /// release path actually ran (the property the 2026-07-30 B2 fix must
    /// hold — a retry that reuses or leaks a wedged capture is the bug).
    struct FakeCapture {
        released: Arc<AtomicBool>,
    }

    impl ReleasableCapture for FakeCapture {
        fn release_blocking(self) {
            self.released.store(true, Ordering::SeqCst);
        }
    }

    /// B2 regression -- drives the REAL bounded first-frame wait used by
    /// `start_camera_publish_with_device` (not a detached pure helper):
    /// a capture that never delivers a frame must produce a terminal error
    /// AND be fully released before that error returns; a subsequent
    /// attempt with a frame-delivering capture must succeed cleanly.
    #[tokio::test]
    async fn first_frame_timeout_releases_capture_and_next_attempt_starts_clean() {
        // Attempt 1: no frame ever arrives (sender kept alive so the recv
        // fails by TIMEOUT, exactly like the live 18:39:11-18:39:29 hangs,
        // not by disconnect).
        let released = Arc::new(AtomicBool::new(false));
        let (wedged_tx, wedged_rx) = std::sync::mpsc::channel::<(u32, u32)>();
        let outcome = await_first_frame_or_release(
            FakeCapture {
                released: released.clone(),
            },
            wedged_rx,
            Duration::from_millis(100),
        )
        .await;
        let error = outcome.err().expect("no frame within the bound must error");
        assert!(
            error.contains("no camera frames"),
            "terminal error must say what happened, got: {error:?}"
        );
        assert!(
            released.load(Ordering::SeqCst),
            "the wedged capture must be RELEASED before the error returns"
        );
        drop(wedged_tx);

        // Attempt 2: a fresh capture that delivers its first frame succeeds
        // and is handed back (NOT released) for publishing.
        let released_2 = Arc::new(AtomicBool::new(false));
        let (ok_tx, ok_rx) = std::sync::mpsc::channel::<(u32, u32)>();
        ok_tx.send((1280, 720)).expect("send first frame size");
        let (returned_capture, size) = await_first_frame_or_release(
            FakeCapture {
                released: released_2.clone(),
            },
            ok_rx,
            Duration::from_millis(100),
        )
        .await
        .expect("a frame-delivering capture must succeed");
        assert_eq!(size, (1280, 720));
        assert!(
            !released_2.load(Ordering::SeqCst),
            "a successful attempt must hand the capture back, not release it"
        );
        drop(returned_capture);
    }

    /// B1/self-heal -- drives the REAL heal loop (`drive_camera_publish_attempts`,
    /// the exact function `ensure_camera_published` runs) against a real
    /// `SessionState`'s intent + room-generation gating.
    #[tokio::test]
    async fn camera_heal_retries_with_backoff_then_succeeds() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        let generation = state.begin_room_generation();
        let attempts = Arc::new(AtomicUsize::new(0));
        let slept = Arc::new(Mutex::new(Vec::<Duration>::new()));

        let attempts_in = attempts.clone();
        let slept_in = slept.clone();
        let outcome = drive_camera_publish_attempts(
            &state,
            &generation,
            &[
                Duration::ZERO,
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            move || {
                let attempts = attempts_in.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 {
                        Err("no camera frames".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
            move |d| {
                slept_in.lock_unpoisoned().push(d);
                async {}
            },
        )
        .await;

        assert_eq!(outcome, CameraHealOutcome::Published);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(
            slept.lock_unpoisoned().clone(),
            vec![Duration::from_secs(2), Duration::from_secs(4)],
            "retries must be backed off per the schedule"
        );
        assert!(
            state.camera_intent(),
            "a successful heal must NOT clear the user's intent"
        );
    }

    /// Turning the camera off mid-heal (the user's newest action) must
    /// cancel the loop before its next attempt -- the heal serves intent,
    /// never overrides it.
    #[tokio::test]
    async fn camera_heal_cancels_when_intent_turns_off_mid_backoff() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        let generation = state.begin_room_generation();
        let attempts = Arc::new(AtomicUsize::new(0));

        let attempts_in = attempts.clone();
        let state_ref = &state;
        let outcome = drive_camera_publish_attempts(
            state_ref,
            &generation,
            &[Duration::ZERO, Duration::from_secs(2)],
            move || {
                let attempts = attempts_in.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err("no camera frames".to_string())
                }
            },
            move |_d| {
                // The user toggles the camera OFF during the backoff sleep.
                state_ref.set_camera_intent(false);
                async {}
            },
        )
        .await;

        assert_eq!(outcome, CameraHealOutcome::Cancelled);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "no further attempts after the user turned the camera off"
        );
    }

    /// Leaving the room (generation invalidated) mid-heal must cancel the
    /// loop -- exactly the leave→rejoin lifecycle boundary the 2026-07-30
    /// incident crossed; a stale heal from the OLD room must never publish
    /// into the new one.
    #[tokio::test]
    async fn camera_heal_cancels_when_room_generation_is_invalidated() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        let generation = state.begin_room_generation();
        let attempts = Arc::new(AtomicUsize::new(0));

        let attempts_in = attempts.clone();
        let state_ref = &state;
        let outcome = drive_camera_publish_attempts(
            state_ref,
            &generation,
            &[Duration::ZERO, Duration::from_secs(2)],
            move || {
                let attempts = attempts_in.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err("no camera frames".to_string())
                }
            },
            move |_d| {
                // leave_room during the backoff sleep.
                state_ref.invalidate_room_generation();
                async {}
            },
        )
        .await;

        assert_eq!(outcome, CameraHealOutcome::Cancelled);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    /// Exhausted retries are TERMINAL: the intent is cleared (no surface may
    /// keep claiming ON) and the last error is reported for the UI's retry
    /// affordance. "Four attempts, no terminal error, only a relaunch fixed
    /// it" is the exact live failure this locks out.
    #[tokio::test]
    async fn camera_heal_terminal_failure_clears_intent_and_reports_error() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        let generation = state.begin_room_generation();

        let outcome = drive_camera_publish_attempts(
            &state,
            &generation,
            &[Duration::ZERO, Duration::from_secs(2)],
            move || async move { Err("no camera frames within 5s".to_string()) },
            move |_d| async {},
        )
        .await;

        match outcome {
            CameraHealOutcome::TerminalFailure(error) => {
                assert!(error.contains("no camera frames"));
            }
            other => panic!("expected TerminalFailure, got {other:?}"),
        }
        assert!(
            !state.camera_intent(),
            "terminal failure must clear the intent so the toggle stops lying"
        );
    }

    /// An already-publishing camera short-circuits the heal (e.g. the user's
    /// own manual retry won the race while the heal slept).
    #[tokio::test]
    async fn camera_heal_short_circuits_when_intent_already_met() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        let generation = state.begin_room_generation();

        // No ActiveCamera can be constructed in tests (it holds a live
        // capture), so drive the equivalent short-circuit: Ok(()) from the
        // first attempt reports Published without touching the schedule's
        // remaining entries.
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_in = attempts.clone();
        let outcome = drive_camera_publish_attempts(
            &state,
            &generation,
            &[Duration::ZERO, Duration::from_secs(2)],
            move || {
                let attempts = attempts_in.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
            move |_d| async {},
        )
        .await;
        assert_eq!(outcome, CameraHealOutcome::Published);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn camera_start_gate_requires_intent_and_current_room_generation() {
        let state = crate::session::SessionState::default();
        let generation = state.current_room_generation();

        assert!(!camera_start_is_current(&state, &generation));
        state.set_camera_intent(true);
        assert!(camera_start_is_current(&state, &generation));
        state.invalidate_room_generation();
        assert!(!camera_start_is_current(&state, &generation));
    }

    #[test]
    fn camera_publication_waits_for_subscriber_negotiation() {
        assert_eq!(
            CAMERA_SUBSCRIBER_NEGOTIATION_DELAY,
            Duration::from_millis(500)
        );
    }

    #[test]
    fn camera_publish_health_log_contract_includes_drop_counters() {
        let line = format_camera_publish_health(&CameraPublishHealth {
            captured_frames: 120,
            pushed_frames: 118,
            dropped_push_frames: 1,
            overwritten_latest_frames: 2,
            capture_fps: 29.97,
            encode_fps: 29.40,
        });
        assert_eq!(
            line,
            "session: camera publish health -- captured=120 pushed=118 dropped_push=1 overwritten_latest=2 capture_fps=30.0 encode_fps=29.4"
        );
    }

    #[test]
    fn camera_publish_health_emits_only_for_classified_unhealthy_intervals() {
        let healthy = CameraPublishHealth {
            captured_frames: 150,
            pushed_frames: 149,
            dropped_push_frames: 1,
            overwritten_latest_frames: 1,
            capture_fps: 30.0,
            encode_fps: 29.8,
        };
        assert_eq!(unhealthy_camera_publish_diagnostic(&healthy, 1, 1), None);

        let degraded = CameraPublishHealth {
            captured_frames: 5,
            pushed_frames: 4,
            dropped_push_frames: 4,
            overwritten_latest_frames: 8,
            capture_fps: 1.0,
            encode_fps: 0.8,
        };
        assert_eq!(
            unhealthy_camera_publish_diagnostic(&degraded, 4, 8),
            Some(SentryDiagnosticEvent::CameraHealth(
                CameraHealthDiagnostic {
                    role: DiagnosticRole::Sharer,
                    direction: CameraDirection::Publish,
                    capture_cadence: CadenceBucket::Severe,
                    encode_cadence: CadenceBucket::Severe,
                    queue_backpressure: QueueBackpressureBucket::Saturated,
                    decoder_render: DecoderRenderHealth::NotApplicable,
                }
            ))
        );
    }

    #[test]
    fn pre_pump_camera_counters_are_baseline_not_an_unhealthy_interval() {
        let baseline = CameraPublishBaseline {
            captured_frames: 50_000,
            pushed_frames: 49_000,
            dropped_push_frames: 500,
            overwritten_latest_frames: 750,
        };
        let current = CameraPublishTelemetry {
            captured_frames: 50_150,
            pushed_frames: 49_149,
            dropped_push_frames: 501,
            overwritten_latest_frames: 751,
        };
        let (health, dropped, overwritten) =
            camera_publish_interval_health(&current, baseline, 5.0);
        assert_eq!(health.capture_fps, 30.0);
        assert_eq!(health.encode_fps, 29.8);
        assert_eq!((dropped, overwritten), (1, 1));
        assert_eq!(
            unhealthy_camera_publish_diagnostic(&health, dropped, overwritten),
            None
        );
    }

    // #842: a network-cockpit/window-picker window re-seeding preferences on
    // mount must not restart a live camera publish. These test the exact
    // decision function set_camera_device/set_camera_prefs call, not a
    // duplicated re-implementation.
    #[test]
    fn normalize_device_id_treats_empty_string_as_no_preference() {
        assert_eq!(normalize_device_id(""), None);
        assert_eq!(normalize_device_id("cam-1"), Some("cam-1".to_string()));
    }

    #[test]
    fn camera_request_is_unchanged_matches_identical_device_requests() {
        // The empty-string/None normalization boundary: re-requesting "no
        // preference" against an already-None preference must count as
        // unchanged, not as a mismatch that forces a restart.
        assert!(camera_request_is_unchanged(
            &normalize_device_id(""),
            &None::<String>
        ));
        assert!(camera_request_is_unchanged(
            &normalize_device_id("cam-1"),
            &Some("cam-1".to_string())
        ));
        assert!(!camera_request_is_unchanged(
            &normalize_device_id("cam-2"),
            &Some("cam-1".to_string())
        ));
        assert!(!camera_request_is_unchanged(
            &normalize_device_id("cam-1"),
            &None::<String>
        ));
    }

    #[test]
    fn camera_request_is_unchanged_matches_identical_mode_requests() {
        let mode_a = Some(PreferredCameraMode {
            width: 1280,
            height: 720,
            frame_rate: 30,
        });
        let mode_b = Some(PreferredCameraMode {
            width: 1920,
            height: 1080,
            frame_rate: 30,
        });
        assert!(camera_request_is_unchanged(&mode_a, &mode_a));
        assert!(camera_request_is_unchanged(
            &None::<PreferredCameraMode>,
            &None::<PreferredCameraMode>
        ));
        assert!(!camera_request_is_unchanged(&mode_a, &mode_b));
        assert!(!camera_request_is_unchanged(&mode_a, &None::<PreferredCameraMode>));
    }
}
