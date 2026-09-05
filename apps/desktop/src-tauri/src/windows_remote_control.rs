//! Windows remote-control target validation and native input replay.
//!
//! The portable authorization, ordering, admission, and held-input state stays
//! in `remote_control_core`; this module owns only Win32/UIA target resolution
//! and injection.

use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, BOOL, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_CONTROL_FLAGS,
    DESKTOP_READOBJECTS, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_InvokePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetAncestor, GetClassNameW, GetCursorPos, GetForegroundWindow,
    GetGUIThreadInfo, GetWindowLongPtrW, GetWindowThreadProcessId, IsChild, IsIconic, IsWindow,
    IsWindowVisible, PostMessageW, SendMessageTimeoutW, SetCursorPos, SetForegroundWindow,
    WindowFromPoint, GA_ROOTOWNER, GUITHREADINFO, GWL_STYLE, SMTO_ABORTIFHUNG, WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::platform::cg::WindowFrame;
use crate::remote_control_core::{
    RemoteControlAction, RemoteControlCapability, RemoteControlMessage, RemoteControlMode,
    RemoteControlTargetKind, RemoteControlType,
};
use crate::sync_ext::MutexExt;
use crate::windows_capture_target::TargetKind;

const SYNTHETIC_INPUT_MARKER: usize = 0x5045_5441_4c52_4301;
const FOREGROUND_WAIT: Duration = Duration::from_millis(250);
/// How close the cursor must still be to where Petal last posted it for a
/// cursor-preserving restore to be count safe. Beyond this a human physically
/// moved the mouse mid-gesture and we must not yank it back (macOS parity:
/// `SESSION_TAP_CURSOR_TOLERANCE_POINTS`).
const CURSOR_TAKEOVER_TOLERANCE_PX: i32 = 6;
static GLOBAL_INPUT_COORDINATOR: Mutex<()> = Mutex::new(());

/// Per-share Host control mode (sharer-chosen policy; default cursor-
/// preserving). Written at share start / live mode change (session_stub),
/// read at replay time keyed by `window_id`. Host-side only.
static SHARE_MODES: LazyLock<Mutex<HashMap<u32, RemoteControlMode>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cursor-preserving takeover state: where the host's cursor was before the
/// gesture (`saved`) and where we last posted it (`last_posted`). Restore is
/// skipped unless the cursor is still within tolerance of `last_posted`.
#[derive(Clone, Copy)]
struct CursorTakeover {
    saved: (i32, i32),
    last_posted: (i32, i32),
}

static CURSOR_TAKEOVERS: LazyLock<Mutex<HashMap<u32, CursorTakeover>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static SYNTHETIC_KEYS: LazyLock<Mutex<HashSet<(u32, String, String)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
/// Modifier keys the host synthesized (not forwarded by the controller) for a
/// given main keypress, released together with it. macOS carries modifier flags
/// on the key event; Windows SendInput needs the actual modifier held around
/// the key, so a shift+letter must press Shift itself. We synthesize the
/// modifier ONLY when it is neither already synthetic nor physically held, so a
/// controller that forwards a separate Shift keydown (the normal desktop
/// route) is never double-pressed.
static SYNTHETIC_MODIFIERS: LazyLock<
    Mutex<HashMap<(u32, String, String), Vec<(u32, String, String)>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));
const PENDING_OPERATION_TTL: Duration = Duration::from_secs(15);
const MAX_PENDING_OPERATIONS: usize = 1_024;

#[derive(Clone)]
struct PendingControllerOperation {
    owner_identity: String,
    target_kind: RemoteControlTargetKind,
    share_instance_id: String,
    control_session_id: String,
    input_seq: u64,
    operation_fingerprint: String,
    expires_at: Instant,
}

static PENDING_CONTROLLER_OPERATIONS: LazyLock<
    Mutex<HashMap<(u32, String), PendingControllerOperation>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) async fn prepare_control_request(
    message: &mut RemoteControlMessage,
    window_id: u32,
    owner_identity: &str,
) -> Result<(), String> {
    if let Some(target) =
        crate::windows_compositor::remote_control_target_metadata(window_id, Some(owner_identity))
    {
        message.target_kind = Some(target.target_kind);
        message.share_instance_id = Some(target.share_instance_id);
        message.controller_capabilities = controller_capabilities(target.target_kind);
        return Ok(());
    }
    crate::windows_compositor::compositor_list_windows()
        .await
        .iter()
        .any(|window| window.window_id == window_id && window.owner_identity == owner_identity)
        .then_some(())
        .ok_or_else(|| format!("remote window {window_id} is not open"))
}

/// Host-side notification that an authorized remote-control input for a local
/// share has arrived — briefly raises that share's capture/publish cadence so
/// the receiver stays responsive while the target reacts (see the boost in
/// the session frame pump).
pub(crate) fn note_remote_input(window_id: u32) {
    crate::session::boost_share_fps(window_id, crate::session::RC_FPS_BOOST_WINDOW);
}

pub(crate) fn controller_capabilities(
    target_kind: RemoteControlTargetKind,
) -> Vec<RemoteControlCapability> {
    match target_kind {
        RemoteControlTargetKind::Window => vec![
            RemoteControlCapability::LegacyControl,
            RemoteControlCapability::DiscretePointerV1,
            RemoteControlCapability::DiscreteScrollV1,
            RemoteControlCapability::WindowLocalPointer,
            RemoteControlCapability::GlobalKeyboard,
            RemoteControlCapability::UiaInvoke,
            RemoteControlCapability::UnicodeText,
        ],
        RemoteControlTargetKind::Display => vec![
            RemoteControlCapability::LegacyControl,
            RemoteControlCapability::DiscretePointerV1,
            RemoteControlCapability::GlobalKeyboard,
            RemoteControlCapability::UnicodeText,
            RemoteControlCapability::DiscreteScrollV1,
        ],
        RemoteControlTargetKind::Unknown => Vec::new(),
    }
}

fn window_capabilities(calculator: bool) -> Vec<RemoteControlCapability> {
    let mut capabilities = vec![
        RemoteControlCapability::DiscretePointerV1,
        RemoteControlCapability::DiscreteScrollV1,
        RemoteControlCapability::GlobalKeyboard,
        RemoteControlCapability::UnicodeText,
    ];
    if calculator {
        capabilities.extend([
            RemoteControlCapability::WindowLocalPointer,
            RemoteControlCapability::UiaInvoke,
        ]);
    }
    capabilities
}

pub(crate) fn host_capabilities(
    window_id: u32,
    target_kind: RemoteControlTargetKind,
) -> Vec<RemoteControlCapability> {
    let Ok(target) = crate::windows_capture_target::resolve(window_id) else {
        return Vec::new();
    };
    match (target_kind, target.kind()) {
        (RemoteControlTargetKind::Window, TargetKind::Window) => {
            let hwnd = HWND(target.raw_handle() as *mut _);
            // Wheel is the generic cursor-preserving PostMessageW route; UIA
            // scroll is no longer selected, so it is not advertised.
            window_capabilities(calculator_app_pid(hwnd).is_some())
        }
        (RemoteControlTargetKind::Display, TargetKind::Display)
        | (RemoteControlTargetKind::Display, TargetKind::Window)
            if crate::region_window::resolve(window_id).is_some() =>
        {
            vec![
                RemoteControlCapability::DiscretePointerV1,
                RemoteControlCapability::GlobalKeyboard,
                RemoteControlCapability::UnicodeText,
                RemoteControlCapability::DiscreteScrollV1,
            ]
        }
        (RemoteControlTargetKind::Display, TargetKind::Display) => vec![
            RemoteControlCapability::DiscretePointerV1,
            RemoteControlCapability::GlobalKeyboard,
            RemoteControlCapability::UnicodeText,
            RemoteControlCapability::DiscreteScrollV1,
        ],
        _ => Vec::new(),
    }
}

