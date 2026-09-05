//! SCK stream-churn IOSurface-leak probe (#878 follow-up).
//!
//! Question this answers: does cycling the PRODUCTION capture lifecycle
//! (`WindowCapture::start` -> frames -> `stop()` -> drop) leak IOSurfaces
//! on the SERVER side (WindowServer / replayd) -- memory invisible to
//! Petal's own footprint? Historical motivation: the #841 3Hz republish
//! storm and the 336x idle-restart storm (#capture-idle-restart-loop) ran
//! this exact lifecycle for hours on machines that later lost their window
//! server (#878), with system-wide `kCVReturnAllocationFailed` in the
//! endgame while Petal itself held almost nothing.
//!
//! Method: sample `ioclasscount IOSurface` (global kernel instance count)
//! and WindowServer/replayd RSS before churn (baseline), during churn, and
//! after a settle period. A leak reads as monotonic growth in the global
//! surface count / server RSS that survives the settle; lazy reclamation
//! reads as growth that returns to baseline after settling.
//!
//! Also reports THIS process's own RSS and its large private Foundation
//! region count per cycle (#889): the capture path leaks ~500MB-1GB of
//! Foundation buffers per share/unshare cycle in the real app, and the
//! original version of this probe could not see it because it watched only
//! system-side metrics.
//!
//! Usage:
//!   cargo run --example sck_churn_probe -- [window_id] [--cycles N] [--no-stop]
//!
//! `--no-stop` skips `stop()` and just drops the handle -- the error/crash
//! teardown path, exercised separately from the clean path on purpose.
//! No LiveKit, no encoder: pure SCK lifecycle churn, frames dropped on
//! arrival, so any growth is attributable to stream create/teardown alone.

#[cfg(target_os = "macos")]
mod probe {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[derive(Debug, Clone, Copy)]
    pub struct Sample {
        pub iosurface: u64,
        pub windowserver_rss_kb: u64,
        pub replayd_rss_kb: u64,
        /// THIS process's own phys_footprint (#889). The original version of
        /// this probe tracked only system-side metrics and therefore missed
        /// the biggest leak in the capture path -- ~500MB-1GB of Foundation
        /// buffers per share/unshare cycle, which lives in the SHARER's own
        /// footprint. Never ship a churn probe that cannot see its own
        /// memory.
        pub own_footprint_kb: u64,
        /// Count of the large private Foundation regions that dominate the
        /// leak (see #889); 0 when vmmap is unavailable.
        pub foundation_big_regions: u64,
        /// Total DIRTY MB in Foundation regions -- the number that costs
        /// real memory (#889).
        pub foundation_dirty_mb: u64,
    }

    /// phys_footprint, NOT RSS. RSS counts ~1.1GB of clean shared library
    /// text on this app and made a trivial probe look like it held 1.7GB
    /// (#889 measurement correction).
    fn own_footprint_kb() -> u64 {
        let pid = std::process::id().to_string();
        let Ok(out) = Command::new("/usr/bin/footprint").arg(&pid).output() else {
            return 0;
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|line| {
                let rest = line.trim().strip_prefix("phys_footprint:")?;
                let mut it = rest.split_whitespace();
                let value: f64 = it.next()?.parse().ok()?;
                let unit = it.next().unwrap_or("MB");
                Some(match unit {
                    "GB" => (value * 1024.0 * 1024.0) as u64,
                    "KB" => value as u64,
                    "B" => (value / 1024.0) as u64,
                    _ => (value * 1024.0) as u64,
                })
            })
            .unwrap_or(0)
    }

    /// Parse one vmmap region line into (virtual_mb, dirty_mb). Columns are
    /// NAME RANGE [ VIRTUAL RESIDENT DIRTY SWAPPED ] -- the first cut of this
    /// probe read the VIRTUAL column and therefore counted address-space
    /// RESERVATIONS as if they were committed memory (#889 correction: a
    /// reserved-but-untouched arena costs nothing).
    fn parse_region_sizes(line: &str) -> Option<(f64, f64)> {
        fn mb(token: &str) -> Option<f64> {
            let value: f64 = token
                .trim_end_matches(|c: char| c.is_ascii_alphabetic())
                .parse()
                .ok()?;
            Some(match token.chars().last()? {
                'G' => value * 1024.0,
                'K' => value / 1024.0,
                'M' => value,
                _ => value / (1024.0 * 1024.0),
            })
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        Some((mb(fields.get(3)?)?, mb(fields.get(5)?)?))
    }

    /// Total DIRTY MB across this process's Foundation regions, plus how
    /// many of them are >=16MB dirty. Dirty is the number that costs real
    /// memory (#889).
    fn foundation_dirty_mb() -> (u64, u64) {
        let pid = std::process::id().to_string();
        let Ok(out) = Command::new("/usr/bin/vmmap").arg(&pid).output() else {
            return (0, 0);
        };
        let mut total = 0.0;
        let mut big = 0u64;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if !line.starts_with("Foundation") {
                continue;
            }
            let Some((_virtual_mb, dirty_mb)) = parse_region_sizes(line) else {
                continue;
            };
            total += dirty_mb;
            if dirty_mb >= 16.0 {
                big += 1;
            }
        }
        (total as u64, big)
    }

