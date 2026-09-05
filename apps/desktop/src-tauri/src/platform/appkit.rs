//! Small AppKit/CoreAnimation operations shared by the compositor.
//!
//! This module is deliberately leaf-only: it owns the raw Objective-C
//! message sends needed to style/order/activate Tauri-backed `NSWindow`s and
//! attach the compositor's native display view. Callers remain responsible for
//! app state, labels, URLs, and ensuring these functions run on the main
//! thread.

#![cfg(target_os = "macos")]

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSRunningApplication,
};

use crate::native_display::DisplayLayer;

pub fn apply_window_border(
    window: &tauri::WebviewWindow,
    rgb: (f64, f64, f64),
    stroke_width: f64,
    corner_radius: f64,
) -> Result<(), String> {
    let ns_window_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;

    // SAFETY: `ns_window_ptr` is the AppKit `NSWindow*` exposed by Tauri for
    // this live `WebviewWindow`; all messages are sent while the caller is on
    // the main thread. The content view/layer are borrowed AppKit objects and
    // are only messaged synchronously here.
    unsafe {
        let ns_window = ns_window_ptr as *mut AnyObject;
        let clear_color: *mut AnyObject = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![ns_window, setOpaque: false];
        let _: () = msg_send![ns_window, setBackgroundColor: clear_color];
        let content_view: *mut AnyObject = msg_send![ns_window, contentView];
        if content_view.is_null() {
            return Ok(());
        }
        let _: () = msg_send![content_view, setWantsLayer: true];
        let layer: *mut AnyObject = msg_send![content_view, layer];
        if layer.is_null() {
            return Ok(());
        }
        let color: *mut AnyObject = msg_send![
            class!(NSColor),
            colorWithSRGBRed: rgb.0,
            green: rgb.1,
            blue: rgb.2,
            alpha: 1.0f64
        ];
        let cg_color: *mut std::ffi::c_void = msg_send![color, CGColor];
        let _: () = msg_send![layer, setBorderWidth: stroke_width];
        let _: () = msg_send![layer, setBorderColor: cg_color];
        let _: () = msg_send![layer, setCornerRadius: corner_radius];
        let _: () = msg_send![layer, setMasksToBounds: true];
    }

    Ok(())
}

pub fn attach_display_layer(
    window: &tauri::WebviewWindow,
    display: &DisplayLayer,
    content_width: f64,
    content_height: f64,
    debug_background: bool,
) -> Result<i64, String> {
    let ns_window_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;

    // SAFETY: `ns_window_ptr` is a live AppKit `NSWindow*` from Tauri and
    // `display.as_view_ptr()` / `display.as_layer_ptr()` are retained AppKit /
    // CoreAnimation objects owned by `DisplayLayer`. The caller performs this
    // only on the main thread, which AppKit and CALayer view hierarchy
    // mutation require.
    unsafe {
        let ns_window = ns_window_ptr as *mut AnyObject;
        let content_view: *mut AnyObject = msg_send![ns_window, contentView];
        if content_view.is_null() {
            return Err("contentView unavailable".into());
        }
        let _: () = msg_send![content_view, setWantsLayer: true];

        // Size the layer-hosting NSView to the CONTENT area the caller names
        // (the region below the header strip), never to the content view's
        // full bounds. The panel's content view is `HEADER_HEIGHT` taller
        // than the video content area; the display layer's gravity is
        // `resizeAspect`, so a view that fills the full bounds fits the video
        // by width and centres it vertically in the too-tall box — painting
        // the video HEADER_HEIGHT/2 too high so it overlaps the lower half
        // of the header strip, and leaving a transparent ~HEADER_HEIGHT/2
        // gap at the window BOTTOM, on every attach (defect E2, 2026-07-30;
        // same mechanism as the archived ~18px letterbox note).
        // `settle_panel_content_geometry` (compositor.rs) writes the identical
        // `(0, 0, width, content_height)` frame on every settled resize —
        // this must agree with it from the very first frame.
        //
        // Coordinates are AppKit bottom-up: the content area occupies
        // y ∈ [0, content_height) and the header the top HEADER_HEIGHT pt.
        // The autoresizing mask keeps the top margin (the header strip)
        // fixed while width/height track later panel resizes.
        display.set_contents_scale(window.scale_factor().unwrap_or(1.0));
        display.set_frame(0.0, 0.0, content_width.max(1.0), content_height.max(1.0));
        let view_ptr = display.as_view_ptr();

        const NS_VIEW_WIDTH_SIZABLE: u64 = 2;
        const NS_VIEW_HEIGHT_SIZABLE: u64 = 16;
        let _: () = msg_send![
            view_ptr,
            setAutoresizingMask: NS_VIEW_WIDTH_SIZABLE | NS_VIEW_HEIGHT_SIZABLE
        ];
        let _: () = msg_send![content_view, addSubview: view_ptr];

        let window_number: i64 = msg_send![ns_window, windowNumber];

        if debug_background {
            let layer_ptr = display.as_layer_ptr();
            let red: *mut AnyObject = msg_send![class!(NSColor), redColor];
            let cg: *mut std::ffi::c_void = msg_send![red, CGColor];
            let _: () = msg_send![layer_ptr, setBackgroundColor: cg];
        }

        Ok(window_number)
    }
}