pub(crate) fn validate_host_request(
    state: &crate::session::SessionState,
    message: &RemoteControlMessage,
) -> Result<(), String> {
    let target_kind = message
        .target_kind
        .ok_or_else(|| "controller upgrade required".to_string())?;
    let share_instance_id = message
        .share_instance_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "controller upgrade required".to_string())?;
    let target = state
        .control_target_snapshot(message.window_id, share_instance_id)
        .ok_or_else(|| "target share instance is unavailable".to_string())?;
    let actual_kind = match target.kind {
        TargetKind::Window => RemoteControlTargetKind::Window,
        TargetKind::Display => RemoteControlTargetKind::Display,
    };
    if target_kind != actual_kind {
        return Err("target kind does not match the active share".to_string());
    }
    let supported = host_capabilities(message.window_id, target_kind);
    if supported.is_empty() {
        return Err("target application is outside the supported Windows envelope".to_string());
    }
    if !message
        .controller_capabilities
        .iter()
        .any(|capability| supported.contains(capability))
    {
        return Err("controller and host have no compatible control route".to_string());
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControllerStatusEffect {
    Activate,
    Feedback,
    Terminate,
}

pub(crate) fn controller_status_effect(status: &str) -> ControllerStatusEffect {
    match status {
        "active" => ControllerStatusEffect::Activate,
        "stopped" | "disabled" => ControllerStatusEffect::Terminate,
        _ => ControllerStatusEffect::Feedback,
    }
}

pub(crate) fn record_controller_status(message: &RemoteControlMessage, status: &str) -> bool {
    let engine = crate::remote_control_core::remote_control_engine();
    match controller_status_effect(status) {
        ControllerStatusEffect::Terminate => {
            engine.remove_controller_grant(message.window_id, &message.controller_id);
            clear_pending_controller_operations(message.window_id, Some(&message.controller_id));
            return true;
        }
        ControllerStatusEffect::Feedback => return true,
        ControllerStatusEffect::Activate => {}
    }

    let has_negotiated_envelope = message.target_kind.is_some()
        || message.share_instance_id.is_some()
        || message.control_session_id.is_some()
        || message.result_capability.is_some()
        || !message.host_capabilities.is_empty();
    if !has_negotiated_envelope {
        // Mixed-version compatibility: a legacy active status may still carry
        // the v1 grant token, but none of the negotiated v2 fields.
        return true;
    }

    let (Some(target_kind), Some(share_instance_id), Some(control_session_id), Some(grant_token)) = (
        message.target_kind,
        message.share_instance_id.clone(),
        message.control_session_id.clone(),
        message.grant_token.clone(),
    ) else {
        return false;
    };
    let Some(target) = crate::windows_compositor::remote_control_target_metadata(
        message.window_id,
        Some(&message.controller_id),
    ) else {
        return false;
    };
    let reliable_result = message
        .result_capability
        .as_ref()
        .is_some_and(|capability| capability.version == 2 && !capability.retry_enabled);
    if target.target_kind != target_kind
        || target.share_instance_id != share_instance_id
        || message.host_capabilities.is_empty()
        || !reliable_result
    {
        return false;
    }

    clear_pending_controller_operations(message.window_id, Some(&message.controller_id));
    engine.install_controller_grant(
        message.window_id,
        message.controller_id.clone(),
        crate::remote_control_core::ControllerGrantEnvelope {
            target_kind,
            share_instance_id,
            control_session_id,
            grant_token,
            full_pointer: false,
            host_capabilities: message.host_capabilities.clone(),
            next_input_seq: 1,
        },
    );
    true
}

pub(crate) fn prepare_outbound_input(message: &mut RemoteControlMessage) -> Result<bool, String> {
    let Some(grant) = crate::remote_control_core::remote_control_engine()
        .next_controller_grant(message.window_id, &message.target_user_id)
    else {
        // A legacy host does not advertise a capable grant. Preserve the
        // existing v1 controller behavior for mixed-version rooms.
        return Ok(true);
    };
    message.target_kind = Some(grant.target_kind);
    message.share_instance_id = Some(grant.share_instance_id.clone());
    message.host_capabilities = grant.host_capabilities.clone();
    message.grant_token = Some(grant.grant_token.clone());

    let supported = match (message.message_type, message.action) {
        // macOS parity, v2-granted sessions: only the ATOMIC `Click` is a
        // v2-discrete pointer op; the lossy Move/Down/Up drag stream is the
        // legacy-shaped path (grant token only) — authorized by the mirrored
        // legacy grant below, replayed by the global route, and what makes
        // drag-to-select and reliable double-clicks work.
        (
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move | RemoteControlAction::Down),
        ) => grant
            .host_capabilities
            .contains(&RemoteControlCapability::DiscretePointerV1),
        (RemoteControlType::Pointer, Some(RemoteControlAction::Up)) => {
            if grant
                .host_capabilities
                .contains(&RemoteControlCapability::UiaInvoke)
            {
                message.action = Some(RemoteControlAction::Click);
                message.buttons = Some(0);
                true
            } else {
                grant
                    .host_capabilities
                    .contains(&RemoteControlCapability::DiscretePointerV1)
            }
        }
        (RemoteControlType::Pointer, Some(RemoteControlAction::Click)) => {
            grant
                .host_capabilities
                .contains(&RemoteControlCapability::UiaInvoke)
                || grant
                    .host_capabilities
                    .contains(&RemoteControlCapability::DiscretePointerV1)
        }
        (RemoteControlType::Wheel, _) => {
            grant
                .host_capabilities
                .contains(&RemoteControlCapability::DiscreteScrollV1)
                || grant
                    .host_capabilities
                    .contains(&RemoteControlCapability::UiaScroll)
        }
        (RemoteControlType::Key, _) => grant
            .host_capabilities
            .contains(&RemoteControlCapability::GlobalKeyboard),
        (RemoteControlType::Text, _) => grant
            .host_capabilities
            .contains(&RemoteControlCapability::UnicodeText),
        _ => false,
    };
    if !supported {
        return Ok(false);
    }

    // macOS parity: only atomic `Click` — plus Wheel/Key/Text — is a v2-discrete
    // (reliable, terminal-result) op; the engine admits exactly those
    // (`v2_discrete_admission`'s eligible set). Plain Pointer Move/Down/Up are
    // the lossy ordered stream: send them WITHOUT a v2 admission envelope, so
    // the host replays them without demanding a terminal result. Stamping a
    // Down/Up as discrete makes the host reject the admission and surface
    // "Remote input was not accepted." on the controller.
    let v2_discrete = matches!(
        (message.message_type, message.action),
        (RemoteControlType::Pointer, Some(RemoteControlAction::Click))
            | (RemoteControlType::Wheel, _)
            | (
                RemoteControlType::Key,
                Some(RemoteControlAction::Down | RemoteControlAction::Up)
            )
            | (RemoteControlType::Text, _)
    );
    if !v2_discrete {
        // macOS parity, lossy drag stream: legacy-shaped — grant token only.
        // Clearing the v2 markers (target kind, share instance, host caps)
        // keeps the host from classifying this as a v2 attempt (and rejecting
        // it as an incomplete envelope); the mirrored legacy grant on the host
        // authorizes it.
        message.target_kind = None;
        message.share_instance_id = None;
        message.host_capabilities = Vec::new();
        return Ok(true);
    }

    message.control_session_id = Some(grant.control_session_id.clone());
    message.input_id = Some(random_token());
    message.input_seq = Some(grant.next_input_seq);
    message.operation_fingerprint_version = Some(1);
    let admission = crate::remote_control_core::DiscreteAdmission {
        controller_id: message.controller_id.clone(),
        window_id: message.window_id,
        target_kind: message.target_kind,
        share_instance_id: message.share_instance_id.clone(),
        control_session_id: grant.control_session_id.clone(),
        input_id: message.input_id.clone().expect("input id assigned"),
        input_seq: grant.next_input_seq,
        operation_fingerprint: String::new(),
    };
    let operation_fingerprint =
        crate::remote_control_core::canonical_operation_fingerprint(message, &admission);
    message.operation_fingerprint = Some(operation_fingerprint.clone());
    remember_pending_controller_operation(message, &grant, operation_fingerprint);
    Ok(true)
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable for remote-control token");
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

fn remember_pending_controller_operation(
    message: &RemoteControlMessage,
    grant: &crate::remote_control_core::ControllerGrantEnvelope,
    operation_fingerprint: String,
) {
    let Some(input_id) = message.input_id.clone() else {
        return;
    };
    let now = Instant::now();
    let mut pending = PENDING_CONTROLLER_OPERATIONS.lock_unpoisoned();
    pending.retain(|_, operation| operation.expires_at > now);
    if pending.len() >= MAX_PENDING_OPERATIONS {
        if let Some(oldest_key) = pending
            .iter()
            .min_by_key(|(_, operation)| operation.expires_at)
            .map(|(key, _)| key.clone())
        {
            pending.remove(&oldest_key);
        }
    }
    pending.insert(
        (message.window_id, input_id),
        PendingControllerOperation {
            owner_identity: message.target_user_id.clone(),
            target_kind: grant.target_kind,
            share_instance_id: grant.share_instance_id.clone(),
            control_session_id: grant.control_session_id.clone(),
            input_seq: grant.next_input_seq,
            operation_fingerprint,
            expires_at: now + PENDING_OPERATION_TTL,
        },
    );
}

pub(crate) fn accept_controller_result(message: &RemoteControlMessage) -> bool {
    let (
        Some(input_id),
        Some(input_seq),
        Some(target_kind),
        Some(share_instance_id),
        Some(control_session_id),
        Some(operation_fingerprint),
    ) = (
        message.input_id.as_deref(),
        message.input_seq,
        message.target_kind,
        message.share_instance_id.as_deref(),
        message.control_session_id.as_deref(),
        message.operation_fingerprint.as_deref(),
    )
    else {
        return false;
    };
    let key = (message.window_id, input_id.to_string());
    let now = Instant::now();
    let mut pending = PENDING_CONTROLLER_OPERATIONS.lock_unpoisoned();
    pending.retain(|_, operation| operation.expires_at > now);
    let Some(operation) = pending.get(&key) else {
        return false;
    };
    let matches_pending = operation.owner_identity == message.controller_id
        && operation.target_kind == target_kind
        && operation.share_instance_id == share_instance_id
        && operation.control_session_id == control_session_id
        && operation.input_seq == input_seq
        && operation.operation_fingerprint == operation_fingerprint;
    if !matches_pending {
        return false;
    }
    let Some(grant) = crate::remote_control_core::remote_control_engine()
        .controller_grant(message.window_id, &message.controller_id)
    else {
        return false;
    };
    if grant.target_kind != target_kind
        || grant.share_instance_id != share_instance_id
        || grant.control_session_id != control_session_id
    {
        return false;
    }
    pending.remove(&key);
    true
}

pub(crate) fn clear_pending_controller_operations(window_id: u32, owner_identity: Option<&str>) {
    PENDING_CONTROLLER_OPERATIONS
        .lock_unpoisoned()
        .retain(|(stored_window_id, _), operation| {
            *stored_window_id != window_id
                || owner_identity.is_some_and(|owner| operation.owner_identity != owner)
        });
}

pub(crate) fn clear_all_pending_controller_operations() {
    PENDING_CONTROLLER_OPERATIONS.lock_unpoisoned().clear();
}

pub(crate) fn clear_pending_controller_operations_for_owner(owner_identity: &str) {
    PENDING_CONTROLLER_OPERATIONS
        .lock_unpoisoned()
        .retain(|_, operation| operation.owner_identity != owner_identity);
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WheelAxis {
    Horizontal,
    Vertical,
}

static WHEEL_REMAINDERS: LazyLock<Mutex<HashMap<(u32, String, WheelAxis), f64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn available() -> bool {
    input_desktop_is_default()
}

pub(crate) fn prompt_accessibility() -> bool {
    false
}

pub(crate) fn clear_pid(_pid: i32) {}

pub(crate) fn clear_window(window_id: u32) {
    release_synthetic_keys(|stored_window_id, _| stored_window_id == window_id);
    WHEEL_REMAINDERS
        .lock_unpoisoned()
        .retain(|(stored_window_id, _, _), _| *stored_window_id != window_id);
}

pub(crate) fn clear_controller(window_id: u32, controller_id: &str) {
    release_synthetic_keys(|stored_window_id, stored_controller_id| {
        stored_window_id == window_id && stored_controller_id == controller_id
    });
    WHEEL_REMAINDERS
        .lock_unpoisoned()
        .retain(|(stored_window_id, stored_controller_id, _), _| {
            *stored_window_id != window_id || stored_controller_id != controller_id
        });
}

pub(crate) fn window_is_on_screen(raw_handle: usize) -> bool {
    let hwnd = HWND(raw_handle as *mut _);
    unsafe {
        IsWindow(Some(hwnd)).as_bool()
            && IsWindowVisible(hwnd).as_bool()
            && !IsIconic(hwnd).as_bool()
    }
}

pub(crate) fn clear_all() {
    release_synthetic_keys(|_, _| true);
    WHEEL_REMAINDERS.lock_unpoisoned().clear();
}

fn replay_target_kind_matches(
    message_target_kind: Option<RemoteControlTargetKind>,
    expected_kind: RemoteControlTargetKind,
) -> bool {
    message_target_kind.is_none() || message_target_kind == Some(expected_kind)
}

pub(crate) fn replay(
    message: &RemoteControlMessage,
    frame: WindowFrame,
    target_pid: Option<i32>,
) -> Result<(), String> {
    if !input_desktop_is_default() {
        return Err("secure desktop active".to_string());
    }
    let target = crate::windows_capture_target::resolve(message.window_id)
        .map_err(|_| "target capture instance is stale".to_string())?;
    let expected_kind = if crate::region_window::resolve(message.window_id).is_some() {
        RemoteControlTargetKind::Display
    } else {
        match target.kind() {
            TargetKind::Window => RemoteControlTargetKind::Window,
            TargetKind::Display => RemoteControlTargetKind::Display,
        }
    };
    if !replay_target_kind_matches(message.target_kind, expected_kind) {
        return Err("target kind does not match active share".to_string());
    }
    let validation_pid = if expected_kind == RemoteControlTargetKind::Window {
        target_pid
    } else {
        None
    };
    ensure_current_target(message, expected_kind, validation_pid)?;

    if crate::region_window::resolve(message.window_id).is_some() {
        // The selector HWND identifies the share, but input is display-like
        // and must be mapped through the published ROI frame.
        replay_display(message, frame)
    } else {
        match target.kind() {
            TargetKind::Window => {
                let hwnd = HWND(target.raw_handle() as *mut _);
                validate_window(hwnd, target.owner_process_id(), target_pid)?;
                let fresh_frame =
                    crate::platform::windows::window_frame_for_raw(target.raw_handle())
                        .ok_or_else(|| "target window geometry is unavailable".to_string())?;
                replay_window(message, fresh_frame, hwnd, target.owner_process_id())
            }
            TargetKind::Display => replay_display(message, frame),
        }
    }
}

fn ensure_current_target(
    message: &RemoteControlMessage,
    expected_kind: RemoteControlTargetKind,
    expected_pid: Option<i32>,
) -> Result<(), String> {
    if crate::remote_control::injection_was_cancelled()
        || !crate::remote_control::input_authority_is_current(message)
    {
        return Err("remote input superseded".to_string());
    }
    let target = crate::windows_capture_target::resolve(message.window_id)
        .map_err(|_| "target capture instance is stale".to_string())?;
    let current_kind = if crate::region_window::resolve(message.window_id).is_some() {
        RemoteControlTargetKind::Display
    } else {
        match target.kind() {
            TargetKind::Window => RemoteControlTargetKind::Window,
            TargetKind::Display => RemoteControlTargetKind::Display,
        }
    };
    if current_kind != expected_kind
        || expected_pid.is_some_and(|pid| {
            pid > 0 && target.owner_process_id() != 0 && target.owner_process_id() != pid as u32
        })
    {
        return Err("target capture instance changed".to_string());
    }
    Ok(())
}

/// Whether a target window runs at a higher Windows integrity level than
/// Petal. Lower-integrity overlays cannot reliably stay above or receive
/// hit-tests over elevated windows (UIPI), so callers must fail closed rather
/// than create an interactive rectangle detached from the target.
pub(crate) fn window_integrity_exceeds_petal(hwnd: HWND) -> Result<bool, String> {
    if hwnd.0.is_null() {
        return Err("target window handle is null".to_string());
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return Err("target process unavailable".to_string());
    }
    Ok(process_integrity_level(pid)? > process_integrity_level_current()?)
}

fn validate_window(hwnd: HWND, pid: u32, expected_pid: Option<i32>) -> Result<(), String> {
    if unsafe {
        !IsWindow(Some(hwnd)).as_bool()
            || !IsWindowVisible(hwnd).as_bool()
            || IsIconic(hwnd).as_bool()
    } {
        return Err("target window is unavailable".to_string());
    }
    let mut current_pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut current_pid)) };
    if current_pid == 0
        || current_pid != pid
        || expected_pid.is_some_and(|expected| expected > 0 && expected as u32 != current_pid)
    {
        return Err("target HWND/PID identity changed".to_string());
    }
    if window_integrity_exceeds_petal(hwnd)? {
        return Err("target integrity level exceeds Petal".to_string());
    }
    Ok(())
}

fn replay_window(
    message: &RemoteControlMessage,
    frame: WindowFrame,
    hwnd: HWND,
    pid: u32,
) -> Result<(), String> {
    match message.message_type {
        RemoteControlType::Pointer => {
            if share_mode(message.window_id) == RemoteControlMode::CursorPreserving {
                // Shipped full-control still routes Calculator atomic clicks
                // through UIA; cursor-preserving wraps the discrete global
                // gesture in a save/restore (and keeps the UIA shortcut).
                replay_window_pointer_cursor_preserving(message, frame, hwnd, pid)
            } else if message.action == Some(RemoteControlAction::Click)
                && message.button.unwrap_or(0) == 0
                && calculator_app_pid(hwnd).is_some()
            {
                replay_uia_invoke(
                    message,
                    frame,
                    calculator_app_pid(hwnd).expect("checked above"),
                )
            } else {
                replay_window_pointer_global(message, frame, hwnd, pid)
            }
        }
        RemoteControlType::Wheel => {
            // Exactly one cursor-preserving route for window shares: a
            // targeted `PostMessageW(WM_MOUSEWHEEL/WM_MOUSEHWHEEL)` to the
            // validated same-root/same-PID destination. It never focuses,
            // moves the sharer's cursor, or falls through to `SendInput`/UIA/
            // Notepad-message routes.
            replay_window_wheel_postmessage(message, frame, hwnd, pid)
        }
        RemoteControlType::Key => {
            // Host decides per target kind (macOS sends keys as-is): a WINDOW
            // share excludes keys that hijack the sharer's shell (Win/Meta) or
            // cannot be injected (unmappable code); a display share allows
            // them. Refusal is an explicit per-op failure, never a session demote.
            validate_window_key_code(message.code.as_deref().unwrap_or(""))?;
            if share_mode(message.window_id) == RemoteControlMode::CursorPreserving {
                replay_window_key_cursor_preserving(message, hwnd, pid)
            } else {
                with_global_input(|| {
                    focus_and_verify(hwnd, pid)?;
                    reject_secure_field(hwnd, pid)?;
                    focus_and_verify(hwnd, pid)?;
                    ensure_current_target(
                        message,
                        RemoteControlTargetKind::Window,
                        Some(pid as i32),
                    )?;
                    replay_key(message)
                })
            }
        }
        RemoteControlType::Text => {
            if share_mode(message.window_id) == RemoteControlMode::CursorPreserving {
                replay_window_text_cursor_preserving(message, hwnd, pid)
            } else {
                with_global_input(|| {
                    focus_and_verify(hwnd, pid)?;
                    reject_secure_field(hwnd, pid)?;
                    focus_and_verify(hwnd, pid)?;
                    ensure_current_target(
                        message,
                        RemoteControlTargetKind::Window,
                        Some(pid as i32),
                    )?;
                    replay_unicode_text(message.text.as_deref().unwrap_or(""))
                })
            }
        }
        _ => Err("unsupported Windows window-control operation".to_string()),
    }
}

fn replay_display(
    message: &RemoteControlMessage,
    frame: WindowFrame,
) -> Result<(), String> {
    match message.message_type {
        RemoteControlType::Key => with_global_input(|| {
            let pid = foreground_pid()?;
            validate_foreground_integrity(pid)?;
            reject_secure_field(unsafe { GetForegroundWindow() }, pid)?;
            validate_foreground_integrity(pid)?;
            ensure_current_target(message, RemoteControlTargetKind::Display, None)?;
            replay_key(message)
        }),
        RemoteControlType::Text => with_global_input(|| {
            let pid = foreground_pid()?;
            validate_foreground_integrity(pid)?;
            reject_secure_field(unsafe { GetForegroundWindow() }, pid)?;
            validate_foreground_integrity(pid)?;
            ensure_current_target(message, RemoteControlTargetKind::Display, None)?;
            replay_unicode_text(message.text.as_deref().unwrap_or(""))
        }),
        RemoteControlType::Pointer => replay_display_pointer(message, frame),
        RemoteControlType::Wheel => replay_display_wheel(message, frame),
        _ => Err("unsupported Windows display-control operation".to_string()),
    }
}

/// Display-share pointer: the global `SendInput` route, scoped to a point
/// inside the shared display. Display shares intentionally use the real system
/// cursor because Windows has no target HWND for monitor content.
fn replay_display_pointer(
    message: &RemoteControlMessage,
    frame: WindowFrame,
) -> Result<(), String> {
    with_global_input(|| {
        let (px, py) = normalized_point(frame, message)?;
        let pid = foreground_pid()?;
        validate_foreground_integrity(pid)?;
        ensure_current_target(message, RemoteControlTargetKind::Display, None)?;
        if unsafe { SetCursorPos(px, py) }.is_err() {
            return Err("SetCursorPos failed for display pointer".to_string());
        }
        let (down_flag, up_flag) = mouse_button_flags(message.button)?;
        match message.action {
            Some(RemoteControlAction::Move) | Some(RemoteControlAction::Unknown) | None => Ok(()),
            Some(RemoteControlAction::Down) => submit_inputs(&[mouse_input(down_flag, 0)], true),
            Some(RemoteControlAction::Up) => submit_inputs(&[mouse_input(up_flag, 0)], true),
            Some(RemoteControlAction::Click) => {
                submit_inputs(&[mouse_input(down_flag, 0), mouse_input(up_flag, 0)], true)
            }
        }
    })
}

