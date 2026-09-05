//! Gesture fast path (#747, plan §4): a listen-only mouse `CGEventTap` that
//! tracks the ONE window under a live title-bar drag with per-id frame reads
//! (65 µs, §9.2) published straight into the registry snapshot — so a drag is
//! pixel-followed at device rate without a single full window-list
//! enumeration. Same tap pattern as `ai_chat/takeover.rs` (the repo's proven
//! listen-only tap), same thread/runloop discipline.
//!
//! Degradation: tap creation fails on rigs without the Accessibility/input
//! TCC grant -> `gesture_live()` stays false, callers keep their full-sweep
//! behavior, and the tier line never claims the gesture path. The tap is
//! enabled only while in a room (idle cost: zero).

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

type CFMachPortRef = *mut c_void;
type CGEventRef = *mut c_void;
type CGEventTapProxy = *const c_void;
type CGEventTapCallBack =
    extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;
const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

#[repr(C)]
#[derive(Clone, Copy)]
struct CgPoint {
    x: f64,
    y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetLocation(event: CGEventRef) -> CgPoint;
}

extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> *mut c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    static kCFRunLoopDefaultMode: *const c_void;
}

/// Tap exists and is servicing events (tier signal).
static GESTURE_LIVE: AtomicBool = AtomicBool::new(false);
/// App handle for #761 event-driven pill nudges (set once at start()).
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
/// Whether the tap is currently enabled (in-room only).
static TAP: OnceLock<usize> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);

/// Active drag track (#761): the last REAL frame read, the cursor position at
/// that read, and whether the drag is RIGID — frame moving in lockstep with
/// the cursor. Rigid is what licenses cursor-lock dead reckoning downstream:
/// a mouse-down in window CONTENT (text selection) moves the cursor while the
/// frame stays put, and reckoning there would drag the pill off into space.
#[derive(Clone, Copy)]
pub struct GestureTrack {
    pub wid: u32,
    pub fx: f64,
    pub fy: f64,
    pub fw: f64,
    pub fh: f64,
    /// Cursor (global top-left points) at the instant of the frame read.
    pub cx: f64,
    pub cy: f64,
    /// Lockstep proven this gesture; LATCHED until mouse-up (see
    /// [`drag_rigidity`]).
    pub rigid: bool,
    /// Consecutive gross-divergence events (the latched state's only exit).
    pub diverge_streak: u8,
    /// EMA cursor velocity at EVENT rate (points/sec) — far less noisy than
    /// tick-rate estimates; feeds the #761 render-latency lead.
    pub vx: f64,
    pub vy: f64,
    pub at: Instant,
}

static TARGET: OnceLock<Mutex<Option<GestureTrack>>> = OnceLock::new();
fn target() -> &'static Mutex<Option<GestureTrack>> {
    TARGET.get_or_init(|| Mutex::new(None))
}

/// Rigidity decision (#761), pure, LATCHING: a title-bar drag cannot become a
/// content drag mid-gesture (the mouse stays down on the same title bar), so
/// once lockstep is proven (≤3px) the state LATCHES until mouse-up. Two live
/// rounds proved anything less flickers: the head-insert tap sees each event
/// BEFORE WindowServer applies it, so the frame read trails the cursor by a
/// full event-delta at speed (>15px on fast flicks), and every flicker snaps
/// the pill between reckoned/real — the reported clunk. The only exit is a
/// PERSISTENT gross divergence (safety net for exotic window behavior):
/// `diverge_streak` counts consecutive events off by >40px; 3 in a row exits.
/// Returns (rigid, new_streak).
pub fn drag_rigidity(
    was_rigid: bool,
    streak: u8,
    frame_dx: f64,
    frame_dy: f64,
    cursor_dx: f64,
    cursor_dy: f64,
) -> (bool, u8) {
    let moved = cursor_dx.abs() >= 1.0 || cursor_dy.abs() >= 1.0;
    if !moved {
        return (was_rigid, streak); // stationary instant: keep state
    }
    let dvx = (frame_dx - cursor_dx).abs();
    let dvy = (frame_dy - cursor_dy).abs();
    if was_rigid {
        let new_streak = if dvx > 40.0 || dvy > 40.0 {
            streak + 1
        } else {
            0
        };
        (new_streak < 3, new_streak.min(3))
    } else {
        (dvx <= 3.0 && dvy <= 3.0, 0)
    }
}

/// The current track for `wid`, if fresh within `max_ms` (#761 consumer read).
pub fn gesture_track_for(wid: u32, max_ms: u64) -> Option<GestureTrack> {
    let t = (*target().lock().expect("gesture target lock poisoned"))?;
    (t.wid == wid && t.at.elapsed().as_millis() as u64 <= max_ms).then_some(t)
}