/// Return the WindowServer id for a live Tauri-backed AppKit window.
///
/// This is deliberately a tiny, main-thread-only primitive. The cockpit uses
/// it to bind an authenticated remote compositor key to the exact panel that
/// received frames; scanning by process id alone can select chrome instead of
/// video content.
pub fn window_number(window: &tauri::WebviewWindow) -> Result<u32, String> {
    let ns_window_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;
    // SAFETY: `ns_window_ptr` is a live NSWindow and this helper is only
    // invoked from compositor code already marshalled onto the main thread.
    let number: i64 = unsafe {
        let ns_window = ns_window_ptr as *mut AnyObject;
        msg_send![ns_window, windowNumber]
    };
    u32::try_from(number).map_err(|_| format!("invalid NSWindow number {number}"))
}

/// Whether an AppKit window is hidden or completely covered by other windows.
/// `NSWindowOcclusionStateVisible` is bit 1. This is a read-only query; the
/// caller is responsible for performing it on the main thread when required.
pub fn is_fully_occluded(window: &tauri::WebviewWindow) -> bool {
    let Ok(ns_ptr) = window.ns_window() else {
        return false;
    };
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let visible: bool = msg_send![ns, isVisible];
        let occlusion_state: u64 = msg_send![ns, occlusionState];
        !visible || (occlusion_state & (1 << 1)) == 0
    }
}

/// Allow AppKit-managed tooltips in a window while Petal is inactive. This is
/// required for the hover tab because its nonactivating panel sits over the
/// user's currently active application.
pub fn allow_tooltips_when_application_inactive(
    window: &tauri::WebviewWindow,
) -> Result<(), String> {
    let _marker = objc2::MainThreadMarker::new().ok_or_else(|| {
        "allow_tooltips_when_application_inactive must run on the main thread".to_string()
    })?;
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;
    // SAFETY: `ns_ptr` is Tauri's live NSWindow and this setter is called on
    // the AppKit main thread during hover-tab setup.
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let _: () = msg_send![ns, setAllowsToolTipsWhenApplicationIsInactive: true];
    }
    Ok(())
}

/// Set the AppKit tooltip on a native view. Unlike an HTML `title`, this
/// uses AppKit's own tracking/default delay and does not depend on WKWebView
/// hit-testing inside the non-key hover panel.
pub fn set_view_tooltip(view: &objc2_app_kit::NSView, tooltip: &str) -> Result<(), String> {
    let _marker = objc2::MainThreadMarker::new()
        .ok_or_else(|| "set_view_tooltip must run on the main thread".to_string())?;
    let text = objc2_foundation::NSString::from_str(tooltip);
    view.setToolTip(Some(&text));
    Ok(())
}

pub fn is_main_thread() -> bool {
    unsafe { msg_send![class!(NSThread), isMainThread] }
}

