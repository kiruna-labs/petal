//! Accessibility (AX) subrole resolution for window classification (#747,
//! Phase 3 stage 1).
//!
//! Subrole is the strongest "what kind of window is this" differentiator
//! (plan §3, from alt-tab-macos's `WindowDiscriminator` and AeroSpace's
//! window/dialog/popup typing, and the user's prior art) -- geometry alone
//! mis-classifies dialogs, popups, and per-app oddities. It is also EXPENSIVE
//! and can stall on a busy app's main thread, so every call here is bounded by
//! `AXUIElementSetMessagingTimeout`, resolved at most once per window lifetime
//! by the registry's cache, and never on a consumer's read path (plan §3
//! stage-1 rules; the AX-budget counting-oracle tests enforce this).
//!
//! Uses the private `_AXUIElementGetWindow` (AeroSpace ships notarized with
//! exactly this one symbol, SIP intact -- §2) to map an app's AX window
//! elements to their `CGWindowID`, resolved via `dlsym` so a missing symbol
//! degrades gracefully rather than failing to link.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;
use std::sync::OnceLock;

/// The window kind we care about, derived from AX subrole (+ chrome buttons for
/// the dialog/popup split, per AeroSpace). `Standard` is a real top-level user
/// window; `Dialog` a modal/utility panel; `Popup` a chrome-less transient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxKind {
    Standard,
    Dialog,
    Popup,
}

/// Map an AX subrole string (+ whether the window has a close/fullscreen button)
/// to an [`AxKind`]. Pure and unit-tested; the FFI half feeds it.
///
/// Rules follow the research (§3):
/// - `AXStandardWindow` -> Standard.
/// - `AXDialog` / `AXSystemDialog` -> Dialog.
/// - `AXFloatingWindow` -> Dialog (utility/floating panels; AeroSpace treats
///   these as not-standard).
/// - anything else (unknown/empty subrole) -> Popup if it has NO window chrome
///   ("incredibly weird popup like AXWindows without any buttons" -- AeroSpace),
///   else Dialog (a titled/buttoned window with a nonstandard subrole, e.g.
///   JetBrains, is still a real window -> treat as Dialog, not Popup).
pub fn classify_from_subrole(subrole: &str, has_window_chrome: bool) -> AxKind {
    match subrole {
        "AXStandardWindow" => AxKind::Standard,
        "AXDialog" | "AXSystemDialog" => AxKind::Dialog,
        "AXFloatingWindow" => AxKind::Dialog,
        _ => {
            if has_window_chrome {
                AxKind::Dialog
            } else {
                AxKind::Popup
            }
        }
    }
}

// ---- FFI (declared locally; resolved/bounded per call) --------------------

// The AX FUNCTIONS resolve from the ApplicationServices framework already
// linked by remote_control (same crate); no local #[link] -- a duplicate
// umbrella-framework link trips a SwiftUICore linker error in the test binary.
// The kAX*Attribute CFString CONSTANTS are deliberately NOT imported as extern
// statics (that would require the umbrella #[link] and the SwiftUICore trip);
// instead we build them from their documented, stable string names via
// CFStringCreateWithCString -- see `ax_attr`.
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, timeout: f32) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
}

// CoreFoundation is linked by cg.rs (same crate).
extern "C" {
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFStringGetCString(s: *const c_void, buf: *mut u8, len: isize, encoding: u32) -> bool;
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        cstr: *const std::os::raw::c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFEqual(a: *const c_void, b: *const c_void) -> bool;
    fn CFStringGetTypeID() -> usize;
}

const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.25;
const KCF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// Build an AX attribute-name CFString from its stable literal (e.g.
/// "AXWindows"). Caller releases it. Avoids importing the kAX* extern statics.
unsafe fn ax_attr(name: &std::ffi::CStr) -> *const c_void {
    CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), KCF_STRING_ENCODING_UTF8)
}

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const std::os::raw::c_char) -> *mut c_void;
}
/// RTLD_DEFAULT: search all loaded images for the symbol.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

