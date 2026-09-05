#![cfg(target_os = "windows")]
//! Unified Windows.Graphics.Capture (WGC) pipeline for window AND display
//! capture.
//!
//! A token from [`crate::windows_capture_target`] resolves to either kind;
//! item creation is the ONLY split (`CreateForWindow` vs `CreateForMonitor`,
//! both on the same `IGraphicsCaptureItemInterop`). Everything after the
//! `GraphicsCaptureItem` is one shared D3D11 path: free-threaded
//! `Direct3D11CaptureFramePool` (2 buffers) → `CopyResource` into a
//! CPU-readable staging texture → BGRA copy → callback.
//!
//! All WGC/D3D11 state lives on ONE dedicated capture thread per session
//! (COM MTA entered there); the `FrameArrived` handler only bumps a counter
//! and wakes that thread, so every pool/device/context call is serialized.
//! Teardown happens on the same thread (Drop joins it), matching the
//! camera's single-owner discipline.
//!
//! Fail closed: item creation or first-frame errors surface as `Err` /
//! terminal status; there is no desktop-region fallback.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::sync_ext::MutexExt;
use crate::windows_capture_target::{self, WindowsCaptureTarget};
use windows::core::{factory, IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess,
    GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Resource,
    ID3D11Texture2D, D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET, D3D11_BOX, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG, D3D11_MAP_READ,
    D3D11_RESOURCE_MISC_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use crate::windows_capture_target::TargetKind;

/// Bounded setup budget for item/pool/session creation on the capture thread.
const CAPTURE_SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// A consent prompt must never hold the share-control lane forever if Windows
/// cannot complete the WinRT operation (for example, an unavailable shell).
const BORDERLESS_ACCESS_TIMEOUT: Duration = Duration::from_secs(10);
/// Periodic arrival/delivery cadence log while the pump loop is idle.
const CAPTURE_HEALTH_INTERVAL: Duration = Duration::from_secs(5);
/// Latest-wins drain: at most this many frames are processed per wakeup
/// (the last one wins; older pool frames are released).
const DRAIN_FRAME_BUDGET: usize = 8;

/// Which local surface owns the visible sharing indicator for a WGC session.
/// `System` is the fail-safe: Windows draws its own capture border. `Petal`
/// is legal only after borderless access and a visible, local replacement have
/// both been proven by the share-start coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureIndicatorMode {
    System,
    Petal,
}

impl CaptureIndicatorMode {
    fn system_border_required(self) -> bool {
        matches!(self, Self::System)
    }
}

/// Process-lifetime result of the Windows borderless-capture consent request.
/// Every non-Allowed status intentionally collapses to `Denied`: sharing must
/// continue with the WGC indicator rather than failing or creating an
/// unmarked capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BorderlessAccess {
    Allowed,
    Denied,
}

pub(crate) fn borderless_access_from_status(status: AppCapabilityAccessStatus) -> BorderlessAccess {
    if status == AppCapabilityAccessStatus::Allowed {
        BorderlessAccess::Allowed
    } else {
        BorderlessAccess::Denied
    }
}

