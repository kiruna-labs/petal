//! Shared native webcam capture core: the types, the
//! `CameraBackend` trait, the platform-neutral status handle, and the
//! cfg-dispatched providers. The two platform adapters implement the rest:
//! `mf` (Windows Media Foundation) and `avf` (macOS AVFoundation).
//!
//! All session orchestration (first-frame wait, publish pump, loss monitor,
//! self-heal, Tauri commands) lives in `crate::camera_session` — this module
//! only captures frames and describes cameras/modes.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::sync_ext::MutexExt;

pub mod avf; // macOS-only (the file carries its own #![cfg] gate)
pub mod mf; // Windows-only (the file carries its own #![cfg] gate)

// Re-export the concrete capture type so consumers can name the adapter
// when they need its inherent API (tests, the Windows loss monitor).
#[cfg(target_os = "windows")]
pub use mf::CameraCapture;
#[cfg(target_os = "macos")]
pub use avf::CameraCapture;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CameraError {
    #[error("no camera devices are available")]
    NoDevices,
    #[error("no camera device available")]
    NoCamera,
    #[error("camera permission not granted (status: {0})")]
    PermissionDenied(String),
    #[error("camera configuration failed: {0}")]
    Configuration(String),
    #[error("{0}")]
    Operation(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraDeviceInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedCameraDevice {
    pub applied: bool,
    pub in_room: bool,
    pub used_default_fallback: bool,
    pub error: Option<String>,
}

/// A user-chosen camera capture mode (Settings resolution/FPS menus).
/// `frame_rate` is the integer fps preset (15/30/60); matching against the
/// camera's native modes rounds 29.97-style rates to the nearest integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreferredCameraMode {
    pub width: u32,
    pub height: u32,
    pub frame_rate: u32,
}

/// In-memory camera preference mirror. The frontend owns durable persistence;
/// this mirror is seeded at launch and read by the publish/restart paths.
#[derive(Default)]
pub struct CameraDevicePreferences {
    preferred_device_id: Mutex<Option<String>>,
    preferred_mode: Mutex<Option<PreferredCameraMode>>,
}

impl CameraDevicePreferences {
    pub(crate) fn preferred_device(&self) -> Option<String> {
        self.preferred_device_id.lock_unpoisoned().clone()
    }

    pub(crate) fn preferred_mode(&self) -> Option<PreferredCameraMode> {
        self.preferred_mode.lock_unpoisoned().clone()
    }

    pub(crate) fn set_preferred_mode(&self, mode: Option<PreferredCameraMode>) {
        *self.preferred_mode.lock_unpoisoned() = mode;
    }

    pub(crate) fn set_preferred_device(&self, device_id: String) {
        *self.preferred_device_id.lock_unpoisoned() = if device_id.is_empty() {
            None
        } else {
            Some(device_id)
        };
    }
}

/// One copied-out NV12 frame: Y plane + interleaved UV plane, each with its
/// real stride (may exceed `width` on macOS due to row alignment; equals
/// `width` on the Windows Media Foundation path).
#[derive(Debug, Clone)]
pub struct CameraFrame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub y_stride: u32,
    pub uv: Vec<u8>,
    pub uv_stride: u32,
    /// Capture wall-clock time (µs since epoch) — the SPEC.md §7 embedded
    /// measurement timestamp, stamped at copy time.
    pub capture_wall_time_us: u64,
}

/// Adapter-side capture status backing the shared [`CameraStatus`] handle.
/// Implemented by Windows's `CallbackState` and macOS's `DelegateShared`.
pub(crate) trait CameraStatusSource: Send + Sync {
    fn terminal_error(&self) -> Option<String>;
    fn frames_delivered(&self) -> u64;
}

/// Unified status handle for a running capture: terminal error, delivered
/// frame count, and identity (`same_capture` — `Arc::ptr_eq` on the adapter's
/// callback state, so a stale handle can be told apart from a newer capture).
#[derive(Clone)]
pub struct CameraStatus {
    state: Arc<dyn CameraStatusSource>,
}

impl CameraStatus {
    pub(crate) fn new(state: Arc<dyn CameraStatusSource>) -> Self {
        Self { state }
    }

    pub fn terminal_error(&self) -> Option<String> {
        self.state.terminal_error()
    }

    pub fn frames_delivered(&self) -> u64 {
        self.state.frames_delivered()
    }