/// Full-tier kill switch (degradation drill, §7.4): PETAL_DISABLE_AX=1
/// simulates a machine where stage-1 cannot run at all -> T2 only.
fn ax_tier_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var("PETAL_DISABLE_AX").is_ok())
}

/// `_AXUIElementGetWindow(element, &wid)` -- private, resolved once via dlsym.
/// PETAL_DISABLE_AX_GETWINDOW=1 (degradation drill, §7.4) simulates the
/// private symbol vanishing on a future macOS: resolution then falls back to
/// (pid,frame) CORRELATION (§3), it does NOT disable the tier.
type AxGetWindowFn = unsafe extern "C" fn(*const c_void, *mut u32) -> i32;
fn ax_get_window_fn() -> Option<AxGetWindowFn> {
    if ax_tier_disabled() {
        return None;
    }
    static GW_DISABLED: OnceLock<bool> = OnceLock::new();
    if *GW_DISABLED.get_or_init(|| std::env::var("PETAL_DISABLE_AX_GETWINDOW").is_ok()) {
        return None;
    }
    static F: OnceLock<Option<AxGetWindowFn>> = OnceLock::new();
    *F.get_or_init(|| unsafe {
        let sym = dlsym(
            RTLD_DEFAULT,
            b"_AXUIElementGetWindow\0".as_ptr() as *const std::os::raw::c_char,
        );
        if sym.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, AxGetWindowFn>(sym))
        }
    })
}

/// Whether the private id-mapping symbol is available (canary / degradation).
pub fn get_window_symbol_available() -> bool {
    ax_get_window_fn().is_some()
}

/// Whether ANY stage-1 id-mapping mechanism is available: the private symbol
/// or the (pid,frame) correlation fallback (always present unless the tier is
/// force-disabled). This is the tier line's "mechanism" input — a symbol miss
/// alone must log as T1-via-correlation, not `AX-UNAVAILABLE`.
pub fn ax_mechanism_available() -> bool {
    !ax_tier_disabled()
}

extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Whether this process is Accessibility-trusted. ⚠ A `true` here is NOT
/// proof AX window reads work: under inherited (responsible-process) trust
/// this returns a FALSE-POSITIVE `true` while every window-element read
/// silently degrades to app-element copies (plan §9.14). Only
/// [`window_read_canary`] proves T1.
pub fn process_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Cheap preconditions for stage-1: tier not force-disabled and the process
/// Accessibility-trusted. The private id-mapping symbol is NOT required —
/// when it is missing, resolution falls back to (pid,frame) correlation (§3),
/// so a symbol miss demotes the mechanism, not the tier. NOT sufficient on
/// its own -- see [`window_read_canary`] and [`process_trusted`]'s caveat.
pub fn ax_classification_preconditions() -> bool {
    !ax_tier_disabled() && process_trusted()
}

