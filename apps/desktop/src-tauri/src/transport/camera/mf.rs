#![cfg(target_os = "windows")]

//! Windows Media Foundation camera adapter. Delivers
//! packed NV12 frames to the shared `on_frame` callback; implements the
//! shared [`super::CameraBackend`]. Session orchestration lives in
//! `crate::camera_session`.

use std::panic::AssertUnwindSafe;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{implement, Error as WindowsError, Interface, Ref, HRESULT};
use windows::Win32::Foundation::{HMODULE, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEvent, IMFMediaSource,
    IMFMediaType, IMFSample, IMFSourceReader, IMFSourceReaderCallback, IMFSourceReaderCallback_Impl,
    IMFSourceReaderEx,
    MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
    MFShutdown, MFStartup, MFVideoFormat_NV12,
    MFSTARTUP_FULL,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ASYNC_CALLBACK, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

use crate::sync_ext::MutexExt;
use super::{
    camera_cadence_floor, camera_cadence_qualifies, dedupe_and_sort_modes,
    frame_from_packed_nv12, operation_error, select_camera_format_index, CameraBackend,
    CameraDeviceInfo, CameraError, CameraFormatMetadata, CameraFrame, CameraMode, CameraStatus,
    CameraStatusSource, FrameLayout, PreferredCameraMode,
};

const CALLBACK_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const CANDIDATE_RETRY_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const CAMERA_STARTUP_QUALIFICATION_BUDGET: Duration = Duration::from_secs(3);
/// A cold BRIO reader can take more than 850 ms to deliver its first sample.
/// Readiness gets a larger first-candidate slice; cadence is measured only
/// after the first sample exists, so startup latency is never reported as 0 fps.
const CAMERA_FIRST_FRAME_READINESS_WINDOW: Duration = Duration::from_millis(2500);
const CAMERA_RETRY_READINESS_WINDOW: Duration = Duration::from_secs(1);

impl From<WindowsError> for CameraError {
    fn from(error: WindowsError) -> Self {
        operation_error("Media Foundation camera operation failed", error)
    }
}

struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Result<Self, CameraError> {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        initialized.ok().map_err(|error| {
            operation_error("failed to initialize COM for camera capture", error)
        })?;
        Ok(Self(true))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

struct MediaFoundationRuntime;

impl MediaFoundationRuntime {
    fn start() -> Result<Self, CameraError> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|error| operation_error("failed to start Media Foundation", error))?;
        Ok(Self)
    }
}

impl Drop for MediaFoundationRuntime {
    fn drop(&mut self) {
        if let Err(error) = unsafe { MFShutdown() } {
            log::warn!("camera: Media Foundation shutdown failed: {error}");
        }
    }
}

/// Enumerate real Windows camera devices (the shared `list_devices` provider).
pub(super) fn list_devices() -> Result<Vec<CameraDeviceInfo>, CameraError> {
    let _apartment = ComApartment::enter()?;
    let _runtime = MediaFoundationRuntime::start()?;
    enumerate_cameras()
        .map(|cameras| cameras.into_iter().map(|camera| camera.info).collect())
}

/// Enumerate the concrete (width, height, frame-rate) modes the selected
/// camera actually supports by walking the source reader's native media types
/// with a SYNCHRONOUS reader — types are read, no samples, so the camera light
/// never comes on. Feeds the Settings menus: only presets the camera can
/// actually deliver are enabled (the shared `list_modes` provider).
pub(super) fn list_modes(preferred_device_id: Option<&str>) -> Result<Vec<CameraMode>, CameraError> {
    let _apartment = ComApartment::enter()?;
    let _runtime = MediaFoundationRuntime::start()?;
    let mut cameras = enumerate_cameras()?;
    if cameras.is_empty() {
        return Ok(Vec::new());
    }
    let infos = cameras
        .iter()
        .map(|camera| camera.info.clone())
        .collect::<Vec<_>>();
    let (selected_index, _) = choose_device_index(&infos, preferred_device_id)?;
    let selected = cameras.swap_remove(selected_index);

    let source = unsafe { selected.activation.ActivateObject::<IMFMediaSource>() }
        .map_err(|error| operation_error("failed to open Windows camera", error))?;
    // Owner shuts the source + activation down when this fn returns (the
    // light was never on — no samples were requested).
    let _owner = MediaSourceOwner {
        activation: Some(selected.activation),
        source: Some(source.clone()),
    };
    // No MF_SOURCE_READER_ASYNC_CALLBACK => synchronous reader; we only walk
    // native media types, never ReadSample.
    let attributes = create_attributes(4)?;
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }
        .map_err(|error| operation_error("failed to create camera source reader", error))?;

    let candidates = enumerate_native_candidates(&reader)?;
    let modes = candidates
        .iter()
        .map(|candidate| CameraMode {
            width: candidate.metadata.width,
            height: candidate.metadata.height,
            frame_rate_numerator: candidate.metadata.frame_rate_numerator,
            frame_rate_denominator: candidate.metadata.frame_rate_denominator,
        })
        .collect();
    Ok(dedupe_and_sort_modes(modes))
}

struct EnumeratedCamera {
    info: CameraDeviceInfo,
    activation: IMFActivate,
}

fn create_attributes(capacity: u32) -> Result<IMFAttributes, CameraError> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, capacity) }
        .map_err(|error| operation_error("failed to create Media Foundation attributes", error))?;
    attributes
        .ok_or_else(|| CameraError::Operation("Media Foundation returned no attributes".into()))
}

/// Create a D3D11 device + Media Foundation DXGI device manager so the camera
/// source reader can use hardware MFTs (GPU color conversion, hardware camera
/// drivers that require a D3D11 device — see `MF_SOURCE_READER_D3D_MANAGER`).
///
/// Mirrors the HARDWARE → WARP fallback already used by
/// `windows_screen_capture::create_d3d_device`. Returns `None` (and logs) when
/// no D3D11 device is available at all — the caller then runs the camera on the
/// plain no-manager path exactly as before this change (never breaks camera
/// start). The returned token is the MF reset token paired with the manager.
fn create_d3d_device_manager() -> Option<(IMFDXGIDeviceManager, u32)> {
    let mut device: Option<ID3D11Device> = None;
    let mut used_driver: &str = "none";
    let mut last_error: Option<WindowsError> = None;
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        device = None;
        match unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        } {
            Ok(()) => {
                used_driver = if driver == D3D_DRIVER_TYPE_HARDWARE {
                    "hardware"
                } else {
                    "warp"
                };
                break;
            }
            Err(error) => {
                last_error = Some(error);
                device = None;
            }
        }
    }
    let Some(device) = device else {
        log::warn!(
            "camera: no D3D11 device available (last error: {:?}); camera will run without a hardware DXGI device manager",
            last_error.as_ref().map(ToString::to_string)
        );
        return None;
    };

    let mut reset_token: u32 = 0;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    if let Err(error) = unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager) } {
        log::warn!(
            "camera: MFCreateDXGIDeviceManager failed ({error}); camera will run without a hardware DXGI device manager"
        );
        return None;
    }
    let Some(manager) = manager else {
        log::warn!("camera: MFCreateDXGIDeviceManager returned no manager");
        return None;
    };
    if let Err(error) = unsafe { manager.ResetDevice(&device, reset_token) } {
        log::warn!(
            "camera: DXGI device manager ResetDevice failed ({error}); camera will run without a hardware DXGI device manager"
        );
        return None;
    }
    log::info!("camera: hardware DXGI device manager armed ({used_driver} D3D11 device)");
    Some((manager, reset_token))
}

fn attribute_string(
    attributes: &IMFAttributes,
    key: &windows::core::GUID,
    label: &str,
) -> Result<String, CameraError> {
    let length = unsafe { attributes.GetStringLength(key) }.map_err(|error| {
        operation_error(&format!("failed to read camera {label} length"), error)
    })?;
    let mut value = vec![0u16; length as usize + 1];
    unsafe { attributes.GetString(key, &mut value, None) }
        .map_err(|error| operation_error(&format!("failed to read camera {label}"), error))?;
    String::from_utf16(&value[..length as usize])
        .map_err(|error| operation_error(&format!("camera {label} is not valid UTF-16"), error))
}

fn enumerate_cameras() -> Result<Vec<EnumeratedCamera>, CameraError> {
    let attributes = create_attributes(1)?;
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }
    .map_err(|error| operation_error("failed to configure camera enumeration", error))?;

    let mut raw_activations: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0;
    unsafe { MFEnumDeviceSources(&attributes, &mut raw_activations, &mut count) }
        .map_err(|error| operation_error("failed to enumerate Windows cameras", error))?;

    if count == 0 {
        if !raw_activations.is_null() {
            unsafe { CoTaskMemFree(Some(raw_activations.cast())) };
        }
        return Ok(Vec::new());
    }
    if raw_activations.is_null() {
        return Err(CameraError::Operation(
            "Media Foundation returned a null camera array".into(),
        ));
    }

    let activations = unsafe {
        let slots = std::slice::from_raw_parts_mut(raw_activations, count as usize);
        let values = slots
            .iter_mut()
            .filter_map(Option::take)
            .collect::<Vec<_>>();
        CoTaskMemFree(Some(raw_activations.cast()));
        values
    };

    let mut cameras = Vec::with_capacity(activations.len());
    for activation in activations {
        let name = attribute_string(&activation, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, "name")?;
        let id = attribute_string(
            &activation,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            "symbolic link",
        )?;
        cameras.push(EnumeratedCamera {
            info: CameraDeviceInfo { id, name },
            activation,
        });
    }
    Ok(cameras)
}

fn choose_device_index(
    devices: &[CameraDeviceInfo],
    preferred_device_id: Option<&str>,
) -> Result<(usize, bool), CameraError> {
    if devices.is_empty() {
        return Err(CameraError::NoDevices);
    }
    let Some(preferred) = preferred_device_id.filter(|value| !value.is_empty()) else {
        return Ok((0, false));
    };
    Ok(
        match devices.iter().position(|device| device.id == preferred) {
            Some(index) => (index, false),
            None => (0, true),
        },
    )
}

struct ConfiguredCameraFormat {
    layout: FrameLayout,
    frame_rate_numerator: u32,
    frame_rate_denominator: u32,
    native_index: u32,
    native_subtype: windows::core::GUID,
    candidate_native_indices: Vec<u32>,
    conversion_hardware_transforms: bool,
}

