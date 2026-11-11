//! Shared camera session orchestration: capture start,
//! publish pump + telemetry, loss monitoring, the bounded self-heal loop,
//! reconnect publication repair (macOS), and every camera Tauri command.
//! Platform-neutral — the platform differences live in the `SessionState`
//! methods it calls (see `session_stub.rs` / `session/mod.rs`) and in
//! `transport::camera`.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::Manager;

use crate::logging::{
    CadenceBucket, CameraDirection, CameraHealthDiagnostic, CameraStallCauseTag,
    DecoderRenderHealth, DiagnosticRole, QueueBackpressureBucket, SentryDiagnosticEvent,
};
use crate::room_generation::{CameraDeferral, RoomGeneration};
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
            // #126: receive-side only -- a publish interval has no
            // subscription to pause and no decoder to stall.
            stall_cause: CameraStallCauseTag::NotApplicable,
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
    /// Room generation the outcome belongs to: a surface ignores an outcome
    /// older than its own camera request (PR #503).
    pub generation: u64,
    /// Process-wide emit order ([`camera_event_seq`]). Orders an outcome
    /// against a start's result within one room generation (PR #503).
    pub seq: u64,
}

/// One counter for every `camera-publish-state` emit, so a surface can tell
/// an OFF emitted before its start from one emitted after (PR #503).
static CAMERA_EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_camera_event_seq() -> u64 {
    CAMERA_EVENT_SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

/// The seq of the latest `camera-publish-state` emitted. A start reads it
/// under the camera control lock: every outcome with a higher seq was
/// emitted after the start's own result was decided.
pub(crate) fn camera_event_seq() -> u64 {
    CAMERA_EVENT_SEQ.load(Ordering::SeqCst)
}

/// A `camera-publish-state` payload, stamped with the next seq.
pub(crate) fn camera_publish_state_event(
    generation: u64,
    publishing: bool,
    error: Option<String>,
) -> CameraPublishStateEvent {
    CameraPublishStateEvent {
        publishing,
        error,
        generation,
        seq: next_camera_event_seq(),
    }
}

/// Payload of the `camera-intent-changed` event: the user's camera INTENT,
/// announced at the two moments the physical device changes hands -- `true`
/// BEFORE the meeting tries to acquire it, `false` AFTER it has been
/// released. `camera-publish-state` cannot carry this: it fires only at a
/// terminal publish OUTCOME, and every surface reads its `publishing: false`
/// as "the camera is off". The Settings window runs in its own webview and
/// its getUserMedia preview owns the same device, so it needs the intent
/// EDGE, not the outcome (PR #72 review).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CameraIntentEvent {
    pub intended: bool,
}

/// Result of `start_camera_publish_command`. `published: false` means the
/// immediate attempt failed but the bounded self-heal loop is retrying in
/// the background; the terminal outcome arrives as a `camera-publish-state`
/// event (see [`ensure_camera_published`]).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartCameraPublishResult {
    pub published: bool,
    /// Room generation this request is bound to (the joined room, or the
    /// join it was deferred to); later outcomes carry the same or newer.
    pub generation: u64,
    /// [`camera_event_seq`] read under the camera control lock as the result
    /// was decided: an OFF with a higher seq came after this start.
    pub seq: u64,
}