/// The honest T1 canary (plan §3 ladder): perform ONE REAL window-element read
/// against OUR OWN process and require a non-app role back. Under degraded
/// trust the elements in `kAXWindows` collapse to the application element
/// (role `AXApplication`, `_AXUIElementGetWindow` -> kAXErrorIllegalArgument)
/// even for one's own process -- verified live in both directions (#747
/// §9.14): a directly-granted process reads its own window as `AXWindow` with
/// a real CGWindowID; an untrusted/inherited-trust process does not.
///
/// Self-targeted only (never stalls on a foreign app's main thread; messaging
/// timeout bounds it anyway). Returns `false` when preconditions fail, when no
/// window element is served yet (fresh windows lag AX registration -- caller
/// retries), or when the served elements are the degraded app-element copies.
pub fn window_read_canary() -> bool {
    if !ax_classification_preconditions() {
        return false;
    }
    let get_window = ax_get_window_fn(); // None -> correlation-mode criterion
    // SAFETY: same lifecycle rules as `resolve_window_kinds_for_app`, own pid.
    unsafe {
        let app = AXUIElementCreateApplication(std::process::id() as i32);
        if app.is_null() {
            return false;
        }
        AXUIElementSetMessagingTimeout(app, AX_MESSAGING_TIMEOUT_SECONDS);
        let attr_windows = ax_attr(c"AXWindows");
        let attr_role = ax_attr(c"AXRole");
        let mut ok = false;
        if let Some(windows) = copy_attr(app, attr_windows) {
            let count = CFArrayGetCount(windows);
            for i in 0..count {
                let w = CFArrayGetValueAtIndex(windows, i);
                if w.is_null() {
                    continue;
                }
                let role = copy_attr(w, attr_role).and_then(|r| {
                    let out = cfstring_to_string(r);
                    CFRelease(r);
                    out
                });
                if role.as_deref() != Some("AXWindow") {
                    continue; // degraded elements read AXApplication here
                }
                let mapped = match get_window {
                    Some(gw) => {
                        let mut wid: u32 = 0;
                        gw(w, &mut wid) == 0 && wid != 0
                    }
                    // Correlation mode: the mapping input is AXPosition/AXSize;
                    // the canary passes iff a real frame is readable (degraded
                    // elements fail both reads with -25205).
                    None => element_frame(w).is_some(),
                };
                if mapped {
                    ok = true;
                    break;
                }
            }
            CFRelease(windows);
        }
        if !attr_windows.is_null() {
            CFRelease(attr_windows);
        }
        if !attr_role.is_null() {
            CFRelease(attr_role);
        }
        CFRelease(app);
        ok
    }
}

// AXValue unwrapping for AXPosition/AXSize (public API; used by the
// (pid,frame) correlation fallback when `_AXUIElementGetWindow` is missing).
extern "C" {
    fn AXValueGetValue(value: *const c_void, value_type: u32, out: *mut c_void) -> bool;
}
const KAX_VALUE_TYPE_CGPOINT: u32 = 1;
const KAX_VALUE_TYPE_CGSIZE: u32 = 2;
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CgPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CgSize {
    w: f64,
    h: f64,
}

/// Read an element's global top-left frame via public AXPosition/AXSize.
/// Returns `None` when either attribute is unreadable (degraded elements
/// return -25205 on both, so correlation naturally fails closed there).
unsafe fn element_frame(el: *const c_void) -> Option<(f64, f64, f64, f64)> {
    let attr_pos = ax_attr(c"AXPosition");
    let attr_size = ax_attr(c"AXSize");
    let mut result = None;
    if let Some(v) = copy_attr(el, attr_pos) {
        let mut p = CgPoint::default();
        let ok_p = AXValueGetValue(v, KAX_VALUE_TYPE_CGPOINT, &mut p as *mut _ as *mut c_void);
        CFRelease(v);
        if ok_p {
            if let Some(v2) = copy_attr(el, attr_size) {
                let mut sz = CgSize::default();
                let ok_s =
                    AXValueGetValue(v2, KAX_VALUE_TYPE_CGSIZE, &mut sz as *mut _ as *mut c_void);
                CFRelease(v2);
                if ok_s {
                    result = Some((p.x, p.y, sz.w, sz.h));
                }
            }
        }
    }
    if !attr_pos.is_null() {
        CFRelease(attr_pos);
    }
    if !attr_size.is_null() {
        CFRelease(attr_size);
    }
    result
}

/// (pid,frame) correlation (§3 fallback): match one AX element's frame against
/// the wanted windows' raw CG frames. A match must be UNIQUE among wanted —
/// two same-frame candidates return `None` (never risk misclassifying window
/// A with window B's subrole). Pure; unit-tested.
pub fn correlate_frame_to_wid(
    frame: (f64, f64, f64, f64),
    wanted: &std::collections::HashMap<u32, (f64, f64, f64, f64)>,
) -> Option<u32> {
    correlate_frame_to_wid_detailed(frame, wanted).ok()
}

