//! Small CoreGraphics window/cursor primitives shared by hover-tab,
//! share-border, telepointer, shortcuts, and diagnostics code.
//!
//! This module is deliberately leaf-only: it knows nothing about Tauri
//! windows, sessions, borders, or app state. It only wraps the read-only
//! `CGWindowListCopyWindowInfo` and cursor-position calls the app already used
//! in several places.

/// The slice of a window-list entry that occlusion needs: no titles, no app
/// names (privacy, #743). Kept outside the macOS module so the pure
/// computation below is testable from a synthetic list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OcclusionRect {
    pub number: i64,
    pub owner_pid: i64,
    pub layer: i64,
    pub alpha: f64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// How much of a window is covered, split by who covers it. Fractions are
/// 0.0..=1.0 of the target's area, measured on a coarse grid (diagnostic-
/// grade), and `combined` is the UNION, so overlapping coverers are not
/// double-counted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OcclusionBreakdown {
    /// Covered by other apps' opaque normal-level windows.
    pub foreign: f64,
    /// Covered by Petal's remote-share windows ONLY. Every other Petal
    /// window (share border, share overlay, hover tab, main window, picker,
    /// settings, AI chat) is excluded: the border and overlay sit directly
    /// above the shared window at its exact frame and are not coverage.
    pub petal: f64,
    /// Union of both.
    pub combined: f64,
    /// False when the remote-window set could not be read, so `petal` is
    /// not evidence either way.
    pub petal_known: bool,
}

impl OcclusionBreakdown {
    pub fn total(self) -> f64 {
        self.combined.clamp(0.0, 1.0)
    }
}

const OCCLUSION_GRID: usize = 32;

