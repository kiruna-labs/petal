//! Receiver-side decoder-churn leak probe (#878 follow-up, hypothesis 2 of
//! the churn ladder; sck_churn_probe was hypothesis 1).
//!
//! Field mechanism under test: every sharer-side republish forces every
//! receiver through new-remote-track -> new libwebrtc/VideoToolbox decoder
//! -> teardown of the old one. During the #841 storm era that ran at ~3Hz
//! for hours on the machine that later lost its window server, and the
//! 2026-08-24 endgame logged 28x kCVReturnAllocationFailed -- DECODE-side
//! pixel-buffer allocation failing machine-wide. This probe cycles the real
//! subscribe->decode->teardown path against a steady synthetic publisher
//! and watches for GPU/system memory that fails to come back.
//!
//! Measures per sample: global IOSurface count, WindowServer/replayd RSS,
//! this process's own RSS, and the AGX accelerator's "In use system
//! memory" / "Alloc system memory" / recoveryCount (GPU restarts) from
//! IOAccelerator's PerformanceStatistics.
//!
//! Needs: LIVEKIT_URL/LIVEKIT_API_KEY/LIVEKIT_API_SECRET (local
//! `livekit-server --dev`), and a running publisher in the same room, e.g.
//!   cargo run --example publish_probe -- <room> --source synthetic
//!
//! Usage: decode_churn_probe -- [room_name] [--cycles N] [--frames N]

#[cfg(target_os = "macos")]
mod probe {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[derive(Debug, Clone, Copy, Default)]
    pub struct Sample {
        pub iosurface: u64,
        pub windowserver_rss_kb: u64,
        pub replayd_rss_kb: u64,
        pub own_rss_kb: u64,
        pub gpu_in_use: u64,
        pub gpu_alloc: u64,
        pub gpu_restarts: u64,
    }

    fn ioclasscount(class: &str) -> Option<u64> {
        let out = Command::new("/usr/sbin/ioclasscount").arg(class).output().ok()?;
        String::from_utf8_lossy(&out.stdout)
            .split('=')
            .nth(1)?
            .trim()
            .parse()
            .ok()
    }