/// Why an AX element could not be mapped to one unambiguous CG window.
///
/// Remote control treats every variant as a capability failure and refuses
/// input. In particular, callers must not turn a correlation miss into a
/// legitimate "different window" verdict: only a successfully resolved CG
/// window id can prove that distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowIdentityError {
    NullElement,
    GetWindowFailed(i32),
    RoleUnavailable,
    ParentUnavailable,
    ParentNotElement,
    AncestorCycle,
    AncestorLimit,
    FrameUnavailable,
    FrameDidNotMatch,
    FrameMatchAmbiguous,
}

/// Resolve one real AX element to its CGWindowID.
///
/// `_AXUIElementGetWindow` is the primary mechanism. When that private symbol
/// is unavailable, use the public AXPosition/AXSize attributes and correlate
/// them against the fresh registry candidates and an OptionAll same-pid
/// universe. The sets are supplied by the caller so this primitive remains
/// usable by live tests without mutating the process-global registry.
///
/// A correlation must be unique. Misses, ambiguity, unreadable frames, and
/// private-API errors are all errors, so security-sensitive callers fail
/// closed rather than silently degrading to pid-only scoping.
///
/// On-screen registry candidates for one pid -- the set a resolved id must
/// BELONG to before it authorizes anything. Provenance: a fresh on-screen
/// registry snapshot.
///
/// Distinct newtype on purpose: this and [`UniverseFrames`] carry the same
/// map shape, and an earlier revision took both as bare `HashMap`s -- a
/// mutation swapping the arguments at the production call site survived the
/// entire pure-test battery, because those tests construct their own correct
/// arguments. The type distinction kills that whole mutation class at
/// compile time instead.
pub(crate) struct CandidateFrames(
    pub(crate) std::collections::HashMap<u32, (f64, f64, f64, f64)>,
);

/// The COMPLETE same-pid window universe (CG `OptionAll`), including
/// minimised/off-Space windows. Correlation uniqueness is decided against
/// this set, so a sibling absent from the on-screen list still participates
/// in ambiguity and can never be silently mis-resolved onto an authorized
/// window (#779 review finding 2).
pub(crate) struct UniverseFrames(
    pub(crate) std::collections::HashMap<u32, (f64, f64, f64, f64)>,
);

/// # Safety
/// `element` must be a valid AXUIElementRef.
pub(crate) unsafe fn resolve_element_window_id(
    element: *const c_void,
    same_pid_frames: &CandidateFrames,
    all_same_pid_frames: &UniverseFrames,
) -> Result<u32, WindowIdentityError> {
    resolve_element_window_id_with(
        element,
        same_pid_frames,
        all_same_pid_frames,
        ax_get_window_fn(),
    )
}

/// Test-only entry into the exact production fallback, bypassing the normally
/// available private symbol so the real-window gate can exercise correlation.
#[cfg(test)]
pub(crate) unsafe fn resolve_element_window_id_via_frame_fallback(
    element: *const c_void,
    same_pid_frames: &CandidateFrames,
    all_same_pid_frames: &UniverseFrames,
) -> Result<u32, WindowIdentityError> {
    resolve_element_window_id_with(element, same_pid_frames, all_same_pid_frames, None)
}

