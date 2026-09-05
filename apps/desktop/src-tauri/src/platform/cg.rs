//! Small CoreGraphics window/cursor primitives shared by hover-tab,
//! share-border, telepointer, shortcuts, and diagnostics code.
//!
//! This module is deliberately leaf-only: it knows nothing about Tauri
//! windows, sessions, borders, or app state. It only wraps the read-only
//! `CGWindowListCopyWindowInfo` and cursor-position calls the app already used
//! in several places.

/// A window's on-screen frame in global, top-left-origin logical points.
/// Equivalent to takt's `ScreenshotCoordinateFrame`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowFrame {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[cfg(target_os = "macos")]
mod macos {
    use super::WindowFrame;
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{CFTypeRef, TCFType};
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds, kCGWindowIsOnscreen,
        kCGWindowLayer, kCGWindowListOptionAll, kCGWindowListOptionOnScreenOnly, kCGWindowName,
        kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
    };
    use std::os::raw::c_void;

    // CoreFoundation number types (CFNumberType values).
    const K_CF_NUMBER_SINT64: i64 = 4; // kCFNumberSInt64Type
    const K_CF_NUMBER_FLOAT64: i64 = 6; // kCFNumberFloat64Type

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFDictionaryGetValueIfPresent(
            dict: CFTypeRef,
            key: *const c_void,
            value: *mut *const c_void,
        ) -> bool;
        fn CFNumberGetValue(number: *const c_void, the_type: i64, value_ptr: *mut c_void) -> bool;
        fn CFBooleanGetValue(boolean: *const c_void) -> bool;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGRectMakeWithDictionaryRepresentation(dict: *const c_void, rect: *mut CGRect) -> bool;
        // The array elements are raw CGWindowID values cast to pointers (the
        // "Son of Grab" contract), NOT CFNumbers. Returns a CFArray of CGWindow
        // description dictionaries (create rule).
        fn CGWindowListCreateDescriptionFromArray(window_array: CFArrayRef) -> CFArrayRef;
        fn CFArrayCreate(
            allocator: *const c_void,
            values: *const *const c_void,
            num_values: isize,
            callbacks: *const c_void,
        ) -> CFArrayRef;
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *const c_void) -> CGPoint;
        fn CGEventSourceButtonState(state: u32, button: u32) -> bool;
        fn CGEventSourceKeyState(state: u32, key: u16) -> bool;
        fn CFRelease(cf: *const c_void);
    }

    /// One on-screen window entry from `CGWindowListCopyWindowInfo`, in the
    /// same global coordinate space as [`WindowFrame`].
    #[derive(Debug, Clone)]
    pub struct WindowEntry {
        pub number: i64,
        pub owner_pid: i64,
        pub owner_name: String,
        /// May be empty -- many windows publish no kCGWindowName.
        pub name: String,
        pub layer: i64,
        pub alpha: f64,
        pub x: f64,
        pub y: f64,
        pub w: f64,
        pub h: f64,
    }

    unsafe fn dict_value(dict: *const c_void, key: *const c_void) -> Option<*const c_void> {
        let mut val: *const c_void = std::ptr::null();
        // SAFETY: `dict` comes directly from CoreGraphics'
        // CGWindowListCopyWindowInfo result array and `key` is one of the
        // exported kCGWindow* CFString constants. The returned value is only
        // borrowed for the duration of this call path.
        if !unsafe { CFDictionaryGetValueIfPresent(dict as CFTypeRef, key, &mut val) }
            || val.is_null()
        {
            return None;
        }
        Some(val)
    }

    unsafe fn dict_i64(dict: *const c_void, key: *const c_void) -> Option<i64> {
        // SAFETY: `dict_value` validates the key exists and returns a non-null
        // borrowed CF value; callers pass keys whose values are CFNumber in the
        // CGWindow dictionary schema.
        let val = unsafe { dict_value(dict, key) }?;
        let mut out: i64 = 0;
        // SAFETY: `val` is a CFNumber for the requested numeric key and `out`
        // is a valid writable i64 buffer for kCFNumberSInt64Type.
        unsafe { CFNumberGetValue(val, K_CF_NUMBER_SINT64, &mut out as *mut i64 as *mut c_void) }
            .then_some(out)
    }

    unsafe fn dict_f64(dict: *const c_void, key: *const c_void) -> Option<f64> {
        // SAFETY: `dict_value` validates the key exists and returns a non-null
        // borrowed CF value; callers pass keys whose values are CFNumber in the
        // CGWindow dictionary schema.
        let val = unsafe { dict_value(dict, key) }?;
        let mut out: f64 = 0.0;
        // SAFETY: `val` is a CFNumber for the requested numeric key and `out`
        // is a valid writable f64 buffer for kCFNumberFloat64Type.
        unsafe {
            CFNumberGetValue(
                val,
                K_CF_NUMBER_FLOAT64,
                &mut out as *mut f64 as *mut c_void,
            )
        }
        .then_some(out)
    }

    unsafe fn dict_string(dict: *const c_void, key: *const c_void) -> Option<String> {
        // SAFETY: `dict_value` validates the key exists and returns a non-null
        // borrowed CF value. For string keys, CGWindow dictionaries store
        // CFString values owned by the dictionary.
        let val = unsafe { dict_value(dict, key) }?;
        // SAFETY: Get-rule: the dictionary owns this CFString. We wrap without
        // consuming and immediately copy it into a Rust String.
        Some(unsafe { CFString::wrap_under_get_rule(val as CFStringRef) }.to_string())
    }

    unsafe fn dict_bool(dict: *const c_void, key: *const c_void) -> Option<bool> {
        // SAFETY: `dict_value` validates the key; kCGWindowIsOnscreen stores a
        // CFBoolean, read via CFBooleanGetValue.
        let val = unsafe { dict_value(dict, key) }?;
        Some(unsafe { CFBooleanGetValue(val) })
    }

    unsafe fn dict_rect(dict: *const c_void, key: *const c_void) -> Option<CGRect> {
        // SAFETY: `dict_value` validates the key exists and returns a non-null
        // borrowed CF value. kCGWindowBounds is a CGRect dictionary suitable
        // for CGRectMakeWithDictionaryRepresentation.
        let val = unsafe { dict_value(dict, key) }?;
        let mut rect = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
        // SAFETY: `val` is the bounds dictionary from CoreGraphics, and `rect`
        // is a valid out pointer.
        unsafe { CGRectMakeWithDictionaryRepresentation(val, &mut rect) }.then_some(rect)
    }

    /// Cursor position via raw `CGEventCreate(NULL)` + `CGEventGetLocation`.
    /// Returns global top-left-origin logical points.
    pub fn cursor_position() -> Option<(f64, f64)> {
        // SAFETY: Passing NULL asks CoreGraphics for the default event source.
        // The created event follows the create rule and is released exactly
        // once after reading its location.
        unsafe {
            let event = CGEventCreate(std::ptr::null());
            if event.is_null() {
                return None;
            }
            let point = CGEventGetLocation(event);
            CFRelease(event);
            Some((point.x, point.y))
        }
    }

    /// Current HID left-button state, used for edge-triggered click-away
    /// dismissal of the pinned hover drawer.
    pub fn left_mouse_button_is_down() -> bool {
        unsafe { CGEventSourceButtonState(1, 0) }
    }

    /// Current HID Escape state (keycode 53).
    pub fn escape_is_down() -> bool {
        unsafe { CGEventSourceKeyState(1, 53) }
    }

    /// Fetch the CGWindow dictionary for a SINGLE id via
    /// `CGWindowListCreateDescriptionFromArray`, instead of enumerating every
    /// window and scanning (#743). Plan §9.6 measured the old full-list scan at
    /// ~800us (`OnScreenOnly`) to ~2315us (`OptionAll`) of WindowServer CPU per
    /// call; this targeted lookup is ~65us. Returns the borrowed dict inside a
    /// closure so its owning array outlives the reads.
    fn with_window_dict<R>(window_id: u32, f: impl FnOnce(*const c_void) -> R) -> Option<R> {
        use core_foundation::base::TCFType;
        // The id array holds the CGWindowID cast to a pointer, with NULL
        // callbacks -- the values ARE the window ids, not CF objects.
        let value = window_id as usize as *const c_void;
        // SAFETY: one integer-as-pointer element, NULL callbacks (no retain of
        // the "pointer"); both CFArrays follow the create rule and are released
        // exactly once.
        let descriptions: CFArray = unsafe {
            let ids = CFArrayCreate(std::ptr::null(), &value, 1, std::ptr::null());
            if ids.is_null() {
                return None;
            }
            let raw = CGWindowListCreateDescriptionFromArray(ids);
            CFRelease(ids as *const c_void);
            if raw.is_null() {
                return None;
            }
            TCFType::wrap_under_create_rule(raw)
        };
        // 0 or 1 entries for a single id.
        let dict = descriptions.get(0)?;
        Some(f(*dict as *const c_void))
    }

    /// Look up a specific window's current on-screen frame by CGWindowID.
    /// Preserves the old semantics: returns `None` for a window that exists but
    /// is NOT on screen (the old `OnScreenOnly` filter), so callers keep
    /// treating off-screen/minimized windows as "no frame".
    pub fn frame_for_window_id(window_id: u32) -> Option<WindowFrame> {
        with_window_dict(window_id, |dict| {
            // SAFETY: `dict` is the CGWindow description for this id, borrowed
            // from the array kept alive by `with_window_dict`; helpers read
            // typed values and skip missing/mismatched fields.
            unsafe {
                if !dict_bool(dict, kCGWindowIsOnscreen as *const c_void).unwrap_or(false) {
                    return None;
                }
                let rect = dict_rect(dict, kCGWindowBounds as *const c_void)?;
                Some(WindowFrame {
                    x: rect.origin.x.round() as i32,
                    y: rect.origin.y.round() as i32,
                    width: rect.size.width.round() as i32,
                    height: rect.size.height.round() as i32,
                })
            }
        })
        .flatten()
    }

    /// RAW f64 variant of [`frame_for_window_id`] for the gesture fast path
    /// (#747 §4): the registry snapshot carries raw geometry (§9.10), so the
    /// per-drag-event update must not round.
    pub fn frame_for_window_id_raw(window_id: u32) -> Option<(f64, f64, f64, f64)> {
        with_window_dict(window_id, |dict| {
            // SAFETY: same contract as `frame_for_window_id`.
            unsafe {
                if !dict_bool(dict, kCGWindowIsOnscreen as *const c_void).unwrap_or(false) {
                    return None;
                }
                let rect = dict_rect(dict, kCGWindowBounds as *const c_void)?;
                Some((
                    rect.origin.x,
                    rect.origin.y,
                    rect.size.width,
                    rect.size.height,
                ))
            }
        })
        .flatten()
    }

    /// Look up the owning process for a CGWindowID. Matches the old
    /// `OptionAll` semantics (a window off-screen / on another Space / minimized
    /// still resolves), which share-start focus restoration relies on.
    pub fn owner_pid_for_window_id(window_id: u32) -> Option<i32> {
        with_window_dict(window_id, |dict| {
            // SAFETY: see `frame_for_window_id`.
            unsafe {
                dict_i64(dict, kCGWindowOwnerPID as *const c_void)
                    .and_then(|p| i32::try_from(p).ok())
            }
        })
        .flatten()
    }

    /// Look up the owning process name for a CGWindowID. Matches `OptionAll`
    /// semantics (a window off-screen / on another Space / minimized still resolves).
    pub fn owner_name_for_window_id(window_id: u32) -> Option<String> {
        with_window_dict(window_id, |dict| unsafe {
            dict_string(dict, kCGWindowOwnerName as *const c_void)
        })
        .flatten()
    }

    /// Look up a specific window's CURRENT title (`kCGWindowName`) by
    /// CGWindowID. Matches `OptionAll` semantics like its siblings above (a
    /// window off-screen / on another Space / minimized still resolves).
    /// `None` when the window has no title at all (many windows publish no
    /// `kCGWindowName`) or an empty one -- #915's browser-URL refresh poller
    /// uses this to notice a browser window's title changing (e.g. Chrome
    /// retitling on every navigation) without paying the cost of a full
    /// `window_source::list()`/`onscreen_windows_lean()` enumeration on every
    /// poll.
    pub fn name_for_window_id(window_id: u32) -> Option<String> {
        with_window_dict(window_id, |dict| unsafe {
            dict_string(dict, kCGWindowName as *const c_void)
        })
        .flatten()
        .filter(|name| !name.is_empty())
    }

    /// Whether the window still exists anywhere, including minimized windows and
    /// windows on another Space (old `OptionAll` semantics). A per-id
    /// description query returns an entry iff the window exists.
    pub fn window_exists(window_id: u32) -> bool {
        with_window_dict(window_id, |_dict| ()).is_some()
    }

    /// All on-screen windows, in the front-to-back order
    /// `CGWindowListCopyWindowInfo` returns. Includes `name` / `owner_name`
    /// (two CFString reads per window). Only consumers that actually read those
    /// — `hover_tab`'s own-chrome check, `window_diag`'s log — should call this;
    /// everything else should use [`onscreen_windows_lean`], which skips the
    /// string marshaling (§9.2 measured names at ~1.5x the enumeration cost).
    pub fn onscreen_windows() -> Option<Vec<WindowEntry>> {
        windows_impl(kCGWindowListOptionOnScreenOnly, true)
    }

    /// Like [`onscreen_windows`] but WITHOUT `name` / `owner_name` (both left
    /// empty) — for consumers that read only number/pid/layer/alpha/bounds
    /// (`share_border`, `remote_control`'s blocking hit-test, occlusion). #743.
    pub fn onscreen_windows_lean() -> Option<Vec<WindowEntry>> {
        windows_impl(kCGWindowListOptionOnScreenOnly, false)
    }

    /// Every window, including minimized and other-Space windows, without
    /// names. Reserved for cold paths whose correctness requires OptionAll.
    pub fn all_windows_lean() -> Option<Vec<WindowEntry>> {
        windows_impl(kCGWindowListOptionAll, false)
    }

    fn windows_impl(option: u32, with_names: bool) -> Option<Vec<WindowEntry>> {
        let infos = copy_window_info(option, kCGNullWindowID)?;
        let mut out = Vec::new();
        for dict in infos.get_all_values() {
            // SAFETY: Each `dict` is a CGWindow dictionary borrowed from the
            // immutable array returned by CoreGraphics; helper functions only
            // read typed values and skip missing/mismatched fields.
            unsafe {
                let Some(rect) = dict_rect(dict, kCGWindowBounds as *const c_void) else {
                    continue;
                };
                let (owner_name, name) = if with_names {
                    (
                        dict_string(dict, kCGWindowOwnerName as *const c_void).unwrap_or_default(),
                        dict_string(dict, kCGWindowName as *const c_void).unwrap_or_default(),
                    )
                } else {
                    (String::new(), String::new())
                };
                out.push(WindowEntry {
                    number: dict_i64(dict, kCGWindowNumber as *const c_void).unwrap_or(-1),
                    owner_pid: dict_i64(dict, kCGWindowOwnerPID as *const c_void).unwrap_or(-1),
                    owner_name,
                    name,
                    layer: dict_i64(dict, kCGWindowLayer as *const c_void).unwrap_or(0),
                    alpha: dict_f64(dict, kCGWindowAlpha as *const c_void).unwrap_or(-1.0),
                    x: rect.origin.x,
                    y: rect.origin.y,
                    w: rect.size.width,
                    h: rect.size.height,
                });
            }
        }
        Some(out)
    }

    /// Estimate what fraction of `window_id`'s area is covered by opaque
    /// normal-level windows sitting in front of it (lower index in the
    /// front-to-back `CGWindowListCopyWindowInfo` order). Returns `Some(0.0)`
    /// for a fully visible window, `Some(1.0)` for fully occluded, or `None`
    /// if the window isn't in the on-screen list (minimized / other Space /
    /// closed — a different state from "occluded", and the caller should treat
    /// it as such). Diagnostic-grade: this sums covering rects' overlap and
    /// clamps to 1.0, so heavy mutual overlap between multiple coverers can
    /// over-count before the clamp; it's meant to answer "is this window
    /// visually buried right now?", not to be pixel-exact.
    pub fn occlusion_fraction(window_id: u32) -> Option<f64> {
        // Occlusion reads only layer/alpha/pid/bounds -- no window names (#743).
        let entries = onscreen_windows_lean()?;
        let target_idx = entries
            .iter()
            .position(|e| e.number >= 0 && e.number as u32 == window_id)?;
        let target = &entries[target_idx];
        let target_area = target.w * target.h;
        if target_area <= 0.0 {
            return Some(0.0);
        }
        let self_pid = std::process::id() as i64;
        let tx0 = target.x;
        let ty0 = target.y;
        let tx1 = target.x + target.w;
        let ty1 = target.y + target.h;
        let mut covered = 0.0_f64;
        for front in &entries[..target_idx] {
            // Only opaque, normal-level windows visually cover content. Skip
            // our own windows (share border/hover overlays) so Petal's own UI
            // is never mistaken for occlusion by the source app.
            if front.layer != 0 || front.alpha < 0.99 || front.owner_pid == self_pid {
                continue;
            }
            let left = front.x.max(tx0);
            let top = front.y.max(ty0);
            let right = (front.x + front.w).min(tx1);
            let bottom = (front.y + front.h).min(ty1);
            let ow = (right - left).max(0.0);
            let oh = (bottom - top).max(0.0);
            covered += ow * oh;
        }
        Some((covered / target_area).clamp(0.0, 1.0))
    }

    /// All on-screen windows as `(window_number, frame)`, in front-to-back
    /// order. This preserves share_border.rs's previous behavior: window
    /// number and bounds are required, and bounds are truncated to i32.
    pub fn onscreen_stack() -> Option<Vec<(i64, WindowFrame)>> {
        let infos = copy_window_info(kCGWindowListOptionOnScreenOnly, kCGNullWindowID)?;
        let mut out = Vec::new();
        for dict in infos.get_all_values() {
            // SAFETY: Each `dict` is a CGWindow dictionary borrowed from the
            // immutable array returned by CoreGraphics; helper functions only
            // read typed values and skip missing/mismatched fields.
            unsafe {
                let Some(number) = dict_i64(dict, kCGWindowNumber as *const c_void) else {
                    continue;
                };
                let Some(rect) = dict_rect(dict, kCGWindowBounds as *const c_void) else {
                    continue;
                };
                out.push((
                    number,
                    WindowFrame {
                        x: rect.origin.x as i32,
                        y: rect.origin.y as i32,
                        width: rect.size.width as i32,
                        height: rect.size.height as i32,
                    },
                ));
            }
        }
        Some(out)
    }
}

#[cfg(target_os = "macos")]
pub use macos::{
    all_windows_lean, cursor_position, escape_is_down, frame_for_window_id,
    frame_for_window_id_raw, left_mouse_button_is_down, name_for_window_id, occlusion_fraction,
    onscreen_stack, onscreen_windows, onscreen_windows_lean, owner_name_for_window_id,
    owner_pid_for_window_id, window_exists, WindowEntry,
};

#[cfg(not(target_os = "macos"))]
pub fn occlusion_fraction(_window_id: u32) -> Option<f64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn cursor_position() -> Option<(f64, f64)> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn left_mouse_button_is_down() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn escape_is_down() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn frame_for_window_id(_window_id: u32) -> Option<WindowFrame> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn owner_pid_for_window_id(_window_id: u32) -> Option<i32> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn owner_name_for_window_id(_window_id: u32) -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn name_for_window_id(_window_id: u32) -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn window_exists(_window_id: u32) -> bool {
    false
}