pub fn order_below_anchor(window: &tauri::WebviewWindow, anchor: i64) -> Result<(), String> {
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;

    // SAFETY: `ns_ptr` is Tauri's live `NSWindow*` for this webview window.
    // Ordering and level changes are AppKit operations and the caller only
    // invokes this from code already marshalled to the main thread.
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let _: () = msg_send![ns, setLevel: 0isize];
        // NSWindowBelow = -1. This is a one-shot passive placement; explicit
        // user activation still uses `activate_window`.
        let _: () = msg_send![ns, orderWindow: -1isize, relativeTo: anchor as isize];
    }

    Ok(())
}

/// Order a normal-level child webview immediately above its normal-level
/// parent panel. This is relative ordering, not floating/global ordering.
pub fn order_above_panel(
    chrome: &tauri::WebviewWindow,
    panel: &tauri::WebviewWindow,
) -> Result<(), String> {
    let chrome_ptr = chrome
        .ns_window()
        .map_err(|e| format!("chrome ns_window unavailable: {e}"))?;
    let panel_ptr = panel
        .ns_window()
        .map_err(|e| format!("panel ns_window unavailable: {e}"))?;

    // SAFETY: both pointers are live NSWindows and this helper is called from
    // compositor paths already marshalled to the AppKit main thread.
    unsafe {
        let chrome_ns = chrome_ptr as *mut AnyObject;
        let panel_ns = panel_ptr as *mut AnyObject;
        let panel_number: i64 = msg_send![panel_ns, windowNumber];
        if panel_number <= 0 {
            return Err(format!("invalid panel window number {panel_number}"));
        }
        let _: () = msg_send![chrome_ns, setLevel: 0isize];
        let _: () = msg_send![chrome_ns, orderWindow: 1isize, relativeTo: panel_number as isize];
    }

    Ok(())
}

/// Attach `child` to `parent` as an AppKit child window
/// (`addChildWindow:ordered:`, `NSWindowAbove` = 1), so `child` automatically
/// follows `parent`'s position during a native drag/move -- the same
/// mechanism `WebviewWindowBuilder::parent()` already gives control/pointer
/// (see `compositor.rs`'s `create_chrome_webview`) for a window built WITHOUT
/// that tauri-level API (#844: `PanelBuilder` has no equivalent `.parent()`).
///
/// Callers must only attach a window that is ALREADY visible: `ordered:`
/// itself can order a hidden window onto the screen (the same effect
/// `order_above_panel`/`order_below_anchor` above already document for
/// `orderWindow:relativeTo:`), so attaching before showing risks flashing
/// the child on screen before its own reveal path meant to. Pair every call
/// with `remove_child_window` wherever that window is hidden.
pub fn add_child_window_above(
    parent: &tauri::WebviewWindow,
    child: &tauri::WebviewWindow,
) -> Result<(), String> {
    let parent_ptr = parent
        .ns_window()
        .map_err(|e| format!("parent ns_window unavailable: {e}"))?;
    let child_ptr = child
        .ns_window()
        .map_err(|e| format!("child ns_window unavailable: {e}"))?;

    // SAFETY: both pointers are live NSWindows and this helper is called from
    // compositor paths already marshalled to the AppKit main thread.
    unsafe {
        let parent_ns = parent_ptr as *mut AnyObject;
        let child_ns = child_ptr as *mut AnyObject;
        let _: () = msg_send![parent_ns, addChildWindow: child_ns, ordered: 1isize];
    }

    Ok(())
}

/// Detach `child` from `parent` (`removeChildWindow:`). Safe to call even if
/// `child` is not currently attached -- AppKit documents this as a no-op in
/// that case, which is what lets every hide path for an attach-on-show
/// window call this unconditionally rather than tracking attach state
/// separately.
pub fn remove_child_window(
    parent: &tauri::WebviewWindow,
    child: &tauri::WebviewWindow,
) -> Result<(), String> {
    let parent_ptr = parent
        .ns_window()
        .map_err(|e| format!("parent ns_window unavailable: {e}"))?;
    let child_ptr = child
        .ns_window()
        .map_err(|e| format!("child ns_window unavailable: {e}"))?;

    // SAFETY: same as `add_child_window_above` above. Wrapped in
    // `objc2::exception::catch` so every unconditional hide-path caller is
    // covered uniformly -- an unexpected AppKit raise here must degrade to a
    // logged error, not abort the app (#844 review note; `remove_window`'s
    // teardown already ran inside its own catch, the toggle-close did not).
    let result = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
        let parent_ns = parent_ptr as *mut AnyObject;
        let child_ns = child_ptr as *mut AnyObject;
        let _: () = msg_send![parent_ns, removeChildWindow: child_ns];
    }));
    result.map_err(|e| format!("removeChildWindow: raised: {e:?}"))
}