    fn foundation_big_regions() -> u64 {
        foundation_dirty_mb().1
    }

    /// Sizes (MB) of this process's private Foundation regions >=16MB, for
    /// diagnosing WHAT is allocated (#889).
    fn foundation_big_region_sizes() -> Vec<String> {
        let pid = std::process::id().to_string();
        let Ok(out) = Command::new("/usr/bin/vmmap").arg(&pid).output() else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.starts_with("Foundation"))
            .filter_map(|line| {
                let (virtual_mb, dirty_mb) = parse_region_sizes(line)?;
                (dirty_mb >= 16.0).then(|| format!("{dirty_mb:.0}MB dirty (of {virtual_mb:.0}MB reserved)"))
            })
            .collect()
    }

    /// #889 discriminator: does merely ENUMERATING shareable content (no
    /// capture, no stream) allocate the big Foundation regions, and does
    /// repeating it grow them? The first `window_source::list()` in a fresh
    /// process was observed to bring ~1GB / 14 regions with it, while a
    /// process that never touches ScreenCaptureKit has zero.
    pub fn run_enumerate_only(rounds: u32) {
        println!("enumerate-only: {rounds} rounds of window_source::list()");
        println!(
            "PRE-SCK: footprint={}MB foundationDirty={}MB sizes={:?}",
            own_footprint_kb() / 1024,
            foundation_dirty_mb().0,
            foundation_big_region_sizes()
        );
        for round in 0..rounds {
            match desktop_lib::window_source::list() {
                Ok(w) => {
                    if round == 0 || (round + 1) % 5 == 0 {
                        println!(
                            "round {:3}: windows={} footprint={}MB foundationDirty={}MB",
                            round + 1,
                            w.len(),
                            own_footprint_kb() / 1024,
                            foundation_dirty_mb().0
                        );
                    }
                }
                Err(e) => {
                    eprintln!("list() failed: {e}");
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        std::thread::sleep(Duration::from_secs(5));
        println!(
            "POST: footprint={}MB foundationDirty={}MB sizes={:?}",
            own_footprint_kb() / 1024,
            foundation_dirty_mb().0,
            foundation_big_region_sizes()
        );
    }

    fn ioclasscount(class: &str) -> Option<u64> {
        let out = Command::new("/usr/sbin/ioclasscount").arg(class).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // "IOSurface = 168"
        text.split('=').nth(1)?.trim().parse().ok()
    }

    fn rss_kb_of(comm_exact: &str) -> u64 {
        let Ok(out) = Command::new("ps").args(["axo", "rss=,comm="]).output() else {
            return 0;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .filter_map(|line| {
                let mut parts = line.trim().splitn(2, ' ');
                let rss: u64 = parts.next()?.trim().parse().ok()?;
                let comm = parts.next()?.trim();
                // comm is the full executable path; match its basename.
                (comm.rsplit('/').next() == Some(comm_exact)).then_some(rss)
            })
            .sum()
    }

    pub fn sample() -> Sample {
        Sample {
            iosurface: ioclasscount("IOSurface").unwrap_or(0),
            windowserver_rss_kb: rss_kb_of("WindowServer"),
            replayd_rss_kb: rss_kb_of("replayd"),
            own_footprint_kb: own_footprint_kb(),
            foundation_big_regions: foundation_dirty_mb().1,
            foundation_dirty_mb: foundation_dirty_mb().0,
        }
    }

    pub fn print_sample(tag: &str, s: &Sample) {
        println!(
            "SAMPLE {tag}: IOSurface={} WindowServerRSS={}KB replaydRSS={}KB ownFootprint={}MB foundationDirty={}MB bigRegions={}",
            s.iosurface,
            s.windowserver_rss_kb,
            s.replayd_rss_kb,
            s.own_footprint_kb / 1024,
            s.foundation_dirty_mb,
            s.foundation_big_regions
        );
    }

    fn mean(values: &[u64]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        values.iter().sum::<u64>() as f64 / values.len() as f64
    }

    pub fn run(window_id: Option<u32>, cycles: u32, clean_stop: bool) {
        if !desktop_lib::window_source::has_screen_recording_access() {
            eprintln!(
                "FATAL: no Screen Recording access for this binary; grant it (or run from a \
                 granted shell -- SR inherits) and re-run."
            );
            std::process::exit(1);
        }

        let windows = match desktop_lib::window_source::list() {
            Ok(w) => w,
            Err(e) => {
                eprintln!("FATAL: window_source::list failed: {e}");
                std::process::exit(1);
            }
        };
        if windows.is_empty() {
            eprintln!("FATAL: no shareable windows.");
            std::process::exit(1);
        }
        // Source ids with this bit set are Petal's synthetic display entries
        // (window_source::DISPLAY_SOURCE_MARKER | CGDirectDisplayID) -- they
        // are NOT SCK window ids and need the display capture entry point.
        const DISPLAY_SOURCE_MARKER: u32 = 0x4000_0000;
        let target = match window_id {
            Some(id) => windows
                .iter()
                .find(|w| w.window_id == id)
                .unwrap_or_else(|| {
                    eprintln!("window_id {id} not in shareable set");
                    std::process::exit(1);
                }),
            // Default: first REAL window (a display pseudo-entry as default
            // burned run 1 -- SCK has no window by that synthetic id).
            None => windows
                .iter()
                .find(|w| w.window_id & DISPLAY_SOURCE_MARKER == 0)
                .unwrap_or_else(|| {
                    eprintln!("no real (non-display) shareable window found");
                    std::process::exit(1);
                }),
        };
        let is_display = target.window_id & DISPLAY_SOURCE_MARKER != 0;
        println!(
            "churn target: {} {} ({} - {:?}); cycles={cycles} clean_stop={clean_stop}",
            if is_display { "display-source" } else { "window" },
            target.window_id,
            target.app_name,
            target.title
        );

        println!(
            "baseline Foundation region sizes: {:?}",
            foundation_big_region_sizes()
        );
        // Baseline: 5 samples over ~10s with no capture at all.
        let mut baseline = Vec::new();
        for i in 0..5 {
            let s = sample();
            print_sample(&format!("baseline[{i}]"), &s);
            baseline.push(s);
            std::thread::sleep(Duration::from_secs(2));
        }

        let started = Instant::now();
        let mut cycle_ok = 0u32;
        let mut cycle_noframe = 0u32;
        for cycle in 0..cycles {
            let frames = Arc::new(AtomicU64::new(0));
            let fc = frames.clone();
            // Drop each frame (and its retained SCK CVPixelBuffer)
            // immediately: this probe isolates LIFECYCLE churn, not
            // retention.
            let on_frame = move |_frame| {
                fc.fetch_add(1, Ordering::Relaxed);
            };
            let start_result = if is_display {
                desktop_lib::capture::WindowCapture::start_display_with_error_handler_at_resolution(
                    target.window_id & !DISPLAY_SOURCE_MARKER,
                    10,
                    desktop_lib::transport::publisher::CaptureResolution::default(),
                    on_frame,
                    |_| {},
                )
            } else {
                desktop_lib::capture::WindowCapture::start(target.window_id, on_frame)
            };
            let capture = match start_result {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("cycle {cycle}: start failed: {e}");
                    // Server-side state of a FAILED start is part of what we
                    // are measuring; keep going.
                    continue;
                }
            };
            // Wait for up to 2s for >=2 frames -- proof the stream actually
            // ran (a stream that never delivers costs the server less).
            let wait_start = Instant::now();
            while frames.load(Ordering::Relaxed) < 2 && wait_start.elapsed() < Duration::from_secs(2)
            {
                std::thread::sleep(Duration::from_millis(30));
            }
            if frames.load(Ordering::Relaxed) >= 2 {
                cycle_ok += 1;
            } else {
                cycle_noframe += 1;
            }
            if clean_stop {
                let _ = capture.stop();
            }
            drop(capture);
            std::thread::sleep(Duration::from_millis(100));

            if (cycle + 1) % 10 == 0 {
                let s = sample();
                print_sample(&format!("churn[{}]", cycle + 1), &s);
            }
        }
        println!(
            "churn done: {cycles} cycles ({cycle_ok} with frames, {cycle_noframe} frameless) in {:.0}s",
            started.elapsed().as_secs_f64()
        );

        // Settle: give SCK/WindowServer time to reclaim lazily, then sample.
        let mut settle = Vec::new();
        for (i, delay_s) in [5u64, 10, 15].iter().enumerate() {
            std::thread::sleep(Duration::from_secs(*delay_s));
            let s = sample();
            print_sample(&format!("settle[{i}]"), &s);
            settle.push(s);
        }

        let base_surf = mean(&baseline.iter().map(|s| s.iosurface).collect::<Vec<_>>());
        let settle_surf = mean(&settle.iter().map(|s| s.iosurface).collect::<Vec<_>>());
        let base_ws = mean(&baseline.iter().map(|s| s.windowserver_rss_kb).collect::<Vec<_>>());
        let settle_ws = mean(&settle.iter().map(|s| s.windowserver_rss_kb).collect::<Vec<_>>());
        let base_rp = mean(&baseline.iter().map(|s| s.replayd_rss_kb).collect::<Vec<_>>());
        let settle_rp = mean(&settle.iter().map(|s| s.replayd_rss_kb).collect::<Vec<_>>());
        println!("RESULT: cycles={cycles} clean_stop={clean_stop}");
        println!(
            "RESULT: IOSurface baseline={base_surf:.1} settled={settle_surf:.1} delta={:+.1} ({:+.3}/cycle)",
            settle_surf - base_surf,
            (settle_surf - base_surf) / cycles as f64
        );
        println!(
            "RESULT: WindowServer RSS baseline={base_ws:.0}KB settled={settle_ws:.0}KB delta={:+.0}KB",
            settle_ws - base_ws
        );
        println!(
            "RESULT: replayd RSS baseline={base_rp:.0}KB settled={settle_rp:.0}KB delta={:+.0}KB",
            settle_rp - base_rp
        );
        // #889: the metric the original probe lacked.
        let base_own = mean(&baseline.iter().map(|s| s.own_footprint_kb).collect::<Vec<_>>());
        let settle_own = mean(&settle.iter().map(|s| s.own_footprint_kb).collect::<Vec<_>>());
        println!(
            "RESULT: OWN footprint baseline={:.0}MB settled={:.0}MB delta={:+.0}MB ({:+.2}MB/cycle)",
            base_own / 1024.0,
            settle_own / 1024.0,
            (settle_own - base_own) / 1024.0,
            (settle_own - base_own) / 1024.0 / cycles as f64
        );
        let base_fd = mean(&baseline.iter().map(|s| s.foundation_dirty_mb).collect::<Vec<_>>());
        let settle_fd = mean(&settle.iter().map(|s| s.foundation_dirty_mb).collect::<Vec<_>>());
        println!(
            "RESULT: Foundation DIRTY baseline={base_fd:.0}MB settled={settle_fd:.0}MB delta={:+.0}MB ({:+.2}MB/cycle)",
            settle_fd - base_fd,
            (settle_fd - base_fd) / cycles as f64
        );
        let base_fr = mean(&baseline.iter().map(|s| s.foundation_big_regions).collect::<Vec<_>>());
        let settle_fr = mean(&settle.iter().map(|s| s.foundation_big_regions).collect::<Vec<_>>());
        println!(
            "RESULT: big Foundation regions baseline={base_fr:.1} settled={settle_fr:.1} delta={:+.1} ({:+.3}/cycle)",
            settle_fr - base_fr,
            (settle_fr - base_fr) / cycles as f64
        );
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut window_id = None;
    let mut cycles = 150u32;
    let mut clean_stop = true;
    let mut enumerate_only = 0u32;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cycles" => {
                i += 1;
                cycles = args[i].parse().expect("--cycles N");
            }
            "--no-stop" => clean_stop = false,
            "--enumerate-only" => {
                i += 1;
                enumerate_only = args[i].parse().expect("--enumerate-only N");
            }
            other => window_id = Some(other.parse().expect("window_id must be a u32")),
        }
        i += 1;
    }
    if enumerate_only > 0 {
        probe::run_enumerate_only(enumerate_only);
        return;
    }
    probe::run(window_id, cycles, clean_stop);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("sck_churn_probe is macOS-only");
}
