#![cfg(target_os = "windows")]
//! Receiver-side native compositor: renders remote window/display shares into
//! movable/resizable borderless windows.
//!
//! Each remote share is a Tauri `WebviewWindow` hosting the SAME Svelte
//! surface route the macOS compositor uses (`compositor/surface.html`). The
//! webview renders the real `RemoteWindowHeader.svelte` header strip (identity
//! colors, styled buttons, drag) at the top; the decoded video
//! renders in a native child HWND positioned BELOW the header (mirroring how
//! macOS attaches the video layer below the panel's header). The colored
//! border is the owner-color ring painted by the webview page via the
//! borderColor/borderStroke/borderRadius query params (the same contract the
//! macOS surface route receives).
//!
//! One dedicated compositor thread (spawned lazily) runs a Win32 message
//! loop for the VIDEO CHILD windows; ALL D3D11 state lives on that thread.
//! Commands arrive over a bounded sync channel (`Command`); frame commands
//! for the same window are coalesced to latest-wins before rendering.
//!
//! Rendering uses the supported CPU I420→BGRA path: conversion into a dynamic
//! D3D11 texture, `CopyResource` to the swap-chain back buffer, then `Present`.
//!
//! `DXGI_ERROR_DEVICE_REMOVED`/`DEVICE_RESET` (e.g. WARP/VM hosts, GPU
//! driver resets) recreate the device + every swap chain + texture and
//! re-present the latest stored frame — windows never die from a device
//! loss.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use crate::sync_ext::MutexExt;
use crate::transport::publisher::SharedSourceKind;
use crate::video_color::VideoColorProfile;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tokio_util::sync::CancellationToken;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Resource,
    ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DYNAMIC,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory2, IDXGISwapChain1, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_ERROR_DEVICE_RESET, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetParent, GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindowVisible, RegisterClassW,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, GWLP_USERDATA, HWND_MESSAGE, HWND_TOP, MSG, SWP_HIDEWINDOW, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_SHOWNOACTIVATE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ERASEBKGND, WM_QUIT, WM_SIZE, WM_TIMER, WNDCLASSW, WS_CHILD,
    WS_VISIBLE,
};

/// A remote share is keyed by (owner identity, published window id) — the
/// same two-tuple the macOS compositor uses.
pub(crate) type WindowKey = (String, u32);

/// Bounded command queue; the receiver coalesces frames per key, so this
/// never grows without bound (verification item 10's bounded-queue gate).
const COMMAND_QUEUE_CAPACITY: usize = 16;
/// Message-pump cadence: how often queued commands are drained.
const PUMP_TICK_MS: u32 = 16;
/// Height of the remote-window header strip, in client pixels. Mirrors the
/// macOS compositor's `HEADER_HEIGHT` (44.0 logical points) and
/// `RemoteWindowHeader.svelte`'s rendered 44px bar; the video renders below
/// it in the client area.
const HEADER_HEIGHT: i32 = 44;
/// Placeholder size for a window created before its first frame arrives; the
/// first `Frame` command resizes it to the published dimensions. This is the
/// VIDEO content size (excluding the header strip).
const DEFAULT_WINDOW_SIZE: (u32, u32) = (800, 600);
/// Keep a newly received remote surface reachable on the receiver even when
/// the publisher shared a display larger than the receiver's monitor.
const INITIAL_MAX_WORK_AREA_FRACTION: f64 = 0.8;
/// A letterbox content-rect change must persist for this long before the
/// presented crop (window size + swap chain) follows it — a sender drag
/// changes the bars every frame, and resizing the window per frame would
/// jank. The sender re-anchors the published size ~2s after a settle, so
/// this lag is only visible mid-drag.
const CROP_SETTLE_DWELL: std::time::Duration = std::time::Duration::from_millis(400);
const VIDEO_WINDOW_CLASS: &str = "PetalRemoteVideo";
const PUMP_WINDOW_CLASS: &str = "PetalCompositorPump";
const PUMP_TIMER_ID: usize = 1;

/// Remote-window border stroke (macOS `SCREENSHARE_BORDER_STROKE_PX`),
/// forwarded to the surface route as `borderStroke` so the webview paints the
/// owner-colored ring (same contract macOS sends).
const BORDER_STROKE_PX: i32 = 4;
/// Remote-window border corner radius (macOS `SCREENSHARE_BORDER_RADIUS_PX`),
/// forwarded to the surface route as `borderRadius`.
const BORDER_RADIUS_PX: i32 = 10;

/// Keep this native owner palette in lockstep with the TS PALETTE and hex map
/// in apps/desktop/src/lib/data/identityColor.ts and the macOS compositor's
/// OWNER_COLOR_PALETTE_HEX (compositor.rs).
const OWNER_COLOR_PALETTE_HEX: [&str; 6] = [
    "#f06cc9", // plum
    "#6e8bff", // blue
    "#7ff0a3", // green
    "#e8b84b", // amber
    "#d6b8f0", // lilac
    "#8fa6b8", // slate
];

/// Deterministic owner-palette pick matching the macOS compositor's
/// `owner_color_hex` and the frontend's `colorForIdentity`: fold UTF-16 code
/// units with `h = h * 31 + unit`, then mod the palette length. The header
/// tint, ink, and border color all derive from this single hash of the owner
/// identity, so a given sharer always gets the same color across peers.
fn owner_palette_hex(owner_identity: &str) -> &'static str {
    let hash = owner_identity.encode_utf16().fold(0u32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(unit as u32)
    });
    OWNER_COLOR_PALETTE_HEX[(hash as usize) % OWNER_COLOR_PALETTE_HEX.len()]
}