pub fn order_front_without_activating(window: &tauri::WebviewWindow) -> Result<(), String> {
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;

    // SAFETY: `ns_ptr` is Tauri's live `NSWindow*` for this window and the
    // caller runs on the AppKit main thread. `orderFrontRegardless` raises the
    // picker without making Petal active, preserving the shared app's focus.
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let _: () = msg_send![ns, orderFrontRegardless];
    }

    Ok(())
}

/// Raise `window`'s panel to front and give it key-window status WITHOUT
/// activating the whole app (`[NSApp activateIgnoringOtherApps:YES]`).
///
/// Issue #356: `activate_window` below starts with app-wide activation,
/// which on reactivation lets AppKit restore key/front status to whichever
/// ordinary (non-panel) window was previously key -- for this app that's
/// always "main" (the gallery). The remote compositor panel
/// (`compositor.rs`'s `RemoteWindowPanel` config) is built with
/// `can_become_key_window: true` and NO nonactivating style mask, so it CAN
/// become key directly -- app-wide activation is not required to key it.
/// Skipping app activation removes the race that let the gallery win and
/// land on top of the panel being dragged/resized. Use this for
/// drag/resize/programmatic-activate call sites; full `activate_window`
/// remains available for more deliberate, app-level activation needs.
pub fn raise_panel_and_make_key(window: &tauri::WebviewWindow) -> Result<(), String> {
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;

    // SAFETY: `ns_ptr` is the live AppKit `NSWindow*` backing `window`, and
    // the caller runs on the main thread (AppKit window ordering/key-status
    // APIs require it; see the module doc comment).
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let _: () = msg_send![ns, setLevel: 0isize];
        let _: () = msg_send![ns, orderFrontRegardless];
        let _: () = msg_send![ns, makeKeyWindow];
    }

    Ok(())
}

/// Raise `window`'s panel to front WITHOUT giving it key-window status and
/// WITHOUT activating the app -- the ordering-only half of
/// `raise_panel_and_make_key`.
///
/// Issue #678: a click inside the remote-control or draw overlay child
/// window must raise the parent panel, but `makeKeyWindow` on the *panel*
/// would steal key status from the control child that the click is actually
/// targeting (see `compositor::raise_window_for_click`, which raises via
/// this function and then re-keys the child explicitly, atomically, on the
/// main thread). Callers that need to raise a panel out from under a click
/// on one of its children should use this, then re-key the intended target
/// window themselves.
///
/// `orderFrontRegardless` un-hides an ordered-out window, not just an
/// already-visible one (#445's finding, see `order_chrome_above_panel`'s own
/// guard) -- callers MUST only pass a window they have already confirmed is
/// currently open/visible (`raise_window_for_click`'s use of
/// `resolve_open_window_key`, which excludes retired windows, is what makes
/// this safe today). A future caller that skips that check could resurrect
/// a deliberately-hidden window.
pub fn raise_panel_only(window: &tauri::WebviewWindow) -> Result<(), String> {
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;

    // SAFETY: same as `raise_panel_and_make_key` above.
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        raise_via_level_bump(ns);
    }

    Ok(())
}

/// `NSFloatingWindowLevel`. High enough to clear the normal band, low enough
/// that the window is never left stranded above the menu bar if a caller
/// somehow fails to restore (see `raise_via_level_bump`).
const NS_FLOATING_WINDOW_LEVEL: isize = 3;
const NS_NORMAL_WINDOW_LEVEL: isize = 0;