/// A native camera format is deliberately kept separate from the converted
/// NV12 output type. The Source Reader can otherwise silently choose a
/// different sensor subtype (for example MJPG or YUY2) while still reporting
/// the requested NV12 dimensions to the caller.
struct NativeCameraCandidate {
    native_index: u32,
    media_type: IMFMediaType,
    metadata: CameraFormatMetadata,
    subtype: windows::core::GUID,
}

/// Does this media type fit under the product capture ceiling?
fn candidate_within_capture_cap(metadata: &crate::transport::camera::CameraFormatMetadata) -> bool {
    metadata.frame_rate().is_none_or(|(numerator, denominator)| {
        numerator as u64
            <= crate::transport::camera::CAMERA_MAX_CAPTURE_FPS as u64 * denominator.max(1) as u64
    })
}

fn ordered_candidate_indices(
    candidates: &[NativeCameraCandidate],
    width: u32,
    height: u32,
    requested_fps: f64,
    selected_native_index: u32,
) -> Vec<u32> {
    // Retries must respect the product capture ceiling: a diagnostic override
    // or an unusually wide device mode must not reintroduce a >60 fps mode
    // that the ordinary selection path already rejects.
    let mut same_resolution = candidates
        .iter()
        .filter(|candidate| candidate.metadata.width == width && candidate.metadata.height == height)
        .filter(|candidate| candidate_within_capture_cap(&candidate.metadata))
        .collect::<Vec<_>>();
    same_resolution.sort_by(|left, right| {
        let left_fps = left.metadata.frame_rate_numerator as f64
            / left.metadata.frame_rate_denominator.max(1) as f64;
        let right_fps = right.metadata.frame_rate_numerator as f64
            / right.metadata.frame_rate_denominator.max(1) as f64;
        let left_exact = (left_fps.round() - requested_fps).abs() < 0.5;
        let right_exact = (right_fps.round() - requested_fps).abs() < 0.5;
        right_exact
            .cmp(&left_exact)
            .then_with(|| right.metadata.is_nv12.cmp(&left.metadata.is_nv12))
            .then_with(|| {
                (left_fps - requested_fps)
                    .abs()
                    .partial_cmp(&(right_fps - requested_fps).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| right_fps.partial_cmp(&left_fps).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| left.native_index.cmp(&right.native_index))
    });
    if let Some(position) = same_resolution
        .iter()
        .position(|candidate| candidate.native_index == selected_native_index)
    {
        let selected = same_resolution.remove(position);
        same_resolution.insert(0, selected);
    }
    same_resolution
        .into_iter()
        .map(|candidate| candidate.native_index)
        .collect()
}

fn enumerate_native_candidates(
    reader: &IMFSourceReader,
) -> Result<Vec<NativeCameraCandidate>, CameraError> {
    let mut candidates = Vec::new();
    for native_index in 0..512 {
        let media_type = match unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, native_index)
        } {
            Ok(media_type) => media_type,
            Err(error)
                if error.code() == windows::Win32::Media::MediaFoundation::MF_E_NO_MORE_TYPES =>
            {
                break;
            }
            Err(error) => {
                return Err(operation_error(
                    "failed to enumerate camera media types",
                    error,
                ))
            }
        };
        if unsafe { media_type.GetGUID(&MF_MT_MAJOR_TYPE) }.ok() != Some(MFMediaType_Video) {
            continue;
        }
        let Ok(layout) = FrameLayout::from_media_type(&media_type) else {
            continue;
        };
        let packed_frame_rate = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or(0);
        let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
        candidates.push(NativeCameraCandidate {
            native_index,
            media_type,
            metadata: CameraFormatMetadata {
                width: layout.width,
                height: layout.height,
                frame_rate_numerator: (packed_frame_rate >> 32) as u32,
                frame_rate_denominator: packed_frame_rate as u32,
                is_nv12: subtype == MFVideoFormat_NV12,
            },
            subtype,
        });
    }
    Ok(candidates)
}

struct CallbackState {
    active: AtomicBool,
    generation: AtomicU64,
    layout: AtomicU64,
    reader: Mutex<Option<IMFSourceReader>>,
    /// True while exactly one `ReadSample` is outstanding. Media Foundation's
    /// async reader is a self-perpetuating loop: each `OnReadSample` re-arms
    /// the next read. Candidate qualification also arms the first read, so a
    /// post-commit arm must be a no-op when that read is still in flight --
    /// otherwise the same reader has two concurrent reads and returns
    /// duplicate frames.
    read_armed: AtomicBool,
    on_frame: Box<dyn Fn(CameraFrame) + Send + Sync>,
    terminal_error: Mutex<Option<String>>,
    frames_delivered: AtomicU64,
    callbacks_active: Mutex<usize>,
    callbacks_idle: Condvar,
    delivered_at: Mutex<Vec<Instant>>,
}

// SAFETY: Media Foundation source-reader callbacks and teardown are explicitly
// synchronized here. The COM reader is kept behind `reader`, `stop()` clears it
// before flushing, and callbacks drain via `callbacks_active` before owned COM
// interfaces are released. The remaining callback state is atomics, mutexes,
// and the caller-provided `Send + Sync` frame sink.
unsafe impl Send for CallbackState {}
unsafe impl Sync for CallbackState {}

impl CameraStatusSource for CallbackState {
    fn terminal_error(&self) -> Option<String> {
        self.terminal_error.lock_unpoisoned().clone()
    }

    fn frames_delivered(&self) -> u64 {
        self.frames_delivered.load(Ordering::Relaxed)
    }

    fn observed_frame_rate(&self) -> Option<f64> {
        self.observed_frame_rate()
    }
}

impl CallbackState {
    fn new(on_frame: impl Fn(CameraFrame) + Send + Sync + 'static) -> Self {
        Self {
            active: AtomicBool::new(true),
            generation: AtomicU64::new(0),
            layout: AtomicU64::new(0),
            reader: Mutex::new(None),
            read_armed: AtomicBool::new(false),
            on_frame: Box::new(on_frame),
            terminal_error: Mutex::new(None),
            frames_delivered: AtomicU64::new(0),
            callbacks_active: Mutex::new(0),
            callbacks_idle: Condvar::new(),
            delivered_at: Mutex::new(Vec::with_capacity(CADENCE_HISTORY_FRAMES)),
        }
    }

    fn set_layout(&self, layout: FrameLayout) {
        self.layout
            .store(layout.packed_size_attribute(), Ordering::Release);
    }

    fn layout(&self) -> Result<FrameLayout, CameraError> {
        let packed = self.layout.load(Ordering::Acquire);
        FrameLayout::new((packed >> 32) as u32, packed as u32)
    }

    fn fail(&self, message: String) {
        self.active.store(false, Ordering::Release);
        let mut terminal_error = self.terminal_error.lock_unpoisoned();
        if terminal_error.is_none() {
            log::warn!("camera: {message}");
            *terminal_error = Some(message);
        }
    }

    fn rearm(&self) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        // Exactly one read may be outstanding. A candidate that is already
        // streaming re-arms itself from its own callback, so this call has to
        // be idempotent rather than adding a second concurrent read.
        if self.read_armed.swap(true, Ordering::AcqRel) {
            return;
        }
        let error = {
            let reader = self.reader.lock_unpoisoned();
            match reader.as_ref() {
                Some(reader) => unsafe {
                    reader.ReadSample(
                        MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                        0,
                        None,
                        None,
                        None,
                        None,
                    )
                }
                .err(),
                None => {
                    self.read_armed.store(false, Ordering::Release);
                    None
                }
            }
        };
        if let Some(error) = error {
            self.read_armed.store(false, Ordering::Release);
            self.fail(format!("failed to request the next camera frame: {error}"));
        }
    }

    /// Called when `OnReadSample` fires: the read that produced this callback
    /// is no longer outstanding, so the next `rearm()` may ask for another.
    fn note_read_completed(&self) {
        self.read_armed.store(false, Ordering::Release);
    }

    /// Release the read pipeline for a reader that is being closed. Without
    /// this, the flag would stay set for the abandoned candidate and the next
    /// candidate's first arm would be swallowed as a duplicate.
    fn note_reader_abandoned(&self) {
        self.read_armed.store(false, Ordering::Release);
    }

    fn begin_callback(&self) -> CallbackActivity<'_> {
        *self.callbacks_active.lock_unpoisoned() += 1;
        CallbackActivity { state: self }
    }

    fn begin_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn reset_cadence(&self) {
        self.delivered_at.lock_unpoisoned().clear();
        self.terminal_error.lock_unpoisoned().take();
        self.active.store(true, Ordering::Release);
    }

    /// Drop only the recorded frame timestamps, keeping the reader's terminal
    /// state and liveness. Used to start the cadence estimate after the
    /// reader's ramp-up instead of including it.
    fn reset_cadence_window(&self) {
        self.delivered_at.lock_unpoisoned().clear();
    }

    fn generation_is_active(&self, generation: u64) -> bool {
        self.active.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) == generation
    }

    fn first_frame_at(&self) -> Option<Instant> {
        self.delivered_at.lock_unpoisoned().first().copied()
    }

    fn record_delivered_frame(&self) {
        let mut delivered_at = self.delivered_at.lock_unpoisoned();
        delivered_at.push(Instant::now());
        if delivered_at.len() > CADENCE_HISTORY_FRAMES {
            let excess = delivered_at.len() - CADENCE_HISTORY_FRAMES;
            delivered_at.drain(..excess);
        }
    }

    fn observed_frame_rate(&self) -> Option<f64> {
        let delivered_at = self.delivered_at.lock_unpoisoned();
        // Report the CURRENT rate, not the average since the reader opened. A
        // cold USB camera ramps for a second or two, and averaging that ramp
        // in made a 60 fps BRIO read as a 23 fps camera -- which rejected it,
        // opened the device a second time (the camera light blinking), and
        // published at the wrong cadence. A short trailing window forgets the
        // ramp as soon as the reader is genuinely up to speed.
        let window = delivered_at.len().min(CADENCE_WINDOW_FRAMES);
        let recent = &delivered_at[delivered_at.len() - window..];
        let first = recent.first().copied()?;
        let last = recent.last().copied()?;
        let elapsed = last.duration_since(first).as_secs_f64();
        (elapsed > 0.0 && window > 1).then(|| (window - 1) as f64 / elapsed)
    }

    fn wait_for_callbacks(&self, timeout: Duration) {
        let active = self.callbacks_active.lock_unpoisoned();
        let (active, _) = self
            .callbacks_idle
            .wait_timeout_while(active, timeout, |count| *count != 0)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *active != 0 {
            log::warn!(
                "camera: timed out waiting for {} callback(s) to finish",
                *active
            );
        }
    }
}

