//! Objective frontmost-application sampler.
//!
//! Passive measurement instrument for focus/activation bugs (#120: "sharing a
//! window visibly flashes Petal to the foreground"). It samples
//! `NSWorkspace.frontmostApplication` at a fixed high rate and prints a
//! timeline of ownership transitions, so "did the app flash to the front?"
//! becomes a number in milliseconds instead of an eyeball judgement.
//!
//! It is deliberately decoupled from Petal: it observes ANY running app by
//! name/pid and never touches product code, so the same before/after
//! measurement works against a `tauri dev` binary, a packaged `.app`, or a
//! standalone AppKit probe.
//!
//! ```sh
//! cargo run --example frontmost_probe -- --watch Petal --seconds 12
//! cargo run --example frontmost_probe -- --watch-pid 4242 --seconds 12 --interval-ms 5
//! ```
//!
//! Output: every transition with its start offset and dwell time, then a
//! summary giving total and longest contiguous foreground time for `--watch`.
//! A non-zero "longest contiguous" for the watched app across a share-start is
//! exactly the flash this issue is about.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("frontmost_probe: macOS only");
}

#[cfg(target_os = "macos")]
fn main() {
    use std::io::Write;
    use std::time::{Duration, Instant};

    let mut watch: Option<String> = None;
    let mut watch_pid: Option<i32> = None;
    let mut seconds: f64 = 10.0;
    let mut interval_ms: u64 = 10;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--watch" => watch = args.next(),
            "--watch-pid" => watch_pid = args.next().and_then(|v| v.parse().ok()),
            "--seconds" => seconds = args.next().and_then(|v| v.parse().ok()).unwrap_or(10.0),
            "--interval-ms" => interval_ms = args.next().and_then(|v| v.parse().ok()).unwrap_or(10),
            other => eprintln!("frontmost_probe: ignoring unknown arg {other}"),
        }
    }

    eprintln!(
        "frontmost_probe: sampling every {interval_ms}ms for {seconds}s (watch={:?})",
        watch.as_deref()
    );
    std::io::stderr().flush().ok();

    // A single contiguous stretch during which one app owned the foreground.
    struct Span {
        front: FrontInner,
        start_ms: f64,
        end_ms: f64,
        samples: u32,
    }

    let start = Instant::now();
    let deadline = Duration::from_secs_f64(seconds);
    let mut spans: Vec<Span> = Vec::new();
    let mut total_samples: u64 = 0;

    while start.elapsed() < deadline {
        let now_ms = start.elapsed().as_secs_f64() * 1000.0;
        let front = frontmost();
        total_samples += 1;

        match spans.last_mut() {
            Some(last) if last.front == front => {
                last.end_ms = now_ms;
                last.samples += 1;
            }
            _ => spans.push(Span {
                front,
                start_ms: now_ms,
                end_ms: now_ms,
                samples: 1,
            }),
        }

        // Let NSWorkspace process its activation notifications; without a
        // runloop turn `frontmostApplication` can serve a stale cached value.
        pump_runloop(0.001);
        std::thread::sleep(Duration::from_millis(interval_ms));
    }

    println!("\n=== frontmost timeline ({total_samples} samples) ===");
    for span in &spans {
        // Dwell is measured to the first sample of the NEXT span, so a span's
        // real duration includes the sampling gap after its last hit.
        println!(
            "  {:>8.1}ms  +{:>7.1}ms  {} ({}, pid {})  [{} samples]",
            span.start_ms,
            span.end_ms - span.start_ms,
            span.front.name,
            span.front.bundle,
            span.front.pid,
            span.samples
        );
    }

    if watch.is_some() || watch_pid.is_some() {
        let label = watch
            .clone()
            .unwrap_or_else(|| format!("pid {}", watch_pid.unwrap_or(-1)));
        let needle = watch.unwrap_or_default().to_lowercase();
        // Prefer exact pid matching. Substring matching on a name/bundle
        // false-positives easily (watching "desktop" also matches an
        // unrelated `com.example.foodesktop`), which silently turns a real
        // measurement into a wrong verdict.
        let matches: Vec<&Span> = spans
            .iter()
            .filter(|s| match watch_pid {
                Some(pid) => s.front.pid == pid,
                None => {
                    s.front.name.to_lowercase().contains(&needle)
                        || s.front.bundle.to_lowercase().contains(&needle)
                }
            })
            .collect();
        let total: f64 = matches
            .iter()
            .map(|s| s.end_ms - s.start_ms)
            .sum::<f64>()
            .max(0.0);
        let longest = matches
            .iter()
            .map(|s| s.end_ms - s.start_ms)
            .fold(0.0f64, f64::max);
        println!("\n=== watch summary: {label} ===");
        println!("  foreground episodes : {}", matches.len());
        println!("  total foreground    : {total:.1}ms");
        println!("  longest contiguous  : {longest:.1}ms");
        println!(
            "  verdict             : {}",
            if matches.is_empty() {
                "CLEAN - watched app never held the foreground"
            } else {
                "FLASH - watched app held the foreground (see episodes above)"
            }
        );
    }
}

/// One sampled frontmost-application identity.
#[cfg(target_os = "macos")]
#[derive(Clone, PartialEq, Eq)]
struct FrontInner {
    pid: i32,
    name: String,
    bundle: String,
}

#[cfg(target_os = "macos")]
impl FrontInner {
    fn unknown() -> Self {
        Self {
            pid: -1,
            name: "unknown".to_string(),
            bundle: "unknown".to_string(),
        }
    }
}

#[cfg(target_os = "macos")]
fn pump_runloop(seconds: f64) {
    use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoopRunInMode};
    // SAFETY: plain CFRunLoop call on the current (main) thread.
    unsafe {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, 1);
    }
}

#[cfg(target_os = "macos")]
fn frontmost() -> FrontInner {
    // The typed objc2-app-kit API (rather than `class!(NSWorkspace)`) so the
    // AppKit framework is actually linked into this example binary.
    use objc2_app_kit::NSWorkspace;

    let front = unsafe { NSWorkspace::sharedWorkspace().frontmostApplication() };
    let Some(app) = front else {
        return FrontInner::unknown();
    };
    FrontInner {
        pid: unsafe { app.processIdentifier() },
        name: unsafe { app.localizedName() }
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".into()),
        bundle: unsafe { app.bundleIdentifier() }
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".into()),
    }
}