/// The ONLY reliable way to bring a window to the front on macOS without
/// activating the app (#901, owner-confirmed from prior art): bump the level
/// to always-on-top, order front, then put the level straight back.
///
/// Why the obvious form does not work: `orderFrontRegardless` at
/// `NSNormalWindowLevel` only re-orders the window WITHIN its own level band
/// and within this app's windows. For a non-activating panel belonging to a
/// background app, that routinely leaves it behind other applications'
/// windows -- the window is "front" as far as AppKit is concerned and still
/// invisible to the user, which is exactly the "shares are impossible to
/// discover" report.
///
/// Changing a window's level re-inserts it at the FRONT of the destination
/// band, so bump-order-restore lands it frontmost among normal windows and
/// leaves it there. Restoring in the same main-thread turn is deliberate:
/// the window must never be left floating above other apps, and an
/// intermediate turn is what would let a user's own window get sandwiched.
///
/// Does NOT key the window, activate the app, or touch focus -- #677/#21 are
/// prior art for raising that steals focus being its own bug. Main thread
/// only (AppKit).
///
/// SAFETY: `ns` must be a live `NSWindow*`.
unsafe fn raise_via_level_bump(ns: *mut AnyObject) {
    let _: () = msg_send![ns, setLevel: NS_FLOATING_WINDOW_LEVEL];
    let _: () = msg_send![ns, orderFrontRegardless];
    let _: () = msg_send![ns, setLevel: NS_NORMAL_WINDOW_LEVEL];
}

/// Bring a remote share's panel to the front when it first appears (#901),
/// using the same level-bump recipe as `raise_panel_only` but without that
/// function's click-path contract. Callers MUST have confirmed the window is
/// currently open/visible: `orderFrontRegardless` un-hides an ordered-out
/// window (#445), so passing a retired window would resurrect it.
pub fn raise_panel_to_front(window: &tauri::WebviewWindow) -> Result<(), String> {
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;

    // SAFETY: `ns_ptr` is the live AppKit `NSWindow*` backing `window`; this
    // runs on the main thread via the caller's `platform::on_main` hop.
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        raise_via_level_bump(ns);
    }

    Ok(())
}

/// #823: put the app in Accessory activation policy -- no Dock tile, no
/// Cmd-Tab entry, and self-activation becomes a no-op. Env-gated for
/// harness-launched instances; the NSStatusItem menubar pill is unaffected
/// (status items are independent of activation policy). Main thread only.
pub fn set_accessory_activation_policy() -> Result<(), String> {
    let marker = objc2::MainThreadMarker::new()
        .ok_or_else(|| "set_accessory_activation_policy must run on the main thread".to_string())?;
    unsafe {
        let app = NSApplication::sharedApplication(marker);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
    Ok(())
}

pub fn activate_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;

    let mtm = objc2::MainThreadMarker::new()
        .ok_or_else(|| "activate_window must run on the main thread".to_string())?;

    // SAFETY: `ns_ptr` is the live AppKit `NSWindow*` backing `window`, and
    // `mtm` proves this call is executing on the main thread before touching
    // `NSApplication`/`NSWindow` activation APIs.
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let app_obj = NSApplication::sharedApplication(mtm);
        let _: () = msg_send![&*app_obj, activateIgnoringOtherApps: true];
        let _: () = msg_send![ns, setLevel: 0isize];
        let _: () = msg_send![ns, orderFrontRegardless];
        let _: () = msg_send![ns, makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![ns, makeMainWindow];
    }

    Ok(())
}

/// Observed outcome of the cockpit-only synthetic-source activation request.
///
/// This deliberately does not alter the generic product activation helper:
/// the cockpit needs a proof that its fixed-geometry QA source can actually
/// become key, while ordinary windows retain their established focus behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CockpitSourceActivation {
    pub regular_policy: bool,
    pub policy_change_accepted: bool,
    pub can_become_key: bool,
    pub activation_requested: bool,
    pub activation_accepted: bool,
    pub ns_app_activate_requested: bool,
    pub legacy_activate_requested: bool,
    pub app_active: bool,
    pub window_key: bool,
    pub window_visible: bool,
    pub geometry_matches: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CockpitActivationPlan {
    pub modern_requested: bool,
    pub legacy_requested: bool,
}

pub(crate) fn cockpit_activation_plan(
    ns_application_activate_available: bool,
    app_active_after_primary_request: bool,
) -> CockpitActivationPlan {
    CockpitActivationPlan {
        modern_requested: ns_application_activate_available,
        legacy_requested: !ns_application_activate_available || !app_active_after_primary_request,
    }
}