    pub fn same_capture(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

/// The platform-agnostic capture surface both adapters implement. Frame
/// delivery is callback-driven (`open_camera`'s `on_frame`); the query
/// methods are for the session layer's publish decisions.
pub trait CameraBackend: Send {
    fn stop(&mut self);
    fn dimensions(&self) -> (u32, u32);
    /// The negotiated capture frame rate (numerator, denominator).
    fn frame_rate(&self) -> (u32, u32);
    fn device_id(&self) -> &str;
    fn used_default_fallback(&self) -> bool;
    fn status_handle(&self) -> CameraStatus;
}

// ---------------------------------------------------------------------------
// Synthetic camera source (#815, journey CAM-05 / cockpit scenario CAM-N2W).
//
// The sibling of `transport::audio`'s `PETAL_AUDIO_SYNTH_TONE` hook, and it
// exists for the same reason: "does a web peer actually SEE this Mac's
// camera?" has no automated answer otherwise. An agent machine may have no
// camera at all, and a real camera in a dark room delivers frames that are
// indistinguishable from a broken pipeline at the receiver. Substituting the
// capture INPUT keeps every later stage -- track creation, publish options,
// the frame pump, the encoder, the SFU -- exactly as a user runs it. It
// deliberately does NOT test AVFoundation/Media Foundation capture itself,
// and the scenario that uses it says so.
// ---------------------------------------------------------------------------

/// `PETAL_CAMERA_SYNTH_SOURCE=1` — capture a deterministic NV12 test pattern
/// instead of opening a real camera. Off unless set; never reachable in a
/// normal run.
pub(crate) fn synthetic_camera_capture_enabled() -> bool {
    std::env::var("PETAL_CAMERA_SYNTH_SOURCE").as_deref() == Ok("1")
}

/// `PETAL_CAMERA_SYNTH_FREEZE=1` — hold the synthetic pattern on one frame so
/// a live CAM-N2W run must go red. This is the mutation lever for the web
/// oracle: an assertion nobody has watched fail proves nothing.
///
/// **Only honored together with `PETAL_CAMERA_SYNTH_SOURCE=1`**, for the same
/// safety reason as `audio::publish_unmuted_for_tests`: a lone variable
/// leaking into a dev shell must never be able to alter what a real camera
/// publishes.
pub(crate) fn synthetic_camera_freeze_enabled() -> bool {
    synthetic_camera_capture_enabled()
        && std::env::var("PETAL_CAMERA_SYNTH_FREEZE").as_deref() == Ok("1")
}

pub(crate) const SYNTH_CAMERA_WIDTH: u32 = 640;
pub(crate) const SYNTH_CAMERA_HEIGHT: u32 = 480;
pub(crate) const SYNTH_CAMERA_FPS: u32 = 30;
pub(crate) const SYNTH_CAMERA_DEVICE_ID: &str = "petal-synthetic-camera";

const SYNTH_BACKGROUND_LUMA: u8 = 128;
const SYNTH_BAR_LUMA: u8 = 235;
const SYNTH_BAR_STEP_PX: u64 = 8;

/// One NV12 test frame: a bright bar sweeping across a mid-grey field.
///
/// Both properties are load-bearing for CAM-N2W's web oracle — mid-grey is
/// not black, and the bar moves every frame — so this is deliberately not a
/// flat fill. Deterministic in `frame_index`, which is what lets the freeze
/// lever above remove the motion without changing anything else.
fn synthetic_camera_frame(
    width: u32,
    height: u32,
    frame_index: u64,
    capture_wall_time_us: u64,
) -> CameraFrame {
    let mut y = vec![SYNTH_BACKGROUND_LUMA; (width as usize) * (height as usize)];
    let bar_width = (width / 8).max(1);
    let bar_x = ((frame_index * SYNTH_BAR_STEP_PX) % u64::from(width)) as u32;
    for row in 0..height as usize {
        let row_start = row * width as usize;
        for offset in 0..bar_width {
            let column = ((bar_x + offset) % width) as usize;
            y[row_start + column] = SYNTH_BAR_LUMA;
        }
    }
    CameraFrame {
        width,
        height,
        y,
        y_stride: width,
        // Neutral chroma (128,128) — the luma plane carries the whole signal.
        uv: vec![128u8; (width as usize) * (height as usize) / 2],
        uv_stride: width,
        capture_wall_time_us,
    }
}

fn wall_time_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
struct SyntheticCameraState {
    frames_delivered: std::sync::atomic::AtomicU64,
    terminal_error: Mutex<Option<String>>,
}

impl CameraStatusSource for SyntheticCameraState {
    fn terminal_error(&self) -> Option<String> {
        self.terminal_error.lock_unpoisoned().clone()
    }

    fn frames_delivered(&self) -> u64 {
        self.frames_delivered.load(Ordering::Relaxed)
    }
}

/// A `CameraBackend` that pumps [`synthetic_camera_frame`] at
/// [`SYNTH_CAMERA_FPS`] from its own thread, standing in for the platform
/// adapter at the `open_camera` boundary and nowhere deeper.
struct SyntheticCameraCapture {
    state: Arc<SyntheticCameraState>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl SyntheticCameraCapture {
    fn start(on_frame: impl Fn(CameraFrame) + Send + Sync + 'static) -> Result<Self, CameraError> {
        let state = Arc::new(SyntheticCameraState::default());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let frozen = synthetic_camera_freeze_enabled();
        let pump_state = state.clone();
        let pump_stop = stop.clone();
        let pump = std::thread::Builder::new()
            .name("petal-synthetic-camera".to_string())
            .spawn(move || {
                let interval =
                    std::time::Duration::from_micros(1_000_000 / u64::from(SYNTH_CAMERA_FPS));
                let mut frame_index: u64 = 0;
                while !pump_stop.load(Ordering::Relaxed) {
                    // Frozen still DELIVERS: a stopped pump is a different
                    // failure (no frames at all) from the one the freeze
                    // lever models (frames arriving, picture not changing).
                    let index = if frozen { 0 } else { frame_index };
                    on_frame(synthetic_camera_frame(
                        SYNTH_CAMERA_WIDTH,
                        SYNTH_CAMERA_HEIGHT,
                        index,
                        wall_time_us(),
                    ));
                    pump_state.frames_delivered.fetch_add(1, Ordering::Relaxed);
                    frame_index = frame_index.wrapping_add(1);
                    std::thread::sleep(interval);
                }
            })
            .map_err(|error| {
                CameraError::Operation(format!("synthetic camera pump failed to start: {error}"))
            })?;
        log::warn!(
            "camera: PETAL_CAMERA_SYNTH_SOURCE=1 -- publishing a synthetic \
             {SYNTH_CAMERA_WIDTH}x{SYNTH_CAMERA_HEIGHT}@{SYNTH_CAMERA_FPS} test pattern \
             INSTEAD of camera input (test hook; the rest of the publish path is unchanged; \
             frozen={frozen})"
        );
        Ok(Self {
            state,
            stop,
            pump: Some(pump),
        })
    }
}

impl CameraBackend for SyntheticCameraCapture {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        (SYNTH_CAMERA_WIDTH, SYNTH_CAMERA_HEIGHT)
    }

    fn frame_rate(&self) -> (u32, u32) {
        (SYNTH_CAMERA_FPS, 1)
    }

    fn device_id(&self) -> &str {
        SYNTH_CAMERA_DEVICE_ID
    }

    fn used_default_fallback(&self) -> bool {
        false
    }

    fn status_handle(&self) -> CameraStatus {
        CameraStatus::new(self.state.clone())
    }
}

impl Drop for SyntheticCameraCapture {
    fn drop(&mut self) {
        CameraBackend::stop(self);
    }
}

/// Enumerate the platform's camera devices.
#[cfg(target_os = "windows")]
pub fn list_devices() -> Result<Vec<CameraDeviceInfo>, CameraError> {
    mf::list_devices()
}

#[cfg(target_os = "macos")]
pub fn list_devices() -> Result<Vec<CameraDeviceInfo>, CameraError> {
    avf::list_devices()
}

/// Enumerate the concrete (width, height, frame-rate) modes a camera
/// supports. Windows walks the source reader's native media types with a
/// SYNCHRONOUS reader (types only, no samples — the camera light never comes
/// on); macOS has no mode selection and returns an empty list, which keeps
/// the Settings resolution/FPS menus disabled exactly as today.
#[cfg(target_os = "windows")]
pub fn list_modes(preferred_device_id: Option<&str>) -> Result<Vec<CameraMode>, CameraError> {
    mf::list_modes(preferred_device_id)
}

#[cfg(target_os = "macos")]
pub fn list_modes(_preferred_device_id: Option<&str>) -> Result<Vec<CameraMode>, CameraError> {
    Ok(Vec::new())
}

/// Open the requested camera and start delivering NV12 frames to `on_frame`
/// (called on a capture thread — must be cheap and thread-safe). BLOCKS for
/// up to a few hundred ms (`startRunning`/MF reader setup); call via
/// `spawn_blocking` from async contexts. Falls back to the default device
/// when the preferred id has disappeared.
#[cfg(target_os = "windows")]
pub fn open_camera(
    preferred_device_id: Option<&str>,
    preferred_mode: Option<PreferredCameraMode>,
    on_frame: impl Fn(CameraFrame) + Send + Sync + 'static,
) -> Result<Box<dyn CameraBackend>, CameraError> {
    if synthetic_camera_capture_enabled() {
        return Ok(Box::new(SyntheticCameraCapture::start(on_frame)?));
    }
    Ok(Box::new(mf::CameraCapture::start_with_device(
        preferred_device_id,
        preferred_mode,
        on_frame,
    )?))
}

#[cfg(target_os = "macos")]
pub fn open_camera(
    preferred_device_id: Option<&str>,
    _preferred_mode: Option<PreferredCameraMode>,
    on_frame: impl Fn(CameraFrame) + Send + Sync + 'static,
) -> Result<Box<dyn CameraBackend>, CameraError> {
    if synthetic_camera_capture_enabled() {
        return Ok(Box::new(SyntheticCameraCapture::start(on_frame)?));
    }
    // macOS has no resolution/FPS selection (the Settings UI disables those
    // menus); capture runs at the session preset.
    Ok(Box::new(avf::CameraCapture::start_with_device(
        preferred_device_id,
        on_frame,
    )?))
}

// ---------------------------------------------------------------------------
// Mode/format selection — Windows Media Foundation concepts (macOS has no
// format negotiation; the Settings UI disables the menus there). Kept in the
// shared module so the selection logic is testable on every platform; gated
// to Windows + tests so macOS production builds do not carry dead code.
// ---------------------------------------------------------------------------

/// One distinct (width, height, frame-rate) mode a camera can deliver —
/// surfaced by [`list_modes`] to feed the Settings resolution/FPS menus so
/// unsupported presets can be greyed out accurately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    pub frame_rate_numerator: u32,
    pub frame_rate_denominator: u32,
}

#[cfg(any(target_os = "windows", test))]
const CAMERA_TARGET_WIDTH: u32 = 1280;
#[cfg(any(target_os = "windows", test))]
const CAMERA_TARGET_HEIGHT: u32 = 720;
#[cfg(any(target_os = "windows", test))]
const CAMERA_TARGET_FPS: u32 = 30;
#[cfg(any(target_os = "windows", test))]
const CAMERA_MIN_HEALTHY_FPS: u32 = 24;

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameLayout {
    width: u32,
    height: u32,
    y_len: usize,
    uv_len: usize,
}

#[cfg(any(target_os = "windows", test))]
impl FrameLayout {
    fn new(width: u32, height: u32) -> Result<Self, CameraError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(CameraError::Operation(format!(
                "camera returned invalid NV12 dimensions {width}x{height}"
            )));
        }
        let y_len = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| CameraError::Operation("camera frame size overflow".into()))?;
        let uv_len = y_len
            .checked_div(2)
            .ok_or_else(|| CameraError::Operation("camera frame size overflow".into()))?;
        let packed_len = y_len
            .checked_add(uv_len)
            .ok_or_else(|| CameraError::Operation("camera frame size overflow".into()))?;
        if packed_len > u32::MAX as usize {
            return Err(CameraError::Operation(
                "camera frame exceeds Media Foundation's buffer limit".into(),
            ));
        }
        Ok(Self {
            width,
            height,
            y_len,
            uv_len,
        })
    }

    /// Windows-only (reads MF media-type attributes — the `windows` crate is
    /// not a dependency on macOS, so this cannot live under `test`-cfg code
    /// that macOS test builds compile).
    #[cfg(target_os = "windows")]
    fn from_media_type(media_type: &windows::Win32::Media::MediaFoundation::IMFAttributes) -> Result<Self, CameraError> {
        let packed = unsafe { media_type.GetUINT64(&windows::Win32::Media::MediaFoundation::MF_MT_FRAME_SIZE) }
            .map_err(|error| operation_error("camera media type has no frame size", error))?;
        Self::new((packed >> 32) as u32, packed as u32)
    }

    fn packed_len(self) -> usize {
        self.y_len + self.uv_len
    }

    fn packed_size_attribute(self) -> u64 {
        ((self.width as u64) << 32) | self.height as u64
    }
}