struct CallbackActivity<'a> {
    state: &'a CallbackState,
}

impl Drop for CallbackActivity<'_> {
    fn drop(&mut self) {
        let mut active = self.state.callbacks_active.lock_unpoisoned();
        *active = active.saturating_sub(1);
        if *active == 0 {
            self.state.callbacks_idle.notify_all();
        }
    }
}

#[implement(IMFSourceReaderCallback)]
struct SourceReaderCallback {
    state: Arc<CallbackState>,
    generation: u64,
}

impl IMFSourceReaderCallback_Impl for SourceReaderCallback_Impl {
    fn OnReadSample(
        &self,
        hrstatus: HRESULT,
        _stream_index: u32,
        stream_flags: u32,
        _timestamp_100ns: i64,
        sample: Ref<'_, IMFSample>,
    ) -> windows::core::Result<()> {
        if !self.state.generation_is_active(self.generation) {
            return Ok(());
        }
        // This generation owns the read that just completed.
        self.state.note_read_completed();
        let _activity = self.state.begin_callback();
        if !self.state.generation_is_active(self.generation) {
            return Ok(());
        }
        if hrstatus.is_err() {
            self.state.fail(format!(
                "Media Foundation camera read failed: {}",
                WindowsError::from(hrstatus)
            ));
            return Ok(());
        }

        let terminal_flags = MF_SOURCE_READERF_ERROR.0 as u32
            | MF_SOURCE_READERF_ENDOFSTREAM.0 as u32
            | MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32
            | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0 as u32;
        if stream_flags & terminal_flags != 0 {
            self.state.fail(format!(
                "Media Foundation camera stream stopped (flags 0x{stream_flags:08x})"
            ));
            return Ok(());
        }

        if let Some(sample) = sample.as_ref() {
            let result = (|| {
                let layout = self.state.layout()?;
                let packed = copy_sample_nv12(sample, layout)?;
                let frame = frame_from_packed_nv12(packed, layout, crate::time_util::now_us())?;
                std::panic::catch_unwind(AssertUnwindSafe(|| (self.state.on_frame)(frame)))
                    .map_err(|_| CameraError::Operation("camera frame callback panicked".into()))?;
                Ok::<(), CameraError>(())
            })();
            if let Err(error) = result {
                self.state.fail(error.to_string());
                return Ok(());
            }
            self.state.frames_delivered.fetch_add(1, Ordering::Relaxed);
            self.state.record_delivered_frame();
        }

        self.state.rearm();
        Ok(())
    }

    fn OnFlush(&self, _stream_index: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnEvent(
        &self,
        _stream_index: u32,
        _event: Ref<'_, IMFMediaEvent>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

fn copy_sample_nv12(sample: &IMFSample, layout: FrameLayout) -> Result<Vec<u8>, CameraError> {
    if unsafe { sample.GetBufferCount() }
        .map_err(|error| operation_error("failed to inspect camera sample buffers", error))?
        == 1
    {
        let buffer = unsafe { sample.GetBufferByIndex(0) }
            .map_err(|error| operation_error("failed to read camera sample buffer", error))?;
        if let Ok(buffer_2d) = buffer.cast::<IMF2DBuffer>() {
            let length = unsafe { buffer_2d.GetContiguousLength() }
                .map_err(|error| operation_error("failed to size camera sample", error))?
                as usize;
            if length != layout.packed_len() {
                return Err(CameraError::Operation(format!(
                    "camera sample is {length} bytes; expected exactly {}",
                    layout.packed_len()
                )));
            }
            let mut packed = vec![0u8; length];
            unsafe { buffer_2d.ContiguousCopyTo(&mut packed) }
                .map_err(|error| operation_error("failed to copy camera sample", error))?;
            return Ok(packed);
        }
    }

    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|error| operation_error("failed to make camera sample contiguous", error))?;
    let length = unsafe { buffer.GetCurrentLength() }
        .map_err(|error| operation_error("failed to size contiguous camera sample", error))?
        as usize;
    if length != layout.packed_len() {
        return Err(CameraError::Operation(format!(
            "contiguous camera sample is {length} bytes; expected exactly {} without row padding",
            layout.packed_len()
        )));
    }

    let mut data = ptr::null_mut();
    let mut current_length = 0;
    unsafe { buffer.Lock(&mut data, None, Some(&mut current_length)) }
        .map_err(|error| operation_error("failed to lock camera sample", error))?;
    let copy_result = if data.is_null() || current_length as usize != length {
        Err(CameraError::Operation(
            "camera sample lock returned an invalid buffer".into(),
        ))
    } else {
        Ok(unsafe { std::slice::from_raw_parts(data, length) }.to_vec())
    };
    let unlock_result = unsafe { buffer.Unlock() }
        .map_err(|error| operation_error("failed to unlock camera sample", error));
    let packed = copy_result?;
    unlock_result?;
    Ok(packed)
}

/// A media source that must be shut down when dropped (turns the camera
/// device off). Does NOT own the activation — the caller keeps that for a
/// possible retry and shuts it down only once the open has fully finished.
struct SourceShutdownGuard(Option<IMFMediaSource>);

impl SourceShutdownGuard {
    fn new(source: IMFMediaSource) -> Self {
        Self(Some(source))
    }

    fn take_source(&mut self) -> IMFMediaSource {
        self.0.take().expect("camera source taken twice")
    }
}

impl Drop for SourceShutdownGuard {
    fn drop(&mut self) {
        if let Some(source) = self.0.take() {
            if let Err(error) = unsafe { source.Shutdown() } {
                // Expected after a failed MFCreateSourceReaderFromMediaSource:
                // MF shuts the source down on reader-creation failure, so the
                // device is already off and this is a no-op error. Debug so the
                // fallback retry is not noisy, yet a genuinely unexpected
                // shutdown failure stays greppable.
                log::debug!("camera: media source shutdown failed: {error}");
            }
        }
    }
}

/// One reader-open attempt for `activation`: activate a fresh media source,
/// build the source-reader attributes (optionally arming the DXGI device
/// manager for hardware MFTs), create the reader, and negotiate the NV12
/// output format. On failure the caller may retry with the same activation —
/// the failed attempt never touches the activation, only the source (which MF
/// shuts down itself when reader creation fails).
fn open_reader_attempt(
    activation: &IMFActivate,
    callback: &IMFSourceReaderCallback,
    arm_d3d_manager: bool,
    preferred: Option<PreferredCameraMode>,
    forced_native_index: Option<u32>,
    enable_hardware_transforms: bool,
) -> Result<(SourceShutdownGuard, IMFSourceReader, ConfiguredCameraFormat), CameraError> {
    let source = unsafe { activation.ActivateObject::<IMFMediaSource>() }
        .map_err(|error| operation_error("failed to open Windows camera", error))?;
    let guard = SourceShutdownGuard::new(source.clone());
    let attributes = create_attributes(4)?;
    if arm_d3d_manager {
        // The source reader AddRefs the manager when the attribute is set, so
        // dropping our handle here is safe. Rejected with E_INVALIDARG by some
        // machines at reader creation — the caller falls back to unarmed.
        if let Some((manager, _reset_token)) = create_d3d_device_manager() {
            if let Err(error) =
                unsafe { attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager) }
            {
                log::warn!(
                    "camera: failed to set MF_SOURCE_READER_D3D_MANAGER ({error}); continuing on the software camera path"
                );
            }
        }
    }
    unsafe {
        attributes.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, callback)?;
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        attributes.SetUINT32(
            &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
            u32::from(enable_hardware_transforms),
        )?;
    }
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }
        .map_err(|error| operation_error("failed to create camera source reader", error))?;
    let configured_format = configure_nv12_reader(
        &reader,
        preferred,
        forced_native_index,
        enable_hardware_transforms,
    )?;
    Ok((guard, reader, configured_format))
}

struct MediaSourceOwner {
    activation: Option<IMFActivate>,
    source: Option<IMFMediaSource>,
}

impl MediaSourceOwner {
    fn take(mut self) -> (IMFActivate, IMFMediaSource) {
        (
            self.activation.take().expect("camera activation owned"),
            self.source.take().expect("camera media source owned"),
        )
    }
}

impl Drop for MediaSourceOwner {
    fn drop(&mut self) {
        if let Some(source) = self.source.take() {
            if let Err(error) = unsafe { source.Shutdown() } {
                // Runs only after a reader was successfully created (startup
                // failure), so the source is live here; an already-shutdown
                // source would be a no-op error, not worth a warn.
                log::debug!("camera: media source shutdown failed: {error}");
            }
        }
        if let Some(activation) = self.activation.take() {
            if let Err(error) = unsafe { activation.ShutdownObject() } {
                log::warn!("camera: activation shutdown failed: {error}");
            }
        }
    }
}