/// Convert AppKit/Tauri physical pixels back to logical source coordinates.
/// A 960x600 fixed test surface is 1920x1200 physical pixels on a 2x display.
pub(crate) fn cockpit_source_geometry_matches(
    physical_width: u32,
    physical_height: u32,
    scale_factor: f64,
    expected_width: f64,
    expected_height: f64,
) -> bool {
    scale_factor.is_finite()
        && scale_factor > 0.0
        && ((physical_width as f64 / scale_factor) - expected_width).abs() < 0.5
        && ((physical_height as f64 / scale_factor) - expected_height).abs() < 0.5
}

/// Activate only the cockpit synthetic source and return observations rather
/// than treating an Objective-C call's return as completion. Must run on the
/// AppKit main thread.
pub fn activate_cockpit_source_window(
    window: &tauri::WebviewWindow,
    expected_width: f64,
    expected_height: f64,
) -> Result<CockpitSourceActivation, String> {
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;
    let marker = objc2::MainThreadMarker::new()
        .ok_or_else(|| "activate_cockpit_source_window must run on the main thread".to_string())?;
    let size = window
        .outer_size()
        .map_err(|e| format!("cockpit source outer_size unavailable: {e}"))?;
    let scale_factor = window
        .scale_factor()
        .map_err(|e| format!("cockpit source scale_factor unavailable: {e}"))?;

    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let app = NSApplication::sharedApplication(marker);
        let policy_change_accepted =
            app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        let regular_policy = app.activationPolicy() == NSApplicationActivationPolicy::Regular;
        let can_become_key: bool = msg_send![ns, canBecomeKeyWindow];
        let running = NSRunningApplication::currentApplication();
        let options = NSApplicationActivationOptions::ActivateAllWindows
            | NSApplicationActivationOptions::ActivateIgnoringOtherApps;
        let activation_accepted = running.activateWithOptions(options);
        let app_object: &AnyObject = &*app;
        let selector = objc2::sel!(activate);
        let responds: bool = msg_send![app_object, respondsToSelector: selector];
        // `-[NSApplication activate]` is macOS 14+. Keep the selector check so
        // the cockpit binary remains safe on macOS 13, where the legacy
        // activateIgnoringOtherApps request is the only available fallback.
        if responds {
            let _: () = msg_send![app_object, activate];
        } else {
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        }
        let _: () = msg_send![ns, orderFrontRegardless];
        let _: () = msg_send![ns, makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()];
        let app_active_after_primary_request: bool = msg_send![app_object, isActive];
        let activation_plan = cockpit_activation_plan(responds, app_active_after_primary_request);
        let ns_app_activate_requested = activation_plan.modern_requested;
        let legacy_activate_requested = activation_plan.legacy_requested;
        if legacy_activate_requested && responds {
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            let _: () = msg_send![ns, orderFrontRegardless];
            let _: () = msg_send![ns, makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()];
        }

        Ok(CockpitSourceActivation {
            regular_policy,
            policy_change_accepted,
            can_become_key,
            activation_requested: true,
            activation_accepted,
            ns_app_activate_requested,
            legacy_activate_requested,
            app_active: msg_send![&*app, isActive],
            window_key: msg_send![ns, isKeyWindow],
            window_visible: msg_send![ns, isVisible],
            geometry_matches: cockpit_source_geometry_matches(
                size.width,
                size.height,
                scale_factor,
                expected_width,
                expected_height,
            ),
        })
    }
}

/// Minimal AppKit readiness facts for the cockpit's app-owned synthetic source.
/// This intentionally exposes only window/app state, never webview contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowReadiness {
    pub app_active: bool,
    pub window_key: bool,
    pub window_visible: bool,
}

pub fn window_readiness(window: &tauri::WebviewWindow) -> Result<WindowReadiness, String> {
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;
    let marker = objc2::MainThreadMarker::new()
        .ok_or_else(|| "window_readiness must run on the main thread".to_string())?;
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let app = NSApplication::sharedApplication(marker);
        let app_active: bool = msg_send![&*app, isActive];
        let window_key: bool = msg_send![ns, isKeyWindow];
        let window_visible: bool = msg_send![ns, isVisible];
        Ok(WindowReadiness {
            app_active,
            window_key,
            window_visible,
        })
    }
}

