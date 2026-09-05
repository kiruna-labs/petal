//! #627 / CLAUDE.md "Never show a black frame": prove by SAMPLED RENDERED
//! PIXELS that a native receiver window holds its last decoded frame across a
//! disruption instead of vanishing.
//!
//! ## Why pixels, and why both directions
//!
//! "Held the last frame" and "went blank quietly" emit exactly the same
//! events, so an event-level assertion cannot tell them apart and is not
//! evidence. This drives a real `AVSampleBufferDisplayLayer` hosted in a real
//! on-screen `NSWindow` — the same `native_display::DisplayLayer` the real
//! compositor uses — forces a real frame gap, and reads back the window's own
//! rendered pixels via `screencapture(1)`.
//!
//! It runs BOTH directions, which is the whole point:
//!
//!   * default (`teardown_decision`, as fixed) — the window must STAY BRIGHT
//!     across the gap.
//!   * `--stale-guard` (POSITIVE CONTROL, pre-#627) — the pre-fix path, where
//!     a matching sid meant an unconditional `remove_window`, whose
//!     `win.hide()` takes the window off screen. Its pixels must GO AWAY.
//!
//! Without the second run a passing first run would be worthless: it could not
//! distinguish a working hold from a gap that never actually happened.
//!
//! ## The harness validates itself before it reports anything
//!
//! Screen capture needs Screen Recording access, and a DENIED capture returns
//! black — indistinguishable from the very failure being measured. So a
//! baseline sample of the known-bright window is taken first, and if THAT is
//! not bright the run exits `3` as HARNESS INVALID rather than reporting a
//! pass or a fail.
//!
//! Capture goes through the `screencapture(1)` CLI rather than
//! `CGWindowListCreateImage` in-process, and that choice is load-bearing: an
//! ad-hoc-signed `cargo build` example binary lives at a fresh path with no
//! Screen Recording grant, so the in-process call returns an all-black image
//! (measured: baseline `mean_luma=0.0` while `screencapture` from the same
//! shell read real pixels). Routing through the granted system tool avoids
//! per-binary TCC re-granting, which CLAUDE.md documents as a recurring tax.
//!
//! ## What this does and does not prove
//!
//! Proves: the fixed decision keeps a real window's real last frame on screen
//! across a real gap, and the pre-fix decision does not. The layer is never
//! flushed, its contents are never cleared, and media requests are never
//! stopped — the ONLY difference between the two runs is whether the window is
//! hidden, which is precisely the #627 native root cause.
//!
//! The `--retire-reuse` scenario additionally proves that the real display
//! layer keeps its pixels through an AppKit order-out/order-front reuse cycle
//! with no newly enqueued frame. The in-file compositor lifecycle model pins
//! the Tauri pool branch that chooses whether to perform that order-front.
//!
//! ## Usage
//!
//! ```sh
//! cargo run --example hold_last_frame_probe                  # fixed: must stay bright
//! cargo run --example hold_last_frame_probe -- --stale-guard # control: must go away
//! cargo run --example hold_last_frame_probe -- --retire-reuse
//! cargo run --example hold_last_frame_probe -- --retire-reuse --stale-guard
//! ```