fn configure_nv12_reader(
    reader: &IMFSourceReader,
    preferred: Option<PreferredCameraMode>,
    forced_native_index: Option<u32>,
    conversion_hardware_transforms: bool,
) -> Result<ConfiguredCameraFormat, CameraError> {
    unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;
    }

    let candidates = enumerate_native_candidates(reader)?;
    if std::env::var_os("PETAL_MF_CHARACTERIZE").is_some() {
        for candidate in &candidates {
            log::info!(
                "camera: native candidate index={} subtype={:?} {}x{} @ {}/{} fps{}",
                candidate.native_index,
                candidate.subtype,
                candidate.metadata.width,
                candidate.metadata.height,
                candidate.metadata.frame_rate_numerator,
                candidate.metadata.frame_rate_denominator,
                if candidate.metadata.is_nv12 { " (NV12)" } else { "" },
            );
        }
    }
    let selected_index = forced_native_index
        .and_then(|native_index| {
            candidates
                .iter()
                .position(|candidate| candidate.native_index == native_index)
        })
        .or_else(|| {
            select_camera_format_index(
                candidates.iter().map(|candidate| candidate.metadata),
                preferred,
            )
        })
        .ok_or_else(|| CameraError::Operation("camera exposes no usable video media type".into()))?;
    let selected = &candidates[selected_index];
    // A forced diagnostic index is still a candidate: reject one above the
    // capture ceiling rather than silently publishing more fps than the
    // product supports.
    if !candidate_within_capture_cap(&selected.metadata) {
        return Err(CameraError::Operation(format!(
            "camera native index {} advertises {} fps, above the {} fps capture ceiling",
            selected.native_index,
            selected.metadata.frame_rate_numerator
                / selected.metadata.frame_rate_denominator.max(1),
            crate::transport::camera::CAMERA_MAX_CAPTURE_FPS,
        )));
    }
    let native_format = selected.metadata;
    let requested_fps = preferred
        .map(|mode| mode.frame_rate as f64)
        .unwrap_or(30.0);
    let candidate_native_indices = ordered_candidate_indices(
        &candidates,
        native_format.width,
        native_format.height,
        requested_fps,
        selected.native_index,
    );
    let native_layout = FrameLayout::new(native_format.width, native_format.height)?;
    // Pinning the exact native media type is what makes the advertised
    // rational cadence trustworthy, but `IMFSourceReaderEx` is not guaranteed
    // on every reader/device. When it is missing, fall back to the baseline
    // `SetCurrentMediaType` negotiation below instead of refusing to open the
    // camera: a working camera at a possibly-inexact cadence beats no camera.
    match reader.cast::<IMFSourceReaderEx>() {
        Ok(source_reader_ex) => {
            let native_flags = unsafe {
                source_reader_ex.SetNativeMediaType(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    &selected.media_type,
                )
            }
            .map_err(|error| operation_error("failed to pin camera native media type", error))?;
            log::info!(
                "camera: pinned native media type index={} subtype={:?} {}x{} @ {}/{} fps (flags=0x{native_flags:08x})",
                selected.native_index,
                selected.subtype,
                native_format.width,
                native_format.height,
                native_format.frame_rate_numerator,
                native_format.frame_rate_denominator,
            );
        }
        Err(error) => {
            log::warn!(
                "camera: reader has no IMFSourceReaderEx ({error}); using baseline media-type negotiation, so the pinned native cadence is unavailable for this device"
            );
        }
    }
    let output_type = unsafe { MFCreateMediaType() }
        .map_err(|error| operation_error("failed to create NV12 camera media type", error))?;
    unsafe {
        output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        output_type.SetUINT64(&MF_MT_FRAME_SIZE, native_layout.packed_size_attribute())?;
        if let Some(frame_rate) = native_format.packed_frame_rate() {
            output_type.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
        }
        reader.SetCurrentMediaType(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            None,
            &output_type,
        )?;
    }

    let current =
        unsafe { reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }
            .map_err(|error| {
                operation_error("failed to read negotiated camera media type", error)
            })?;
    let subtype = unsafe { current.GetGUID(&MF_MT_SUBTYPE) }
        .map_err(|error| operation_error("negotiated camera type has no subtype", error))?;
    if subtype != MFVideoFormat_NV12 {
        return Err(CameraError::Operation(
            "Media Foundation did not negotiate NV12 camera output".into(),
        ));
    }
    Ok(ConfiguredCameraFormat {
        layout: FrameLayout::from_media_type(&current)?,
        frame_rate_numerator: native_format.frame_rate_numerator,
        frame_rate_denominator: native_format.frame_rate_denominator,
        native_index: selected.native_index,
        native_subtype: selected.subtype,
        candidate_native_indices,
        conversion_hardware_transforms,
    })
}

pub struct CameraCapture {
    state: Arc<CallbackState>,
    reader: Option<IMFSourceReader>,
    callback: Option<IMFSourceReaderCallback>,
    source: Option<IMFMediaSource>,
    activation: Option<IMFActivate>,
    runtime: Option<MediaFoundationRuntime>,
    dimensions: (u32, u32),
    frame_rate: (u32, u32),
    device_id: String,
    used_default_fallback: bool,
}

// SAFETY: `CameraCapture` owns a Media Foundation source reader configured for
// asynchronous callbacks. Runtime use is synchronized through `CallbackState`;
// teardown enters COM, disables new reads, flushes the reader, waits for active
// callbacks to drain, then releases the owned COM interfaces once.
unsafe impl Send for CameraCapture {}

struct OpenedCameraReader {
    source_guard: SourceShutdownGuard,
    reader: IMFSourceReader,
    callback: IMFSourceReaderCallback,
    configured_format: ConfiguredCameraFormat,
    activation: IMFActivate,
}

fn is_hardware_mft_start_failure(error: &CameraError) -> bool {
    matches!(error, CameraError::Operation(message) if message.to_ascii_uppercase().contains("0XC00D3704"))
}

fn open_fresh_camera_reader(
    preferred_device_id: Option<&str>,
    callback: &IMFSourceReaderCallback,
    arm_d3d_manager: bool,
    preferred_mode: Option<PreferredCameraMode>,
    forced_native_index: Option<u32>,
    enable_hardware_transforms: bool,
) -> Result<OpenedCameraReader, CameraError> {
    let mut cameras = enumerate_cameras()?;
    let infos = cameras
        .iter()
        .map(|camera| camera.info.clone())
        .collect::<Vec<_>>();
    let (selected_index, _) = choose_device_index(&infos, preferred_device_id)?;
    let selected = cameras.swap_remove(selected_index);
    let activation = selected.activation;
    match open_reader_attempt(
        &activation,
        callback,
        arm_d3d_manager,
        preferred_mode,
        forced_native_index,
        enable_hardware_transforms,
    ) {
        // Counted on SUCCESS: this is what actually starts the camera
        // streaming, so it is what the user sees as one activation of the
        // camera light. A hardware-conversion attempt that falls back to the
        // software reader is one activation, not two.
        Ok((source_guard, reader, configured_format)) => {
            NATIVE_CANDIDATE_ACTIVATIONS.fetch_add(1, Ordering::Relaxed);
            Ok(OpenedCameraReader {
                source_guard,
                reader,
                callback: callback.clone(),
                configured_format,
                activation,
            })
        }
        Err(error) => {
            if let Err(shutdown_error) = unsafe { activation.ShutdownObject() } {
                log::debug!("camera: failed activation shutdown failed: {shutdown_error}");
            }
            Err(error)
        }
    }
}

fn open_candidate_with_conversion_retry(
    preferred_device_id: Option<&str>,
    callback: &IMFSourceReaderCallback,
    preferred_mode: Option<PreferredCameraMode>,
    native_index: Option<u32>,
    arm_d3d_manager: bool,
) -> Result<OpenedCameraReader, CameraError> {
    match open_fresh_camera_reader(
        preferred_device_id,
        callback,
        arm_d3d_manager,
        preferred_mode,
        native_index,
        true,
    ) {
        Ok(opened) => Ok(opened),
        Err(error) if is_hardware_mft_start_failure(&error) => {
            log::warn!(
                "camera: hardware capture conversion failed for native candidate {:?}; retrying with hardware transforms disabled",
                native_index
            );
            open_fresh_camera_reader(
                preferred_device_id,
                callback,
                false,
                preferred_mode,
                native_index,
                false,
            )
        }
        Err(error) => Err(error),
    }
}

fn close_camera_reader_attempt(
    state: &CallbackState,
    reader: IMFSourceReader,
    callback: IMFSourceReaderCallback,
    source_guard: SourceShutdownGuard,
    activation: IMFActivate,
) {
    state.active.store(false, Ordering::Release);
    state.note_reader_abandoned();
    state.reader.lock_unpoisoned().take();
    if let Err(error) = unsafe { reader.Flush(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) } {
        log::debug!("camera: candidate reader flush failed: {error}");
    }
    state.wait_for_callbacks(CANDIDATE_RETRY_DRAIN_TIMEOUT);
    drop(reader);
    drop(callback);
    drop(source_guard);
    if let Err(error) = unsafe { activation.ShutdownObject() } {
        log::debug!("camera: candidate activation shutdown failed: {error}");
    }
}

/// How many of the most recent delivered frames the cadence estimate uses.
/// Short enough to report the reader's current rate rather than diluting a
/// settled camera with its own ramp-up; long enough (~0.5s at 60 fps) that two
/// slow frames cannot read as a cadence collapse.
const CADENCE_WINDOW_FRAMES: usize = 32;

/// How long the reader is allowed to produce frames before its cadence is
/// judged. A cold USB camera ramps for a second or more after its first
/// frame; averaging that ramp in is what made a 60 fps BRIO read as a 23 fps
/// camera. Frames delivered during this settle are discarded from the
/// estimate (but still delivered to the caller).
const CAMERA_CADENCE_SETTLE: Duration = Duration::from_millis(600);

/// How many delivered-frame timestamps are retained. Larger than
/// [`CADENCE_WINDOW_FRAMES`] so readiness/latency keep the reader's true first
/// frame while the rate estimate stays recent.
const CADENCE_HISTORY_FRAMES: usize = 64;