#[cfg(any(target_os = "windows", test))]
fn operation_error(context: &str, error: impl std::fmt::Display) -> CameraError {
    CameraError::Operation(format!("{context}: {error}"))
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CameraFormatMetadata {
    width: u32,
    height: u32,
    frame_rate_numerator: u32,
    frame_rate_denominator: u32,
    is_nv12: bool,
}

#[cfg(any(target_os = "windows", test))]
impl CameraFormatMetadata {
    fn frame_rate(self) -> Option<(u32, u32)> {
        (self.frame_rate_numerator != 0 && self.frame_rate_denominator != 0)
            .then_some((self.frame_rate_numerator, self.frame_rate_denominator))
    }

    fn packed_frame_rate(self) -> Option<u64> {
        self.frame_rate()
            .map(|(numerator, denominator)| ((numerator as u64) << 32) | denominator as u64)
    }

    fn has_healthy_frame_rate(self) -> bool {
        self.frame_rate().is_some_and(|(numerator, denominator)| {
            numerator as u64 >= CAMERA_MIN_HEALTHY_FPS as u64 * denominator as u64
        })
    }

    fn is_usable(self) -> bool {
        FrameLayout::new(self.width, self.height).is_ok()
    }
}

#[cfg(any(target_os = "windows", test))]
fn compare_frame_rates(
    left: CameraFormatMetadata,
    right: CameraFormatMetadata,
) -> std::cmp::Ordering {
    match (left.frame_rate(), right.frame_rate()) {
        (Some((left_numerator, left_denominator)), Some((right_numerator, right_denominator))) => {
            (left_numerator as u128 * right_denominator as u128)
                .cmp(&(right_numerator as u128 * left_denominator as u128))
        }
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

#[cfg(any(target_os = "windows", test))]
fn compare_frame_rate_distance(
    left: CameraFormatMetadata,
    right: CameraFormatMetadata,
) -> std::cmp::Ordering {
    match (left.frame_rate(), right.frame_rate()) {
        (Some((left_numerator, left_denominator)), Some((right_numerator, right_denominator))) => {
            let left_error = (left_numerator as u128)
                .abs_diff(CAMERA_TARGET_FPS as u128 * left_denominator as u128);
            let right_error = (right_numerator as u128)
                .abs_diff(CAMERA_TARGET_FPS as u128 * right_denominator as u128);
            (left_error * right_denominator as u128).cmp(&(right_error * left_denominator as u128))
        }
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

#[cfg(any(target_os = "windows", test))]
fn resolution_tier(format: CameraFormatMetadata) -> u8 {
    if (format.width, format.height) == (CAMERA_TARGET_WIDTH, CAMERA_TARGET_HEIGHT) {
        0
    } else if format.width as u64 * CAMERA_TARGET_HEIGHT as u64
        == format.height as u64 * CAMERA_TARGET_WIDTH as u64
    {
        if format.width >= CAMERA_TARGET_WIDTH && format.height >= CAMERA_TARGET_HEIGHT {
            1
        } else {
            2
        }
    } else {
        3
    }
}

#[cfg(any(target_os = "windows", test))]
fn compare_resolutions(
    left: CameraFormatMetadata,
    right: CameraFormatMetadata,
) -> std::cmp::Ordering {
    let left_tier = resolution_tier(left);
    let right_tier = resolution_tier(right);
    left_tier.cmp(&right_tier).then_with(|| match left_tier {
        0 => std::cmp::Ordering::Equal,
        1 => ((left.width as u64) * left.height as u64)
            .cmp(&((right.width as u64) * right.height as u64)),
        2 => ((right.width as u64) * right.height as u64)
            .cmp(&((left.width as u64) * left.height as u64)),
        _ => {
            let left_error = (left.width as u64 * CAMERA_TARGET_HEIGHT as u64)
                .abs_diff(left.height as u64 * CAMERA_TARGET_WIDTH as u64);
            let right_error = (right.width as u64 * CAMERA_TARGET_HEIGHT as u64)
                .abs_diff(right.height as u64 * CAMERA_TARGET_WIDTH as u64);
            left_error.cmp(&right_error)
        }
    })
}

/// Pick the camera mode to capture at. An explicit user preference wins when
/// the requested resolution exists — the closest fps within it (the Settings
/// UI only enables modes the camera actually delivers, so this is normally an
/// exact hit). Without a preference, or when the requested resolution is
/// absent, the best healthy mode is chosen (unchanged default).
#[cfg(any(target_os = "windows", test))]
fn select_camera_format_index<I>(
    formats: I,
    preferred: Option<PreferredCameraMode>,
) -> Option<usize>
where
    I: IntoIterator<Item = CameraFormatMetadata>,
{
    let formats: Vec<CameraFormatMetadata> = formats.into_iter().collect();

    if let Some(preferred) = preferred {
        let same_resolution: Vec<(usize, CameraFormatMetadata)> = formats
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, format)| {
                format.width == preferred.width && format.height == preferred.height
            })
            .collect();
        if let Some((index, _)) = same_resolution.iter().min_by(|(_, left), (_, right)| {
            let left_distance = format_frame_rate_distance(*left, preferred.frame_rate);
            let right_distance = format_frame_rate_distance(*right, preferred.frame_rate);
            left_distance
                .partial_cmp(&right_distance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.frame_rate_numerator.cmp(&left.frame_rate_numerator))
        }) {
            return Some(*index);
        }
    }

    let has_healthy_format = formats
        .iter()
        .any(|format| format.is_usable() && format.has_healthy_frame_rate());
    formats
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, format)| {
            format.is_usable() && (!has_healthy_format || format.has_healthy_frame_rate())
        })
        .min_by(|(_, left), (_, right)| {
            let cadence = if has_healthy_format {
                std::cmp::Ordering::Equal
            } else {
                compare_frame_rates(*right, *left)
            };
            cadence
                .then_with(|| compare_resolutions(*left, *right))
                .then_with(|| {
                    has_healthy_format
                        .then(|| compare_frame_rate_distance(*left, *right))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| right.is_nv12.cmp(&left.is_nv12))
                .then_with(|| left.width.cmp(&right.width))
                .then_with(|| left.height.cmp(&right.height))
                .then_with(|| left.frame_rate_numerator.cmp(&right.frame_rate_numerator))
                .then_with(|| {
                    left.frame_rate_denominator
                        .cmp(&right.frame_rate_denominator)
                })
        })
        .map(|(index, _)| index)
}

/// Absolute distance between a mode's fps and a target integer fps preset.
#[cfg(any(target_os = "windows", test))]
fn format_frame_rate_distance(format: CameraFormatMetadata, target_fps: u32) -> f64 {
    let fps = format.frame_rate_numerator as f64 / format.frame_rate_denominator.max(1) as f64;
    (fps - target_fps as f64).abs()
}

/// Stable, deduplicated mode list for the UI: the same (w, h, fps) exposed
/// under different subtypes (NV12 vs YUY2, etc.) collapses to one entry,
/// ordered by resolution ascending, then fps descending.
#[cfg(any(target_os = "windows", test))]
fn dedupe_and_sort_modes(mut modes: Vec<CameraMode>) -> Vec<CameraMode> {
    modes.sort_by(|left, right| {
        (left.width * left.height)
            .cmp(&(right.width * right.height))
            .then_with(|| left.height.cmp(&right.height))
            .then_with(|| {
                let left_fps =
                    left.frame_rate_numerator as f64 / left.frame_rate_denominator.max(1) as f64;
                let right_fps =
                    right.frame_rate_numerator as f64 / right.frame_rate_denominator.max(1) as f64;
                right_fps
                    .partial_cmp(&left_fps)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    modes.dedup_by(|left, right| {
        left.width == right.width
            && left.height == right.height
            && left.frame_rate_numerator == right.frame_rate_numerator
            && left.frame_rate_denominator == right.frame_rate_denominator
    });
    modes
}

/// Split a packed NV12 sample (Y plane followed by interleaved UV, no row
/// padding) into the two-plane [`CameraFrame`]. Windows-only in production
/// (macOS copies planes separately in its delegate).
#[cfg(any(target_os = "windows", test))]
fn frame_from_packed_nv12(
    mut packed: Vec<u8>,
    layout: FrameLayout,
    capture_wall_time_us: u64,
) -> Result<CameraFrame, CameraError> {
    if packed.len() != layout.packed_len() {
        return Err(CameraError::Operation(format!(
            "camera NV12 sample is {} bytes; expected exactly {}",
            packed.len(),
            layout.packed_len()
        )));
    }
    let uv = packed.split_off(layout.y_len);
    Ok(CameraFrame {
        width: layout.width,
        height: layout.height,
        y: packed,
        y_stride: layout.width,
        uv,
        uv_stride: layout.width,
        capture_wall_time_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env-var tests mutate process-global state; serialize them so a sibling
    /// test cannot observe a half-set pair (a racing test writing the same
    /// globals can rescue a broken guard and hide the mutation).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn mean_luma(frame: &CameraFrame) -> f64 {
        frame.y.iter().map(|value| f64::from(*value)).sum::<f64>() / frame.y.len() as f64
    }

    /// #815: the two properties CAM-N2W's web oracle keys on. A flat black
    /// pattern would make "the tile is black" unfalsifiable, and a still one
    /// would make "frames are advancing" unfalsifiable — the scenario would
    /// pass for a product that shows nothing.
    #[test]
    fn synthetic_camera_frames_are_bright_and_moving() {
        let first = synthetic_camera_frame(SYNTH_CAMERA_WIDTH, SYNTH_CAMERA_HEIGHT, 0, 1);
        let later = synthetic_camera_frame(SYNTH_CAMERA_WIDTH, SYNTH_CAMERA_HEIGHT, 3, 2);

        assert_eq!(
            first.y.len(),
            (SYNTH_CAMERA_WIDTH * SYNTH_CAMERA_HEIGHT) as usize
        );
        assert_eq!(
            first.uv.len(),
            (SYNTH_CAMERA_WIDTH * SYNTH_CAMERA_HEIGHT / 2) as usize
        );
        assert_eq!(first.y_stride, SYNTH_CAMERA_WIDTH);
        assert_eq!(first.uv_stride, SYNTH_CAMERA_WIDTH);
        assert!(
            mean_luma(&first) > 100.0,
            "the synthetic pattern must be far from black, mean luma was {}",
            mean_luma(&first)
        );
        assert_ne!(
            first.y, later.y,
            "consecutive synthetic frames must differ, or a frozen stream is indistinguishable from a live one"
        );
    }

    /// The mutation counterpart of the test above: with the frame index held
    /// (what `PETAL_CAMERA_SYNTH_FREEZE=1` does to the pump) the signal the
    /// web oracle measures really is gone, so a live CAM-N2W run must go red.
    #[test]
    fn frozen_synthetic_camera_frames_do_not_advance() {
        let held = synthetic_camera_frame(SYNTH_CAMERA_WIDTH, SYNTH_CAMERA_HEIGHT, 0, 1);
        let held_again = synthetic_camera_frame(SYNTH_CAMERA_WIDTH, SYNTH_CAMERA_HEIGHT, 0, 999);

        assert_eq!(held.y, held_again.y);
        assert_eq!(held.uv, held_again.uv);
    }

    /// The freeze lever must be unreachable without the synthetic source. Un-
    /// gated, a leaked `PETAL_CAMERA_SYNTH_FREEZE=1` in a dev shell would be a
    /// variable that alters what a REAL camera publishes; the env pair is the
    /// safety property, so it is pinned here rather than left to a comment.
    #[test]
    fn synthetic_camera_freeze_requires_the_synthetic_source() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let restore = (
            std::env::var("PETAL_CAMERA_SYNTH_SOURCE").ok(),
            std::env::var("PETAL_CAMERA_SYNTH_FREEZE").ok(),
        );

        std::env::set_var("PETAL_CAMERA_SYNTH_FREEZE", "1");
        std::env::remove_var("PETAL_CAMERA_SYNTH_SOURCE");
        assert!(
            !synthetic_camera_freeze_enabled(),
            "the freeze lever must be inert without PETAL_CAMERA_SYNTH_SOURCE=1 -- it must never reach a real camera"
        );

        std::env::set_var("PETAL_CAMERA_SYNTH_SOURCE", "1");
        assert!(
            synthetic_camera_freeze_enabled(),
            "with the synthetic source the lever must work, or CAM-N2W has no way to be proven falsifiable"
        );

        match restore.0 {
            Some(value) => std::env::set_var("PETAL_CAMERA_SYNTH_SOURCE", value),
            None => std::env::remove_var("PETAL_CAMERA_SYNTH_SOURCE"),
        }
        match restore.1 {
            Some(value) => std::env::set_var("PETAL_CAMERA_SYNTH_FREEZE", value),
            None => std::env::remove_var("PETAL_CAMERA_SYNTH_FREEZE"),
        }
    }

    #[test]
    fn camera_modes_dedupe_and_sort_is_stable() {
        let modes = vec![
            CameraMode {
                width: 1280,
                height: 720,
                frame_rate_numerator: 30000,
                frame_rate_denominator: 1001,
            },
            CameraMode {
                width: 1280,
                height: 720,
                frame_rate_numerator: 30,
                frame_rate_denominator: 1,
            },
            // Duplicate: the camera exposes the same mode under two subtypes
            // (e.g. NV12 and YUY2) — must collapse to one entry.
            CameraMode {
                width: 1280,
                height: 720,
                frame_rate_numerator: 30,
                frame_rate_denominator: 1,
            },
            CameraMode {
                width: 1920,
                height: 1080,
                frame_rate_numerator: 30,
                frame_rate_denominator: 1,
            },
            CameraMode {
                width: 640,
                height: 480,
                frame_rate_numerator: 30,
                frame_rate_denominator: 1,
            },
        ];
        let sorted = dedupe_and_sort_modes(modes);
        assert_eq!(sorted.len(), 4);
        assert_eq!(
            sorted[0],
            CameraMode {
                width: 640,
                height: 480,
                frame_rate_numerator: 30,
                frame_rate_denominator: 1
            }
        );
        assert_eq!(
            sorted[3],
            CameraMode {
                width: 1920,
                height: 1080,
                frame_rate_numerator: 30,
                frame_rate_denominator: 1
            }
        );
        // Within 720p: 30 fps sorts before the 29.97 (30000/1001) variant.
        assert_eq!(sorted[1].frame_rate_numerator, 30);
        assert_eq!(sorted[1].frame_rate_denominator, 1);
        assert_eq!(sorted[2].frame_rate_numerator, 30000);
        assert_eq!(sorted[2].frame_rate_denominator, 1001);
    }

    #[test]
    fn camera_contract_serializes_for_shared_settings() {
        let device = CameraDeviceInfo {
            id: "camera-link".into(),
            name: "Front Camera".into(),
        };
        assert_eq!(
            serde_json::to_value(device).unwrap(),
            serde_json::json!({"id": "camera-link", "name": "Front Camera"})
        );

        let applied = AppliedCameraDevice {
            applied: false,
            in_room: true,
            used_default_fallback: true,
            error: Some("camera unavailable".into()),
        };
        assert_eq!(
            serde_json::to_value(applied).unwrap(),
            serde_json::json!({
                "applied": false,
                "inRoom": true,
                "usedDefaultFallback": true,
                "error": "camera unavailable"
            })
        );
    }

    #[test]
    fn frame_layout_rejects_invalid_or_overflowing_nv12_dimensions() {
        assert!(FrameLayout::new(1280, 720).is_ok());
        assert!(FrameLayout::new(0, 720).is_err());
        assert!(FrameLayout::new(641, 480).is_err());
        assert!(FrameLayout::new(640, 481).is_err());
        assert!(FrameLayout::new(65_536, 65_536).is_err());
    }

    fn camera_format(width: u32, height: u32, fps: u32, is_nv12: bool) -> CameraFormatMetadata {
        CameraFormatMetadata {
            width,
            height,
            frame_rate_numerator: fps,
            frame_rate_denominator: u32::from(fps != 0),
            is_nv12,
        }
    }

    fn selected_format(formats: &[CameraFormatMetadata]) -> CameraFormatMetadata {
        formats[select_camera_format_index(formats.iter().copied(), None)
            .expect("select a usable camera format")]
    }

    #[test]
    fn exact_720p30_beats_first_low_camera_mode() {
        let formats = [
            camera_format(320, 240, 15, true),
            camera_format(1280, 720, 30, false),
        ];
        assert_eq!(selected_format(&formats), formats[1]);
    }

    #[test]
    fn healthy_1080p_beats_sub_720p_when_720p_is_absent() {
        let formats = [
            camera_format(640, 360, 30, true),
            camera_format(1920, 1080, 30, false),
        ];
        assert_eq!(selected_format(&formats), formats[1]);
    }

    #[test]
    fn slow_720p_loses_to_healthy_camera_mode() {
        let formats = [
            camera_format(1280, 720, 15, true),
            camera_format(640, 360, 30, false),
        ];
        assert_eq!(selected_format(&formats), formats[1]);
    }

    #[test]
    fn below_healthy_threshold_chooses_highest_cadence_first() {
        let formats = [
            camera_format(1280, 720, 15, true),
            camera_format(640, 360, 23, false),
            camera_format(1920, 1080, 0, true),
        ];
        assert_eq!(selected_format(&formats), formats[1]);
    }

    #[test]
    fn native_nv12_is_only_the_final_camera_format_tiebreaker() {
        let higher_priority_formats = [
            camera_format(1280, 720, 30, false),
            camera_format(1920, 1080, 30, true),
        ];
        assert_eq!(
            selected_format(&higher_priority_formats),
            higher_priority_formats[0]
        );

        let tied_formats = [
            camera_format(1280, 720, 30, false),
            camera_format(1280, 720, 30, true),
        ];
        assert_eq!(selected_format(&tied_formats), tied_formats[1]);
    }

    #[test]
    fn preferred_resolution_picks_closest_fps_within_it() {
        let formats = [
            camera_format(1280, 720, 24, true),
            camera_format(1280, 720, 60, false),
            camera_format(1920, 1080, 30, false),
        ];
        let preferred_60 = PreferredCameraMode {
            width: 1280,
            height: 720,
            frame_rate: 60,
        };
        assert_eq!(
            select_camera_format_index(formats.iter().copied(), Some(preferred_60)).unwrap(),
            1
        );
        // Requesting 30: 24 is closer than 60 → picks the 24 fps mode.
        let preferred_30 = PreferredCameraMode {
            width: 1280,
            height: 720,
            frame_rate: 30,
        };
        assert_eq!(
            select_camera_format_index(formats.iter().copied(), Some(preferred_30)).unwrap(),
            0
        );
    }

    #[test]
    fn preferred_resolution_absent_falls_back_to_best_healthy() {
        let formats = [
            camera_format(1280, 720, 30, false),
            camera_format(640, 480, 30, true),
        ];
        // 4K requested but the camera has no 2160p mode → best healthy (720p).
        let preferred = PreferredCameraMode {
            width: 3840,
            height: 2160,
            frame_rate: 30,
        };
        assert_eq!(
            select_camera_format_index(formats.iter().copied(), Some(preferred)).unwrap(),
            0
        );
    }

    #[test]
    fn preferred_mode_rounds_2997_to_30() {
        // A 29.97-only camera must still satisfy a "30" preset.
        let formats = [CameraFormatMetadata {
            width: 1280,
            height: 720,
            frame_rate_numerator: 30000,
            frame_rate_denominator: 1001,
            is_nv12: true,
        }];
        let preferred = PreferredCameraMode {
            width: 1280,
            height: 720,
            frame_rate: 30,
        };
        assert_eq!(
            select_camera_format_index(formats.iter().copied(), Some(preferred)).unwrap(),
            0
        );
    }

    #[test]
    fn packed_nv12_copy_splits_y_and_uv_without_padding() {
        let layout = FrameLayout::new(4, 2).unwrap();
        let packed: Vec<u8> = (0..layout.packed_len()).map(|value| value as u8).collect();
        let frame = frame_from_packed_nv12(packed.clone(), layout, 42).unwrap();

        assert_eq!(frame.width, 4);
        assert_eq!(frame.height, 2);
        assert_eq!(frame.y_stride, 4);
        assert_eq!(frame.uv_stride, 4);
        assert_eq!(frame.y, &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(frame.uv, &[8, 9, 10, 11]);
        assert_eq!(frame.capture_wall_time_us, 42);
        assert!(frame_from_packed_nv12(packed[..11].to_vec(), layout, 42).is_err());
    }

    #[test]
    fn empty_camera_preference_clears_saved_device() {
        let preferences = CameraDevicePreferences::default();
        preferences.set_preferred_device("camera-link".into());
        assert_eq!(preferences.preferred_device(), Some("camera-link".into()));

        preferences.set_preferred_device(String::new());
        assert_eq!(preferences.preferred_device(), None);
    }
}