pub(crate) fn capture_indicator_mode(
    access: BorderlessAccess,
    replacement_ready: bool,
) -> CaptureIndicatorMode {
    if access == BorderlessAccess::Allowed && replacement_ready {
        CaptureIndicatorMode::Petal
    } else {
        CaptureIndicatorMode::System
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureSourceKind {
    Window,
    Display,
    DisplayRegion,
}

/// Source-aware policy gate. A full-display replacement must be excluded from
/// the outgoing pixels; a Petal View selector is itself the replacement, but
/// that selector must also be excluded so its visible frame is not published.
/// Ordinary window overlays are not part of the WGC window item. Their
/// replacement is authoritative only when the native overlay is still owned
/// by its source; an elevated or integrity-unknown source therefore stays on
/// WGC's system indicator even if a passive telepointer is visible.
pub(crate) fn capture_indicator_mode_for_source(
    access: BorderlessAccess,
    source: CaptureSourceKind,
    replacement_ready: bool,
    capture_excluded: bool,
    owner_verified: bool,
) -> CaptureIndicatorMode {
    let replacement_ready = match source {
        CaptureSourceKind::Window => replacement_ready && owner_verified,
        CaptureSourceKind::Display => replacement_ready && capture_excluded,
        CaptureSourceKind::DisplayRegion => replacement_ready && capture_excluded,
    };
    capture_indicator_mode(access, replacement_ready)
}

/// Ask Windows once for permission to suppress its capture border. This is
/// called only for a real share, never for picker thumbnail one-shots, so a
/// thumbnail refresh cannot unexpectedly summon a consent prompt. The result
/// is process-cached and serialized: concurrent Share actions share one
/// request and one bounded log line.
pub(crate) async fn request_borderless_access() -> BorderlessAccess {
    static ACCESS: OnceLock<tokio::sync::Mutex<Option<BorderlessAccess>>> = OnceLock::new();
    let cache = ACCESS.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut guard = cache.lock().await;
    if let Some(access) = *guard {
        return access;
    }

    let access = match GraphicsCaptureAccess::RequestAccessAsync(
        GraphicsCaptureAccessKind::Borderless,
    ) {
        Ok(operation) => match tokio::time::timeout(
            BORDERLESS_ACCESS_TIMEOUT,
            tokio::task::spawn_blocking(move || operation.get()),
        )
        .await
        {
            Ok(Ok(Ok(status))) => {
                let access = borderless_access_from_status(status);
                log::info!(
                    "windows screen capture: borderless capture access status={}",
                    if access == BorderlessAccess::Allowed {
                        "allowed"
                    } else {
                        "denied"
                    }
                );
                access
            }
            Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => {
                log::warn!(
                    "windows screen capture: borderless capture access status=api-failure; using system indicator"
                );
                BorderlessAccess::Denied
            }
        },
        Err(_) => {
            log::warn!(
                "windows screen capture: borderless capture access status=unsupported-or-unavailable; using system indicator"
            );
            BorderlessAccess::Denied
        }
    };
    *guard = Some(access);
    access
}

/// WGC exposes a monitor-sized GraphicsCaptureItem, so region capture keeps
/// that texture GPU-internal and fails closed if the ROI-only path cannot be
/// maintained through validation, resource creation, or device loss.
pub(crate) const REGION_CAPTURE_GPU_ROI_FAILED: &str =
    "Windows display-region capture could not maintain the GPU ROI path";

/// One copied BGRA frame (tightly packed, `bytes_per_row == width * 4`).
pub struct BgraFrame {
    pub bgra: Vec<u8>,
    pub bytes_per_row: usize,
    pub width: u32,
    pub height: u32,
    pub capture_wall_time_us: u64,
    pub region_generation: Option<u64>,
}

#[derive(Clone, Copy)]
struct RegionCaptureSpec {
    monitor: usize,
    roi: crate::region_window::PhysicalRegion,
    output_width: u32,
    output_height: u32,
    offset_x: u32,
    offset_y: u32,
    generation: u64,
}

#[derive(Clone)]
pub struct CaptureStatus {
    state: Arc<CaptureState>,
}

impl CaptureStatus {
    pub fn terminal_error(&self) -> Option<String> {
        self.state.terminal_error.lock_unpoisoned().clone()
    }

    pub fn frames_delivered(&self) -> u64 {
        self.state.frames_delivered.load(Ordering::Relaxed)
    }

    pub(crate) fn same_capture(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

struct CaptureState {
    terminal_error: Mutex<Option<String>>,
    frames_delivered: AtomicU64,
    /// Monotonic arrival counter; the capture thread compares it against its
    /// last-seen value to decide whether a wakeup has real work.
    frames_arrived: AtomicU64,
    /// Current WGC captured content size (the item's size), updated whenever
    /// a copied frame arrives with new dimensions. Ground truth for the
    /// telepointer share frame: WGC captures the client area minus the
    /// invisible DWM resize borders, so `GetClientRect` is NOT the captured
    /// region — the item size is.
    captured_size: Mutex<Option<(u32, u32)>>,
}

/// Cross-thread wakeup/stop signal between the capture thread and Drop.
struct CaptureSignal {
    stopped: AtomicU64,
    /// Set by a live custom-indicator overlay when its replacement can no
    /// longer be trusted. The capture thread consumes this one-shot request.
    system_indicator_requested: AtomicBool,
    /// Once WGC's system border has been restored, later stale overlay events
    /// must not ask the same session to perform another transition.
    system_indicator_restored: AtomicBool,
    arrival: Condvar,
    arrival_mutex: Mutex<()>,
}

impl CaptureSignal {
    fn new() -> Self {
        Self {
            stopped: AtomicU64::new(0),
            system_indicator_requested: AtomicBool::new(false),
            system_indicator_restored: AtomicBool::new(false),
            arrival: Condvar::new(),
            arrival_mutex: Mutex::new(()),
        }
    }

    fn notify_arrival(&self) {
        let _guard = self.arrival_mutex.lock_unpoisoned();
        self.arrival.notify_all();
    }

    fn request_stop(&self) {
        self.stopped.store(1, Ordering::SeqCst);
        self.notify_arrival();
    }

    fn stop_requested(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) != 0
    }

    /// Queue one system-indicator restoration and wake the capture thread.
    /// Returns false when the request was already queued, already completed,
    /// or the capture is stopping.
    fn request_system_indicator(&self) -> bool {
        if self.stop_requested() || self.system_indicator_restored.load(Ordering::Acquire) {
            return false;
        }
        let first = !self.system_indicator_requested.swap(true, Ordering::AcqRel);
        if first {
            self.notify_arrival();
        }
        first
    }

    fn system_indicator_requested(&self) -> bool {
        self.system_indicator_requested.load(Ordering::Acquire)
    }

    fn system_indicator_pending(&self) -> bool {
        self.system_indicator_requested.load(Ordering::Acquire)
            && !self.system_indicator_restored.load(Ordering::Acquire)
    }

    fn mark_system_indicator_restored(&self) {
        self.system_indicator_restored
            .store(true, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeIndicatorFallbackOutcome {
    Restored,
    Terminal,
}

fn runtime_indicator_fallback_outcome(
    indicator_mode: CaptureIndicatorMode,
    border_restore_succeeded: bool,
) -> Option<RuntimeIndicatorFallbackOutcome> {
    if indicator_mode != CaptureIndicatorMode::Petal {
        return None;
    }
    Some(if border_restore_succeeded {
        RuntimeIndicatorFallbackOutcome::Restored
    } else {
        RuntimeIndicatorFallbackOutcome::Terminal
    })
}

/// Keep the safe indicator transition ordered at the capture-thread boundary.
/// The disable closure runs only after WGC confirms its system border is back.
fn restore_system_indicator_before_disable(
    indicator_mode: CaptureIndicatorMode,
    restore_system_indicator: impl FnOnce() -> Result<(), String>,
    disable_petal: impl FnOnce(),
) -> Result<(), String> {
    if indicator_mode != CaptureIndicatorMode::Petal {
        return Ok(());
    }
    restore_system_indicator()?;
    disable_petal();
    Ok(())
}

fn capture_signal_registry() -> &'static Mutex<HashMap<u32, Weak<CaptureSignal>>> {
    static SIGNALS: OnceLock<Mutex<HashMap<u32, Weak<CaptureSignal>>>> = OnceLock::new();
    SIGNALS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_capture_signal(token: u32, signal: &Arc<CaptureSignal>) {
    capture_signal_registry()
        .lock_unpoisoned()
        .insert(token, Arc::downgrade(signal));
}

pub(crate) fn unregister_capture_signal(token: u32) {
    capture_signal_registry().lock_unpoisoned().remove(&token);
}

/// Request a capture-thread-owned transition back to WGC's visible border.
/// The request is intentionally internal: only the native sharer overlay can
/// detect that its replacement became untrustworthy.
pub(crate) fn request_system_indicator_fallback(token: u32) -> bool {
    let signal = {
        let mut signals = capture_signal_registry().lock_unpoisoned();
        let Some(signal) = signals.get(&token).and_then(Weak::upgrade) else {
            signals.remove(&token);
            return false;
        };
        signal
    };
    signal.request_system_indicator()
}

pub struct TargetCaptureSession {
    token: u32,
    thread: Option<JoinHandle<()>>,
    state: Arc<CaptureState>,
    signal: Arc<CaptureSignal>,
}

impl TargetCaptureSession {
    /// Resolve `token`, create the WGC item for its kind, and start a
    /// dedicated capture thread pumping BGRA frames into `on_frame`.
    ///
    /// Returns once item + pool + session are live (the caller still waits
    /// for the FIRST frame through its own channel, mirroring the camera
    /// publish flow). Item creation or first-frame failures surface as
    /// `Err(String)`; nothing falls back to desktop-region capture.
    /// Current WGC captured content size (the item's size, physical px at the
    /// window's DPI), updated whenever a copied frame arrives with new
    /// dimensions. `None` until the first frame is copied. Ground truth for
    /// the telepointer share frame: WGC captures the client area minus the
    /// invisible DWM resize borders, so the item size is what the receiver
    /// actually displays.
    pub(crate) fn captured_size(&self) -> Option<(u32, u32)> {
        *self.state.captured_size.lock_unpoisoned()
    }

    pub fn start(
        token: u32,
        indicator_mode: CaptureIndicatorMode,
        on_frame: impl Fn(BgraFrame) + Send + Sync + 'static,
    ) -> Result<(Self, CaptureStatus), String> {
        let target = resolve_capture_target(token)?;
        let is_region = crate::region_window::resolve(token).is_some();
        let region = region_capture_spec(token, &target, None)?;
        if let Some(error) = region_capture_validation_error(is_region, region.is_some()) {
            return Err(error.to_string());
        }

        let state = Arc::new(CaptureState {
            terminal_error: Mutex::new(None),
            frames_delivered: AtomicU64::new(0),
            frames_arrived: AtomicU64::new(0),
            captured_size: Mutex::new(None),
        });
        let signal = Arc::new(CaptureSignal::new());
        if indicator_mode == CaptureIndicatorMode::System {
            signal.mark_system_indicator_restored();
        }
        let (setup_tx, setup_rx) = std::sync::mpsc::sync_channel(1);
        register_capture_signal(token, &signal);

        let thread_state = state.clone();
        let thread_signal = signal.clone();
        let thread = match std::thread::Builder::new()
            .name(format!("petal-wgc-capture-{token}"))
            .spawn(move || {
                capture_thread_main(
                    token,
                    target,
                    region,
                    indicator_mode,
                    thread_state,
                    thread_signal,
                    on_frame,
                    setup_tx,
                );
            }) {
            Ok(thread) => thread,
            Err(error) => {
                unregister_capture_signal(token);
                return Err(format!("failed to spawn capture thread: {error}"));
            }
        };

        match setup_rx.recv_timeout(CAPTURE_SETUP_TIMEOUT) {
            Ok(Ok(())) => Ok((
                Self {
                    token,
                    thread: Some(thread),
                    state: state.clone(),
                    signal: signal.clone(),
                },
                CaptureStatus { state },
            )),
            Ok(Err(error)) => {
                log::error!("windows screen capture: capture setup failed: {error}");
                signal.request_stop();
                let _ = thread.join();
                unregister_capture_signal(token);
                Err(error)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let terminal = state.terminal_error.lock_unpoisoned().clone();
                log::warn!(
                    "windows screen capture: capture setup timed out after {}s (terminal={:?}); a step between item creation and StartCapture likely hung",
                    CAPTURE_SETUP_TIMEOUT.as_secs(),
                    terminal
                );
                signal.request_stop();
                let _ = thread.join();
                unregister_capture_signal(token);
                Err("capture setup timed out".to_string())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let terminal = state.terminal_error.lock_unpoisoned().clone();
                log::error!(
                    "windows screen capture: capture thread exited during setup (terminal={terminal:?})"
                );
                signal.request_stop();
                let _ = thread.join();
                unregister_capture_signal(token);
                Err("capture thread exited during setup".to_string())
            }
        }
    }

    pub fn status_handle(&self) -> CaptureStatus {
        CaptureStatus {
            state: self.state.clone(),
        }
    }
}

impl Drop for TargetCaptureSession {
    fn drop(&mut self) {
        self.signal.request_stop();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                log::warn!("windows screen capture: capture thread panicked during teardown");
            }
        }
        // Keep the registry free of a stale token even if the capture thread
        // exited abnormally before its own final cleanup.
        unregister_capture_signal(self.token);
    }
}

/// One-shot capture of the latest frame for a token (picker thumbnails).
/// Works for window AND display tokens (the registry kind dispatches item
/// creation). Runs its own setup/teardown; callers must run it off the main
/// thread.
pub fn capture_one_shot(token: u32, timeout: Duration) -> Result<BgraFrame, String> {
    let target = resolve_capture_target(token)?;
    let is_region = crate::region_window::resolve(token).is_some();
    let region = region_capture_spec(token, &target, None)?;
    if let Some(error) = region_capture_validation_error(is_region, region.is_some()) {
        return Err(error.to_string());
    }
    let _apartment = ComApartment::enter();
    let item = create_capture_item(
        &target,
        region.map(|capture| HMONITOR(capture.monitor as *mut core::ffi::c_void)),
    )?;
    let (device, context) = cached_oneshot_device()?;
    let direct3d_device = create_direct3d_device(&device)?;

    let size = item
        .Size()
        .map_err(|error| format!("failed to read capture item size: {error}"))?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err(format!(
            "capture target has zero size ({}x{})",
            size.Width, size.Height
        ));
    }

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &direct3d_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        1,
        size,
    )
    .map_err(|error| format!("failed to create one-shot capture frame pool: {error}"))?;
    let session = pool
        .CreateCaptureSession(&item)
        .map_err(|error| format!("failed to create one-shot capture session: {error}"))?;
    // Picker thumbnails must NOT flash the yellow capture border on every
    // refresh (each one-shot would otherwise briefly outline the window) —
    // macOS parity: the picker never shows capture borders. `IsBorderRequired`
    // only exists on Windows 10 build >= 20348; on older Windows builds it
    // fails and the border shows — accepted (a GDI fallback would trade
    // occlusion correctness for a borderless thumbnail), but never silent.
    if let Err(error) = session.SetIsBorderRequired(false) {
        log::warn!(
            "windows screen capture: one-shot border suppression unsupported on this Windows \
             build ({error}); the yellow capture border will show for picker thumbnails"
        );
    }
    session
        .StartCapture()
        .map_err(|error| format!("failed to start one-shot capture: {error}"))?;

    let mut current_size = size;
    let mut staging = create_staging_texture(
        &device,
        region
            .map(|capture| SizeInt32 {
                Width: capture.output_width as i32,
                Height: capture.output_height as i32,
            })
            .unwrap_or(size),
    )?;
    let mut roi_texture = None;
    let mut canvas_texture = None;
    let deadline = Instant::now() + timeout;
    let frame = loop {
        if let Ok(frame) = pool.TryGetNextFrame() {
            break frame;
        }
        if Instant::now() >= deadline {
            let _ = session.Close();
            let _ = pool.Close();
            return Err(format!(
                "no capture frame arrived within {}ms",
                timeout.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let result = copy_frame_to_bgra(
        frame,
        &pool,
        &direct3d_device,
        &device,
        &context,
        &mut current_size,
        &mut staging,
        &mut roi_texture,
        region.as_ref(),
        &mut canvas_texture,
    );
    let _ = session.Close();
    let _ = pool.Close();
    close_item(&item);
    result
}

/// Everything needed to pump frames after setup completes. Owned by the
/// capture thread; dropped (and WGC objects closed) when the pump loop exits.
struct CaptureSetup {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    direct3d_device: IDirect3DDevice,
    current_size: SizeInt32,
    staging: (ID3D11Texture2D, ID3D11Resource),
    roi_texture: Option<(ID3D11Texture2D, ID3D11Resource)>,
    canvas_texture: Option<(ID3D11Resource, ID3D11RenderTargetView)>,
    region: Option<RegionCaptureSpec>,
    region_paused: bool,
    closed_token: i64,
    arrival_token: i64,
}

fn capture_thread_main(
    token: u32,
    target: WindowsCaptureTarget,
    region: Option<RegionCaptureSpec>,
    indicator_mode: CaptureIndicatorMode,
    state: Arc<CaptureState>,
    signal: Arc<CaptureSignal>,
    on_frame: impl Fn(BgraFrame) + Send + Sync + 'static,
    setup_tx: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let _apartment = ComApartment::enter();
    match setup_capture(token, target, region, indicator_mode, &state, &signal) {
        Ok(mut setup) => {
            // Setup-complete MUST be signaled here, before the pump loop:
            // the loop (and its teardown) only ends when the session is
            // stopped, so sending after it would make every `start()` wait
            // out the full setup timeout for a signal that only arrives at
            // shutdown (observed: "capture setup timed out" on every share).
            let _ = setup_tx.send(Ok(()));
            run_pump_loop(
                token,
                indicator_mode,
                &mut setup,
                &state,
                &signal,
                &on_frame,
            );
        }
        Err(error) => {
            let _ = setup_tx.send(Err(error));
        }
    }
    unregister_capture_signal(token);
}

/// Create the WGC item for `target` and bring the whole capture pipeline
/// live (device, pool, session, handlers, StartCapture). Every step is
/// logged so a hang here is diagnosable from petal.log alone.
fn setup_capture(
    token: u32,
    target: WindowsCaptureTarget,
    region: Option<RegionCaptureSpec>,
    indicator_mode: CaptureIndicatorMode,
    state: &Arc<CaptureState>,
    signal: &Arc<CaptureSignal>,
) -> Result<CaptureSetup, String> {
    log::info!(
        "windows screen capture: setup begin token={token} kind={:?}",
        target.kind()
    );
    let item = create_capture_item(
        &target,
        region.map(|capture| HMONITOR(capture.monitor as *mut core::ffi::c_void)),
    )?;
    log::info!("windows screen capture: setup item created");
    let (device, context) = create_d3d_device()?;
    let direct3d_device = create_direct3d_device(&device)?;
    log::info!("windows screen capture: setup d3d11 device ready");

    let initial_size = item
        .Size()
        .map_err(|error| format!("failed to read capture item size: {error}"))?;
    if initial_size.Width <= 0 || initial_size.Height <= 0 {
        return Err(format!(
            "capture target has zero size ({}x{})",
            initial_size.Width, initial_size.Height
        ));
    }

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &direct3d_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        initial_size,
    )
    .map_err(|error| format!("failed to create capture frame pool: {error}"))?;
    let session = pool
        .CreateCaptureSession(&item)
        .map_err(|error| format!("failed to create capture session: {error}"))?;
    // `IsBorderRequired` is available from Windows 10 build 20348 (not
    // 19041). The coordinator has already proven that borderless consent and
    // the replacement surface are both ready before a `Petal` mode reaches
    // this boundary. Any API failure is logged; the capture itself continues
    // because the system indicator is the safe fallback.
    let mut effective_indicator_mode = indicator_mode;
    let mut system_border_required = indicator_mode.system_border_required();
    if let Err(error) = session.SetIsBorderRequired(system_border_required) {
        log::warn!(
            "windows screen capture: could not configure capture border mode={:?} region={} system_required={system_border_required}: {error}",
            effective_indicator_mode,
            region.is_some()
        );
        if indicator_mode == CaptureIndicatorMode::Petal {
            // A failed false-setting must never leave a custom border next to
            // an uncertain WGC state. Restore WGC's border first, then hide
            // the local replacement before any frame is delivered.
            let fallback = restore_system_indicator_before_disable(
                indicator_mode,
                || {
                    session
                        .SetIsBorderRequired(true)
                        .map_err(|error| error.to_string())
                },
                || {
                    signal.mark_system_indicator_restored();
                    crate::windows_share_overlay::disable_custom_indicator_for_fallback(token);
                },
            );
            match fallback {
                Ok(()) => {
                    effective_indicator_mode = CaptureIndicatorMode::System;
                    system_border_required = true;
                }
                Err(fallback_error) => {
                    let message = format!(
                        "system indicator fallback failed during capture setup: {fallback_error}"
                    );
                    set_terminal_error(state, &message);
                    return Err(message);
                }
            }
        } else {
            let message = format!("required WGC system indicator could not be configured: {error}");
            set_terminal_error(state, &message);
            return Err(message);
        }
    }
    log::info!(
        "windows screen capture: indicator mode={:?} region={} system_required={system_border_required}",
        effective_indicator_mode,
        region.is_some()
    );
    log::info!("windows screen capture: setup pool+session ready");

    // The item's Closed event is the source-gone signal (shared window closed
    // or display unplugged) -> terminal status for the share loss monitor.
    let closed_state = state.clone();
    let closed_handler =
        TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_sender, _args| {
            set_terminal_error(&closed_state, "capture target closed");
            Ok(())
        });
    let closed_token = item
        .Closed(&closed_handler)
        .map_err(|error| format!("failed to register capture Closed event: {error}"))?;

    let arrival_state = state.clone();
    let arrival_signal = signal.clone();
    let arrival_handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
        move |_sender, _args| {
            arrival_state.frames_arrived.fetch_add(1, Ordering::Relaxed);
            arrival_signal.notify_arrival();
            Ok(())
        },
    );
    let arrival_token = pool
        .FrameArrived(&arrival_handler)
        .map_err(|error| format!("failed to register FrameArrived handler: {error}"))?;

    session
        .StartCapture()
        .map_err(|error| format!("failed to start capture: {error}"))?;
    log::info!("windows screen capture: setup StartCapture done");

    let staging_size = region
        .map(|capture| SizeInt32 {
            Width: capture.output_width as i32,
            Height: capture.output_height as i32,
        })
        .unwrap_or(initial_size);
    let staging = create_staging_texture(&device, staging_size)?;
    log::info!(
        "windows screen capture: setup complete token={token} source={}x{} output={}x{} region_generation={:?}",
        initial_size.Width,
        initial_size.Height,
        staging_size.Width,
        staging_size.Height,
        region.map(|capture| capture.generation)
    );

    Ok(CaptureSetup {
        item,
        pool,
        session,
        device,
        context,
        direct3d_device,
        current_size: initial_size,
        staging,
        roi_texture: None,
        canvas_texture: None,
        region,
        region_paused: false,
        closed_token,
        arrival_token,
    })
}

/// Latest-wins pump loop: wait for FrameArrived wakeups, drain and push
/// frames, then tear the WGC objects down on stop. While idle, a periodic
/// arrival/delivery cadence is logged so a silent WGC stream (static or
/// minimized source) reads as "0 arrived" instead of an unexplained freeze.
fn run_pump_loop(
    token: u32,
    indicator_mode: CaptureIndicatorMode,
    setup: &mut CaptureSetup,
    state: &Arc<CaptureState>,
    signal: &Arc<CaptureSignal>,
    on_frame: &(impl Fn(BgraFrame) + Send + Sync),
) {
    let mut seen_arrivals = state.frames_arrived.load(Ordering::Relaxed);
    let mut seen_delivered = state.frames_delivered.load(Ordering::Relaxed);
    let mut arrived_this_interval = 0u64;
    let mut delivered_this_interval = 0u64;
    let mut last_region_geometry_check: Option<Instant> = None;
    loop {
        let mut guard = signal.arrival_mutex.lock_unpoisoned();
        loop {
            if signal.stop_requested()
                || signal.system_indicator_pending()
                || state.frames_arrived.load(Ordering::Relaxed) != seen_arrivals
            {
                break;
            }
            let wait_timeout = if setup.region.is_some() {
                crate::region_window::REGION_GEOMETRY_INTERVAL
            } else {
                CAPTURE_HEALTH_INTERVAL
            };
            let (next_guard, timed_out) = match signal.arrival.wait_timeout(guard, wait_timeout) {
                Ok((guard, result)) => (guard, result.timed_out()),
                Err(poisoned) => {
                    let (guard, _) = poisoned.into_inner();
                    (guard, true)
                }
            };
            guard = next_guard;
            if timed_out && setup.region.is_some() {
                // A region share must poll selector geometry even when the
                // pinned owning display is static: WGC delivers no frames
                // then, and without this break the poll below never runs --
                // a selector dragged back onto its owning display was never
                // noticed and the share stayed paused forever (014A).
                break;
            }
            if timed_out && wait_timeout == CAPTURE_HEALTH_INTERVAL {
                log::info!(
                    "windows screen capture: {arrived_this_interval} frame(s) arrived, {delivered_this_interval} delivered in the last {}s",
                    CAPTURE_HEALTH_INTERVAL.as_secs()
                );
                arrived_this_interval = 0;
                delivered_this_interval = 0;
            }
        }
        if signal.stop_requested() {
            break;
        }
        if signal.system_indicator_pending() {
            // Keep the request latched until the WGC call succeeds. This
            // prevents a concurrent stale overlay event from queueing a
            // second restoration while the first one is in flight.
            let fallback = restore_system_indicator_before_disable(
                indicator_mode,
                || {
                    setup
                        .session
                        .SetIsBorderRequired(true)
                        .map_err(|error| error.to_string())
                },
                || {
                    signal.mark_system_indicator_restored();
                    crate::windows_share_overlay::disable_custom_indicator_for_fallback(token);
                },
            );
            match fallback {
                Ok(()) => {
                    log::info!(
                        "windows screen capture: restored system indicator before disabling Petal replacement token={token}"
                    );
                }
                Err(detail) => {
                    set_terminal_error(
                        state,
                        &format!("system indicator fallback failed; terminating capture: {detail}"),
                    );
                    break;
                }
            }
        }
        if signal.stop_requested() {
            break;
        }
        let arrivals = state.frames_arrived.load(Ordering::Relaxed);
        arrived_this_interval += arrivals - seen_arrivals;
        seen_arrivals = arrivals;
        drop(guard);

        if let Some(previous_region) = setup.region {
            let now = Instant::now();
            if crate::region_window::region_geometry_due(last_region_geometry_check, now) {
                last_region_geometry_check = Some(now);
                match resolve_capture_target(token).and_then(|target| {
                    region_capture_spec(token, &target, Some(previous_region.monitor))
                        .map(|next| (target, next))
                }) {
                    Ok((_target, Some(next_region))) => {
                        if next_region.generation != previous_region.generation
                            || next_region.roi != previous_region.roi
                            || next_region.output_width != previous_region.output_width
                            || next_region.output_height != previous_region.output_height
                            || next_region.offset_x != previous_region.offset_x
                            || next_region.offset_y != previous_region.offset_y
                        {
                            match create_staging_texture(
                                &setup.device,
                                SizeInt32 {
                                    Width: next_region.output_width as i32,
                                    Height: next_region.output_height as i32,
                                },
                            ) {
                                Ok(staging) => setup.staging = staging,
                                Err(error) => {
                                    set_terminal_error(state, &error);
                                    break;
                                }
                            }
                            setup.region = Some(next_region);
                            setup.roi_texture = None;
                            setup.canvas_texture = None;
                        }
                        setup.region_paused = false;
                    }
                    Ok((_target, None)) => {
                        if crate::region_window::resolve(token).is_none() {
                            set_terminal_error(state, "Petal View region registration disappeared");
                            break;
                        }
                        // The selector has no physical overlap with its latched
                        // display. Keep WGC alive, but do not publish a stale ROI
                        // as if it still described the selector.
                        setup.region_paused = true;
                    }
                    Err(error) => {
                        set_terminal_error(state, &error);
                        break;
                    }
                }
            }
        }

        if setup.region_paused {
            continue;
        }

        drain_and_push(
            &setup.pool,
            &setup.direct3d_device,
            &setup.device,
            &setup.context,
            &mut setup.current_size,
            &mut setup.staging,
            &mut setup.roi_texture,
            &mut setup.canvas_texture,
            setup.region.as_ref(),
            state,
            on_frame,
        );
        let delivered = state.frames_delivered.load(Ordering::Relaxed);
        delivered_this_interval += delivered - seen_delivered;
        seen_delivered = delivered;
    }

    let _ = setup.item.RemoveClosed(setup.closed_token);
    let _ = setup.pool.RemoveFrameArrived(setup.arrival_token);
    let _ = setup.session.Close();
    let _ = setup.pool.Close();
    close_item(&setup.item);
    log::info!("windows screen capture: capture session torn down");
}

fn drain_and_push(
    pool: &Direct3D11CaptureFramePool,
    direct3d_device: &IDirect3DDevice,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    current_size: &mut SizeInt32,
    staging: &mut (ID3D11Texture2D, ID3D11Resource),
    roi_texture: &mut Option<(ID3D11Texture2D, ID3D11Resource)>,
    canvas_texture: &mut Option<(ID3D11Resource, ID3D11RenderTargetView)>,
    region: Option<&RegionCaptureSpec>,
    state: &CaptureState,
    on_frame: &(impl Fn(BgraFrame) + Send + Sync),
) {
    // Latest-wins drain: release older frames, keep only the newest.
    let mut last: Option<Direct3D11CaptureFrame> = None;
    for _ in 0..DRAIN_FRAME_BUDGET {
        match pool.TryGetNextFrame() {
            Ok(frame) => {
                if let Some(previous) = last.take() {
                    let _ = previous.Close();
                }
                last = Some(frame);
            }
            Err(_) => break,
        }
    }
    let Some(frame) = last else {
        return;
    };
    match copy_frame_to_bgra(
        frame,
        pool,
        direct3d_device,
        device,
        context,
        current_size,
        staging,
        roi_texture,
        region,
        canvas_texture,
    ) {
        Ok(frame) => {
            state.frames_delivered.fetch_add(1, Ordering::Relaxed);
            *state.captured_size.lock_unpoisoned() = Some((frame.width, frame.height));
            on_frame(frame);
        }
        Err(error) => set_terminal_error(state, &error),
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_frame_to_bgra(
    frame: Direct3D11CaptureFrame,
    pool: &Direct3D11CaptureFramePool,
    direct3d_device: &IDirect3DDevice,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    current_size: &mut SizeInt32,
    staging: &mut (ID3D11Texture2D, ID3D11Resource),
    roi_texture: &mut Option<(ID3D11Texture2D, ID3D11Resource)>,
    region: Option<&RegionCaptureSpec>,
    canvas_texture: &mut Option<(ID3D11Resource, ID3D11RenderTargetView)>,
) -> Result<BgraFrame, String> {
    let content_size = frame
        .ContentSize()
        .map_err(|error| format!("frame ContentSize failed: {error}"))?;
    if content_size.Width <= 0 || content_size.Height <= 0 {
        return Err("capture frame has zero size".to_string());
    }
    if content_size != *current_size {
        // Resize path for BOTH kinds: a shared window resized or a display
        // resolution change. Old pool buffers are dropped; the staging
        // texture is recreated at the new size.
        pool.Recreate(
            direct3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            content_size,
        )
        .map_err(|error| format!("capture pool Recreate failed: {error}"))?;
        *current_size = content_size;
        *staging = create_staging_texture(
            device,
            region
                .map(|capture| SizeInt32 {
                    Width: capture.output_width as i32,
                    Height: capture.output_height as i32,
                })
                .unwrap_or(content_size),
        )?;
        *roi_texture = None;
    }

    let surface = frame
        .Surface()
        .map_err(|error| format!("frame Surface failed: {error}"))?;
    let source = surface_to_d3d11_texture(&surface)?;
    let source_resource: ID3D11Resource = source
        .cast()
        .map_err(|error| format!("texture cast to ID3D11Resource failed: {error}"))?;
    let (output_width, output_height, region_generation) = if let Some(region) = region {
        let right = region.roi.x.saturating_add(region.roi.width);
        let bottom = region.roi.y.saturating_add(region.roi.height);
        if right > content_size.Width as u32 || bottom > content_size.Height as u32 {
            return Err("Petal View ROI exceeds the WGC monitor texture".to_string());
        }
        if roi_texture.is_none() {
            *roi_texture = Some(create_gpu_texture(
                device,
                SizeInt32 {
                    Width: region.roi.width as i32,
                    Height: region.roi.height as i32,
                },
            )?);
        }
        if canvas_texture.is_none() {
            *canvas_texture = Some(create_gpu_canvas_texture(
                device,
                SizeInt32 {
                    Width: region.output_width as i32,
                    Height: region.output_height as i32,
                },
            )?);
        }
        let Some((_, roi_resource)) = roi_texture.as_ref() else {
            return Err("Petal View GPU ROI texture was not created".to_string());
        };
        let Some((canvas_resource, canvas_rtv)) = canvas_texture.as_ref() else {
            return Err("Petal View GPU canvas texture was not created".to_string());
        };
        let source_box = D3D11_BOX {
            left: region.roi.x,
            top: region.roi.y,
            front: 0,
            right,
            bottom,
            back: 1,
        };
        unsafe {
            context.CopySubresourceRegion(
                roi_resource,
                0,
                0,
                0,
                0,
                &source_resource,
                0,
                Some(&source_box as *const D3D11_BOX),
            );
            context.ClearRenderTargetView(canvas_rtv, &[0.0, 0.0, 0.0, 1.0]);
            context.CopySubresourceRegion(
                canvas_resource,
                0,
                region.offset_x,
                region.offset_y,
                0,
                roi_resource,
                0,
                None,
            );
            context.CopyResource(&staging.1, canvas_resource);
        }
        (
            region.output_width as usize,
            region.output_height as usize,
            Some(region.generation),
        )
    } else {
        unsafe {
            context.CopyResource(&staging.1, &source_resource);
        }
        (
            content_size.Width as usize,
            content_size.Height as usize,
            None,
        )
    };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context
            .Map(&staging.1, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|error| format!("staging Map failed: {error}"))?;
    }

    let width = output_width;
    let height = output_height;
    let bytes_per_row = width
        .checked_mul(4)
        .ok_or_else(|| "capture frame width overflow".to_string())?;
    let total = bytes_per_row
        .checked_mul(height)
        .ok_or_else(|| "capture frame size overflow".to_string())?;
    let mut bgra = vec![0u8; total];
    unsafe {
        let src_base = mapped.pData as *const u8;
        for row in 0..height {
            std::ptr::copy_nonoverlapping(
                src_base.add(row * mapped.RowPitch as usize),
                bgra.as_mut_ptr().add(row * bytes_per_row),
                bytes_per_row,
            );
        }
        context.Unmap(&staging.1, 0);
    }
    let _ = frame.Close();

    let (frame_width, frame_height) = frame_output_dimensions(content_size, region);
    debug_assert_eq!(frame_width as usize, output_width);
    debug_assert_eq!(frame_height as usize, output_height);
    Ok(BgraFrame {
        bgra,
        bytes_per_row,
        width: frame_width,
        height: frame_height,
        capture_wall_time_us: crate::time_util::now_us(),
        region_generation,
    })
}

fn frame_output_dimensions(
    content_size: SizeInt32,
    region: Option<&RegionCaptureSpec>,
) -> (u32, u32) {
    region
        .map(|capture| (capture.output_width, capture.output_height))
        .unwrap_or((content_size.Width as u32, content_size.Height as u32))
}

fn create_gpu_texture(
    device: &ID3D11Device,
    size: SizeInt32,
) -> Result<(ID3D11Texture2D, ID3D11Resource), String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.Width as u32,
        Height: size.Height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|error| format!("CreateTexture2D GPU ROI failed: {error}"))?;
    let texture =
        texture.ok_or_else(|| "CreateTexture2D GPU ROI returned no texture".to_string())?;
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| format!("GPU ROI texture cast failed: {error}"))?;
    Ok((texture, resource))
}

fn create_gpu_canvas_texture(
    device: &ID3D11Device,
    size: SizeInt32,
) -> Result<(ID3D11Resource, ID3D11RenderTargetView), String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.Width as u32,
        Height: size.Height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|error| format!("CreateTexture2D GPU canvas failed: {error}"))?;
    let texture =
        texture.ok_or_else(|| "CreateTexture2D GPU canvas returned no texture".to_string())?;
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| format!("GPU canvas texture cast failed: {error}"))?;
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    unsafe { device.CreateRenderTargetView(Some(&resource), None, Some(&mut rtv)) }
        .map_err(|error| format!("CreateRenderTargetView GPU canvas failed: {error}"))?;
    let rtv =
        rtv.ok_or_else(|| "CreateRenderTargetView returned no GPU canvas view".to_string())?;
    Ok((resource, rtv))
}