/// Percent-encode a surface-route query value (arbitrary OS/user strings).
fn percent_encode(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// Surface webview URL for one remote window, carrying the header metadata as
/// query params — the exact same route and contract the macOS compositor's
/// `surface_route_url` produces, so `RemoteWindowHeader.svelte` renders the
/// identity-colored header and the webview paints the owner-colored border.
/// `owner` is the human-readable display name (macOS passes the same); the
/// UUID identity travels separately as `ownerIdentity` and drives the color
/// hash, exactly like the macOS header query string.
fn surface_route_url(
    window_id: u32,
    owner_identity: &str,
    owner_display_name: &str,
    source_title: &str,
    source_url: Option<&str>,
    source_kind: SharedSourceKind,
    remote_control_available: bool,
    share_instance_id: Option<&str>,
    control_mode: crate::remote_control_core::RemoteControlMode,
) -> String {
    let remote_control = u8::from(remote_control_available);
    let share_instance_id = share_instance_id.map(percent_encode).unwrap_or_default();
    let control_mode = match control_mode {
        crate::remote_control_core::RemoteControlMode::FullControl => "fullControl",
        _ => "cursorPreserving",
    };
    let mut route = format!(
        "compositor/surface.html?windowId={window_id}&owner={}&title={}&ownerIdentity={}&borderColor=%23{}&borderStroke={BORDER_STROKE_PX}&borderRadius={BORDER_RADIUS_PX}&remoteControl={remote_control}&targetKind={}&shareInstanceId={share_instance_id}&controlMode={control_mode}",
        percent_encode(owner_display_name),
        percent_encode(source_title),
        percent_encode(owner_identity),
        &owner_palette_hex(owner_identity)[1..],
        source_kind.as_wire(),
    );
    if let Some(source_url) =
        source_url.and_then(crate::browser_url::privacy_minimized_openable_url)
    {
        route.push_str("&url=");
        route.push_str(&percent_encode(&source_url));
    }
    route
}

/// Stable, unique Tauri window label per remote share.
///
/// Tauri window labels allow only `[A-Za-z0-9-/_:]`. LiveKit participant ids
/// can contain `-`, `@`, `.`, `:`, `_` and unicode, so every disallowed
/// character is replaced with `_` (an allowed character). The mapping is
/// deterministic and injective for identities that differ only in allowed
/// characters; the `-<window_id>` suffix keeps distinct shares of the same
/// owner distinct. (Percent-encoding is NOT usable here: it emits `%`, which
/// Tauri's label validator rejects — that is exactly the runtime error seen
/// in the field: "Window labels must only include alphanumeric characters,
/// `-`, `/`, `:` and `_`".)
fn remote_window_label(key: &WindowKey) -> String {
    let safe_owner: String = key
        .0
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '/' | ':' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("petal-remote-{safe_owner}-{}", key.1)
}

/// WebviewWindow label of the pointer overlay for a compositor window
/// (transparent click-through webview sized to the video content area). Same
/// sanitization rule as `remote_window_label` (Tauri label charset).
fn pointer_overlay_label(key: &WindowKey) -> String {
    let safe_owner: String = key
        .0
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '/' | ':' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("petal-pointer-{safe_owner}-{}", key.1)
}

/// WebviewWindow label of the control overlay for a compositor window (the
/// input-capture surface hosting `compositor/control.html`). Same
/// sanitization rule as `pointer_overlay_label`; a distinct namespace so the
/// two sibling overlays never collide.
fn control_overlay_label(key: &WindowKey) -> String {
    let safe_owner: String = key
        .0
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '/' | ':' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("petal-control-{safe_owner}-{}", key.1)
}

/// Labels of every pointer overlay whose compositor window is owned by
/// `owner_identity` and carries this window id. Both must match: HWND-derived
/// window ids collide across senders routinely, so a single-id filter would
/// bleed one participant's cursor dots onto another's window.
pub(crate) fn pointer_overlay_labels_for(owner_identity: &str, window_id: u32) -> Vec<String> {
    snapshot()
        .lock_unpoisoned()
        .iter()
        .filter(|window| window.owner_identity == owner_identity && window.window_id == window_id)
        .map(|window| pointer_overlay_label(&(window.owner_identity.clone(), window.window_id)))
        .collect()
}

/// Letterbox crop of the displayed surface for (owner, window), as fractions of
/// the full source frame. The telepointer receiver maps full-frame normalized
/// coords through this so the tag stays glued to the visible content when the
/// crop re-anchors (bars appearing/disappearing) — otherwise the tag bobs.
pub(crate) fn content_crop_fraction(
    owner_identity: &str,
    window_id: u32,
) -> Option<(f64, f64, f64, f64)> {
    snapshot()
        .lock_unpoisoned()
        .iter()
        .find(|window| window.owner_identity == owner_identity && window.window_id == window_id)
        .and_then(|window| window.content_crop)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PointerTargetSnapshot {
    pub(crate) window_id: u32,
    pub(crate) frame: crate::platform::cg::WindowFrame,
    pub(crate) owner_identity: String,
    pub(crate) root_hwnds: Vec<isize>,
}

/// On-screen remote compositor targets consumed by the Windows telepointer
/// sender (~9Hz). Each snapshot carries the content frame plus every top-level
/// HWND that can own a point over that surface (surface and sibling overlays).
pub(crate) fn open_content_frames() -> Vec<PointerTargetSnapshot> {
    let handle = compositor_handle();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    if handle
        .tx
        .try_send(Command::SnapshotContentFrames { reply: tx })
        .is_err()
    {
        return Vec::new();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
    loop {
        match rx.try_recv() {
            Ok(frames) => return frames,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(_) => return Vec::new(),
        }
    }
}

enum Command {
    Create {
        key: WindowKey,
        title: String,
        /// HWND of the WebviewWindow's native window; the video child is
        /// created as a child of it, below the header strip. Stored as a raw
        /// integer so the command channel stays `Send` (HWND is pointer-sized
        /// but not `Send` in the windows crate).
        parent_hwnd: usize,
        source_kind: SharedSourceKind,
        share_instance_id: Option<String>,
        /// The publisher's canonical (source) resolution; the video child +
        /// window are sized to this instead of the first decoded frame (macOS
        /// parity — see `RemoteWindow::canonical_pixel_size`).
        canonical_source_size: Option<(u32, u32)>,
    },
    UpdateMetadata {
        key: WindowKey,
        title: String,
        source_kind: SharedSourceKind,
        share_instance_id: Option<String>,
    },
    UpdateCanonicalSourceSize {
        key: WindowKey,
        size: (u32, u32),
    },
    SetHidden {
        key: WindowKey,
        hidden: bool,
    },
    Activate {
        key: WindowKey,
    },
    /// A frame arrived at a size different from the webview window: resize the
    /// WebviewWindow so the video content area matches the frame. The webview
    /// (header) stays on top; only the window's outer size changes.
    ResizeWebview {
        key: WindowKey,
        width: u32,
        height: u32,
    },
    /// First frame arrived for a key: show the (hidden) surface window on the
    /// main thread after the resize that precedes it in the FIFO queue.
    RevealWebview {
        key: WindowKey,
    },
    /// The WebviewWindow was resized (user drag-resize of the native edges):
    /// reposition the video child to fill the content area below the header.
    RepositionVideo {
        key: WindowKey,
        width: u32,
        height: u32,
    },
    /// Reposition the pointer overlay to the video child's CURRENT screen
    /// rect (physical px). Sent on window move (the video child moves but
    /// does not resize) and after overlay creation; `RepositionVideo`/`Create`
    /// also sync internally. The optional reply reports whether the compositor
    /// window exists (FIFO-after-Create makes it authoritative) so the overlay
    /// creator can detect a window removed mid-build without a snapshot race.
    SyncPointerOverlay {
        key: WindowKey,
        reply: Option<tokio::sync::oneshot::Sender<bool>>,
    },
    /// Snapshot open remote pointer targets with content geometry, owner, and
    /// target-owned top-level HWNDs for one-surface cursor hit selection.
    SnapshotContentFrames {
        reply: tokio::sync::oneshot::Sender<Vec<PointerTargetSnapshot>>,
    },
    Remove {
        key: WindowKey,
    },
    RemoveAllFor {
        owner_identity: String,
    },
    RemoveAll,
    Shutdown,
}

struct StoredI420Frame {
    y: Vec<u8>,
    y_stride: usize,
    u: Vec<u8>,
    u_stride: usize,
    v: Vec<u8>,
    v_stride: usize,
    width: u32,
    height: u32,
}

/// One latest-frame mailbox per remote window, plus a deduplicated FIFO of
/// ready keys for round-robin fairness.
///
/// Replaces the old `Command::Frame` FIFO admission: decoded frames are no
/// longer carried on the ordered compositor control channel, so a burst of
/// frames can never fill that channel and block a producer (or a Tauri
/// move/resize callback) behind seconds of video work. Publication replaces
/// the previous pending frame in place (O(1), memory bounded to one frame per
/// open window); `take_next` pops keys in arrival order so windows are served
/// fairly, and `remove`/`clear` drop pending frames before teardown so a stale
/// frame can never outlive its window.
pub(crate) struct FrameMailbox {
    inner: Mutex<FrameMailboxState>,
}

#[derive(Default)]
struct FrameMailboxState {
    latest: HashMap<WindowKey, StoredI420Frame>,
    ready: std::collections::VecDeque<WindowKey>,
}

impl FrameMailbox {
    fn new() -> Self {
        Self {
            inner: Mutex::new(FrameMailboxState::default()),
        }
    }

    /// Publish the latest decoded frame for `key`, replacing any stale pending
    /// frame. Never blocks and never fails: a producer that falls behind only
    /// ever holds its own newest frame.
    fn publish(&self, key: WindowKey, frame: StoredI420Frame) {
        let mut state = self.inner.lock_unpoisoned();
        if !state.latest.contains_key(&key) {
            state.ready.push_back(key.clone());
        }
        state.latest.insert(key, frame);
    }

    /// Take the next ready key's latest frame (round-robin), or `None` when
    /// empty. Keys that were removed since being enqueued are skipped.
    fn take_next(&self) -> Option<(WindowKey, StoredI420Frame)> {
        let mut state = self.inner.lock_unpoisoned();
        while let Some(key) = state.ready.pop_front() {
            if let Some(frame) = state.latest.remove(&key) {
                return Some((key, frame));
            }
        }
        None
    }

    fn remove(&self, key: &WindowKey) {
        let mut state = self.inner.lock_unpoisoned();
        state.latest.remove(key);
        state.ready.retain(|ready| ready != key);
    }

    fn clear(&self) {
        let mut state = self.inner.lock_unpoisoned();
        state.latest.clear();
        state.ready.clear();
    }

    #[cfg(test)]
    fn pending_keys(&self) -> Vec<WindowKey> {
        let state = self.inner.lock_unpoisoned();
        state.ready.iter().cloned().collect()
    }
}

struct RemoteWindow {
    key: WindowKey,
    /// Child window hosting the swap chain, positioned below the header.
    video_hwnd: HWND,
    swap_chain: IDXGISwapChain1,
    /// Video content size (the child window's client size, excluding the
    /// header strip).
    back_buffer_size: (u32, u32),
    /// Size of the last frame the SENDER published. Resize of the webview
    /// window (and the swap chain) happens ONLY when this changes — a real
    /// sender republish. A user drag-resize changes the child window size
    /// (and thus `back_buffer_size`) but must never bounce the webview back
    /// to the source dimensions.
    published_frame_size: (u32, u32),
    /// The publisher's canonical (source) resolution, from the track's
    /// `dimension()`. The receiver sizes its window to THIS — not to the
    /// first decoded frame — so a low simulcast layer's first frame doesn't
    /// pin the whole remote window at reduced resolution (macOS parity: the
    /// macOS compositor sizes the panel to `canonical_source_size`, giving
    /// the high layer's full-res frames the space to be shown crisp instead
    /// of perpetually downscaled). `None` falls back to the first decoded
    /// frame's size.
    canonical_pixel_size: Option<(u32, u32)>,
    /// CPU-writable dynamic BGRA texture uploaded from the CPU-converted
    /// frame, same size as the back buffer.
    texture: Option<(ID3D11Texture2D, ID3D11Resource)>,
    /// Presented letterbox-crop rect in decoded-frame coordinates
    /// (off_x, off_y, w, h); None = present the full frame. Updated only
    /// after the content rect has been stable for CROP_SETTLE_DWELL, so a
    /// sender drag does not resize the window every frame. The sender
    /// letterboxes while a resize is in progress and re-anchors the
    /// published size once it settles; this crop removes the interim bars.
    content_rect: Option<(u32, u32, u32, u32)>,
    /// (pending rect, first-seen instant) while a crop change is debouncing.
    crop_pending: Option<(Option<(u32, u32, u32, u32)>, std::time::Instant)>,
    /// Latest received frame, kept across republish/unsubscribe so the
    /// window freezes on the last presented content instead of going blank.
    latest_frame: Option<StoredI420Frame>,
    /// Keep the sibling overlays hidden until the first frame-driven geometry
    /// update has positioned the video child at its real size. The surface is
    /// intentionally revealed earlier with a placeholder, but exposing the
    /// control overlay before this barrier makes its resize handles—and the
    /// normalized input coordinates derived from its HWND—stale until the
    /// user's first manual resize.
    overlay_geometry_ready: bool,
    hidden: bool,
    title: String,
    source_kind: SharedSourceKind,
    share_instance_id: Option<String>,
}

struct Compositor {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    windows: HashMap<WindowKey, Box<RemoteWindow>>,
}

impl Compositor {
    fn create_window(
        &mut self,
        key: &WindowKey,
        title: &str,
        parent_hwnd: usize,
        source_kind: SharedSourceKind,
        share_instance_id: Option<String>,
        canonical_source_size: Option<(u32, u32)>,
    ) {
        let parent_hwnd = HWND(parent_hwnd as *mut core::ffi::c_void);
        if self.windows.contains_key(key) {
            return;
        }
        let Some(video_hwnd) = create_video_child_hwnd(parent_hwnd) else {
            log::error!("windows compositor: failed to create video child for {key:?}");
            return;
        };
        let swap_chain =
            match create_swap_chain_for_hwnd(&self.device, video_hwnd, DEFAULT_WINDOW_SIZE) {
                Ok(swap_chain) => swap_chain,
                Err(error) => {
                    log::error!(
                        "windows compositor: failed to create swap chain for {key:?}: {error}"
                    );
                    let _ = unsafe { DestroyWindow(video_hwnd) };
                    return;
                }
            };
        let window = RemoteWindow {
            key: key.clone(),
            video_hwnd,
            swap_chain,
            back_buffer_size: DEFAULT_WINDOW_SIZE,
            published_frame_size: DEFAULT_WINDOW_SIZE,
            canonical_pixel_size: canonical_source_size,
            texture: None,
            content_rect: None,
            crop_pending: None,
            latest_frame: None,
            overlay_geometry_ready: false,
            hidden: false,
            title: title.to_string(),
            source_kind,
            share_instance_id,
        };
        let window = Box::new(window);
        // Reveal-early companion: tint the still-black back buffer a neutral
        // gray so the window, revealed as soon as the video child is attached
        // (Command::Create), reads as "connecting" instead of a hollow black
        // void while the first decoded frame is in flight.
        paint_placeholder(&self.device, &self.context, &window.swap_chain);
        // The video child's wndproc reads this pointer; the Box lives in
        // `self.windows` (stable address) and is freed only after
        // DestroyWindow returns.
        unsafe {
            SetWindowLongPtrW(
                video_hwnd,
                GWLP_USERDATA,
                &*window as *const RemoteWindow as isize,
            );
            let _ = ShowWindow(video_hwnd, SW_SHOWNOACTIVATE);
        }
        self.windows.insert(key.clone(), window);
        log::info!(
            "windows compositor: created remote window for {key:?} ({title}) as '{}'",
            remote_window_label(key)
        );
        // The pointer overlay (created alongside the surface webview) must
        // track the video content area exactly, in every scaling regime.
        self.sync_pointer_overlay(key);
        self.publish_snapshot();
    }

    fn update_canonical_source_size(&mut self, key: &WindowKey, size: (u32, u32)) {
        let Some(window) = self.windows.get_mut(key) else {
            return;
        };
        if size.0 == 0 || size.1 == 0 || window.canonical_pixel_size == Some(size) {
            return;
        }
        window.canonical_pixel_size = Some(size);
        window_geometry().lock_unpoisoned().insert(key.clone(), size);
        let _ = compositor_handle().tx.try_send(Command::ResizeWebview {
            key: key.clone(),
            width: size.0,
            height: size.1,
        });
    }

    fn update_metadata(
        &mut self,
        key: &WindowKey,
        title: String,
        source_kind: SharedSourceKind,
        share_instance_id: Option<String>,
    ) {
        let Some(window) = self.windows.get_mut(key) else {
            return;
        };
        window.title = title;
        window.source_kind = source_kind;
        window.share_instance_id = share_instance_id;
        self.publish_snapshot();
    }

    /// Returns true when a device-removal error was encountered.
    fn render_frame(&mut self, key: &WindowKey, frame: StoredI420Frame) {
        let Some(window) = self.windows.get_mut(key) else {
            window_geometry()
                .lock_unpoisoned()
                .insert(key.clone(), (frame.width, frame.height));
            return;
        };
        // Petal View is a live ROI: its native capture dimensions change when
        // the sharer resizes the selector, without a track republish. Ordinary
        // window shares can also resize in place when LiveKit does not emit a
        // replacement TrackSubscribed event. Keep same-aspect low simulcast
        // frames on the published canonical size, but treat a real aspect-ratio
        // change as a new source geometry.
        let region_source_size_changed = window.source_kind == SharedSourceKind::DisplayRegion
            && region_frame_is_new_source_size(
                window.canonical_pixel_size,
                (frame.width, frame.height),
            );
        let source_resize = region_source_size_changed
            || decoded_frame_has_source_aspect_change(
                window.canonical_pixel_size,
                (frame.width, frame.height),
            );
        if source_resize {
            window.canonical_pixel_size = Some((frame.width, frame.height));
        }
        let geometry_size = window
            .canonical_pixel_size
            .unwrap_or((frame.width, frame.height));
        window_geometry()
            .lock_unpoisoned()
            .insert(key.clone(), geometry_size);
        let Some(window) = self.windows.get_mut(key) else {
            return;
        };
        if source_resize {
            // Preserve the source aspect ratio when the sender really resized.
            // Same-aspect simulcast layer switches do not enter this branch.
            let key = window.key.clone();
            let _ = compositor_handle().tx.try_send(Command::ResizeWebview {
                key,
                width: frame.width,
                height: frame.height,
            });
        }
        // Skip presenting while the remote window is hidden (compositor hide)
        // or otherwise not visible: the frame is still stored so the window
        // resumes instantly on restore (latest-frame freeze), but no CPU/GPU
        // work is spent while nobody can see it.
        let was_hidden = window.hidden || !window_on_screen(window.video_hwnd);
        let first_frame = window.latest_frame.is_none();
        let first_frame_visible = first_frame && !window.hidden;
        window.latest_frame = Some(frame);
        let mut overlay_ready_now = false;
        if first_frame && !window.hidden {
            // Size authority for the first frame: size to the publisher's
            // CANONICAL resolution when known (macOS parity) so the high
            // simulated layer's full-res frames are shown at full resolution
            // instead of being locked to a low layer's first-frame size and
            // perpetually downscaled (the fuzzy-video complaint). A low
            // layer's first frame briefly fills the larger window via DXGI
            // stretch until the high layer catches up. The window itself was
            // already revealed at attachment (Command::Create), so this
            // re-asserts the canonical size and (idempotently) re-shows it
            // (commands are FIFO: resize lands before reveal).
            let (width, height) = window.canonical_pixel_size.unwrap_or_else(|| {
                window
                    .latest_frame
                    .as_ref()
                    .map(|f| (f.width, f.height))
                    .unwrap_or((0, 0))
            });
            let key = window.key.clone();
            let _ = compositor_handle().tx.try_send(Command::ResizeWebview {
                key: key.clone(),
                width,
                height,
            });
            let _ = compositor_handle()
                .tx
                .try_send(Command::RevealWebview { key: key.clone() });

            // If the initial size was already applied before the first
            // frame arrived, no WM_SIZE transition may follow this frame.
            // The automatic resize may be capped to the receiver work area,
            // so readiness means "not the pre-frame default" rather than an
            // exact match with the publisher's source dimensions.
            let mut rect = RECT::default();
            let child_width = (unsafe { GetClientRect(window.video_hwnd, &mut rect) })
                .ok()
                .map(|_| (rect.right - rect.left).max(0) as u32);
            let child_height = child_width.map(|_| (rect.bottom - rect.top).max(0) as u32);
            let child_size = child_width.zip(child_height);
            let source_is_default = (width, height) == DEFAULT_WINDOW_SIZE;
            let child_matches = child_size == Some((width, height));
            let child_is_post_resize =
                child_size.is_some_and(|size| size != DEFAULT_WINDOW_SIZE && !source_is_default);
            if child_matches || child_is_post_resize {
                window.overlay_geometry_ready = true;
                overlay_ready_now = true;
            }
        }
        if overlay_ready_now {
            self.sync_pointer_overlay(key);
        }
        if was_hidden && !first_frame_visible {
            return;
        }
        let device_lost = {
            let window = self.windows.get_mut(key).expect("window present");
            let Some(frame) = window.latest_frame.take() else {
                return;
            };
            let device_lost = present_frame(&self.device, &self.context, window, &frame);
            window.latest_frame = Some(frame);
            device_lost
        };
        if device_lost {
            // Device loss during present: recreate everything once, then
            // re-present the stored frame (WARP/VM hosts, GPU resets).
            self.recover_from_device_loss();
            let Some(window) = self.windows.get_mut(key) else {
                return;
            };
            let Some(frame) = window.latest_frame.take() else {
                return;
            };
            let _ = present_frame(&self.device, &self.context, window, &frame);
            window.latest_frame = Some(frame);
        }
    }

    /// Recreate the device and every window's swap chain + texture after
    /// `DXGI_ERROR_DEVICE_REMOVED`/`RESET`; stored frames survive and are
    /// re-presented by the caller.
    fn recover_from_device_loss(&mut self) {
        log::warn!("windows compositor: recreating device after DXGI device loss");
        match create_d3d_device() {
            Ok((device, context)) => {
                self.device = device;
                self.context = context;
                let keys: Vec<WindowKey> = self.windows.keys().cloned().collect();
                for key in keys {
                    let Some(window) = self.windows.get_mut(&key) else {
                        continue;
                    };
                    let size = window.back_buffer_size;
                    let video_hwnd = window.video_hwnd;
                    match create_swap_chain_for_hwnd(&self.device, video_hwnd, size) {
                        Ok(swap_chain) => {
                            window.swap_chain = swap_chain;
                            recreate_texture_for_window(&self.device, window);
                        }
                        Err(error) => {
                            log::error!(
                                "windows compositor: swap chain recreation failed for {key:?}: {error}"
                            );
                        }
                    }
                }
            }
            Err(error) => log::error!("windows compositor: device recreation failed: {error}"),
        }
    }

    fn set_hidden(&mut self, key: &WindowKey, hidden: bool) {
        let Some(window) = self.windows.get_mut(key) else {
            return;
        };
        window.hidden = hidden;
        let _ = unsafe {
            SetWindowPos(
                window.video_hwnd,
                None,
                0,
                0,
                0,
                0,
                if hidden {
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_HIDEWINDOW
                } else {
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_SHOWWINDOW
                },
            )
        };
        // Hide/unhide the pointer overlay in lockstep so cursor dots never
        // render over hidden content (or vanish while the window is shown).
        self.sync_pointer_overlay_hidden(hidden, key);
        self.publish_snapshot();
    }

    fn activate(&mut self, key: &WindowKey) {
        let Some(window) = self.windows.get_mut(key) else {
            return;
        };
        window.hidden = false;
        let _ = unsafe {
            SetWindowPos(
                window.video_hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )
        };
        self.sync_pointer_overlay_hidden(false, key);
        self.publish_snapshot();
    }

    /// The WebviewWindow was resized: keep the video child filling the content
    /// area below the header strip.
    fn reposition_video(&mut self, key: &WindowKey, width: u32, height: u32) {
        let Some(window) = self.windows.get_mut(key) else {
            return;
        };
        let content_height = height.saturating_sub(HEADER_HEIGHT as u32);
        let _ = unsafe {
            SetWindowPos(
                window.video_hwnd,
                None,
                0,
                HEADER_HEIGHT,
                width as i32,
                content_height as i32,
                SWP_NOZORDER,
            )
        };
        // A post-frame resize is the geometry barrier. An earlier default-size
        // WM_SIZE can arrive after the first frame due to the main-thread /
        // compositor ordering; it must not expose stale overlays before the
        // automatic source resize (possibly work-area-capped) catches up.
        // Once ready, ordinary user free-scaling remains valid.
        let source_size = window.canonical_pixel_size.or_else(|| {
            window
                .latest_frame
                .as_ref()
                .map(|frame| (frame.width, frame.height))
        });
        let is_default_pre_frame_resize = (width, content_height) == DEFAULT_WINDOW_SIZE
            && source_size.is_some_and(|size| size != DEFAULT_WINDOW_SIZE);
        if !window.overlay_geometry_ready
            && window.latest_frame.is_some()
            && !is_default_pre_frame_resize
        {
            window.overlay_geometry_ready = true;
        }
        // The video child moved/resized: keep the pointer overlay glued to
        // it (Windows free scaling — never assume a source aspect lock).
        self.sync_pointer_overlay(key);
        // WM_SIZE on the video child resizes the swap chain + texture.
    }

    /// Reconcile a dirty key's geometry from the CURRENT parent client rect,
    /// so a dropped/coalesced move/resize command can never leave the video
    /// child or overlays stale (a later event is never required).
    fn reconcile_dirty_geometry(&mut self, key: &WindowKey) {
        let Some(window) = self.windows.get(key) else {
            return;
        };
        let Ok(parent) = (unsafe { GetParent(window.video_hwnd) }) else {
            return;
        };
        if parent.0.is_null() {
            return;
        }
        let mut rect = RECT::default();
        if (unsafe { GetClientRect(parent, &mut rect) }).is_err() {
            return;
        }
        self.reposition_video(
            key,
            (rect.right - rect.left).max(0) as u32,
            (rect.bottom - rect.top).max(0) as u32,
        );
    }

    /// Keep the pointer overlay webview exactly over this window's video
    /// content area. The video child (positioned at (0, HEADER_HEIGHT) within
    /// the surface window) IS the content area, so its current screen rect is
    /// the overlay's target geometry. Windows free scaling: the rect is
    /// physical px, so we set PHYSICAL position/size directly — no DPI
    /// conversion, hence correct on mixed-DPI setups regardless of which
    /// monitor the overlay happened to be born on. Cheap; called from the
    /// compositor thread on create/resize/move and after overlay creation.
    fn sync_pointer_overlay(&self, key: &WindowKey) {
        let Some(window) = self.windows.get(key) else {
            return;
        };
        let mut rect = RECT::default();
        if (unsafe { GetWindowRect(window.video_hwnd, &mut rect) }).is_err() {
            return;
        };
        let Ok(surface_hwnd) = (unsafe { GetParent(window.video_hwnd) }) else {
            return;
        };
        if surface_hwnd.0.is_null() {
            return;
        }
        // Synchronous with the video child: SetWindowPos each overlay's native
        // HWND directly on the compositor thread — the SAME thread that sizes
        // the child — so the overlays never lag it. Pin them into the z-order
        // immediately above the remote window (never HWND_TOPMOST): occluders
        // of the window occlude the overlays too, so dots never float over a
        // covering window.
        let above = crate::platform::windows::window_above_in_z_order(surface_hwnd);
        let width = (rect.right - rect.left) as i32;
        let height = (rect.bottom - rect.top) as i32;
        let show_overlays = window.overlay_geometry_ready && !window.hidden;
        let visibility = if show_overlays {
            SWP_SHOWWINDOW
        } else {
            SWP_HIDEWINDOW
        };
        let pointer = overlay_hwnds().lock_unpoisoned().get(key).copied();
        let control = control_overlay_hwnds().lock_unpoisoned().get(key).copied();
        // The pointer overlay must be above the full-size input overlay. The
        // latter is intentionally cursor-interactive, but its transparent
        // WebView2 surface can otherwise occlude the tagged cursor entirely.
        for overlay_hwnd in [control, pointer].into_iter().flatten() {
            let result = unsafe {
                SetWindowPos(
                    HWND(overlay_hwnd as *mut core::ffi::c_void),
                    Some(above),
                    rect.left,
                    rect.top,
                    width,
                    height,
                    visibility | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                )
            };
            if result.is_err() {
                log::warn!("windows compositor: overlay SetWindowPos failed for {key:?}");
                return;
            }
        }
    }

    /// Hide/show the pointer overlay in lockstep with the surface window
    /// (retire paths hide the surface webview + video child; activate and
    /// unhide must restore the overlay too, or cursor dots render over
    /// hidden content / vanish while the window is on screen).
    fn sync_pointer_overlay_hidden(&self, hidden: bool, key: &WindowKey) {
        let Some(app) = app_handle().lock_unpoisoned().clone() else {
            return;
        };
        let ready = self
            .windows
            .get(key)
            .map(|window| window.overlay_geometry_ready)
            .unwrap_or(false);
        for label in [pointer_overlay_label(key), control_overlay_label(key)] {
            let Some(overlay) = app.get_webview_window(&label) else {
                continue;
            };
            let _ = if hidden || !ready {
                overlay.hide()
            } else {
                overlay.show()
            };
        }
    }

    /// On-screen content frames (physical px) of every open remote window,
    /// with the shared surface's owner: (window_id, GetWindowRect(video_hwnd),
    /// owner_identity). Consumed by the telepointer sender's remote-compositor
    /// targets (~9Hz).
    fn snapshot_content_frames(&self) -> Vec<PointerTargetSnapshot> {
        let pointer_hwnds = overlay_hwnds().lock_unpoisoned();
        let control_hwnds = control_overlay_hwnds().lock_unpoisoned();
        self.windows
            .iter()
            // Hidden (retired) windows must not publish phantom pointer
            // "enters" over invisible content.
            .filter(|(_, window)| !window.hidden)
            .filter_map(|(key, window)| {
                let mut rect = RECT::default();
                if (unsafe { GetWindowRect(window.video_hwnd, &mut rect) }).is_err() {
                    return None;
                }
                let surface = unsafe { GetParent(window.video_hwnd) }.ok()?;
                if surface.0.is_null() {
                    return None;
                }
                let mut root_hwnds = vec![surface.0 as isize];
                root_hwnds.extend(pointer_hwnds.get(key).copied());
                root_hwnds.extend(control_hwnds.get(key).copied());
                Some(PointerTargetSnapshot {
                    window_id: key.1,
                    frame: crate::platform::cg::WindowFrame {
                        x: rect.left,
                        y: rect.top,
                        width: rect.right - rect.left,
                        height: rect.bottom - rect.top,
                    },
                    owner_identity: key.0.clone(),
                    root_hwnds,
                })
            })
            .collect()
    }

    fn remove(&mut self, key: &WindowKey) {
        // Drop any pending mailbox frame before native teardown so a stale
        // decoded frame can never be presented into a removed/replaced window.
        frame_mailbox().remove(key);
        if let Some(window) = self.windows.remove(key) {
            unsafe {
                let _ = DestroyWindow(window.video_hwnd);
                let _ = SetWindowLongPtrW(window.video_hwnd, GWLP_USERDATA, 0);
            }
            drop(window);
            window_geometry().lock_unpoisoned().remove(key);
            overlay_hwnds().lock_unpoisoned().remove(key);
            control_overlay_hwnds().lock_unpoisoned().remove(key);
            // Tear down the pointer + control overlays with the surface window.
            if let Some(app) = app_handle().lock_unpoisoned().clone() {
                for label in [pointer_overlay_label(key), control_overlay_label(key)] {
                    if let Some(overlay) = app.get_webview_window(&label) {
                        let _ = overlay.close();
                    }
                }
            }
            log::info!("windows compositor: removed remote window for {key:?}");
            self.publish_snapshot();
        }
    }

    fn remove_all_for(&mut self, owner_identity: &str) {
        let keys: Vec<WindowKey> = self
            .windows
            .keys()
            .filter(|(identity, _)| identity == owner_identity)
            .cloned()
            .collect();
        for key in keys {
            self.remove(&key);
        }
    }

    fn remove_all(&mut self) {
        frame_mailbox().clear();
        let keys: Vec<WindowKey> = self.windows.keys().cloned().collect();
        for key in keys {
            self.remove(&key);
        }
    }

    fn publish_snapshot(&self) {
        let summary = self
            .windows
            .values()
            .map(|window| RemoteWindowSummary {
                window_id: window.key.1,
                owner_identity: window.key.0.clone(),
                owner_display_name: window.key.0.clone(),
                source_title: window.title.clone(),
                hidden: window.hidden,
                source_kind: window.source_kind,
                share_instance_id: window.share_instance_id.clone(),
                content_crop: window.content_rect.map(|(ox, oy, cw, ch)| {
                    let (fw, fh) = window.published_frame_size;
                    (
                        ox as f64 / fw.max(1) as f64,
                        oy as f64 / fh.max(1) as f64,
                        cw as f64 / fw.max(1) as f64,
                        ch as f64 / fh.max(1) as f64,
                    )
                }),
            })
            .collect();
        *snapshot().lock_unpoisoned() = summary;
    }
}

fn create_swap_chain_for_hwnd(
    device: &ID3D11Device,
    hwnd: HWND,
    (width, height): (u32, u32),
) -> windows::core::Result<IDXGISwapChain1> {
    let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory1() }?;
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    let swap_chain = unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None) }?;
    swap_chain.cast::<IDXGISwapChain1>()
}

