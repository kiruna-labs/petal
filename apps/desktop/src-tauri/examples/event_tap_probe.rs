//! #654 spike, question 6: what can a listen-only `CGEventTap` observe under
//! this process's TCC grants, and does keyboard observation require the
//! **Input Monitoring** grant that Petal does NOT hold (it holds Screen
//! Recording + Accessibility)?
//!
//! Phase 3 (#658) wants a listen-only tap to count the sharer's *physical*
//! input (events NOT carrying Petal's synthetic-source marker) so it can
//! auto-revoke agent control the instant the human takes over. If a
//! keyboard-observing tap needs Input Monitoring, #658 must either plan that
//! grant's UX or fall back to a mouse-only tap + AX-observer hybrid. Fail-closed
//! means: if the tap can't be created, the whole click/key/scroll tier of
//! agent control simply never grants — so this must be known before #658.
//!
//! ## What this standalone probe can and cannot conclude
//!
//! This example binary is ad-hoc signed with its OWN code identity and holds
//! NO TCC grants (not Accessibility, not Input Monitoring). So:
//! - If a keyboard-observing listen-only tap is created AND actually receives
//!   keyDown events here → NO special grant is required; Petal (which has more)
//!   definitely works. Strong, complete answer.
//! - If the tap is created but keyDown events never arrive (the tap is placed
//!   in a disabled state until the grant is present) → SOME grant is required.
//!   This binary can't say whether Accessibility alone suffices, because it has
//!   neither grant. Resolving Accessibility-vs-Input-Monitoring then needs an
//!   in-Petal-process run (Petal has Accessibility but not Input Monitoring) —
//!   noted in the finding, not faked here.
//!
//! It also prints the current process's `AXIsProcessTrusted()` (Accessibility)
//! and `CGPreflightListenEventAccess()` (Input Monitoring) so the run's grant
//! context is explicit in the evidence.
//!
//! Usage: `cargo run --example event_tap_probe -- [seconds]`  (default 12s).
//! Move the mouse and TYPE during the window; the probe prints per-class counts.
//! Requires no Gemini key and no LiveKit — pure local macOS.

// Apple's `kCGEvent*` symbol names are matched as patterns below; keep them.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

// ---- CoreGraphics / ApplicationServices FFI --------------------------------
//
// Declared by hand (same style as the repo's other probes and remote_control's
// AX FFI) rather than leaning on a specific core-graphics wrapper version.

type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CGEventTapProxy = *const c_void;
type CGEventRef = *mut c_void;

// CGEventTapCallBack: extern "C" fn(proxy, type, event, user) -> event
type CGEventTapCallBack = extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

#[allow(non_upper_case_globals)]
const kCGSessionEventTap: u32 = 1; // session-level tap
#[allow(non_upper_case_globals)]
const kCGHeadInsertEventTap: u32 = 0; // placement: before existing taps
#[allow(non_upper_case_globals)]
const kCGEventTapOptionListenOnly: u32 = 1; // passive: cannot modify/drop events

// Event type bits we care about for the mask (1 << CGEventType).
#[allow(non_upper_case_globals)]
const kCGEventLeftMouseDown: u32 = 1;
#[allow(non_upper_case_globals)]
const kCGEventMouseMoved: u32 = 5;
#[allow(non_upper_case_globals)]
const kCGEventKeyDown: u32 = 10;
#[allow(non_upper_case_globals)]
const kCGEventScrollWheel: u32 = 22;

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
    // Input Monitoring (kIOHIDRequestTypeListenEvent) TCC state, macOS 10.15+.
    fn CGPreflightListenEventAccess() -> bool;
    // Provenance round-trip (the #658 takeover-detector mechanism).
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventSourceCreate(state_id: u32) -> CGEventSourceRef;
    fn CGEventSourceSetUserData(source: CGEventSourceRef, user_data: i64);
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        point: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventPost(tap: u32, event: CGEventRef);
}

type CGEventSourceRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source_handled: bool)
        -> i32;
    static kCFRunLoopDefaultMode: CFStringRef;
}

// ---- observed counters (one per class) -------------------------------------

static MOUSE_MOVED: AtomicU64 = AtomicU64::new(0);
static LEFT_DOWN: AtomicU64 = AtomicU64::new(0);
static KEY_DOWN: AtomicU64 = AtomicU64::new(0);
static SCROLL: AtomicU64 = AtomicU64::new(0);
static OTHER: AtomicU64 = AtomicU64::new(0);