fn create_staging_texture(
    device: &ID3D11Device,
    size: SizeInt32,
) -> Result<(ID3D11Texture2D, ID3D11Resource), String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.Width as u32,
        Height: size.Height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|error| format!("CreateTexture2D failed: {error}"))?;
    let texture = texture.ok_or_else(|| "CreateTexture2D returned no texture".to_string())?;
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| format!("staging texture cast to ID3D11Resource failed: {error}"))?;
    Ok((texture, resource))
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let result = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    };
    if let Err(hardware_error) = result {
        log::warn!(
            "windows screen capture: hardware D3D11 device failed ({hardware_error}); retrying with WARP"
        );
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|error| format!("D3D11CreateDevice (WARP fallback) failed: {error}"))?;
    }
    Ok((
        device.ok_or_else(|| "D3D11CreateDevice returned no device".to_string())?,
        context.ok_or_else(|| "D3D11CreateDevice returned no context".to_string())?,
    ))
}

fn create_direct3d_device(device: &ID3D11Device) -> Result<IDirect3DDevice, String> {
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|error| format!("device cast to IDXGIDevice failed: {error}"))?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
        .map_err(|error| format!("CreateDirect3D11DeviceFromDXGIDevice failed: {error}"))?;
    let direct3d: IDirect3DDevice = inspectable
        .cast()
        .map_err(|error| format!("IDirect3DDevice cast failed: {error}"))?;
    Ok(direct3d)
}

