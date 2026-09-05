#![cfg(target_os = "windows")]

//! Windows Media Foundation camera adapter. Delivers
//! packed NV12 frames to the shared `on_frame` callback; implements the
//! shared [`super::CameraBackend`]. Session orchestration lives in
//! `crate::camera_session`.

use std::panic::AssertUnwindSafe;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use windows::core::{implement, Error as WindowsError, Interface, Ref, HRESULT};
use windows::Win32::Foundation::{HMODULE, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEvent, IMFMediaSource,
    IMFSample, IMFSourceReader, IMFSourceReaderCallback, IMFSourceReaderCallback_Impl,
    MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video, MFShutdown,
    MFStartup, MFVideoFormat_NV12, MFSTARTUP_FULL, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
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
    CameraBackend, CameraDeviceInfo, CameraError, CameraFrame, CameraMode, CameraStatus,
    CameraStatusSource, FrameLayout, PreferredCameraMode, dedupe_and_sort_modes,
    frame_from_packed_nv12, operation_error, select_camera_format_index, CameraFormatMetadata,
};

const CALLBACK_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

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

    let mut modes = Vec::new();
    for index in 0..512 {
        let media_type = match unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, index)
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
        modes.push(CameraMode {
            width: layout.width,
            height: layout.height,
            frame_rate_numerator: (packed_frame_rate >> 32) as u32,
            frame_rate_denominator: packed_frame_rate as u32,
        });
    }
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
}

struct CallbackState {
    active: AtomicBool,
    layout: AtomicU64,
    reader: Mutex<Option<IMFSourceReader>>,
    on_frame: Box<dyn Fn(CameraFrame) + Send + Sync>,
    terminal_error: Mutex<Option<String>>,
    frames_delivered: AtomicU64,
    callbacks_active: Mutex<usize>,
    callbacks_idle: Condvar,
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
}