/// Display-share wheel: the marked global `SendInput` route, scoped to a
/// point inside the shared display. Moves the system cursor to the aimed
/// point (display shares already use the global cursor for keyboard/pointer
/// semantics), serialized through the global input lane. This is the ONE
/// place global cursor movement is expected — window shares never reach it.
fn replay_display_wheel(
    message: &RemoteControlMessage,
    frame: WindowFrame,
) -> Result<(), String> {
    with_global_input(|| {
        let (px, py) = normalized_point(frame, message)?;
        // The point must lie inside the shared display's frame.
        if px < frame.x
            || px >= frame.x.saturating_add(frame.width)
            || py < frame.y
            || py >= frame.y.saturating_add(frame.height)
        {
            return Err("wheel point is outside the shared display".to_string());
        }
        let pid = foreground_pid()?;
        validate_foreground_integrity(pid)?;
        if unsafe { SetCursorPos(px, py) }.is_err() {
            return Err("SetCursorPos failed for display wheel".to_string());
        }
        ensure_current_target(message, RemoteControlTargetKind::Display, None)?;

        let mut inputs = Vec::new();
        for (axis, delta, flags) in [
            (WheelAxis::Horizontal, message.delta_x, MOUSEEVENTF_HWHEEL),
            (WheelAxis::Vertical, message.delta_y, MOUSEEVENTF_WHEEL),
        ] {
            let key = (message.window_id, message.controller_id.clone(), axis);
            let previous = WHEEL_REMAINDERS.lock_unpoisoned().get(&key).copied();
            let (units, remainder) = wheel_delta_units(
                previous.unwrap_or_default(),
                message.delta_mode,
                delta,
                axis,
            )?;
            if units != 0 {
                inputs.push(mouse_input(flags, units as u32));
            }
            if crate::remote_control::input_authority_is_current(message) {
                let mut remainders = WHEEL_REMAINDERS.lock_unpoisoned();
                if remainders.get(&key).copied() == previous {
                    remainders.insert(key, remainder);
                }
            }
        }
        submit_inputs(&inputs, true)?;
        Ok(())
    })
}

/// One synthetic mouse `INPUT` record (Petal marker stamped so the low-level
/// hooks can distinguish our events; marker proven safe at 32-bit width).
fn mouse_input(flags: MOUSE_EVENT_FLAGS, mouse_data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: SYNTHETIC_INPUT_MARKER,
            },
        },
    }
}

/// Window-pointer (global) route: move the system cursor to the point the
/// controller aimed at (normalized against the shared window's outer frame,
/// matching what the receiver displays) and forward click down/up via
/// SendInput into the foreground-verified shared window. The sharer's cursor
/// stays where the controller aimed so the visible tag and the real cursor
/// coincide — this is what lets remote clicks land (and focus for typing).
/// Serialized with keyboard replay through the global input lane.
fn replay_window_pointer_global(
    message: &RemoteControlMessage,
    frame: WindowFrame,
    hwnd: HWND,
    pid: u32,
) -> Result<(), String> {
    with_global_input(|| {
        let (px, py) = normalized_point(frame, message)?;
        // Focus/raise FIRST on gesture start. WGC window capture composites
        // the shared window as if unoccluded, so the controller legitimately
        // clicks pixels that a foreign window covers on the host -- refusing
        // those with "occluded" felt like dead input (019: terminal shares
        // next to an always-on-top Petal View). Standard remote-desktop
        // semantics: the click raises the shared window, then applies.
        // Focus/verify once when the gesture starts; held-button drag MOVE
        // events must not re-steal foreground per move.
        if matches!(
            message.action,
            Some(RemoteControlAction::Down | RemoteControlAction::Click)
        ) {
            focus_and_verify(hwnd, pid)?;
        }
        // A release must always be submitted once Petal pressed the button,
        // even if another window covered the point mid-gesture. All actions
        // that can begin or redirect a gesture remain bound to visible target
        // pixels (the target-owned click-through overlay counts as the target).
        // Validated AFTER the raise so a just-raised target owns its point,
        // and re-checked here against any race between raise and replay.
        if message.action != Some(RemoteControlAction::Up) {
            validate_pointer_point(message.window_id, hwnd, px, py)?;
        }
        // A release is never DROPPED once a button was pressed (a covered
        // point mid-gesture must still submit the Up). But warping the cursor
        // to a point whose Down was REFUSED is a side effect that contradicts
        // the occlusion no-op: if the point is covered, submit the Up at the
        // current cursor (wherever the accepted Down landed) and skip the
        // warp, instead of teleporting the sharer's cursor onto the covering
        // window (the 011B "tag jumped to Notepad on click" leak).
        let point_owned = message.action != Some(RemoteControlAction::Up)
            || validate_pointer_point(message.window_id, hwnd, px, py).is_ok();
        if point_owned && unsafe { SetCursorPos(px, py) }.is_err() {
            return Err("SetCursorPos failed for remote pointer".to_string());
        }
        // macOS parity: honor the requested button (the engine models
        // Left/Middle/Right; macOS replays all three). Windows dropped
        // right/middle because it only ever emitted the left button.
        let (down_flag, up_flag) = mouse_button_flags(message.button)?;
        match message.action {
            Some(RemoteControlAction::Move) | Some(RemoteControlAction::Unknown) | None => Ok(()),
            Some(RemoteControlAction::Down) => submit_inputs(&[mouse_input(down_flag, 0)], true),
            Some(RemoteControlAction::Up) => submit_inputs(&[mouse_input(up_flag, 0)], true),
            Some(RemoteControlAction::Click) => {
                submit_inputs(&[mouse_input(down_flag, 0), mouse_input(up_flag, 0)], true)
            }
        }
    })
}

/// Host-side control-mode registry (Step 3D.2). The sharer selects the mode
/// per share; the replay path reads it by `window_id` to decide the delivery
/// route. Display-like Petal View regions default to full control; ordinary
/// HWND shares default to cursor-preserving when absent.
pub(crate) fn set_share_mode(window_id: u32, mode: RemoteControlMode) {
    SHARE_MODES.lock_unpoisoned().insert(window_id, mode);
}

pub(crate) fn share_mode(window_id: u32) -> RemoteControlMode {
    SHARE_MODES
        .lock_unpoisoned()
        .get(&window_id)
        .copied()
        .or_else(|| {
            crate::region_window::resolve(window_id).map(|_| RemoteControlMode::FullControl)
        })
        .unwrap_or(RemoteControlMode::CursorPreserving)
}

pub(crate) fn clear_share_mode(window_id: u32) {
    SHARE_MODES.lock_unpoisoned().remove(&window_id);
    clear_controller_focus_targets_for_window(window_id);
}

fn cursor_position() -> Option<(i32, i32)> {
    let mut point = windows::Win32::Foundation::POINT::default();
    unsafe { GetCursorPos(&mut point).ok()? };
    Some((point.x, point.y))
}

/// Restore the cursor only if it is still within tolerance of where Petal last
/// posted it. If a human moved it mid-gesture, abandoning the restore is
/// strictly better than yanking the pointer out from under them (macOS
/// parity). Unreadable cursor => do not guess, do not warp.
fn cursor_restore_is_safe(takeover: &CursorTakeover) -> bool {
    match cursor_position() {
        Some(current) => {
            (current.0 - takeover.last_posted.0).abs() <= CURSOR_TAKEOVER_TOLERANCE_PX
                && (current.1 - takeover.last_posted.1).abs() <= CURSOR_TAKEOVER_TOLERANCE_PX
        }
        None => false,
    }
}

/// Save the host cursor position at the start of a cursor-preserving gesture
/// (an atomic Click or a Down starting a drag).
fn save_cursor_takeover(window_id: u32) {
    let Some(saved) = cursor_position() else {
        return;
    };
    CURSOR_TAKEOVERS.lock_unpoisoned().insert(
        window_id,
        CursorTakeover {
            saved,
            last_posted: saved,
        },
    );
}

/// Record the last coordinate we actually posted so the restore knows whether
/// the cursor is still where we left it.
fn note_cursor_posted(window_id: u32, posted: (i32, i32)) {
    if let Some(t) = CURSOR_TAKEOVERS.lock_unpoisoned().get_mut(&window_id) {
        t.last_posted = posted;
    }
}

/// End the takeover and restore the host's cursor. Called once per gesture
/// (after an atomic Click or after the Up that ends a drag), never per event.
fn end_cursor_takeover(window_id: u32) {
    let entry = CURSOR_TAKEOVERS.lock_unpoisoned().remove(&window_id);
    let Some(takeover) = entry else { return };
    if !cursor_restore_is_safe(&takeover) {
        log::info!(
            "windows-remote-control: cursor restore skipped window_id={window_id} reason=host-moved-cursor"
        );
        return;
    }
    if unsafe { SetCursorPos(takeover.saved.0, takeover.saved.1) }.is_err() {
        log::warn!("windows-remote-control: cursor restore failed window_id={window_id}");
    }
}

/// Per-controller focus target for cursor-preserving WINDOW keyboard (Step
/// 3D.2 / parallel keyboard). Maps `(window_id, controller_id)` -> the HWND
/// that controller is addressing (its share target / last accepted cursor-
/// preserving click). Keys are message-injected here WITHOUT stealing the
/// sharer's foreground focus, enabling parallel input. Display shares never
/// use this (they keep global foreground injection for simplicity).
static CONTROLLER_FOCUS_TARGETS: LazyLock<Mutex<HashMap<(u32, String), isize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn set_controller_focus_target(window_id: u32, controller_id: &str, hwnd: HWND) {
    CONTROLLER_FOCUS_TARGETS
        .lock_unpoisoned()
        .insert((window_id, controller_id.to_string()), hwnd.0 as isize);
}

fn controller_focus_target(window_id: u32, controller_id: &str, default: HWND) -> HWND {
    let raw = CONTROLLER_FOCUS_TARGETS
        .lock_unpoisoned()
        .get(&(window_id, controller_id.to_string()))
        .copied()
        .unwrap_or(default.0 as isize);
    HWND(raw as *mut _)
}

fn clear_controller_focus_targets_for_window(window_id: u32) {
    CONTROLLER_FOCUS_TARGETS
        .lock_unpoisoned()
        .retain(|(wid, _), _| *wid != window_id);
}

/// Resolve the descendant CONTROL to route cursor-preserving parallel keyboard
/// to, given the shared top-level window and the SCREEN point of the
/// controller's cursor. `WindowFromPoint` at that point returns the deepest
/// window under it (the edit/text control for Notepad, the console input for
/// cmd.exe); we accept it only if it is a descendant of the shared window so a
/// covering window is never typed into. Falls back to the top-level window.
///
/// This is keyboard ADDRESS resolution and is intentionally distinct from the
/// wheel's z-order-independent path: parallel keyboard aims at the control the
/// controller clicked to type in.
fn resolve_keyboard_target(hwnd: HWND, point: (i32, i32)) -> HWND {
    let at = unsafe {
        WindowFromPoint(windows::Win32::Foundation::POINT {
            x: point.0,
            y: point.1,
        })
    };
    if !at.is_invalid() && at.0 != hwnd.0 && unsafe { IsChild(hwnd, at) }.as_bool() {
        at
    } else {
        hwnd
    }
}

/// The UTF-16 code unit for `WM_CHAR` when the controller's `key` is exactly
/// one printable character; None for control/navigation names (Enter, Shift,
/// ArrowLeft), multi-codepoint text, or empty input.
fn single_wm_char(key: Option<&str>) -> Option<u32> {
    let key = key?;
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let c0 = ch as u32;
    if c0 == 0 || c0 < 0x20 {
        return None;
    }
    // For a single BMP char the code point is the UTF-16 code unit WM_CHAR
    // expects. Surrogate-plane input is a multi-codepoint string and already
    // rejected above.
    Some(c0)
}

/// Whether the given window is currently the foreground (focused) top-level
/// window. When the per-controller target IS focused, real global `SendInput`
/// is reliable AND non-intrusive (the sharer is already working there, so we
/// are not stealing their focus) — and the cursor-preserving click that got us
/// here already focused it. Otherwise we must NOT `SendInput` into whatever
/// other app the sharer is using; we post messages instead.
fn target_is_foreground(target: HWND) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() {
        return false;
    }
    let root = unsafe { GetAncestor(target, GA_ROOTOWNER) }.0 as isize;
    root == foreground.0 as isize || target.0 as isize == foreground.0 as isize
}

/// Keep the message-injected key path best-effort and bounded: a key press is
/// posted as `WM_KEYDOWN` (+ `WM_CHAR` when the controller produced a
/// printable character) and the matching `WM_KEYUP`, addressed to the target
/// control (the edit/text child, not the top-level window -- see 06B/
/// resolved-child approach). We report OS submission, never application; the
/// user judges the result. Delivery failures are logged: a swallowed post
/// otherwise looks like "remote typing did nothing" with zero diagnostics.
fn post_key_message(
    target: HWND,
    code: &str,
    key_up: bool,
    alt_held: bool,
    char_hint: Option<&str>,
) {
    let Some(spec) = key_spec(code) else { return };
    let mut lparam: u32 = (spec.scan as u32 & 0xff) << 16;
    if spec.extended {
        lparam |= 0x0100_0000;
    }
    let lparam_up = lparam | 0xC000_0000; // prev-state + transition (key up)
    let down_msg = if alt_held { WM_SYSKEYDOWN } else { WM_KEYDOWN };
    let up_msg = if alt_held { WM_SYSKEYUP } else { WM_KEYUP };
    let post = |msg: u32, wparam: usize, lparam: isize| {
        let sent = unsafe { PostMessageW(Some(target), msg, WPARAM(wparam), LPARAM(lparam)) };
        if sent.is_err() {
            log::warn!("remote-control: PostMessageW failed for key '{code}' up={key_up}: {sent:?}");
        }
    };
    if key_up {
        post(up_msg, spec.vk as usize, lparam_up as isize);
    } else {
        post(down_msg, spec.vk as usize, lparam as isize);
        // A printable character the controller produced follows the key-down
        // with WM_CHAR so text entry actually lands (Notepad/cmd insert on
        // WM_CHAR; the 06B child-addressed route).
        if let Some(unit) = single_wm_char(char_hint) {
            post(WM_CHAR, unit as usize, 0);
        }
    }
}