pub fn gesture_live() -> bool {
    GESTURE_LIVE.load(Ordering::Relaxed)
}

/// Whether the gesture feed published a fresh frame for `wid` within `max_ms`.
/// Consumers (hover follow) use this to skip a full sweep while the gesture
/// path is actively tracking the exact window they follow.
pub fn gesture_fresh_for(wid: u32, max_ms: u64) -> bool {
    gesture_track_for(wid, max_ms).is_some()
}

extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    _user: *mut c_void,
) -> CGEventRef {
    match event_type {
        K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT | K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT => {
            // The OS silenced us; re-enable (listen-only taps are cheap) and
            // report honestly until events flow again.
            if let Some(&tap) = TAP.get() {
                unsafe { CGEventTapEnable(tap as CFMachPortRef, true) };
            }
        }
        K_CG_EVENT_LEFT_MOUSE_DOWN => {
            let p = unsafe { CGEventGetLocation(event) };
            let hit = crate::window_registry::global()
                .and_then(|reg| reg.topmost_foreign_at(p.x, p.y, std::process::id() as i32));
            *target().lock().expect("gesture target lock poisoned") = hit.and_then(|wid| {
                let (fx, fy, fw, fh) = crate::platform::cg::frame_for_window_id_raw(wid)?;
                Some(GestureTrack {
                    wid,
                    fx,
                    fy,
                    fw,
                    fh,
                    cx: p.x,
                    cy: p.y,
                    rigid: false, // proven per drag event, never assumed (#761)
                    diverge_streak: 0,
                    vx: 0.0,
                    vy: 0.0,
                    at: Instant::now(),
                })
            });
        }
        K_CG_EVENT_LEFT_MOUSE_DRAGGED => {
            let prev = *target().lock().expect("gesture target lock poisoned");
            if let Some(prev) = prev {
                let p = unsafe { CGEventGetLocation(event) };
                // One 65 µs CG per-id read + one snapshot publish; well inside
                // the tap-timeout budget, zero full enumerations. Geometry
                // truth stays CG: §9.15.2 — SLS bounds can differ from CG by a
                // per-window-class shadow-like margin, and published frames
                // feed pill/border placement (#416-class sensitivity).
                if let Some(raw) = crate::platform::cg::frame_for_window_id_raw(prev.wid) {
                    if let Some(reg) = crate::window_registry::global() {
                        reg.update_window_frame(prev.wid, raw.0, raw.1, raw.2, raw.3);
                    }
                    // Rigidity: did the frame move in lockstep with the cursor
                    // since the previous event? Sticky across stationary
                    // events mid-drag. (#761 dead-reckoning license.)
                    let (rigid, streak) = drag_rigidity(
                        prev.rigid,
                        prev.diverge_streak,
                        raw.0 - prev.fx,
                        raw.1 - prev.fy,
                        p.x - prev.cx,
                        p.y - prev.cy,
                    );
                    if rigid && !prev.rigid {
                        // once per gesture: the #761 dead-reckoning license
                        // engaged (diagnostic, like the classified-window line)
                        log::info!(
                            "winsrv: rigid drag detected for window {} -- cursor-lock reckoning engaged",
                            prev.wid
                        );
                    }
                    // Event-rate cursor velocity (EMA) for the render lead.
                    let dt = prev.at.elapsed().as_secs_f64().max(0.001);
                    let vx = prev.vx * 0.5 + ((p.x - prev.cx) / dt) * 0.5;
                    let vy = prev.vy * 0.5 + ((p.y - prev.cy) / dt) * 0.5;
                    *target().lock().expect("gesture target lock poisoned") = Some(GestureTrack {
                        wid: prev.wid,
                        fx: raw.0,
                        fy: raw.1,
                        fw: raw.2,
                        fh: raw.3,
                        cx: p.x,
                        cy: p.y,
                        rigid,
                        diverge_streak: streak,
                        vx,
                        vy,
                        at: Instant::now(),
                    });
                    // #761 event-driven nudge: reposition the pill in the SAME
                    // input-event rhythm the window moves in. Lead covers the
                    // remaining render latency (~1 frame); capped.
                    if rigid {
                        if let Some(app) = APP.get() {
                            // With SLS-direct panel moves the pipeline is
                            // ~one compositing cycle; a small residual lead
                            // only. (12ms tuned for the old main-thread path.)
                            let lead = 0.004;
                            let lx = (vx * lead).clamp(-32.0, 32.0);
                            let ly = (vy * lead).clamp(-32.0, 32.0);
                            let led = crate::platform::cg::WindowFrame {
                                x: (raw.0 + lx).round() as i32,
                                y: (raw.1 + ly).round() as i32,
                                width: raw.2.round() as i32,
                                height: raw.3.round() as i32,
                            };
                            crate::hover_tab::drag_nudge(app, prev.wid, led);
                            // #761: shared-window chrome rides the same event
                            // (border + telepointer overlay sit AT the window
                            // frame origin; drags never resize).
                            crate::share_border::drag_nudge_border(
                                prev.wid,
                                led.x as f64,
                                led.y as f64,
                            );
                            crate::share_overlay::drag_nudge_overlay(
                                prev.wid,
                                led.x as f64,
                                led.y as f64,
                            );
                        }
                    }
                }
            }
        }
        K_CG_EVENT_LEFT_MOUSE_UP => {
            let had = target()
                .lock()
                .expect("gesture target lock poisoned")
                .take()
                .is_some();
            if had {
                // Reconcile order/occlusion with a sweep soon (drag may have
                // raised the window).
                crate::platform::ax_observer::mark_dirty();
            }
        }
        _ => {}
    }
    event
}