pub fn activate_running_app(pid: i32) -> Result<bool, String> {
    if pid <= 0 {
        return Err(format!("invalid pid {pid}"));
    }
    let _mtm = objc2::MainThreadMarker::new()
        .ok_or_else(|| "activate_running_app must run on the main thread".to_string())?;

    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .ok_or_else(|| format!("no running application for pid {pid}"))?;
    // Issue #21: after a picker/pill click starts sharing, Petal must hand
    // active-app focus back to the source app instead of remaining foreground.
    #[allow(deprecated)]
    let options = NSApplicationActivationOptions::ActivateIgnoringOtherApps;
    Ok(app.activateWithOptions(options))
}

/// Fallback for the share-start focus handback when activating the source app
/// is declined (`activate_running_app` returns `Ok(false)` — common on macOS
/// 14/15's cooperative activation), when the source pid is unknown, or for a
/// full-display share. Petal RESIGNS its own active status so the foreground
/// returns to the previously-active (source) app, instead of Petal's main
/// window covering the shared window. Prefers the macOS 14+ cooperative
/// `-[NSApplication yieldActivationToApplication:]` (hands to the specific
/// source app) and falls back to `-[NSApplication deactivate]`.
pub fn yield_active_app_to(pid: i32) -> Result<(), String> {
    let mtm = objc2::MainThreadMarker::new()
        .ok_or_else(|| "yield_active_app_to must run on the main thread".to_string())?;
    let app_obj = NSApplication::sharedApplication(mtm);

    // SAFETY: on the main thread (proven by `mtm`); `app_obj` is the shared
    // NSApplication and `target` (when present) is a live NSRunningApplication.
    unsafe {
        let ns_app: &AnyObject = &*app_obj;
        if pid > 0 {
            if let Some(target) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            {
                let sel = objc2::sel!(yieldActivationToApplication:);
                let responds: bool = msg_send![ns_app, respondsToSelector: sel];
                if responds {
                    let target_ref: &AnyObject = &*target;
                    let _: () = msg_send![ns_app, yieldActivationToApplication: target_ref];
                    return Ok(());
                }
            }
        }
        #[allow(deprecated)]
        let _: () = msg_send![ns_app, deactivate];
    }
    Ok(())
}

// ── Focus diagnostics ───────────────────────────────────────────────────
// Used to instrument the share-start focus handback (#21) so the outcome is
// determinable from petal.log on a single Mac, rather than by eye: log the
// frontmost app + whether Petal is active before and after the handback.

unsafe fn nsstring_ptr_to_string(s: *mut AnyObject) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let utf8: *const std::os::raw::c_char = unsafe { msg_send![s, UTF8String] };
    if utf8.is_null() {
        return None;
    }
    Some(
        unsafe { std::ffi::CStr::from_ptr(utf8) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// A human-readable label for the current system-frontmost application, e.g.
/// `"Finder (com.apple.finder, pid 456)"`. Reads `NSWorkspace`'s
/// `frontmostApplication` via the ObjC runtime. Best-effort; returns a marker
/// string when unavailable.
pub fn frontmost_app_label() -> String {
    // SAFETY: plain ObjC message sends to NSWorkspace / NSRunningApplication;
    // this read is safe off the main thread.
    unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return "unknown".to_string();
        }
        let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return "none".to_string();
        }
        let pid: i32 = msg_send![app, processIdentifier];
        let name = nsstring_ptr_to_string(msg_send![app, localizedName]);
        let bundle = nsstring_ptr_to_string(msg_send![app, bundleIdentifier]);
        format!(
            "{} ({}, pid {pid})",
            name.as_deref().unwrap_or("?"),
            bundle.as_deref().unwrap_or("?")
        )
    }
}

/// Whether Petal itself is the active (frontmost) application.
pub fn app_is_active() -> bool {
    // SAFETY: `[NSApplication sharedApplication].isActive` — a simple BOOL read.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return false;
        }
        msg_send![app, isActive]
    }
}

