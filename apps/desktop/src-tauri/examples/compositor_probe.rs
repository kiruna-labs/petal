//! Standalone verification harness for the receiver-side compositor's
//! zero-copy decode-to-display path (SPEC.md §4.4, `compositor.rs` +
//! `native_display.rs`).
//!
//! ## Why this exists instead of driving the real Tauri app
//!
//! This session (like the prior M0/rooms phases, per CLAUDE.md's own
//! documented limitation) cannot interactively drive a full
//! `tauri::Builder::default().run()` GUI app from this non-interactive
//! shell. So, following the task brief's own suggested fallback ("a
//! windowed test harness ... similar to how M0's original examples proved
//! the pipeline stage by stage"), this example proves the actual hard part
//! -- real decoded `CVPixelBuffer`s reaching a real `AVSampleBufferDisplayLayer`
//! with no CPU copy -- OUTSIDE of Tauri/tauri_nspanel entirely: it opens one
//! plain, real, borderless `NSWindow` directly via `objc2-app-kit` (no
//! Tauri window management involved at all), attaches the SAME
//! `native_display::DisplayLayer` the real compositor uses, and runs a real
//! `NSApplication` event loop so the window actually appears on screen and
//! actually paints.
//!
//! ## What this does NOT prove
//!
//! This does not exercise `compositor.rs`'s Tauri-panel-based window
//! creation, cascade placement, or the header/pointer child-webview wiring
//! -- those are real code (see `compositor.rs`), but only exercised via
//! `cargo check`/`cargo build`'s type-checking + this session's own code
//! review, not a live screenshot, for the reasons above. This example's
//! scope is deliberately just: "does a real subscribed H.264 window-share
//! track's decoded output reach an on-screen native layer with zero CPU
//! copies" -- the task brief's own stated priority #1, "the actual core
//! 'real window' magic."
//!
//! ## Usage
//!
//! Run a real publisher in one process (reuses `publish_probe`, unmodified):
//! ```text
//! cargo run --example publish_probe -- <window_id> petal-compositor-probe
//! ```
//! Then, in a second process, run this:
//! ```text
//! cargo run --example compositor_probe -- petal-compositor-probe
//! ```
//! A real borderless window should appear showing the shared window's live
//! content. Frame count / real dimensions / buffer-type are logged to
//! stdout every second.

#[derive(Clone, Debug, PartialEq)]
struct ProbeConfig {
    room_name: String,
    window_x: f64,
    window_y: f64,
    window_width: f64,
    window_height: f64,
    enqueue_delay_ms: u64,
    nonactivating: bool,
    /// Auto-exit after this many seconds (0 = run until killed).
    seconds: u64,
    /// #886 regression gate: with `--seconds` set, sample the GLOBAL kernel
    /// IOSurface instance count (`ioclasscount IOSurface`) after a 20s
    /// warmup and again at the end; exit 1 if it grew by more than this.
    /// The un-fixed layer accumulated one surface per displayed frame
    /// (+29.8/s measured) -- ambient machine noise is tens, so a gate of
    /// ~120 cleanly separates "flat" from "leaking" over a 60s+ run.
    iosurface_gate: Option<i64>,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            room_name: "petal-compositor-probe".to_string(),
            window_x: 200.0,
            window_y: 200.0,
            window_width: 640.0,
            window_height: 400.0,
            enqueue_delay_ms: 0,
            nonactivating: false,
            seconds: 0,
            iosurface_gate: None,
        }
    }
}

fn parse_probe_args<I>(args: I) -> Result<ProbeConfig, String>
where
    I: IntoIterator<Item = String>,
{
    let mut config = ProbeConfig::default();
    let mut args = args.into_iter();
    let _program = args.next();
    let mut room_seen = false;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--window-x" => {
                config.window_x = args
                    .next()
                    .ok_or("--window-x requires a value")?
                    .parse()
                    .map_err(|_| "invalid --window-x")?;
            }
            "--window-y" => {
                config.window_y = args
                    .next()
                    .ok_or("--window-y requires a value")?
                    .parse()
                    .map_err(|_| "invalid --window-y")?;
            }
            "--window-width" => {
                config.window_width = args
                    .next()
                    .ok_or("--window-width requires a value")?
                    .parse()
                    .map_err(|_| "invalid --window-width")?;
            }
            "--window-height" => {
                config.window_height = args
                    .next()
                    .ok_or("--window-height requires a value")?
                    .parse()
                    .map_err(|_| "invalid --window-height")?;
            }
            "--enqueue-delay-ms" => {
                config.enqueue_delay_ms = args
                    .next()
                    .ok_or("--enqueue-delay-ms requires a value")?
                    .parse()
                    .map_err(|_| "invalid --enqueue-delay-ms")?;
            }
            "--nonactivating" => config.nonactivating = true,
            "--seconds" => {
                config.seconds = args
                    .next()
                    .ok_or("--seconds requires a value")?
                    .parse::<u64>()
                    .map_err(|e| format!("--seconds: {e}"))?;
            }
            "--iosurface-gate" => {
                config.iosurface_gate = Some(
                    args.next()
                        .ok_or("--iosurface-gate requires a value")?
                        .parse::<i64>()
                        .map_err(|e| format!("--iosurface-gate: {e}"))?,
                );
            }
            value if value.starts_with('-') => return Err(format!("unknown argument: {value}")),
            value if room_seen => return Err(format!("unexpected positional argument: {value}")),
            value => {
                config.room_name = value.to_string();
                room_seen = true;
            }
        }
    }
    if config.window_width <= 0.0 || config.window_height <= 0.0 {
        return Err("window dimensions must be positive".to_string());
    }
    Ok(config)
}