// ---------------------------------------------------------------------------
// Tauri command surface
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteWindowSummary {
    pub window_id: u32,
    pub owner_identity: String,
    pub owner_display_name: String,
    pub source_title: String,
    pub hidden: bool,
    pub source_kind: SharedSourceKind,
    pub share_instance_id: Option<String>,
    /// Letterbox crop as fractions of the full source frame:
    /// (ox, oy, content_w, content_h) / (full_w, full_h). The displayed child
    /// shows ONLY this (crop-removed) region, so the telepointer receiver
    /// maps full-frame normalized coords through it.
    pub content_crop: Option<(f64, f64, f64, f64)>,
}

/// Current remote-window roster, as seen by the frontend's compositor panel
/// (same wire shape as the macOS compositor).
///
/// Async for the same reason as `compositor_window_debug_stats`: a sync
/// command runs on the MAIN thread and takes `snapshot()`, whose lock is
/// compositor-thread-owned and can be held across a blocking WebView2/D3D
/// call — a main-thread wait on it ABBA-deadlocks the UI (live-verified via
/// WinDbg on the freeze). Async moves this read off the main thread.
#[tauri::command]
pub async fn compositor_list_windows() -> Vec<RemoteWindowSummary> {
    snapshot().lock_unpoisoned().clone()
}

/// Window frame for the aspect-locked resize drag, in logical pixels — the
/// same wire shape as the macOS `CompositorResizeFrame`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompositorResizeFrame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompositorResizeDirection {
    East,
    North,
    NorthEast,
    NorthWest,
    South,
    SouthEast,
    SouthWest,
    West,
}

impl CompositorResizeDirection {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "East" => Ok(Self::East),
            "North" => Ok(Self::North),
            "NorthEast" => Ok(Self::NorthEast),
            "NorthWest" => Ok(Self::NorthWest),
            "South" => Ok(Self::South),
            "SouthEast" => Ok(Self::SouthEast),
            "SouthWest" => Ok(Self::SouthWest),
            "West" => Ok(Self::West),
            _ => Err(format!("invalid resize direction '{raw}'")),
        }
    }

    fn has_east(self) -> bool {
        matches!(self, Self::East | Self::NorthEast | Self::SouthEast)
    }

    fn has_west(self) -> bool {
        matches!(self, Self::West | Self::NorthWest | Self::SouthWest)
    }

    fn has_north(self) -> bool {
        matches!(self, Self::North | Self::NorthEast | Self::NorthWest)
    }

    fn has_south(self) -> bool {
        matches!(self, Self::South | Self::SouthEast | Self::SouthWest)
    }
}

/// Minimum video-content size a remote window may shrink to (macOS
/// `MIN_RESIZE_CONTENT_WIDTH/HEIGHT`).
const MIN_RESIZE_CONTENT_WIDTH: f64 = 300.0;
const MIN_RESIZE_CONTENT_HEIGHT: f64 = 150.0;

/// Source content aspect ratio for the aspect-locked resize, from the last
/// published frame's content geometry (the video area, excluding the header).
fn source_aspect_for_resize(key: &WindowKey, fallback_width: f64, fallback_height: f64) -> f64 {
    let fallback_content_h = (fallback_height - HEADER_HEIGHT as f64).max(1.0);
    let (source_w, source_h) =
        content_geometry_for(key).unwrap_or((fallback_width as u32, fallback_content_h as u32));
    (source_w as f64 / (source_h as f64).max(1.0)).max(0.01)
}

/// Aspect-locked frame math — a direct port of the macOS
/// `resized_frame_from_drag`: width drives, height follows the source aspect,
/// and north/west drags adjust the origin to keep the opposite edge pinned.
fn resized_frame_from_drag(
    direction: CompositorResizeDirection,
    aspect: f64,
    start: CompositorResizeFrame,
    delta_x: f64,
    delta_y: f64,
) -> CompositorResizeFrame {
    let start_content_h = (start.height - HEADER_HEIGHT as f64).max(1.0);
    let horizontal_width = if direction.has_west() {
        start.width - delta_x
    } else if direction.has_east() {
        start.width + delta_x
    } else {
        start.width
    };
    let vertical_content_h = if direction.has_north() {
        start_content_h - delta_y
    } else if direction.has_south() {
        start_content_h + delta_y
    } else {
        start_content_h
    };
    let vertical_width = vertical_content_h * aspect;
    let mut width = match (
        direction.has_east() || direction.has_west(),
        direction.has_north() || direction.has_south(),
    ) {
        (true, true) => {
            if (horizontal_width - start.width).abs() >= (vertical_width - start.width).abs() {
                horizontal_width
            } else {
                vertical_width
            }
        }
        (true, false) => horizontal_width,
        (false, true) => vertical_width,
        (false, false) => start.width,
    };
    width = width.max(MIN_RESIZE_CONTENT_WIDTH.max(MIN_RESIZE_CONTENT_HEIGHT * aspect));
    let height = HEADER_HEIGHT as f64 + (width / aspect).max(MIN_RESIZE_CONTENT_HEIGHT);
    let x = if direction.has_west() {
        start.x + start.width - width
    } else {
        start.x
    };
    let y = if direction.has_north() {
        start.y + start.height - height
    } else {
        start.y
    };
    CompositorResizeFrame {
        x,
        y,
        width,
        height,
    }
}

/// Retire (hide) one remote window. `owner_identity` disambiguates when the
/// same window id is used by two peers (reuse after leave).
#[tauri::command]
pub fn compositor_hide_window(
    app: tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) {
    let Some(key) = resolve_key(window_id, owner_identity.as_deref()) else {
        return;
    };
    if let Some(window) = app.get_webview_window(&remote_window_label(&key)) {
        let _ = window.hide();
    }
    send_command_sync(Command::SetHidden { key, hidden: true });
}

/// Reveal + raise one remote window.
#[tauri::command]
pub fn compositor_activate_window(
    app: tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) {
    let Some(key) = resolve_key(window_id, owner_identity.as_deref()) else {
        return;
    };
    if let Some(window) = app.get_webview_window(&remote_window_label(&key)) {
        let _ = window.show();
        let _ = window.set_focus();
    }
    send_command_sync(Command::Activate { key });
}

/// #875: raise ALL of `owner_identity`'s remote windows -- native window
/// parity with the macOS `compositor::compositor_raise_participant_windows`
/// -- restoring any the viewer had hidden, without stealing focus.
///
/// Unlike the macOS compositor (`compositor.rs`'s `retired` bucket, a
/// separate pool a manually-hidden window moves into), a hidden Windows
/// remote window stays tracked right where it is: `set_hidden` above only
/// flips `window.hidden` on the SAME `self.windows` entry `snapshot()`
/// mirrors. A window whose share genuinely ended is removed from that map
/// entirely (`Command::Remove`/`RemoveAllFor`). So every entry `snapshot()`
/// returns for this owner is, by construction, still backed by a live
/// share -- there is no retired-without-publication phantom case to guard
/// against the way there is on macOS (see that command's doc comment).
///
/// The Windows compositor does not store `petalWindowZOrder`'s per-window
/// rank (macOS-only field, `compositor.rs`'s `CompositorWindow::z_rank`) --
/// windows raise in a stable ascending-window-id order here rather than the
/// sharer's real z-order. Full rank parity is a follow-up; #875 only
/// requires this command exist and behave reasonably on Windows rather than
/// being macOS-gated out of the registry.
#[tauri::command]
pub fn compositor_raise_participant_windows(app: tauri::AppHandle, owner_identity: String) {
    let mut keys: Vec<WindowKey> = snapshot()
        .lock_unpoisoned()
        .iter()
        .filter(|window| window.owner_identity == owner_identity)
        .map(|window| (window.owner_identity.clone(), window.window_id))
        .collect();
    if keys.is_empty() {
        log::info!(
            "windows compositor: raise-participant-windows requested for '{owner_identity}' with no windows"
        );
        return;
    }
    // Deterministic stand-in for the sharer's real z-order -- see the doc
    // comment above.
    keys.sort_by_key(|key| key.1);

    for key in &keys {
        let Some(window) = app.get_webview_window(&remote_window_label(key)) else {
            log::warn!(
                "windows compositor: raise-participant-windows missing window {} for '{owner_identity}'",
                key.1
            );
            continue;
        };
        // Restore (video child + pointer overlay) if the user had hidden it
        // -- the same recovery `compositor_activate_window` performs, minus
        // the focus steal below.
        send_command_sync(Command::Activate { key: key.clone() });
        let _ = window.show();
        // Raise-only: bring the webview's own HWND to the top of the
        // Z-order WITHOUT activating/focusing it (`SWP_NOACTIVATE`) --
        // macOS parity with `platform::appkit::raise_panel_only`: no
        // `makeKeyWindow`/focus steal (#356). The gallery window the user
        // clicked stays focused.
        if let Ok(hwnd) = window.hwnd() {
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
            };
        } else {
            log::warn!(
                "windows compositor: raise-participant-windows no hwnd for window {} ('{owner_identity}')",
                key.1
            );
        }
    }

    log::info!(
        "windows compositor: raised {} window(s) for participant '{owner_identity}' (no focus steal)",
        keys.len()
    );
}

fn resolve_key(window_id: u32, owner_identity: Option<&str>) -> Option<WindowKey> {
    match owner_identity {
        Some(identity) => Some((identity.to_string(), window_id)),
        None => snapshot()
            .lock_unpoisoned()
            .iter()
            .find(|window| window.window_id == window_id)
            .map(|window| (window.owner_identity.clone(), window_id)),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteControlTargetMetadata {
    pub(crate) owner_identity: String,
    pub(crate) target_kind: crate::remote_control_core::RemoteControlTargetKind,
    pub(crate) share_instance_id: String,
}

pub(crate) fn remote_control_window_exists(
    window_id: u32,
    owner_identity: Option<&str>,
) -> bool {
    snapshot().lock_unpoisoned().iter().any(|window| {
        window.window_id == window_id
            && owner_identity.is_none_or(|owner| owner == window.owner_identity)
            && !window.hidden
    })
}

pub(crate) fn remote_control_target_metadata(
    window_id: u32,
    owner_identity: Option<&str>,
) -> Option<RemoteControlTargetMetadata> {
    let windows = snapshot().lock_unpoisoned();
    let window = windows.iter().find(|window| {
        window.window_id == window_id
            && owner_identity.is_none_or(|owner| owner == window.owner_identity)
            && !window.hidden
    })?;
    Some(RemoteControlTargetMetadata {
        owner_identity: window.owner_identity.clone(),
        target_kind: match window.source_kind {
            SharedSourceKind::Window => crate::remote_control_core::RemoteControlTargetKind::Window,
            SharedSourceKind::Display | SharedSourceKind::DisplayRegion => {
                crate::remote_control_core::RemoteControlTargetKind::Display
            }
        },
        share_instance_id: window.share_instance_id.clone()?,
    })
}

/// Start a native window drag from the header strip. The surface route's
/// header mousedown calls this (macOS: same `start_dragging`).
#[tauri::command]
pub fn compositor_start_drag(
    app: tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) {
    let Some(key) = resolve_key(window_id, owner_identity.as_deref()) else {
        return;
    };
    let Some(window) = app.get_webview_window(&remote_window_label(&key)) else {
        return;
    };
    if let Err(error) = window.start_dragging() {
        log::warn!("windows compositor: start_dragging failed for window {window_id}: {error}");
    }
}

/// Resize the webview window so the video content area matches the last
/// published frame size (macOS `compositor_fit_to_source`).
#[tauri::command]
pub fn compositor_fit_to_source(
    app: tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) {
    let Some(key) = resolve_key(window_id, owner_identity.as_deref()) else {
        return;
    };
    let Some(window) = app.get_webview_window(&remote_window_label(&key)) else {
        return;
    };
    let Some((content_w, content_h)) = content_geometry_for(&key) else {
        return;
    };
    let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize::new(
        content_w as f64,
        content_h as f64 + HEADER_HEIGHT as f64,
    )));
}

/// Begin an aspect-locked resize drag: return the window's current frame in
/// logical pixels (the drag anchor). Mirrors the macOS `compositor_begin_resize`.
#[tauri::command]
pub fn compositor_begin_resize(
    app: tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) -> Result<CompositorResizeFrame, String> {
    let key = resolve_key(window_id, owner_identity.as_deref())
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let window = app
        .get_webview_window(&remote_window_label(&key))
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let scale = window.scale_factor().unwrap_or(1.0).max(1.0);
    let position = window
        .outer_position()
        .map_err(|e| format!("read remote window position: {e}"))?;
    let size = window
        .outer_size()
        .map_err(|e| format!("read remote window size: {e}"))?;
    Ok(CompositorResizeFrame {
        x: position.x as f64 / scale,
        y: position.y as f64 / scale,
        width: size.width as f64 / scale,
        height: size.height as f64 / scale,
    })
}

/// Apply one step of an aspect-locked resize drag: recompute the frame from
/// the drag delta (width drives, height follows the source aspect), then
/// size + reposition the webview window. Mirrors the macOS
/// `compositor_resize_window`; the video child follows via the window's
/// Resized handler.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn compositor_resize_window(
    app: tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
    direction: String,
    start_x: f64,
    start_y: f64,
    start_width: f64,
    start_height: f64,
    delta_x: f64,
    delta_y: f64,
    _finalize: Option<bool>,
) -> Result<(), String> {
    let direction = CompositorResizeDirection::parse(&direction)?;
    let key = resolve_key(window_id, owner_identity.as_deref())
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let window = app
        .get_webview_window(&remote_window_label(&key))
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let aspect = source_aspect_for_resize(&key, start_width, start_height);
    let frame = resized_frame_from_drag(
        direction,
        aspect,
        CompositorResizeFrame {
            x: start_x,
            y: start_y,
            width: start_width,
            height: start_height,
        },
        delta_x,
        delta_y,
    );
    window
        .set_size(tauri::Size::Logical(tauri::LogicalSize::new(
            frame.width,
            frame.height,
        )))
        .map_err(|e| format!("resize remote window: {e}"))?;
    window
        .set_position(tauri::Position::Logical(tauri::LogicalPosition::new(
            frame.x, frame.y,
        )))
        .map_err(|e| format!("position remote window after resize: {e}"))?;
    Ok(())
}

