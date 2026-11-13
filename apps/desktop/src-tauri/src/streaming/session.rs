//! The client-mode live-stream session: Idle -> Connecting -> Live <-> Paused
//! (slate) -> Reconnecting -> Ended/Failed. One per app (one stream per room,
//! one room at a time).
//!
//! Owns the frame tap binding, the audio taps, the encode thread and the RTMP
//! publisher; announces `petal.live` and waits for the room to accept it
//! before any media goes out, and clears it on every way out (stop, failure,
//! slate timeout, leave, quit).
//! Emits `stream-session-changed` (about once a second while live, for the
//! sharer's notice) and `stream-session-ended`.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio_util::sync::CancellationToken;

use super::capability::{audio_output_available_async, client_encoder_async, ClientEncoder};
use super::destinations::DestinationStore;
use super::keystore::platform_store;
use super::plan::{
    plan_stream_output, StreamOrientation, StreamPlan, StreamPlanInput, StreamResolution,
};
use super::rtmp::{PublisherState, RtmpDestination};
use super::wire::{
    encode_client_live_attribute, ClientLiveAttribute, ClientLiveState, StreamMode, STREAM_LIVE_ATTRIBUTE,
};
use crate::sync_ext::MutexExt;

pub const STREAM_SESSION_CHANGED_EVENT: &str = "stream-session-changed";
pub const STREAM_SESSION_ENDED_EVENT: &str = "stream-session-ended";
/// A destination the user asked to save couldn't be stored after going live.
pub const STREAM_DESTINATION_SAVE_FAILED_EVENT: &str = "stream-destination-save-failed";
/// Two streams started together: the later one (by `startedAt`, then
/// identity) stops with this.
pub const LOST_ARBITRATION_ERROR: &str = "Someone else in this meeting went live first";

/// A new destination to save once the server accepts the stream, so a wrong
/// URL or key is never stored. `RtmpDestination`'s Debug redacts the key.
#[derive(Debug)]
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
struct PendingSave {
    name: String,
    destination: RtmpDestination,
}
/// How long go-live waits for the bound share's first tapped frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// How long go-live waits for the room to accept `petal.live` before any
/// media goes out.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(5);
pub const ANNOUNCE_FAILED_ERROR: &str = "Couldn't show the meeting that you're live. Check your connection and try again.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionPhase {
    Connecting,
    Live,
    Paused,
    Reconnecting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSessionSnapshot {
    pub phase: SessionPhase,
    pub window_id: u32,
    pub started_at: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub target_bitrate_bps: u32,
    pub actual_bitrate_bps: u64,
    pub dropped_frames: u64,
    pub slate_remaining_s: Option<u32>,
    pub reconnect_attempt: Option<u32>,
    pub hardware_encoder: bool,
    /// No audio output device: other people in the meeting aren't heard on
    /// the stream (Petal never plays their audio). Checked every 2 s.
    pub no_audio_output: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    User,
    /// Left the meeting (including Quit Petal, which leaves first).
    Left,
    SlateTimeout,
    Failed(String),
}