/// Run `work` on the main thread, the way `compositor::push_frame` does with
/// `AppHandle::run_on_main_thread`.
///
/// `AVSampleBufferDisplayLayer` is a `CALayer`, and enqueuing or resizing off
/// the main thread leaves *nothing displayed* while every call still appears to
/// succeed -- the original black-compositor-window bug (see
/// `native_display::enqueue_prepared`'s doc comment, and CLAUDE.md's AppKit
/// crash class). This harness has no Tauri `AppHandle`, so it dispatches to the
/// main queue directly; `NSApplication::run` below drives that queue.
#[cfg(target_os = "macos")]
fn run_on_main<F: FnOnce() + Send + 'static>(work: F) {
    use std::ffi::c_void;

    extern "C" {
        /// `dispatch_get_main_queue()` is a macro over this symbol.
        static _dispatch_main_q: c_void;
        fn dispatch_async_f(
            queue: *const c_void,
            context: *mut c_void,
            work: extern "C" fn(*mut c_void),
        );
    }

    extern "C" fn trampoline(context: *mut c_void) {
        // SAFETY: `context` is exactly the box leaked below, reconstituted once
        // and then dropped -- libdispatch invokes this function one time per
        // `dispatch_async_f` call.
        let work: Box<Box<dyn FnOnce() + Send>> = unsafe { Box::from_raw(context.cast()) };
        work();
    }

    let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(work));
    // SAFETY: the leaked pointer is owned by libdispatch until `trampoline`
    // takes it back; `_dispatch_main_q` is the process-wide main queue.
    unsafe {
        dispatch_async_f(
            std::ptr::addr_of!(_dispatch_main_q),
            Box::into_raw(boxed).cast(),
            trampoline,
        );
    }
}

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    env_logger::init();
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));

    let config = parse_probe_args(std::env::args()).unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2);
    });
    let room_name = config.room_name.clone();

    let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let token = desktop_lib::transport::mint_access_token(
        "petal-compositor-probe-subscriber",
        &room_name,
        false,
        true,
    )
    .unwrap_or_else(|e| {
        eprintln!("Failed to mint access token: {e}");
        std::process::exit(1);
    });

    println!("compositor_probe: joining room '{room_name}' as subscriber...");

    let mtm = MainThreadMarker::new().expect("must run on the main thread");

    // Real, plain, borderless NSWindow -- NOT going through Tauri/
    // tauri_nspanel at all, so this is unaffected by whatever mechanism
    // makes a full `tauri::Builder::run()` hang in this shell.
    let content_rect = NSRect {
        origin: NSPoint::new(config.window_x, config.window_y),
        size: NSSize::new(config.window_width, config.window_height),
    };
    let style_mask = if config.nonactivating {
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel
    } else {
        NSWindowStyleMask::Borderless
    };
    let window: Retained<NSWindow> = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content_rect,
            style_mask,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("Petal compositor_probe"));
    window.setOpaque(true);
    // #613's observer attests the ScreenCaptureKit frame against this planned
    // physical crop. A diagnostic shadow expands/insets that frame, so disable
    // it instead of weakening the crop-containment contract.
    window.setHasShadow(false);
    if config.nonactivating {
        unsafe {
            use objc2::msg_send;
            let _: () = msg_send![&*window, orderFrontRegardless];
        }
    } else {
        window.makeKeyAndOrderFront(None);
    }
    let scale = window.backingScaleFactor();
    // The coordinator intentionally places this window on a selected display;
    // use the window's actual screen rather than mainScreen so crop coordinates
    // remain correct for secondary/negative-origin displays.
    let screen_frame = window
        .screen()
        .map(|screen| screen.frame())
        .unwrap_or(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)));
    let frame = window.frame();
    let crop_x = ((frame.origin.x - screen_frame.origin.x) * scale).round() as i64;
    let crop_y = ((screen_frame.origin.y + screen_frame.size.height
        - (frame.origin.y + frame.size.height))
        * scale)
        .round() as i64;
    println!(
        "DESTINATION_CROP_PX {crop_x} {crop_y} {} {}",
        (frame.size.width * scale).round() as i64,
        (frame.size.height * scale).round() as i64
    );
    println!("WINDOW_ID {}", window.windowNumber());
    println!(
        "COMPOSITOR_WINDOW_READY nonactivating={} shadow=false key={} main={} enqueue_delay_ms={}",
        config.nonactivating,
        window.isKeyWindow(),
        window.isMainWindow(),
        config.enqueue_delay_ms
    );

    // Real zero-copy display path -- the exact same `DisplayLayer` type
    // `compositor.rs` uses for the real app, attached the same way
    // `platform::appkit::attach_display_layer` does: the layer-HOSTING VIEW
    // becomes a subview.
    //
    // This previously added `as_layer_ptr()` as a bare SUBLAYER instead. That
    // compiles, decodes, and reports healthy frame counts while displaying
    // NOTHING: `DisplayLayer::new` makes the layer the view's hosted backing
    // store, so `set_frame` sizes it through the VIEW -- and a view outside the
    // hierarchy never lays out, leaving the layer zero-sized. Measured at
    // uniform `mean_luma=30` (window background) with 167 frames decoded (#594).
    let display = Arc::new(desktop_lib::native_display::DisplayLayer::new());
    unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        let ns_window_ptr: *mut AnyObject = Retained::as_ptr(&window) as *mut AnyObject;
        let content_view: *mut AnyObject = msg_send![ns_window_ptr, contentView];
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

    let frame_count = Arc::new(AtomicU64::new(0));
    let display_enqueue_count = Arc::new(AtomicU64::new(0));
    let native_buffer_count = Arc::new(AtomicU64::new(0));
    let non_native_count = Arc::new(AtomicU64::new(0));
    let last_size = Arc::new(Mutex::new((0u32, 0u32)));
    let last_buffer_type = Arc::new(Mutex::new(String::new()));

    let display_for_room = display.clone();
    let enqueue_count_cb = display_enqueue_count.clone();
    let enqueue_delay_ms = config.enqueue_delay_ms;
    let frame_count_cb = frame_count.clone();
    let native_count_cb = native_buffer_count.clone();
    let non_native_cb = non_native_count.clone();
    let last_size_cb = last_size.clone();
    let last_type_cb = last_buffer_type.clone();

    let window_width = Arc::new(AtomicU32::new(640));
    let window_height = Arc::new(AtomicU32::new(400));
    let width_cb = window_width.clone();
    let height_cb = window_height.clone();

    // Run the LiveKit connect + subscribe loop on a background tokio runtime
    // (NSApplication owns the main thread's run loop below), feeding
    // decoded frames into `display` via `enqueue_frame`/`push_frame`-style
    // logic identical to `subscriber::start_compositor_feed`'s real
    // per-frame handling, just without the Tauri compositor's window
    // registry (this harness has exactly one window, not N).
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async move {
            use futures::StreamExt;
            use livekit::prelude::*;
            use livekit::webrtc::video_frame::VideoBufferType;
            use livekit::webrtc::video_stream::native::NativeVideoStream;

            let mut room_options = RoomOptions::default();
            room_options.auto_subscribe = true;
            let (room, mut events) = match Room::connect(&url, &token, room_options).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("compositor_probe: room connect failed: {e}");
                    std::process::exit(1);
                }
            };
            println!("compositor_probe: connected, sid={}", room.sid().await);

            while let Some(event) = events.recv().await {
                if let RoomEvent::TrackSubscribed { track, .. } = event {
                    let RemoteTrack::Video(video_track) = track else {
                        continue;
                    };
                    println!(
                        "compositor_probe: subscribed to real video track '{}'",
                        video_track.name()
                    );
                    let display = display_for_room.clone();
                    let enqueue_count = enqueue_count_cb.clone();
                    let frame_count = frame_count_cb.clone();
                    let native_count = native_count_cb.clone();
                    let non_native = non_native_cb.clone();
                    let last_size = last_size_cb.clone();
                    let last_type = last_type_cb.clone();
                    let width = width_cb.clone();
                    let height = height_cb.clone();

                    tokio::spawn(async move {
                        let rtc_track = video_track.rtc_track();
                        let mut stream = NativeVideoStream::new(rtc_track);
                        let mut resized = false;
                        while let Some(frame) = stream.next().await {
                            frame_count.fetch_add(1, Ordering::Relaxed);
                            let buffer_type = frame.buffer.buffer_type();
                            *last_type.lock().unwrap() = format!("{buffer_type:?}");
                            *last_size.lock().unwrap() =
                                (frame.buffer.width(), frame.buffer.height());

                            if buffer_type == VideoBufferType::Native {
                                native_count.fetch_add(1, Ordering::Relaxed);
                                if let Some(native) = frame.buffer.as_native() {
                                    let cv_pixel_buffer = native.get_cv_pixel_buffer();
                                    if !cv_pixel_buffer.is_null() {
                                        if !resized {
                                            // Recorded for the status line only.
                                            // The hosted view fills the window
                                            // and the layer's videoGravity is
                                            // ResizeAspect, so it already
                                            // letterboxes the source to fit --
                                            // resizing the view to the SOURCE's
                                            // pixel size instead would push the
                                            // content outside the 640x400 window.
                                            width.store(frame.buffer.width(), Ordering::Relaxed);
                                            height.store(frame.buffer.height(), Ordering::Relaxed);
                                            resized = true;
                                        }
                                        // Two steps, and the split is the point:
                                        // `prepare_sample` wraps the pixel
                                        // buffer with NO copy and retains it, so
                                        // it runs here on the decode thread
                                        // while the frame is still alive;
                                        // `enqueue_prepared` must then run on
                                        // the main thread.
                                        // #886 discriminator 2: stop right
                                        // after the get_cv_pixel_buffer
                                        // bridge call -- no CMSampleBuffer,
                                        // no layer. Isolates the ObjC
                                        // bridge getter itself.
                                        if std::env::var("PETAL_PROBE_GETBUF_ONLY").is_ok() {
                                            continue;
                                        }
                                        if let Some(sample) =
                                            display.prepare_sample(cv_pixel_buffer.cast())
                                        {
                                            let display = display.clone();
                                            let enqueue_count = enqueue_count.clone();
                                            // #886 discriminator: build the
                                            // CMSampleBuffer wrapper exactly
                                            // like production but NEVER hand
                                            // it to the layer -- separates
                                            // "wrapper machinery retains" from
                                            // "the layer retains".
                                            if std::env::var("PETAL_PROBE_PREPARE_ONLY").is_ok() {
                                                drop(sample);
                                            } else if enqueue_delay_ms == 0 {
                                                run_on_main(move || {
                                                    display.enqueue_prepared(&sample);
                                                    enqueue_count.fetch_add(1, Ordering::Relaxed);
                                                });
                                            } else {
                                                tokio::spawn(async move {
                                                    tokio::time::sleep(
                                                        std::time::Duration::from_millis(
                                                            enqueue_delay_ms,
                                                        ),
                                                    )
                                                    .await;
                                                    run_on_main(move || {
                                                        display.enqueue_prepared(&sample);
                                                        enqueue_count
                                                            .fetch_add(1, Ordering::Relaxed);
                                                    });
                                                });
                                            }
                                        }
                                    }
                                }
                            } else {
                                non_native.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        println!("compositor_probe: video stream ended");
                    });
                }
            }
        });
    });

    // NOTE: this harness deliberately does NOT resize the outer NSWindow to
    // match the source's real resolution -- `NSWindow`/`Retained<NSWindow>`
    // is not `Send` (AppKit objects are main-thread-only, confirmed
    // directly by the compiler when this was first attempted from a
    // background thread), so doing so safely would need a main-thread
    // dispatch (`compositor.rs`'s real Tauri-window path handles this via
    // Tauri's own main-thread-marshaled `set_size`, which this bare-AppKit
    // harness doesn't have). The display LAYER's frame (see
    // `display_resize.set_frame` above, called from the frame-receiving
    // task) is resized to the real source dimensions regardless, so the
    // video content itself displays at (and reveals) its true aspect ratio
    // within the fixed 640x400 outer window -- sufficient to visually
    // confirm real content is rendering, just not a pixel-perfect window
    // bounds match. `compositor.rs`'s real `resize_to_source` (used by the
    // actual app) does the full window resize correctly via Tauri's
    // main-thread-safe API.

    // Periodic stdout status line -- this is the honest, human-checkable
    // evidence this harness produces: real frame counts, real dimensions,
    // real buffer_type, updated every second while the window is on screen.
    let frame_count_poll = frame_count.clone();
    let enqueue_count_poll = display_enqueue_count.clone();
    let native_poll = native_buffer_count.clone();
    let non_native_poll = non_native_count.clone();
    let last_size_poll = last_size.clone();
    let last_type_poll = last_buffer_type.clone();
    std::thread::spawn(move || {
        let mut last = 0u64;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let count = frame_count_poll.load(Ordering::Relaxed);
            if count != last {
                let (w, h) = *last_size_poll.lock().unwrap();
                let buffer_type = last_type_poll.lock().unwrap().clone();
                println!(
                    "compositor_probe: frames={count} display_enqueued={} native={} non_native={} size={w}x{h} buffer_type={buffer_type}",
                    enqueue_count_poll.load(Ordering::Relaxed),
                    native_poll.load(Ordering::Relaxed),
                    non_native_poll.load(Ordering::Relaxed),
                );
                last = count;
            }
        }
    });

    // #886 regression watchdog: bounded-duration run with a global
    // IOSurface-count gate. The un-fixed AVSampleBufferDisplayLayer retained
    // one surface per displayed frame (+29.8/s measured live); with the
    // bounded flush cadence the count must stay flat (+- ambient noise +
    // the <=LAYER_FLUSH_EVERY_N_ENQUEUES standing pool). Runs from a thread
    // and exits the whole process -- NSApplication::run never returns.
    if config.seconds > 0 {
        let seconds = config.seconds;
        let gate = config.iosurface_gate;
        let frames_at_end = frame_count.clone();
        std::thread::spawn(move || {
            let count_iosurfaces = || -> Option<i64> {
                let out = std::process::Command::new("/usr/sbin/ioclasscount")
                    .arg("IOSurface")
                    .output()
                    .ok()?;
                String::from_utf8_lossy(&out.stdout)
                    .split('=')
                    .nth(1)?
                    .trim()
                    .parse()
                    .ok()
            };
            // Warmup covers connect + first frames + the decoder pool
            // filling to its steady size.
            std::thread::sleep(std::time::Duration::from_secs(20.min(seconds)));
            let start = count_iosurfaces();
            std::thread::sleep(std::time::Duration::from_secs(
                seconds.saturating_sub(20.min(seconds)),
            ));
            let end = count_iosurfaces();
            let frames = frames_at_end.load(Ordering::Relaxed);
            match (start, end) {
                (Some(start), Some(end)) => {
                    let grown = end - start;
                    println!(
                        "IOSURFACE_GATE frames={frames} start={start} end={end} grown={grown}"
                    );
                    if let Some(gate) = gate {
                        if grown > gate {
                            println!(
                                "IOSURFACE_GATE FAIL: grew {grown} > gate {gate} -- the #886 \
                                 layer-retention class is back"
                            );
                            std::process::exit(1);
                        }
                        println!("IOSURFACE_GATE PASS (gate {gate})");
                    }
                    std::process::exit(0);
                }
                _ => {
                    println!("IOSURFACE_GATE ERROR: ioclasscount unreadable");
                    // A gate that cannot measure must fail closed.
                    std::process::exit(if gate.is_some() { 1 } else { 0 });
                }
            }
        });
    }

    println!(
        "compositor_probe: opening window and running NSApplication event loop (Ctrl-C to stop)..."
    );
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(if config.nonactivating {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    });
    if !config.nonactivating {
        app.activateIgnoringOtherApps(true);
    }
    app.run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("compositor_probe is macOS-only.");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        std::iter::once("compositor_probe".to_string())
            .chain(values.iter().map(|value| (*value).to_string()))
            .collect()
    }

    #[test]
    fn presentation_flags_are_bounded_and_explicit() {
        let parsed = parse_probe_args(args(&[
            "room-a",
            "--window-x",
            "80",
            "--window-y",
            "120",
            "--window-width",
            "480",
            "--window-height",
            "300",
            "--enqueue-delay-ms",
            "200",
            "--nonactivating",
        ]))
        .unwrap();
        assert_eq!(parsed.room_name, "room-a");
        assert_eq!(parsed.enqueue_delay_ms, 200);
        assert!(parsed.nonactivating);
        assert_eq!((parsed.window_x, parsed.window_y), (80.0, 120.0));
        assert_eq!((parsed.window_width, parsed.window_height), (480.0, 300.0));
    }

    #[test]
    fn invalid_dimensions_and_unknown_flags_fail_closed() {
        assert!(parse_probe_args(args(&["--window-width", "0"])).is_err());
        assert!(parse_probe_args(args(&["--unknown"])).is_err());
    }
}