unsafe fn resolve_element_window_id_with(
    element: *const c_void,
    same_pid_frames: &CandidateFrames,
    all_same_pid_frames: &UniverseFrames,
    get_window: Option<AxGetWindowFn>,
) -> Result<u32, WindowIdentityError> {
    if element.is_null() {
        return Err(WindowIdentityError::NullElement);
    }
    if let Some(get_window) = get_window {
        let mut wid = 0u32;
        let status = get_window(element, &mut wid);
        return if status == 0 && wid != 0 {
            Ok(wid)
        } else {
            Err(WindowIdentityError::GetWindowFailed(status))
        };
    }

    // The hit-test primitive returns an arbitrary descendant. AXPosition/Size
    // on that descendant describes a control, not its containing window, so
    // correlation must first ascend through AXParent to an AXWindow. Keep all
    // create-rule parent references alive until the walk ends, both for pointer
    // validity and for cycle detection via CFEqual.
    const MAX_PARENT_HOPS: usize = 25;
    let attr_role = ax_attr(c"AXRole");
    let attr_parent = ax_attr(c"AXParent");
    if attr_role.is_null() || attr_parent.is_null() {
        if !attr_role.is_null() {
            CFRelease(attr_role);
        }
        if !attr_parent.is_null() {
            CFRelease(attr_parent);
        }
        return Err(WindowIdentityError::RoleUnavailable);
    }
    let mut current = element;
    let mut owned_ancestors: Vec<*const c_void> = Vec::new();
    let top_level = loop {
        let role = copy_attr(current, attr_role)
            .and_then(|value| {
                let role = cfstring_to_string(value);
                CFRelease(value);
                role
            })
            .ok_or(WindowIdentityError::RoleUnavailable);
        match role {
            Ok(role) if role == "AXWindow" => break Ok(current),
            Ok(_) => {}
            Err(error) => break Err(error),
        }
        if owned_ancestors.len() >= MAX_PARENT_HOPS {
            break Err(WindowIdentityError::AncestorLimit);
        }
        let Some(parent) = copy_attr(current, attr_parent) else {
            break Err(WindowIdentityError::ParentUnavailable);
        };
        if CFGetTypeID(parent) != AXUIElementGetTypeID() {
            CFRelease(parent);
            break Err(WindowIdentityError::ParentNotElement);
        }
        if CFEqual(parent, element)
            || owned_ancestors
                .iter()
                .any(|ancestor| CFEqual(parent, *ancestor))
        {
            CFRelease(parent);
            break Err(WindowIdentityError::AncestorCycle);
        }
        owned_ancestors.push(parent);
        current = parent;
    };
    CFRelease(attr_role);
    CFRelease(attr_parent);
    let result = top_level.and_then(|window| {
        let frame = element_frame(window).ok_or(WindowIdentityError::FrameUnavailable)?;
        // The CG OptionAll read and this AX frame read are not atomic: a window
        // can still move between them. Unique + near-tie refusal narrows that
        // residual race, but cannot eliminate it.
        correlate_frame_to_wid_with_universe(frame, same_pid_frames, all_same_pid_frames)
    });
    for ancestor in owned_ancestors {
        CFRelease(ancestor);
    }
    result
}

fn correlate_frame_to_wid_detailed(
    frame: (f64, f64, f64, f64),
    same_pid_frames: &std::collections::HashMap<u32, (f64, f64, f64, f64)>,
) -> Result<u32, WindowIdentityError> {
    const EPS: f64 = 1.5;
    let close = |a: f64, b: f64| (a - b).abs() <= EPS;
    let mut hit = None;
    for (&wid, &(x, y, w, h)) in same_pid_frames {
        if close(frame.0, x) && close(frame.1, y) && close(frame.2, w) && close(frame.3, h) {
            if hit.is_some() {
                return Err(WindowIdentityError::FrameMatchAmbiguous);
            }
            hit = Some(wid);
        }
    }
    hit.ok_or(WindowIdentityError::FrameDidNotMatch)
}