/// Resolve a token to its native target with the kind-appropriate validity
/// gate. This is the ONLY place a token's target is checked against
/// `IsWindow` — tokens are generated registry ids, never raw handles, so the
/// registry kind is the dispatch.
fn resolve_capture_target(token: u32) -> Result<WindowsCaptureTarget, String> {
    let target = windows_capture_target::resolve(token).map_err(|error| match error {
        windows_capture_target::TargetRegistryError::UnknownOrStale(_) => {
            "capture target is unknown or stale".to_string()
        }
        other => format!("capture target resolve failed: {other}"),
    })?;
    if target.kind() == TargetKind::Window {
        let hwnd = HWND(target.raw_handle() as *mut core::ffi::c_void);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err("window no longer exists".to_string());
        }
    }
    Ok(target)
}

/// Process-lifetime anchor: a held `IGraphicsCaptureItemInterop` reference
/// keeps Windows.Graphics.Capture's DLL loaded for the whole process. Called
/// from every item-creation path before the first WGC object is created.
///
/// SAFETY: `IGraphicsCaptureItemInterop` is the WinRT activation factory of
/// `GraphicsCaptureItem` — a COM singleton that is inherently
/// thread-safe (same reasoning as `transport::camera::mf::CameraCapture`'s
/// `Send` impl); the reference is set once and never released, so no
/// lifetime race is possible.
struct WgcAnchor(IGraphicsCaptureItemInterop);
unsafe impl Send for WgcAnchor {}
unsafe impl Sync for WgcAnchor {}