/// Debug panel is a macOS-only webview surface; Windows has no equivalent
/// panel, so this is a logged no-op (keeps the header's Debug button working).
#[tauri::command]
pub fn compositor_toggle_debug_panel(
    _app: tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) {
    log::info!(
        "windows compositor: debug panel not implemented on Windows (window {window_id}, owner {owner_identity:?})"
    );
}

/// Activate/deactivate Draw mode in the remote window's control overlay.
/// The Svelte route owns the Draw/control mutual exclusion; this command only
/// resolves the authenticated compositor surface and forwards the state.
#[tauri::command]
pub fn compositor_set_draw_active(
    app: tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
    active: bool,
) -> Result<(), String> {
    let key = resolve_key(window_id, owner_identity.as_deref())
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let overlay = app
        .get_webview_window(&control_overlay_label(&key))
        .ok_or_else(|| format!("control overlay for window {window_id} is not open"))?;
    let active_json = if active { "true" } else { "false" };
    overlay
        .eval(format!("window.__petalDrawSetActive?.({active_json});"))
        .map_err(|error| format!("draw control eval failed: {error}"))?;
    send_command_sync(Command::SyncPointerOverlay { key, reply: None });
    Ok(())
}

/// Minimal per-window stats for the header's freshness tooltip; mirrors the
/// wire shape of the macOS `compositor_window_debug_stats`.
///
/// Async (not a sync command) on purpose: a sync `#[tauri::command]` runs on
/// the MAIN thread, and this command reads `snapshot()`, whose lock is owned
/// by the `petal-compositor` thread (written in `publish_snapshot`). The
/// compositor thread can hold `snapshot()` while blocked inside a WebView2/
/// D3D call that itself waits on the main thread — a sync command on main
/// would then ABBA-deadlock the whole UI (observed live: `Responding=False`,
/// WinDbg shows main blocked in `Mutex::lock_contended` inside this command).
/// Async commands run on the tokio runtime, so a blocked `snapshot()` wait
/// stalls only this stat read, never the UI thread.
#[tauri::command]
pub async fn compositor_window_debug_stats(
    app: tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) -> Option<RemoteWindowDebugStats> {
    let key = resolve_key(window_id, owner_identity.as_deref())?;
    let label = remote_window_label(&key);
    let window = app.get_webview_window(&label)?;
    let (content_w, content_h) = content_geometry_for(&key).unwrap_or(DEFAULT_WINDOW_SIZE);
    let scale = window.scale_factor().unwrap_or(1.0).max(1.0);
    // Single, short-lived snapshot read: derive both title and RC availability
    // from one lock acquisition (two acquisitions double the ABBA exposure).
    let snapshot_guard = snapshot().lock_unpoisoned();
    let summary = snapshot_guard
        .iter()
        .find(|summary| summary.window_id == window_id);
    let source_title = summary
        .map(|summary| summary.source_title.clone())
        .unwrap_or_default();
    let remote_control_available =
        summary.is_some_and(|summary| summary.share_instance_id.is_some());
    drop(snapshot_guard);
    Some(RemoteWindowDebugStats {
        window_id,
        owner_identity: key.0.clone(),
        owner_display_name: key.0,
        source_title,
        source_url: None,
        content_width: content_w,
        content_height: content_h,
        receiver_scale: scale,
        display_pixel_width: content_w,
        display_pixel_height: content_h,
        source_pixel_width: None,
        source_pixel_height: None,
        last_frame_received_ms: None,
        frames_received: 0,
        last_display_enqueued_ms: None,
        frames_display_enqueued: 0,
        remote_control_available,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteWindowDebugStats {
    pub window_id: u32,
    pub owner_identity: String,
    pub owner_display_name: String,
    pub source_title: String,
    pub source_url: Option<String>,
    pub content_width: u32,
    pub content_height: u32,
    pub receiver_scale: f64,
    pub display_pixel_width: u32,
    pub display_pixel_height: u32,
    pub source_pixel_width: Option<u32>,
    pub source_pixel_height: Option<u32>,
    pub last_frame_received_ms: Option<u64>,
    pub frames_received: u64,
    pub last_display_enqueued_ms: Option<u64>,
    pub frames_display_enqueued: u64,
    pub remote_control_available: bool,
}

fn content_geometry_for(key: &WindowKey) -> Option<(u32, u32)> {
    window_geometry().lock_unpoisoned().get(key).copied()
}

/// Shared `(width, height)` content geometry per key, updated by the
/// compositor thread as frames arrive.
fn window_geometry() -> &'static Mutex<HashMap<WindowKey, (u32, u32)>> {
    static GEOMETRY: LazyLock<Mutex<HashMap<WindowKey, (u32, u32)>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &GEOMETRY
}

/// Shared per-key "window is on screen" flag: true while the remote window is
/// visible, not minimized, and on the visible virtual desktop. The subscriber
/// feed reads this to pause decoding while the user cannot see the window
/// (saving CPU), and the compositor thread refreshes it every pump tick from
/// the authoritative Win32 window state. Absent keys default to visible so a
/// window that has not produced a frame yet still decodes.
fn window_visible_state() -> &'static Mutex<HashMap<WindowKey, bool>> {
    static VISIBLE: LazyLock<Mutex<HashMap<WindowKey, bool>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &VISIBLE
}

/// Native HWND of each pointer overlay webview (recorded on the async side
/// at creation, where `hwnd()`'s blocking RPC is safe). Lets the compositor
/// thread `SetWindowPos`/`GetWindowRect` it directly — the overlay must be
/// positioned synchronously with the video child (same thread), never via an
/// async Tauri dispatch that lags a drag by one full cycle.
fn overlay_hwnds() -> &'static Mutex<HashMap<WindowKey, isize>> {
    static OVERLAY_HWNDS: LazyLock<Mutex<HashMap<WindowKey, isize>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &OVERLAY_HWNDS
}

/// Native HWND of each control overlay webview (the input-capture sibling of
/// the pointer overlay). Recorded on the async side at creation; positioned by
/// the compositor thread together with the pointer overlay.
fn control_overlay_hwnds() -> &'static Mutex<HashMap<WindowKey, isize>> {
    static CONTROL_OVERLAY_HWNDS: LazyLock<Mutex<HashMap<WindowKey, isize>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &CONTROL_OVERLAY_HWNDS
}

/// Last z-above handle per remote window, so the periodic z re-assert is a
/// no-op while the covering-window arrangement is unchanged.
fn overlay_z_above() -> &'static Mutex<HashMap<WindowKey, isize>> {
    static OVERLAY_Z_ABOVE: LazyLock<Mutex<HashMap<WindowKey, isize>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &OVERLAY_Z_ABOVE
}

fn set_window_visible(key: &WindowKey, visible: bool) {
    window_visible_state()
        .lock_unpoisoned()
        .insert(key.clone(), visible);
}

/// Whether the remote window for `key` is currently on screen (feed-facing
/// read). Defaults to true for unknown keys.
pub(crate) fn window_is_on_screen(key: &WindowKey) -> bool {
    window_visible_state()
        .lock_unpoisoned()
        .get(key)
        .copied()
        .unwrap_or(true)
}

/// Keys whose geometry (move/resize) changed but whose command has not yet
/// been applied by the compositor. Move/resize callbacks mark the key here
/// (cheap, nonblocking) and best-effort try-send once; the compositor pump
/// drains this set each tick and re-reads current HWND geometry, so a
/// dropped/coalesced command can never leave an overlay or video child stale
/// (a later event is never required to correct it).
fn dirty_geometry_keys() -> &'static Mutex<HashSet<WindowKey>> {
    static DIRTY: LazyLock<Mutex<HashSet<WindowKey>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    &DIRTY
}

/// Nonblocking, coalescing send for move/resize overlay synchronization. A
/// Tauri window-event callback must never wait for compositor capacity (the
/// old `send_command_sync` retried up to 5,000×1ms — the observed
/// "Not responding" window). Mark the key dirty (the pump will reconcile
/// real geometry regardless), then try-send once; on a full queue the dirty
/// reconcile is the safety net and the command is simply dropped.
fn send_geometry_command(command: Command) {
    let key = match &command {
        Command::RepositionVideo { key, .. } | Command::SyncPointerOverlay { key, .. } => {
            Some(key.clone())
        }
        _ => None,
    };
    if let Some(key) = key {
        dirty_geometry_keys().lock_unpoisoned().insert(key);
    }
    let handle = compositor_handle();
    if let Err(TrySendError::Full(_)) = handle.tx.try_send(command) {
        // The dirty-key reconcile below will correct geometry next pump tick;
        // no retry, no blocking, no multi-second stall.
        log::debug!("windows compositor: geometry command coalesced (queue full)");
    }
}

/// Number of remote windows currently off-screen (minimized/hidden), for the
/// feed's health log — so a 0-fps health line reads as "windows paused", not
/// "receiver broken".
pub(crate) fn off_screen_window_count() -> usize {
    window_visible_state()
        .lock_unpoisoned()
        .values()
        .filter(|visible| !**visible)
        .count()
}

// ---------------------------------------------------------------------------
// Per-window last-frame timing (Windows no-frame watchdog, subscriber.rs)
// ---------------------------------------------------------------------------
//
// The pinned LiveKit SDK never emits `RoomEvent::TrackUnpublished` for an
// explicit stop-sharing on Windows (verified across sessions on current
// builds of both sides), so the feed has no event-driven signal to retire a
// remote window when the sharer stops — without this, the window lingers with
// its frozen frame until the sharer disconnects. The Windows feed's no-frame
// watchdog (subscriber.rs, the Windows sibling of macOS's
// `retire_no_frame_windows`) consults this registry, which plays the role of
// macOS's `ReceiveWindowState::last_frame_at`. Standalone registry like
// `window_geometry`/`window_visible_state` above: the compositor's own
// `CompositorWindow` map lives on the dedicated D3D11/Win32-message-loop
// thread and is not reachable from the async feed.

fn frame_timing_registry() -> &'static Mutex<HashMap<WindowKey, std::time::Instant>> {
    static REGISTRY: OnceLock<Mutex<HashMap<WindowKey, std::time::Instant>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that a frame was pushed for `key` (called from `push_frame`).
pub(crate) fn note_frame_arrived(key: &WindowKey) {
    frame_timing_registry()
        .lock_unpoisoned()
        .insert(key.clone(), std::time::Instant::now());
}

/// When `key` last had a frame, for the no-frame watchdog.
pub(crate) fn last_frame_at(key: &WindowKey) -> Option<std::time::Instant> {
    frame_timing_registry().lock_unpoisoned().get(key).copied()
}

/// Keys the watchdog should consider (windows that have received frames).
pub(crate) fn active_frame_keys() -> Vec<WindowKey> {
    frame_timing_registry()
        .lock_unpoisoned()
        .keys()
        .cloned()
        .collect()
}

/// Forget timing when the window is retired.
pub(crate) fn drop_frame_timing(key: &WindowKey) {
    frame_timing_registry().lock_unpoisoned().remove(key);
}

// ---------------------------------------------------------------------------
// Decode-loop cancellation (#694, the Windows sibling of #682's macOS fix)
// ---------------------------------------------------------------------------
//
// `spawn_windows_decode_loop` (subscriber.rs) was a detached `tokio::spawn`
// with nothing tying its lifetime to the window it feeds: removing (or
// republish-replacing) the compositor window for a key never stopped the
// loop, which parks forever in `stream.next()` (the underlying frame queue
// is only closed by that same task's own `Drop`, which cannot run while the
// task is parked awaiting a frame -- see #682's identical mechanics writeup
// on the macOS side). This file's window registry (`Compositor::windows`)
// lives entirely on the dedicated D3D11/Win32-message-loop thread and is not
// reachable from the async decode-loop task, so -- unlike macOS's
// `ReceiveWindowState`, which carries its own `CancellationToken` inline --
// this is a small standalone registry alongside `window_geometry`/
// `window_visible_state` above, keyed by the same `WindowKey`.
//
// `install_decode_loop_token` and the `cancel_decode_loop*` functions below
// are the ONLY places that should ever touch this map, mirroring
// subscriber.rs's `insert_window_state`/`remove_window_state` shape: every
// install cancels whatever token it displaces, and every cancellation
// removes the entry, so "no token registered for this key" and "its decode
// loop has been told to stop" stay the same fact by construction.

/// Per-window decode-loop `CancellationToken`, installed by
/// `install_decode_loop_token` and consumed by `spawn_windows_decode_loop`'s
/// `next_frame_or_cancelled` race.
fn decode_loop_tokens() -> &'static Mutex<HashMap<WindowKey, CancellationToken>> {
    static TOKENS: LazyLock<Mutex<HashMap<WindowKey, CancellationToken>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &TOKENS
}

/// Install a fresh `CancellationToken` for `key`'s decode loop, cancelling
/// whatever token was previously registered for the same key first. This is
/// what makes a republish's fresh `spawn_windows_decode_loop` call
/// automatically stop any still-running predecessor loop for the same
/// window -- the `TrackSubscribed` arm (subscriber.rs) MUST call this rather
/// than spawning the loop directly, or a republish double-spawns the same
/// way #682's macOS replacement-insert bug did before its fix.
pub(crate) fn install_decode_loop_token(key: &WindowKey) -> CancellationToken {
    let token = CancellationToken::new();
    let previous = decode_loop_tokens()
        .lock_unpoisoned()
        .insert(key.clone(), token.clone());
    if let Some(previous) = previous {
        previous.cancel();
    }
    token
}

/// Cancel and remove `key`'s decode-loop token, if any. Called by
/// `remove_window` (`TrackUnpublished`) as part of that same operation, so
/// every window-removal call cancels its decode loop "for free" by
/// construction.
fn cancel_decode_loop(key: &WindowKey) {
    if let Some(token) = decode_loop_tokens().lock_unpoisoned().remove(key) {
        token.cancel();
    }
}

/// Cancel and remove every decode-loop token owned by `owner_identity`.
/// Called by `remove_all_for` (`ParticipantDisconnected`).
fn cancel_decode_loops_for(owner_identity: &str) {
    let mut tokens = decode_loop_tokens().lock_unpoisoned();
    let keys: Vec<WindowKey> = tokens
        .keys()
        .filter(|(identity, _)| identity == owner_identity)
        .cloned()
        .collect();
    for key in keys {
        if let Some(token) = tokens.remove(&key) {
            token.cancel();
        }
    }
}

/// Cancel and remove every decode-loop token. Called by `remove_all`
/// (`Disconnected` / room leave) so no decode loop outlives a full teardown,
/// mirroring subscriber.rs's `cancel_all_window_states` (added in #682's
/// counselors-review follow-up, where the equivalent gap on the macOS side
/// was the MOST common exit path -- ordinary room leave/rejoin, not just a
/// republish).
fn cancel_all_decode_loops() {
    for (_, token) in decode_loop_tokens().lock_unpoisoned().drain() {
        token.cancel();
    }
}

/// App handle captured at first window creation; the compositor thread uses
/// it (via `run_on_main_thread`) to resize WebviewWindows when frames arrive
/// at a different size.
fn app_handle() -> &'static Mutex<Option<tauri::AppHandle>> {
    static APP: LazyLock<Mutex<Option<tauri::AppHandle>>> = LazyLock::new(|| Mutex::new(None));
    &APP
}

/// Return the current monitor's work area in logical points. The source
/// dimensions are applied through `LogicalSize`, while Tauri's monitor rect is
/// physical, so divide by the receiver scale before comparing them.
fn work_area_size_for_window(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
) -> Option<(f64, f64)> {
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let scale = monitor.scale_factor().max(1.0);
    let work_area = monitor.work_area();
    Some((
        work_area.size.width as f64 / scale,
        work_area.size.height as f64 / scale,
    ))
}

/// Fit an automatic source size inside the receiver work area while
/// preserving aspect ratio. This is intentionally only for source-driven
/// initial sizing; an explicit user "fit to source" remains an explicit
/// command and may be larger than the current monitor.
fn initial_content_size_within_work_area(
    source_width: u32,
    source_height: u32,
    work_area: Option<(f64, f64)>,
) -> (f64, f64) {
    let source_width = source_width as f64;
    let source_height = source_height as f64;
    let Some((work_width, work_height)) = work_area else {
        return (source_width.max(1.0), source_height.max(1.0));
    };
    if source_width <= 0.0 || source_height <= 0.0 || work_width <= 0.0 || work_height <= 0.0 {
        return (source_width.max(1.0), source_height.max(1.0));
    }
    let max_width = work_width * INITIAL_MAX_WORK_AREA_FRACTION;
    let max_height = (work_height * INITIAL_MAX_WORK_AREA_FRACTION - HEADER_HEIGHT as f64).max(1.0);
    let factor = (max_width / source_width)
        .min(max_height / source_height)
        .min(1.0);
    (
        (source_width * factor).round().max(1.0),
        (source_height * factor).round().max(1.0),
    )
}