fn correlate_frame_to_wid_strict_detailed(
    frame: (f64, f64, f64, f64),
    same_pid_frames: &std::collections::HashMap<u32, (f64, f64, f64, f64)>,
) -> Result<u32, WindowIdentityError> {
    const EPS: f64 = 1.5;
    let distance = |candidate: (f64, f64, f64, f64)| {
        (frame.0 - candidate.0)
            .abs()
            .max((frame.1 - candidate.1).abs())
            .max((frame.2 - candidate.2).abs())
            .max((frame.3 - candidate.3).abs())
    };
    let mut ranked: Vec<(f64, u32)> = Vec::with_capacity(same_pid_frames.len());
    for (&wid, &(x, y, w, h)) in same_pid_frames {
        ranked.push((distance((x, y, w, h)), wid));
    }
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
    let Some(&(best_distance, best_wid)) = ranked.first() else {
        return Err(WindowIdentityError::FrameDidNotMatch);
    };
    if best_distance > EPS {
        return Err(WindowIdentityError::FrameDidNotMatch);
    }
    if ranked
        .get(1)
        .is_some_and(|(second_distance, _)| *second_distance <= best_distance + EPS)
    {
        return Err(WindowIdentityError::FrameMatchAmbiguous);
    }
    Ok(best_wid)
}

fn correlate_frame_to_wid_with_universe(
    frame: (f64, f64, f64, f64),
    same_pid_frames: &CandidateFrames,
    all_same_pid_frames: &UniverseFrames,
) -> Result<u32, WindowIdentityError> {
    let wid = correlate_frame_to_wid_strict_detailed(frame, &all_same_pid_frames.0)?;
    same_pid_frames
        .0
        .contains_key(&wid)
        .then_some(wid)
        .ok_or(WindowIdentityError::FrameDidNotMatch)
}

/// Crate-visible helpers for the observer module (`ax_observer.rs`) — same
/// safety contracts as the private versions they wrap.
///
/// # Safety
/// `s` must be a valid CF object pointer or null.
pub(crate) unsafe fn cfstring_to_string_public(s: *const c_void) -> Option<String> {
    cfstring_to_string(s)
}

/// # Safety
/// Caller releases the returned CFString.
pub(crate) unsafe fn ax_attr_public(name: &std::ffi::CStr) -> *const c_void {
    ax_attr(name)
}

/// Map an AX element to its CGWindowID via the private symbol; `None` when
/// the symbol is unavailable (correlation-only mode) or the element is not a
/// window. Cheap enough for an observer callback.
///
/// # Safety
/// `element` must be a valid AXUIElementRef or null.
pub(crate) unsafe fn element_wid(element: *const c_void) -> Option<u32> {
    if element.is_null() {
        return None;
    }
    let gw = ax_get_window_fn()?;
    let mut wid: u32 = 0;
    if gw(element, &mut wid) == 0 && wid != 0 {
        Some(wid)
    } else {
        None
    }
}

unsafe fn cfstring_to_string(s: *const c_void) -> Option<String> {
    if s.is_null() || CFGetTypeID(s) != CFStringGetTypeID() {
        return None;
    }
    let mut buf = [0u8; 256];
    if CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, KCF_STRING_ENCODING_UTF8) {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        std::str::from_utf8(&buf[..end]).ok().map(|s| s.to_string())
    } else {
        None
    }
}

unsafe fn copy_attr(element: *const c_void, attr: *const c_void) -> Option<*const c_void> {
    let mut out: *const c_void = std::ptr::null();
    if AXUIElementCopyAttributeValue(element, attr, &mut out) == 0 && !out.is_null() {
        Some(out)
    } else {
        None
    }
}

/// Outcome of one per-APP resolution pass (#747 batch AX budget).
pub enum AppKindsOutcome {
    /// The app's `kAXWindows` array was readable. `kinds` maps every WANTED
    /// wid that was present in the array to its classification; wanted wids
    /// absent from the map were not (yet) served by the app's AX server —
    /// the caller retries those briefly (birth lag) then gives up.
    Resolved(std::collections::HashMap<u32, AxKind>),
    /// The app-LEVEL query failed (no AX server, API disabled, timeout, dead
    /// app element). Counts toward the caller's per-pid `AXDead` strikes —
    /// never toward per-window retry budgets.
    AppUnavailable,
}