fn wgc_dll_anchor() -> &'static Mutex<Option<WgcAnchor>> {
    static ANCHOR: Mutex<Option<WgcAnchor>> = Mutex::new(None);
    &ANCHOR
}

/// Shared D3D11 device for one-shot picker thumbnails. Creating a hardware
/// (or WARP) D3D11 device is expensive (~tens of ms); reusing one across all
/// one-shot captures removes that per-thumbnail cost. `ID3D11Device` is
/// thread-safe, so the mutex-guarded cache is safe across the concurrent
/// thumbnail tasks. The per-call `IDirect3DDevice` wrapper and staging
/// textures derive from this device and are created per call.
static ONESHOT_DEVICE: Mutex<Option<(ID3D11Device, ID3D11DeviceContext)>> = Mutex::new(None);

/// Return the shared one-shot device, creating it on first use or when the
/// cached device has been lost (GPU reset / driver TDR).
fn cached_oneshot_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut guard = ONESHOT_DEVICE
        .lock()
        .map_err(|_| "oneshot device cache poisoned")?;
    if let Some((device, _)) = guard.as_ref() {
        // `GetDeviceRemovedReason` returns Ok while the device is healthy;
        // any Err means the device was lost and must be recreated.
        let healthy = unsafe { device.GetDeviceRemovedReason() }.is_ok();
        if healthy {
            return Ok(guard.as_ref().expect("checked Some").clone());
        }
        log::warn!("windows screen capture: one-shot D3D11 device lost; recreating");
        *guard = None;
    }
    let (device, context) = create_d3d_device()?;
    *guard = Some((device.clone(), context.clone()));
    Ok((device, context))
}