/// Resize a remote window's WebviewWindow to fit `width`x`height` video
/// content (plus the header strip) on the Tauri main thread. Automatic source
/// sizing is capped to the receiver work area so a remote display cannot open
/// larger than the controller's screen.
fn resize_webview_for(key: &WindowKey, width: u32, height: u32) {
    let Some(app) = app_handle().lock_unpoisoned().clone() else {
        return;
    };
    let label = remote_window_label(key);
    let app_for_closure = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(window) = app_for_closure.get_webview_window(&label) else {
            return;
        };
        let (content_width, content_height) = initial_content_size_within_work_area(
            width,
            height,
            work_area_size_for_window(&app_for_closure, &window),
        );
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize::new(
            content_width,
            content_height + HEADER_HEIGHT as f64,
        )));
    });
}

/// Reveal a remote surface window on the Tauri main thread (created hidden;
/// revealed by the compositor thread once the video child is attached, see
/// the `Command::Create` arm).
fn reveal_webview_for(key: &WindowKey) {
    let Some(app) = app_handle().lock_unpoisoned().clone() else {
        return;
    };
    let label = remote_window_label(key);
    let app_for_closure = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(window) = app_for_closure.get_webview_window(&label) {
            let _ = window.show();
        }
    });
}

// ---------------------------------------------------------------------------
// Command plumbing
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CompositorHandle {
    tx: SyncSender<Command>,
}

fn compositor_handle() -> &'static CompositorHandle {
    static HANDLE: OnceLock<CompositorHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Command>(COMMAND_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("petal-compositor".to_string())
            .spawn(move || compositor_thread_main(rx))
            .expect("failed to spawn Windows compositor thread");
        CompositorHandle { tx }
    })
}

/// The cross-thread latest-frame mailbox. Frames are published by decoder
/// tasks and drained by the compositor pump; it is deliberately NOT part of
/// the ordered `Command` channel so frame payloads can never occupy command
/// capacity or block a lifecycle/geometry producer.
fn frame_mailbox() -> &'static FrameMailbox {
    static MAILBOX: OnceLock<FrameMailbox> = OnceLock::new();
    MAILBOX.get_or_init(FrameMailbox::new)
}

fn snapshot() -> &'static Mutex<Vec<RemoteWindowSummary>> {
    static SNAPSHOT: OnceLock<Mutex<Vec<RemoteWindowSummary>>> = OnceLock::new();
    SNAPSHOT.get_or_init(|| Mutex::new(Vec::new()))
}