/// Enable/disable the tap with room membership (idle cost zero).
pub fn set_enabled(enabled: bool) {
    if let Some(&tap) = TAP.get() {
        unsafe { CGEventTapEnable(tap as CFMachPortRef, enabled) };
        if !enabled {
            *target().lock().expect("gesture target lock poisoned") = None;
        }
    }
}

/// Start the tap thread (idempotent). Failure (no grant) leaves
/// `gesture_live()` false — callers keep full-sweep behavior.
pub fn start(app: &tauri::AppHandle) {
    let _ = APP.set(app.clone());
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("winsrv-gesture".into())
        .spawn(|| unsafe {
            let mask: u64 = (1u64 << K_CG_EVENT_LEFT_MOUSE_DOWN)
                | (1u64 << K_CG_EVENT_LEFT_MOUSE_UP)
                | (1u64 << K_CG_EVENT_LEFT_MOUSE_DRAGGED);
            let tap = CGEventTapCreate(
                K_CG_SESSION_EVENT_TAP,
                K_CG_HEAD_INSERT_EVENT_TAP,
                K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
                mask,
                tap_callback,
                std::ptr::null_mut(),
            );
            if tap.is_null() {
                log::info!(
                    "winsrv: gesture tap unavailable (CGEventTapCreate refused) -- drags stay on the sweep path"
                );
                return;
            }
            let _ = TAP.set(tap as usize);
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CGEventTapEnable(tap, false); // enabled per room membership
            GESTURE_LIVE.store(true, Ordering::Relaxed);
            log::info!("winsrv: gesture tap live (listen-only; enabled in-room)");
            CFRunLoopRun();
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #761 rigidity license: lockstep motion within epsilon = rigid; content
    /// drags (cursor moves, frame doesn't) and stationary events are not.
    #[test]
    fn rigidity_latches_until_persistent_gross_divergence() {
        // enter: lockstep within 3px
        assert_eq!(drag_rigidity(false, 0, 24.0, -13.0, 24.0, -13.0), (true, 0));
        assert_eq!(drag_rigidity(false, 0, 24.0, -13.0, 26.0, -11.5), (true, 0));
        // content drag never enters
        assert_eq!(drag_rigidity(false, 0, 0.0, 0.0, 24.0, -13.0).0, false);
        // fast-drag trailing does not enter (needs true lockstep once)...
        assert_eq!(drag_rigidity(false, 0, 16.0, 0.0, 24.0, 0.0).0, false);
        // ...but once LATCHED, even 30px trailing never exits (flicker fix)
        assert_eq!(drag_rigidity(true, 0, 0.0, 0.0, 30.0, 0.0), (true, 0));
        // gross divergence must PERSIST 3 events to exit
        let (r1, s1) = drag_rigidity(true, 0, 0.0, 0.0, 50.0, 0.0);
        assert!(r1 && s1 == 1, "one gross event: still rigid");
        let (r2, s2) = drag_rigidity(true, s1, 0.0, 0.0, 50.0, 0.0);
        assert!(r2 && s2 == 2, "two gross events: still rigid");
        let (r3, _) = drag_rigidity(true, s2, 0.0, 0.0, 50.0, 0.0);
        assert!(!r3, "three consecutive gross events: exit");
        // a clean event resets the streak
        let (_, s_reset) = drag_rigidity(true, 2, 24.0, 0.0, 24.0, 0.0);
        assert_eq!(s_reset, 0);
        // stationary instant keeps state
        assert_eq!(drag_rigidity(true, 0, 0.3, 0.2, 0.3, 0.2).0, true);
        assert_eq!(drag_rigidity(false, 0, 0.3, 0.2, 0.3, 0.2).0, false);
    }
}