fn region_capture_validation_error(is_region: bool, has_region_spec: bool) -> Option<&'static str> {
    (is_region && !has_region_spec).then_some("Petal View has no overlap with its owning display")
}

fn region_capture_spec(
    token: u32,
    target: &WindowsCaptureTarget,
    preferred_monitor: Option<usize>,
) -> Result<Option<RegionCaptureSpec>, String> {
    if target.kind() != TargetKind::Window {
        return Ok(None);
    }
    // The Windows window registry's CoreGraphics-compatible lookup is a
    // deliberate no-op. Refresh the selector from the HWND directly so the
    // ROI follows native drag/resize events even when the excluded selector
    // itself causes no monitor frame to arrive.
    if let Some(frame) = crate::platform::windows::window_frame_for_raw(target.raw_handle()) {
        crate::region_window::update_frame(
            token,
            crate::region_window::RegionRect::new(
                frame.x as f64,
                frame.y as f64,
                frame.width as f64,
                frame.height as f64,
            ),
        );
    }
    let Some(source) = crate::region_window::resolve(token) else {
        // Ordinary window/display capture uses the target itself. Only a
        // registered Petal selector needs ROI geometry and display-overlap
        // validation.
        return Ok(None);
    };
    let hwnd = HWND(target.raw_handle() as *mut core::ffi::c_void);
    let monitor = preferred_monitor
        .map(|raw| HMONITOR(raw as *mut core::ffi::c_void))
        .unwrap_or_else(|| unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) });
    if monitor.0.is_null() {
        return Err("Petal View selector has no containing display".to_string());
    }
    let mut monitor_info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut monitor_info) }.as_bool() {
        return Err("could not query the Petal View display bounds".to_string());
    }
    let display_token = windows_capture_target::register_display(monitor.0 as usize)
        .map_err(|error| format!("could not register the Petal View display: {error}"))?;
    let display = crate::region_window::RegionDisplay {
        id: display_token,
        frame: crate::region_window::RegionRect::new(
            monitor_info.rcMonitor.left as f64,
            monitor_info.rcMonitor.top as f64,
            (monitor_info.rcMonitor.right - monitor_info.rcMonitor.left) as f64,
            (monitor_info.rcMonitor.bottom - monitor_info.rcMonitor.top) as f64,
        ),
        // Per-monitor-v2 coordinates and Win32 monitor bounds are physical
        // pixels here; no additional logical-to-physical conversion applies.
        scale: 1.0,
    };
    crate::region_window::update_display(token, Some(display));
    let clipped = display.clipped_physical_roi(source.frame);
    if let Some(outside) = crate::region_window::set_outside_display(token, clipped.is_none()) {
        if outside {
            log::warn!(
                "windows screen capture: Petal View {token} is outside its owning display; holding the last good frame"
            );
        } else {
            log::info!("windows screen capture: Petal View {token} returned to its owning display");
        }
    }
    let Some(clipped) = clipped else {
        return Ok(None);
    };
    let generation = crate::region_window::resolve(token)
        .map(|current| current.generation.0)
        .unwrap_or(source.generation.0);
    Ok(Some(RegionCaptureSpec {
        monitor: monitor.0 as usize,
        roi: clipped.roi,
        output_width: clipped.output_width,
        output_height: clipped.output_height,
        offset_x: clipped.offset_x,
        offset_y: clipped.offset_y,
        generation,
    }))
}