/// Async send with bounded retry (never blocks a runtime worker).
async fn send_command_async(command: Command) {
    let handle = compositor_handle();
    let mut command = Some(command);
    let mut retries: u32 = 0;
    loop {
        let Some(next) = command.take() else {
            return;
        };
        match handle.tx.try_send(next) {
            Ok(()) => {
                if retries > 0 {
                    log::warn!(
                        "windows compositor: send_command_async queue was full for {retries} retries before sending"
                    );
                }
                return;
            }
            Err(TrySendError::Full(next)) => {
                let tag = command_tag(&next);
                command = Some(next);
                retries += 1;
                if retries == 1 || retries % 1000 == 0 {
                    log::warn!(
                        "windows compositor: send_command_async command queue FULL at retry {retries} (compositor thread may be stalled) — {tag}"
                    );
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(TrySendError::Disconnected(_)) => {
                log::error!("windows compositor: send_command_async queue DISCONNECTED (compositor thread died)");
                return;
            }
        }
    }
}

fn command_tag(command: &Command) -> &'static str {
    match command {
        Command::Create { .. } => "Create",
        Command::UpdateMetadata { .. } => "UpdateMetadata",
        Command::UpdateCanonicalSourceSize { .. } => "UpdateCanonicalSourceSize",
        Command::ResizeWebview { .. } => "ResizeWebview",
        Command::RevealWebview { .. } => "RevealWebview",
        Command::SetHidden { .. } => "SetHidden",
        Command::Activate { .. } => "Activate",
        Command::Remove { .. } => "Remove",
        Command::RemoveAll { .. } => "RemoveAll",
        Command::RemoveAllFor { .. } => "RemoveAllFor",
        Command::SnapshotContentFrames { .. } => "SnapshotContentFrames",
        Command::RepositionVideo { .. } => "RepositionVideo",
        Command::SyncPointerOverlay { .. } => "SyncPointerOverlay",
        Command::Shutdown => "Shutdown",
    }
}

/// Synchronous send for Tauri commands (rare UI actions); retries a bounded
/// number of times so a momentarily full queue cannot wedge the main thread.
fn send_command_sync(command: Command) {
    let handle = compositor_handle();
    let mut command = Some(command);
    for _ in 0..5_000 {
        let Some(next) = command.take() else {
            return;
        };
        match handle.tx.try_send(next) {
            Ok(()) => return,
            Err(TrySendError::Full(next)) => {
                command = Some(next);
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => return,
        }
    }
    log::warn!("windows compositor: command queue stayed full; dropping command");
}

/// Remove every compositor window (used at room leave / forced disconnect).
pub(crate) async fn remove_all(app: &tauri::AppHandle) {
    // #694: cancel every decode loop as part of the same teardown -- the
    // Windows analog of subscriber.rs's `cancel_all_window_states`, which
    // #682's counselors review added because room leave/rejoin (not just a
    // republish) is the MOST common way a decode loop's window goes away.
    cancel_all_decode_loops();
    frame_timing_registry().lock_unpoisoned().clear();
    for key in snapshot().lock_unpoisoned().iter() {
        let label = remote_window_label(&(key.owner_identity.clone(), key.window_id));
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
        if let Some(overlay) = app.get_webview_window(&pointer_overlay_label(&(
            key.owner_identity.clone(),
            key.window_id,
        ))) {
            let _ = overlay.close();
        }
        if let Some(overlay) = app.get_webview_window(&control_overlay_label(&(
            key.owner_identity.clone(),
            key.window_id,
        ))) {
            let _ = overlay.close();
        }
    }
    send_command_async(Command::RemoveAll).await;
}

/// Remove every compositor window owned by one participant.
pub(crate) async fn remove_all_for(app: &tauri::AppHandle, owner_identity: String) {
    // #694: cancel this owner's decode loops as part of the same teardown
    // (`ParticipantDisconnected`).
    cancel_decode_loops_for(&owner_identity);
    frame_timing_registry()
        .lock_unpoisoned()
        .retain(|key, _| key.0 != owner_identity);
    for key in snapshot().lock_unpoisoned().iter() {
        if key.owner_identity == owner_identity {
            let label = remote_window_label(&(key.owner_identity.clone(), key.window_id));
            if let Some(window) = app.get_webview_window(&label) {
                let _ = window.close();
            }
            if let Some(overlay) = app.get_webview_window(&pointer_overlay_label(&(
                key.owner_identity.clone(),
                key.window_id,
            ))) {
                let _ = overlay.close();
            }
            if let Some(overlay) = app.get_webview_window(&control_overlay_label(&(
                key.owner_identity.clone(),
                key.window_id,
            ))) {
                let _ = overlay.close();
            }
        }
    }
    send_command_async(Command::RemoveAllFor { owner_identity }).await;
}

/// Hide a window (retire) — internal feed path.
pub(crate) async fn hide_window(app: &tauri::AppHandle, key: WindowKey) {
    if let Some(window) = app.get_webview_window(&remote_window_label(&key)) {
        let _ = window.hide();
    }
    if let Some(overlay) = app.get_webview_window(&pointer_overlay_label(&key)) {
        let _ = overlay.hide();
    }
    if let Some(overlay) = app.get_webview_window(&control_overlay_label(&key)) {
        let _ = overlay.hide();
    }
    send_command_async(Command::SetHidden { key, hidden: true }).await;
}

/// Push one decoded I420 frame for a window.
pub(crate) async fn push_frame(
    key: WindowKey,
    y: Vec<u8>,
    y_stride: usize,
    u: Vec<u8>,
    u_stride: usize,
    v: Vec<u8>,
    v_stride: usize,
    width: u32,
    height: u32,
) {
    // Feed the no-frame watchdog (subscriber.rs) before the key moves into
    // the mailbox.
    note_frame_arrived(&key);
    // Latest-frame mailbox admission: replace-in-place, never blocks, never
    // carries frame payloads on the ordered compositor command channel.
    frame_mailbox().publish(
        key,
        StoredI420Frame {
            y,
            y_stride,
            u,
            u_stride,
            v,
            v_stride,
            width,
            height,
        },
    );
}
pub(crate) async fn update_window_canonical_source_size(
    key: WindowKey,
    size: (u32, u32),
) {
    send_command_async(Command::UpdateCanonicalSourceSize { key, size }).await;
}

pub(crate) async fn update_window_metadata(
    app: &tauri::AppHandle,
    key: WindowKey,
    owner_display_name: String,
    title: String,
    source_url: Option<String>,
    source_kind: SharedSourceKind,
    remote_control_available: bool,
    share_instance_id: Option<String>,
    control_mode: crate::remote_control_core::RemoteControlMode,
) {
    send_command_async(Command::UpdateMetadata {
        key: key.clone(),
        title: title.clone(),
        source_kind,
        share_instance_id: share_instance_id.clone(),
    })
    .await;

    let route = surface_route_url(
        key.1,
        &key.0,
        &owner_display_name,
        &title,
        source_url.as_deref(),
        source_kind,
        remote_control_available,
        share_instance_id.as_deref(),
        control_mode,
    );
    let Some((_, query)) = route.split_once('?') else {
        return;
    };
    let search = serde_json::to_string(&format!("?{query}"))
        .expect("Windows compositor metadata query is serializable");
    let label = remote_window_label(&key);
    let app = app.clone();
    if let Err(error) = app.clone().run_on_main_thread(move || {
        let Some(window) = app.get_webview_window(&label) else {
            return;
        };
        // A mode-only metadata update must not navigate the live surface:
        // navigation resets the controller header's active state even though
        // the grant remains valid. Other metadata changes still use the
        // existing navigation path so the title/URL/kind stay authoritative.
        let script = format!(
            "(() => {{ const nextSearch = {search}; if (window.location.search === nextSearch) return; const current = new URLSearchParams(window.location.search); const next = new URLSearchParams(nextSearch); const nextMode = next.get('controlMode'); current.delete('controlMode'); next.delete('controlMode'); if (current.toString() === next.toString() && typeof window.__petalRemoteControlMode === 'function') {{ window.__petalRemoteControlMode(nextMode === 'fullControl' ? 'fullControl' : 'cursorPreserving'); }} else {{ window.location.replace(window.location.pathname + nextSearch); }} }})();"
        );
        if let Err(error) = window.eval(&script) {
            log::warn!(
                "windows compositor: failed to refresh metadata for '{}': {error}",
                window.label()
            );
        }
    }) {
        log::warn!("windows compositor: metadata refresh dispatch failed: {error}");
    }
}

/// Create a remote window as a Tauri WebviewWindow hosting the surface route,
/// then hand the native HWND to the compositor thread to attach the video
/// child + swap chain below the header. Idempotent: a republish under the
/// same key reuses the existing window.
pub(crate) async fn create_window(
    app: &tauri::AppHandle,
    key: WindowKey,
    owner_display_name: String,
    title: String,
    source_url: Option<String>,
    source_kind: SharedSourceKind,
    remote_control_available: bool,
    share_instance_id: Option<String>,
    control_mode: crate::remote_control_core::RemoteControlMode,
    canonical_source_size: Option<(u32, u32)>,
) {
    if window_open_for(&key) {
        return;
    }
    let label = remote_window_label(&key);
    if app.get_webview_window(&label).is_some() {
        return;
    }
    // Remember the app handle for main-thread webview resize routing from the
    // compositor thread (frames arrive at sizes the window must follow).
    *app_handle().lock_unpoisoned() = Some(app.clone());
    let url = surface_route_url(
        key.1,
        &key.0,
        &owner_display_name,
        &title,
        source_url.as_deref(),
        source_kind,
        remote_control_available,
        share_instance_id.as_deref(),
        control_mode,
    );
    let window = match WebviewWindowBuilder::new(app, label.clone(), WebviewUrl::App(url.into()))
        .decorations(false)
        .transparent(true)
        .additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS)
        .resizable(true)
        .title(&title)
        // Reveal gate: created hidden so the user never sees a hollow frame;
        // the compositor thread reveals it as soon as the native video child
        // is attached (Command::Create), so the window appears the moment the
        // share is announced instead of after the first decoded frame lands.
        .visible(false)
        .inner_size(
            DEFAULT_WINDOW_SIZE.0 as f64,
            DEFAULT_WINDOW_SIZE.1 as f64 + HEADER_HEIGHT as f64,
        )
        .build()
    {
        Ok(window) => window,
        Err(error) => {
            log::error!("windows compositor: failed to create surface window for {key:?}: {error}");
            return;
        }
    };
    // Opaque window with DWM-native corners — matches macOS RemoteWindowPanel
    // corner_radius(10.0). Header strip paints its own opaque background
    // (--identity-header-bg); video child HWND covers the rest.
    crate::windows_corner::make_native_rounded(&window);
    // Reuse the window's native HWND as the video child's parent.
    let parent_hwnd = match window.hwnd() {
        Ok(hwnd) => hwnd,
        Err(error) => {
            log::error!("windows compositor: no hwnd for surface window: {error}");
            let _ = window.close();
            return;
        }
    };
    // A closed webview window (user hits Alt+F4 / the header's close path)
    // should retire the share: tear down the video child + swap chain.
    let close_label = label.clone();
    let resize_key = key.clone();
    let overlay_move_key = key.clone();
    let _ = window.on_window_event(move |event| {
        match event {
            WindowEvent::Destroyed => {
                log::info!("windows compositor: surface window closed: {close_label}");
            }
            WindowEvent::Resized(size) => {
                // Keep the video child filling the area below the header
                // whenever the webview window is resized (native edge drag).
                // Nonblocking + coalescing: a resize flood must never stall
                // this callback (see send_geometry_command).
                send_geometry_command(Command::RepositionVideo {
                    key: resize_key.clone(),
                    width: size.width,
                    height: size.height,
                });
            }
            WindowEvent::Moved(_) => {
                // The video child moved with its parent: the pointer overlay
                // must follow (no resize involved). Nonblocking + coalescing.
                send_geometry_command(Command::SyncPointerOverlay {
                    key: overlay_move_key.clone(),
                    reply: None,
                });
            }
            _ => {}
        }
    });
    send_command_async(Command::Create {
        key: key.clone(),
        title,
        parent_hwnd: parent_hwnd.0 as usize,
        source_kind,
        share_instance_id,
        canonical_source_size,
    })
    .await;
    // Control overlay first (below the pointer overlay in z-order): the
    // input-capture surface. Created alongside the surface window, idempotent
    // per key, positioned by the same sync pass as the pointer overlay.
    create_control_overlay(app, &key);
    // Pointer overlay: a transparent, click-through webview hosting the
    // compositor/pointer route, sized to the video content area and kept in
    // sync on every create/resize/move by the compositor thread.
    create_pointer_overlay(app, &key);
}

/// Create the pointer overlay webview for a remote window (idempotent per
/// key). Built on the MAIN thread like the surface window — WebView2 cannot
/// be constructed on the tracker/sender threads.
fn create_pointer_overlay(app: &tauri::AppHandle, key: &WindowKey) {
    let label = pointer_overlay_label(key);
    if app.get_webview_window(&label).is_some() {
        return;
    }
    let url = format!(
        "compositor/pointer?windowId={}&ownerIdentity={}",
        key.1,
        percent_encode(&key.0)
    );
    match WebviewWindowBuilder::new(app, label.clone(), WebviewUrl::App(url.into()))
        .decorations(false)
        .transparent(true)
        // NO shadow: tao's undecorated-shadow implementation insets the
        // client area by the invisible frame (WM_NCCALCSIZE + insets), which
        // would render the overlay's content ~7px RIGHT of its positioned
        // rect — the constant telepointer offset seen live. A fully
        // transparent, click-through overlay needs no shadow anyway.
        .shadow(false)
        // NOT user-resizable: a resizable overlay absorbs the user's drags
        // on the remote content (observed live: the overlay grew to
        // 2123x968 while the user was free-scaling the remote window).
        .resizable(false)
        .skip_taskbar(true)
        .additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS)
        .title("Petal Pointer")
        // Fully transparent until the first pointer update renders inside;
        // the route only draws name-tagged cursor dots, so a shown-but-empty
        // overlay is invisible (never a black frame).
        .visible(false)
        .inner_size(1.0, 1.0)
        .build()
    {
        Ok(overlay) => {
            // Click-through: never block interaction with the remote content
            // beneath the overlay (wry sets WS_EX_TRANSPARENT | WS_EX_LAYERED).
            if let Err(error) = overlay.set_ignore_cursor_events(true) {
                log::warn!("windows compositor: pointer overlay click-through failed: {error}");
            }
            // Remember the overlay's native HWND so the compositor thread can
            // SetWindowPos it synchronously with the video child (never an
            // async dispatch — the overlay must not lag the child mid-drag).
            // `hwnd()` is a blocking RPC but this runs on the async side
            // (never the compositor thread).
            if let Ok(hwnd) = overlay.hwnd() {
                overlay_hwnds()
                    .lock_unpoisoned()
                    .insert(key.clone(), hwnd.0 as isize);
            }
            // The surface window may have been removed while the overlay was
            // building (concurrent stop-share / room leave). Ask the
            // compositor thread authoritatively (FIFO after `Command::Create`,
            // so by the time this reply lands the window state is settled):
            // a snapshot-based check could race Create itself and kill a
            // healthy overlay, or leak an orphan. On timeout we err toward
            // keeping the overlay (it will be closed by the removal path).
            let (exists_tx, mut exists_rx) = tokio::sync::oneshot::channel();
            send_command_sync(Command::SyncPointerOverlay {
                key: key.clone(),
                reply: Some(exists_tx),
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
            let window_exists = loop {
                match exists_rx.try_recv() {
                    Ok(exists) => break exists,
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break true,
                }
            };
            if !window_exists {
                let _ = overlay.close();
            }
        }
        Err(error) => {
            log::error!(
                "windows compositor: failed to create pointer overlay for {key:?}: {error}"
            );
        }
    }
}

/// Create the control overlay webview for a remote window (idempotent per
/// key). This is the input-capture surface: a transparent, always
/// cursor-interactive overlay over the video content area hosting
/// `compositor/control.html`, whose route forwards remote-control pointer/
/// wheel/key events only after explicit activation
/// (`set_remote_control_active`). Stays interactive even before control is
/// active so its bezel + resize-handle zones keep working (mirrors macOS).
/// Z-order is pinned above the remote window by the same
/// `sync_pointer_overlay` pass, so it is occluded with the window.
fn create_control_overlay(app: &tauri::AppHandle, key: &WindowKey) {
    let label = control_overlay_label(key);
    if app.get_webview_window(&label).is_some() {
        return;
    }
    let owner_identity =
        percent_encoding::utf8_percent_encode(&key.0, percent_encoding::NON_ALPHANUMERIC);
    let url = format!(
        "compositor/control.html?windowId={}&owner={owner_identity}&sourceWidth=0&sourceHeight=0",
        key.1
    );
    match WebviewWindowBuilder::new(app, label.clone(), WebviewUrl::App(url.into()))
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .skip_taskbar(true)
        .additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS)
        .title("Petal Control")
        .visible(false)
        .inner_size(1.0, 1.0)
        .build()
    {
        Ok(overlay) => {
            // Cursor-interactive always (resize handles + #678 raise), unlike
            // the click-through pointer overlay above it.
            if let Ok(hwnd) = overlay.hwnd() {
                control_overlay_hwnds()
                    .lock_unpoisoned()
                    .insert(key.clone(), hwnd.0 as isize);
            }
            // Authoritative existence check + first position (same FIFO-after-
            // Create reasoning as the pointer overlay).
            let (exists_tx, mut exists_rx) = tokio::sync::oneshot::channel();
            send_command_sync(Command::SyncPointerOverlay {
                key: key.clone(),
                reply: Some(exists_tx),
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
            let window_exists = loop {
                match exists_rx.try_recv() {
                    Ok(exists) => break exists,
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break true,
                }
            };
            if !window_exists {
                let _ = overlay.close();
            }
        }
        Err(error) => {
            log::error!(
                "windows compositor: failed to create control overlay for {key:?}: {error}"
            );
        }
    }
}

/// Whether a remote window currently exists for this key (used by the feed
/// to distinguish a fresh share from a republish).
pub(crate) fn window_open_for(key: &WindowKey) -> bool {
    snapshot()
        .lock_unpoisoned()
        .iter()
        .any(|window| window.owner_identity == key.0 && window.window_id == key.1)
}

/// Activate/deactivate remote control for a compositor window: toggles the
/// control overlay's in-page `active` state (which gates its pointer/wheel/key
/// forwarding) via `remote_control_active_script`, and re-syncs the overlay so
/// it is on screen and under the pointer overlay. Called by
/// `remote_control::set_remote_window_control_active` whenever a host status
/// flips. No-op (Err) when the window or its control overlay is gone.
pub(crate) fn set_remote_control_active(
    app: &tauri::AppHandle,
    window_id: u32,
    owner_identity: Option<&str>,
    active: bool,
) -> Result<(), String> {
    let key = resolve_key(window_id, owner_identity)
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    let overlay = app
        .get_webview_window(&control_overlay_label(&key))
        .ok_or_else(|| format!("control overlay for window {window_id} is not open"))?;
    let js =
        format!("window.__petalPendingRemoteControlActive = {active}; window.__petalRemoteControlSetActive?.({active});");
    overlay
        .eval(&js)
        .map_err(|e| format!("control eval failed: {e}"))?;
    // Ensure the input overlay is positioned (video content rect) and shown.
    send_command_sync(Command::SyncPointerOverlay {
        key: key.clone(),
        reply: None,
    });
    Ok(())
}

/// Remove a remote window (terminal: stop-sharing / unpublish).
pub(crate) async fn remove_window(app: &tauri::AppHandle, key: WindowKey) {
    crate::remote_control_core::remote_control_engine().remove_controller_grant(key.1, &key.0);
    crate::windows_remote_control::clear_pending_controller_operations(key.1, Some(&key.0));
    // #694: cancel this window's decode loop as part of the same teardown
    // (`TrackUnpublished`) -- see the "Decode-loop cancellation" section
    // above for why this lives in a standalone registry rather than on
    // `Compositor::windows` itself.
    cancel_decode_loop(&key);
    let label = remote_window_label(&key);
    // Diagnostics for the stop-sharing lifecycle: "contents cleared but the
    // window stayed" means `Command::Remove` destroyed the video child while
    // the webview survived — i.e. the close below failed or was skipped.
    match app.get_webview_window(&label) {
        Some(window) => match window.close() {
            Ok(()) => log::info!("windows compositor: remove_window: closed webview {label}"),
            Err(error) => log::warn!(
                "windows compositor: remove_window: close of webview {label} FAILED: {error}"
            ),
        },
        None => log::warn!(
            "windows compositor: remove_window: webview {label} not found — close SKIPPED (video child will clear while the window survives)"
        ),
    }
    // Tear down the pointer + control overlays with the surface window.
    if let Some(overlay) = app.get_webview_window(&pointer_overlay_label(&key)) {
        let _ = overlay.close();
    }
    if let Some(overlay) = app.get_webview_window(&control_overlay_label(&key)) {
        let _ = overlay.close();
    }
    send_command_async(Command::Remove { key: key.clone() }).await;
    drop_frame_timing(&key);
    log::info!("windows compositor: remove_window: compositor entry removed for {label}");
}

/// Whether the remote window is currently on-screen: the webview window (the
/// video child's parent) is visible and not minimized. Used to skip rendering
/// while the user can't see the window, saving CPU/GPU work; the last frame
/// is kept so the window resumes instantly on restore.
fn window_on_screen(video_hwnd: HWND) -> bool {
    let Ok(parent) = (unsafe { GetParent(video_hwnd) }) else {
        return false;
    };
    let visible = unsafe { IsWindowVisible(parent) };
    let minimized = unsafe { IsIconic(parent) };
    visible.as_bool() && !minimized.as_bool() && is_win_on_visible_space(parent)
}

/// Whether `hwnd` sits on the currently visible virtual desktop (Windows 10+
/// virtual desktops). Placeholder: always true today — virtual-desktop
/// awareness is a follow-up. The real implementation would query
/// `IVirtualDesktopManager::IsWindowOnCurrentVirtualDesktop` (ShObjIdl) or
/// watch `VirtualDesktopManager` change notifications; wire that in here when
/// the feature lands, and the render + decode gates pick it up automatically.
fn is_win_on_visible_space(_hwnd: HWND) -> bool {
    true
}

// Re-assert each remote window's overlays into the z-order immediately above
// its surface window. The pointer/control overlays are pinned there by
// `sync_pointer_overlay`, but that only runs on OUR window events (create/
// resize/move) — this 16ms-timer pass catches OTHER windows being dragged over
// the share, which never reach us. Change-detected via `overlay_z_above` so an
// unchanged arrangement is a no-op.
/// Reconcile every dirty geometry key from current HWND state. Drains the
/// set each pump tick so a coalesced/dropped move/resize command is corrected
/// without waiting for another window event.
fn reconcile_dirty_geometry(compositor: &mut Compositor) {
    let dirty: Vec<WindowKey> = dirty_geometry_keys().lock_unpoisoned().drain().collect();
    for key in dirty {
        compositor.reconcile_dirty_geometry(&key);
    }
}

fn reassert_overlay_z_orders(compositor: &Compositor) {
    let mut cached = overlay_z_above().lock_unpoisoned();
    for window in compositor.windows.values() {
        let Ok(surface) = (unsafe { GetParent(window.video_hwnd) }) else {
            continue;
        };
        if surface.0.is_null() {
            continue;
        }
        let above = crate::platform::windows::window_above_in_z_order(surface);
        let value = above.0 as isize;
        if cached.get(&window.key).copied() == Some(value) {
            continue;
        }
        let pointer = overlay_hwnds().lock_unpoisoned().get(&window.key).copied();
        let control = control_overlay_hwnds()
            .lock_unpoisoned()
            .get(&window.key)
            .copied();
        // Keep the rendered pointer above the full-size input overlay on
        // every z-order reassertion, not only during initial positioning.
        for overlay_hwnd in [control, pointer].into_iter().flatten() {
            let _ = unsafe {
                SetWindowPos(
                    HWND(overlay_hwnd as *mut core::ffi::c_void),
                    Some(above),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                )
            };
        }
        cached.insert(window.key.clone(), value);
    }
    cached.retain(|key, _| compositor.windows.contains_key(key));
}

// ---------------------------------------------------------------------------
// Compositor thread
// ---------------------------------------------------------------------------

fn compositor_thread_main(rx: Receiver<Command>) {
    let _apartment = ComApartment::enter();
    if !register_window_classes() {
        return;
    }
    let Ok((device, context)) = create_d3d_device() else {
        log::error!("windows compositor: no D3D11 device available");
        return;
    };
    let Some(pump_hwnd) = create_pump_window() else {
        log::error!("windows compositor: failed to create pump window");
        return;
    };
    let _ = unsafe { SetTimer(Some(pump_hwnd), PUMP_TIMER_ID, PUMP_TICK_MS, None) };
    set_thread_device(Some((device.clone(), context.clone())));
    let mut compositor = Compositor {
        device,
        context,
        windows: HashMap::new(),
    };

    let mut msg = MSG::default();
    let mut running = true;
    while running {
        let get_result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if get_result.0 == 0 || get_result.0 == -1 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if msg.message == WM_QUIT {
            running = false;
        }
        if msg.message == WM_TIMER && msg.wParam.0 as usize == PUMP_TIMER_ID {
            drain_commands(&mut compositor, &rx);
            refresh_visibility(&compositor);
            reconcile_dirty_geometry(&mut compositor);
            reassert_overlay_z_orders(&compositor);
        }
    }
    drain_commands(&mut compositor, &rx);
    compositor.remove_all();
    set_thread_device(None);
}

/// Refresh the shared per-key on-screen flags from the authoritative window
/// state, every pump tick. Combines the Win32 visibility/minimize/virtual-
/// desktop checks with the compositor's own hidden flag, so the feed's
/// decode gate tracks the real on-screen state even when no frames are
/// flowing (the decode loop is the thing that stops when hidden).
fn refresh_visibility(compositor: &Compositor) {
    let mut state = window_visible_state().lock_unpoisoned();
    state.clear();
    for window in compositor.windows.values() {
        // A window that has not produced its FIRST frame is still behind the
        // reveal gate (the surface webview is created hidden). Marking it
        // off-screen here deadlocks the reveal: paused -> no frame -> no
        // reveal -> stays hidden -> stays paused (B001 transparent-window
        // incident). `latest_frame.is_none()` is exactly the "still behind
        // the reveal gate" proxy — the reveal fires in render_frame on the
        // first frame. Only once a window has actually shown content do the
        // real on-screen checks apply (minimize / user hide / virtual
        // desktop).
        let on_screen = window.latest_frame.is_none()
            || (!window.hidden && window_on_screen(window.video_hwnd));
        state.insert(window.key.clone(), on_screen);
    }
}

/// Drain queued ordered commands, then present a bounded fair batch of
/// mailbox frames. Ordered lifecycle/geometry commands are always processed
/// before any frame work, so a frame flood can never delay them; the frame
/// batch is taken one ready window per tick (round-robin) so a single
/// high-rate window cannot starve the others.
fn drain_commands(compositor: &mut Compositor, rx: &Receiver<Command>) {
    let mut batch: Vec<Command> = Vec::with_capacity(COMMAND_QUEUE_CAPACITY);
    while let Ok(command) = rx.try_recv() {
        batch.push(command);
    }
    for command in batch {
        process_non_frame_command(compositor, command);
    }
    // Present at most one ready window's latest frame per tick; remaining
    // windows stay queued in the mailbox and are served on later ticks.
    let Some((key, frame)) = frame_mailbox().take_next() else {
        return;
    };
    compositor.render_frame(&key, frame);
}

fn process_non_frame_command(compositor: &mut Compositor, command: Command) {
    match command {
        Command::Create {
            key,
            title,
            parent_hwnd,
            source_kind,
            share_instance_id,
            canonical_source_size,
        } => {
            compositor.create_window(
                &key,
                &title,
                parent_hwnd,
                source_kind,
                share_instance_id,
                canonical_source_size,
            );
            // Reveal the surface as soon as the native video child is
            // attached, instead of waiting for the first decoded frame (the
            // old first-frame reveal gate). With the placeholder tinted, the
            // header + neutral video area are visible the moment the share
            // is announced, removing the multi-second "remote window does
            // not appear" delay on cold subscriptions; the first frame's
            // canonical ResizeWebview sharpens it in place. Commands are
            // FIFO on the main thread (resize lands before reveal).
            if let Some((width, height)) = canonical_source_size {
                resize_webview_for(&key, width, height);
            }
            reveal_webview_for(&key);
        }
        Command::UpdateMetadata {
            key,
            title,
            source_kind,
            share_instance_id,
        } => compositor.update_metadata(&key, title, source_kind, share_instance_id),
        Command::UpdateCanonicalSourceSize { key, size } => {
            compositor.update_canonical_source_size(&key, size)
        }
        Command::SetHidden { key, hidden } => compositor.set_hidden(&key, hidden),
        Command::Activate { key } => compositor.activate(&key),
        Command::ResizeWebview { key, width, height } => {
            resize_webview_for(&key, width, height);
        }
        Command::RevealWebview { key } => reveal_webview_for(&key),
        Command::RepositionVideo { key, width, height } => {
            compositor.reposition_video(&key, width, height);
        }
        Command::SyncPointerOverlay { key, reply } => {
            let exists = compositor.windows.contains_key(&key);
            compositor.sync_pointer_overlay(&key);
            if let Some(reply) = reply {
                let _ = reply.send(exists);
            }
        }
        Command::SnapshotContentFrames { reply } => {
            let frames = compositor.snapshot_content_frames();
            let _ = reply.send(frames);
        }
        Command::Remove { key } => compositor.remove(&key),
        Command::RemoveAllFor { owner_identity } => compositor.remove_all_for(&owner_identity),
        Command::RemoveAll => compositor.remove_all(),
        Command::Shutdown => {}
    }
}

// ---------------------------------------------------------------------------
// Win32 window plumbing
// ---------------------------------------------------------------------------

fn register_window_classes() -> bool {
    let instance: HINSTANCE = match unsafe { GetModuleHandleW(None) } {
        Ok(instance) => instance.into(),
        Err(error) => {
            log::error!("windows compositor: GetModuleHandleW failed: {error}");
            return false;
        }
    };

    let video_name: Vec<u16> = VIDEO_WINDOW_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let video_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(video_window_proc),
        hInstance: instance,
        lpszClassName: PCWSTR(video_name.as_ptr()),
        ..Default::default()
    };
    let result = unsafe { RegisterClassW(&video_class) };
    if result == 0 {
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            log::error!(
                "windows compositor: RegisterClassW (video) failed (0x{:08X})",
                error.0
            );
            return false;
        }
    }

    let pump_name: Vec<u16> = PUMP_WINDOW_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let pump_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(pump_window_proc),
        hInstance: instance,
        lpszClassName: PCWSTR(pump_name.as_ptr()),
        ..Default::default()
    };
    let result = unsafe { RegisterClassW(&pump_class) };
    if result == 0 {
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            log::error!(
                "windows compositor: RegisterClassW (pump) failed (0x{:08X})",
                error.0
            );
            return false;
        }
    }
    true
}

fn create_pump_window() -> Option<HWND> {
    let class_name: Vec<u16> = PUMP_WINDOW_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.ok()?.into();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(std::ptr::null()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
        .ok()
    }
}

/// Create the child window that hosts the video swap chain, positioned below
/// the header strip of the WebviewWindow.
fn create_video_child_hwnd(parent: HWND) -> Option<HWND> {
    let class_name: Vec<u16> = VIDEO_WINDOW_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.ok()?.into();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(std::ptr::null()),
            WS_CHILD | WS_VISIBLE,
            0,
            HEADER_HEIGHT,
            DEFAULT_WINDOW_SIZE.0 as i32,
            DEFAULT_WINDOW_SIZE.1 as i32,
            Some(parent),
            None,
            Some(instance),
            None,
        )
        .ok()
    }
}