/// Pure occlusion computation over a front-to-back window list.
/// `petal_remote_numbers` are the compositor's remote-share window numbers
/// (`None` = unreadable). A window owned by `self_pid` that is not in that
/// set is ignored. `None` result when `window_id` is not in the list
/// (minimized, other Space, display sentinel).
pub fn occlusion_breakdown_from(
    entries: &[OcclusionRect],
    window_id: u32,
    self_pid: i64,
    petal_remote_numbers: Option<&[u32]>,
) -> Option<OcclusionBreakdown> {
    let target_idx = entries
        .iter()
        .position(|e| e.number >= 0 && e.number as u32 == window_id)?;
    let target = &entries[target_idx];
    let petal_known = petal_remote_numbers.is_some();
    if target.w * target.h <= 0.0 {
        return Some(OcclusionBreakdown { foreign: 0.0, petal: 0.0, combined: 0.0, petal_known });
    }
    let remote = petal_remote_numbers.unwrap_or(&[]);
    // (is_petal, rect) for every opaque coverer in front of the target.
    let coverers: Vec<(bool, &OcclusionRect)> = entries[..target_idx]
        .iter()
        .filter_map(|front| {
            if front.alpha < 0.99 {
                return None;
            }
            if front.owner_pid == self_pid {
                let is_remote = front.number >= 0 && remote.contains(&(front.number as u32));
                (is_remote && (0..=3).contains(&front.layer)).then_some((true, front))
            } else {
                (front.layer == 0).then_some((false, front))
            }
        })
        .collect();
    let (mut foreign, mut petal, mut any) = (0usize, 0usize, 0usize);
    for gy in 0..OCCLUSION_GRID {
        for gx in 0..OCCLUSION_GRID {
            let px = target.x + target.w * (gx as f64 + 0.5) / OCCLUSION_GRID as f64;
            let py = target.y + target.h * (gy as f64 + 0.5) / OCCLUSION_GRID as f64;
            let (mut by_foreign, mut by_petal) = (false, false);
            for (is_petal, r) in &coverers {
                if px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h {
                    if *is_petal {
                        by_petal = true;
                    } else {
                        by_foreign = true;
                    }
                }
            }
            foreign += by_foreign as usize;
            petal += by_petal as usize;
            any += (by_foreign || by_petal) as usize;
        }
    }
    let cells = (OCCLUSION_GRID * OCCLUSION_GRID) as f64;
    Some(OcclusionBreakdown {
        foreign: foreign as f64 / cells,
        petal: petal as f64 / cells,
        combined: any as f64 / cells,
        petal_known,
    })
}

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
        kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID, kCGWindowSharingState,
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

    /// `kCGWindowSharingState` for a window that exists anywhere (`None` when
    /// it does not; `Some(None)` when the key is absent). 0 is
    /// `kCGWindowSharingNone`: the app opted the window out of capture.
    pub fn sharing_state_for_window_id(window_id: u32) -> Option<Option<i64>> {
        // SAFETY: see `frame_for_window_id`; a missing key reads as `None`.
        with_window_dict(window_id, |dict| unsafe {
            dict_i64(dict, kCGWindowSharingState as *const c_void)
        })
    }

    /// Diagnostic facts for a window ScreenCaptureKit could not find even
    /// though it still exists (Sentry PETAL-DESKTOP-3C/-3D): on-screen flag,
    /// layer, `kCGWindowSharingState` (0 = the window opts out of capture)
    /// and owning app. `None` when the window no longer exists.
    pub fn capture_facts_for_window_id(window_id: u32) -> Option<String> {
        // SAFETY: see `frame_for_window_id`; every read tolerates a missing key.
        with_window_dict(window_id, |dict| unsafe {
            let number = |key: CFStringRef| {
                dict_i64(dict, key as *const c_void).map_or_else(|| "?".to_string(), |v| v.to_string())
            };
            format!(
                "on_screen={} layer={} sharing_state={} owner={:?}",
                dict_bool(dict, kCGWindowIsOnscreen as *const c_void).unwrap_or(false),
                number(kCGWindowLayer),
                number(kCGWindowSharingState),
                dict_string(dict, kCGWindowOwnerName as *const c_void).unwrap_or_default(),
            )
        })
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

    /// Fresh occlusion split into foreign vs Petal-owned coverage (#393).
    /// Unlike `occlusion_fraction` this counts Petal's own remote windows,
    /// separately, because they are the leading suspect for covered shares.
    pub fn occlusion_breakdown(
        window_id: u32,
        petal_remote_numbers: Option<&[u32]>,
    ) -> Option<super::OcclusionBreakdown> {
        let entries = onscreen_windows_lean()?;
        let rects: Vec<super::OcclusionRect> = entries
            .iter()
            .map(|e| super::OcclusionRect {
                number: e.number,
                owner_pid: e.owner_pid,
                layer: e.layer,
                alpha: e.alpha,
                x: e.x,
                y: e.y,
                w: e.w,
                h: e.h,
            })
            .collect();
        super::occlusion_breakdown_from(
            &rects,
            window_id,
            std::process::id() as i64,
            petal_remote_numbers,
        )
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
    all_windows_lean, capture_facts_for_window_id, cursor_position, escape_is_down,
    frame_for_window_id,
    frame_for_window_id_raw, left_mouse_button_is_down, name_for_window_id, occlusion_breakdown,
    occlusion_fraction,
    onscreen_stack, onscreen_windows, onscreen_windows_lean, owner_name_for_window_id,
    owner_pid_for_window_id, sharing_state_for_window_id, window_exists, WindowEntry,
};

#[cfg(not(target_os = "macos"))]
pub fn occlusion_fraction(_window_id: u32) -> Option<f64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn occlusion_breakdown(
    _window_id: u32,
    _petal_remote_numbers: Option<&[u32]>,
) -> Option<OcclusionBreakdown> {
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

#[cfg(test)]
mod occlusion_tests {
    use super::*;

    const SELF_PID: i64 = 100;
    const REMOTE: u32 = 1;

    fn rect(number: i64, owner_pid: i64, layer: i64, alpha: f64, x: f64, w: f64) -> OcclusionRect {
        OcclusionRect { number, owner_pid, layer, alpha, x, y: 0.0, w, h: 100.0 }
    }

    fn breakdown(entries: &[OcclusionRect]) -> OcclusionBreakdown {
        occlusion_breakdown_from(entries, 7, SELF_PID, Some(&[REMOTE])).unwrap()
    }

    /// #393: only the compositor's remote-share windows count as Petal
    /// coverage; foreign coverage is counted separately.
    #[test]
    fn breakdown_splits_foreign_and_remote_window_coverage() {
        let target = rect(7, 200, 0, 1.0, 0.0, 100.0);
        let entries = [
            rect(REMOTE as i64, SELF_PID, 0, 1.0, 0.0, 50.0),
            rect(3, 300, 0, 1.0, 80.0, 40.0),
            rect(4, 300, 0, 0.5, 0.0, 100.0),
            target,
            rect(5, 300, 0, 1.0, 0.0, 100.0),
        ];
        let b = breakdown(&entries);
        assert!((b.petal - 0.5).abs() < 0.04, "{b:?}");
        assert!((b.foreign - 0.2).abs() < 0.04, "{b:?}");
        assert!((b.total() - 0.7).abs() < 0.04, "{b:?}");
    }

    /// The share border and overlay are Petal windows at layer 0 directly
    /// above the shared window at its EXACT frame. They are not coverage.
    #[test]
    fn a_layer_zero_petal_border_at_the_exact_target_frame_is_not_coverage() {
        let entries = [rect(50, SELF_PID, 0, 1.0, 0.0, 100.0), rect(7, 200, 0, 1.0, 0.0, 100.0)];
        let b = breakdown(&entries);
        assert_eq!((b.petal, b.foreign, b.total()), (0.0, 0.0, 0.0));
    }

    #[test]
    fn a_remote_share_window_over_sixty_percent_is_petal_coverage() {
        let entries = [rect(REMOTE as i64, SELF_PID, 0, 1.0, 0.0, 60.0), rect(7, 200, 0, 1.0, 0.0, 100.0)];
        let b = breakdown(&entries);
        assert!((b.petal - 0.6).abs() < 0.04, "{b:?}");
        assert_eq!(b.foreign, 0.0);
    }

    #[test]
    fn the_petal_main_window_over_the_target_is_not_coverage() {
        let entries = [rect(60, SELF_PID, 0, 1.0, 0.0, 100.0), rect(7, 200, 0, 1.0, 0.0, 100.0)];
        assert_eq!(breakdown(&entries).total(), 0.0);
    }

    /// Overlapping coverers are a union, not a sum.
    #[test]
    fn overlapping_foreign_and_petal_coverers_are_not_double_counted() {
        let entries = [
            rect(REMOTE as i64, SELF_PID, 0, 1.0, 0.0, 60.0),
            rect(3, 300, 0, 1.0, 0.0, 60.0),
            rect(7, 200, 0, 1.0, 0.0, 100.0),
        ];
        let b = breakdown(&entries);
        assert!((b.total() - 0.6).abs() < 0.04, "{b:?}");
        assert!(b.foreign > 0.55 && b.petal > 0.55);
    }

    #[test]
    fn an_unreadable_remote_set_marks_petal_unknown() {
        let entries = [rect(REMOTE as i64, SELF_PID, 0, 1.0, 0.0, 60.0), rect(7, 200, 0, 1.0, 0.0, 100.0)];
        let b = occlusion_breakdown_from(&entries, 7, SELF_PID, None).unwrap();
        assert!(!b.petal_known);
        assert_eq!(b.petal, 0.0);
    }

    #[test]
    fn breakdown_is_none_for_a_window_not_on_screen() {
        let entries = [rect(1, 300, 0, 1.0, 0.0, 100.0)];
        assert_eq!(occlusion_breakdown_from(&entries, 99, SELF_PID, Some(&[])), None);
    }
}