fn create_capture_item(
    target: &WindowsCaptureTarget,
    region_monitor: Option<HMONITOR>,
) -> Result<GraphicsCaptureItem, String> {
    // Use (and thereby pin) the process-lifetime WGC anchor: without a held
    // reference, releasing the last Windows.Graphics.Capture object can unload
    // GraphicsCapture.dll while WGC's internal frame-delivery thread still
    // runs, terminating the process with a BEX
    // (`GraphicsCapture.dll_unloaded`) — the classic rapid
    // create/capture/teardown crash, reproduced by repeated one-shot picker
    // thumbnails on this host.
    let anchor_slot = wgc_dll_anchor();
    let mut anchor_guard = anchor_slot.lock_unpoisoned();
    if anchor_guard.is_none() {
        *anchor_guard = Some(WgcAnchor(
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .map_err(|error| format!("failed to get GraphicsCaptureItem factory: {error}"))?,
        ));
    }
    let interop = &anchor_guard.as_ref().expect("WGC anchor populated").0;
    if let Some(hmonitor) = region_monitor {
        return unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(hmonitor) }
            .map_err(|error| format!("WGC CreateForMonitor failed: {error}"));
    }
    match target.kind() {
        TargetKind::Window => {
            let hwnd = HWND(target.raw_handle() as *mut core::ffi::c_void);
            unsafe { interop.CreateForWindow::<GraphicsCaptureItem>(hwnd) }
                .map_err(|error| format!("WGC CreateForWindow failed: {error}"))
        }
        TargetKind::Display => {
            let hmonitor = HMONITOR(target.raw_handle() as *mut core::ffi::c_void);
            unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(hmonitor) }
                .map_err(|error| format!("WGC CreateForMonitor failed: {error}"))
        }
    }
}

fn close_item(item: &GraphicsCaptureItem) {
    if let Ok(closable) = Interface::cast::<windows::Foundation::IClosable>(item) {
        let _ = closable.Close();
    }
}

fn set_terminal_error(state: &CaptureState, message: &str) {
    let mut guard = state.terminal_error.lock_unpoisoned();
    if guard.is_none() {
        log::error!("windows screen capture: {message}");
        *guard = Some(message.to_string());
    }
}

/// The undocumented `IDirect3DDXGIInterfaceAccess` (IID
/// A9B3D012-3DF2-4EE3-B8D1-8695F457D3C1) that every WinRT `IDirect3DSurface`
/// implements. `GetInterface` returns the underlying DXGI object — here the
/// capture surface's `ID3D11Texture2D`. windows-rs does not generate it
/// (undocumented), so the vtable is declared by hand; this is the same
/// interop Microsoft's own WGC samples use.
#[repr(transparent)]
#[derive(Clone, PartialEq, Eq)]
struct IDirect3DDXGIInterfaceAccess(windows::core::IUnknown);

#[repr(C)]
#[allow(non_snake_case)]
struct IDirect3DDXGIInterfaceAccess_Vtbl {
    base__: windows::core::IUnknown_Vtbl,
    #[allow(non_snake_case)]
    GetInterface: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        iid: *const windows::core::GUID,
        interface: *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT,
}

unsafe impl windows::core::Interface for IDirect3DDXGIInterfaceAccess {
    type Vtable = IDirect3DDXGIInterfaceAccess_Vtbl;
    const IID: windows::core::GUID =
        windows::core::GUID::from_u128(0xA9B3D012_3DF2_4EE3_B8D1_8695F457D3C1);
}

impl IDirect3DDXGIInterfaceAccess {
    #[allow(non_snake_case)]
    unsafe fn GetInterface<T: windows::core::Interface>(&self) -> windows::core::Result<T> {
        let mut result = std::ptr::null_mut();
        (windows::core::Interface::vtable(self).GetInterface)(
            windows::core::Interface::as_raw(self),
            &T::IID,
            &mut result,
        )
        .ok()?;
        Ok(windows::core::Type::from_abi(result)?)
    }
}

/// Resolve a WGC frame surface to the underlying `ID3D11Texture2D` via
/// `IDirect3DDXGIInterfaceAccess::GetInterface` (a direct QI to
/// `ID3D11Texture2D` fails — the WinRT surface object does not implement it
/// as a COM interface).
fn surface_to_d3d11_texture(surface: &IDirect3DSurface) -> Result<ID3D11Texture2D, String> {
    let access: IDirect3DDXGIInterfaceAccess = surface.cast().map_err(|error| {
        format!("surface does not expose IDirect3DDXGIInterfaceAccess: {error}")
    })?;
    unsafe { access.GetInterface::<ID3D11Texture2D>() }
        .map_err(|error| format!("IDirect3DDXGIInterfaceAccess::GetInterface failed: {error}"))
}

struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Self {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized == RPC_E_CHANGED_MODE {
            return Self(false);
        }
        if initialized.is_ok() {
            Self(true)
        } else {
            log::warn!("windows screen capture: CoInitializeEx failed: {initialized}");
            Self(false)
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borderless_policy_allows_only_allowed_status() {
        for status in [
            AppCapabilityAccessStatus::DeniedBySystem,
            AppCapabilityAccessStatus::NotDeclaredByApp,
            AppCapabilityAccessStatus::DeniedByUser,
            AppCapabilityAccessStatus::UserPromptRequired,
        ] {
            assert_eq!(
                borderless_access_from_status(status),
                BorderlessAccess::Denied
            );
        }
        assert_eq!(
            borderless_access_from_status(AppCapabilityAccessStatus::Allowed),
            BorderlessAccess::Allowed
        );
    }

    #[test]
    fn capture_indicator_policy_requires_both_consent_and_replacement() {
        assert_eq!(
            capture_indicator_mode(BorderlessAccess::Denied, true),
            CaptureIndicatorMode::System
        );
        assert_eq!(
            capture_indicator_mode(BorderlessAccess::Allowed, false),
            CaptureIndicatorMode::System
        );
        assert_eq!(
            capture_indicator_mode(BorderlessAccess::Allowed, true),
            CaptureIndicatorMode::Petal
        );
        assert!(CaptureIndicatorMode::System.system_border_required());
        assert!(!CaptureIndicatorMode::Petal.system_border_required());
    }

    #[test]
    fn runtime_indicator_fallback_is_one_shot_and_fails_closed() {
        let signal = CaptureSignal::new();
        assert!(signal.request_system_indicator());
        assert!(!signal.request_system_indicator());
        assert!(signal.system_indicator_requested());
        assert!(signal.system_indicator_pending());
        signal.mark_system_indicator_restored();
        assert!(!signal.system_indicator_pending());
        assert!(!signal.request_system_indicator());
        assert_eq!(
            runtime_indicator_fallback_outcome(CaptureIndicatorMode::Petal, true),
            Some(RuntimeIndicatorFallbackOutcome::Restored)
        );
        assert_eq!(
            runtime_indicator_fallback_outcome(CaptureIndicatorMode::Petal, false),
            Some(RuntimeIndicatorFallbackOutcome::Terminal)
        );
        assert_eq!(
            runtime_indicator_fallback_outcome(CaptureIndicatorMode::System, false),
            None
        );
    }

    #[test]
    fn capture_signal_request_wakes_the_waiting_capture_thread() {
        let signal = Arc::new(CaptureSignal::new());
        let ready = Arc::new(std::sync::Barrier::new(2));
        let waiter_signal = signal.clone();
        let waiter_ready = ready.clone();
        let waiter = std::thread::spawn(move || {
            let mut guard = waiter_signal.arrival_mutex.lock_unpoisoned();
            waiter_ready.wait();
            while !waiter_signal.system_indicator_pending() {
                let (next_guard, timed_out) = waiter_signal
                    .arrival
                    .wait_timeout(guard, Duration::from_secs(1))
                    .expect("capture signal wait must not poison");
                guard = next_guard;
                assert!(
                    !timed_out.timed_out(),
                    "fallback request did not wake the waiter"
                );
            }
        });
        ready.wait();
        assert!(signal.request_system_indicator());
        waiter.join().expect("capture signal waiter must exit");
    }

    #[test]
    fn fallback_transition_restores_system_indicator_before_disabling_petal() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let restore_operations = operations.clone();
        let disable_operations = operations.clone();
        assert!(restore_system_indicator_before_disable(
            CaptureIndicatorMode::Petal,
            move || {
                restore_operations.lock_unpoisoned().push("restore-system");
                Ok(())
            },
            move || disable_operations.lock_unpoisoned().push("disable-petal"),
        )
        .is_ok());
        assert_eq!(
            *operations.lock_unpoisoned(),
            vec!["restore-system", "disable-petal"]
        );

        let operations = Arc::new(Mutex::new(Vec::new()));
        let restore_operations = operations.clone();
        let disable_operations = operations.clone();
        let result = restore_system_indicator_before_disable(
            CaptureIndicatorMode::Petal,
            move || {
                restore_operations.lock_unpoisoned().push("restore-system");
                Err("restore failed".to_string())
            },
            move || disable_operations.lock_unpoisoned().push("disable-petal"),
        );
        assert_eq!(result, Err("restore failed".to_string()));
        assert_eq!(*operations.lock_unpoisoned(), vec!["restore-system"]);
    }

    #[test]
    fn capture_signal_registry_is_token_scoped_and_cleanup_is_idempotent() {
        let token = 0xfeed_beefu32;
        unregister_capture_signal(token);
        let signal = Arc::new(CaptureSignal::new());
        register_capture_signal(token, &signal);
        assert!(request_system_indicator_fallback(token));
        assert!(!request_system_indicator_fallback(token));
        unregister_capture_signal(token);
        assert!(!request_system_indicator_fallback(token));
        unregister_capture_signal(token);

        let stale_token = token.wrapping_add(1);
        unregister_capture_signal(stale_token);
        let stale_signal = Arc::new(CaptureSignal::new());
        register_capture_signal(stale_token, &stale_signal);
        drop(stale_signal);
        assert!(!request_system_indicator_fallback(stale_token));
        assert!(!capture_signal_registry()
            .lock_unpoisoned()
            .contains_key(&stale_token));
    }

    #[test]
    fn source_policy_requires_verified_window_ownership_and_display_affinity() {
        assert_eq!(
            capture_indicator_mode_for_source(
                BorderlessAccess::Allowed,
                CaptureSourceKind::Window,
                true,
                false,
                true,
            ),
            CaptureIndicatorMode::Petal
        );
        assert_eq!(
            capture_indicator_mode_for_source(
                BorderlessAccess::Allowed,
                CaptureSourceKind::Window,
                true,
                false,
                false,
            ),
            CaptureIndicatorMode::System
        );
        assert_eq!(
            capture_indicator_mode_for_source(
                BorderlessAccess::Allowed,
                CaptureSourceKind::Window,
                false,
                false,
                true,
            ),
            CaptureIndicatorMode::System
        );
        assert_eq!(
            capture_indicator_mode_for_source(
                BorderlessAccess::Allowed,
                CaptureSourceKind::Display,
                true,
                false,
                false,
            ),
            CaptureIndicatorMode::System
        );
        assert_eq!(
            capture_indicator_mode_for_source(
                BorderlessAccess::Allowed,
                CaptureSourceKind::Display,
                true,
                true,
                false,
            ),
            CaptureIndicatorMode::Petal
        );
        assert_eq!(
            capture_indicator_mode_for_source(
                BorderlessAccess::Allowed,
                CaptureSourceKind::DisplayRegion,
                true,
                false,
                false,
            ),
            CaptureIndicatorMode::System
        );
        assert_eq!(
            capture_indicator_mode_for_source(
                BorderlessAccess::Allowed,
                CaptureSourceKind::DisplayRegion,
                true,
                true,
                false,
            ),
            CaptureIndicatorMode::Petal
        );
        for source in [
            CaptureSourceKind::Window,
            CaptureSourceKind::Display,
            CaptureSourceKind::DisplayRegion,
        ] {
            assert_eq!(
                capture_indicator_mode_for_source(
                    BorderlessAccess::Denied,
                    source,
                    true,
                    true,
                    true,
                ),
                CaptureIndicatorMode::System
            );
        }
    }

    /// The one-shot device cache must return the SAME device pair across
    /// calls (no per-call D3D11CreateDevice) and recreate after the cache is
    /// cleared (device-loss path). Device creation alone does not touch WGC
    /// item/session teardown, so this is safe on hosts where repeated WGC
    /// one-shots crash the process (documented `GraphicsCapture.dll_unloaded`
    /// BEX).
    #[test]
    fn ordinary_capture_does_not_require_region_geometry() {
        assert_eq!(region_capture_validation_error(false, false), None);
        assert_eq!(
            region_capture_validation_error(true, false),
            Some("Petal View has no overlap with its owning display")
        );
        assert_eq!(region_capture_validation_error(true, true), None);
    }

    #[test]
    fn region_capture_error_describes_gpu_roi_failure() {
        assert_eq!(
            REGION_CAPTURE_GPU_ROI_FAILED,
            "Windows display-region capture could not maintain the GPU ROI path"
        );
    }

    #[test]
    fn region_frame_dimensions_preserve_selector_canvas() {
        let region = RegionCaptureSpec {
            monitor: 1,
            roi: crate::region_window::PhysicalRegion {
                x: 20,
                y: 30,
                width: 640,
                height: 400,
            },
            output_width: 800,
            output_height: 500,
            offset_x: 0,
            offset_y: 0,
            generation: 2,
        };
        assert_eq!(
            frame_output_dimensions(
                SizeInt32 {
                    Width: 1920,
                    Height: 1080
                },
                Some(&region)
            ),
            (800, 500)
        );
        assert_eq!(
            frame_output_dimensions(
                SizeInt32 {
                    Width: 1920,
                    Height: 1080
                },
                None
            ),
            (1920, 1080)
        );
    }

    #[test]
    fn cached_oneshot_device_reuses_and_recreates() {
        let _apartment = ComApartment::enter();
        let (d1, _c1) = cached_oneshot_device().expect("first device creation");
        let (d2, _c2) = cached_oneshot_device().expect("second device creation");
        // Same cached device, not a fresh one (ID3D11Device PartialEq is
        // COM pointer identity).
        assert_eq!(d1, d2);
        // Simulate device loss by clearing the cache: the next call must
        // create a fresh device (pointer differs).
        *ONESHOT_DEVICE.lock().unwrap() = None;
        let (d3, _c3) = cached_oneshot_device().expect("recreation after cache clear");
        assert_ne!(d1, d3);
        // Leave the cache populated for subsequent tests/process lifetime.
        let _ = cached_oneshot_device();
    }
}