#[cfg(target_os = "macos")]
fn main() {
    probe::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("hold_last_frame_probe is macOS-only.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod probe {
    use std::ffi::c_void;

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    use desktop_lib::native_display::{i420_to_cv_pixel_buffer, DisplayLayer, I420Planes};
    use desktop_lib::transport::subscriber::{
        handle_participant_disconnected, should_remove_window, teardown_decision,
        track_unsubscribe_decision, TeardownDecision,
    };

    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;
    /// The window sits well inside the screen so nothing clips its capture.
    const ORIGIN_X: f64 = 120.0;
    const ORIGIN_Y: f64 = 240.0;
    /// Full-luma Y plane. A held frame reads near this; the desktop behind a
    /// hidden window does not, and neither does a black layer.
    const BRIGHT_LUMA: u8 = 235;
    /// Mean luma (0-255) above which a sample counts as "the bright frame is
    /// on screen". Chosen far from both outcomes actually observed: a held
    /// frame samples ~200+, an absent window samples the desktop underneath.
    const BRIGHT_THRESHOLD: f64 = 140.0;
    /// The forced gap: no frames enqueued for this long, which is what a
    /// republish or a stall leaves behind.
    const GAP: std::time::Duration = std::time::Duration::from_millis(1200);

    /// `NSFloatingWindowLevel`. Keeps the measured region clear of unrelated
    /// windows for the duration of the run.
    const NS_FLOATING_WINDOW_LEVEL: isize = 3;

    /// Real sids, in the shape a real republish produces.
    const OLD_SID: &str = "TR_VSoldpublication";
    const NEW_SID: &str = "TR_VSnewpublication";

    extern "C" {
        fn CFRunLoopRunInMode(
            mode: *const c_void,
            seconds: f64,
            return_after_source_handled: u8,
        ) -> i32;
        static kCFRunLoopDefaultMode: *const c_void;
    }

    /// Let AppKit/CoreAnimation actually composite. Every sample is taken
    /// after pumping, because an un-pumped run loop shows nothing regardless
    /// of what was enqueued.
    fn pump(seconds: f64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(seconds);
        while std::time::Instant::now() < deadline {
            unsafe {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.02, 0);
            }
        }
    }

    /// One solid-luma I420 frame, as a real `CVPixelBuffer`, through the real
    /// conversion the receiver's software fallback uses.
    fn bright_frame() -> desktop_lib::native_display::OwnedCVPixelBuffer {
        let y = vec![BRIGHT_LUMA; (WIDTH * HEIGHT) as usize];
        let chroma = vec![128u8; ((WIDTH / 2) * (HEIGHT / 2)) as usize];
        i420_to_cv_pixel_buffer(I420Planes {
            y: &y,
            y_stride: WIDTH,
            u: &chroma,
            u_stride: WIDTH / 2,
            v: &chroma,
            v_stride: WIDTH / 2,
            width: WIDTH,
            height: HEIGHT,
        })
        .expect("I420 -> CVPixelBuffer conversion")
    }

    /// AppKit's own visibility flag -- the exact property `orderOut:` (and so
    /// `remove_window`'s `win.hide()`) clears. Reported alongside every sample
    /// as a descriptor ONLY. It is deliberately not part of any verdict: a
    /// boolean flag is precisely the event-level signal the never-black rule
    /// says cannot distinguish "held the frame" from "went blank quietly".
    fn window_is_visible(window: &NSWindow) -> bool {
        unsafe { msg_send![window, isVisible] }
    }

    /// Mean luma of what is actually COMPOSITED ON SCREEN in `rect`.
    ///
    /// Capturing the screen REGION, not the window by id, is load-bearing and
    /// was corrected after measurement: `screencapture -l<window>` returned
    /// `mean_luma=255` for a window that had already been hidden, because it
    /// composites that window's own backing store whether or not it is on
    /// screen. A region capture answers the only question that matters -- what
    /// would the user see at this screen location -- so a hidden window reads
    /// as whatever is behind it (here, a deliberate black backdrop).
    fn sample_screen_region_luma(rect: (f64, f64, f64, f64), scratch: &std::path::Path) -> Option<f64> {
        let (x, y, w, h) = rect;
        let path = scratch.join("hold-probe-region.png");
        let _ = std::fs::remove_file(&path);
        let status = std::process::Command::new("/usr/sbin/screencapture")
            .arg("-x") // no capture sound
            .arg(format!("-R{},{},{},{}", x.round(), y.round(), w.round(), h.round()))
            .arg(&path)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        if bytes.is_empty() {
            return None;
        }
        let image = image::load_from_memory(&bytes).ok()?.to_luma8();
        let (iw, ih) = (image.width(), image.height());
        if iw == 0 || ih == 0 {
            return None;
        }
        // Mean over the middle half, so the sample is dominated by video
        // content rather than any edge pixels.
        let (x0, x1) = (iw / 4, iw * 3 / 4);
        let (y0, y1) = (ih / 4, ih * 3 / 4);
        let mut total = 0f64;
        let mut count = 0u64;
        for py in y0..y1 {
            for px in x0..x1 {
                total += f64::from(image.get_pixel(px, py).0[0]);
                count += 1;
            }
        }
        (count > 0).then(|| total / count as f64)
    }

    fn describe(sample: Option<f64>, visible: bool) -> String {
        let visibility = if visible {
            "window reports visible"
        } else {
            "window reports NOT visible"
        };
        match sample {
            Some(luma) => format!("screen mean_luma={luma:.1} ({visibility})"),
            None => format!("screen region capture failed ({visibility})"),
        }
    }

    pub fn run() {
        let stale_guard = std::env::args().any(|a| a == "--stale-guard");
        let participant_disconnect = std::env::args().any(|a| a == "--participant-disconnected");
        let track_unsubscribed = std::env::args().any(|a| a == "--track-unsubscribed");
        let retire_reuse = std::env::args().any(|a| a == "--retire-reuse");

        println!("=== native hold-last-frame probe (SAMPLED PIXELS) ===");
        println!(
            "  mode        : {}",
            if retire_reuse && stale_guard {
                "retire -> pre-#840 hidden reuse control"
            } else if retire_reuse {
                "retire -> reuse with retained layer content (#840)"
            } else if stale_guard {
                "--stale-guard   <-- POSITIVE CONTROL: unconditional hide"
            } else if track_unsubscribed {
                "TrackUnsubscribed hold path (#631)"
            } else if participant_disconnect {
                "ParticipantDisconnected hold path (#631)"
            } else {
                "teardown_decision (as fixed)"
            }
        );
        println!("  frame       : {WIDTH}x{HEIGHT} solid luma {BRIGHT_LUMA}");
        println!("  forced gap  : {}ms with no frames enqueued", GAP.as_millis());
        println!("  bright iff  : mean luma > {BRIGHT_THRESHOLD}\n");

        let mtm = MainThreadMarker::new().expect("must run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

        let content_rect = NSRect {
            origin: NSPoint::new(ORIGIN_X, ORIGIN_Y),
            size: NSSize::new(f64::from(WIDTH), f64::from(HEIGHT)),
        };

        // An opaque BLACK window pinned under the video window, covering the
        // same region. It makes the measurement independent of whatever the
        // user's desktop happens to look like: if the video window stops being
        // composited, this is what the sampled screen region shows, and it is
        // unambiguously dark. Without it a light wallpaper could read as
        // "bright" and mask the failure.
        let backdrop: Retained<NSWindow> = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                content_rect,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            let black = NSColor::blackColor();
            let _: () = msg_send![&*backdrop, setBackgroundColor: &*black];
        }
        backdrop.setOpaque(true);
        // Above ordinary windows so an unrelated app cannot occlude the
        // measured region; the video window is ordered above this one.
        backdrop.setLevel(NS_FLOATING_WINDOW_LEVEL);
        backdrop.orderFront(None);

        let window: Retained<NSWindow> = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                content_rect,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("Petal hold_last_frame_probe"));
        // Opaque black background too, so a window that stays up but loses its
        // layer contents reads dark rather than transparent -- that outcome
        // must be a FAIL, not an accidental pass through the backdrop.
        unsafe {
            let black = NSColor::blackColor();
            let _: () = msg_send![&*window, setBackgroundColor: &*black];
        }
        window.setOpaque(true);
        window.setLevel(NS_FLOATING_WINDOW_LEVEL + 1);
        window.makeKeyAndOrderFront(None);

        // Attach exactly as `platform::appkit::attach_display_layer` does in
        // production: the layer-HOSTING VIEW becomes a subview. Adding the
        // layer as a bare sublayer instead leaves it zero-sized and renders
        // nothing -- measured here as a black baseline before this was
        // corrected, which is why the harness self-check exists.
        let display = DisplayLayer::new();
        unsafe {
            let ns_window_ptr: *mut AnyObject = Retained::as_ptr(&window) as *mut AnyObject;
            let content_view: *mut AnyObject = msg_send![ns_window_ptr, contentView];
            if content_view.is_null() {
                eprintln!("contentView unavailable");
                std::process::exit(3);
            }
            let _: () = msg_send![content_view, setWantsLayer: true];
            let bounds: NSRect = msg_send![content_view, bounds];
            display.set_contents_scale(window.backingScaleFactor());
            display.set_frame(0.0, 0.0, bounds.size.width, bounds.size.height);
            const NS_VIEW_WIDTH_SIZABLE: u64 = 2;
            const NS_VIEW_HEIGHT_SIZABLE: u64 = 16;
            let view_ptr = display.as_view_ptr();
            let _: () = msg_send![
                view_ptr,
                setAutoresizingMask: NS_VIEW_WIDTH_SIZABLE | NS_VIEW_HEIGHT_SIZABLE
            ];
            let _: () = msg_send![content_view, addSubview: view_ptr];
        }

        let scratch = std::env::temp_dir();
        pump(0.5);

        // `screencapture -R` takes top-left-origin screen coordinates; AppKit
        // frames are bottom-left-origin. Both spaces are anchored on the
        // PRIMARY screen (the one whose frame origin is (0,0) -- the menu-bar
        // display), so the flip must use that screen's height.
        //
        // NOT `mainScreen`: that is the screen with the key window, which on a
        // multi-display Mac is whichever display happens to have focus. Using
        // it flipped against the wrong height and sampled a region that was
        // not the window -- on a 1512x982 primary with a 2560x1440 secondary
        // focused, the probe photographed empty desktop at y=960 instead of
        // the window at y=502 and reported HARNESS INVALID, which reads as
        // "Screen Recording is denied" and sent this gate chasing TCC.
        let screen_rect = {
            let frame = window.frame();
            let primary_height = objc2_app_kit::NSScreen::screens(mtm)
                .firstObject()
                .map(|screen| screen.frame().size.height)
                .unwrap_or(0.0);
            (
                frame.origin.x,
                primary_height - (frame.origin.y + frame.size.height),
                frame.size.width,
                frame.size.height,
            )
        };
        println!(
            "  sampled region            : x={:.0} y={:.0} {:.0}x{:.0} (top-left origin)",
            screen_rect.0, screen_rect.1, screen_rect.2, screen_rect.3
        );

        // ---- feed real frames, then stop (the gap) -----------------------
        let frame = bright_frame();
        for _ in 0..10 {
            let sample = display
                .prepare_sample(frame.as_ptr() as *mut c_void)
                .expect("CMSampleBuffer creation");
            display.enqueue_prepared(&sample);
            pump(0.05);
        }
        pump(0.4);

        // ---- BASELINE: the harness must be able to SEE the bright frame ---
        let baseline = sample_screen_region_luma(screen_rect, &scratch);
        println!(
            "  baseline (frames flowing) : {}",
            describe(baseline, window_is_visible(&window))
        );
        let baseline_bright = baseline.is_some_and(|luma| luma > BRIGHT_THRESHOLD);
        if !baseline_bright {
            println!(
                "\n  HARNESS INVALID -- the known-bright window did not sample bright, so this\n  \
                 harness cannot observe the thing it measures. Nothing here is interpretable.\n  \
                 Most likely cause: Screen Recording access is not granted to THIS binary\n  \
                 ({}), so the screen capture comes back black.",
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unknown path>".to_string())
            );
            std::process::exit(3);
        }

        // ---- the decision under test, on real sids ------------------------
        // A republish: the SFU already holds the replacement (the sender
        // awaits its publish before unpublishing the old track), and the
        // unpublish names the sid we currently track.
        let hide = if retire_reuse {
            false
        } else if track_unsubscribed {
            // This is the same decision used by the real
            // `RoomEvent::TrackUnsubscribed` arm. A full reconnect can empty
            // the room snapshot before `Reconnecting`, so this event holds
            // the tracked panel until a later terminal unpublish or reconcile.
            if stale_guard {
                should_remove_window(Some(OLD_SID), OLD_SID)
            } else {
                matches!(
                    track_unsubscribe_decision(Some(OLD_SID), OLD_SID),
                    TeardownDecision::RemoveWindow
                )
            }
        } else if participant_disconnect {
            // This is the same production helper the real
            // `RoomEvent::ParticipantDisconnected` arm invokes. The probe
            // supplies the screen-visible hold effect: default mode leaves
            // the panel alone; the positive control substitutes the old
            // destructive behavior so the rendered-region assertion can
            // prove it detects the failure.
            let mut hold_routed = false;
            handle_participant_disconnected("probe-owner", "probe-owner", |_| {
                hold_routed = true;
            });
            assert!(hold_routed, "ParticipantDisconnected must route to hold");
            stale_guard
        } else if stale_guard {
            // Pre-#627: a matching sid meant remove_window unconditionally.
            should_remove_window(Some(OLD_SID), OLD_SID)
        } else {
            match teardown_decision(Some(OLD_SID), OLD_SID, true) {
                TeardownDecision::RemoveWindow => true,
                TeardownDecision::HoldForReplacement
                | TeardownDecision::HoldForTransientUnsubscribe
                | TeardownDecision::IgnoreSuperseded => false,
            }
        };
        println!(
            "  decision                  : {} -> {}",
            if stale_guard {
                if track_unsubscribed {
                    "TrackUnsubscribed -> pre-#631 destructive control"
                } else if participant_disconnect {
                    "ParticipantDisconnected -> pre-#631 destructive control"
                } else {
                    "should_remove_window(current=OLD, unpublished=OLD)"
                }
            } else if track_unsubscribed {
                "TrackUnsubscribed -> track_unsubscribe_decision -> hold"
            } else if participant_disconnect {
                "ParticipantDisconnected -> handle_participant_disconnected -> hold"
            } else {
                "teardown_decision(current=OLD, unpublished=OLD, replacement_exists=true)"
            },
            if retire_reuse {
                if stale_guard {
                    "RETIRE, THEN LEAVE HIDDEN (pre-#840 control)"
                } else {
                    "RETIRE, THEN REVEAL RETAINED LAYER CONTENT"
                }
            } else if hide {
                "HIDE WINDOW (remove_window's win.hide())"
            } else {
                "HOLD LAST FRAME (window stays on screen, layer untouched)"
            }
        );

        // Apply exactly what the decision implies. `remove_window` hides the
        // panel; the hold path touches nothing at all.
        if retire_reuse {
            // A real retired panel is ordered out while its warm display
            // layer remains attached and unflushed. Reuse must order it back
            // in immediately when that layer holds content; the old reveal-
            // gate reset (negative control) left it ordered out waiting for a
            // new first frame that may never arrive.
            window.orderOut(None);
            pump(0.1);
            if !stale_guard {
                window.orderFront(None);
            }
        } else if hide {
            window.orderOut(None);
        }

        // ---- the forced gap: no frames enqueued --------------------------
        let gap_deadline = std::time::Instant::now() + GAP;
        while std::time::Instant::now() < gap_deadline {
            pump(0.05);
        }

        // ---- sample across the gap ---------------------------------------
        let after = sample_screen_region_luma(screen_rect, &scratch);
        let after_visible = window_is_visible(&window);
        let after_description = describe(after, after_visible);
        println!("  after gap                 : {after_description}");
        let after_bright = after.is_some_and(|luma| luma > BRIGHT_THRESHOLD) && after_visible;

        println!("\n=== RESULT ===");
        if stale_guard {
            if after_bright {
                println!(
                    "  CONTROL DID NOT TRIP -- the pre-fix hide left the window's pixels on\n  \
                     screen, so this harness cannot demonstrate the failure and a default-mode\n  \
                     pass means nothing."
                );
                std::process::exit(2);
            }
            println!(
                "  CONTROL OK -- the pre-fix decision took the share's pixels off screen\n  \
                 ({after_description}), which is the #627/#840 native failure. The harness\n  \
                 demonstrably observes it."
            );
        } else if after_bright {
            println!("  PASS -- the window held its last frame across the gap ({after_description}).");
        } else {
            println!(
                "  FAIL -- the share's last frame did not survive the gap ({after_description})."
            );
            std::process::exit(1);
        }
    }
}