pub fn disallow_window_tiling(window: &tauri::WebviewWindow) -> Result<(), String> {
    let ns_ptr = window
        .ns_window()
        .map_err(|e| format!("ns_window unavailable: {e}"))?;

    // SAFETY: `ns_ptr` is the live AppKit `NSWindow*` backing `window`; callers
    // invoke this from setup/on-main before touching AppKit collectionBehavior.
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let current: u64 = msg_send![ns, collectionBehavior];
        // NSWindowCollectionBehaviorFullScreenAllowsTiling = 1 << 11;
        // NSWindowCollectionBehaviorFullScreenDisallowsTiling = 1 << 12.
        let next = (current & !(1u64 << 11)) | (1u64 << 12);
        let _: () = msg_send![ns, setCollectionBehavior: next];
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{cockpit_activation_plan, cockpit_source_geometry_matches, CockpitActivationPlan};

    #[test]
    fn cockpit_source_geometry_normalizes_retina_physical_pixels() {
        assert!(cockpit_source_geometry_matches(960, 600, 1.0, 960.0, 600.0));
        assert!(cockpit_source_geometry_matches(
            1920, 1200, 2.0, 960.0, 600.0
        ));
        assert!(!cockpit_source_geometry_matches(
            1918, 1200, 2.0, 960.0, 600.0
        ));
        assert!(!cockpit_source_geometry_matches(
            1920, 1200, 0.0, 960.0, 600.0
        ));
    }

    #[test]
    fn cockpit_activation_plan_uses_legacy_when_modern_is_unavailable() {
        assert_eq!(
            cockpit_activation_plan(false, false),
            CockpitActivationPlan {
                modern_requested: false,
                legacy_requested: true,
            }
        );
    }

    #[test]
    fn cockpit_activation_plan_escalates_when_modern_leaves_app_inactive() {
        assert_eq!(
            cockpit_activation_plan(true, false),
            CockpitActivationPlan {
                modern_requested: true,
                legacy_requested: true,
            }
        );
    }

    #[test]
    fn cockpit_activation_plan_skips_legacy_when_app_is_active() {
        assert_eq!(
            cockpit_activation_plan(true, true),
            CockpitActivationPlan {
                modern_requested: true,
                legacy_requested: false,
            }
        );
    }

    /// Regression test for `activate_window`'s hardening (issue #681):
    /// `.expect("main thread")` on `MainThreadMarker::new()` used to panic
    /// when called off the main thread; it now bails with `Err` via
    /// `.ok_or_else(...)?`, matching every other `MainThreadMarker` call
    /// site in this file (`activate_cockpit_source_window`,
    /// `window_readiness`, `activate_running_app`, `yield_active_app_to`).
    ///
    /// `activate_window` itself can't be exercised end-to-end here: it takes
    /// a live `&tauri::WebviewWindow`, and this crate has no `tauri::test`
    /// mock-builder usage (see `autotest.rs`'s `dump_metrics_value` doc
    /// comment) to construct one off a real app. What IS directly testable,
    /// and is the load-bearing part of the fix, is that the guard clause
    /// itself -- the exact `MainThreadMarker::new().ok_or_else(...)`
    /// expression now used in `activate_window` -- returns a graceful `Err`
    /// instead of panicking under the precise condition that used to panic:
    /// running off the process's real main thread. Rust's `#[test]` harness
    /// runs every test on a spawned worker thread, never the process's
    /// actual main thread, so this test body IS that condition.
    #[test]
    fn main_thread_marker_guard_bails_gracefully_off_the_main_thread() {
        // The load-bearing precondition: Rust's `#[test]` harness runs every
        // test body on a spawned worker thread, never the process's actual
        // AppKit main thread, so `MainThreadMarker::new()` returning `None`
        // here reproduces the exact condition that used to make
        // `activate_window` panic via `.expect("main thread")`.
        assert!(
            objc2::MainThreadMarker::new().is_none(),
            "expected this test thread not to be the process main thread"
        );

        let result: Result<(), String> = objc2::MainThreadMarker::new()
            .ok_or_else(|| "activate_window must run on the main thread".to_string())
            .map(|_mtm| ());

        assert_eq!(
            result,
            Err("activate_window must run on the main thread".to_string())
        );
    }
}