impl StartCameraPublishResult {
    /// Build under the camera control lock (see `seq`).
    fn decided(published: bool, generation: u64) -> Self {
        Self {
            published,
            generation,
            seq: camera_event_seq(),
        }
    }
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
    /// surface keeps claiming ON for a publish that will never happen. The
    /// `u64` is its camera-publish-state seq, reserved under the control lock
    /// with the clear, so a start that takes the lock after it is newer.
    TerminalFailure(String, u64),
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
///
/// `report_failure` runs the moment the wait fails, BEFORE the release: a
/// wedged device can block that release indefinitely, and the caller's own
/// log line would then never be written (#274).
pub(super) async fn await_first_frame_or_release<C: ReleasableCapture>(
    capture: C,
    size_rx: std::sync::mpsc::Receiver<(u32, u32)>,
    timeout: Duration,
    report_failure: impl FnOnce(&str),
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
            report_failure(&e);
            // Stop + release the capture we just started before bailing.
            // Platform teardown blocks — run on a blocking thread.
            let release_started = std::time::Instant::now();
            if tokio::task::spawn_blocking(move || capture.release_blocking())
                .await
                .is_err()
            {
                log::error!(
                    "session: camera capture release task panicked after first-frame timeout"
                );
            }
            let release_took = release_started.elapsed();
            if release_took >= Duration::from_secs(1) {
                log::warn!(
                    "session: camera capture release took {:.1}s after the first-frame failure",
                    release_took.as_secs_f64()
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
        match await_first_frame_or_release(
            capture,
            first_frame_rx,
            CAMERA_FIRST_FRAME_TIMEOUT,
            |error| {
                log::warn!("session: camera first-frame wait failed ({error}); releasing the capture")
            },
        )
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

    #[cfg(target_os = "macos")]
    start_camera_encoder_health_monitor(
        app.clone(),
        generation.clone(),
        published.clone(),
        telemetry.clone(),
    );
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

/// #411: watch the camera's VideoToolbox encoder and re-create it when it has
/// stopped producing frames. See `transport::encoder_health` for the evidence
/// and the decision rule; this is the thin I/O loop around that state machine:
/// it samples pushed frames (the pump's counter), the track's own
/// `frames_encoded`, and the process-wide VideoToolbox failure counts, and acts
/// on what the detector says.
///
/// Bound to the publication: it exits when the track is terminally unpublished
/// (`PublishedTrack::lifecycle_token`) or the room generation moves on. The
/// re-creation runs under the camera control lock so it cannot interleave with
/// a manual stop/device switch; a leave that tears the camera down without that
/// lock is caught by the post-check (the token is cancelled first thing in
/// `unpublish`), which takes the just-republished sender back down.
#[cfg(target_os = "macos")]
fn start_camera_encoder_health_monitor(
    app: tauri::AppHandle,
    generation: RoomGeneration,
    published: Arc<PublishedTrack>,
    telemetry: Arc<Mutex<CameraPublishTelemetry>>,
) {
    use crate::analytics::{EncoderRecoveryOutcome, EncoderTrackKind};
    use crate::transport::encoder_health::{
        vt_encode_failure_counts, EncoderHealthAction, EncoderHealthSample,
        EncoderStarvationDetector, SampleVerdict, ENCODER_HEALTH_SAMPLE_INTERVAL,
    };

    /// Log the per-interval counts every Nth sample while healthy (every
    /// failing sample is logged): ~30 s at the 2 s sample interval.
    const HEALTHY_LOG_EVERY: u32 = 15;

    let token = published.lifecycle_token();
    tauri::async_runtime::spawn(async move {
        let mut detector = EncoderStarvationDetector::new();
        let mut interval = tokio::time::interval(ENCODER_HEALTH_SAMPLE_INTERVAL);
        let mut samples: u32 = 0;
        let mut reported_first_failure = false;
        // What the last re-creation was for, so a `recovered` event names the
        // failure it recovered from rather than a placeholder.
        let mut last_recreate = None;
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = interval.tick() => {}
            }
            if !generation.is_current() {
                break;
            }
            let encoded_frames = tokio::select! {
                _ = token.cancelled() => break,
                encoded = published.frames_encoded() => encoded,
            };
            let pushed_frames = telemetry.lock_unpoisoned().pushed_frames;
            let action = detector.observe(EncoderHealthSample {
                pushed_frames,
                encoded_frames,
                vt_failures: vt_encode_failure_counts(),
                at: Instant::now(),
            });
            samples = samples.wrapping_add(1);
            let verdict = detector.last_verdict();
            if let Some((pushed, encoded, failures)) = detector.last_interval() {
                // A baseline sample judged nothing; its `last_interval` is the
                // previous one's, so do not report it as current.
                let judged = verdict.is_some_and(|verdict| verdict != SampleVerdict::Baseline);
                if judged
                    && (verdict == Some(SampleVerdict::Failing) || samples % HEALTHY_LOG_EVERY == 0)
                {
                    // The per-track encode counter #411 asked for: `encode_fps`
                    // in `camera publish health` counts frames PUSHED, this
                    // counts frames the encoder EMITTED.
                    log::info!(
                        "session: camera encoder health -- pushed={pushed} encoded={encoded} vt_encode_failures={failures} verdict={verdict:?} streak={}",
                        detector.failing_streak()
                    );
                }
            }
            match action {
                EncoderHealthAction::None => {}
                EncoderHealthAction::Recreate {
                    attempt,
                    vt_failures,
                    dominant,
                } => {
                    log::warn!(
                        "session: camera encoder produced almost nothing for {} samples while VideoToolbox logged {vt_failures} encode failures (mostly {}); re-creating the encoder (attempt {attempt})",
                        crate::transport::encoder_health::FAILING_SAMPLES_BEFORE_RECREATE,
                        dominant.tag()
                    );
                    if !reported_first_failure {
                        reported_first_failure = true;
                        // Fixed text so Sentry groups every occurrence into ONE
                        // issue; the counts ride on the warn breadcrumb above.
                        log::error!(
                            "session: camera VideoToolbox encoder stopped producing frames; re-creating it (#411)"
                        );
                    }
                    let Some(state) = app.try_state::<crate::session::SessionState>() else {
                        break;
                    };
                    let _control = state.lock_camera_control().await;
                    if token.is_cancelled() || !generation.is_current() {
                        break;
                    }
                    match published.recreate_camera_encoder().await {
                        Ok(()) => {
                            detector.note_recreated();
                            last_recreate = Some((dominant, vt_failures));
                            crate::analytics::video_encoder_recovery(
                                EncoderTrackKind::Camera,
                                EncoderRecoveryOutcome::Recreated,
                                dominant,
                                vt_failures,
                                attempt,
                            );
                            if token.is_cancelled() || !generation.is_current() {
                                // A leave tore the camera down while we were
                                // republishing: do not leave a sender behind.
                                if let Err(error) = published.unpublish().await {
                                    log::warn!(
                                        "session: camera encoder re-create raced a teardown; cleanup unpublish failed: {error}"
                                    );
                                }
                                break;
                            }
                        }
                        Err(error) => {
                            detector.note_recreate_failed();
                            log::warn!("session: camera encoder re-create failed: {error}");
                            crate::analytics::video_encoder_recovery(
                                EncoderTrackKind::Camera,
                                EncoderRecoveryOutcome::RecreateFailed,
                                dominant,
                                vt_failures,
                                attempt,
                            );
                        }
                    }
                }
                EncoderHealthAction::Recovered { attempts } => {
                    log::info!(
                        "session: camera encoder producing frames again after {attempts} re-creation(s) (#411)"
                    );
                    if let Some((dominant, vt_failures)) = last_recreate.take() {
                        crate::analytics::video_encoder_recovery(
                            EncoderTrackKind::Camera,
                            EncoderRecoveryOutcome::Recovered,
                            dominant,
                            vt_failures,
                            attempts,
                        );
                    }
                }
                EncoderHealthAction::GiveUp {
                    attempts,
                    vt_failures,
                    dominant,
                } => {
                    log::warn!(
                        "session: camera encoder still failing after {attempts} re-creation(s) inside {}s ({vt_failures} VideoToolbox failures, mostly {}); giving up for this publication",
                        crate::transport::encoder_health::RECREATE_WINDOW.as_secs(),
                        dominant.tag()
                    );
                    log::error!(
                        "session: camera VideoToolbox encoder keeps failing after re-creation; stopped retrying (#411)"
                    );
                    crate::analytics::video_encoder_recovery(
                        EncoderTrackKind::Camera,
                        EncoderRecoveryOutcome::GaveUp,
                        dominant,
                        vt_failures,
                        attempts,
                    );
                    break;
                }
            }
        }
    });
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
            emit_camera_intent(&app, false);
            emit_camera_publish_state(&app, generation.value(), false, Some(error));
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
pub(crate) async fn stop_camera_publish(
    app: &tauri::AppHandle,
    state: &crate::session::SessionState,
) {
    if let Some(camera) = state.take_active_camera() {
        log::info!("session: stop_camera_publish begin");
        teardown_active_camera(camera, true).await;
        #[cfg(target_os = "windows")]
        crate::camera_self_view::clear();
        log::info!("session: stop_camera_publish done (camera released)");
    }
    // The device is free NOW -- announced even when there was nothing to
    // release, so a stop that lands while the first publish attempt is still
    // in flight (nothing acquired yet, the self-heal loop just cancels
    // silently) still tells a waiting preview it may come back. Report the
    // intent as it stands rather than a blanket `false`: a live device switch
    // stops the old capture with the intent still ON, and a preview that
    // grabbed the camera in that gap would be holding it when the new
    // capture starts.
    emit_camera_intent(app, state.camera_intent());
}

pub(crate) fn emit_camera_publish_state(
    app: &tauri::AppHandle,
    generation: u64,
    publishing: bool,
    error: Option<String>,
) {
    send_camera_publish_state(
        app,
        camera_publish_state_event(generation, publishing, error),
    );
}

/// Emit an already-stamped payload (a seq reserved under the control lock).
fn send_camera_publish_state(app: &tauri::AppHandle, event: CameraPublishStateEvent) {
    if let Err(error) = tauri::Emitter::emit(app, "camera-publish-state", event) {
        log::warn!("session: failed to emit camera-publish-state: {error}");
    }
}

/// Broadcast the user's camera intent (see [`CameraIntentEvent`]). Emitted
/// BEFORE the device is acquired on the way on and AFTER it is released on
/// the way off, so a second webview holding the same camera can yield and
/// come back.
///
/// Delivery is asynchronous: a preview may not have let go by the time the
/// immediate publish attempt runs. That attempt can still lose the race --
/// the bounded self-heal loop (`CAMERA_HEAL_RETRY_BACKOFF`) is what makes it
/// converge, and this event is what makes the preview yield in time for it.
///
/// MEASURED (#76, 2026-09-12, MacBook Air, macOS 26, build 0.9.25, three
/// runs of `apps/desktop/scripts/measure-camera-intent.mjs` with the Settings
/// preview live): the first attempt won all 6 contended ON episodes and all
/// 6 live device switches; no self-heal retry was ever needed.
///
/// - intent -> `start_camera_publish begin`: 0-1 ms. The preview's release
///   landed 2-6 ms after the intent, i.e. AFTER the native acquisition had
///   begun, every time. The immediate attempt still succeeded because the
///   acquisition itself takes ~1.1 s to its first frame (margin intent ->
///   succeeded 1.06-1.41 s), and on macOS AVFoundation does not refuse a
///   second client while the webview's capture winds down.
/// - OFF: capture released 124-181 ms after the intent cleared; the preview
///   was back 10-17 ms after that.
/// - device switch: old capture released 87-134 ms; the preview never took
///   the device in the stop -> start gap (the intent stays ON across it).
///
/// So on macOS the ordering plus the retry loop is sufficient and the retry
/// loop was not exercised. Windows' single-client capture was not measured;
/// re-run the script there before relying on the same conclusion.
pub(crate) fn emit_camera_intent(app: &tauri::AppHandle, intended: bool) {
    // Timestamped so one live run measures the margin that matters (#76): the
    // gap between this line and the `start_camera_publish` that follows is the
    // window a Settings preview has to release the device. If the first
    // acquisition then succeeds, the preview yielded in time; if it fails and
    // only the self-heal retry recovers, it did not.
    log::info!("session: camera-intent intended={intended}");
    if let Err(error) = tauri::Emitter::emit(app, "camera-intent-changed", CameraIntentEvent {
        intended,
    }) {
        log::warn!("session: failed to emit camera-intent-changed: {error}");
    }
}

/// The Settings window's camera PREVIEW, as seen from the native log (#76).
///
/// The preview holds the device from the webview (`getUserMedia`), which no
/// native line can see, so a `camera-intent` episode alone cannot say
/// whether the publish was contended at all -- an uncontended win looks
/// identical to a contended one. Settings reports its own edges here:
///
/// ```text
/// settings: camera preview acquired
/// settings: camera preview released reason=<slug>
/// settings: camera preview failed reason=<DOMException name>
/// ```
///
/// `scripts/analyze-field-log.mjs` reads exactly these three shapes to label
/// each episode contended or not, so the text is fixed and the free-form
/// `reason` is reduced to a slug: webview-supplied text must never be able to
/// forge a second log line or carry a device label.
#[tauri::command]
pub fn log_camera_preview_state(state: String, reason: Option<String>) {
    log::info!("{}", camera_preview_log_line(&state, reason.as_deref()));
}

pub(crate) fn camera_preview_log_line(state: &str, reason: Option<&str>) -> String {
    let phase = match state {
        "acquired" | "released" | "failed" => state,
        _ => "unknown",
    };
    let reason: String = reason
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(48)
        .collect();
    if reason.is_empty() {
        format!("settings: camera preview {phase}")
    } else {
        format!("settings: camera preview {phase} reason={reason}")
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
    // A room change landed during the final attempt: its failure belongs to
    // the old room and must not clear the new room's intent (PR #503 F2).
    // Checked and cleared under the control lock, which every intent-ON
    // writer holds, so a new room's camera can't land between the two.
    let _control = state.lock_camera_control().await;
    if !generation.is_current() {
        return CameraHealOutcome::Cancelled;
    }
    // Bounded retries exhausted: clear the intent so the toggle cannot keep
    // claiming ON, and report the terminal failure. Its seq is taken here,
    // under the lock: emitted later, it must still order before any start
    // that follows the clear (PR #503 round 4).
    state.set_camera_intent(false);
    CameraHealOutcome::TerminalFailure(last_error, next_camera_event_seq())
}

/// `drive_camera_publish_attempts`, re-targeted at each newer room: a room
/// change cancels the attempts, but the new room's own start was refused as
/// "already running" while this loop held the heal slot, so the camera is
/// carried into the new room rather than dropped silently. Returns the
/// outcome and the room generation it belongs to.
pub(crate) async fn drive_camera_heal_following_rooms<Att, AttFut, Slp, SlpFut>(
    state: &crate::session::SessionState,
    schedule: &[Duration],
    mut attempt: Att,
    mut sleep: Slp,
    in_room: impl Fn() -> bool,
) -> (CameraHealOutcome, RoomGeneration)
where
    Att: FnMut() -> AttFut,
    AttFut: Future<Output = Result<(), String>>,
    Slp: FnMut(Duration) -> SlpFut,
    SlpFut: Future<Output = ()>,
{
    let mut generation = state.current_room_generation();
    loop {
        let outcome =
            drive_camera_publish_attempts(state, &generation, schedule, &mut attempt, &mut sleep)
                .await;
        // No follow cap (PR #503 F3): each follow needs a newer room, so it
        // is bounded by real joins, and a capped heal exited with the intent
        // ON and nothing left to publish it.
        if matches!(outcome, CameraHealOutcome::Cancelled)
            && heal_should_follow_new_room(
                generation.is_current(),
                state.camera_intent(),
                state.camera_publishing(),
                in_room(),
            )
        {
            log::info!("session: camera self-heal following the camera into the new room");
            generation = state.current_room_generation();
            continue;
        }
        return (outcome, generation);
    }
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
    // Clear the busy flag on every exit path.
    struct HealGuard<'a>(&'a crate::session::SessionState);
    impl Drop for HealGuard<'_> {
        fn drop(&mut self) {
            self.0.end_camera_heal();
        }
    }
    let (outcome, generation, _guard) = loop {
        if !state.try_begin_camera_heal() {
            log::info!("session: camera self-heal already running -- not starting a second loop");
            return;
        }
        let guard = HealGuard(state);

        let (outcome, generation) = drive_camera_heal_following_rooms(
            state,
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
            || state.is_in_room(),
        )
        .await;
        if matches!(outcome, CameraHealOutcome::Cancelled) {
            drop(guard);
            // A join committing between the follow check and this release had
            // its own publish refused by our slot: take the camera there.
            if heal_should_follow_new_room(
                generation.is_current(),
                state.camera_intent(),
                state.camera_publishing(),
                state.is_in_room(),
            ) {
                continue;
            }
            return;
        }
        break (outcome, generation, guard);
    };

    let (error, seq) = match outcome {
        CameraHealOutcome::Published => {
            announce_heal_published(
                state,
                || state.camera_publishing() && state.camera_intent(),
                || emit_camera_publish_state(app, generation.value(), true, None),
            )
            .await;
            return;
        }
        CameraHealOutcome::Cancelled => return,
        CameraHealOutcome::TerminalFailure(error, seq) => {
            log::error!(
                "session: camera publish self-heal exhausted its retries -- camera intent cleared: {error}"
            );
            if !generation.is_current() {
                // The room this heal belonged to is gone; the UI for a NEW
                // room must not receive a stale terminal error.
                return;
            }
            // Intent was cleared with the retries exhausted and nothing
            // acquired -- the device is free for a Settings preview again.
            emit_camera_intent(app, false);
            (error, seq)
        }
    };
    send_camera_publish_state(
        app,
        CameraPublishStateEvent {
            publishing: false,
            error: Some(error),
            generation: generation.value(),
            seq,
        },
    );
}

/// #461: check-and-announce a heal's publish UNDER the control lock. Checked
/// outside it, a stop queued behind the winning attempt ran after the check,
/// so the UI was told ON for a camera torn down in the same millisecond and
/// stayed ON with nothing published. Returns whether it announced.
async fn announce_heal_published(
    state: &crate::session::SessionState,
    still_live: impl FnOnce() -> bool,
    announce: impl FnOnce(),
) -> bool {
    let _control = state.lock_camera_control().await;
    if !still_live() {
        return false;
    }
    announce();
    true
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

    stop_camera_publish(&app, &state).await;
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
            emit_camera_intent(&app, false);
            emit_camera_publish_state(
                &app,
                state.current_room_generation().value(),
                false,
                Some(error.clone()),
            );
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
    apply_camera_device(&app, device_id, preferences.inner(), state.inner()).await
}

/// [`set_camera_device`] without the Tauri command envelope, so the #76
/// runbook driver (`autotest.rs`'s `set_camera_device` socket command) goes
/// through exactly the path the Settings picker does.
pub(crate) async fn apply_camera_device(
    app: &tauri::AppHandle,
    device_id: String,
    preferences: &CameraDevicePreferences,
    state: &crate::session::SessionState,
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

    stop_camera_publish(app, state).await;
    match start_camera_publish_with_device(
        app,
        state,
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
            emit_camera_intent(app, false);
            emit_camera_publish_state(
                app,
                state.current_room_generation().value(),
                false,
                Some(error.clone()),
            );
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
    start_camera_publish_intent(&app, preferences.inner(), state.inner()).await
}

/// The body of [`start_camera_publish_command`]: intent first, then the
/// device. Shared with the #76 runbook driver (`autotest.rs`'s `camera_on`
/// socket command) so the measurement exercises the real ON path and not a
/// test-only copy of it.
pub(crate) async fn start_camera_publish_intent(
    app: &tauri::AppHandle,
    preferences: &CameraDevicePreferences,
    state: &crate::session::SessionState,
) -> Result<StartCameraPublishResult, String> {
    let _control = state.lock_camera_control().await;
    state.set_camera_intent(true);
    // Announce the intent BEFORE acquiring: the Settings window's preview
    // holds the same physical device and has to let go for this to succeed.
    emit_camera_intent(app, true);
    // A join is pending: its commit publishes it (`publish_deferred_camera`).
    // Asked before the room slot, which Windows fills before it commits.
    if state.deferred_camera().defer_if_pending() {
        log::info!(
            "session: camera turned on while the room join is pending -- publishing once joined"
        );
        return Ok(StartCameraPublishResult::decided(
            false,
            state.current_room_generation().value(),
        ));
    }
    match start_camera_publish_with_device(
        app,
        state,
        preferences.preferred_device(),
        preferences.preferred_mode(),
    )
    .await
    {
        Ok(_) => Ok(StartCameraPublishResult::decided(
            true,
            state.current_room_generation().value(),
        )),
        Err(error) if error == "not in room" => {
            let generation = state.current_room_generation().value();
            match state.deferred_camera().defer(|| state.is_in_room()) {
                CameraDeferral::Deferred => {
                    // Clicked while a join is pending: its commit publishes
                    // it (`publish_deferred_camera`). No error.
                    log::info!(
                        "session: camera turned on while the room join is pending -- publishing once joined"
                    );
                    Ok(StartCameraPublishResult::decided(false, generation))
                }
                CameraDeferral::RetryNow => {
                    // The join committed between our room check and now.
                    log::info!(
                        "session: room join completed during camera start -- retrying publish"
                    );
                    spawn_camera_heal(app, CAMERA_REJOIN_ATTEMPT_SCHEDULE);
                    Ok(StartCameraPublishResult::decided(
                        false,
                        state.current_room_generation().value(),
                    ))
                }
                CameraDeferral::Refused => {
                    // No room and none pending. Intent is cleared so a later
                    // join doesn't surprise-start the camera.
                    state.set_camera_intent(false);
                    emit_camera_intent(app, false);
                    Err(error)
                }
            }
        }
        Err(error) => {
            log::warn!(
                "session: start_camera_publish_command immediate attempt failed, starting self-heal: {error}"
            );
            spawn_camera_heal(app, CAMERA_HEAL_RETRY_BACKOFF);
            Ok(StartCameraPublishResult::decided(
                false,
                state.current_room_generation().value(),
            ))
        }
    }
}

/// A heal loop cancelled by a room change keeps going in the NEW room when
/// the user still wants the camera there and nothing is publishing it.
pub(crate) fn heal_should_follow_new_room(
    generation_current: bool,
    intent: bool,
    publishing: bool,
    in_room: bool,
) -> bool {
    !generation_current && intent && !publishing && in_room
}

fn spawn_camera_heal(app: &tauri::AppHandle, schedule: &'static [Duration]) {
    let heal_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(state) = tauri::Manager::try_state::<crate::session::SessionState>(&heal_app)
        else {
            return;
        };
        ensure_camera_published(&heal_app, &state, schedule).await;
    });
}

/// Called by both platforms' `join_room` when the room commits with a camera
/// waiting for it (`JoinAttempt::commit`). Taken under the control lock, so a
/// camera turned off since the commit stays off.
pub(crate) fn publish_deferred_camera(app: &tauri::AppHandle, generation: RoomGeneration) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(state) = tauri::Manager::try_state::<crate::session::SessionState>(&app) else {
            return;
        };
        if !take_deferred_camera(&state, &generation).await {
            return;
        }
        log::info!("session: publishing the camera turned on while the join was pending");
        emit_camera_intent(&app, true);
        ensure_camera_published(&app, &state, CAMERA_REJOIN_ATTEMPT_SCHEDULE).await;
    });
}