/// Best-effort parallel text injection: `PostMessageW(WM_CHAR)` each UTF-16
/// code unit of the provided text to the per-controller focus target. No
/// foreground change; may or may not be consumed by the target app.
fn post_text_message(target: HWND, text: &str) {
    for unit in text.encode_utf16() {
        unsafe {
            let _ = PostMessageW(
                Some(target),
                WM_CHAR,
                windows::Win32::Foundation::WPARAM(unit as usize),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    }
}

fn is_normalized_clipboard_shortcut(message: &RemoteControlMessage) -> bool {
    message.target_kind == Some(RemoteControlTargetKind::Window)
        && message.share_instance_id.is_some()
        && message.control_session_id.is_none()
        && message.input_id.is_none()
        && message.input_seq.is_none()
        && message.operation_fingerprint_version.is_none()
        && message.operation_fingerprint.is_none()
        && message.modifiers.ctrl
        && !message.modifiers.meta
        && !message.modifiers.alt
        && !message.modifiers.shift
        && matches!(message.key.as_deref(), Some("c" | "C" | "v" | "V"))
}

/// Cursor-preserving WINDOW keyboard (Step 3D.2): route key events to the
/// per-controller focus target. When that target is already the foreground
/// window, real global input is used (reliable, non-intrusive); otherwise a
/// best-effort message injection is posted to the target so the sharer can
/// keep typing elsewhere (parallel keyboard). Display shares do not use this.
fn replay_window_key_cursor_preserving(
    message: &RemoteControlMessage,
    hwnd: HWND,
    pid: u32,
) -> Result<(), String> {
    let target = controller_focus_target(message.window_id, &message.controller_id, hwnd);
    // Legitimate safety gates: the target must still be the valid shared
    // window for this share instance. These are not "guessable" outcomes --
    // if they fail the target really is gone/changed, so we refuse.
    validate_window(target, pid, Some(pid as i32))?;
    ensure_current_target(message, RemoteControlTargetKind::Window, Some(pid as i32))?;
    if target_is_foreground(target) {
        // Target is focused: real global injection is reliable and does not
        // steal focus (the cursor-preserving click already focused it). The
        // secure-field check applies here because real input could reach a
        // password field.
        reject_secure_field(target, pid)?;
        with_global_input(|| replay_key(message))
    } else {
        // Best-effort parallel injection into a NON-focused target. We cannot
        // verify delivery or application, and the sharer's focus is elsewhere
        // (so foreground-based secure-field checks are meaningless here). Per
        // the trust model, Petal submits and lets the user judge the result --
        // it does NOT guess a success/failure status. An unmappable key is
        // skipped (nothing injected, no misleading status).
        let code = message.code.as_deref().unwrap_or("");
        let key_up = matches!(message.action, Some(RemoteControlAction::Up));
        if key_spec(code).is_some() {
            if is_normalized_clipboard_shortcut(message) {
                // The native controller suppresses the raw Ctrl+C/V pair, so
                // this message is the complete shortcut. The ordinary raw-key
                // route receives a separate modifier event and must not use
                // this branch or it would double-press Control.
                if key_up {
                    post_key_message(target, code, true, false, None);
                    post_key_message(target, "ControlLeft", true, false, None);
                } else {
                    post_key_message(target, "ControlLeft", false, false, None);
                    post_key_message(target, code, false, false, None);
                }
            } else {
                post_key_message(
                    target,
                    code,
                    key_up,
                    message.modifiers.alt,
                    if message.modifiers.ctrl
                        || message.modifiers.meta
                        || message.modifiers.shift
                    {
                        None
                    } else {
                        message.key.as_deref()
                    },
                );
            }
        }
        Ok(())
    }
}

/// Cursor-preserving WINDOW text (Step 3D.2): identical focus-target routing
/// as keys above -- real global input when the target is foreground, else
/// best-effort `WM_CHAR` posts into the target for parallel entry.
fn replay_window_text_cursor_preserving(
    message: &RemoteControlMessage,
    hwnd: HWND,
    pid: u32,
) -> Result<(), String> {
    let target = controller_focus_target(message.window_id, &message.controller_id, hwnd);
    validate_window(target, pid, Some(pid as i32))?;
    ensure_current_target(message, RemoteControlTargetKind::Window, Some(pid as i32))?;
    let text = message.text.as_deref().unwrap_or("");
    if target_is_foreground(target) {
        reject_secure_field(target, pid)?;
        with_global_input(|| replay_unicode_text(text))
    } else {
        post_text_message(target, text);
        Ok(())
    }
}

/// Cursor-preserving pointer route (Step 3D.2): reuses the shipped global
/// injector (`SetCursorPos` + serialized `SendInput`) for DISCRETE gestures,
/// then restores the host's cursor to its prior position -- guarded by
/// `cursor_restore_is_safe`, once per gesture, never yanking a host who moved
/// it. Continuous pointer tracking (a bare Move with no in-flight gesture) is
/// full-control semantics and refuses with a not-injectible error that drives
/// the user-initiated escalation affordance; it never silently falls back to
/// full control.
fn replay_window_pointer_cursor_preserving(
    message: &RemoteControlMessage,
    frame: WindowFrame,
    hwnd: HWND,
    pid: u32,
) -> Result<(), String> {
    // Calculator atomic click stays genuinely cursor-preserving via UIA with
    // no cursor movement at all (no restore needed).
    if message.action == Some(RemoteControlAction::Click)
        && message.button.unwrap_or(0) == 0
        && calculator_app_pid(hwnd).is_some()
    {
        return replay_uia_invoke(message, frame, calculator_app_pid(hwnd).expect("checked"));
    }
    // A bare Move with no in-flight gesture = hover tracking (full-control
    // semantics). Cursor-preserving has no hover-follow, so this is a SILENT
    // no-op -- NOT an error. Emitting a failure here would flood the
    // controller with a misleading "Input ignored" on every mouse move.
    if message.action == Some(RemoteControlAction::Move)
        && !CURSOR_TAKEOVERS
            .lock_unpoisoned()
            .contains_key(&message.window_id)
    {
        return Ok(());
    }
    // Save the host cursor at gesture start (atomic Click or a Down that
    // begins a drag).
    if matches!(
        message.action,
        Some(RemoteControlAction::Down) | Some(RemoteControlAction::Click)
    ) {
        save_cursor_takeover(message.window_id);
    }
    let posted = normalized_point(frame, message).ok();
    let result = replay_window_pointer_global(message, frame, hwnd, pid);
    if result.is_err() && matches!(
        message.action,
        Some(RemoteControlAction::Down) | Some(RemoteControlAction::Click)
    ) {
        // A rejected/failed gesture must not leave the takeover marker behind:
        // otherwise the next bare Move is mistaken for a drag and starts
        // moving the sharer's cursor as if full control were enabled.
        end_cursor_takeover(message.window_id);
    }
    if result.is_ok() {
        if let Some(posted) = posted {
            note_cursor_posted(message.window_id, posted);
        }
        // The controller addressed this window: resolve the actual control at
        // the click point (the edit/text child to type into) as its focus
        // target, so subsequent cursor-preserving keys route there (parallel
        // keyboard into a non-focused control).
        if matches!(
            message.action,
            Some(RemoteControlAction::Down) | Some(RemoteControlAction::Click)
        ) {
            let child = posted
                .map(|p| resolve_keyboard_target(hwnd, p))
                .unwrap_or(hwnd);
            set_controller_focus_target(message.window_id, &message.controller_id, child);
        }
    }
    // Restore once per gesture: after an atomic Click or after the Up ending a
    // drag. Never between a Down and its moves/Up (that would break the drag).
    if matches!(
        message.action,
        Some(RemoteControlAction::Up) | Some(RemoteControlAction::Click)
    ) {
        end_cursor_takeover(message.window_id);
    }
    result
}

/// Left/Middle/Right wire button (1=middle, 2=right, else left) -> the
/// corresponding SendInput down/up flags, matching the engine's
/// `RemoteControlButton` model and macOS's three-button replay.
fn validate_pointer_point(window_id: u32, hwnd: HWND, px: i32, py: i32) -> Result<(), String> {
    let target_root = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) }.0 as isize;
    let overlay_roots = crate::windows_share_overlay::hwnd_for_local_share(window_id);
    // Walk through THIS process's own chrome (hover pill, overlays, panel,
    // main window) — it floats over the shared window and must not read as an
    // occluder. A foreign window above the point is still a genuine "covered".
    let point_root = crate::platform::windows::root_window_at_skipping_self(
        (px as f64, py as f64),
        target_root,
        overlay_roots.as_slice(),
    )
    .ok_or_else(|| "pointer point is not on a visible window".to_string())?;
    pointer_root_matches_target(point_root, target_root, overlay_roots.as_slice())
        .then_some(())
        .ok_or_else(|| "pointer point belongs to another window".to_string())
}

fn pointer_root_matches_target(
    point_root: isize,
    target_root: isize,
    overlay_roots: &[isize],
) -> bool {
    point_root == target_root || overlay_roots.contains(&point_root)
}

fn mouse_button_flags(
    button: Option<i16>,
) -> Result<(MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS), String> {
    Ok(match crate::remote_control_core::button_from_wire(button) {
        crate::remote_control_core::RemoteControlButton::Left => {
            (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
        }
        crate::remote_control_core::RemoteControlButton::Right => {
            (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
        }
        crate::remote_control_core::RemoteControlButton::Middle => {
            (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP)
        }
    })
}

fn replay_uia_invoke(
    message: &RemoteControlMessage,
    frame: WindowFrame,
    pid: u32,
) -> Result<(), String> {
    if message.action != Some(RemoteControlAction::Click) {
        return Err("cursor-preserving window mode supports atomic click only".to_string());
    }
    let (x, y) = normalized_point(frame, message)?;
    with_uia(|uia| {
        let element = unsafe { uia.ElementFromPoint(POINT { x, y }) }
            .map_err(|error| format!("UIA point resolution failed: {error}"))?;
        ensure_uia_pid(&element, pid)?;
        let invoke: IUIAutomationInvokePattern =
            unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) }
                .map_err(|_| "UIA element is not invokable".to_string())?;
        ensure_current_target(message, RemoteControlTargetKind::Window, None)?;
        unsafe { invoke.Invoke() }.map_err(|error| format!("UIA invoke failed: {error}"))
    })
}

fn with_uia<T>(operation: impl FnOnce(&IUIAutomation) -> Result<T, String>) -> Result<T, String> {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let uia: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }
            .map_err(|error| format!("UI Automation unavailable: {error}"))?;
    operation(&uia)
}

fn ensure_uia_pid(element: &IUIAutomationElement, pid: u32) -> Result<(), String> {
    let element_pid = unsafe { element.CurrentProcessId() }
        .map_err(|error| format!("UIA process lookup failed: {error}"))?;
    (element_pid == pid as i32)
        .then_some(())
        .ok_or_else(|| "UIA element does not belong to the shared target".to_string())
}

fn ensure_uia_target(
    element: &IUIAutomationElement,
    target_hwnd: HWND,
    target_pid: u32,
) -> Result<(), String> {
    let element_pid = unsafe { element.CurrentProcessId() }
        .map_err(|error| format!("UIA process lookup failed: {error}"))?;
    if element_pid == target_pid as i32 {
        return Ok(());
    }
    let element_hwnd = unsafe { element.CurrentNativeWindowHandle() }
        .map_err(|error| format!("UIA native window lookup failed: {error}"))?;
    let same_root = !element_hwnd.0.is_null()
        && unsafe {
            GetAncestor(element_hwnd, GA_ROOTOWNER) == GetAncestor(target_hwnd, GA_ROOTOWNER)
        };
    if !same_root {
        return Err("UIA element does not belong to the shared target".to_string());
    }
    let element_pid = u32::try_from(element_pid)
        .map_err(|_| "UIA element process identity is invalid".to_string())?;
    if process_integrity_level(element_pid)? > process_integrity_level_current()? {
        return Err("focused element integrity level exceeds Petal".to_string());
    }
    Ok(())
}

fn trusted_uia_text_provider(framework_id: &str) -> bool {
    matches!(
        framework_id.to_ascii_lowercase().as_str(),
        "winform" | "wpf" | "xaml" | "chrome"
    )
}

fn reject_secure_field(target_hwnd: HWND, pid: u32) -> Result<(), String> {
    let uia_decision = with_uia(|uia| {
        let element = unsafe { uia.GetFocusedElement() }
            .map_err(|error| format!("UIA focused element unavailable: {error}"))?;
        ensure_uia_target(&element, target_hwnd, pid)?;
        let control_type = unsafe { element.CurrentControlType() }
            .map_err(|error| format!("UIA focused control type unavailable: {error}"))?;
        if control_type != UIA_EditControlTypeId && control_type != UIA_DocumentControlTypeId {
            return Ok(Some(()));
        }
        let is_password = unsafe { element.CurrentIsPassword() }
            .map_err(|error| format!("UIA password state unavailable: {error}"))?
            .as_bool();
        if is_password {
            return Err("secure text field refuses remote input".to_string());
        }
        let framework_id = unsafe { element.CurrentFrameworkId() }
            .map_err(|error| format!("UIA framework identity unavailable: {error}"))?
            .to_string();
        if trusted_uia_text_provider(&framework_id) {
            Ok(Some(()))
        } else if framework_id.eq_ignore_ascii_case("win32") {
            Ok(None)
        } else {
            Err("unsupported UIA text provider cannot prove secure-field state".to_string())
        }
    })?;
    if uia_decision.is_some() {
        return Ok(());
    }

    let foreground = unsafe { GetForegroundWindow() };
    let mut foreground_pid = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(foreground, Some(&mut foreground_pid)) };
    if foreground_pid != pid || thread_id == 0 {
        return Err("focused target does not belong to the shared app".to_string());
    }
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(thread_id, &mut info) }
        .map_err(|error| format!("focused Win32 control unavailable: {error}"))?;
    let focus = info.hwndFocus;
    let mut focus_pid = 0;
    unsafe { GetWindowThreadProcessId(focus, Some(&mut focus_pid)) };
    let same_root =
        unsafe { GetAncestor(focus, GA_ROOTOWNER) == GetAncestor(target_hwnd, GA_ROOTOWNER) };
    if focus.0.is_null() || focus_pid != pid || !same_root {
        return Err("focused target is not an approved editable field".to_string());
    }
    let class_name = window_class_name(focus);
    let style = unsafe { GetWindowLongPtrW(focus, GWL_STYLE) };
    const ES_PASSWORD: isize = 0x0020;
    if style & ES_PASSWORD != 0 {
        return Err("secure text field refuses remote input".to_string());
    }
    if !trusted_win32_text_control(&class_name, style) {
        return Err("focused target is not an approved editable field".to_string());
    }
    Ok(())
}

fn trusted_win32_text_control(class_name: &str, style: isize) -> bool {
    const ES_PASSWORD: isize = 0x0020;
    let class_name = class_name.to_ascii_lowercase();
    style & ES_PASSWORD == 0 && (class_name == "edit" || class_name.starts_with("richedit"))
}

fn foreground_belongs_to(hwnd: HWND, pid: u32) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return false;
    }
    let mut foreground_pid = 0;
    unsafe { GetWindowThreadProcessId(foreground, Some(&mut foreground_pid)) };
    if foreground_pid != pid {
        return false;
    }
    let expected_root = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    let foreground_root = unsafe { GetAncestor(foreground, GA_ROOTOWNER) };
    foreground == hwnd || foreground_root == expected_root
}