/// Resolve the [`AxKind`] for EVERY wanted window of one app in a single AX
/// pass: one `AXUIElementCreateApplication`, ONE `kAXWindowsAttribute` copy,
/// one id-mapping per element, and subrole + chrome reads ONLY for elements
/// mapped to a wanted wid (§3 stage-1 budget: a fixed handful of attribute
/// copies on elements already in hand, never per-window array re-copies —
/// resolving K windows of one app costs one array copy, not K).
///
/// Id mapping: `_AXUIElementGetWindow` when the private symbol resolves;
/// otherwise the (pid,frame) CORRELATION fallback (§3) — the element's public
/// AXPosition/AXSize matched uniquely against `wanted`'s raw CG frames.
///
/// Runs on the registry's 10Hz ingest thread ONLY — never on `refresh_now`'s
/// callers (hover 60Hz follow, remote-control), because
/// `AXUIElementSetMessagingTimeout` still allows each app-level call to block
/// up to 250ms on a busy app's main thread (#747 audit; the user's prior
/// system hit exactly this).
pub fn resolve_window_kinds_for_app(
    pid: i32,
    wanted: &std::collections::HashMap<u32, (f64, f64, f64, f64)>,
) -> AppKindsOutcome {
    if ax_tier_disabled() {
        return AppKindsOutcome::AppUnavailable;
    }
    let get_window = ax_get_window_fn(); // None -> correlation fallback
    // SAFETY: standard AX app-element lifecycle; every returned CF value is
    // released exactly once. Messaging timeout bounds a hung app.
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return AppKindsOutcome::AppUnavailable;
        }
        AXUIElementSetMessagingTimeout(app, AX_MESSAGING_TIMEOUT_SECONDS);
        let attr_windows = ax_attr(c"AXWindows");
        let attr_subrole = ax_attr(c"AXSubrole");
        let attr_close = ax_attr(c"AXCloseButton");
        let attr_fullscreen = ax_attr(c"AXFullScreenButton");
        let cleanup = |extra: &[*const c_void]| {
            for p in [attr_windows, attr_subrole, attr_close, attr_fullscreen]
                .iter()
                .chain(extra.iter())
            {
                if !p.is_null() {
                    CFRelease(*p);
                }
            }
            CFRelease(app);
        };
        let Some(windows) = copy_attr(app, attr_windows) else {
            cleanup(&[]);
            return AppKindsOutcome::AppUnavailable;
        };
        let count = CFArrayGetCount(windows);
        let mut kinds = std::collections::HashMap::new();
        for i in 0..count {
            if kinds.len() == wanted.len() {
                break; // every wanted window classified — stop reading
            }
            let w = CFArrayGetValueAtIndex(windows, i);
            if w.is_null() {
                continue;
            }
            let this_wid: u32 = match get_window {
                Some(gw) => {
                    let mut wid: u32 = 0;
                    if gw(w, &mut wid) != 0 || !wanted.contains_key(&wid) {
                        continue; // getwindow is the only read spent on unwanted elements
                    }
                    wid
                }
                None => {
                    // Correlation: two public reads per element, unique frame
                    // match required (ambiguity -> skip, never misclassify).
                    let Some(frame) = element_frame(w) else {
                        continue;
                    };
                    let Some(wid) = correlate_frame_to_wid(frame, wanted) else {
                        continue;
                    };
                    wid
                }
            };
            let subrole = copy_attr(w, attr_subrole)
                .and_then(|s| {
                    let out = cfstring_to_string(s);
                    CFRelease(s);
                    out
                })
                .unwrap_or_default();
            let has_chrome = copy_attr(w, attr_close)
                .map(|b| {
                    CFRelease(b);
                    true
                })
                .or_else(|| {
                    copy_attr(w, attr_fullscreen).map(|b| {
                        CFRelease(b);
                        true
                    })
                })
                .unwrap_or(false);
            kinds.insert(this_wid, classify_from_subrole(&subrole, has_chrome));
        }
        cleanup(&[windows]);
        AppKindsOutcome::Resolved(kinds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_window_is_standard() {
        assert_eq!(classify_from_subrole("AXStandardWindow", true), AxKind::Standard);
        assert_eq!(classify_from_subrole("AXStandardWindow", false), AxKind::Standard);
    }

    #[test]
    fn dialogs_and_floats_are_dialog() {
        assert_eq!(classify_from_subrole("AXDialog", true), AxKind::Dialog);
        assert_eq!(classify_from_subrole("AXSystemDialog", false), AxKind::Dialog);
        assert_eq!(classify_from_subrole("AXFloatingWindow", true), AxKind::Dialog);
    }

    #[test]
    fn chromeless_unknown_subrole_is_popup_titled_is_dialog() {
        // AeroSpace: "weird popup like AXWindows without any buttons" -> Popup.
        assert_eq!(classify_from_subrole("", false), AxKind::Popup);
        assert_eq!(classify_from_subrole("AXUnknown", false), AxKind::Popup);
        // A nonstandard subrole WITH chrome (JetBrains-style) is a real window.
        assert_eq!(classify_from_subrole("AXUnknown", true), AxKind::Dialog);
    }

    /// (pid,frame) correlation (§3 fallback): unique match within epsilon
    /// wins; ambiguity and out-of-epsilon refuse (never misclassify).
    #[test]
    fn frame_correlation_requires_a_unique_close_match() {
        let wanted: std::collections::HashMap<u32, (f64, f64, f64, f64)> = [
            (10, (100.0, 200.0, 300.0, 200.0)),
            (11, (500.0, 200.0, 300.0, 200.0)),
        ]
        .into_iter()
        .collect();
        // exact + within-epsilon matches
        assert_eq!(correlate_frame_to_wid((100.0, 200.0, 300.0, 200.0), &wanted), Some(10));
        assert_eq!(correlate_frame_to_wid((501.0, 199.0, 300.5, 200.0), &wanted), Some(11));
        // out of epsilon -> no match
        assert_eq!(correlate_frame_to_wid((110.0, 200.0, 300.0, 200.0), &wanted), None);
        // ambiguous (two same-frame candidates) -> refuse
        let dup: std::collections::HashMap<u32, (f64, f64, f64, f64)> = [
            (10, (100.0, 200.0, 300.0, 200.0)),
            (11, (100.0, 200.0, 300.0, 200.0)),
        ]
        .into_iter()
        .collect();
        assert_eq!(correlate_frame_to_wid((100.0, 200.0, 300.0, 200.0), &dup), None);
    }

    #[test]
    fn frame_correlation_refuses_when_true_window_is_missing_from_candidates() {
        let universe: std::collections::HashMap<u32, (f64, f64, f64, f64)> = [
            (10, (100.0, 200.0, 300.0, 200.0)),
            (11, (100.0, 200.0, 300.0, 200.0)),
        ]
        .into_iter()
        .collect();
        let mut candidates = universe.clone();
        candidates.remove(&11);

        assert_eq!(
            correlate_frame_to_wid_with_universe(
                (100.0, 200.0, 300.0, 200.0),
                &CandidateFrames(candidates),
                &UniverseFrames(universe),
            ),
            Err(WindowIdentityError::FrameMatchAmbiguous)
        );
    }

    #[test]
    fn frame_correlation_refuses_a_near_tie_across_the_match_tolerance() {
        let wanted: std::collections::HashMap<u32, (f64, f64, f64, f64)> = [
            (10, (101.4, 200.0, 300.0, 200.0)),
            (11, (101.6, 200.0, 300.0, 200.0)),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            correlate_frame_to_wid_with_universe(
                (100.0, 200.0, 300.0, 200.0),
                &CandidateFrames(wanted.clone()),
                &UniverseFrames(wanted),
            ),
            Err(WindowIdentityError::FrameMatchAmbiguous)
        );
    }
}