struct CadenceQualification {
    advertised_fps: f64,
    observed_fps: Option<f64>,
    effective_frame_rate: (u32, u32),
    qualifies: bool,
    first_frame_latency_ms: Option<u64>,
    terminal_error: Option<String>,
    outcome: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct ViableCandidate {
    native_index: u32,
    advertised_fps: f64,
    effective_frame_rate: (u32, u32),
    conversion_hardware_transforms: bool,
}

type BestObservedCandidate = (u32, f64, (u32, u32), bool);

fn is_async_conversion_start_failure(error: Option<&str>) -> bool {
    error.is_some_and(|message| {
        let message = message.to_ascii_uppercase();
        message.contains("0X80004005") || message.contains("0XC00D3704")
    })
}

/// How many native reader activations this process has opened. A healthy
/// startup opens exactly ONE: the preferred candidate qualifies and stays
/// open. Every extra activation is a retry or a best-measured reopen, and on
/// real hardware that is visible to the user as the camera light blinking
/// once per activation. Counted always (one relaxed increment per open, no
/// I/O) so a field log and the physical regression test read the same number.
static NATIVE_CANDIDATE_ACTIVATIONS: AtomicU64 = AtomicU64::new(0);

/// Native reader activations opened so far in this process.
pub(crate) fn native_candidate_activations() -> u64 {
    NATIVE_CANDIDATE_ACTIVATIONS.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn reset_native_candidate_activations() {
    NATIVE_CANDIDATE_ACTIVATIONS.store(0, Ordering::Relaxed);
}

fn is_frame_producing_candidate(qualification: &CadenceQualification) -> bool {
    qualification.terminal_error.is_none() && qualification.first_frame_latency_ms.is_some()
}

fn update_best_observed_candidate(
    best: &mut Option<BestObservedCandidate>,
    native_index: u32,
    observed_fps: Option<f64>,
    effective_frame_rate: (u32, u32),
    conversion_hardware_transforms: bool,
) {
    let Some(observed_fps) = observed_fps.filter(|fps| fps.is_finite() && *fps > 0.0) else {
        return;
    };
    if best.as_ref().is_none_or(|candidate| observed_fps > candidate.1) {
        *best = Some((
            native_index,
            observed_fps,
            effective_frame_rate,
            conversion_hardware_transforms,
        ));
    }
}

fn qualify_camera_cadence(
    state: &CallbackState,
    configured_format: &ConfiguredCameraFormat,
    preferred_mode: Option<PreferredCameraMode>,
    window: Duration,
) -> CadenceQualification {
    // Readiness and cadence are separate. A cold camera may need most of the
    // first slice before producing sample one; measuring from open time would
    // incorrectly turn startup latency into a dead/low-fps verdict.
    let started_at = Instant::now();
    let deadline = started_at + window;
    let readiness_deadline = (started_at
        + if window >= CAMERA_FIRST_FRAME_READINESS_WINDOW {
            CAMERA_FIRST_FRAME_READINESS_WINDOW
        } else {
            CAMERA_RETRY_READINESS_WINDOW.min(window)
        })
    .min(deadline);
    while Instant::now() < readiness_deadline
        && state.first_frame_at().is_none()
        && state.terminal_error.lock_unpoisoned().is_none()
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    let first_frame_at = state.first_frame_at();
    // Let the reader reach its working rate before judging it, then measure.
    // `window` is the candidate's whole slice, so the settle is skipped when
    // there is not enough budget left to settle AND measure.
    if first_frame_at.is_some() && window > CAMERA_CADENCE_SETTLE {
        std::thread::sleep(CAMERA_CADENCE_SETTLE);
        state.reset_cadence_window();
    }
    // A cold USB camera does not reach its advertised rate on its first frame.
    // Waiting out the whole budget and then averaging that ramp rejected a
    // candidate that was already at full rate by the time the window closed.
    // Poll instead, and stop as soon as the reader's RECENT cadence clears the
    // floor; the deadline still bounds a camera that never gets there.
    let requested_fps = preferred_mode
        .map(|mode| mode.frame_rate as f64)
        .unwrap_or(30.0);
    while first_frame_at.is_some()
        && Instant::now() < deadline
        && state.terminal_error.lock_unpoisoned().is_none()
    {
        if state
            .observed_frame_rate()
            .is_some_and(|fps| camera_cadence_qualifies(requested_fps, fps))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let advertised_fps = configured_format.frame_rate_numerator as f64
        / configured_format.frame_rate_denominator.max(1) as f64;
    let observed_fps = state.observed_frame_rate();
    let terminal_error = state.terminal_error.lock_unpoisoned().clone();
    // Auto targets the product's 30-fps camera contract; a native 60-fps
    // fallback is still healthy for that contract when it sustains >=24 fps.
    let cadence_floor = camera_cadence_floor(requested_fps);
    let qualifies = observed_fps
        .map(|fps| camera_cadence_qualifies(requested_fps, fps))
        .unwrap_or(false);
    if requested_fps >= 24.0 && !qualifies {
        log::warn!(
            "camera: native cadence below requested floor -- advertised={advertised_fps:.2} observed={} requested={requested_fps:.0} floor={cadence_floor:.0}",
            observed_fps
                .map(|fps| format!("{fps:.2}"))
                .unwrap_or_else(|| "unknown".into()),
        );
    }
    let effective_frame_rate = observed_fps
        .filter(|fps| fps.is_finite() && *fps > 0.0)
        .map(|fps| (fps.round().max(1.0) as u32, 1))
        .unwrap_or((
            configured_format.frame_rate_numerator,
            configured_format.frame_rate_denominator,
        ));
    let first_frame_latency_ms = first_frame_at.map(|at| {
        at.saturating_duration_since(started_at)
            .as_millis()
            .min(u64::MAX as u128) as u64
    });
    let outcome = if terminal_error.is_some() {
        "terminal"
    } else if qualifies {
        "qualified"
    } else if first_frame_latency_ms.is_some() {
        "frame-producing-degraded"
    } else {
        "no-frame"
    };
    CadenceQualification {
        advertised_fps,
        observed_fps,
        effective_frame_rate,
        qualifies,
        first_frame_latency_ms,
        terminal_error,
        outcome,
    }
}

impl CameraBackend for CameraCapture {
    fn stop(&mut self) {
        CameraCapture::stop(self);
    }

    fn dimensions(&self) -> (u32, u32) {
        self.dimensions
    }

    fn frame_rate(&self) -> (u32, u32) {
        self.frame_rate
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn used_default_fallback(&self) -> bool {
        self.used_default_fallback
    }

    fn status_handle(&self) -> CameraStatus {
        CameraStatus::new(self.state.clone())
    }
}

impl CameraCapture {
    pub fn start(
        on_frame: impl Fn(CameraFrame) + Send + Sync + 'static,
    ) -> Result<Self, CameraError> {
        Self::start_with_device(None, None, on_frame)
    }

    pub fn start_with_device(
        preferred_device_id: Option<&str>,
        preferred_mode: Option<PreferredCameraMode>,
        on_frame: impl Fn(CameraFrame) + Send + Sync + 'static,
    ) -> Result<Self, CameraError> {
        let _apartment = ComApartment::enter()?;
        let runtime = MediaFoundationRuntime::start()?;
        let mut cameras = enumerate_cameras()?;
        let infos = cameras
            .iter()
            .map(|camera| camera.info.clone())
            .collect::<Vec<_>>();
        let (selected_index, used_default_fallback) =
            choose_device_index(&infos, preferred_device_id)?;
        let device_id = cameras.swap_remove(selected_index).info.id;
        let forced_native_index = std::env::var("PETAL_MF_NATIVE_INDEX")
            .ok()
            .and_then(|value| value.parse::<u32>().ok());
        if let Some(native_index) = forced_native_index {
            log::warn!(
                "camera: PETAL_MF_NATIVE_INDEX={native_index} forces a diagnostic native media type"
            );
        }

        // Candidate qualification must not leak trial frames into self-view or
        // publication. The reader callback still counts and timestamps every
        // decoded sample, but forwards frames only after a candidate commits.
        let expose_frames = Arc::new(AtomicBool::new(false));
        let user_callback = Arc::new(on_frame);
        let callback_expose = expose_frames.clone();
        let state = Arc::new(CallbackState::new(move |frame| {
            if callback_expose.load(Ordering::Acquire) {
                user_callback(frame);
            }
        }));
        let generation = state.begin_generation();
        let callback: IMFSourceReaderCallback = SourceReaderCallback {
            state: state.clone(),
            generation,
        }
        .into();

        // Open the preferred candidate with the DXGI path first. A failed
        // hardware conversion is retried on a fresh activation with hardware
        // transforms disabled; this is especially important for MJPEG camera
        // modes whose decoder MFT advertises support but cannot stream.
        let opened = match open_candidate_with_conversion_retry(
            preferred_device_id,
            &callback,
            preferred_mode,
            forced_native_index,
            true,
        ) {
            Ok(opened) => opened,
            Err(error) => {
                log::warn!(
                    "camera: hardware reader open failed ({error}); retrying on the software camera path"
                );
                open_candidate_with_conversion_retry(
                    preferred_device_id,
                    &callback,
                    preferred_mode,
                    forced_native_index,
                    false,
                )?
            }
        };

        let candidate_indices = opened.configured_format.candidate_native_indices.clone();
        let startup_deadline = Instant::now() + CAMERA_STARTUP_QUALIFICATION_BUDGET;
        let mut best: Option<BestObservedCandidate> = None;
        let mut viable = Vec::<ViableCandidate>::new();
        let mut selected_qualification: Option<CadenceQualification> = None;
        let mut selected_native_index = opened.configured_format.native_index;
        let mut attempts = 1usize;
        let mut next_candidate_position = 1usize;
        let mut current = Some(opened);
        let mut current_software_retry = false;
        let mut committed: Option<OpenedCameraReader> = None;

        // A native candidate attempt is counted once. A compressed candidate
        // may receive one same-index software-conversion retry after an
        // asynchronous startup failure; that retry must not consume a native
        // candidate slot.
        while attempts <= 3 && Instant::now() < startup_deadline {
            if current.is_none() {
                if attempts >= 3 {
                    break;
                }
                let Some(next_native_index) = candidate_indices.get(next_candidate_position).copied()
                else {
                    break;
                };
                next_candidate_position += 1;
                attempts += 1;
                current_software_retry = false;
                let generation = state.begin_generation();
                let callback: IMFSourceReaderCallback = SourceReaderCallback {
                    state: state.clone(),
                    generation,
                }
                .into();
                log::warn!(
                    "camera: retrying native candidate {next_native_index} within bounded startup policy"
                );
                current = match open_candidate_with_conversion_retry(
                    preferred_device_id,
                    &callback,
                    preferred_mode,
                    Some(next_native_index),
                    false,
                ) {
                    Ok(opened) => Some(opened),
                    Err(error) => {
                        log::warn!(
                            "camera: native candidate {next_native_index} failed: {error}"
                        );
                        None
                    }
                };
                continue;
            }

            let opened = current.take().expect("candidate reader must be present");
            let remaining = startup_deadline.saturating_duration_since(Instant::now());
            // The first candidate receives enough time for a cold BRIO start;
            // later candidates get the remaining budget, never a forced
            // pre-first-frame 850 ms slice.
            let readiness_window = if attempts == 1 {
                remaining.min(CAMERA_FIRST_FRAME_READINESS_WINDOW)
            } else {
                remaining.min(CAMERA_RETRY_READINESS_WINDOW)
            };
            state.reset_cadence();
            state.set_layout(opened.configured_format.layout);
            *state.reader.lock_unpoisoned() = Some(opened.reader.clone());
            state.rearm();
            let qualification = qualify_camera_cadence(
                &state,
                &opened.configured_format,
                preferred_mode,
                readiness_window,
            );
            let observed = qualification.observed_fps.unwrap_or(0.0);
            if qualification.terminal_error.is_none() {
                update_best_observed_candidate(
                    &mut best,
                    opened.configured_format.native_index,
                    qualification.observed_fps,
                    qualification.effective_frame_rate,
                    opened.configured_format.conversion_hardware_transforms,
                );
            }
            log::info!(
                "camera: candidate attempt {attempts}/3 native_index={} advertised={:.2} observed={} qualifies={} outcome={} first_frame_latency_ms={} terminal={} conversion_hardware={}",
                opened.configured_format.native_index,
                qualification.advertised_fps,
                qualification
                    .observed_fps
                    .map(|fps| format!("{fps:.2}"))
                    .unwrap_or_else(|| "unknown".into()),
                qualification.qualifies,
                qualification.outcome,
                qualification
                    .first_frame_latency_ms
                    .map(|latency| latency.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                qualification.terminal_error.as_deref().unwrap_or("none"),
                opened.configured_format.conversion_hardware_transforms,
            );

            if qualification.qualifies {
                selected_qualification = Some(qualification);
                selected_native_index = opened.configured_format.native_index;
                committed = Some(opened);
                break;
            }

            let terminal_error = qualification.terminal_error.clone();
            let compressed = opened.configured_format.native_subtype != MFVideoFormat_NV12;
            let should_retry_software = !current_software_retry
                && opened.configured_format.conversion_hardware_transforms
                && compressed
                && qualification.first_frame_latency_ms.is_none()
                && is_async_conversion_start_failure(terminal_error.as_deref());
            if should_retry_software {
                let native_index = opened.configured_format.native_index;
                close_camera_reader_attempt(
                    &state,
                    opened.reader,
                    opened.callback,
                    opened.source_guard,
                    opened.activation,
                );
                let generation = state.begin_generation();
                let callback: IMFSourceReaderCallback = SourceReaderCallback {
                    state: state.clone(),
                    generation,
                }
                .into();
                log::warn!(
                    "camera: asynchronous compressed startup failed for native index={native_index} ({}) ; retrying with hardware transforms disabled",
                    terminal_error.as_deref().unwrap_or("unknown error")
                );
                current = match open_fresh_camera_reader(
                    preferred_device_id,
                    &callback,
                    false,
                    preferred_mode,
                    Some(native_index),
                    false,
                ) {
                    Ok(opened) => Some(opened),
                    Err(error) => {
                        log::warn!(
                            "camera: software conversion retry failed for native index={native_index}: {error}"
                        );
                        None
                    }
                };
                current_software_retry = true;
                continue;
            }

            if is_frame_producing_candidate(&qualification) {
                viable.push(ViableCandidate {
                    native_index: opened.configured_format.native_index,
                    advertised_fps: qualification.advertised_fps,
                    effective_frame_rate: qualification.effective_frame_rate,
                    conversion_hardware_transforms: opened.configured_format.conversion_hardware_transforms,
                });
            }
            let attempted_native_index = opened.configured_format.native_index;
            close_camera_reader_attempt(
                &state,
                opened.reader,
                opened.callback,
                opened.source_guard,
                opened.activation,
            );
            if attempts >= 3 || Instant::now() >= startup_deadline {
                break;
            }
            log::warn!(
                "camera: candidate native index={attempted_native_index} did not meet cadence floor after observed {observed:.2}; outcome={}",
                qualification.outcome,
            );
        }

        // A candidate reader must never still be open when the fallback tries
        // to reopen the device: leaving one live would hold the camera while a
        // second activation is created.
        if let Some(stale) = current.take() {
            close_camera_reader_attempt(
                &state,
                stale.reader,
                stale.callback,
                stale.source_guard,
                stale.activation,
            );
        }

        // If no candidate met its floor, reopen the best measured candidate.
        // If no cadence was measurable, a frame-producing candidate is still a
        // valid availability result; use its pinned native rate provisionally.
        let mut opened = if let Some(opened) = committed {
            opened
        } else if let Some((best_index, best_fps, best_rate, best_hardware)) = best {
            log::warn!(
                "camera: no native candidate met cadence floor; reopening best observed native index={} at {best_fps:.2} fps",
                best_index
            );
            let generation = state.begin_generation();
            let callback: IMFSourceReaderCallback = SourceReaderCallback {
                state: state.clone(),
                generation,
            }
            .into();
            let reopened = if best_hardware {
                open_candidate_with_conversion_retry(
                    preferred_device_id,
                    &callback,
                    preferred_mode,
                    Some(best_index),
                    false,
                )?
            } else {
                open_fresh_camera_reader(
                    preferred_device_id,
                    &callback,
                    false,
                    preferred_mode,
                    Some(best_index),
                    false,
                )?
            };
            selected_native_index = best_index;
            selected_qualification = Some(CadenceQualification {
                advertised_fps: reopened.configured_format.frame_rate_numerator as f64
                    / reopened.configured_format.frame_rate_denominator.max(1) as f64,
                observed_fps: Some(best_fps),
                effective_frame_rate: best_rate,
                qualifies: false,
                first_frame_latency_ms: None,
                terminal_error: None,
                outcome: "best-measured",
            });
            reopened
        } else {
            let mut reopened = None;
            for candidate in viable {
                let generation = state.begin_generation();
                let callback: IMFSourceReaderCallback = SourceReaderCallback {
                    state: state.clone(),
                    generation,
                }
                .into();
                log::warn!(
                    "camera: no stable cadence measured; reopening frame-producing native index={} provisionally",
                    candidate.native_index
                );
                let result = if candidate.conversion_hardware_transforms {
                    open_candidate_with_conversion_retry(
                        preferred_device_id,
                        &callback,
                        preferred_mode,
                        Some(candidate.native_index),
                        false,
                    )
                } else {
                    open_fresh_camera_reader(
                        preferred_device_id,
                        &callback,
                        false,
                        preferred_mode,
                        Some(candidate.native_index),
                        false,
                    )
                };
                match result {
                    Ok(opened) => {
                        selected_native_index = candidate.native_index;
                        selected_qualification = Some(CadenceQualification {
                            advertised_fps: candidate.advertised_fps,
                            observed_fps: None,
                            effective_frame_rate: candidate.effective_frame_rate,
                            qualifies: false,
                            first_frame_latency_ms: None,
                            terminal_error: None,
                            outcome: "provisional-viable",
                        });
                        reopened = Some(opened);
                        break;
                    }
                    Err(error) => log::warn!(
                        "camera: provisional reopen failed for native index={}: {error}",
                        candidate.native_index
                    ),
                }
            }
            reopened.ok_or_else(|| {
                CameraError::Operation(
                    "camera produced no frame-producing candidate during startup qualification".into(),
                )
            })?
        };

        let qualification = selected_qualification.expect("camera candidate must be selected");
        let layout = opened.configured_format.layout;
        // Re-arm the committed reader after a provisional/best-candidate
        // reopen. This also clears a terminal error from an earlier trial and
        // ensures the outer first-frame gate observes the committed reader.
        state.reset_cadence();
        state.set_layout(layout);
        *state.reader.lock_unpoisoned() = Some(opened.reader.clone());
        state.rearm();
        let startup_error = state.terminal_error.lock_unpoisoned().clone();
        if let Some(error) = startup_error {
            close_camera_reader_attempt(
                &state,
                opened.reader,
                opened.callback,
                opened.source_guard,
                opened.activation,
            );
            return Err(CameraError::Operation(error));
        }
        if opened.configured_format.native_index != selected_native_index {
            return Err(CameraError::Operation(
                "camera startup selected an unexpected native candidate".into(),
            ));
        }
        expose_frames.store(true, Ordering::Release);
        let source = opened.source_guard.take_source();
        log::info!(
            "camera: startup commitment reason={} first_frame_latency_ms={} terminal={} ; capturing '{}' at {}x{} @ {}/{} fps (advertised {:.2}, observed {}) from native index={} subtype={:?} conversion_hardware={}{}",
            qualification.outcome,
            qualification
                .first_frame_latency_ms
                .map(|latency| latency.to_string())
                .unwrap_or_else(|| "unknown".into()),
            qualification.terminal_error.as_deref().unwrap_or("none"),
            device_id,
            layout.width,
            layout.height,
            qualification.effective_frame_rate.0,
            qualification.effective_frame_rate.1,
            qualification.advertised_fps,
            qualification
                .observed_fps
                .map(|fps| format!("{fps:.2}"))
                .unwrap_or_else(|| "unknown".into()),
            selected_native_index,
            opened.configured_format.native_subtype,
            opened.configured_format.conversion_hardware_transforms,
            if used_default_fallback {
                " (preferred camera unavailable; using default)"
            } else {
                ""
            }
        );
        Ok(Self {
            state,
            reader: Some(opened.reader),
            callback: Some(opened.callback),
            source: Some(source),
            activation: Some(opened.activation),
            runtime: Some(runtime),
            dimensions: (layout.width, layout.height),
            frame_rate: qualification.effective_frame_rate,
            device_id,
            used_default_fallback,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.dimensions
    }

    /// The negotiated capture frame rate (numerator, denominator).
    pub fn frame_rate(&self) -> (u32, u32) {
        self.frame_rate
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn used_default_fallback(&self) -> bool {
        self.used_default_fallback
    }

    pub fn status_handle(&self) -> CameraStatus {
        CameraStatus::new(self.state.clone())
    }

    pub fn terminal_error(&self) -> Option<String> {
        self.state.terminal_error.lock_unpoisoned().clone()
    }

    pub fn frames_delivered(&self) -> u64 {
        self.state.frames_delivered.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.state.active.store(false, Ordering::Release);
        let _apartment = match ComApartment::enter() {
            Ok(apartment) => Some(apartment),
            Err(error) => {
                log::warn!("camera: COM unavailable during teardown: {error}");
                None
            }
        };
        self.state.reader.lock_unpoisoned().take();
        if let Some(reader) = self.reader.take() {
            if let Err(error) =
                unsafe { reader.Flush(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }
            {
                log::debug!("camera: source reader flush during teardown failed: {error}");
            }
            self.state.wait_for_callbacks(CALLBACK_DRAIN_TIMEOUT);
            drop(reader);
        }
        self.callback.take();
        if let Some(source) = self.source.take() {
            if let Err(error) = unsafe { source.Shutdown() } {
                log::debug!("camera: media source shutdown failed: {error}");
            }
        }
        if let Some(activation) = self.activation.take() {
            if let Err(error) = unsafe { activation.ShutdownObject() } {
                log::debug!("camera: activation shutdown failed: {error}");
            }
        }
        self.runtime.take();
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_with(
        native_index: u32,
        width: u32,
        height: u32,
        fps: u32,
    ) -> NativeCameraCandidate {
        NativeCameraCandidate {
            native_index,
            subtype: MFVideoFormat_NV12,
            media_type: unsafe { MFCreateMediaType() }.expect("test media type"),
            metadata: crate::transport::camera::CameraFormatMetadata {
                width,
                height,
                frame_rate_numerator: fps,
                frame_rate_denominator: u32::from(fps != 0),
                is_nv12: true,
            },
        }
    }

    #[test]
    fn capture_cap_rejects_forced_and_retry_candidates_above_60fps() {
        let capped = crate::transport::camera::CAMERA_MAX_CAPTURE_FPS;
        assert!(candidate_within_capture_cap(
            &candidate_with(0, 1280, 720, capped).metadata
        ));
        assert!(!candidate_within_capture_cap(
            &candidate_with(1, 1280, 720, capped + 1).metadata
        ));
        // A type with no advertised rate cannot be judged, so it stays eligible.
        let mut unrated = candidate_with(2, 1280, 720, 0).metadata;
        unrated.frame_rate_denominator = 0;
        assert!(candidate_within_capture_cap(&unrated));
    }

    #[test]
    fn retry_order_never_reintroduces_a_mode_above_the_capture_cap() {
        let capped = crate::transport::camera::CAMERA_MAX_CAPTURE_FPS;
        let candidates = vec![
            candidate_with(10, 1280, 720, capped),
            candidate_with(11, 1280, 720, capped * 2),
            candidate_with(12, 1280, 720, 15),
        ];
        let ordered = ordered_candidate_indices(&candidates, 1280, 720, capped as f64, 10);
        assert_eq!(ordered, vec![10, 12], "the over-cap candidate must be skipped");
    }

    /// The cadence estimate must report the reader's CURRENT rate, not the
    /// average since it opened. A cold camera ramps, and averaging that ramp
    /// in made a 60 fps device look like a 23 fps one -- which rejected the
    /// requested mode and opened the camera a second time.
    #[test]
    fn cadence_estimate_forgets_the_ramp_once_the_reader_is_up_to_speed() {
        let state = CallbackState::new(|_| {});
        let start = Instant::now();
        // A ramp: the first 40 frames arrive at ~20 fps ...
        for index in 0..40 {
            state
                .delivered_at
                .lock_unpoisoned()
                .push(start + Duration::from_millis(index * 50));
        }
        let ramping = state.observed_frame_rate().expect("a rate while ramping");
        assert!(
            ramping < 24.0,
            "the ramp itself should read slow, got {ramping:.2}"
        );
        // ... then the reader settles at 60 fps. Only the recent window is
        // used, so the estimate recovers as soon as those frames arrive.
        for index in 0..CADENCE_WINDOW_FRAMES as u64 {
            state
                .delivered_at
                .lock_unpoisoned()
                .push(start + Duration::from_millis(2_000 + index * 16));
        }
        let settled = state
            .observed_frame_rate()
            .expect("a rate once settled");
        assert!(
            settled >= 48.0,
            "a settled 60 fps reader must measure at or above the 48 fps floor, got {settled:.2}"
        );
    }

    /// The retained history must stay bounded, and readiness must keep
    /// referring to the oldest retained frame while the RATE estimate uses only
    /// the most recent window.
    #[test]
    fn cadence_history_is_bounded_and_readiness_keeps_the_first_frame() {
        let state = CallbackState::new(|_| {});
        for _ in 0..(CADENCE_HISTORY_FRAMES * 3) {
            state.record_delivered_frame();
        }
        assert_eq!(
            state.delivered_at.lock_unpoisoned().len(),
            CADENCE_HISTORY_FRAMES,
            "the timestamp history must stay bounded"
        );
        assert!(
            state.first_frame_at().is_some(),
            "readiness must still see a first frame after the history rolls"
        );
        assert!(state.observed_frame_rate().is_some());
    }

    /// Clearing only the cadence window must not disturb a reader's liveness or
    /// its terminal state: the settle step uses this to start measuring after
    /// the ramp without losing an error that arrived during it.
    #[test]
    fn resetting_the_cadence_window_preserves_liveness_and_terminal_state() {
        let state = CallbackState::new(|_| {});
        state.record_delivered_frame();
        assert!(state.first_frame_at().is_some());
        state.reset_cadence_window();
        assert!(
            state.first_frame_at().is_none(),
            "the window is what gets cleared"
        );
        assert!(
            state.generation_is_active(state.generation.load(Ordering::Acquire)),
            "clearing the window must not mark the reader abandoned"
        );
        assert!(state.terminal_error().is_none());
    }

    /// A completed request permits exactly one re-arm, a duplicate re-arm is
    /// suppressed, and abandoning the reader releases the slot for the next
    /// candidate. This is the invariant that keeps at most one read in flight.
    #[test]
    fn rearm_state_permits_one_outstanding_request() {
        let state = CallbackState::new(|_| {});
        // No reader: re-arm cannot succeed and must not leave the guard set.
        state.rearm();
        assert!(!state.read_armed.load(Ordering::Acquire));
        // A completed request allows the next one.
        state.read_armed.store(true, Ordering::Release);
        state.note_read_completed();
        assert!(!state.read_armed.load(Ordering::Acquire));
        // Duplicate re-arm while one is outstanding is suppressed.
        state.read_armed.store(true, Ordering::Release);
        state.rearm();
        assert!(
            state.read_armed.load(Ordering::Acquire),
            "a second read must not be requested while one is outstanding"
        );
        // Abandoning the reader releases the slot for the next candidate.
        state.note_reader_abandoned();
        assert!(!state.read_armed.load(Ordering::Acquire));
    }

    #[test]
    fn slow_first_frame_is_retained_as_a_viable_candidate() {
        let qualification = CadenceQualification {
            advertised_fps: 30.0,
            observed_fps: None,
            effective_frame_rate: (30, 1),
            qualifies: false,
            first_frame_latency_ms: Some(1_850),
            terminal_error: None,
            outcome: "frame-producing-degraded",
        };
        assert!(is_frame_producing_candidate(&qualification));
        assert_eq!(qualification.outcome, "frame-producing-degraded");
    }

    #[test]
    fn terminal_candidate_is_not_retained_even_if_it_delivered_a_sample() {
        let qualification = CadenceQualification {
            advertised_fps: 60.0,
            observed_fps: Some(20.0),
            effective_frame_rate: (20, 1),
            qualifies: false,
            first_frame_latency_ms: Some(120),
            terminal_error: Some("Media Foundation camera read failed: 0x80004005".into()),
            outcome: "terminal",
        };
        assert!(!is_frame_producing_candidate(&qualification));
        assert!(is_async_conversion_start_failure(
            qualification.terminal_error.as_deref()
        ));
    }

    #[test]
    fn generic_errors_do_not_trigger_compressed_conversion_retry() {
        assert!(!is_async_conversion_start_failure(Some(
            "camera frame callback panicked"
        )));
        assert!(is_async_conversion_start_failure(Some(
            "Media Foundation camera read failed: 0x80004005"
        )));
        assert!(is_async_conversion_start_failure(Some(
            "Media Foundation camera read failed: 0xc00d3704"
        )));
    }

    #[test]
    fn best_observed_candidate_is_not_the_last_attempted_candidate() {
        let mut best = None;
        update_best_observed_candidate(&mut best, 297, Some(20.62), (21, 1), true);
        update_best_observed_candidate(&mut best, 295, Some(19.57), (20, 1), false);
        update_best_observed_candidate(&mut best, 298, Some(23.91), (24, 1), false);
        assert_eq!(best, Some((298, 23.91, (24, 1), false)));
    }

    #[test]
    fn best_observed_candidate_ignores_failed_or_nonfinite_trials() {
        let mut best = None;
        update_best_observed_candidate(&mut best, 1, None, (30, 1), true);
        update_best_observed_candidate(&mut best, 2, Some(f64::NAN), (30, 1), true);
        update_best_observed_candidate(&mut best, 3, Some(0.0), (30, 1), true);
        assert_eq!(best, None);
    }

    #[test]
    fn exact_camera_preference_wins_and_missing_preference_falls_back() {
        let devices = vec![
            CameraDeviceInfo {
                id: "first".into(),
                name: "First".into(),
            },
            CameraDeviceInfo {
                id: "second".into(),
                name: "Second".into(),
            },
        ];

        assert_eq!(
            choose_device_index(&devices, Some("second")).unwrap(),
            (1, false)
        );
        assert_eq!(
            choose_device_index(&devices, Some("gone")).unwrap(),
            (0, true)
        );
        assert_eq!(choose_device_index(&devices, None).unwrap(), (0, false));
        assert!(choose_device_index(&[], None).is_err());
    }

    #[test]
    #[ignore = "probes MF for AV1 encoder/decoder MFTs on this machine (Windows + MF + COM required)"]
    fn probe_av1_mfts() {
        use windows::Win32::Media::MediaFoundation::{
            MFTEnumEx, MFT_REGISTER_TYPE_INFO, MFT_CATEGORY_VIDEO_DECODER,
            MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFMediaType_Video, MFVideoFormat_AV1,
        };
        use windows::Win32::System::Com::CoTaskMemFree;
        let _apartment = ComApartment::enter().expect("com");
        let _runtime = MediaFoundationRuntime::start().expect("mf");
        let av1_video = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_AV1,
        };
        let any_video = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: windows::core::GUID::zeroed(),
        };
        let flags = MFT_ENUM_FLAG(
            windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG_SYNCMFT.0
                | windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG_ASYNCMFT.0
                | windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG_HARDWARE.0
                | windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG_LOCALMFT.0,
        );

        // Encoders that can OUTPUT AV1.
        let mut handles: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        let hr = unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&any_video as *const MFT_REGISTER_TYPE_INFO),
                Some(&av1_video as *const MFT_REGISTER_TYPE_INFO),
                &mut handles,
                &mut count,
            )
        };
        println!("AV1 encoder MFT count={count} hr={hr:?}");
        if !handles.is_null() {
            unsafe { CoTaskMemFree(Some(handles as _)) };
        }

        // Decoders that can INPUT AV1.
        let mut handles: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        let hr = unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                flags,
                Some(&any_video as *const MFT_REGISTER_TYPE_INFO),
                Some(&av1_video as *const MFT_REGISTER_TYPE_INFO),
                &mut handles,
                &mut count,
            )
        };
        println!("AV1 decoder MFT count={count} hr={hr:?}");
        if !handles.is_null() {
            unsafe { CoTaskMemFree(Some(handles as _)) };
        }
    }

    #[test]
    #[ignore = "requires a real Windows camera (types only; the light stays off)"]
    fn list_camera_modes_on_real_hardware_returns_sane_modes() {
        let modes = list_modes(None).expect("camera mode enumeration should succeed");
        for mode in &modes {
            println!(
                "camera mode: {}x{} @ {}/{} fps",
                mode.width, mode.height, mode.frame_rate_numerator, mode.frame_rate_denominator
            );
        }
        assert!(
            !modes.is_empty(),
            "expected at least one mode on a machine with a camera"
        );
        for mode in &modes {
            assert!(mode.width > 0 && mode.height > 0);
            assert!(mode.frame_rate_numerator > 0 && mode.frame_rate_denominator > 0);
        }
    }

    use livekit::webrtc::video_frame::{NV12Buffer, VideoFrame, VideoRotation};
    use livekit::webrtc::video_source::native::NativeVideoSource;
    use livekit::webrtc::video_source::VideoResolution;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Characterization gate for the reported Windows regression. It is
    /// intentionally ignored in normal CI because it needs a physical camera,
    /// The physical self-view regression: request the exact 60 fps mode the
    /// UI offers, push every captured frame through the production self-view
    /// feed, and pull at display rate. This is the seam that failed when a
    /// 1280x720@60 type delivered ~23 fps and the app reopened the camera
    /// twice (three LED flashes).
    ///
    /// Asserts all three user-visible properties in one run: at least 48
    /// pulled frames per second, no repeated capture timestamp, and exactly
    /// one native activation.
    #[test]
    #[ignore = "requires a real Windows camera; run explicitly on the BRIO machine"]
    fn media_foundation_self_view_delivers_60fps_with_one_activation() {
        let _ = env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Warn)
            .try_init();
        reset_native_candidate_activations();
        let mut capture = CameraCapture::start_with_device(
            None,
            Some(PreferredCameraMode {
                width: 1280,
                height: 720,
                frame_rate: 60,
            }),
            |frame| crate::camera_self_view::feed_frame(&frame),
        )
        .expect("open Media Foundation camera at 1280x720@60");
        let activations = native_candidate_activations();
        // Assertions come after the measurement so a red run reports cadence,
        // duplication and activation count together instead of stopping at the
        // first failed expectation.
        let mode_frame_rate = capture.frame_rate().0;

        // Consume at display rate, the way the webview's rAF pull loop does.
        // The first second is warm-up: publish only starts after the first
        // frame arrives, and the encoder ramp is not what this test measures.
        let started = Instant::now();
        let warmup = Duration::from_millis(1_000);
        let measure = Duration::from_secs(3);
        let mut last_timestamp: Option<u64> = None;
        let mut unique = 0u64;
        let mut repeated = 0u64;
        while Instant::now() < started + warmup {
            if let Some(buffer) = crate::camera_self_view::take_latest() {
                if let Some(header) = crate::camera_self_view::parse_frame_header(&buffer) {
                    last_timestamp = Some(header.capture_wall_time_us);
                }
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        let measure_started = Instant::now();
        while Instant::now() < measure_started + measure {
            if let Some(buffer) = crate::camera_self_view::take_latest() {
                if let Some(header) = crate::camera_self_view::parse_frame_header(&buffer) {
                    if last_timestamp == Some(header.capture_wall_time_us) {
                        repeated = repeated.saturating_add(1);
                    } else {
                        unique = unique.saturating_add(1);
                        last_timestamp = Some(header.capture_wall_time_us);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        let elapsed = measure_started.elapsed().as_secs_f64();
        let pulled_fps = unique as f64 / elapsed;
        let steady_state_callback_fps = capture.status_handle().observed_frame_rate();
        eprintln!(
            "self-view cadence: pulled_unique={unique} repeated={repeated} elapsed={elapsed:.2}s pulled_fps={pulled_fps:.2} activations={activations} selected={}x{}@{}/{} steady_state_callback_fps={}",
            capture.dimensions().0,
            capture.dimensions().1,
            capture.frame_rate().0,
            capture.frame_rate().1,
            steady_state_callback_fps
                .map(|fps| format!("{fps:.2}"))
                .unwrap_or_else(|| "unknown".into()),
        );
        capture.stop();
        assert!(
            mode_frame_rate >= 48,
            "the requested 60 fps mode must be retained, not replaced by a degraded fallback: {mode_frame_rate}"
        );
        assert_eq!(
            repeated, 0,
            "the latest-wins slot must never hand back a frame the consumer already saw"
        );
        assert!(
            pulled_fps >= 48.0,
            "self-view pulled only {pulled_fps:.2} fps from a 60 fps capture"
        );
        assert_eq!(
            activations, 1,
            "a healthy preferred candidate must be opened once; {activations} activations means the camera light blinked that many times"
        );
    }

    /// The physical cadence characterization kept from the original capture
    /// work: it must fail while a camera advertises a healthy mode but
    /// delivers below the floor.
    #[test]
    #[ignore = "requires a real Windows camera; run explicitly on the BRIO machine"]
    fn media_foundation_camera_cadence_characterization() {
        let _ = env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Warn)
            .try_init();
        let (sender, receiver) = mpsc::sync_channel(256);
        let mut capture = CameraCapture::start_with_device(None, None, move |_| {
            let _ = sender.try_send(Instant::now());
        })
        .expect("open Media Foundation camera");
        // `start_with_device` performs bounded candidate qualification before
        // returning. Drain those startup samples so this probe measures the
        // accepted final candidate rather than averaging across a retry.
        while receiver.try_recv().is_ok() {}
        let first = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("receive the first camera callback");
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut timestamps = vec![first];
        while Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(250)) {
                Ok(timestamp) => timestamps.push(timestamp),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let elapsed = timestamps
            .first()
            .zip(timestamps.last())
            .map(|(first, last)| last.duration_since(*first).as_secs_f64())
            .unwrap_or_default();
        let observed_fps = if elapsed > 0.0 {
            (timestamps.len().saturating_sub(1) as f64) / elapsed
        } else {
            0.0
        };
        eprintln!(
            "MF camera characterization: selected={}x{} advertised={}/{} fps callbacks={} observed={observed_fps:.2} fps",
            capture.dimensions().0,
            capture.dimensions().1,
            capture.frame_rate().0,
            capture.frame_rate().1,
            timestamps.len(),
        );
        capture.stop();
        assert!(
            observed_fps >= 24.0,
            "camera advertised a healthy mode but delivered only {observed_fps:.2} fps"
        );
    }

    #[tokio::test]
    #[ignore = "requires an available Windows camera and camera permission"]
    async fn media_foundation_captures_nv12_into_native_source() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let mut capture = CameraCapture::start_with_device(None, None, move |frame| {
            let _ = sender.try_send(frame);
        })
        .expect("open Media Foundation camera");
        let frame = receiver
            .recv_timeout(Duration::from_secs(8))
            .expect("receive an NV12 camera frame");
        for _ in 1..3 {
            let next = receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("receive another NV12 camera frame");
            assert_eq!((next.width, next.height), (frame.width, frame.height));
            assert_eq!(next.y.len(), frame.y.len());
            assert_eq!(next.uv.len(), frame.uv.len());
        }

        let mut buffer =
            NV12Buffer::with_strides(frame.width, frame.height, frame.y_stride, frame.uv_stride);
        let (y, uv) = buffer.data_mut();
        y.copy_from_slice(&frame.y);
        uv.copy_from_slice(&frame.uv);
        let source = NativeVideoSource::new(
            VideoResolution {
                width: frame.width,
                height: frame.height,
            },
            false,
        );
        source.capture_frame(&VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: 0,
            frame_metadata: None,
            buffer: &buffer,
        });

        assert_eq!(capture.dimensions(), (frame.width, frame.height));
        assert!(capture.terminal_error().is_none());
        assert!(capture.frames_delivered() >= 3);
        eprintln!(
            "Media Foundation delivered {} NV12 frames at {}x{} into NativeVideoSource",
            capture.frames_delivered(),
            frame.width,
            frame.height
        );
        let selected_device_id = capture.device_id().to_string();
        capture.stop();
        capture.stop();

        let (reacquired_sender, reacquired_receiver) = mpsc::sync_channel(1);
        let reacquired =
            CameraCapture::start_with_device(Some(&selected_device_id), None, move |frame| {
                let _ = reacquired_sender.try_send(frame);
            })
            .expect("reacquire Media Foundation camera after idempotent stop");
        let reacquired_frame = reacquired_receiver
            .recv_timeout(Duration::from_secs(8))
            .expect("receive a frame after camera reacquisition");
        assert_eq!(
            reacquired.dimensions(),
            (reacquired_frame.width, reacquired_frame.height)
        );
        assert!(!reacquired.used_default_fallback());
        eprintln!("Media Foundation released and reacquired the selected camera");
        drop(reacquired);
    }
}