fn focus_and_verify(hwnd: HWND, pid: u32) -> Result<(), String> {
    if foreground_belongs_to(hwnd, pid) {
        return Ok(());
    }
    let _ = unsafe { SetForegroundWindow(hwnd) };
    let deadline = Instant::now() + FOREGROUND_WAIT;
    while Instant::now() < deadline {
        if foreground_belongs_to(hwnd, pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("Windows refused target foreground activation".to_string())
}

fn foreground_pid() -> Result<u32, String> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return Err("no foreground window".to_string());
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid > 0)
        .then_some(pid)
        .ok_or_else(|| "foreground process unavailable".to_string())
}

/// Display-like shares (full displays and Petal View regions) accept remote
/// input regardless of which host window has focus: multiple display-like
/// shares are controlled concurrently, so any focus/geometry coupling would
/// arbitrarily reject all but the last-focused one (016B/017B). The one
/// invariant that remains is security, not focus: never inject while an
/// elevated (higher-integrity) window owns the host foreground.
fn validate_foreground_integrity(pid: u32) -> Result<(), String> {
    if process_integrity_level(pid)? > process_integrity_level_current()? {
        return Err("foreground integrity level exceeds Petal".to_string());
    }
    Ok(())
}

fn window_class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let count = unsafe { GetClassNameW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..count])
}

fn process_exe_name(pid: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = vec![0u16; 1024];
    let mut size = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    unsafe { CloseHandle(handle) }.ok();
    result.ok()?;
    buffer.truncate(size as usize);
    String::from_utf16_lossy(&buffer)
        .rsplit(['\\', '/'])
        .next()
        .map(ToOwned::to_owned)
}

struct ChildProcessSearch {
    executable: &'static str,
    pid: Option<u32>,
}

unsafe extern "system" fn find_child_process(hwnd: HWND, state: LPARAM) -> BOOL {
    let state = unsafe { &mut *(state.0 as *mut ChildProcessSearch) };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if process_exe_name(pid).is_some_and(|name| name.eq_ignore_ascii_case(state.executable)) {
        state.pid = Some(pid);
        BOOL(0)
    } else {
        BOOL(1)
    }
}

fn calculator_app_pid(hwnd: HWND) -> Option<u32> {
    if window_class_name(hwnd) != "ApplicationFrameWindow" {
        return None;
    }
    let mut search = ChildProcessSearch {
        executable: "CalculatorApp.exe",
        pid: None,
    };
    let _ = unsafe {
        EnumChildWindows(
            Some(hwnd),
            Some(find_child_process),
            LPARAM((&mut search as *mut ChildProcessSearch) as isize),
        )
    };
    search.pid
}

fn with_global_input<T>(operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let _lane = GLOBAL_INPUT_COORDINATOR.lock_unpoisoned();
    operation()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeySpec {
    vk: u16,
    scan: u16,
    extended: bool,
    scan_code: bool,
}

impl KeySpec {
    const fn scan(vk: u16, scan: u16, extended: bool) -> Self {
        Self {
            vk,
            scan,
            extended,
            scan_code: true,
        }
    }

    const fn virtual_key(vk: u16) -> Self {
        Self {
            vk,
            scan: 0,
            extended: false,
            scan_code: false,
        }
    }
}

fn key_input(code: &str, key_up: bool) -> Result<(u16, INPUT), String> {
    let spec = key_spec(code).ok_or_else(|| "unsupported Windows key".to_string())?;
    let mut flags = if key_up {
        KEYEVENTF_KEYUP
    } else {
        Default::default()
    };
    if spec.scan_code {
        flags |= KEYEVENTF_SCANCODE;
    }
    if spec.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    Ok((
        spec.vk,
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: if spec.scan_code {
                        VIRTUAL_KEY(0)
                    } else {
                        VIRTUAL_KEY(spec.vk)
                    },
                    wScan: spec.scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: SYNTHETIC_INPUT_MARKER,
                },
            },
        },
    ))
}

fn replay_key(message: &RemoteControlMessage) -> Result<(), String> {
    let code = message.code.as_deref().unwrap_or("");
    let key = (
        message.window_id,
        message.controller_id.clone(),
        code.to_string(),
    );
    match message.action {
        Some(RemoteControlAction::Down) => {
            // macOS parity: hold the message's modifiers (flags on the event)
            // by pressing the modifier key when the controller did not forward
            // it separately and the host isn't physically holding it.
            let altgr_held = code == "AltRight"
                || SYNTHETIC_KEYS.lock_unpoisoned().contains(&(
                    message.window_id,
                    message.controller_id.clone(),
                    "AltRight".to_string(),
                ));
            let mut synthesized: Vec<(u32, String, String)> = Vec::new();
            for (name, held) in [
                ("ShiftLeft", message.modifiers.shift),
                ("ControlLeft", message.modifiers.ctrl),
                ("AltLeft", message.modifiers.alt),
            ] {
                if !held {
                    continue;
                }
                // The controller forwards modifier keys as their own key events
                // (nowhere does it filter them out of onKey), so when this
                // event's `code` IS the modifier being synthesized we must not
                // press a second one -- the forwarded event below already
                // presses it. Also match the mirrored right-side code
                // (ShiftRight while message.modifiers.shift) so a right-side
                // modifier isn't double-pressed via a synthesized left one.
                if code_is_same_modifier(code, name)
                    || altgr_supplies_modifier(
                        altgr_held,
                        message.modifiers.ctrl,
                        message.modifiers.alt,
                        name,
                    )
                {
                    continue;
                }
                let modifier_key = (
                    message.window_id,
                    message.controller_id.clone(),
                    name.to_string(),
                );
                let side_codes = modifier_side_codes(name);
                let already_synthetic = SYNTHETIC_KEYS.lock_unpoisoned().iter().any(
                    |(window_id, controller_id, code)| {
                        *window_id == message.window_id
                            && controller_id == &message.controller_id
                            && side_codes.contains(&code.as_str())
                    },
                );
                if already_synthetic
                    || side_codes.iter().any(|code| {
                        key_spec(code)
                            .is_some_and(|spec| unsafe { GetAsyncKeyState(spec.vk as i32) < 0 })
                    })
                {
                    continue;
                }
                let (_, modifier_input) = key_input(name, false)?;
                send_inputs(&[modifier_input])?;
                SYNTHETIC_KEYS
                    .lock_unpoisoned()
                    .insert(modifier_key.clone());
                synthesized.push(modifier_key);
            }

            let (vk, input) = key_input(code, false)?;
            let already_synthetic = SYNTHETIC_KEYS.lock_unpoisoned().contains(&key);
            // Record the synthesized-modifier ledger BEFORE the refusal check
            // so the refusal path can release the modifiers we just pressed --
            // otherwise they'd stay physically held and stuck (the ledger
            // insert used to live only after this point, making
            // release_synthetic_modifiers a silent no-op on refusal).
            if !synthesized.is_empty() {
                SYNTHETIC_MODIFIERS
                    .lock_unpoisoned()
                    .insert(key.clone(), synthesized);
            }
            if !already_synthetic && unsafe { GetAsyncKeyState(vk as i32) } < 0 {
                // Don't leave half-pressed modifiers behind on a refusal.
                release_synthetic_modifiers(&key);
                return Err("physical host key is held".to_string());
            }
            send_inputs(&[input])?;
            SYNTHETIC_KEYS.lock_unpoisoned().insert(key);
            Ok(())
        }
        Some(RemoteControlAction::Up) => {
            if !SYNTHETIC_KEYS.lock_unpoisoned().remove(&key) {
                return Ok(());
            }
            let (_, input) = key_input(code, true)?;
            if let Err(error) = send_inputs(&[input]) {
                SYNTHETIC_KEYS.lock_unpoisoned().insert(key);
                return Err(error);
            }
            release_synthetic_modifiers(&key);
            Ok(())
        }
        _ => Err("unsupported key action".to_string()),
    }
}

/// True when an event's `code` is itself (either side of) the modifier the
/// synthesis loop is about to press — the controller already forwarded it as
/// its own key-down, so synthesizing a second press would double-press it
/// (and a single release would leave it stuck).
fn altgr_supplies_modifier(altgr_held: bool, ctrl: bool, alt: bool, name: &str) -> bool {
    altgr_held && ctrl && alt && matches!(name, "ControlLeft" | "AltLeft")
}

fn modifier_side_codes(name: &str) -> &'static [&'static str] {
    match name {
        "ShiftLeft" => &["ShiftLeft", "ShiftRight"],
        "ControlLeft" => &["ControlLeft", "ControlRight"],
        "AltLeft" => &["AltLeft", "AltRight"],
        _ => &[],
    }
}

fn code_is_same_modifier(code: &str, name: &str) -> bool {
    let sides = modifier_side_codes(name);
    if sides.is_empty() {
        code == name
    } else {
        sides.contains(&code)
    }
}

/// Release the modifiers the host synthesized for this keypress (only those we
/// pressed — never a physically held or separately-forwarded modifier).
fn release_synthetic_modifiers(key: &(u32, String, String)) {
    let modifiers = SYNTHETIC_MODIFIERS.lock_unpoisoned().remove(key);
    let Some(modifiers) = modifiers else {
        return;
    };
    for modifier_key in modifiers {
        if SYNTHETIC_KEYS.lock_unpoisoned().remove(&modifier_key) {
            let code = modifier_key.2.clone();
            if let Ok((_, up)) = key_input(&code, true) {
                let _ = send_inputs(&[up]);
            }
        }
    }
}

fn release_synthetic_keys(mut matches_key: impl FnMut(u32, &str) -> bool) {
    let _lane = GLOBAL_INPUT_COORDINATOR.lock_unpoisoned();
    let mut keys = SYNTHETIC_KEYS.lock_unpoisoned();
    let releasing = keys
        .iter()
        .filter(|(window_id, controller_id, _)| matches_key(*window_id, controller_id))
        .cloned()
        .collect::<Vec<_>>();
    let inputs = releasing
        .iter()
        .filter_map(|(_, _, code)| key_input(code, true).ok().map(|(_, input)| input))
        .collect::<Vec<_>>();
    if submit_inputs(&inputs, false).is_ok() {
        for key in releasing {
            keys.remove(&key);
        }
    }
    // Drain the modifier ledger for any main key that just released, so
    // entries recorded against a cleared gesture don't linger (self-healing
    // on next use, but avoid unbounded growth across many clear cycles).
    let mut modifiers = SYNTHETIC_MODIFIERS.lock_unpoisoned();
    modifiers.retain(|key, _| !matches_key(key.0, &key.1));
}

fn unicode_text_inputs(text: &str) -> Vec<INPUT> {
    let mut inputs = Vec::with_capacity(text.encode_utf16().count() * 2);
    for code_unit in text.encode_utf16() {
        for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: code_unit,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: SYNTHETIC_INPUT_MARKER,
                    },
                },
            });
        }
    }
    inputs
}

fn replay_unicode_text(text: &str) -> Result<(), String> {
    send_inputs(&unicode_text_inputs(text))
}

fn normalized_point(
    frame: WindowFrame,
    message: &RemoteControlMessage,
) -> Result<(i32, i32), String> {
    let x = message
        .x
        .ok_or_else(|| "pointer x is missing".to_string())?;
    let y = message
        .y
        .ok_or_else(|| "pointer y is missing".to_string())?;
    if !x.is_finite() || !y.is_finite() || frame.width == 0 || frame.height == 0 {
        return Err("pointer coordinates are invalid".to_string());
    }
    let px =
        frame.x as i64 + (x.clamp(0.0, 1.0) * frame.width.saturating_sub(1) as f64).round() as i64;
    let py =
        frame.y as i64 + (y.clamp(0.0, 1.0) * frame.height.saturating_sub(1) as f64).round() as i64;
    Ok((
        px.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        py.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    ))
}

fn send_inputs(inputs: &[INPUT]) -> Result<(), String> {
    submit_inputs(inputs, true)
}