    fn rss_kb_of(comm_exact: &str) -> u64 {
        let Ok(out) = Command::new("ps").args(["axo", "rss=,comm="]).output() else {
            return 0;
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.trim().splitn(2, ' ');
                let rss: u64 = parts.next()?.trim().parse().ok()?;
                let comm = parts.next()?.trim();
                (comm.rsplit('/').next() == Some(comm_exact)).then_some(rss)
            })
            .sum()
    }

    fn own_rss_kb() -> u64 {
        let pid = std::process::id().to_string();
        let Ok(out) = Command::new("ps").args(["-o", "rss=", "-p", &pid]).output() else {
            return 0;
        };
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
    }

    /// Parse the AGX PerformanceStatistics dictionary out of `ioreg`.
    /// Multiple accelerators sum. recoveryCount is the GPU-restart counter
    /// -- any increase during the run is itself a headline finding.
    fn gpu_stats() -> (u64, u64, u64) {
        let Ok(out) = Command::new("ioreg").args(["-r", "-c", "IOAccelerator", "-d", "1"]).output()
        else {
            return (0, 0, 0);
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut in_use = 0u64;
        let mut alloc = 0u64;
        let mut restarts = 0u64;
        for (key, slot) in [
            ("\"In use system memory\"=", &mut in_use),
            ("\"Alloc system memory\"=", &mut alloc),
            ("\"recoveryCount\"=", &mut restarts),
        ] {
            for chunk in text.split(key).skip(1) {
                let digits: String = chunk.chars().take_while(|c| c.is_ascii_digit()).collect();
                *slot += digits.parse::<u64>().unwrap_or(0);
            }
        }
        (in_use, alloc, restarts)
    }

    pub fn sample() -> Sample {
        let (gpu_in_use, gpu_alloc, gpu_restarts) = gpu_stats();
        Sample {
            iosurface: ioclasscount("IOSurface").unwrap_or(0),
            windowserver_rss_kb: rss_kb_of("WindowServer"),
            replayd_rss_kb: rss_kb_of("replayd"),
            own_rss_kb: own_rss_kb(),
            gpu_in_use,
            gpu_alloc,
            gpu_restarts,
        }
    }

    pub fn print_sample(tag: &str, s: &Sample) {
        println!(
            "SAMPLE {tag}: IOSurface={} WS={}KB replayd={}KB own={}KB gpuInUse={}MB gpuAlloc={}MB gpuRestarts={}",
            s.iosurface,
            s.windowserver_rss_kb,
            s.replayd_rss_kb,
            s.own_rss_kb,
            s.gpu_in_use / (1024 * 1024),
            s.gpu_alloc / (1024 * 1024),
            s.gpu_restarts
        );
    }

    fn mean<F: Fn(&Sample) -> u64>(samples: &[Sample], f: F) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        samples.iter().map(&f).sum::<u64>() as f64 / samples.len() as f64
    }

    pub fn run(
        room_name: &str,
        cycles: u32,
        frames_per_cycle: u64,
        connect_only: bool,
        resubscribe: bool,
        passive_seconds: u64,
        runtime_per_cycle: bool,
    ) {
        let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });

        println!(
            "decode churn: room='{room_name}' cycles={cycles} frames_per_cycle={frames_per_cycle} connect_only={connect_only} resubscribe={resubscribe}"
        );

        let mut baseline = Vec::new();
        for i in 0..5 {
            let s = sample();
            print_sample(&format!("baseline[{i}]"), &s);
            baseline.push(s);
            std::thread::sleep(Duration::from_secs(2));
        }

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let started = Instant::now();
        let mut decoded_total = 0u64;
        let mut cycle_ok = 0u32;
        let mut cycle_dry = 0u32;
        if passive_seconds > 0 {
            // Variant C: ONE persistent auto-subscribing room; the PUBLISHER
            // process restarts per cycle (driven externally), so the receiver
            // sees real TrackUnsubscribed/TrackSubscribed churn -- the exact
            // field topology of a sharer republish storm (#841). Each new
            // track gets a NativeVideoStream whose frames are dropped on
            // arrival; memory is sampled every 15s. `--cycles` here is the
            // EXPECTED number of publisher restarts (for per-cycle math in
            // the report), driven by the external loop.
            let token = desktop_lib::transport::mint_access_token(
                "decode-churn-passive",
                room_name,
                false,
                true,
            )
            .unwrap_or_else(|e| {
                eprintln!("mint failed: {e}");
                std::process::exit(1);
            });
            let (tracks_seen, frames_seen) = rt.block_on(async move {
                use futures::StreamExt;
                use livekit::prelude::*;
                use livekit::webrtc::video_stream::native::NativeVideoStream;

                let mut room_options = RoomOptions::default();
                room_options.auto_subscribe = true;
                let (room, mut events) = match Room::connect(&url, &token, room_options).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("connect failed: {e}");
                        std::process::exit(1);
                    }
                };
                println!("passive: connected; receiving for {passive_seconds}s");
                let tracks_seen = Arc::new(AtomicU64::new(0));
                let frames_seen = Arc::new(AtomicU64::new(0));
                let deadline = tokio::time::Instant::now()
                    + Duration::from_secs(passive_seconds);
                let mut ticker = tokio::time::interval(Duration::from_secs(15));
                let mut sample_index = 0u32;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => break,
                        _ = ticker.tick() => {
                            let s = sample();
                            print_sample(&format!("passive[{sample_index}]"), &s);
                            sample_index += 1;
                        }
                        event = events.recv() => {
                            let Some(event) = event else { break };
                            if let RoomEvent::TrackSubscribed { track, .. } = event {
                                if let RemoteTrack::Video(video_track) = track {
                                    let n = tracks_seen.fetch_add(1, Ordering::Relaxed) + 1;
                                    println!("passive: track #{n} subscribed");
                                    let frames = frames_seen.clone();
                                    tokio::spawn(async move {
                                        let mut stream =
                                            NativeVideoStream::new(video_track.rtc_track());
                                        while let Some(_frame) = stream.next().await {
                                            frames.fetch_add(1, Ordering::Relaxed);
                                        }
                                    });
                                }
                            }
                        }
                    }
                }
                room.close().await.ok();
                (
                    tracks_seen.load(Ordering::Relaxed),
                    frames_seen.load(Ordering::Relaxed),
                )
            });
            println!("passive: saw {tracks_seen} tracks, {frames_seen} frames");
            finish(started, cycles, tracks_seen as u32, 0, frames_seen, &baseline);
            return;
        }
        if resubscribe {
            // Variant B: ONE persistent room; churn only the subscription
            // (set_subscribed toggle -> fresh decoder per cycle). This is
            // the exact receiver-side topology of a sharer republish storm
            // (#841): the room and peer connection survive, the track and
            // its decoder churn.
            let token = desktop_lib::transport::mint_access_token(
                "decode-churn-resub",
                room_name,
                false,
                true,
            )
            .unwrap_or_else(|e| {
                eprintln!("mint failed: {e}");
                std::process::exit(1);
            });
            let (ok_out, dry_out, decoded_out) = rt.block_on(async move {
                use futures::StreamExt;
                use livekit::prelude::*;
                use livekit::webrtc::video_stream::native::NativeVideoStream;

                let mut room_options = RoomOptions::default();
                room_options.auto_subscribe = false;
                let (room, mut events) = match Room::connect(&url, &token, room_options).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("connect failed: {e}");
                        std::process::exit(1);
                    }
                };
                // Find the publisher's video publication.
                let publication = 'find: loop {
                    for (_, participant) in room.remote_participants() {
                        for (_, publication) in participant.track_publications() {
                            if publication.kind() == TrackKind::Video {
                                break 'find publication;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                };
                println!("resubscribe: found video publication {}", publication.sid());

                let mut ok = 0u32;
                let mut dry = 0u32;
                let mut decoded_total = 0u64;
                for cycle in 0..cycles {
                    // Drain any stale events from the previous cycle so this
                    // cycle's TrackSubscribed is not queued behind them.
                    while let Ok(stale) =
                        tokio::time::timeout(Duration::from_millis(10), events.recv()).await
                    {
                        if stale.is_none() {
                            break;
                        }
                    }
                    publication.set_subscribed(true);
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
                    let mut decoded = 0u64;
                    'wait: while let Ok(Some(event)) =
                        tokio::time::timeout_at(deadline, events.recv()).await
                    {
                        if let RoomEvent::TrackSubscribed { track, .. } = event {
                            let RemoteTrack::Video(video_track) = track else {
                                continue;
                            };
                            let mut stream = NativeVideoStream::new(video_track.rtc_track());
                            while let Ok(Some(_frame)) =
                                tokio::time::timeout_at(deadline, stream.next()).await
                            {
                                decoded += 1;
                                if decoded >= frames_per_cycle {
                                    break 'wait;
                                }
                            }
                            break 'wait;
                        }
                    }
                    publication.set_subscribed(false);
                    decoded_total += decoded;
                    if decoded >= frames_per_cycle {
                        ok += 1;
                    } else {
                        dry += 1;
                        println!("cycle[{cycle}] DRY decoded={decoded}");
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if (cycle + 1) % 10 == 0 {
                        let s = sample();
                        print_sample(&format!("churn[{}]", cycle + 1), &s);
                    }
                }
                room.close().await.ok();
                (ok, dry, decoded_total)
            });
            cycle_ok = ok_out;
            cycle_dry = dry_out;
            decoded_total = decoded_out;
            finish(
                started, cycles, cycle_ok, cycle_dry, decoded_total, &baseline,
            );
            return;
        }
        for cycle in 0..cycles {
            let identity = format!("decode-churn-{cycle}");
            let token = desktop_lib::transport::mint_access_token(&identity, room_name, false, true)
                .unwrap_or_else(|e| {
                    eprintln!("mint failed: {e}");
                    std::process::exit(1);
                });
            let url = url.clone();
            let decoded = Arc::new(AtomicU64::new(0));
            let decoded_in = decoded.clone();
            // #883 discriminator: a fresh runtime per cycle is DROPPED at
            // cycle end, killing any detached task that never completed. If
            // the ~320KB/cycle still-reachable growth vanishes in this mode,
            // the holder is a never-ending spawned task; if it persists, it
            // is an Arc cycle or a C++-side refcount.
            let cycle_rt = runtime_per_cycle
                .then(|| tokio::runtime::Runtime::new().expect("cycle runtime"));
            let ok = cycle_rt.as_ref().unwrap_or(&rt).block_on(async move {
                use futures::StreamExt;
                use livekit::prelude::*;
                use livekit::webrtc::video_stream::native::NativeVideoStream;

                let mut room_options = RoomOptions::default();
                room_options.auto_subscribe = !connect_only;
                let (room, mut events) = match Room::connect(&url, &token, room_options).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("cycle: connect failed: {e}");
                        return false;
                    }
                };
                if connect_only {
                    // Variant A: isolate Room/peer-connection churn -- no
                    // subscription, no track, no decoder ever created.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    room.close().await.ok();
                    drop(room);
                    drop(events);
                    return true;
                }
                let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
                let mut got_frames = false;
                'outer: while let Ok(Some(event)) =
                    tokio::time::timeout_at(deadline, events.recv()).await
                {
                    if let RoomEvent::TrackSubscribed { track, .. } = event {
                        let RemoteTrack::Video(video_track) = track else {
                            continue;
                        };
                        let mut stream = NativeVideoStream::new(video_track.rtc_track());
                        // Decode a bounded burst, dropping every frame on
                        // arrival: this isolates decoder-session churn, not
                        // frame retention.
                        while let Ok(Some(_frame)) =
                            tokio::time::timeout_at(deadline, stream.next()).await
                        {
                            let n = decoded_in.fetch_add(1, Ordering::Relaxed) + 1;
                            if n >= frames_per_cycle {
                                got_frames = true;
                                break 'outer;
                            }
                        }
                        break 'outer;
                    }
                }
                room.close().await.ok();
                drop(room);
                got_frames
            });
            decoded_total += decoded.load(Ordering::Relaxed);
            if ok {
                cycle_ok += 1;
            } else {
                cycle_dry += 1;
            }
            std::thread::sleep(Duration::from_millis(100));
            if (cycle + 1) % 10 == 0 {
                let s = sample();
                print_sample(&format!("churn[{}]", cycle + 1), &s);
            }
        }
        finish(started, cycles, cycle_ok, cycle_dry, decoded_total, &baseline);
    }

    /// Keep the process alive after the run so `heap`/`leaks`/`malloc_history`
    /// can inspect still-reachable allocations (a leak that accumulates in a
    /// live structure is invisible to `leaks --atExit`).
    pub fn hold(seconds: u64) {
        if seconds == 0 {
            return;
        }
        println!(
            "HOLDING pid={} for {seconds}s -- inspect with: heap {0} / leaks {0}",
            std::process::id()
        );
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    }

    fn finish(
        started: Instant,
        cycles: u32,
        cycle_ok: u32,
        cycle_dry: u32,
        decoded_total: u64,
        baseline: &[Sample],
    ) {
        println!(
            "churn done: {cycles} cycles ({cycle_ok} full, {cycle_dry} dry), {decoded_total} frames decoded in {:.0}s",
            started.elapsed().as_secs_f64()
        );

        let mut settle = Vec::new();
        for (i, delay_s) in [5u64, 10, 15].iter().enumerate() {
            std::thread::sleep(Duration::from_secs(*delay_s));
            let s = sample();
            print_sample(&format!("settle[{i}]"), &s);
            settle.push(s);
        }

        println!("RESULT: cycles={cycles} decoded={decoded_total}");
        let report = |name: &str, f: fn(&Sample) -> u64, unit: &str, scale: u64| {
            let b = mean(baseline, f) / scale as f64;
            let s = mean(&settle, f) / scale as f64;
            println!(
                "RESULT: {name} baseline={b:.1}{unit} settled={s:.1}{unit} delta={:+.1}{unit} ({:+.4}/cycle)",
                s - b,
                (s - b) / cycles as f64
            );
        };
        report("IOSurface", |s| s.iosurface, "", 1);
        report("WindowServerRSS", |s| s.windowserver_rss_kb, "KB", 1);
        report("ownRSS", |s| s.own_rss_kb, "KB", 1);
        report("gpuInUse", |s| s.gpu_in_use, "MB", 1024 * 1024);
        report("gpuAlloc", |s| s.gpu_alloc, "MB", 1024 * 1024);
        let restart_delta =
            settle.last().map(|s| s.gpu_restarts).unwrap_or(0) as i64
                - baseline.first().map(|s| s.gpu_restarts).unwrap_or(0) as i64;
        println!("RESULT: gpuRestarts delta={restart_delta:+} (ANY increase is a headline finding)");
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut room_name = "decode-churn-probe".to_string();
    let mut cycles = 150u32;
    let mut frames = 30u64;
    let mut connect_only = false;
    let mut resubscribe = false;
    let mut passive_seconds = 0u64;
    let mut hold_seconds = 0u64;
    let mut runtime_per_cycle = false;
    let mut i = 0;
    let mut room_seen = false;
    while i < args.len() {
        match args[i].as_str() {
            "--cycles" => {
                i += 1;
                cycles = args[i].parse().expect("--cycles N");
            }
            "--frames" => {
                i += 1;
                frames = args[i].parse().expect("--frames N");
            }
            "--connect-only" => connect_only = true,
            "--resubscribe" => resubscribe = true,
            "--passive-seconds" => {
                i += 1;
                passive_seconds = args[i].parse().expect("--passive-seconds N");
            }
            "--hold-seconds" => {
                i += 1;
                hold_seconds = args[i].parse().expect("--hold-seconds N");
            }
            "--runtime-per-cycle" => runtime_per_cycle = true,
            other if !room_seen => {
                room_name = other.to_string();
                room_seen = true;
            }
            other => panic!("unexpected argument {other}"),
        }
        i += 1;
    }
    probe::run(
        &room_name,
        cycles,
        frames,
        connect_only,
        resubscribe,
        passive_seconds,
        runtime_per_cycle,
    );
    probe::hold(hold_seconds);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("decode_churn_probe is macOS-only");
}