/// The video child's wndproc: resizes the swap chain + texture when the
/// child is resized (the parent's WM_SIZE repositions it to fill the area
/// below the header).
unsafe extern "system" fn video_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SIZE => {
            // The child window is sized by `reposition_video` to fill the
            // webview window below the header — including user drag-resizes.
            // The swap chain deliberately does NOT follow: it stays at the
            // published frame size and `DXGI_SCALING_STRETCH` scales it to
            // the (possibly larger/smaller) child window on Present. Resizing
            // buffers here would mislead `present_frame` into treating a
            // user resize as a sender republish and bounce the webview back
            // to the source size.
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn pump_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let _ = (hwnd, wparam, lparam);
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn recreate_texture_for_window(device: &ID3D11Device, window: &mut RemoteWindow) {
    let (width, height) = window.back_buffer_size;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    if unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.is_ok() {
        if let Some(texture) = texture {
            if let Ok(resource) = texture.cast::<ID3D11Resource>() {
                window.texture = Some((texture, resource));
            }
        }
    }
}

/// Detect the sender's letterbox bars and return the centered content
/// rectangle in decoded-frame coordinates, or None when there are no bars or
/// the scan is ambiguous (never crop real content). The sender fills bars
/// with Y=16/U=128/V=128 exactly; after lossy H.264 they decode near-black,
/// so a row/column counts as "bar-like" when >=95% of its luma is below a
/// threshold. A whole-frame-near-black or off-center result is treated as
/// no-crop.
fn letterbox_content_rect(frame: &StoredI420Frame) -> Option<(u32, u32, u32, u32)> {
    const BAR_Y_MAX: u8 = 26;
    const MIN_BAR: usize = 6;
    const MIN_CONTENT: u32 = 16;
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w < 64 || h < 64 {
        return None;
    }
    let y = &frame.y;
    let sy = frame.y_stride as usize;

    let bar_like = |base: usize, count: usize| -> bool {
        let mut dark = 0usize;
        let mut i = 0usize;
        while i < count {
            if y[base + i] < BAR_Y_MAX {
                dark += 1;
            }
            i += 1;
        }
        dark * 100 >= count * 95
    };

    let mut top = 0usize;
    while top < h && bar_like(top * sy, w) {
        top += 1;
    }
    let mut bottom = 0usize;
    while bottom < h && bar_like((h - 1 - bottom) * sy, w) {
        bottom += 1;
    }
    let mut left = 0usize;
    'left: while left < w {
        let mut dark = 0usize;
        let mut i = 0usize;
        while i < h {
            if y[i * sy + left] < BAR_Y_MAX {
                dark += 1;
            }
            i += 1;
        }
        if dark * 100 >= h * 95 {
            left += 1;
        } else {
            break 'left;
        }
    }
    let mut right = 0usize;
    'right: while right < w {
        let mut dark = 0usize;
        let mut i = 0usize;
        while i < h {
            if y[i * sy + (w - 1 - right)] < BAR_Y_MAX {
                dark += 1;
            }
            i += 1;
        }
        if dark * 100 >= h * 95 {
            right += 1;
        } else {
            break 'right;
        }
    }

    let max_bar = top.max(bottom).max(left).max(right);
    if max_bar < MIN_BAR {
        return None; // no significant bars (also the no-bars case)
    }
    if top + bottom >= h - 16 || left + right >= w - 16 {
        return None; // content too small / whole frame near-black: ambiguous
    }
    // The sender centers the content exactly; codec noise must not shift us.
    if left.abs_diff(right) > 2 || top.abs_diff(bottom) > 2 {
        return None;
    }
    let mut off_x = left as u32;
    let mut off_y = top as u32;
    let mut cw = (w - left - right) as u32;
    let mut ch = (h - top - bottom) as u32;
    if cw < MIN_CONTENT || ch < MIN_CONTENT {
        return None;
    }
    // Even-align for I420 chroma (U/V are 2x2 subsampled).
    off_x &= !1;
    off_y &= !1;
    cw = (cw + 1) & !1;
    ch = (ch + 1) & !1;
    Some((off_x, off_y, cw, ch))
}

/// Crop-apply debounce: returns true (and clears the pending state) when the
/// content rect has been stable for `dwell`. CRITICAL: `first_seen` is only
/// (re)started when the rect CHANGES — refreshing it every frame would make
/// the dwell never elapse, so the crop would never apply and the window
/// would stay stuck at a stale size while frames keep arriving.
fn crop_debounce_should_apply(
    pending: &mut Option<(Option<(u32, u32, u32, u32)>, std::time::Instant)>,
    rect: Option<(u32, u32, u32, u32)>,
    now: std::time::Instant,
    dwell: std::time::Duration,
) -> bool {
    match *pending {
        Some((tracked, first_seen)) if tracked == rect => {
            if now.duration_since(first_seen) >= dwell {
                *pending = None;
                true
            } else {
                false // keep first_seen; the dwell is still pending
            }
        }
        _ => {
            *pending = Some((rect, now));
            false
        }
    }
}

/// Paint the swap chain's back buffer a neutral "connecting" gray once at
/// creation, so a revealed-but-not-yet-framed remote window reads as a
/// loading panel rather than a broken black void (the first decoded frame
/// typically lands ~3-4s later, after SFU subscription negotiation).
/// Best-effort: any failure leaves the default black back buffer (the early
/// reveal still happens; only the tint is lost).
fn paint_placeholder(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    swap_chain: &IDXGISwapChain1,
) {
    let back_buffer: ID3D11Texture2D = match unsafe { swap_chain.GetBuffer(0) } {
        Ok(buffer) => buffer,
        Err(_) => return,
    };
    let back_resource: ID3D11Resource = match back_buffer.cast() {
        Ok(resource) => resource,
        Err(_) => return,
    };
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    if unsafe { device.CreateRenderTargetView(Some(&back_resource), None, Some(&mut rtv)) }.is_err()
        || rtv.is_none()
    {
        return;
    }
    let rtv = rtv.expect("rtv is Some after a successful CreateRenderTargetView");
    let color = [0.16f32, 0.18, 0.21, 1.0];
    unsafe {
        context.ClearRenderTargetView(&rtv, &color);
    }
    if let Err(error) = unsafe { swap_chain.Present(1, DXGI_PRESENT(0)).ok() } {
        log::warn!("windows compositor: placeholder present failed: {error}");
    }
}

/// Returns true when a device-removal error was encountered.
fn present_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    window: &mut RemoteWindow,
    frame: &StoredI420Frame,
) -> bool {
    let frame_size = (frame.width, frame.height);
    if frame_size != window.published_frame_size {
        // The SENDER republished at a new size (or a simulcast layer switch
        // changed the decoded dimensions). Resize ONLY the swap chain buffers
        // + texture so the 1:1 frame copy fits — never the WebviewWindow:
        // the window keeps the first-frame size and the user's own drags,
        // and `DXGI_SCALING_STRETCH` scales the buffer into it. Resizing the
        // webview here fought the user's free-scale (observed live: the
        // remote window snapped back to the source size) and churned the
        // pointer overlay. The video child size never changes here either
        // (a USER drag-resize changes only the child via `reposition_video`).
        if let Err(error) = unsafe {
            window.swap_chain.ResizeBuffers(
                2,
                frame_size.0,
                frame_size.1,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        } {
            return device_removed(&error);
        }
        let previous_frame_size = window.published_frame_size;
        window.back_buffer_size = frame_size;
        window.published_frame_size = frame_size;
        log::info!(
            "windows compositor: window {:?} decoded frame switched {}x{} -> {}x{} (canonical {:?}) — receiver layer change or sender republish",
            window.key,
            previous_frame_size.0,
            previous_frame_size.1,
            frame_size.0,
            frame_size.1,
            window.canonical_pixel_size
        );
        // A republish/re-anchor resets the presented region to the full
        // frame; the next frames re-evaluate the letterbox crop.
        window.content_rect = None;
        window.crop_pending = None;
        recreate_texture_for_window(device, window);
    }

    // Sender letterbox crop: while a resize is in progress the sender pads
    // the captured frame to a fixed published size with black bars; crop the
    // bars off so the remote window hugs the content. Debounced to the
    // settled rect (CROP_SETTLE_DWELL) so a drag doesn't resize per frame.
    // Display-region padding is intentional: it preserves the selector's full
    // geometry while the selector partially overlaps its owning display. Do
    // not mistake those blank pixels for sender-resize letterboxing.
    let content_rect = if window.source_kind == SharedSourceKind::DisplayRegion {
        None
    } else {
        letterbox_content_rect(frame)
    };
    if content_rect != window.content_rect {
        let now = std::time::Instant::now();
        let apply = crop_debounce_should_apply(
            &mut window.crop_pending,
            content_rect,
            now,
            CROP_SETTLE_DWELL,
        );
        if apply {
            window.content_rect = content_rect;
            let (crop_w, crop_h) = match content_rect {
                Some((_, _, cw, ch)) => (cw, ch),
                None => (frame.width, frame.height),
            };
            // Resize ONLY the swap chain buffers (the frame copy is 1:1) —
            // never the WebviewWindow: the crop settles between a user drag
            // and the source's republish, and a webview resize here snapped
            // the remote window back to the source size mid-drag. The
            // `DXGI_SCALING_STRETCH` presents the buffer into the window's
            // current (possibly user-scaled) video child.
            if let Err(error) = unsafe {
                window.swap_chain.ResizeBuffers(
                    2,
                    crop_w,
                    crop_h,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
            } {
                return device_removed(&error);
            }
            window.back_buffer_size = (crop_w, crop_h);
            recreate_texture_for_window(device, window);
        }
    } else {
        window.crop_pending = None;
    }

    let Some(bgra) = crate::video_color::convert_i420_to_bgra(
        &frame.y,
        frame.y_stride,
        &frame.u,
        frame.u_stride,
        &frame.v,
        frame.v_stride,
        frame.width,
        frame.height,
        VideoColorProfile::SRGB_BT709_FULL,
    ) else {
        log::warn!(
            "windows compositor: I420→BGRA conversion failed for {:?}",
            window.video_hwnd
        );
        return false;
    };

    let Some((_, texture_resource)) = window.texture.as_ref() else {
        return false;
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if let Err(error) = unsafe {
        context.Map(
            texture_resource,
            0,
            D3D11_MAP_WRITE_DISCARD,
            0,
            Some(&mut mapped),
        )
    } {
        log::warn!("windows compositor: Map failed: {error}");
        return false;
    }
    unsafe {
        let bytes_per_row = frame.width as usize * 4;
        let dst = mapped.pData as *mut u8;
        let dst_pitch = mapped.RowPitch as usize;
        // Copy the intersection of the current content rect and the
        // presented (settled) crop region, in decoded-frame coordinates, so
        // the content stays centered even while a debounced crop update lags
        // a drag; the copy is bounded so it never writes past the texture's
        // own pitch/height.
        let (pr_x, pr_y, pr_w, pr_h) = match window.content_rect {
            Some((ox, oy, cw, ch)) => (ox, oy, cw, ch),
            None => (0, 0, frame.width, frame.height),
        };
        let (cr_x, cr_y, cr_w, cr_h) = match content_rect {
            Some((ox, oy, cw, ch)) => (ox, oy, cw, ch),
            None => (0, 0, frame.width, frame.height),
        };
        let pr_wu = pr_w as usize;
        let pr_hu = pr_h as usize;
        // WRITE_DISCARD leaves the whole texture undefined; when bars are
        // present, black-fill the presented region first so the letterbox
        // bars stay black instead of showing a frozen ghost of the last
        // pre-resize frame around the content.
        if content_rect.is_some() {
            let mut row = 0usize;
            while row < pr_hu {
                std::ptr::write_bytes(dst.add(row * dst_pitch), 0, dst_pitch);
                row += 1;
            }
        }
        let ix0 = cr_x.max(pr_x);
        let iy0 = cr_y.max(pr_y);
        let ix1 = (cr_x + cr_w).min(pr_x + pr_w);
        let iy1 = (cr_y + cr_h).min(pr_y + pr_h);
        if ix0 < ix1 && iy0 < iy1 {
            let mut row = iy0;
            while row < iy1 {
                let dest_row = (row - pr_y) as usize;
                if dest_row < pr_hu {
                    let dest_col = (ix0 - pr_x) as usize;
                    if dest_col < pr_wu {
                        let src_off = row as usize * bytes_per_row + ix0 as usize * 4;
                        let dst_off = dest_row * dst_pitch + dest_col * 4;
                        let copy_len = ((ix1 - ix0) as usize * 4)
                            .min((pr_wu - dest_col) * 4)
                            .min(dst_pitch.saturating_sub(dest_col * 4));
                        std::ptr::copy_nonoverlapping(
                            bgra.as_ptr().add(src_off),
                            dst.add(dst_off),
                            copy_len,
                        );
                    }
                }
                row += 1;
            }
        }
        context.Unmap(texture_resource, 0);
    }

    // Copy the CPU-filled dynamic texture into the swap-chain back buffer
    // (the render path: Map → copy → CopyResource → Present).
    let back_buffer: ID3D11Texture2D = match unsafe { window.swap_chain.GetBuffer(0) } {
        Ok(buffer) => buffer,
        Err(error) => {
            log::warn!("windows compositor: GetBuffer failed: {error}");
            return false;
        }
    };
    let back_resource: ID3D11Resource = match back_buffer.cast() {
        Ok(resource) => resource,
        Err(error) => {
            log::warn!("windows compositor: back buffer cast failed: {error}");
            return false;
        }
    };
    unsafe {
        context.CopyResource(&back_resource, texture_resource);
    }

    if let Err(error) = unsafe { window.swap_chain.Present(1, DXGI_PRESENT(0)).ok() } {
        return device_removed(&error);
    }
    false
}

fn region_frame_is_new_source_size(canonical: Option<(u32, u32)>, incoming: (u32, u32)) -> bool {
    incoming.0 > 0 && incoming.1 > 0 && canonical != Some(incoming)
}

/// Simulcast layers preserve aspect ratio; an aspect-ratio change therefore
/// identifies a sender-side source resize rather than a temporary low layer.
pub(crate) fn decoded_frame_has_source_aspect_change(
    canonical: Option<(u32, u32)>,
    incoming: (u32, u32),
) -> bool {
    let Some((canonical_w, canonical_h)) = canonical else {
        return false;
    };
    if canonical_w == 0 || canonical_h == 0 || incoming.0 == 0 || incoming.1 == 0 {
        return false;
    }
    let canonical_aspect = canonical_w as f64 / canonical_h as f64;
    let incoming_aspect = incoming.0 as f64 / incoming.1 as f64;
    ((incoming_aspect - canonical_aspect) / canonical_aspect).abs() > 0.05
}

fn device_removed(error: &windows::core::Error) -> bool {
    error.code() == DXGI_ERROR_DEVICE_REMOVED || error.code() == DXGI_ERROR_DEVICE_RESET
}

fn thread_device() -> &'static Mutex<Option<(ID3D11Device, ID3D11DeviceContext)>> {
    static DEVICE: OnceLock<Mutex<Option<(ID3D11Device, ID3D11DeviceContext)>>> = OnceLock::new();
    DEVICE.get_or_init(|| Mutex::new(None))
}

fn set_thread_device(device: Option<(ID3D11Device, ID3D11DeviceContext)>) {
    *thread_device().lock_unpoisoned() = device;
}

fn create_d3d_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
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
    if result.is_err() {
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
        }?;
    }
    Ok((
        device.ok_or_else(windows::core::Error::from_win32)?,
        context.ok_or_else(windows::core::Error::from_win32)?,
    ))
}

struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Self {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_ok() {
            Self(true)
        } else {
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

    // ---- #694: decode-loop cancellation ------------------------------------
    //
    // Mirrors #682's macOS test group. These drive the ACTUAL production
    // functions every window-removal site calls (`install_decode_loop_token`,
    // `cancel_decode_loop`, `cancel_decode_loops_for`, `cancel_all_decode_loops`)
    // together with the real `next_frame_or_cancelled` race
    // (`transport::subscriber`, made `pub(crate)` for this purpose) rather than
    // a hand-rolled restatement of either -- per CLAUDE.md's native-lifecycle
    // rule, a unit test on an isolated pure function is not sufficient evidence
    // that a real removal actually stops a real running task.
    //
    // Deliberately NOT exercised here: the outer `remove_window`/
    // `remove_all_for`/`remove_all` async functions, which also open/close
    // real Tauri `WebviewWindow`s and lazily spin up the dedicated D3D11/Win32
    // message-loop compositor thread (`compositor_handle()`) on first use --
    // machinery this sandbox cannot compile-check, let alone run headless, and
    // whose GPU/window-station behavior in CI is unverified from here. Wiring
    // those three functions to call the `cancel_decode_loop*` helpers below is
    // one line each (see the functions themselves) and is covered by code
    // review, not a unit test, for that reason -- the tests below instead
    // target exactly the functions that OWN the cancellation contract, which is
    // what the issue asks for ("whatever windows_compositor function ends up
    // owning cancellation").

    use crate::transport::subscriber::{next_frame_or_cancelled, FrameOrCancelled};
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::Arc;

    // `decode_loop_tokens()` is a single process-global registry. Cargo runs
    // `#[tokio::test]`s in parallel by default, and `cancel_all_decode_loops`
    // below deliberately drains the WHOLE map -- if it interleaves with a
    // sibling test's own install/assert window, it cancels that sibling's
    // token out from under it, producing an intermittent failure unrelated to
    // any real bug (caught by adversarial review of #694, matching this
    // repo's documented history of burning cycles on exactly this class of
    // flake -- see #617). Every test in this group takes this lock for its
    // whole body so they run one at a time.
    static DECODE_LOOP_TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn initial_content_size_caps_large_remote_display_and_preserves_aspect() {
        let (width, height) =
            initial_content_size_within_work_area(3840, 2160, Some((1440.0, 900.0)));
        assert_eq!((width, height), (1152.0, 648.0));
        assert!(width <= 1440.0 * INITIAL_MAX_WORK_AREA_FRACTION);
        assert!(HEADER_HEIGHT as f64 + height <= 900.0 * INITIAL_MAX_WORK_AREA_FRACTION + 0.5);
        assert!((width / height - 3840.0 / 2160.0).abs() < 0.001);
    }

    #[test]
    fn initial_content_size_leaves_small_source_unchanged() {
        assert_eq!(
            initial_content_size_within_work_area(800, 500, Some((1920.0, 1080.0))),
            (800.0, 500.0)
        );
    }

    #[tokio::test]
    async fn decode_loop_task_terminates_when_token_is_cancelled() {
        let _serial = DECODE_LOOP_TEST_SERIAL.lock().await;
        // The real teardown path: `cancel_decode_loop` is exactly what
        // `remove_window` (`TrackUnpublished`) calls. This spawns a task
        // running the SAME `next_frame_or_cancelled` race the real Windows
        // decode loop (`spawn_windows_decode_loop`, subscriber.rs) awaits every
        // iteration, fed by a stream that never yields on its own
        // (`futures::stream::pending`) -- so the ONLY way this task can ever
        // end is cancellation -- then calls `cancel_decode_loop` and asserts
        // the task actually terminates (joined with a bounded timeout) and
        // reached its post-cancellation completion marker, not merely that a
        // token object was flipped.
        let key: WindowKey = ("issue694-terminate-owner".to_string(), 694);
        let cancel_token = install_decode_loop_token(&key);

        let completed = Arc::new(AtomicBool::new(false));
        let completed_for_task = completed.clone();
        let handle = tokio::spawn(async move {
            let mut stream = futures::stream::pending::<()>();
            loop {
                match next_frame_or_cancelled(&mut stream, &cancel_token).await {
                    FrameOrCancelled::Cancelled => break,
                    FrameOrCancelled::Frame(_) => continue,
                }
            }
            completed_for_task.store(true, AtomicOrdering::SeqCst);
        });

        // Give the spawned task a chance to actually start awaiting the race
        // before cancelling -- otherwise a pass could be a false positive (the
        // task ending before it ever raced anything).
        tokio::task::yield_now().await;

        cancel_decode_loop(&key);

        let joined = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            joined.is_ok(),
            "decode loop task did not terminate within 2s of its token being cancelled -- \
             this is exactly the #694 leak (a task that outlives its window)"
        );
        assert!(joined.unwrap().is_ok(), "decode loop task panicked");
        assert!(
            completed.load(AtomicOrdering::SeqCst),
            "task exited without reaching its post-cancellation completion marker, so it did \
             not exit via the Cancelled arm"
        );
        assert!(
            decode_loop_tokens().lock_unpoisoned().get(&key).is_none(),
            "cancel_decode_loop must remove the entry, not just cancel it"
        );
    }

    #[tokio::test]
    async fn cancel_decode_loops_for_terminates_only_that_owners_tasks() {
        let _serial = DECODE_LOOP_TEST_SERIAL.lock().await;
        // `remove_all_for` (`ParticipantDisconnected`) must cancel every
        // decode loop for the departing participant WITHOUT touching a
        // different owner's still-live loop.
        let owner_a = "issue694-remove-for-owner-a";
        let owner_b = "issue694-remove-for-owner-b";
        let key_a = (owner_a.to_string(), 695u32);
        let key_b = (owner_b.to_string(), 696u32);
        let token_a = install_decode_loop_token(&key_a);
        let token_b = install_decode_loop_token(&key_b);

        let spawn_loop = |token: CancellationToken| {
            let completed = Arc::new(AtomicBool::new(false));
            let completed_for_task = completed.clone();
            let handle = tokio::spawn(async move {
                let mut stream = futures::stream::pending::<()>();
                loop {
                    match next_frame_or_cancelled(&mut stream, &token).await {
                        FrameOrCancelled::Cancelled => break,
                        FrameOrCancelled::Frame(_) => continue,
                    }
                }
                completed_for_task.store(true, AtomicOrdering::SeqCst);
            });
            (handle, completed)
        };
        let (handle_a, completed_a) = spawn_loop(token_a);
        let (handle_b, completed_b) = spawn_loop(token_b);
        tokio::task::yield_now().await;

        cancel_decode_loops_for(owner_a);

        let joined_a = tokio::time::timeout(Duration::from_secs(2), handle_a).await;
        assert!(joined_a.is_ok(), "owner a's decode loop did not terminate");
        assert!(completed_a.load(AtomicOrdering::SeqCst));
        assert!(
            decode_loop_tokens().lock_unpoisoned().get(&key_a).is_none(),
            "owner a's token must be removed"
        );

        // Owner b's loop must still be live -- give it a moment to prove it,
        // then cancel it directly so the test cleans up its own task.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !completed_b.load(AtomicOrdering::SeqCst),
            "cancel_decode_loops_for(owner_a) must not cancel owner b's decode loop"
        );
        assert!(
            decode_loop_tokens().lock_unpoisoned().get(&key_b).is_some(),
            "owner b's token must still be registered"
        );
        cancel_decode_loop(&key_b);
        let joined_b = tokio::time::timeout(Duration::from_secs(2), handle_b).await;
        assert!(
            joined_b.is_ok(),
            "owner b's decode loop did not terminate on cleanup"
        );
    }

    #[tokio::test]
    async fn cancel_all_decode_loops_terminates_every_registered_task() {
        let _serial = DECODE_LOOP_TEST_SERIAL.lock().await;
        // `remove_all` (`Disconnected` / room leave) -- mirrors
        // subscriber.rs's `feed_loop_exit_drains_and_cancels_every_remaining_decode_loop`,
        // added in #682's counselors-review follow-up because room leave is
        // the MOST common way a decode loop's window goes away, not just a
        // republish.
        let mut handles = Vec::new();
        let mut completions = Vec::new();
        for window_id in [697u32, 698u32] {
            let key = ("issue694-remove-all-owner".to_string(), window_id);
            let token = install_decode_loop_token(&key);
            let completed = Arc::new(AtomicBool::new(false));
            let completed_for_task = completed.clone();
            let handle = tokio::spawn(async move {
                let mut stream = futures::stream::pending::<()>();
                loop {
                    match next_frame_or_cancelled(&mut stream, &token).await {
                        FrameOrCancelled::Cancelled => break,
                        FrameOrCancelled::Frame(_) => continue,
                    }
                }
                completed_for_task.store(true, AtomicOrdering::SeqCst);
            });
            handles.push(handle);
            completions.push(completed);
        }
        tokio::task::yield_now().await;

        cancel_all_decode_loops();

        for (i, handle) in handles.into_iter().enumerate() {
            let joined = tokio::time::timeout(Duration::from_secs(2), handle).await;
            assert!(joined.is_ok(), "decode loop task {i} did not terminate");
            assert!(joined.unwrap().is_ok(), "decode loop task {i} panicked");
        }
        for (i, completed) in completions.into_iter().enumerate() {
            assert!(
                completed.load(AtomicOrdering::SeqCst),
                "task {i} exited without reaching its post-cancellation completion marker"
            );
        }
        // Check only this test's own keys, not the whole map's size -- other
        // tests share this same process-global registry and run concurrently.
        for window_id in [697u32, 698u32] {
            let key = ("issue694-remove-all-owner".to_string(), window_id);
            assert!(
                decode_loop_tokens().lock_unpoisoned().get(&key).is_none(),
                "cancel_all_decode_loops must remove every entry, not just cancel it"
            );
        }
    }

    #[test]
    fn install_decode_loop_token_cancels_prior_entrys_token() {
        // The replacement-insert regression, analogous to #682's
        // `insert_window_state_cancels_prior_entrys_decode_loop`: a republish
        // (a second `TrackSubscribed` for the SAME window id, with NO removal
        // call in between -- exactly what the real `TrackSubscribed` arm in
        // subscriber.rs does today) must cancel the OLD token by the install
        // alone, so the old loop stops instead of double-feeding the compositor
        // alongside the new one.
        let key: WindowKey = ("issue694-republish-owner".to_string(), 699);

        let old_token = install_decode_loop_token(&key);
        assert!(!old_token.is_cancelled());

        let new_token = install_decode_loop_token(&key);

        assert!(
            old_token.is_cancelled(),
            "the OLD decode loop's token must be cancelled by the replacement install alone"
        );
        assert!(
            !new_token.is_cancelled(),
            "the NEW decode loop's token must still be live"
        );
        assert!(
            decode_loop_tokens().lock_unpoisoned().contains_key(&key),
            "the new token must still be registered for this key"
        );
        cancel_decode_loop(&key); // cleanup: don't leak a live token past this test
    }

    /// Tauri window labels allow only `[A-Za-z0-9-/_:]`. The label must never
    /// contain `%` (the old percent-encoded form) or any other disallowed
    /// character, for ANY owner identity — including the UUID-format identity
    /// from the field failure and hostile identities with `@`, `.`, `:`, `_`,
    /// spaces, and unicode.
    #[test]
    fn remote_window_label_uses_only_tauri_allowed_characters() {
        let identities = [
            "8b89b417-ea57-4ba0-9618-1b8c8934e747", // exact failing identity
            "till@petal",
            "user.name+tag@example.com",
            "weird: id_!@#$",
            "A\u{1F600}",
            "",
        ];
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '/' | ':' | '_');
        for identity in identities {
            let label = remote_window_label(&(identity.to_string(), 2));
            assert!(
                label.chars().all(allowed),
                "label {label:?} (from {identity:?}) contains a disallowed character"
            );
            assert!(
                !label.contains('%'),
                "label {label:?} must not contain percent-encoding"
            );
            assert!(
                label.starts_with("petal-remote-"),
                "label {label:?} missing prefix"
            );
            assert!(
                label.ends_with("-2"),
                "label {label:?} missing window id suffix"
            );
        }
    }

    /// The label is deterministic and distinct per (owner, window id) pair.
    #[test]
    fn remote_window_label_is_deterministic_and_distinct_per_key() {
        let a = remote_window_label(&("bob@petal".to_string(), 1));
        let b = remote_window_label(&("bob@petal".to_string(), 1));
        let c = remote_window_label(&("bob@petal".to_string(), 2));
        let d = remote_window_label(&("alice@petal".to_string(), 1));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    fn mailbox_frame(marker: u8) -> StoredI420Frame {
        StoredI420Frame {
            y: vec![marker; 4],
            y_stride: 2,
            u: vec![128; 1],
            u_stride: 1,
            v: vec![128; 1],
            v_stride: 1,
            width: 2,
            height: 2,
        }
    }

    #[test]
    fn mailbox_replaces_stale_frames_in_place() {
        let mailbox = FrameMailbox::new();
        let key: WindowKey = ("alice".to_string(), 1);
        mailbox.publish(key.clone(), mailbox_frame(1));
        mailbox.publish(key.clone(), mailbox_frame(2));
        mailbox.publish(key.clone(), mailbox_frame(3));
        // Only the newest survives; the ready list holds the key exactly once.
        let (got_key, frame) = mailbox.take_next().expect("a frame must be ready");
        assert_eq!(got_key, key);
        assert_eq!(frame.y, vec![3u8; 4]);
        assert!(mailbox.take_next().is_none());
        assert!(mailbox.pending_keys().is_empty());
    }

    #[test]
    fn mailbox_serves_windows_fairly_in_arrival_order() {
        let mailbox = FrameMailbox::new();
        let a: WindowKey = ("alice".to_string(), 1);
        let b: WindowKey = ("bob".to_string(), 2);
        mailbox.publish(a.clone(), mailbox_frame(10));
        mailbox.publish(b.clone(), mailbox_frame(20));
        mailbox.publish(a.clone(), mailbox_frame(11));

        let (first, frame) = mailbox.take_next().expect("a is ready");
        assert_eq!(first, a);
        assert_eq!(frame.y, vec![11u8; 4]);
        let (second, frame) = mailbox.take_next().expect("b is ready");
        assert_eq!(second, b);
        assert_eq!(frame.y, vec![20u8; 4]);
        assert!(mailbox.take_next().is_none());
    }

    #[test]
    fn mailbox_republish_is_replaced_not_duplicated() {
        let mailbox = FrameMailbox::new();
        let key: WindowKey = ("alice".to_string(), 1);
        mailbox.publish(key.clone(), mailbox_frame(1));
        let _ = mailbox.take_next(); // drain
        mailbox.publish(key.clone(), mailbox_frame(2));
        assert_eq!(mailbox.pending_keys(), vec![key.clone()]);
        let (_, frame) = mailbox.take_next().expect("republished frame ready");
        assert_eq!(frame.y, vec![2u8; 4]);
    }

    #[test]
    fn mailbox_remove_drops_pending_frame_before_take() {
        let mailbox = FrameMailbox::new();
        let a: WindowKey = ("alice".to_string(), 1);
        let b: WindowKey = ("bob".to_string(), 2);
        mailbox.publish(a.clone(), mailbox_frame(1));
        mailbox.publish(b.clone(), mailbox_frame(2));
        mailbox.remove(&a);
        // `a` is skipped entirely; only `b` is served.
        let (key, frame) = mailbox.take_next().expect("b still ready");
        assert_eq!(key, b);
        assert_eq!(frame.y, vec![2u8; 4]);
        assert!(mailbox.take_next().is_none());
    }

    #[test]
    fn mailbox_clear_drops_everything() {
        let mailbox = FrameMailbox::new();
        let a: WindowKey = ("alice".to_string(), 1);
        mailbox.publish(a.clone(), mailbox_frame(1));
        mailbox.clear();
        assert!(mailbox.take_next().is_none());
        assert!(mailbox.pending_keys().is_empty());
    }

    fn frame_with_bars(
        w: usize,
        h: usize,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
    ) -> StoredI420Frame {
        let sy = w;
        let mut y = vec![16u8; w * h];
        for row in top..(h - bottom) {
            for col in left..(w - right) {
                y[row * sy + col] = 100;
            }
        }
        StoredI420Frame {
            y,
            y_stride: sy,
            u: vec![128u8; (w / 2) * (h / 2)],
            u_stride: w / 2,
            v: vec![128u8; (w / 2) * (h / 2)],
            v_stride: w / 2,
            width: w as u32,
            height: h as u32,
        }
    }

    #[test]
    fn source_aspect_change_is_resize_but_same_aspect_low_layer_is_not() {
        assert!(decoded_frame_has_source_aspect_change(
            Some((1001, 662)),
            (1000, 484)
        ));
        assert!(!decoded_frame_has_source_aspect_change(
            Some((1001, 662)),
            (750, 496)
        ));
        assert!(!decoded_frame_has_source_aspect_change(
            Some((1001, 662)),
            (1001, 662)
        ));
        assert!(!decoded_frame_has_source_aspect_change(None, (1000, 484)));
    }

    #[test]
    fn region_geometry_accepts_growth_and_shrink_without_simulcast() {
        assert!(region_frame_is_new_source_size(
            Some((640, 400)),
            (480, 300)
        ));
        assert!(region_frame_is_new_source_size(
            Some((752, 852)),
            (640, 400)
        ));
        assert!(region_frame_is_new_source_size(
            Some((640, 400)),
            (752, 852)
        ));
        assert!(!region_frame_is_new_source_size(
            Some((640, 400)),
            (640, 400)
        ));
        assert!(region_frame_is_new_source_size(None, (480, 300)));
    }

    #[test]
    fn letterbox_scan_finds_top_bottom_bars() {
        let frame = frame_with_bars(320, 240, 40, 40, 0, 0);
        assert_eq!(letterbox_content_rect(&frame), Some((0, 40, 320, 160)));
    }

    #[test]
    fn letterbox_scan_finds_left_right_bars() {
        let frame = frame_with_bars(320, 240, 0, 0, 60, 60);
        assert_eq!(letterbox_content_rect(&frame), Some((60, 0, 200, 240)));
    }

    #[test]
    fn letterbox_scan_finds_centered_bars() {
        let frame = frame_with_bars(320, 240, 32, 32, 24, 24);
        // Even-aligned content rect: off (24,32), w=320-48=272, h=240-64=176.
        assert_eq!(letterbox_content_rect(&frame), Some((24, 32, 272, 176)));
    }

    #[test]
    fn letterbox_scan_no_bars_returns_none() {
        let frame = frame_with_bars(320, 240, 0, 0, 0, 0);
        assert_eq!(letterbox_content_rect(&frame), None);
    }

    #[test]
    fn letterbox_scan_all_black_is_ambiguous() {
        let mut frame = frame_with_bars(320, 240, 0, 0, 0, 0);
        frame.y.fill(16); // whole frame near-black -> content too small
        assert_eq!(letterbox_content_rect(&frame), None);
    }

    #[test]
    fn letterbox_scan_off_center_is_ambiguous() {
        // Asymmetric bars (a non-letterbox dark edge) must NOT be cropped.
        let frame = frame_with_bars(320, 240, 40, 80, 0, 0);
        assert_eq!(letterbox_content_rect(&frame), None);
    }

    #[test]
    fn crop_debounce_applies_after_dwell_at_same_rect() {
        let t0 = std::time::Instant::now();
        let dwell = std::time::Duration::from_millis(400);
        let rect = Some((24, 32, 272, 176));
        let mut pending = None;
        // First differing frame starts the dwell.
        assert!(!crop_debounce_should_apply(&mut pending, rect, t0, dwell));
        // Same rect, dwell pending: first_seen is PRESERVED.
        assert!(!crop_debounce_should_apply(
            &mut pending,
            rect,
            t0 + std::time::Duration::from_millis(200),
            dwell
        ));
        // Same rect, past the dwell: applies and clears.
        assert!(crop_debounce_should_apply(
            &mut pending,
            rect,
            t0 + std::time::Duration::from_millis(401),
            dwell
        ));
        assert!(pending.is_none());
    }

    #[test]
    fn crop_debounce_restarts_when_rect_changes() {
        let t0 = std::time::Instant::now();
        let dwell = std::time::Duration::from_millis(400);
        let mut pending = None;
        let _ = crop_debounce_should_apply(&mut pending, Some((24, 32, 272, 176)), t0, dwell);
        // Rect changes mid-dwell: first_seen resets.
        let t1 = t0 + std::time::Duration::from_millis(300);
        let _ = crop_debounce_should_apply(&mut pending, Some((24, 32, 300, 200)), t1, dwell);
        assert!(!crop_debounce_should_apply(
            &mut pending,
            Some((24, 32, 300, 200)),
            t1 + std::time::Duration::from_millis(300),
            dwell
        ));
        assert!(crop_debounce_should_apply(
            &mut pending,
            Some((24, 32, 300, 200)),
            t1 + std::time::Duration::from_millis(401),
            dwell
        ));
    }
}