fn submit_inputs(inputs: &[INPUT], honor_cancellation: bool) -> Result<(), String> {
    if inputs.is_empty() {
        return Ok(());
    }
    if honor_cancellation && crate::remote_control::injection_was_cancelled() {
        return Err("remote input superseded".to_string());
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent == inputs.len() as u32 {
        Ok(())
    } else {
        Err(format!(
            "SendInput submitted {sent}/{} records",
            inputs.len()
        ))
    }
}

fn wheel_delta_units(
    remainder: f64,
    delta_mode: Option<u8>,
    delta: Option<f64>,
    axis: WheelAxis,
) -> Result<(i32, f64), String> {
    let delta = delta.unwrap_or_default();
    if !delta.is_finite() {
        return Err("wheel delta is invalid".to_string());
    }
    let units_per_delta = match delta_mode.unwrap_or(0) {
        0 => 3.0,
        1 => 120.0,
        2 => 360.0,
        _ => return Err("unsupported Windows wheel delta mode".to_string()),
    };
    let direction = if axis == WheelAxis::Vertical {
        -1.0
    } else {
        1.0
    };
    let accumulated = remainder + delta * units_per_delta * direction;
    if !accumulated.is_finite() {
        return Err("wheel delta is out of range".to_string());
    }
    let units = accumulated.trunc().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
    Ok((units, accumulated - units as f64))
}

// ---------------------------------------------------------------------------
// Cursor-preserving window wheel (PostMessageW) route
// ---------------------------------------------------------------------------

/// Encode the signed wheel WPARAM: the high word carries the signed delta
/// (WHEEL_DELTA multiples), the low word the modifier/button state.
/// `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL` both use the same layout. The MK_* bits
/// are the winuser.h constants (MK_CONTROL=0x0008, MK_SHIFT=0x0004; a wheel
/// event never carries button state since no button is held).
fn wheel_wparam(
    delta: i32,
    modifiers: &crate::remote_control_core::RemoteControlModifiers,
) -> usize {
    const MK_CONTROL: u16 = 0x0008;
    const MK_SHIFT: u16 = 0x0004;
    let mut keys = 0u16;
    if modifiers.ctrl {
        keys |= MK_CONTROL;
    }
    if modifiers.shift {
        keys |= MK_SHIFT;
    }
    (((delta as u32) & 0xFFFF) << 16 | keys as u32) as usize
}

/// Encode the screen-coordinate LPARAM as signed 16-bit x/y. Refuses
/// coordinates that fall outside the signed-16-bit representable range rather
/// than truncating into another location (negative virtual desktops are
/// representable and preserved).
fn wheel_lparam(x: i32, y: i32) -> Result<isize, String> {
    if !(i16::MIN as i32..=i16::MAX as i32).contains(&x)
        || !(i16::MIN as i32..=i16::MAX as i32).contains(&y)
    {
        return Err("wheel target point is outside representable screen coordinates".to_string());
    }
    Ok((((y as i16 as u16) as usize) << 16 | (x as i16 as u16) as usize) as isize)
}

/// Replay a window wheel via `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL` delivered to the
/// TARGET window's scrollable descendant at the aimed point — no focus, no
/// cursor move, no `SendInput`, no fallback.
///
/// ID-addressed injection is deliberately NOT gated on occlusion: the
/// `window_id` names the exact window that must receive the input, and the
/// message goes to that window regardless of what is on top of it. So a
/// window covered by another window still scrolls — which is the whole point
/// of a cursor-preserving window route.
///
/// Destination policy (the 006B regression fix): find the target's own
/// SCROLLABLE descendant under the cursor and deliver to it, so the wheel
/// lands on the control that can actually scroll — classic `WS_*SCROLL`
/// controls and Chromium render widgets alike. Resolution order:
///
/// 1. The aim point must be inside the TARGET window's own client area
///    (`window_contains_point`, via `ScreenToClient` + `GetClientRect`). This
///    is the ONLY point check — it is z-order-independent, so a window
///    covered by another window on the sharer's desktop still accepts the
///    wheel (occlusion must never block ID-addressed injection). We
///    deliberately do NOT use `WindowFromPoint` here, which returns the
///    topmost window and would refuse a covered-but-targeted aim.
/// 2. `scrollable_child_at_point` — the first descendant under the cursor
///    with scroll range (`GetScrollInfo`) or `WS_VSCROLL`/`WS_HSCROLL`.
///    Delivering here reaches the actual editor/render widget; delivering to
///    the top level relies on app forwarding that Win11 Notepad does not do.
/// 3. Fallback: the top-level target itself (a destination fallback — still
///    the same message-delivery mechanism, never `SendInput`).
///
/// Delivery is `SendMessageTimeoutW` (synchronous, `SMTO_ABORTIFHUNG` +
/// 250ms), matching how Chromium itself redirects `WM_MOUSEWHEEL` between its
/// own windows (`ui/base/win/mouse_wheel_util.cc`): a bare `PostMessageW` to
/// Chrome's legacy `Chrome_RenderWidgetHostHWND` is ignored when the browser
/// is covered/not foreground, while a direct window-proc invocation works on
/// background windows. Zero return means delivery failed; a non-zero return
/// proves only delivery to the window proc, not application effect — callers
/// report `submitted`, never `applied`.
fn replay_window_wheel_postmessage(
    message: &RemoteControlMessage,
    frame: WindowFrame,
    hwnd: HWND,
    pid: u32,
) -> Result<(), String> {
    with_global_input(|| {
        let (px, py) = normalized_point(frame, message)?;
        // The aim must be inside the TARGET window's own client area. This is
        // deliberately NOT `WindowFromPoint` (which returns the topmost
        // window and would reject a covered-but-targeted aim): the wheel is
        // addressed to this window by ID, so a window on top of it on the
        // sharer's desktop neither blocks nor redirects the post.
        if crate::platform::windows::window_contains_point(hwnd, (px as f64, py as f64)).is_none() {
            return Err("wheel target point is not on the shared window".to_string());
        }
        ensure_current_target(message, RemoteControlTargetKind::Window, Some(pid as i32))?;

        // Post to the target's scrollable descendant under the cursor, or to
        // the target itself if none exists.
        let destination =
            crate::platform::windows::scrollable_child_at_point(hwnd, (px as f64, py as f64))
                .unwrap_or(hwnd);

        // The LPARAM is in SCREEN coordinates per the WM_MOUSEWHEEL/HWHEEL
        // contract.
        let lparam = wheel_lparam(px, py)?;
        let mut posted_any = false;
        for (axis, delta, msg_id) in [
            (WheelAxis::Horizontal, message.delta_x, WM_MOUSEHWHEEL),
            (WheelAxis::Vertical, message.delta_y, WM_MOUSEWHEEL),
        ] {
            let key = (message.window_id, message.controller_id.clone(), axis);
            let previous = WHEEL_REMAINDERS.lock_unpoisoned().get(&key).copied();
            let (units, remainder) = wheel_delta_units(
                previous.unwrap_or_default(),
                message.delta_mode,
                delta,
                axis,
            )?;
            if units != 0 {
                let wparam = wheel_wparam(units, &message.modifiers);
                // Deliver synchronously with a timeout, matching how Chromium
                // itself redirects WM_MOUSEWHEEL between its own windows
                // (ui/base/win/mouse_wheel_util.cc uses SendMessage). A real
                // wheel reaches the widget through the OS routing to the
                // focused window; a bare PostMessage to Chrome's legacy
                // Chrome_RenderWidgetHostHWND is ignored when the browser is
                // covered/not foreground (the 008B/009B failure).
                // SendMessageTimeoutW invokes the target window proc directly
                // and works on background windows; SMTO_ABORTIFHUNG + 250ms
                // caps a hung app. Zero return = failure to deliver; success
                // proves only delivery to the window proc, never application
                // effect (report `submitted`).
                let result = unsafe {
                    SendMessageTimeoutW(
                        destination,
                        msg_id,
                        WPARAM(wparam),
                        LPARAM(lparam),
                        SMTO_ABORTIFHUNG,
                        250,
                        None,
                    )
                };
                if result.0 == 0 {
                    return Err("Windows did not deliver the wheel message".to_string());
                }
                posted_any = true;
            }
            if crate::remote_control::input_authority_is_current(message) {
                let mut remainders = WHEEL_REMAINDERS.lock_unpoisoned();
                if remainders.get(&key).copied() == previous {
                    remainders.insert(key, remainder);
                }
            }
        }
        if !posted_any {
            return Ok(()); // sub-unit delta: nothing to post, nothing failed
        }
        Ok(())
    })
}

fn dom_code_to_virtual_key(code: &str) -> Option<u16> {
    key_spec(code).map(|spec| spec.vk)
}

fn validate_window_key_code(code: &str) -> Result<(), String> {
    if matches!(code, "MetaLeft" | "MetaRight") {
        return Err("unsupported Windows shell key for a window share".to_string());
    }
    key_spec(code)
        .map(|_| ())
        .ok_or_else(|| format!("unsupported Windows key code '{code}'"))
}

fn key_spec(code: &str) -> Option<KeySpec> {
    let spec = match code {
        "Escape" => KeySpec::scan(0x1b, 0x01, false),
        "Digit1" => KeySpec::scan(0x31, 0x02, false),
        "Digit2" => KeySpec::scan(0x32, 0x03, false),
        "Digit3" => KeySpec::scan(0x33, 0x04, false),
        "Digit4" => KeySpec::scan(0x34, 0x05, false),
        "Digit5" => KeySpec::scan(0x35, 0x06, false),
        "Digit6" => KeySpec::scan(0x36, 0x07, false),
        "Digit7" => KeySpec::scan(0x37, 0x08, false),
        "Digit8" => KeySpec::scan(0x38, 0x09, false),
        "Digit9" => KeySpec::scan(0x39, 0x0a, false),
        "Digit0" => KeySpec::scan(0x30, 0x0b, false),
        "Minus" => KeySpec::scan(0xbd, 0x0c, false),
        "Equal" => KeySpec::scan(0xbb, 0x0d, false),
        "Backspace" => KeySpec::scan(0x08, 0x0e, false),
        "Tab" => KeySpec::scan(0x09, 0x0f, false),
        "KeyQ" => KeySpec::scan(0x51, 0x10, false),
        "KeyW" => KeySpec::scan(0x57, 0x11, false),
        "KeyE" => KeySpec::scan(0x45, 0x12, false),
        "KeyR" => KeySpec::scan(0x52, 0x13, false),
        "KeyT" => KeySpec::scan(0x54, 0x14, false),
        "KeyY" => KeySpec::scan(0x59, 0x15, false),
        "KeyU" => KeySpec::scan(0x55, 0x16, false),
        "KeyI" => KeySpec::scan(0x49, 0x17, false),
        "KeyO" => KeySpec::scan(0x4f, 0x18, false),
        "KeyP" => KeySpec::scan(0x50, 0x19, false),
        "BracketLeft" => KeySpec::scan(0xdb, 0x1a, false),
        "BracketRight" => KeySpec::scan(0xdd, 0x1b, false),
        "Enter" => KeySpec::scan(0x0d, 0x1c, false),
        "ControlLeft" => KeySpec::scan(0xa2, 0x1d, false),
        "KeyA" => KeySpec::scan(0x41, 0x1e, false),
        "KeyS" => KeySpec::scan(0x53, 0x1f, false),
        "KeyD" => KeySpec::scan(0x44, 0x20, false),
        "KeyF" => KeySpec::scan(0x46, 0x21, false),
        "KeyG" => KeySpec::scan(0x47, 0x22, false),
        "KeyH" => KeySpec::scan(0x48, 0x23, false),
        "KeyJ" => KeySpec::scan(0x4a, 0x24, false),
        "KeyK" => KeySpec::scan(0x4b, 0x25, false),
        "KeyL" => KeySpec::scan(0x4c, 0x26, false),
        "Semicolon" => KeySpec::scan(0xba, 0x27, false),
        "Quote" => KeySpec::scan(0xde, 0x28, false),
        "Backquote" => KeySpec::scan(0xc0, 0x29, false),
        "ShiftLeft" => KeySpec::scan(0xa0, 0x2a, false),
        "Backslash" => KeySpec::scan(0xdc, 0x2b, false),
        "KeyZ" => KeySpec::scan(0x5a, 0x2c, false),
        "KeyX" => KeySpec::scan(0x58, 0x2d, false),
        "KeyC" => KeySpec::scan(0x43, 0x2e, false),
        "KeyV" => KeySpec::scan(0x56, 0x2f, false),
        "KeyB" => KeySpec::scan(0x42, 0x30, false),
        "KeyN" => KeySpec::scan(0x4e, 0x31, false),
        "KeyM" => KeySpec::scan(0x4d, 0x32, false),
        "Comma" => KeySpec::scan(0xbc, 0x33, false),
        "Period" => KeySpec::scan(0xbe, 0x34, false),
        "Slash" => KeySpec::scan(0xbf, 0x35, false),
        "ShiftRight" => KeySpec::scan(0xa1, 0x36, false),
        "NumpadMultiply" => KeySpec::scan(0x6a, 0x37, false),
        "AltLeft" => KeySpec::scan(0xa4, 0x38, false),
        "Space" => KeySpec::scan(0x20, 0x39, false),
        "CapsLock" => KeySpec::scan(0x14, 0x3a, false),
        "F1" => KeySpec::scan(0x70, 0x3b, false),
        "F2" => KeySpec::scan(0x71, 0x3c, false),
        "F3" => KeySpec::scan(0x72, 0x3d, false),
        "F4" => KeySpec::scan(0x73, 0x3e, false),
        "F5" => KeySpec::scan(0x74, 0x3f, false),
        "F6" => KeySpec::scan(0x75, 0x40, false),
        "F7" => KeySpec::scan(0x76, 0x41, false),
        "F8" => KeySpec::scan(0x77, 0x42, false),
        "F9" => KeySpec::scan(0x78, 0x43, false),
        "F10" => KeySpec::scan(0x79, 0x44, false),
        "NumLock" => KeySpec::scan(0x90, 0x45, true),
        "ScrollLock" => KeySpec::scan(0x91, 0x46, false),
        "Numpad7" => KeySpec::scan(0x67, 0x47, false),
        "Numpad8" => KeySpec::scan(0x68, 0x48, false),
        "Numpad9" => KeySpec::scan(0x69, 0x49, false),
        "NumpadSubtract" => KeySpec::scan(0x6d, 0x4a, false),
        "Numpad4" => KeySpec::scan(0x64, 0x4b, false),
        "Numpad5" => KeySpec::scan(0x65, 0x4c, false),
        "Numpad6" => KeySpec::scan(0x66, 0x4d, false),
        "NumpadAdd" => KeySpec::scan(0x6b, 0x4e, false),
        "Numpad1" => KeySpec::scan(0x61, 0x4f, false),
        "Numpad2" => KeySpec::scan(0x62, 0x50, false),
        "Numpad3" => KeySpec::scan(0x63, 0x51, false),
        "Numpad0" => KeySpec::scan(0x60, 0x52, false),
        "NumpadDecimal" => KeySpec::scan(0x6e, 0x53, false),
        "IntlBackslash" => KeySpec::scan(0xe2, 0x56, false),
        "F11" => KeySpec::scan(0x7a, 0x57, false),
        "F12" => KeySpec::scan(0x7b, 0x58, false),
        "NumpadEqual" => KeySpec::scan(0x92, 0x59, false),
        "KanaMode" => KeySpec::scan(0x15, 0x70, false),
        "Lang2" => KeySpec::scan(0x19, 0x71, false),
        "Lang1" => KeySpec::scan(0x15, 0x72, false),
        "IntlRo" => KeySpec::scan(0xe2, 0x73, false),
        "Lang5" => KeySpec::scan(0, 0x76, false),
        "Lang4" => KeySpec::scan(0, 0x77, false),
        "Lang3" => KeySpec::scan(0, 0x78, false),
        "Convert" => KeySpec::scan(0x1c, 0x79, false),
        "NonConvert" => KeySpec::scan(0x1d, 0x7b, false),
        "IntlYen" => KeySpec::scan(0xdc, 0x7d, false),
        "NumpadEnter" => KeySpec::scan(0x0d, 0x1c, true),
        "ControlRight" => KeySpec::scan(0xa3, 0x1d, true),
        "NumpadDivide" => KeySpec::scan(0x6f, 0x35, true),
        "PrintScreen" => KeySpec::scan(0x2c, 0x37, true),
        "AltRight" => KeySpec::scan(0xa5, 0x38, true),
        "Home" => KeySpec::scan(0x24, 0x47, true),
        "ArrowUp" => KeySpec::scan(0x26, 0x48, true),
        "PageUp" => KeySpec::scan(0x21, 0x49, true),
        "ArrowLeft" => KeySpec::scan(0x25, 0x4b, true),
        "ArrowRight" => KeySpec::scan(0x27, 0x4d, true),
        "End" => KeySpec::scan(0x23, 0x4f, true),
        "ArrowDown" => KeySpec::scan(0x28, 0x50, true),
        "PageDown" => KeySpec::scan(0x22, 0x51, true),
        "Insert" => KeySpec::scan(0x2d, 0x52, true),
        "Delete" => KeySpec::scan(0x2e, 0x53, true),
        "MetaLeft" => KeySpec::scan(0x5b, 0x5b, true),
        "MetaRight" => KeySpec::scan(0x5c, 0x5c, true),
        "ContextMenu" => KeySpec::scan(0x5d, 0x5d, true),
        "Pause" => KeySpec::virtual_key(0x13),
        "F13" => KeySpec::virtual_key(0x7c),
        "F14" => KeySpec::virtual_key(0x7d),
        "F15" => KeySpec::virtual_key(0x7e),
        "F16" => KeySpec::virtual_key(0x7f),
        "F17" => KeySpec::virtual_key(0x80),
        "F18" => KeySpec::virtual_key(0x81),
        "F19" => KeySpec::virtual_key(0x82),
        "F20" => KeySpec::virtual_key(0x83),
        "F21" => KeySpec::virtual_key(0x84),
        "F22" => KeySpec::virtual_key(0x85),
        "F23" => KeySpec::virtual_key(0x86),
        "F24" => KeySpec::virtual_key(0x87),
        _ => return None,
    };
    Some(spec)
}

fn process_integrity_level_current() -> Result<u32, String> {
    process_integrity_level_from_handle(unsafe { GetCurrentProcess() })
}

fn process_integrity_level(pid: u32) -> Result<u32, String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|error| format!("target process unavailable: {error}"))?;
    let result = process_integrity_level_from_handle(process);
    unsafe { CloseHandle(process) }.ok();
    result
}