/// The commit's hand-off: the deferred camera becomes the room's intent, if
/// it is still wanted and the committed room is still the current one (a
/// room lost since keeps it pending for the recovery's commit).
pub(crate) async fn take_deferred_camera(
    state: &crate::session::SessionState,
    generation: &RoomGeneration,
) -> bool {
    let _control = state.lock_camera_control().await;
    state
        .deferred_camera()
        .take_camera(generation, || state.set_camera_intent(true))
}

/// Room teardown's hand-off to the deferred camera: a lost connection keeps
/// the meeting pending for the route's recovery; a meeting that is over
/// drops whatever was waiting for it.
pub(crate) fn deferred_camera_room_left(
    app: &tauri::AppHandle,
    state: &crate::session::SessionState,
    connection_lost: bool,
    meeting_over: bool,
) {
    if connection_lost {
        state.deferred_camera().connection_lost();
    } else if meeting_over {
        announce_dropped_deferred_camera(
            app,
            state.current_room_generation().value(),
            state.deferred_camera().end(),
        );
    }
}

/// A deferred camera was dropped: camera OFF with no error, to every surface
/// (`camera-intent-changed` reaches Settings, `camera-publish-state` the
/// meeting route and popover, #461).
/// `generation` is the generation the dropped camera belonged to (or older),
/// never a newer join's, so that join's route filters it out.
pub(crate) fn announce_dropped_deferred_camera(
    app: &tauri::AppHandle,
    generation: u64,
    dropped: bool,
) {
    if !dropped {
        return;
    }
    log::info!("session: meeting over -- dropping the camera that was waiting for its join");
    emit_camera_intent(app, false);
    emit_camera_publish_state(app, generation, false, None);
}

