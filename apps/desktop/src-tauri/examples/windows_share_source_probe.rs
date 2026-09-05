//! Windows smoke probe: unified display+window enumeration and WGC one-shot
//! thumbnails, without a backend or a room.
//!
//! Prints displays first (titled "Screen N"), then windows, then captures a
//! thumbnail for the first few entries to prove the WGC one-shot path works
//! on this host. Exits non-zero when NO thumbnail could be captured.

#[cfg(target_os = "windows")]
fn main() {
    let windows = match desktop_lib::window_source::list() {
        Ok(windows) => windows,
        Err(error) => {
            eprintln!("windows_share_source_probe: list() failed: {error}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "windows_share_source_probe: enumerated {} shareable entries",
        windows.len()
    );
    let mut display_count = 0usize;
    for window in &windows {
        // Displays enumerate first and title themselves "Screen N".
        if window
            .title
            .as_deref()
            .is_some_and(|title| title.starts_with("Screen "))
        {
            display_count += 1;
        }
        eprintln!(
            "  id={} title={:?} app={} pid={}",
            window.window_id, window.title, window.app_name, window.app_pid
        );
    }
    eprintln!("windows_share_source_probe: {display_count} display(s) listed first");
    if windows.is_empty() {
        eprintln!("windows_share_source_probe: no shareable entries to thumbnail");
        return;
    }

    // `PETAL_PROBE_FAILURE_PATHS=1`: verify fail-closed behavior for bad
    // tokens (the macOS reference fails fast with a clean error, never
    // "setup timed out" for an unresolvable target). Runs BEFORE the
    // thumbnail section so it works on hosts whose one-shot WGC capture is
    // unstable.
    if std::env::var("PETAL_PROBE_FAILURE_PATHS").is_ok_and(|value| !value.is_empty()) {
        let bad_tokens = [0u32, u32::MAX, 123_456_789u32];
        for token in bad_tokens {
            let result = std::thread::spawn(move || {
                desktop_lib::windows_screen_capture::TargetCaptureSession::start(
                    token,
                    desktop_lib::windows_screen_capture::CaptureIndicatorMode::System,
                    |_frame| {},
                )
            })
            .join();
            match result {
                Ok(Err(error)) => eprintln!(
                    "windows_share_source_probe: failure path token={token} -> Err({error})"
                ),
                Ok(Ok(_)) => eprintln!(
                    "windows_share_source_probe: FAILURE path token={token} UNEXPECTEDLY started"
                ),
                Err(_) => eprintln!(
                    "windows_share_source_probe: failure path token={token} thread panicked"
                ),
            }
        }
        return;
    }

    // `PETAL_PROBE_LIVE_ONLY=1`: skip the one-shot thumbnail section and go
    // straight to the live-session exercise (one-shot WGC capture on some
    // hosts crashes in GraphicsCapture.dll; the live path is the share path).
    let live_only = std::env::var("PETAL_PROBE_LIVE_ONLY").is_ok_and(|value| !value.is_empty());
    if !live_only {
        let mut thumbnailed = 0usize;
        // Windows first, display last: display capture is the first-thing-in-a-
        // fresh-process path most prone to WGC host flakiness (see plan
        // contingency: "if the host cannot run WGC at all, report it as an
        // environment limitation"). Window capture is the primary one-shot path.
        let mut order: Vec<&desktop_lib::window_source::ShareableWindow> =
            windows.iter().skip(1).collect();
        if let Some(display) = windows.first() {
            order.push(display);
        }
        for window in order.iter().take(4) {
            eprintln!(
                "windows_share_source_probe: [step] thumbnail id={} starting",
                window.window_id
            );
            match desktop_lib::window_source::capture_window_thumbnail(window.window_id) {
                Ok(bytes) => {
                    eprintln!(
                        "windows_share_source_probe: thumbnail id={} -> {} bytes (png)",
                        window.window_id,
                        bytes.len()
                    );
                    thumbnailed += 1;
                }
                Err(error) => {
                    eprintln!(
                        "windows_share_source_probe: thumbnail id={} failed: {error}",
                        window.window_id
                    );
                }
            }
            eprintln!(
                "windows_share_source_probe: [step] thumbnail id={} done",
                window.window_id
            );
        }
        eprintln!(
            "windows_share_source_probe: thumbnailed {thumbnailed}/{} entries",
            windows.len().min(4)
        );
        if thumbnailed == 0 {
            eprintln!("windows_share_source_probe: NO thumbnails captured");
            std::process::exit(2);
        }
    }

    // Live-session exercise: the share feature runs TargetCaptureSession::start
    // (FrameArrived handler on a dedicated thread), NOT the polling one-shot.
    // Prove a short live capture survives with real frames.
    //
    // `PETAL_PROBE_WINDOW_TITLE=<substring>` narrows the target to a window
    // whose title contains the substring (for capturing a known-static
    // window). The per-second histogram below is the "does WGC deliver
    // frames at all" measurement: static content may deliver the initial
    // frame and then go silent (efficient frame delivery).
    let target_title = std::env::var("PETAL_PROBE_WINDOW_TITLE")
        .ok()
        .filter(|value| !value.is_empty());
    let live_token = match &target_title {
        Some(substring) => windows
            .iter()
            .find(|window| {
                window
                    .title
                    .as_deref()
                    .is_some_and(|title| title.contains(substring.as_str()))
            })
            .map(|window| window.window_id),
        None => windows
            .iter()
            .find(|window| window.app_name != "Display")
            .map(|window| window.window_id)
            .or_else(|| windows.first().map(|window| window.window_id)),
    };
    if let Some(token) = live_token {
        eprintln!(
            "windows_share_source_probe: [step] live session token={token} starting (target_title={target_title:?})"
        );
        let frames = std::sync::Arc::new(parking_lot::Mutex::new(0u32));
        let callback_frames = frames.clone();
        let started = std::thread::spawn(move || {
            desktop_lib::windows_screen_capture::TargetCaptureSession::start(
                token,
                desktop_lib::windows_screen_capture::CaptureIndicatorMode::System,
                move |_frame| {
                    *callback_frames.lock() += 1;
                },
            )
        });
        let (session, status) = match started.join() {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                eprintln!("windows_share_source_probe: live session start failed: {error}");
                std::process::exit(2);
            }
            Err(_) => {
                eprintln!("windows_share_source_probe: live session thread panicked");
                std::process::exit(2);
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut last_count = 0u32;
        let mut last_log = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if std::time::Instant::now() >= deadline {
                break;
            }
            if last_log.elapsed() >= std::time::Duration::from_secs(1) {
                let count = *frames.lock();
                eprintln!(
                    "windows_share_source_probe: second-cadence frames={count} (+{})",
                    count - last_count
                );
                last_count = count;
                last_log = std::time::Instant::now();
            }
        }
        let received = *frames.lock();
        eprintln!(
            "windows_share_source_probe: live session received {received} frame(s) in 10s (terminal={:?})",
            status.terminal_error()
        );
        drop(session);
        eprintln!("windows_share_source_probe: [step] live session stopped");
        if received == 0 {
            std::process::exit(2);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("windows_share_source_probe is Windows-only");
}