/// Events observed carrying OUR provenance marker (i.e. synthetic, ours).
static MARKED: AtomicU64 = AtomicU64::new(0);
/// Events observed WITHOUT our marker — what #658's takeover detector counts
/// as "the human is driving". Unreadable provenance counts as physical
/// (fail toward revocation).
static UNMARKED: AtomicU64 = AtomicU64::new(0);

/// Petal's synthetic-event provenance marker, mirroring takt's
/// `SYNTHETIC_EVENT_MARKER` ("TAKTSYNT"). Value here is "PETLSYNT".
const SYNTHETIC_EVENT_MARKER: i64 = 0x5045_544C_5359_4E54;

/// `kCGEventSourceUserData` — the event field carrying the marker.
const EVENT_SOURCE_USER_DATA_FIELD: u32 = 42;

extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    _user: *mut c_void,
) -> CGEventRef {
    match event_type {
        kCGEventMouseMoved => MOUSE_MOVED.fetch_add(1, Ordering::Relaxed),
        kCGEventLeftMouseDown => LEFT_DOWN.fetch_add(1, Ordering::Relaxed),
        kCGEventKeyDown => KEY_DOWN.fetch_add(1, Ordering::Relaxed),
        kCGEventScrollWheel => SCROLL.fetch_add(1, Ordering::Relaxed),
        _ => OTHER.fetch_add(1, Ordering::Relaxed),
    };
    // Provenance check — the mechanism #658's auto-revoke rests on.
    // SAFETY: `event` is valid for the duration of the callback.
    let user_data = unsafe { CGEventGetIntegerValueField(event, EVENT_SOURCE_USER_DATA_FIELD) };
    if user_data == SYNTHETIC_EVENT_MARKER {
        MARKED.fetch_add(1, Ordering::Relaxed);
    } else {
        UNMARKED.fetch_add(1, Ordering::Relaxed);
    }
    // Listen-only: return the event unmodified.
    event
}