/// Turn OFF the native webcam publish. Idempotent. Clears the user's camera
/// intent (which also cancels any in-flight self-heal loop before its next
/// attempt).
#[tauri::command]
pub async fn stop_camera_publish_command(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
) -> Result<(), ()> {
    stop_camera_publish_intent(&app, state.inner()).await;
    Ok(())
}

/// The body of [`stop_camera_publish_command`]; shared with the #76 runbook
/// driver (`autotest.rs`'s `camera_off` socket command).
pub(crate) async fn stop_camera_publish_intent(
    app: &tauri::AppHandle,
    state: &crate::session::SessionState,
) {
    let _control = state.lock_camera_control().await;
    // Tagged before the teardown awaits: a room joined meanwhile must not
    // take this OFF as its own (PR #503).
    let generation = state.current_room_generation().value();
    state.set_camera_intent(false);
    state.deferred_camera().cancel_camera();
    stop_camera_publish(app, state).await;
    // #461: every surface showing ON (meeting route, popover) must drop it.
    // `camera-intent-changed` alone reaches only Settings. Not in
    // `stop_camera_publish`, so a device switch's stop/start doesn't flicker.
    emit_camera_publish_state(app, generation, false, None);
}

/// The meeting route is gone. A join it left pending (failed, waiting on
/// "Try again" or a recovery that will never run) ends here, so a later
/// camera-on outside any meeting is refused rather than kept for a join that
/// will never come (PR #503). A joined room is untouched.
#[tauri::command]
pub async fn abandon_pending_join(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    room_name: String,
) -> Result<(), ()> {
    // Read before abandoning: never a newer join's generation.
    let generation = state.current_room_generation().value();
    let dropped = abandon_pending_join_inner(state.inner(), &room_name).await;
    announce_dropped_deferred_camera(&app, generation, dropped);
    Ok(())
}