impl CallbackState {
    fn new(on_frame: impl Fn(CameraFrame) + Send + Sync + 'static) -> Self {
        Self {
            active: AtomicBool::new(true),
            layout: AtomicU64::new(0),
            reader: Mutex::new(None),
            on_frame: Box::new(on_frame),
            terminal_error: Mutex::new(None),
            frames_delivered: AtomicU64::new(0),
            callbacks_active: Mutex::new(0),
            callbacks_idle: Condvar::new(),
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
        let error = {
            let reader = self.reader.lock_unpoisoned();
            if !self.active.load(Ordering::Acquire) {
                return;
            }
            reader.as_ref().and_then(|reader| {
                unsafe {
                    reader.ReadSample(
                        MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                        0,
                        None,
                        None,
                        None,
                        None,
                    )
                }
                .err()
            })
        };
        if let Some(error) = error {
            self.fail(format!("failed to request the next camera frame: {error}"));
        }
    }

    fn begin_callback(&self) -> CallbackActivity<'_> {
        *self.callbacks_active.lock_unpoisoned() += 1;
        CallbackActivity { state: self }
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
        let _activity = self.state.begin_callback();
        if !self.state.active.load(Ordering::Acquire) {
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
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    }
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }
        .map_err(|error| operation_error("failed to create camera source reader", error))?;
    let configured_format = configure_nv12_reader(&reader, preferred)?;
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
) -> Result<ConfiguredCameraFormat, CameraError> {
    unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;
    }

    let mut candidates = Vec::new();
    for index in 0..512 {
        let media_type = match unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, index)
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
        let metadata = CameraFormatMetadata {
            width: layout.width,
            height: layout.height,
            frame_rate_numerator: (packed_frame_rate >> 32) as u32,
            frame_rate_denominator: packed_frame_rate as u32,
            is_nv12: unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12),
        };
        candidates.push((media_type, metadata));
    }

    let selected_index = select_camera_format_index(
        candidates.iter().map(|(_, metadata)| *metadata),
        preferred,
    )
    .ok_or_else(|| CameraError::Operation("camera exposes no usable video media type".into()))?;
    let (native_type, native_format) = candidates.swap_remove(selected_index);
    let native_layout = FrameLayout::new(native_format.width, native_format.height)?;
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
    drop(native_type);

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
        let selected = cameras.swap_remove(selected_index);
        let device_id = selected.info.id.clone();
        let activation = selected.activation;

        let state = Arc::new(CallbackState::new(on_frame));
        let callback: IMFSourceReaderCallback = SourceReaderCallback {
            state: state.clone(),
        }
        .into();

        // Hardware camera path: arm the D3D11/DXGI device manager on the FIRST
        // attempt so the source reader can use GPU-accelerated MFTs. Some
        // machines reject the manager attribute outright (measured 2026-08-07:
        // MFCreateSourceReaderFromMediaSource -> E_INVALIDARG 0x80070057 when
        // the manager is armed), so when the armed attempt fails the whole
        // open is retried WITHOUT the manager — the camera always starts, and
        // machines that accept the manager keep hardware MFTs.
        let first_attempt = open_reader_attempt(&activation, &callback, true, preferred_mode);
        let (mut source_guard, reader, configured_format, active_activation) = match first_attempt {
            Ok(parts) => (parts.0, parts.1, parts.2, activation),
            Err(armed_error) => {
                log::warn!(
                    "camera: hardware reader open failed ({armed_error}); retrying on the software camera path"
                );
                // A failed reader open leaves the media source shut down, and a
                // device-source activation is single-shot: ActivateObject on it
                // again returns the SAME dead source (measured 2026-08-07:
                // 0xC00D3E85 at reader creation on the retry). Re-enumerate for
                // a FRESH activation before the unarmed retry.
                let mut fallback_cameras = enumerate_cameras()?;
                let fallback_infos = fallback_cameras
                    .iter()
                    .map(|camera| camera.info.clone())
                    .collect::<Vec<_>>();
                let (fallback_index, _) =
                    choose_device_index(&fallback_infos, preferred_device_id)?;
                let fallback_selected = fallback_cameras.swap_remove(fallback_index);
                // The armed attempt consumed its activation; shut it down for
                // parity with the pre-fallback failure teardown.
                if let Err(error) = unsafe { activation.ShutdownObject() } {
                    log::debug!("camera: armed activation shutdown failed: {error}");
                }
                let retry = open_reader_attempt(&fallback_selected.activation, &callback, false, preferred_mode);
                match retry {
                    Ok(parts) => (parts.0, parts.1, parts.2, fallback_selected.activation),
                    Err(software_error) => {
                        log::warn!("camera: software camera path also failed: {software_error}");
                        if let Err(error) = unsafe { fallback_selected.activation.ShutdownObject() }
                        {
                            log::debug!("camera: fallback activation shutdown failed: {error}");
                        }
                        return Err(software_error);
                    }
                }
            }
        };
        let source = source_guard.take_source();
        let owner = MediaSourceOwner {
            activation: Some(active_activation),
            source: Some(source),
        };
        let layout = configured_format.layout;
        state.set_layout(layout);
        *state.reader.lock_unpoisoned() = Some(reader.clone());
        state.rearm();
        let startup_error = { state.terminal_error.lock_unpoisoned().clone() };
        if let Some(error) = startup_error {
            state.active.store(false, Ordering::Release);
            state.reader.lock_unpoisoned().take();
            if let Err(flush_error) =
                unsafe { reader.Flush(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }
            {
                log::debug!("camera: failed-start reader flush failed: {flush_error}");
            }
            state.wait_for_callbacks(CALLBACK_DRAIN_TIMEOUT);
            return Err(CameraError::Operation(error));
        }

        let (activation, source) = owner.take();
        log::info!(
            "camera: capturing '{}' at {}x{} @ {}/{} fps{}",
            device_id,
            layout.width,
            layout.height,
            configured_format.frame_rate_numerator,
            configured_format.frame_rate_denominator,
            if used_default_fallback {
                " (preferred camera unavailable; using default)"
            } else {
                ""
            }
        );
        Ok(Self {
            state,
            reader: Some(reader),
            callback: Some(callback),
            source: Some(source),
            activation: Some(activation),
            runtime: Some(runtime),
            dimensions: (layout.width, layout.height),
            frame_rate: (configured_format.frame_rate_numerator, configured_format.frame_rate_denominator),
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
    use std::time::Duration;

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