fn main() {
    let seconds: f64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(12.0);

    // Report the grant context this run executes under, so the evidence is
    // self-describing.
    let ax_trusted = unsafe { AXIsProcessTrusted() };
    let input_monitoring = unsafe { CGPreflightListenEventAccess() };
    println!("event_tap_probe: grant context for THIS process identity:");
    println!("  Accessibility (AXIsProcessTrusted):        {ax_trusted}");
    println!("  Input Monitoring (CGPreflightListenEvent): {input_monitoring}");
    println!(
        "  (this standalone binary is expected to have NEITHER unless previously granted; Petal has Accessibility but not Input Monitoring)"
    );

    let mask: u64 = (1u64 << kCGEventLeftMouseDown)
        | (1u64 << kCGEventMouseMoved)
        | (1u64 << kCGEventKeyDown)
        | (1u64 << kCGEventScrollWheel);

    // SAFETY: standard CGEventTap creation. A null return means the OS refused
    // to create the tap (typically: the required TCC grant is absent).
    let tap = unsafe {
        CGEventTapCreate(
            kCGSessionEventTap,
            kCGHeadInsertEventTap,
            kCGEventTapOptionListenOnly,
            mask,
            tap_callback,
            std::ptr::null_mut(),
        )
    };

    if tap.is_null() {
        println!(
            "\nRESULT: CGEventTapCreate returned NULL — a listen-only tap for these classes could NOT be created under this identity's grants."
        );
        println!(
            "  → A grant is required. Whether Accessibility alone suffices (Petal has it) vs. Input Monitoring being mandatory must be resolved by an in-Petal-process run; this standalone binary holds neither grant so it cannot distinguish them."
        );
        std::process::exit(3);
    }

    unsafe {
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
        if source.is_null() {
            eprintln!("event_tap_probe: failed to create run loop source");
            std::process::exit(4);
        }
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
        CGEventTapEnable(tap, true);
    }

    println!(
        "\nTap created. Observing for {seconds:.0}s — MOVE THE MOUSE and TYPE now (into any app)…"
    );

    // Pump the run loop in short slices so the callback fires; total ~seconds.
    let slice = 0.25_f64;
    let mut elapsed = 0.0;
    while elapsed < seconds {
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, slice, false);
        }
        elapsed += slice;
    }

    // ---- deterministic self-test: provenance round-trip --------------------
    //
    // #658's auto-revoke works by marking every event Petal synthesizes and
    // treating UNMARKED events as "the human took over". That only works if the
    // marker survives the trip out through CGEventPost and back into the tap.
    // Test it with a mouseMoved to the cursor's CURRENT position — observable
    // by the tap, invisible to the user, and it disturbs nothing on screen.
    let marked_before = MARKED.load(Ordering::Relaxed);
    println!("\nSelf-test: posting one MARKED synthetic event (invisible: cursor stays put)…");
    unsafe {
        let probe_event = CGEventCreate(std::ptr::null_mut());
        let here = if probe_event.is_null() {
            CGPoint { x: 400.0, y: 400.0 }
        } else {
            let p = CGEventGetLocation(probe_event);
            CFRelease(probe_event as *const c_void);
            p
        };
        // kCGEventSourceStateHIDSystemState = 1
        let source = CGEventSourceCreate(1);
        if source.is_null() {
            println!("  (could not create an event source — skipping provenance self-test)");
        } else {
            CGEventSourceSetUserData(source, SYNTHETIC_EVENT_MARKER);
            let ev = CGEventCreateMouseEvent(source, kCGEventMouseMoved, here, 0);
            if !ev.is_null() {
                CGEventPost(0 /* kCGHIDEventTap */, ev);
                CFRelease(ev as *const c_void);
            }
            CFRelease(source as *const c_void);
        }
        // Let the tap drain the posted event.
        for _ in 0..8 {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, false);
        }
    }
    let marker_round_trips = MARKED.load(Ordering::Relaxed) > marked_before;

    let moved = MOUSE_MOVED.load(Ordering::Relaxed);
    let down = LEFT_DOWN.load(Ordering::Relaxed);
    let keys = KEY_DOWN.load(Ordering::Relaxed);
    let scroll = SCROLL.load(Ordering::Relaxed);
    let other = OTHER.load(Ordering::Relaxed);

    println!("\nObserved event counts:");
    println!("  mouseMoved:     {moved}");
    println!("  leftMouseDown:  {down}");
    println!("  keyDown:        {keys}");
    println!("  scrollWheel:    {scroll}");
    println!("  other:          {other}");
    println!(
        "  marked (ours):  {}",
        MARKED.load(Ordering::Relaxed)
    );
    println!(
        "  unmarked (phys):{}",
        UNMARKED.load(Ordering::Relaxed)
    );

    println!("\nProvenance self-test (the #658 auto-revoke mechanism):");
    if marker_round_trips {
        println!(
            "  PASS — a synthetic event marked with kCGEventSourceUserData was observed by the tap WITH its marker intact. Petal can reliably distinguish its own injected input from the user's, which is what makes takeover auto-revoke possible."
        );
    } else {
        println!(
            "  INCONCLUSIVE — the marked synthetic event was not observed back. Either posting was blocked, or the marker did not survive. #658 must re-verify this before relying on provenance-based revocation."
        );
    }

    println!("\nRESULT for #654 Q6:");
    let pointer_ok = moved > 0 || down > 0 || scroll > 0;
    if keys > 0 {
        println!(
            "  keyDown observed (Input Monitoring granted here: {input_monitoring}) → the keyboard class IS deliverable to a listen-only tap under these grants."
        );
    } else if pointer_ok && input_monitoring {
        println!(
            "  Pointer/scroll observed; keyDown zero WHILE Input Monitoring is granted → almost certainly nothing was typed during the window, NOT a permission block. Inconclusive for the keyboard class; re-run typing during the window for a definitive answer."
        );
    } else if pointer_ok {
        println!(
            "  Pointer/scroll observed but NO keyDown, and Input Monitoring is NOT granted → consistent with keyboard observation being gated on Input Monitoring while pointer/scroll is not."
        );
    } else {
        println!(
            "  No events observed at all — either the machine was idle, or the tap is installed-but-disabled pending a grant."
        );
    }
    println!(
        "\n  For #658 specifically: pointer+scroll observation is what a takeover detector minimally needs, and the provenance self-test above decides whether marker-based physical-vs-synthetic discrimination is viable at all. The remaining question — whether PETAL's own process (Accessibility granted, Input Monitoring NOT) can observe the keyboard class — needs a run from inside the app; this binary inherits its parent terminal's TCC identity (Input Monitoring reported: {input_monitoring})."
    );
}