/// The body of [`abandon_pending_join`]: returns whether a deferred camera
/// was dropped (its intent is cleared with it, under the control lock).
pub(crate) async fn abandon_pending_join_inner(
    state: &crate::session::SessionState,
    room_name: &str,
) -> bool {
    let _control = state.lock_camera_control().await;
    let dropped = state.deferred_camera().abandon(room_name);
    if dropped {
        state.set_camera_intent(false);
    }
    dropped
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

    /// The command's own decisions while no room is committed (the exact
    /// calls `start_camera_publish_intent` makes, in its order).
    fn click_while_not_in_room(state: &crate::session::SessionState) -> CameraDeferral {
        state.set_camera_intent(true);
        if state.deferred_camera().defer_if_pending() {
            return CameraDeferral::Deferred;
        }
        state.deferred_camera().defer(|| state.is_in_room())
    }

    /// Wiring guard: both platform joins open the window with the generation
    /// bump, publish at the commit, and hand room teardown to the owner. The
    /// command consults the owner. A pure-function test can't see a deleted
    /// call; there is no `tauri::test` harness to drive `join_room` itself.
    #[test]
    fn both_platform_joins_and_the_command_wire_the_deferred_camera() {
        let mac_join = include_str!("session/room.rs");
        let windows = include_str!("session_stub.rs");
        for (platform, join) in [("macOS", mac_join), ("Windows", windows)] {
            let begin = join.find("state.begin_join(&room_record.id, &room_name)");
            let commit = join.find("if join_attempt.commit() {");
            let publish = join.find("publish_deferred_camera(");
            assert!(begin.is_some(), "{platform} join must bump the generation via begin_join");
            assert!(commit.is_some() && publish.is_some(), "{platform} commit must publish");
            assert!(begin < commit && commit < publish, "{platform}: begin, commit, publish");
            assert!(
                join.contains("deferred_camera_room_left("),
                "{platform} teardown must tell the owner whether the meeting is over"
            );
            assert!(
                !join.contains("begin_join_attempt") && !join.contains("release_camera_deferred"),
                "{platform}: one owner, no per-attempt release"
            );
        }
        // macOS publishes at the room commit, not after the audio tail.
        let kick = mac_join.find("publish_deferred_camera(").expect("kick present");
        let audio_tail = mac_join.find("prepare_microphone(").expect("audio tail present");
        assert!(kick < audio_tail, "macOS camera kick must precede the audio tail");
        let src = include_str!("camera_session.rs");
        let command = &src[src.find("pub(crate) async fn start_camera_publish_intent(").unwrap()..];
        let pending = command.find("defer_if_pending()").expect("asks the owner first");
        let attempt = command.find("start_camera_publish_with_device(").expect("attempt");
        assert!(pending < attempt, "a pending join is asked before the room slot");
        assert!(command.contains("match state.deferred_camera().defer(|| state.is_in_room())"));
    }

    /// Round-1 review: Windows fills the room slot before its late commit. A
    /// click in that gap still belongs to the join (the owner is asked before
    /// the slot), so a join that then fails keeps it for the next attempt.
    #[tokio::test]
    async fn a_click_before_the_late_commit_is_still_the_joins() {
        let state = crate::session::SessionState::default();
        {
            let (_attempt, _) = state.begin_join("room-a", "room-a");
            // Slot filled, not committed: the owner still says pending.
            assert!(state.deferred_camera().defer_if_pending());
        } // media startup saw the room drop: the attempt fails
        let (mut next, _) = state.begin_join("room-a", "room-a");
        assert!(next.commit());
        assert!(take_deferred_camera(&state, next.generation()).await);
    }

    /// Round-1 review: a connection loss landing inside the commit hand-off
    /// either sees the intent set (and carries it over) or leaves the camera
    /// with the owner. It can never consume it with neither.
    #[tokio::test]
    async fn a_connection_loss_cannot_split_the_hand_off() {
        let state = crate::session::SessionState::default();
        let (mut join, _) = state.begin_join("room-a", "room-a");
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        assert!(join.commit());
        // Teardown has invalidated the room but not yet told the owner.
        state.invalidate_room_generation();
        assert!(!take_deferred_camera(&state, join.generation()).await);
        state.deferred_camera().connection_lost();
        assert!(state.deferred_camera().camera_deferred(), "kept for the recovery");
        let (mut rejoin, _) = state.begin_join("room-a", "room-a");
        assert!(rejoin.commit());
        assert!(take_deferred_camera(&state, rejoin.generation()).await);
        assert!(state.camera_intent(), "intent set in the same step as the take");
    }

    /// PR #503 F1: the deferred window opens in the same step as the
    /// generation bump, so a camera turned on mid-join is keyed to that join,
    /// and a stale heal of the previous room cannot drop it.
    #[tokio::test]
    async fn a_camera_turned_on_mid_join_belongs_to_that_join() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        let old_room = state.begin_room_generation();
        state.invalidate_room_generation(); // the room-switch cleanup
        state.set_camera_intent(false);

        let (mut join, dropped) = state.begin_join("room-b", "room-b");
        assert!(!dropped);
        assert!(join.generation().is_current(), "bumped with the window");
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);

        // The previous room's heal runs out of retries.
        let outcome = drive_camera_publish_attempts(
            &state,
            &old_room,
            &[Duration::ZERO],
            || std::future::ready(Err::<(), String>("no frames".to_string())),
            |_d| async {},
        )
        .await;
        assert_eq!(outcome, CameraHealOutcome::Cancelled);

        assert!(join.commit(), "the commit sees the camera");
        assert!(take_deferred_camera(&state, join.generation()).await);
        assert!(state.camera_intent(), "published into the joined room");
    }

    /// PR #503 F3: a failed attempt is not terminal. Connection recovery's
    /// next attempt (same room) adopts the camera and its commit publishes
    /// it; a click between attempts is kept too.
    #[tokio::test]
    async fn a_failed_attempt_keeps_the_camera_for_the_next_attempt() {
        let state = crate::session::SessionState::default();
        {
            let (_attempt, _) = state.begin_join("room-a", "room-a");
            assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        } // attempt 1 fails
        state.invalidate_room_generation();
        state.set_camera_intent(false); // attempt 2's room-switch cleanup
        let (mut second, dropped) = state.begin_join("room-a", "room-a");
        assert!(!dropped, "same meeting: kept");
        assert!(second.commit());
        assert!(take_deferred_camera(&state, second.generation()).await);
        assert!(state.camera_intent());

        // Turned on between two attempts.
        let state = crate::session::SessionState::default();
        drop(state.begin_join("room-a", "room-a"));
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        let (mut next, _) = state.begin_join("room-a", "room-a");
        assert!(next.commit());
    }

    /// PR #503 F2: native can drop the room before room-left reaches the
    /// route. A click then is deferred to the recovery, not refused, so the
    /// route needs no grace period to tell a stale "connected" from a real
    /// refusal.
    #[tokio::test]
    async fn a_click_after_the_connection_dropped_is_kept_for_recovery() {
        let state = crate::session::SessionState::default();
        let (mut join, _) = state.begin_join("room-a", "room-a");
        assert!(!join.commit());
        drop(join);
        // Connected, then lost (what the forced-disconnect cleanup reports).
        state.invalidate_room_generation();
        state.deferred_camera().connection_lost();
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        let (mut rejoin, _) = state.begin_join("room-a", "room-a");
        assert!(rejoin.commit());
        assert!(take_deferred_camera(&state, rejoin.generation()).await);
    }

    /// The camera is dropped only when the meeting is over: leave, meeting
    /// ended, a join of a different room, or the camera turned off.
    #[tokio::test]
    async fn the_deferred_camera_is_dropped_only_when_the_meeting_is_over() {
        let state = crate::session::SessionState::default();
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Refused, "no meeting");

        drop(state.begin_join("room-a", "room-a"));
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        assert!(state.deferred_camera().end(), "leave drops it");
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Refused);

        drop(state.begin_join("room-a", "room-a"));
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        let (mut other, dropped) = state.begin_join("room-b", "room-b");
        assert!(dropped, "never carried into a different room");
        assert!(!other.commit());

        let (mut join, _) = state.begin_join("room-c", "room-c");
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        state.deferred_camera().cancel_camera(); // camera off mid-join
        assert!(!join.commit());
    }

    /// PR #503 round 2, item 3: a failed join the meeting route then abandons
    /// ("Back to rooms", a recovery that gave up) must not leave the owner
    /// pending, or a later camera-on outside any meeting is deferred to a
    /// join that will never come and waits forever.
    #[tokio::test]
    async fn an_abandoned_failed_join_refuses_a_later_camera_on() {
        let state = crate::session::SessionState::default();
        drop(state.begin_join("room-a", "room-a")); // the join fails
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        assert!(state.camera_intent());
        // The route is gone (`abandon_pending_join` from its dispose).
        assert!(abandon_pending_join_inner(&state, "room-a").await, "the waiting camera is dropped");
        assert!(!state.camera_intent(), "its intent drops with it");
        assert_eq!(
            click_while_not_in_room(&state),
            CameraDeferral::Refused,
            "outside any meeting: refused, never kept"
        );
        // Abandoned with nothing waiting: still ends the pending meeting.
        drop(state.begin_join("room-a", "room-a"));
        assert!(!abandon_pending_join_inner(&state, "room-a").await);
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Refused);
    }

    /// The route can go while its join is still in flight. The abandon sent
    /// at dispose leaves that join alone (it may be a replacement route's
    /// join of the same room); the one sent when the route's own join
    /// settles ends it.
    #[tokio::test]
    async fn a_join_in_flight_at_dispose_ends_when_it_settles() {
        let state = crate::session::SessionState::default();
        let attempt = state.begin_join("room-a", "room-a");
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        assert!(!abandon_pending_join_inner(&state, "room-a").await, "in flight: untouched");
        drop(attempt); // ...it fails, and the route's join settles
        assert!(abandon_pending_join_inner(&state, "room-a").await);
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Refused);
    }

    /// Round-2 review: the old route's late abandon for the same room lands
    /// while a replacement route's join is in flight. The replacement keeps
    /// its camera.
    #[tokio::test]
    async fn a_late_abandon_never_ends_a_replacement_routes_join() {
        let state = crate::session::SessionState::default();
        drop(state.begin_join("room-a", "room-a")); // old route: failed
        let (mut join, _) = state.begin_join("room-a", "room-a"); // replacement
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        assert!(!abandon_pending_join_inner(&state, "room-a").await);
        assert!(state.camera_intent());
        assert!(join.commit(), "the replacement still publishes its camera");
    }

    /// Abandoning never touches a joined room (a route remount, or a dispose
    /// after leave), but a recovery that the route stopped running is over.
    #[tokio::test]
    async fn abandoning_leaves_a_joined_room_alone() {
        let state = crate::session::SessionState::default();
        let (mut join, _) = state.begin_join("room-a", "room-a");
        assert!(!join.commit());
        drop(join);
        state.set_camera_intent(true);
        assert!(!abandon_pending_join_inner(&state, "room-a").await);
        assert!(state.camera_intent(), "a joined room's camera is untouched");

        state.invalidate_room_generation();
        state.deferred_camera().connection_lost();
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        assert!(abandon_pending_join_inner(&state, "room-a").await, "no route left to recover it");
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Refused);
    }

    /// Abandon is scoped to the route's room: a late abandon from the route
    /// for room A never ends room B's pending join and its camera.
    #[tokio::test]
    async fn a_late_abandon_never_ends_another_rooms_join() {
        let state = crate::session::SessionState::default();
        let (mut join, _) = state.begin_join("room-b", "room-b");
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        assert!(!abandon_pending_join_inner(&state, "room-a").await);
        assert!(state.camera_intent());
        assert!(join.commit(), "room B still publishes its camera");
    }

    /// Round-3 review: a start's result and every camera-publish-state share
    /// one order. An event emitted before the result was decided has a seq
    /// at or below the result's; one emitted after has a higher seq.
    #[test]
    fn a_start_result_is_ordered_against_emitted_events() {
        let before = camera_publish_state_event(1, false, None);
        let result = StartCameraPublishResult::decided(true, 1);
        let after = camera_publish_state_event(1, false, None);
        assert!(before.seq <= result.seq, "emitted before: not newer than the start");
        assert!(after.seq > result.seq, "emitted after: newer than the start");
        assert!(after.seq > before.seq);
    }

    /// The stop/start pair is serialized by the camera control lock: a stop
    /// that holds it emits before a waiting start decides, and a stop that
    /// waits for a start emits after it (the seq read is under the lock).
    #[tokio::test]
    async fn the_control_lock_orders_a_stop_against_a_start() {
        let state = crate::session::SessionState::default();
        let control = state.lock_camera_control().await;
        let start = async {
            let _control = state.lock_camera_control().await;
            StartCameraPublishResult::decided(true, 1)
        };
        let stop = async {
            let event = camera_publish_state_event(1, false, None);
            drop(control);
            event
        };
        let (result, stop_off) = tokio::join!(start, stop);
        assert!(stop_off.seq <= result.seq, "an OFF before the start never stops it");
        let control = state.lock_camera_control().await;
        let later_off = camera_publish_state_event(1, false, None);
        drop(control);
        assert!(later_off.seq > result.seq, "an OFF after the start wins");
    }

    /// Wiring guard: every start result is decided under the control lock,
    /// and every emit is stamped through the one counter.
    #[test]
    fn start_results_and_emits_use_the_shared_seq() {
        let src = include_str!("camera_session.rs");
        let body = &src[src.find("pub(crate) async fn start_camera_publish_intent(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let lock = body.find("let _control = state.lock_camera_control().await;").unwrap();
        let first = body.find("StartCameraPublishResult::decided(").unwrap();
        assert!(lock < first, "decided under the control lock");
        assert!(!body.contains("StartCameraPublishResult {"), "no seq-less literal");
        let emit = &src[src.find("pub(crate) fn emit_camera_publish_state(").unwrap()..];
        let emit = &emit[..emit.find("\n}\n").unwrap()];
        assert!(emit.contains("camera_publish_state_event(generation, publishing, error)"));
    }

    /// Round-4 review: the heal gives up (clearing the intent under the
    /// control lock), the user turns the camera on again and that start
    /// returns, and only then is the old heal's failure emitted. Its seq was
    /// reserved with the clear, so it is older than the start's and the route
    /// drops it instead of turning the new camera OFF.
    #[tokio::test]
    async fn a_late_heal_failure_orders_before_the_start_that_followed_it() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        let generation = state.begin_room_generation();
        let outcome = drive_camera_publish_attempts(
            &state,
            &generation,
            &[Duration::ZERO],
            || std::future::ready(Err::<(), String>("no frames".to_string())),
            |_d| async {},
        )
        .await;
        let CameraHealOutcome::TerminalFailure(_, failure_seq) = outcome else {
            panic!("expected TerminalFailure, got {outcome:?}");
        };
        // The user's start takes the lock after the give-up released it
        // (`start_camera_publish_intent` decides its result under it).
        let start = {
            let _control = state.lock_camera_control().await;
            state.set_camera_intent(true);
            StartCameraPublishResult::decided(true, generation.value())
        };
        // Some other surface emits meanwhile; the heal's emit comes last.
        let _other = camera_publish_state_event(generation.value(), true, None);
        assert!(
            failure_seq <= start.seq,
            "the stale failure ({failure_seq}) must not be newer than the start ({})",
            start.seq
        );
        // And `ensure_camera_published` emits it with that reserved seq, not
        // a fresh one stamped at emit time.
        let src = include_str!("camera_session.rs");
        let arm = &src[src.find("CameraHealOutcome::TerminalFailure(error, seq) => {").unwrap()..];
        let arm = &arm[..arm.find("\n}\n").unwrap()];
        assert!(arm.contains("send_camera_publish_state("), "{arm}");
        assert!(arm.contains("            seq,\n"), "sent with the reserved seq");
        assert!(!arm.contains("camera_publish_state_event("), "never re-stamped");
        assert!(!arm.contains("emit_camera_publish_state("), "never re-stamped");
    }

    /// Wiring guard for item 3: the command is registered on both platforms.
    #[test]
    fn abandon_pending_join_is_registered_on_both_platforms() {
        let lib = include_str!("lib.rs");
        assert_eq!(lib.matches("camera_session::abandon_pending_join,").count(), 2);
    }

    /// Camera off lands between the commit and its hand-off: it stays off.
    #[tokio::test]
    async fn a_camera_turned_off_after_the_commit_stays_off() {
        let state = crate::session::SessionState::default();
        let (mut join, _) = state.begin_join("room-a", "room-a");
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        assert!(join.commit());
        state.set_camera_intent(false);
        state.deferred_camera().cancel_camera();
        assert!(!take_deferred_camera(&state, join.generation()).await);
        assert!(!state.camera_intent());

        // A room lost before the hand-off keeps it for the recovery's commit.
        let (mut join, _) = state.begin_join("room-a", "room-a");
        assert_eq!(click_while_not_in_room(&state), CameraDeferral::Deferred);
        assert!(join.commit());
        state.invalidate_room_generation();
        state.deferred_camera().connection_lost();
        assert!(!take_deferred_camera(&state, join.generation()).await);
        assert!(state.deferred_camera().camera_deferred());
    }

    /// PR #503 F5: a heal whose room is gone emits nothing: a stale OFF would
    /// stop a camera the user started for the new room.
    #[test]
    fn a_stale_terminal_heal_announces_nothing() {
        let src = include_str!("camera_session.rs");
        let start = src.find("CameraHealOutcome::TerminalFailure(error, seq) => {").expect("arm");
        let arm = &src[start..];
        let stale = &arm[arm.find("if !generation.is_current() {").expect("stale check")..];
        let stale = &stale[..stale.find("return;").expect("return")];
        assert!(!stale.contains("emit_") && !stale.contains("announce"), "{stale}");
    }

    /// Fails every attempt; while `rooms_left > 0`, each attempt also lands a
    /// new room mid-attempt (a fast rejoin during the publish).
    fn fail_and_change_room(
        state: &crate::session::SessionState,
        attempts: &AtomicUsize,
        rooms_left: &AtomicUsize,
    ) -> Result<(), String> {
        attempts.fetch_add(1, Ordering::SeqCst);
        if rooms_left.load(Ordering::SeqCst) > 0 {
            rooms_left.fetch_sub(1, Ordering::SeqCst);
            state.begin_room_generation();
            return Err("not in room".to_string());
        }
        Ok(())
    }

    /// PR #503 review F2: the old room's final attempt fails after a new room
    /// landed. That stale failure must not clear the NEW room's intent; the
    /// heal follows the camera into the new room instead.
    #[tokio::test]
    async fn a_stale_final_failure_keeps_the_new_rooms_camera() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        let generation = state.begin_room_generation();
        let attempts = AtomicUsize::new(0);
        let rooms_left = AtomicUsize::new(1);
        let outcome = drive_camera_publish_attempts(
            &state,
            &generation,
            &[Duration::ZERO],
            || std::future::ready(fail_and_change_room(&state, &attempts, &rooms_left)),
            |_d| async {},
        )
        .await;
        assert_eq!(outcome, CameraHealOutcome::Cancelled);
        assert!(state.camera_intent(), "the new room's camera intent must survive");

        let rooms_left = AtomicUsize::new(1);
        let attempts = AtomicUsize::new(0);
        let (outcome, generation) = drive_camera_heal_following_rooms(
            &state,
            &[Duration::ZERO],
            || std::future::ready(fail_and_change_room(&state, &attempts, &rooms_left)),
            |_d| async {},
            || true,
        )
        .await;
        assert_eq!(outcome, CameraHealOutcome::Published);
        assert!(generation.is_current());
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "retried in the new room");
    }

    /// Re-review of F2: the generation check and the intent clear must not
    /// straddle a camera action for a new room. A click that holds the control
    /// lock (mid-join) and lands a new room must win: the stale clear waits
    /// for the lock, re-checks, and leaves the new intent alone.
    #[tokio::test]
    async fn a_stale_clear_waits_for_an_in_flight_camera_action() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        let generation = state.begin_room_generation();
        let control = state.lock_camera_control().await;
        let heal = drive_camera_publish_attempts(
            &state,
            &generation,
            &[Duration::ZERO],
            || std::future::ready(Err::<(), String>("no frames".to_string())),
            |_d| async {},
        );
        let click = async {
            tokio::task::yield_now().await;
            state.begin_room_generation();
            state.set_camera_intent(true);
            drop(control);
        };
        let (outcome, ()) = tokio::join!(heal, click);
        assert_eq!(outcome, CameraHealOutcome::Cancelled);
        assert!(state.camera_intent(), "the new room's camera must survive");
    }

    /// PR #503 review F3: however many rooms the camera crosses, the heal
    /// never exits leaving the intent ON with nothing publishing it.
    #[tokio::test]
    async fn the_heal_follows_the_camera_through_any_number_of_rooms() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        state.begin_room_generation();
        let attempts = AtomicUsize::new(0);
        let rooms_left = AtomicUsize::new(6);
        let (outcome, generation) = drive_camera_heal_following_rooms(
            &state,
            &[Duration::ZERO, Duration::from_secs(1)],
            || std::future::ready(fail_and_change_room(&state, &attempts, &rooms_left)),
            |_d| async {},
            || true,
        )
        .await;
        assert_eq!(outcome, CameraHealOutcome::Published);
        assert!(generation.is_current());
        assert!(state.camera_intent());
        assert_eq!(attempts.load(Ordering::SeqCst), 7);
    }

    /// Following needs a real room change each time: a cancel in the same
    /// room (camera turned off) ends the heal.
    #[tokio::test]
    async fn the_heal_stops_when_the_camera_is_turned_off() {
        let state = crate::session::SessionState::default();
        state.set_camera_intent(true);
        state.begin_room_generation();
        let attempts = AtomicUsize::new(0);
        let (outcome, _) = drive_camera_heal_following_rooms(
            &state,
            &[Duration::ZERO, Duration::from_secs(1)],
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err::<(), String>("no frames".to_string()))
            },
            |_d| {
                state.set_camera_intent(false);
                async {}
            },
            || true,
        )
        .await;
        assert_eq!(outcome, CameraHealOutcome::Cancelled);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn heal_follows_the_camera_into_a_new_room_only_when_still_wanted() {
        assert!(heal_should_follow_new_room(false, true, false, true));
        assert!(!heal_should_follow_new_room(true, true, false, true), "same room: a real cancel");
        assert!(!heal_should_follow_new_room(false, false, false, true), "camera turned off");
        assert!(!heal_should_follow_new_room(false, true, true, true), "already publishing");
        assert!(!heal_should_follow_new_room(false, true, false, false), "no room yet: the join kicks");
    }

    use std::sync::atomic::AtomicUsize;

    #[test]
    fn camera_preview_log_line_is_fixed_text_with_a_slug_reason() {
        assert_eq!(
            camera_preview_log_line("acquired", None),
            "settings: camera preview acquired"
        );
        assert_eq!(
            camera_preview_log_line("released", Some("meeting-camera")),
            "settings: camera preview released reason=meeting-camera"
        );
        // Webview text cannot forge a second line or smuggle a device label.
        assert_eq!(
            camera_preview_log_line("failed", Some("NotReadableError\nsession: x 'FaceTime HD'")),
            "settings: camera preview failed reason=NotReadableErrorsessionxFaceTimeHD"
        );
        assert_eq!(
            camera_preview_log_line("whatever", Some("")),
            "settings: camera preview unknown"
        );
    }

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
        let released_when_reported = Arc::new(std::sync::Mutex::new(None));
        let observed = released_when_reported.clone();
        let observed_flag = released.clone();
        let outcome = await_first_frame_or_release(
            FakeCapture {
                released: released.clone(),
            },
            wedged_rx,
            Duration::from_millis(100),
            move |_| *observed.lock().unwrap() = Some(observed_flag.load(Ordering::SeqCst)),
        )
        .await;
        // #274: the failure is reported BEFORE the (possibly hanging) release.
        assert_eq!(
            *released_when_reported.lock().unwrap(),
            Some(false),
            "the failure must be reported before the capture is released"
        );
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
            |error| panic!("a successful wait reported a failure: {error}"),
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

    /// #461 (petal.log.2026-10-29): a stop queued on the control lock while
    /// the heal's winning attempt holds it must win the report. The old
    /// unlocked check ran before the stop was polled and announced ON; the
    /// stop then tore the camera down, leaving the UI ON with nothing sent.
    #[tokio::test]
    async fn camera_heal_does_not_announce_a_publish_a_queued_stop_tears_down() {
        let state = Arc::new(crate::session::SessionState::default());
        state.set_camera_intent(true);
        let generation = state.begin_room_generation();
        let events = Arc::new(Mutex::new(Vec::<&'static str>::new()));

        let stop_state = state.clone();
        let stop_events = events.clone();
        let stop = Arc::new(Mutex::new(None::<tokio::task::JoinHandle<()>>));
        let outcome = drive_camera_publish_attempts(
            &state,
            &generation,
            &[Duration::ZERO],
            || {
                let attempt_state = state.clone();
                let stop_state = stop_state.clone();
                let stop_events = stop_events.clone();
                let stop = stop.clone();
                async move {
                    let _control = attempt_state.lock_camera_control().await;
                    // The user's stop arrives mid-attempt and queues on the lock
                    // exactly as `stop_camera_publish_intent` does.
                    *stop.lock_unpoisoned() = Some(tokio::spawn(async move {
                        let _control = stop_state.lock_camera_control().await;
                        stop_state.set_camera_intent(false);
                        stop_events.lock_unpoisoned().push("publishing:false");
                    }));
                    tokio::task::yield_now().await;
                    Ok(())
                }
            },
            |_d| async {},
        )
        .await;
        assert_eq!(outcome, CameraHealOutcome::Published);

        let announced = announce_heal_published(
            &state,
            || state.camera_intent(),
            || events.lock_unpoisoned().push("publishing:true"),
        )
        .await;
        let stop = stop.lock_unpoisoned().take().expect("stop was queued");
        stop.await.expect("stop task");

        assert!(!announced, "a publish torn down by a queued stop must not be announced");
        assert_eq!(*events.lock_unpoisoned(), vec!["publishing:false"]);
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
            CameraHealOutcome::TerminalFailure(error, _) => {
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
                    stall_cause: CameraStallCauseTag::NotApplicable,
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
