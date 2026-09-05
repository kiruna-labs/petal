//! Minimal diagnostic: does a plain AppKit window (zero networking, zero
//! Tokio, zero LiveKit) work in this environment at all? Isolates whether
//! the session-wide GUI hang seen elsewhere is a pure WindowServer/display
//! issue or specific to combining AppKit with networking/threading.
//!
//! Logs a heartbeat every second so a stuck run is distinguishable from a
//! silently-still-fine one.

#[cfg(target_os = "macos")]
fn main() {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use std::io::Write;

    eprintln!("bare_window_probe: start, pid={}", std::process::id());
    std::io::stderr().flush().ok();

    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    eprintln!("bare_window_probe: got MainThreadMarker");

    let app = NSApplication::sharedApplication(mtm);
    eprintln!("bare_window_probe: got NSApplication::sharedApplication");
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    eprintln!("bare_window_probe: set activation policy");

    let content_rect = NSRect {
        origin: NSPoint::new(200.0, 200.0),
        size: NSSize::new(400.0, 300.0),
    };
    eprintln!("bare_window_probe: about to alloc/init NSWindow...");
    std::io::stderr().flush().ok();

    let window: Retained<NSWindow> = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content_rect,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    eprintln!("bare_window_probe: NSWindow created");

    window.setTitle(&NSString::from_str("bare_window_probe"));
    eprintln!("bare_window_probe: title set, about to makeKeyAndOrderFront...");
    std::io::stderr().flush().ok();

    window.makeKeyAndOrderFront(None);
    eprintln!("bare_window_probe: makeKeyAndOrderFront returned");

    // Heartbeat thread so a hung run loop is visible from outside.
    std::thread::spawn(|| {
        let mut n = 0u32;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            n += 1;
            eprintln!("bare_window_probe: heartbeat {n}");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            if n >= 20 {
                eprintln!("bare_window_probe: heartbeat limit reached, exiting");
                std::process::exit(0);
            }
        }
    });

    eprintln!("bare_window_probe: about to enter app.run()...");
    std::io::stderr().flush().ok();
    app.run();
    eprintln!("bare_window_probe: app.run() returned (window closed)");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("bare_window_probe is macOS-only");
}