fn process_integrity_level_from_handle(process: HANDLE) -> Result<u32, String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
        .map_err(|error| format!("process token unavailable: {error}"))?;
    let result = (|| {
        let mut required = 0;
        let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut required) };
        if required < size_of::<TOKEN_MANDATORY_LABEL>() as u32 {
            return Err("process integrity information unavailable".to_string());
        }
        let mut buffer = vec![0u8; required as usize];
        unsafe {
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(buffer.as_mut_ptr().cast()),
                required,
                &mut required,
            )
        }
        .map_err(|error| format!("process integrity query failed: {error}"))?;
        let label = unsafe { &*(buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
        let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) } as u32;
        if count == 0 {
            return Err("process integrity SID is malformed".to_string());
        }
        Ok(unsafe { *GetSidSubAuthority(label.Label.Sid, count - 1) })
    })();
    unsafe { CloseHandle(token) }.ok();
    result
}

fn input_desktop_is_default() -> bool {
    let Ok(desktop) =
        (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) })
    else {
        return false;
    };
    let result = (|| {
        let mut required = 0;
        let handle = HANDLE(desktop.0);
        let _ =
            unsafe { GetUserObjectInformationW(handle, UOI_NAME, None, 0, Some(&mut required)) };
        if required < 2 {
            return false;
        }
        let mut buffer = vec![0u16; (required as usize + 1) / 2];
        if unsafe {
            GetUserObjectInformationW(
                handle,
                UOI_NAME,
                Some(buffer.as_mut_ptr().cast()),
                (buffer.len() * 2) as u32,
                Some(&mut required),
            )
        }
        .is_err()
        {
            return false;
        }
        let end = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end]).eq_ignore_ascii_case("default")
    })();
    unsafe { CloseDesktop(desktop) }.ok();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock_unpoisoned()
    }

    #[test]
    fn legacy_pointer_envelope_is_allowed_for_display_target() {
        assert!(replay_target_kind_matches(
            None,
            RemoteControlTargetKind::Display
        ));
        assert!(!replay_target_kind_matches(
            Some(RemoteControlTargetKind::Window),
            RemoteControlTargetKind::Display
        ));
    }

    #[test]
    fn single_wm_char_derives_a_printable_utf16_unit_or_none() {
        // Printable single characters are injected as WM_CHAR.
        assert_eq!(single_wm_char(Some("x")), Some('x' as u32));
        assert_eq!(single_wm_char(Some("A")), Some('A' as u32));
        assert_eq!(single_wm_char(Some(" ")), Some(' ' as u32));
        assert_eq!(single_wm_char(Some("1")), Some('1' as u32));
        // Control / navigation / multi-codepoint names produce no WM_CHAR.
        assert_eq!(single_wm_char(Some("Enter")), None);
        assert_eq!(single_wm_char(Some("Shift")), None);
        assert_eq!(single_wm_char(Some("ArrowLeft")), None);
        assert_eq!(single_wm_char(Some("Control")), None);
        assert_eq!(single_wm_char(Some("\r")), None); // control char
        assert_eq!(single_wm_char(Some("")), None);
        assert_eq!(single_wm_char(None), None);
        // Multi-codepoint grapheme/cluster is not a single WM_CHAR unit.
        assert_eq!(single_wm_char(Some("ab")), None);
    }

    #[test]
    fn standard_keyboard_codes_have_windows_key_specs() {
        let codes = [
            "Escape",
            "F1",
            "F12",
            "F13",
            "F24",
            "PrintScreen",
            "ScrollLock",
            "Pause",
            "Backquote",
            "Digit1",
            "Digit0",
            "Minus",
            "Equal",
            "Backspace",
            "Tab",
            "KeyQ",
            "KeyP",
            "BracketLeft",
            "BracketRight",
            "Backslash",
            "CapsLock",
            "KeyA",
            "KeyL",
            "Semicolon",
            "Quote",
            "Enter",
            "ShiftLeft",
            "KeyZ",
            "KeyM",
            "Comma",
            "Period",
            "Slash",
            "ShiftRight",
            "ControlLeft",
            "MetaLeft",
            "AltLeft",
            "Space",
            "AltRight",
            "MetaRight",
            "ContextMenu",
            "ControlRight",
            "Insert",
            "Home",
            "PageUp",
            "Delete",
            "End",
            "PageDown",
            "ArrowRight",
            "ArrowLeft",
            "ArrowDown",
            "ArrowUp",
            "NumLock",
            "NumpadDivide",
            "NumpadMultiply",
            "NumpadSubtract",
            "NumpadAdd",
            "NumpadEnter",
            "Numpad1",
            "Numpad2",
            "Numpad3",
            "Numpad4",
            "Numpad5",
            "Numpad6",
            "Numpad7",
            "Numpad8",
            "Numpad9",
            "Numpad0",
            "NumpadDecimal",
            "NumpadEqual",
            "IntlBackslash",
            "IntlRo",
            "IntlYen",
            "Convert",
            "NonConvert",
            "KanaMode",
            "Lang1",
            "Lang2",
            "Lang3",
            "Lang4",
            "Lang5",
        ];
        for code in codes {
            assert!(key_spec(code).is_some(), "missing key spec for {code}");
        }
        for letter in b'A'..=b'Z' {
            let code = format!("Key{}", letter as char);
            assert!(key_spec(&code).is_some(), "missing key spec for {code}");
        }
        for digit in 0..=9 {
            assert!(key_spec(&format!("Digit{digit}")).is_some());
            assert!(key_spec(&format!("Numpad{digit}")).is_some());
        }
        for number in 1..=24 {
            assert!(key_spec(&format!("F{number}")).is_some());
        }
        assert!(key_spec("Unidentified").is_none());
    }

    #[test]
    fn side_specific_and_extended_keys_keep_their_physical_identity() {
        assert_eq!(
            key_spec("ShiftLeft"),
            Some(KeySpec::scan(0xa0, 0x2a, false))
        );
        assert_eq!(
            key_spec("ShiftRight"),
            Some(KeySpec::scan(0xa1, 0x36, false))
        );
        assert_eq!(
            key_spec("ControlRight"),
            Some(KeySpec::scan(0xa3, 0x1d, true))
        );
        assert_eq!(key_spec("AltRight"), Some(KeySpec::scan(0xa5, 0x38, true)));
        assert_eq!(
            key_spec("NumpadEnter"),
            Some(KeySpec::scan(0x0d, 0x1c, true))
        );
        assert_eq!(key_spec("ArrowLeft"), Some(KeySpec::scan(0x25, 0x4b, true)));
        assert_eq!(key_spec("Numpad4"), Some(KeySpec::scan(0x64, 0x4b, false)));
        assert_eq!(key_spec("IntlRo"), Some(KeySpec::scan(0xe2, 0x73, false)));
    }

    fn wheel_message(
        window_id: u32,
        controller_id: &str,
        delta: f64,
        mode: u8,
    ) -> RemoteControlMessage {
        serde_json::from_value(serde_json::json!({
            "v": 2,
            "kind": "wheel",
            "targetUserId": "host",
            "controllerId": controller_id,
            "windowId": window_id,
            "seq": 1,
            "deltaY": delta,
            "deltaMode": mode
        }))
        .unwrap()
    }

    #[test]
    fn generic_window_capabilities_do_not_depend_on_application_class() {
        assert_eq!(
            window_capabilities(false),
            vec![
                RemoteControlCapability::DiscretePointerV1,
                RemoteControlCapability::DiscreteScrollV1,
                RemoteControlCapability::GlobalKeyboard,
                RemoteControlCapability::UnicodeText,
            ]
        );
        assert_eq!(
            window_capabilities(true),
            vec![
                RemoteControlCapability::DiscretePointerV1,
                RemoteControlCapability::DiscreteScrollV1,
                RemoteControlCapability::GlobalKeyboard,
                RemoteControlCapability::UnicodeText,
                RemoteControlCapability::WindowLocalPointer,
                RemoteControlCapability::UiaInvoke,
            ]
        );
    }

    #[test]
    fn capabilities_match_the_implemented_routes() {
        assert_eq!(
            controller_capabilities(RemoteControlTargetKind::Window),
            vec![
                RemoteControlCapability::LegacyControl,
                RemoteControlCapability::DiscretePointerV1,
                RemoteControlCapability::DiscreteScrollV1,
                RemoteControlCapability::WindowLocalPointer,
                RemoteControlCapability::GlobalKeyboard,
                RemoteControlCapability::UiaInvoke,
                RemoteControlCapability::UnicodeText,
            ]
        );
        assert_eq!(
            controller_capabilities(RemoteControlTargetKind::Display),
            vec![
                RemoteControlCapability::LegacyControl,
                RemoteControlCapability::DiscretePointerV1,
                RemoteControlCapability::GlobalKeyboard,
                RemoteControlCapability::UnicodeText,
                RemoteControlCapability::DiscreteScrollV1,
            ]
        );
    }

    #[test]
    fn key_inputs_pin_scan_extended_and_virtual_key_flags() {
        let (_, right_control) = key_input("ControlRight", false).unwrap();
        let right_control = unsafe { right_control.Anonymous.ki };
        assert_eq!(right_control.wVk, VIRTUAL_KEY(0));
        assert_eq!(right_control.wScan, 0x1d);
        assert!(right_control.dwFlags.contains(KEYEVENTF_SCANCODE));
        assert!(right_control.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
        assert!(!right_control.dwFlags.contains(KEYEVENTF_KEYUP));

        let (_, numpad_four_up) = key_input("Numpad4", true).unwrap();
        let numpad_four_up = unsafe { numpad_four_up.Anonymous.ki };
        assert_eq!(numpad_four_up.wScan, 0x4b);
        assert!(!numpad_four_up.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
        assert!(numpad_four_up.dwFlags.contains(KEYEVENTF_KEYUP));

        let (_, pause) = key_input("Pause", false).unwrap();
        let pause = unsafe { pause.Anonymous.ki };
        assert_eq!(pause.wVk, VIRTUAL_KEY(0x13));
        assert!(!pause.dwFlags.contains(KEYEVENTF_SCANCODE));
    }

    #[test]
    fn normalized_clipboard_shortcuts_supply_control_for_parallel_window_replay() {
        let message: RemoteControlMessage = serde_json::from_value(serde_json::json!({
            "v": 1,
            "kind": "key",
            "targetUserId": "host",
            "controllerId": "controller",
            "windowId": 42,
            "seq": 1,
            "targetKind": "window",
            "shareInstanceId": "share-42",
            "key": "v",
            "code": "KeyV",
            "action": "down",
            "modifiers": { "alt": false, "ctrl": true, "meta": false, "shift": false }
        }))
        .unwrap();
        assert!(is_normalized_clipboard_shortcut(&message));

        let mut ordinary = message.clone();
        ordinary.control_session_id = Some("session".to_string());
        assert!(!is_normalized_clipboard_shortcut(&ordinary));
        let mut modified = message;
        modified.modifiers.alt = true;
        assert!(!is_normalized_clipboard_shortcut(&modified));
    }

    #[test]
    fn window_key_policy_is_explicit_and_altgr_does_not_double_modifiers() {
        assert!(validate_window_key_code("CapsLock").is_ok());
        assert!(validate_window_key_code("IntlRo").is_ok());
        assert!(validate_window_key_code("MetaLeft")
            .unwrap_err()
            .contains("unsupported"));
        assert!(validate_window_key_code("Unidentified")
            .unwrap_err()
            .contains("unsupported"));
        assert!(altgr_supplies_modifier(true, true, true, "ControlLeft"));
        assert!(altgr_supplies_modifier(true, true, true, "AltLeft"));
        assert!(!altgr_supplies_modifier(false, true, true, "ControlLeft"));
        assert!(!altgr_supplies_modifier(true, true, false, "ControlLeft"));
        assert!(!altgr_supplies_modifier(true, true, true, "ShiftLeft"));
    }

    #[test]
    fn win32_secure_field_policy_accepts_native_editors_but_not_passwords_or_unknown_classes() {
        assert!(trusted_win32_text_control("Edit", 0));
        assert!(trusted_win32_text_control("RichEditD2DPT", 0));
        assert!(trusted_win32_text_control("RICHEDIT50W", 0));
        assert!(!trusted_win32_text_control("RichEditD2DPT", 0x0020));
        assert!(!trusted_win32_text_control("TkChild", 0));
    }

    #[test]
    fn secure_text_provider_policy_fails_closed_for_unproven_frameworks() {
        assert!(trusted_uia_text_provider("Chrome"));
        assert!(trusted_uia_text_provider("WPF"));
        assert!(!trusted_uia_text_provider("Tk"));
        assert!(!trusted_uia_text_provider(""));
    }

    #[test]
    fn normalized_coordinates_include_negative_virtual_desktop_origins() {
        let mut message = wheel_message(1, "controller", 0.0, 0);
        message.x = Some(0.5);
        message.y = Some(0.25);
        assert_eq!(
            normalized_point(
                WindowFrame {
                    x: -1920,
                    y: -200,
                    width: 1600,
                    height: 1200,
                },
                &message,
            ),
            Ok((-1120, 100))
        );
    }

    #[test]
    fn generic_wheel_maps_dom_deltas_to_win32_axes_and_direction() {
        assert_eq!(
            wheel_delta_units(0.0, Some(0), Some(40.0), WheelAxis::Vertical),
            Ok((-120, 0.0))
        );
        assert_eq!(
            wheel_delta_units(0.0, Some(1), Some(1.0), WheelAxis::Horizontal),
            Ok((120, 0.0))
        );
        assert_eq!(
            wheel_delta_units(0.5, Some(0), Some(0.25), WheelAxis::Horizontal),
            Ok((1, 0.25))
        );
        assert!(
            wheel_delta_units(0.0, Some(3), Some(1.0), WheelAxis::Vertical)
                .unwrap_err()
                .contains("unsupported")
        );
        assert!(wheel_delta_units(0.0, Some(0), Some(f64::NAN), WheelAxis::Vertical).is_err());

        let input = mouse_input(MOUSEEVENTF_WHEEL, (-120i32) as u32);
        assert_eq!(unsafe { input.Anonymous.mi.mouseData } as i32, -120);
        assert!(unsafe { input.Anonymous.mi.dwFlags }.contains(MOUSEEVENTF_WHEEL));
        assert_eq!(
            unsafe { input.Anonymous.mi.dwExtraInfo },
            SYNTHETIC_INPUT_MARKER
        );
    }

    fn mods(
        ctrl: bool,
        shift: bool,
        alt: bool,
        meta: bool,
    ) -> crate::remote_control_core::RemoteControlModifiers {
        crate::remote_control_core::RemoteControlModifiers {
            alt,
            ctrl,
            meta,
            shift,
        }
    }

    #[test]
    fn wheel_wparam_packs_signed_delta_and_modifier_bits() {
        // Vertical wheel delta -120 with ctrl+shift held: high word -120,
        // low word MK_CONTROL(0x0008)|MK_SHIFT(0x0004) = 0x000C.
        let wparam = wheel_wparam(-120, &mods(true, true, false, false));
        assert_eq!(wparam, 0xFF88000C);
        // Positive delta, no modifiers.
        assert_eq!(
            wheel_wparam(120, &mods(false, false, false, false)),
            0x00780000
        );
        // Horizontal axis sign is preserved (the caller passes the signed
        // axis delta as-is).
        assert_eq!(
            wheel_wparam(-120, &mods(false, true, false, false)),
            0xFF880004
        );
    }

    #[test]
    fn wheel_lparam_preserves_negative_screen_coordinates() {
        // Negative virtual-desktop origin (-1920, -200) must round-trip as
        // signed 16-bit pairs, not truncate into another location.
        let lparam = wheel_lparam(-1920, -200).expect("representable negative coords");
        let x = (lparam as usize & 0xFFFF) as u16 as i16 as i32;
        let y = ((lparam as usize >> 16) & 0xFFFF) as u16 as i16 as i32;
        assert_eq!((x, y), (-1920, -200));
        // Out-of-range coordinates are refused, never truncated.
        assert!(wheel_lparam(i32::MAX, 0).is_err());
        assert!(wheel_lparam(0, i32::MIN).is_err());
        // In-range positive coordinates round-trip.
        let ok = wheel_lparam(800, 600).expect("in-range coords");
        assert_eq!((ok as usize & 0xFFFF) as i32, 800);
        assert_eq!(((ok as usize >> 16) & 0xFFFF) as i32, 600);
    }

    #[test]
    fn window_wheel_never_uses_global_focus_cursor_or_sendinput() {
        // The cursor-preserving route is a standalone function that resolves
        // a destination and posts WM_MOUSEWHEEL/HWHEEL; it must never call
        // focus_and_verify / SetCursorPos / SendInput. Pin that with a source
        // scan of the function body rather than a runtime HWND test (which
        // this sandbox cannot construct).
        let source = std::fs::read_to_string(file!()).expect("read self");
        let body = source
            .split_once("fn replay_window_wheel_postmessage")
            .expect("function present")
            .1
            .split_once("fn ")
            .map(|(before, _)| before)
            .unwrap_or_default();
        assert!(
            !body.contains("focus_and_verify")
                && !body.contains("SetCursorPos")
                && !body.contains("SendInput")
                && !body.contains("SetForegroundWindow"),
            "window wheel route must not focus, move the cursor, or use SendInput"
        );
        assert!(
            body.contains("SendMessageTimeoutW"),
            "window wheel route must deliver the wheel message to the target window"
        );
        assert!(body.contains("WM_MOUSEWHEEL") && body.contains("WM_MOUSEHWHEEL"));
    }

    #[test]
    fn window_wheel_resolves_a_scrollable_child_and_never_blocks_on_occlusion() {
        // The wheel is ID-addressed: `PostMessageW` goes to the shared window's
        // own queue, so occlusion by OTHER windows must neither block nor
        // redirect it. The destination is the target's own SCROLLABLE child
        // under the cursor (`scrollable_child_at_point` — `EnumChildWindows` +
        // `GetScrollInfo`/`WS_*SCROLL`), with the top-level target as fallback.
        // This is the 006B fix: `ChildWindowFromPointEx` returned a
        // non-scrollable container for Win11 Notepad, silently swallowing the
        // wheel; posting to a scrollable child (or the top level) reaches the
        // actual editor/render widget.
        //
        // The ONLY point check is `window_contains_point` — the target's own
        // client area via ScreenToClient/GetClientRect, which is
        // z-order-independent. We must NOT use `WindowFromPoint`/`IsChild`
        // here: they return the topmost window, so a browser partially
        // covering the Notepad on the sharer's desktop would make the check
        // refuse a perfectly valid aimed wheel (the 006B2 "Covered/Input
        // Ignored" regression). Source-scan the route body to pin this.
        let source = std::fs::read_to_string(file!()).expect("read self");
        let body = source
            .split_once("fn replay_window_wheel_postmessage")
            .expect("function present")
            .1
            .split_once("fn ")
            .map(|(before, _)| before)
            .unwrap_or_default();
        assert!(
            body.contains("window_contains_point"),
            "wheel must validate the aim point inside the TARGET's own client area"
        );
        assert!(
            body.contains("scrollable_child_at_point"),
            "wheel must resolve the target's own scrollable child under the cursor"
        );
        assert!(
            body.contains("SendMessageTimeoutW("),
            "wheel must deliver to the resolved destination (scrollable child, or the target itself) via SendMessageTimeoutW"
        );
        // The ONLY gate is `window_contains_point` (z-order-independent). The
        // pointer occlusion gate (`validate_pointer_point`/`root_window_at`)
        // and child resolution via the global z-order
        // (`child_window_at`/`ChildWindowFromPointEx`) must be absent
        // entirely — occlusion must never block ID-addressed injection.
        assert!(
            !body.contains("validate_pointer_point")
                && !body.contains("root_window_at")
                && !body.contains("child_window_at")
                && !body.contains("ChildWindowFromPointEx")
                && !body.contains("window_from_point_at")
                && !body.contains("IsChild"),
            "wheel must use only window_contains_point as its gate (occlusion must never block ID-addressed injection)"
        );
    }

    #[test]
    fn operation_feedback_preserves_controller_grant_and_pending_result() {
        let _guard = test_lock();
        clear_pending_controller_operations(92, Some("host"));
        let engine = crate::remote_control_core::remote_control_engine();
        engine.remove_controller_grant(92, "host");
        engine.install_controller_grant(
            92,
            "host".to_string(),
            crate::remote_control_core::ControllerGrantEnvelope {
                target_kind: RemoteControlTargetKind::Window,
                share_instance_id: "share-92".to_string(),
                control_session_id: "session-92".to_string(),
                grant_token: "0123456789abcdef0123456789abcdef".to_string(),
                full_pointer: false,
                host_capabilities: vec![RemoteControlCapability::UnicodeText],
                next_input_seq: 1,
            },
        );
        let mut outbound: RemoteControlMessage = serde_json::from_value(serde_json::json!({
            "v": 2,
            "kind": "text",
            "targetUserId": "host",
            "controllerId": "controller",
            "windowId": 92,
            "seq": 1,
            "text": "petal"
        }))
        .unwrap();
        assert_eq!(prepare_outbound_input(&mut outbound), Ok(true));

        let mut feedback = outbound.clone();
        feedback.message_type = RemoteControlType::Status;
        feedback.controller_id = "host".to_string();
        feedback.target_user_id = "controller".to_string();
        assert!(record_controller_status(&feedback, "occluded"));

        let preserved = engine
            .controller_grant(92, "host")
            .expect("operation feedback must preserve the active grant");
        assert_eq!(preserved.share_instance_id, "share-92");
        assert_eq!(preserved.control_session_id, "session-92");
        assert_eq!(preserved.grant_token, "0123456789abcdef0123456789abcdef");

        let mut result = outbound;
        result.message_type = RemoteControlType::Result;
        result.controller_id = "host".to_string();
        result.target_user_id = "controller".to_string();
        assert!(
            accept_controller_result(&result),
            "operation feedback must preserve pending result correlation"
        );
        let mut subsequent: RemoteControlMessage = serde_json::from_value(serde_json::json!({
            "v": 2,
            "kind": "text",
            "targetUserId": "host",
            "controllerId": "controller",
            "windowId": 92,
            "seq": 2,
            "text": "still active"
        }))
        .unwrap();
        assert_eq!(prepare_outbound_input(&mut subsequent), Ok(true));
        assert_eq!(subsequent.control_session_id.as_deref(), Some("session-92"));
        assert_eq!(subsequent.share_instance_id.as_deref(), Some("share-92"));

        assert_eq!(
            controller_status_effect("occluded"),
            ControllerStatusEffect::Feedback
        );
        assert!(record_controller_status(&feedback, "stopped"));
        assert_eq!(
            controller_status_effect("stopped"),
            ControllerStatusEffect::Terminate
        );
        assert!(engine.controller_grant(92, "host").is_none());

        engine.install_controller_grant(
            92,
            "host".to_string(),
            crate::remote_control_core::ControllerGrantEnvelope {
                target_kind: RemoteControlTargetKind::Window,
                share_instance_id: "share-92".to_string(),
                control_session_id: "session-92".to_string(),
                grant_token: "0123456789abcdef0123456789abcdef".to_string(),
                full_pointer: false,
                host_capabilities: vec![RemoteControlCapability::UnicodeText],
                next_input_seq: 2,
            },
        );
        assert!(record_controller_status(&feedback, "disabled"));
        assert_eq!(
            controller_status_effect("disabled"),
            ControllerStatusEffect::Terminate
        );
        assert!(engine.controller_grant(92, "host").is_none());
        clear_pending_controller_operations(92, Some("host"));
    }

    #[test]
    fn request_feedback_without_an_active_grant_stays_inactive() {
        let _guard = test_lock();
        let engine = crate::remote_control_core::remote_control_engine();
        engine.remove_controller_grant(93, "host");
        clear_pending_controller_operations(93, Some("host"));
        let mut feedback = wheel_message(93, "host", 1.0, 0);
        feedback.message_type = RemoteControlType::Status;
        feedback.controller_id = "host".to_string();
        feedback.target_user_id = "controller".to_string();

        assert_eq!(
            controller_status_effect("requestUnavailable"),
            ControllerStatusEffect::Feedback
        );
        assert!(record_controller_status(&feedback, "requestUnavailable"));
        assert!(engine.controller_grant(93, "host").is_none());
    }

    #[test]
    fn result_must_match_one_pending_operation_and_current_grant() {
        let _guard = test_lock();
        clear_pending_controller_operations(91, Some("host"));
        let engine = crate::remote_control_core::remote_control_engine();
        engine.remove_controller_grant(91, "host");
        engine.install_controller_grant(
            91,
            "host".to_string(),
            crate::remote_control_core::ControllerGrantEnvelope {
                target_kind: RemoteControlTargetKind::Window,
                share_instance_id: "share-91".to_string(),
                control_session_id: "session-91".to_string(),
                grant_token: "0123456789abcdef0123456789abcdef".to_string(),
                full_pointer: false,
                host_capabilities: vec![RemoteControlCapability::UnicodeText],
                next_input_seq: 1,
            },
        );
        let mut outbound: RemoteControlMessage = serde_json::from_value(serde_json::json!({
            "v": 2,
            "kind": "text",
            "targetUserId": "host",
            "controllerId": "controller",
            "windowId": 91,
            "seq": 1,
            "text": "petal"
        }))
        .unwrap();
        assert_eq!(prepare_outbound_input(&mut outbound), Ok(true));

        let mut result = outbound.clone();
        result.message_type = RemoteControlType::Result;
        result.controller_id = "host".to_string();
        result.target_user_id = "controller".to_string();
        assert!(accept_controller_result(&result));
        assert!(!accept_controller_result(&result));

        engine.remove_controller_grant(91, "host");
        clear_pending_controller_operations(91, Some("host"));
    }

    #[test]
    fn unicode_text_inputs_preserve_non_bmp_surrogate_pairs() {
        let inputs = unicode_text_inputs("A🪷");
        assert_eq!(inputs.len(), 6);
        let scans = inputs
            .iter()
            .step_by(2)
            .map(|input| unsafe { input.Anonymous.ki.wScan })
            .collect::<Vec<_>>();
        assert_eq!(scans, "A🪷".encode_utf16().collect::<Vec<_>>());
        assert!(inputs
            .iter()
            .all(|input| unsafe { input.Anonymous.ki.dwExtraInfo == SYNTHETIC_INPUT_MARKER }));
    }

    #[test]
    fn synthesized_modifier_skipped_when_controller_forwards_it() {
        // The controller forwards modifier keys as their own key events, so a
        // SHIFT+A typed on the controller arrives as Down(ShiftLeft) then
        // Down(KeyA) (then the symmetric Up pair). The Down(ShiftLeft) event
        // carries modifiers.shift=true; the synthesis loop must NOT press a
        // second Shift -- that would double-press and leave it stuck on a
        // single release. Each name must also exempt its mirrored right-side
        // code, since dom_code_to_virtual_key maps both sides to one VK.
        assert!(code_is_same_modifier("ShiftLeft", "ShiftLeft"));
        assert!(code_is_same_modifier("ShiftRight", "ShiftLeft"));
        assert!(code_is_same_modifier("ControlRight", "ControlLeft"));
        assert!(code_is_same_modifier("AltRight", "AltLeft"));
        // A separate letter is never the modifier being synthesized.
        assert!(!code_is_same_modifier("KeyA", "ShiftLeft"));
        assert!(!code_is_same_modifier("Digit5", "ShiftLeft"));
        assert!(!code_is_same_modifier("KeyA", "ControlLeft"));
    }

    #[test]
    fn pointer_point_accepts_only_the_target_or_its_sharer_overlay() {
        assert!(pointer_root_matches_target(10, 10, &[20]));
        assert!(pointer_root_matches_target(20, 10, &[20]));
        assert!(!pointer_root_matches_target(30, 10, &[20]));
    }

    #[test]
    fn pointer_up_at_a_covered_point_skips_the_cursor_warp() {
        // 011B: a click is Down+Up; the Down at a covered point was refused
        // (no cursor move), but the paired Up skipped the occlusion gate and
        // warped B's cursor to the covered point — making the telepointer tag
        // jump to the covering window. A refused click must be a true no-op:
        // the Up's SetCursorPos must be conditional on the point being owned.
        // Source-scan `replay_window_pointer_global` to pin the guard.
        let source = std::fs::read_to_string(file!()).expect("read self");
        let body = source
            .split_once("fn replay_window_pointer_global")
            .expect("function present")
            .1
            .split_once("fn ")
            .map(|(before, _)| before)
            .unwrap_or_default();
        assert!(
            body.contains("point_owned"),
            "pointer replay must compute whether the point is owned before warping the cursor"
        );
        assert!(
            body.contains("if point_owned && unsafe { SetCursorPos"),
            "SetCursorPos must be skipped when the point is not owned (covered Up must not warp)"
        );
        // The Up itself is still submitted (a release is never dropped).
        assert!(
            body.contains("submit_inputs(&[mouse_input(up_flag, 0)], true)"),
            "the Up button release must still be submitted even at a covered point"
        );
    }

    #[test]
    fn mouse_button_wire_maps_to_the_three_button_flags() {
        // macOS replays Left/Middle/Right; the host must map the wire button
        // (Some(1)=middle, Some(2)=right, else left) to its own down/up flags.
        assert_eq!(
            mouse_button_flags(None).unwrap(),
            (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
        );
        assert_eq!(
            mouse_button_flags(Some(0)).unwrap(),
            (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
        );
        assert_eq!(
            mouse_button_flags(Some(1)).unwrap(),
            (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP)
        );
        assert_eq!(
            mouse_button_flags(Some(2)).unwrap(),
            (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
        );
    }
}