impl StopReason {
    fn wire(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Left => "left",
            Self::SlateTimeout => "slate-timeout",
            Self::Failed(_) => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EndedPayload {
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DestinationChoice {
    Saved {
        id: String,
    },
    #[serde(rename_all = "camelCase")]
    Custom {
        server_url: String,
        stream_key: String,
        #[serde(default)]
        save: bool,
        #[serde(default)]
        name: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoLiveRequest {
    pub window_id: u32,
    pub destination: DestinationChoice,
    pub resolution: StreamResolution,
    #[serde(default)]
    pub orientation: StreamOrientation,
    #[serde(default)]
    pub max_bitrate_bps: Option<u32>,
}

/// What the dialog shows before going live.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamPreview {
    pub capture_width: u32,
    pub capture_height: u32,
    pub share_fps: u32,
    pub plan: StreamPlan,
    /// `client_encoder` can go live (kept for older dialogs).
    pub client_available: bool,
    pub client_encoder: ClientEncoder,
    /// This computer has an audio output device (without one, other people
    /// aren't heard on the stream).
    pub audio_output: bool,
}

/// The dialog's preview for a share of this size and cadence.
pub fn build_preview(
    (capture_width, capture_height): (u32, u32),
    share_fps: u32,
    resolution: StreamResolution,
    orientation: StreamOrientation,
    max_bitrate_bps: Option<u32>,
    client_encoder: ClientEncoder,
) -> StreamPreview {
    let plan = plan_stream_output(&StreamPlanInput {
        capture_width,
        capture_height,
        share_fps,
        resolution,
        orientation,
        max_bitrate_bps,
        cpu_encode: client_encoder == ClientEncoder::Cpu,
    });
    StreamPreview {
        capture_width,
        capture_height,
        share_fps,
        plan,
        client_available: client_encoder.available(),
        client_encoder,
        audio_output: true,
    }
}

struct LiveSession {
    window_id: Arc<std::sync::atomic::AtomicU32>,
    started_at: u64,
    target_bitrate_bps: u32,
    /// The fixed canvas, for holding a Petal View region to its shape.
    canvas: (u32, u32),
    cancel: CancellationToken,
    room: Arc<livekit::Room>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pipeline: Option<super::pipeline::PipelineHandle>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    mic: Option<super::mic::StreamMic>,
    publisher: Option<tokio::task::JoinHandle<PublisherState>>,
    last_snapshot: Arc<Mutex<Option<StreamSessionSnapshot>>>,
    live_attribute: LiveAttributeGate,
}

/// Whether this session may still write `petal.live`. The monitor sets it
/// only through [`LiveAttributeGate::write_if_open`]; stopping closes the gate
/// before clearing, so a monitor iteration still in flight can't put LIVE
/// back after the stream ended.
#[derive(Debug, Clone)]
struct LiveAttributeGate(Arc<Mutex<bool>>);

impl LiveAttributeGate {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(true)))
    }

    /// Run `write` unless the gate is closed; `write` runs under the gate's
    /// lock, so [`Self::close`] waits for it.
    fn write_if_open(&self, write: impl FnOnce()) -> bool {
        let open = self.0.lock_unpoisoned();
        if *open {
            write();
        }
        *open
    }

    fn close(&self) {
        *self.0.lock_unpoisoned() = false;
    }
}

fn slot() -> &'static Mutex<Option<LiveSession>> {
    static SLOT: OnceLock<Mutex<Option<LiveSession>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Serializes go-live/stop/resume so two clicks can't race.
fn transition() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn destinations_store(app: &AppHandle) -> Result<DestinationStore, String> {
    let dir = tauri::Manager::path(app).app_data_dir().map_err(|e| e.to_string())?;
    Ok(DestinationStore::new(&dir, platform_store()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn room_context(app: &AppHandle) -> Result<(Arc<livekit::Room>, String, Vec<u32>), String> {
    session_state(app).live_stream_context().ok_or_else(|| "Join a meeting first".to_string())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn room_context(_app: &AppHandle) -> Result<(Arc<livekit::Room>, String, Vec<u32>), String> {
    Err(CLIENT_UNAVAILABLE.to_string())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const CLIENT_UNAVAILABLE: &str = super::capability::UNSUPPORTED_PLATFORM_ERROR;

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn session_state(app: &AppHandle) -> tauri::State<'_, crate::session::SessionState> {
    tauri::Manager::state::<crate::session::SessionState>(app)
}

fn capture_geometry(app: &AppHandle, window_id: u32) -> Option<(u32, u32)> {
    #[cfg(target_os = "windows")]
    {
        let _ = app;
        crate::share_capture_options::capture_geometry(window_id)
    }
    #[cfg(target_os = "macos")]
    {
        session_state(app).live_stream_capture(window_id).map(|(size, _)| size)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (app, window_id);
        None
    }
}

/// The share's frame-rate ceiling (Windows: the per-share choice; macOS: the
/// share's full-quality cadence, which a live share keeps).
fn share_fps(app: &AppHandle, window_id: u32) -> u32 {
    #[cfg(target_os = "windows")]
    {
        let _ = app;
        crate::share_capture_options::pending(window_id)
    }
    #[cfg(target_os = "macos")]
    {
        session_state(app).live_stream_capture(window_id).map_or(30, |(_, fps)| fps)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (app, window_id);
        30
    }
}

async fn wait_for_first_frame(tap: &super::tap::LiveTap) -> Option<(u32, u32)> {
    let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    while Instant::now() < deadline {
        if let Some((frame, _)) = tap.mailbox.latest() {
            return Some((frame.width, frame.height));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// How long stopping waits for the cleared `petal.live` to reach the room.
const CLEAR_LIVE_ATTRIBUTE_TIMEOUT: Duration = Duration::from_secs(3);

/// Write `petal.live` for a stream about to start and wait until the room
/// has it. On failure it is cleared again (the keeper keeps retrying that).
#[cfg(any(target_os = "windows", target_os = "macos"))]
async fn announce(room: &livekit::Room, attribute: ClientLiveAttribute) -> bool {
    let Some(value) = set_live_attribute(room, Some(attribute)) else { return false };
    let Some(keeper) = super::live_state::keeper_for(room) else { return false };
    if keeper.wait_confirmed(STREAM_LIVE_ATTRIBUTE, &value, ANNOUNCE_TIMEOUT).await {
        return true;
    }
    log::warn!("streaming: the room didn't accept {STREAM_LIVE_ATTRIBUTE} within {ANNOUNCE_TIMEOUT:?}; not going live");
    set_live_attribute(room, None);
    false
}

/// Set our `petal.live` (`None` clears it). The room's attribute keeper
/// writes it until it sticks and again after every reconnect; returns the
/// value it will hold.
fn set_live_attribute(room: &livekit::Room, value: Option<ClientLiveAttribute>) -> Option<String> {
    // An empty value deletes the key.
    let encoded = value.as_ref().map(encode_client_live_attribute).unwrap_or_default();
    match super::live_state::keeper_for(room) {
        Some(keeper) => {
            keeper.set(STREAM_LIVE_ATTRIBUTE, &encoded);
            Some(encoded)
        }
        None => {
            log::warn!("streaming: no attribute writer for this room; {STREAM_LIVE_ATTRIBUTE} not set");
            None
        }
    }
}

pub async fn preview(
    app: &AppHandle,
    window_id: u32,
    resolution: StreamResolution,
    orientation: StreamOrientation,
    max_bitrate_bps: Option<u32>,
) -> Result<StreamPreview, String> {
    let (_, _, shares) = room_context(app)?;
    if !shares.contains(&window_id) {
        return Err("Share this window first".into());
    }
    let capture = capture_geometry(app, window_id).unwrap_or((1920, 1080));
    let share_fps = share_fps(app, window_id);
    let client_encoder = client_encoder_async().await;
    Ok(StreamPreview {
        audio_output: audio_output_available_async().await,
        ..build_preview(capture, share_fps, resolution, orientation, max_bitrate_bps, client_encoder)
    })
}

pub async fn go_live(app: &AppHandle, request: GoLiveRequest) -> Result<StreamSessionSnapshot, String> {
    let _transition = transition().lock().await;
    if slot().lock_unpoisoned().is_some() {
        return Err("You're already live".into());
    }
    let (room, _identity, shares) = room_context(app)?;
    if !shares.contains(&request.window_id) {
        return Err("Share this window first".into());
    }
    if super::live_state::derive_from_room(&room).live {
        return Err("This meeting is already being live streamed".into());
    }
    if let Some(refusal) = super::capability::refusal(client_encoder_async().await) {
        return Err(refusal.into());
    }
    let mut pending_save = None;
    let destination = match &request.destination {
        DestinationChoice::Saved { id } => destinations_store(app)?.resolve(id).map_err(|e| e.to_string())?,
        DestinationChoice::Custom { server_url, stream_key, save, name } => {
            let destination = RtmpDestination::new(server_url, stream_key).map_err(|e| e.to_string())?;
            if *save {
                // Refuse now if it couldn't be saved; save once live.
                destinations_store(app)?
                    .ensure_room(name, &destination)
                    .map_err(|e| e.to_string())?;
                pending_save = Some(PendingSave { name: name.clone(), destination: destination.clone() });
            }
            destination
        }
    };
    start_client(app, room, request, destination, pending_save).await
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn start_client(
    _app: &AppHandle,
    _room: Arc<livekit::Room>,
    _request: GoLiveRequest,
    _destination: RtmpDestination,
    _pending_save: Option<PendingSave>,
) -> Result<StreamSessionSnapshot, String> {
    Err(CLIENT_UNAVAILABLE.into())
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
async fn start_client(
    app: &AppHandle,
    room: Arc<livekit::Room>,
    request: GoLiveRequest,
    destination: RtmpDestination,
    pending_save: Option<PendingSave>,
) -> Result<StreamSessionSnapshot, String> {
    use super::mixer::AudioMixer;
    use super::queue::MediaQueue;
    use super::rtmp::{run_publisher, PublisherStats};
    use super::pipeline::{spawn, PipelineConfig};

    let window_id = request.window_id;
    let tap = super::tap::start(window_id);
    let Some((capture_width, capture_height)) = wait_for_first_frame(&tap).await else {
        super::tap::stop();
        return Err("This share isn't sending frames yet. Try again in a moment.".into());
    };
    let plan = plan_stream_output(&StreamPlanInput {
        capture_width,
        capture_height,
        share_fps: share_fps(app, window_id),
        resolution: request.resolution,
        orientation: request.orientation,
        max_bitrate_bps: request.max_bitrate_bps,
        cpu_encode: client_encoder_async().await == ClientEncoder::Cpu,
    });
    log::info!(
        "streaming: going live window={window_id} {capture_width}x{capture_height} -> {}x{} ({:?}) @ {} fps, {} bps",
        plan.width,
        plan.height,
        plan.resolution,
        plan.fps,
        plan.bitrate_bps,
    );

    // Tell the room first: no media leaves this computer until everyone in
    // the meeting can see LIVE.
    let started_at = now_ms();
    let announced = ClientLiveAttribute {
        mode: StreamMode::Client,
        started_at,
        window_id,
        state: ClientLiveState::Live,
    };
    if !announce(&room, announced).await {
        super::tap::stop();
        return Err(ANNOUNCE_FAILED_ERROR.into());
    }
    super::live_state::refresh(app, &room);

    let cancel = CancellationToken::new();
    let queue = Arc::new(MediaQueue::new());
    let mixer = Arc::new(Mutex::new(AudioMixer::new()));
    // What Petal plays out, for the stream mic's echo cancellation; joins at
    // once so it never lags the echo it describes.
    let reference = Arc::new(Mutex::new(AudioMixer::with_prime(0)));
    super::audio_tap::start(room.clone(), mixer.clone(), reference.clone(), cancel.child_token());
    let mic = super::mic::StreamMic::start(Arc::new(MeetingMicState(app.clone())), mixer.clone(), reference);
    let pipeline = match spawn(
        PipelineConfig {
            canvas: (plan.width, plan.height),
            fps: plan.fps,
            bitrate_bps: plan.bitrate_bps,
            max_bitrate_bps: plan.max_bitrate_bps,
        },
        tap,
        mixer,
        queue.clone(),
    ) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            cancel.cancel();
            release_mic(mic).await;
            super::tap::stop();
            set_live_attribute(&room, None);
            super::live_state::refresh(app, &room);
            return Err(error);
        }
    };
    let stats = Arc::new(PublisherStats::default());
    let (state_tx, state_rx) = tokio::sync::watch::channel(PublisherState::Connecting);
    let live_attribute = LiveAttributeGate::new();
    let publisher = tokio::spawn(run_publisher(
        destination,
        queue.clone(),
        stats.clone(),
        state_tx,
        cancel.child_token(),
        None,
    ));
    let window = Arc::new(std::sync::atomic::AtomicU32::new(window_id));
    let last_snapshot = Arc::new(Mutex::new(None));
    let pipeline_stats = pipeline.stats.clone();
    *slot().lock_unpoisoned() = Some(LiveSession {
        window_id: window.clone(),
        started_at,
        target_bitrate_bps: plan.bitrate_bps,
        canvas: (plan.width, plan.height),
        cancel: cancel.clone(),
        room: room.clone(),
        pipeline: Some(pipeline),
        mic: Some(mic),
        publisher: Some(publisher),
        last_snapshot: last_snapshot.clone(),
        live_attribute: live_attribute.clone(),
    });
    super::region_lock::bind(app, Some((window_id, (plan.width, plan.height))));
    #[cfg(target_os = "macos")]
    reconcile_share_quality(app, &[window_id]);

    let snapshot = StreamSessionSnapshot {
        phase: SessionPhase::Connecting,
        window_id,
        started_at,
        width: plan.width,
        height: plan.height,
        fps: plan.fps,
        target_bitrate_bps: plan.bitrate_bps,
        actual_bitrate_bps: 0,
        dropped_frames: 0,
        slate_remaining_s: None,
        reconnect_attempt: None,
        hardware_encoder: false,
        no_audio_output: !audio_output_available_async().await,
    };
    *last_snapshot.lock_unpoisoned() = Some(snapshot.clone());
    let _ = app.emit(STREAM_SESSION_CHANGED_EVENT, &snapshot);
    super::streamer_mode::set_live(app, true);
    tokio::spawn(monitor(
        app.clone(),
        room,
        cancel,
        state_rx,
        stats,
        pipeline_stats,
        queue,
        window,
        last_snapshot,
        snapshot,
        pending_save,
        live_attribute,
        announced,
    ));
    Ok(current().unwrap_or_else(|| placeholder_snapshot(window_id)))
}

/// The meeting's mic as the stream mic sees it: mute and device, read-only.
#[cfg(any(target_os = "windows", target_os = "macos"))]
struct MeetingMicState(AppHandle);

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl MeetingMicState {
    fn track(&self) -> Option<Arc<crate::transport::audio::MicTrack>> {
        #[cfg(target_os = "windows")]
        {
            session_state(&self.0).live_stream_microphone()
        }
        #[cfg(target_os = "macos")]
        {
            session_state(&self.0).current_mic()
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl super::mic::MeetingMic for MeetingMicState {
    fn muted(&self) -> Option<bool> {
        self.track().map(|track| track.is_muted())
    }

    fn device_name(&self) -> Option<String> {
        self.track().and_then(|track| track.current_device_name())
    }
}

/// macOS keeps the live share at full quality even unfocused (see
/// `session::share::apply_quality`): shares the stream moves onto or off
/// re-check theirs.
#[cfg(target_os = "macos")]
fn reconcile_share_quality(app: &AppHandle, windows: &[u32]) {
    let app = app.clone();
    let windows = windows.to_vec();
    tokio::spawn(async move {
        let state = session_state(&app);
        for window_id in windows {
            crate::session::reconcile_quality_for_window(state.inner(), window_id).await;
        }
    });
}

/// Stopping the mic joins its capture thread: keep that off the runtime.
#[cfg(any(target_os = "windows", target_os = "macos"))]
async fn release_mic(mic: super::mic::StreamMic) {
    let _ = tokio::task::spawn_blocking(move || drop(mic)).await;
}

fn placeholder_snapshot(window_id: u32) -> StreamSessionSnapshot {
    StreamSessionSnapshot {
        phase: SessionPhase::Connecting,
        window_id,
        started_at: now_ms(),
        width: 0,
        height: 0,
        fps: 0,
        target_bitrate_bps: 0,
        actual_bitrate_bps: 0,
        dropped_frames: 0,
        slate_remaining_s: None,
        reconnect_attempt: None,
        hardware_encoder: false,
        no_audio_output: false,
    }
}

/// Watches the publisher and the encoder; owns the attribute and the
/// per-second snapshot; ends the session when either side finishes.
#[cfg(any(target_os = "windows", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
async fn monitor(
    app: AppHandle,
    room: Arc<livekit::Room>,
    cancel: CancellationToken,
    mut state_rx: tokio::sync::watch::Receiver<PublisherState>,
    stats: Arc<super::rtmp::PublisherStats>,
    pipeline: Arc<super::pipeline::PipelineStats>,
    queue: Arc<super::queue::MediaQueue>,
    window: Arc<std::sync::atomic::AtomicU32>,
    last_snapshot: Arc<Mutex<Option<StreamSessionSnapshot>>>,
    mut snapshot: StreamSessionSnapshot,
    mut pending_save: Option<PendingSave>,
    live_attribute: LiveAttributeGate,
    announced: ClientLiveAttribute,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut last_bytes = 0u64;
    let mut last_at = Instant::now();
    let mut attribute = announced;
    let mut output_checked = Instant::now();
    let identity = room.local_participant().identity().to_string();
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            changed = state_rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = ticker.tick() => {}
        }
        let publisher_state = state_rx.borrow().clone();
        if publisher_state == PublisherState::Live {
            if let Some(pending) = pending_save.take() {
                save_destination(&app, pending);
            }
        }
        if let PublisherState::Failed(error) = &publisher_state {
            let error = error.clone();
            let app = app.clone();
            tokio::spawn(async move { stop(&app, StopReason::Failed(error)).await });
            return;
        }
        if pipeline_finished() {
            let reason = pipeline_exit_reason();
            let app = app.clone();
            tokio::spawn(async move { stop(&app, reason).await });
            return;
        }
        // One stream per room: if another participant went live first (two
        // clicks that crossed), this one gives way.
        let streams = super::live_state::streams_in_room(&room);
        if super::wire::loses_arbitration(&identity, snapshot.started_at, &streams) {
            let app = app.clone();
            tokio::spawn(async move { stop(&app, StopReason::Failed(LOST_ARBITRATION_ERROR.into())).await });
            return;
        }
        let bytes = stats.bytes_sent.load(Ordering::Relaxed);
        let elapsed = last_at.elapsed().as_secs_f64().max(0.001);
        let actual = ((bytes.saturating_sub(last_bytes)) as f64 * 8.0 / elapsed) as u64;
        last_bytes = bytes;
        last_at = Instant::now();
        let on_slate = pipeline.on_slate.load(Ordering::Relaxed);
        snapshot.phase = match publisher_state {
            PublisherState::Connecting => SessionPhase::Connecting,
            PublisherState::Reconnecting { .. } => SessionPhase::Reconnecting,
            _ if on_slate => SessionPhase::Paused,
            _ => SessionPhase::Live,
        };
        snapshot.reconnect_attempt = match publisher_state {
            PublisherState::Reconnecting { attempt } => Some(attempt),
            _ => None,
        };
        snapshot.window_id = window.load(Ordering::Relaxed);
        snapshot.width = pipeline.width.load(Ordering::Relaxed);
        snapshot.height = pipeline.height.load(Ordering::Relaxed);
        snapshot.fps = pipeline.fps.load(Ordering::Relaxed);
        snapshot.actual_bitrate_bps = actual;
        snapshot.dropped_frames =
            pipeline.frames_dropped.load(Ordering::Relaxed) + queue.dropped_video_frames();
        snapshot.slate_remaining_s = on_slate.then(|| pipeline.slate_remaining_s.load(Ordering::Relaxed));
        snapshot.hardware_encoder = pipeline.hardware.load(Ordering::Relaxed);
        if output_checked.elapsed() >= Duration::from_secs(2) {
            snapshot.no_audio_output = !audio_output_available_async().await;
            output_checked = Instant::now();
        }
        *last_snapshot.lock_unpoisoned() = Some(snapshot.clone());
        let _ = app.emit(STREAM_SESSION_CHANGED_EVENT, &snapshot);

        // `petal.live` (announced before going live): follows the bound
        // share, and is paused on the slate.
        let desired = ClientLiveAttribute {
            mode: StreamMode::Client,
            started_at: snapshot.started_at,
            window_id: snapshot.window_id,
            state: if on_slate { ClientLiveState::Paused } else { ClientLiveState::Live },
        };
        if attribute != desired && live_attribute.write_if_open(|| {
            set_live_attribute(&room, Some(desired));
        }) {
            attribute = desired;
            super::live_state::refresh(&app, &room);
        }
    }
}

/// The server accepted the stream, so the URL and key work: store them.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn save_destination(app: &AppHandle, pending: PendingSave) {
    let PendingSave { name, destination } = pending;
    let result = destinations_store(app).and_then(|store| {
        store
            .save(&name, destination.server_url(), destination.stream_key())
            .map_err(|e| e.to_string())
    });
    match result {
        Ok(saved) => {
            log::info!("streaming: saved destination {} after going live", saved.id);
        }
        Err(error) => {
            let error = destination.redact(&error);
            log::warn!("streaming: couldn't save the destination: {error}");
            let _ = app.emit(STREAM_DESTINATION_SAVE_FAILED_EVENT, error);
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn pipeline_finished() -> bool {
    slot()
        .lock_unpoisoned()
        .as_ref()
        .and_then(|session| session.pipeline.as_ref())
        .is_some_and(|pipeline| pipeline.thread.is_finished())
}

/// Peeks at a finished encoder thread's exit without joining (stop joins).
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn pipeline_exit_reason() -> StopReason {
    let handle = slot().lock_unpoisoned().as_mut().and_then(|session| session.pipeline.take());
    let Some(handle) = handle else { return StopReason::User };
    match handle.thread.join() {
        Ok(super::pipeline::PipelineExit::SlateTimeout) => StopReason::SlateTimeout,
        Ok(super::pipeline::PipelineExit::Failed(error)) => {
            StopReason::Failed(format!("Encoding failed: {error}"))
        }
        Ok(super::pipeline::PipelineExit::Stopped) => StopReason::User,
        Err(_) => StopReason::Failed("The encoder stopped unexpectedly".into()),
    }
}

/// End the stream, whatever the reason. Idempotent.
pub async fn stop(app: &AppHandle, reason: StopReason) {
    let _transition = transition().lock().await;
    let Some(mut session) = slot().lock_unpoisoned().take() else { return };
    session.cancel.cancel();
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if let Some(pipeline) = session.pipeline.take() {
        pipeline.stop.store(true, Ordering::Release);
        let _ = tokio::task::spawn_blocking(move || pipeline.thread.join()).await;
    }
    if let Some(publisher) = session.publisher.take() {
        let _ = tokio::time::timeout(Duration::from_secs(5), publisher).await;
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if let Some(mic) = session.mic.take() {
        release_mic(mic).await;
    }
    super::tap::stop();
    super::streamer_mode::set_live(app, false);
    #[cfg(target_os = "macos")]
    reconcile_share_quality(app, &[session.window_id.load(Ordering::Relaxed)]);
    super::region_lock::bind(app, None);
    session.live_attribute.close();
    if set_live_attribute(&session.room, None).is_some() {
        if let Some(keeper) = super::live_state::keeper_for(&session.room) {
            if !keeper.wait_confirmed(STREAM_LIVE_ATTRIBUTE, "", CLEAR_LIVE_ATTRIBUTE_TIMEOUT).await {
                log::warn!("streaming: {STREAM_LIVE_ATTRIBUTE} not cleared yet; still retrying");
            }
        }
    }
    super::live_state::refresh(app, &session.room);
    let error = match &reason {
        StopReason::Failed(error) => Some(error.clone()),
        _ => None,
    };
    log::info!(
        "streaming: live stream ended ({}){}",
        reason.wire(),
        error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
    );
    let _ = app.emit(STREAM_SESSION_ENDED_EVENT, EndedPayload { reason: reason.wire(), error });
}

/// "Resume live stream here" / "Switch live stream here": rebind the stream
/// to another of the sharer's shares, from the slate or while live.
pub async fn resume_here(app: &AppHandle, window_id: u32) -> Result<(), String> {
    let _transition = transition().lock().await;
    let (_, _, shares) = room_context(app)?;
    if !shares.contains(&window_id) {
        return Err("Share this window first".into());
    }
    let guard = slot().lock_unpoisoned();
    let session = guard.as_ref().ok_or("No live stream to resume")?;
    // Already live here; but a stopped share's window shared again (macOS
    // keeps the id) still needs the resume.
    if session.window_id.load(Ordering::Relaxed) == window_id && !super::tap::live_share_ended() {
        return Ok(());
    }
    if !super::tap::rebind(window_id) {
        return Err("No live stream to resume".into());
    }
    let previous = session.window_id.swap(window_id, Ordering::Relaxed);
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if let Some(pipeline) = session.pipeline.as_ref() {
        let _ = pipeline.commands.send(super::pipeline::PipelineCommand::Rebound);
    }
    super::region_lock::bind(app, Some((window_id, session.canvas)));
    #[cfg(target_os = "macos")]
    reconcile_share_quality(app, &[previous, window_id]);
    log::info!("streaming: moved from window={previous} to window={window_id}");
    Ok(())
}

pub fn current() -> Option<StreamSessionSnapshot> {
    let guard = slot().lock_unpoisoned();
    let session = guard.as_ref()?;
    let snapshot = session.last_snapshot.lock_unpoisoned().clone();
    snapshot.or_else(|| {
        Some(StreamSessionSnapshot {
            started_at: session.started_at,
            target_bitrate_bps: session.target_bitrate_bps,
            ..placeholder_snapshot(session.window_id.load(Ordering::Relaxed))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_gate_never_writes_live_again() {
        let gate = LiveAttributeGate::new();
        let mut writes = 0;
        assert!(gate.write_if_open(|| writes += 1));
        // Stopping closes it; a monitor iteration that resumes afterwards
        // must not put LIVE back.
        gate.clone().close();
        assert!(!gate.write_if_open(|| writes += 1));
        assert_eq!(writes, 1);
    }

    #[test]
    fn preview_reports_the_encode_path_and_whether_it_can_go_live() {
        for (encoder, available, wire) in [
            (ClientEncoder::Gpu, true, "gpu"),
            (ClientEncoder::Cpu, true, "cpu"),
            (ClientEncoder::MissingEncoders, false, "missingEncoders"),
            (ClientEncoder::UnsupportedPlatform, false, "unsupportedPlatform"),
        ] {
            let preview =
                build_preview((2560, 1440), 30, StreamResolution::Auto, StreamOrientation::Auto, None, encoder);
            assert_eq!((preview.plan.width, preview.plan.height), (1920, 1080));
            assert_eq!(preview.client_available, available, "{encoder:?}");
            let json = serde_json::to_value(&preview).unwrap();
            assert_eq!(json["clientEncoder"], wire);
            assert_eq!(json["clientAvailable"], available);
            assert_eq!(json["audioOutput"], true);
        }
        let snapshot = serde_json::to_value(placeholder_snapshot(7)).unwrap();
        assert_eq!(snapshot["noAudioOutput"], false, "the banner's no-audio-output flag is on the wire");
    }

    #[test]
    fn the_cpu_path_previews_auto_within_its_ceiling() {
        use super::super::plan::{STREAM_CPU_AUTO_MAX_FPS, STREAM_CPU_AUTO_MAX_RESOLUTION};
        let cpu = build_preview((1920, 1080), 60, StreamResolution::Auto, StreamOrientation::Auto, None, ClientEncoder::Cpu);
        assert_eq!(cpu.plan.resolution, STREAM_CPU_AUTO_MAX_RESOLUTION);
        assert_eq!(cpu.plan.fps, STREAM_CPU_AUTO_MAX_FPS);
        let gpu = build_preview((1920, 1080), 60, StreamResolution::Auto, StreamOrientation::Auto, None, ClientEncoder::Gpu);
        assert_eq!((gpu.plan.resolution, gpu.plan.fps), (StreamResolution::P1080, 60));
        let explicit = build_preview((1920, 1080), 60, StreamResolution::P1080, StreamOrientation::Auto, None, ClientEncoder::Cpu);
        assert_eq!((explicit.plan.resolution, explicit.plan.fps), (StreamResolution::P1080, 60), "explicit sizes are kept");
    }
}
