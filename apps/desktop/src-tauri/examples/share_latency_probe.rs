//! #613/#299: single-process, real-capture share-latency probe.
//!
//! Usage: `cargo run --example share_latency_probe -- [window_id] [room_name] [FLAGS]`
//!
//! Captures a real on-screen window, publishes it through the product's own
//! `publish_window_at` path (so the ladder, codec, and frame-metadata stamping
//! are exactly what a real share puts on the wire), joins a subscriber peer to
//! the same room IN THIS PROCESS, and reports capture-stamp -> decoded-frame
//! latency from LiveKit's frame-metadata trailer (`user_timestamp`, stamped by
//! `PublishedTrack::push_frame` with the capture wall clock). One process, one
//! wall clock -- no cross-process offset correction.
//!
//! What this number IS: a per-ladder, real-ScreenCaptureKit-source latency
//! reading you can take with one command, comparable across
//! `PETAL_SHARE_LADDER` values. What it is NOT: glass-to-glass. It excludes
//! the receiver compositor enqueue and display presentation; for the
//! presentation-inclusive matrix use
//! `scripts/run-issue613-presentation-latency.mjs`. It is a LOWER BOUND on
//! what a user experiences.
//!
//! Where it sits among the other probes: `startup_layer_probe` takes the same
//! measurement from a SYNTHETIC I420 source (a queue on the real capture path
//! would be invisible to it -- that gap is why this probe exists), and the
//! `publish_probe` + `subscribe_probe` pair takes it across two processes
//! under `run-issue613-receiver-start-order.sh`'s pre-registered protocol.
//! This probe is the lightweight experiment loop between those: real capture,
//! no orchestration.
//!
//! Flags:
//!   `--pin-lowest`           POSITIVE CONTROL: request a fixed 160x90 for the
//!                            whole run, so the SFU can only serve the live
//!                            ladder's BOTTOM rung. The reported decoded size
//!                            must equal that rung -- if it does not, the build
//!                            is stale and the run is void.
//!   `--inject-delay-ms N`    POSITIVE CONTROL: withhold each captured frame
//!                            from the pipeline for N ms AFTER its capture
//!                            stamp. Reported latency must rise by ~N.
//!   `--steady-after-ms N`    discard everything before this from the latency
//!                            summary (default 8000), so the #299 subscription
//!                            ramp cannot contaminate a steady-state number.
//!   `--seconds N`            length of the measured steady-state window
//!                            (default 30); the total run is
//!                            `--steady-after-ms` + N.
//!
//! `PETAL_PROBE_DUMP=/path.csv` writes every decoded-frame observation (same
//! contract as `subscribe_probe`). Reads LIVEKIT_URL/LIVEKIT_API_KEY/
//! LIVEKIT_API_SECRET from `apps/desktop/.env` (via `dotenvy`) -- never logs
//! their values.
//!
//! Experiment loop for #613/#299 work -- not cockpit apparatus and not a
//! runtime diagnostic subsystem (COURSE_CORRECTION.md §2.1).

#[cfg(target_os = "macos")]
mod probe {
    use futures::StreamExt;
    use livekit::prelude::*;
    use livekit::track::VideoQuality;
    use livekit::track::{RemoteTrack, RemoteVideoTrack};
    use livekit::webrtc::video_stream::native::NativeVideoStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    pub fn now_us() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }

    #[derive(Debug, Clone, Copy)]
    pub struct FrameObservation {
        pub at_ms: u128,
        pub width: u32,
        pub height: u32,
        pub latency_ms: Option<f64>,
    }

    /// Names the simulcast layer from the decoded buffer's own dimensions,
    /// against the ladder that is ACTUALLY live at the REAL capture size.
    ///
    /// Deliberately not a hand-copied q/h/f mirror: under `raised` both lower
    /// rungs would print as `h`, and under `two-rung` the source rung is `h`,
    /// not `f`. It must also be computed at the captured window's own
    /// dimensions, not at an assumed 1920x1080 -- the rungs are fractions of
    /// the source.
    pub fn layer_name(rungs: &[(String, u32, u32)], width: u32) -> String {
        let mut best: Option<&(String, u32, u32)> = None;
        for rung in rungs {
            if width >= rung.1 && best.map_or(true, |b| rung.1 >= b.1) {
                best = Some(rung);
            }
        }
        match best.or_else(|| rungs.first()) {
            Some((rid, w, h)) => format!("{rid} ({w}x{h})"),
            None => "?".to_string(),
        }
    }

    fn pct(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return f64::NAN;
        }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx]
    }

    pub fn summarize(label: &str, mut v: Vec<f64>) {
        if v.is_empty() {
            println!("  {label:<26}      n=0   <no samples>");
            return;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = v.len();
        let avg = v.iter().sum::<f64>() / n as f64;
        println!(
            "  {label:<26} n={n:<5} avg={avg:7.1}  p50={:7.1}  p95={:7.1}  min={:7.1}  max={:7.1}",
            pct(&v, 0.50),
            pct(&v, 0.95),
            v[0],
            v[n - 1]
        );
    }

    /// RTCStats counters, sampled as a PAIR so every reported number is a
    /// delta over the measured window rather than a since-connect cumulative
    /// average with the startup ramp baked in.
    #[derive(Debug, Clone, Default)]
    pub struct StageCounters {
        pub at: Option<Instant>,
        // outbound
        pub frames_encoded: u32,
        pub total_encode_time: f64,
        pub packets_sent: u64,
        pub total_packet_send_delay: f64,
        // inbound
        pub frames_decoded: u32,
        pub frames_dropped: u32,
        pub total_decode_time: f64,
        pub total_processing_delay: f64,
        pub total_assembly_time: f64,
        pub frames_assembled_from_multiple_packets: u64,
        pub jitter_buffer_delay: f64,
        pub jitter_buffer_target_delay: f64,
        pub jitter_buffer_minimum_delay: f64,
        pub jitter_buffer_emitted_count: u64,
        pub total_inter_frame_delay: f64,
        pub encoder_impl: Option<String>,
        pub quality_limitation: Option<String>,
    }

    pub async fn sample_outbound(
        track: &livekit::track::LocalVideoTrack,
        into: &mut StageCounters,
    ) {
        let Ok(stats) = track.get_stats().await else {
            return;
        };
        for stat in &stats {
            if let livekit::webrtc::stats::RtcStats::OutboundRtp(o) = stat {
                // Simulcast: sum across layers. Encode cost is paid on every
                // active layer; attributing only the top one understates the
                // sender's real per-frame work.
                into.frames_encoded += o.outbound.frames_encoded;
                into.total_encode_time += o.outbound.total_encode_time;
                into.packets_sent += o.sent.packets_sent;
                into.total_packet_send_delay += o.outbound.total_packet_send_delay;
                if into.encoder_impl.is_none() && !o.outbound.encoder_implementation.is_empty() {
                    into.encoder_impl = Some(o.outbound.encoder_implementation.clone());
                }
                if into.quality_limitation.is_none() {
                    into.quality_limitation =
                        Some(format!("{:?}", o.outbound.quality_limitation_reason));
                }
            }
        }
    }

    pub async fn sample_inbound(track: &RemoteVideoTrack, into: &mut StageCounters) {
        let Ok(stats) = track.get_stats().await else {
            return;
        };
        for stat in &stats {
            if let livekit::webrtc::stats::RtcStats::InboundRtp(i) = stat {
                into.frames_decoded += i.inbound.frames_decoded;
                into.frames_dropped += i.inbound.frames_dropped;
                into.total_decode_time += i.inbound.total_decode_time;
                into.total_processing_delay += i.inbound.total_processing_delay;
                into.total_assembly_time += i.inbound.total_assembly_time;
                into.frames_assembled_from_multiple_packets +=
                    i.inbound.frames_assembled_from_multiple_packets;
                into.jitter_buffer_delay += i.inbound.jitter_buffer_delay;
                into.jitter_buffer_target_delay += i.inbound.jitter_buffer_target_delay;
                into.jitter_buffer_minimum_delay += i.inbound.jitter_buffer_minimum_delay;
                into.jitter_buffer_emitted_count += i.inbound.jitter_buffer_emitted_count;
                into.total_inter_frame_delay += i.inbound.total_inter_frame_delay;
            }
        }
    }

    fn per_unit_ms(delta_seconds: f64, count: u64) -> Option<f64> {
        if count == 0 {
            return None;
        }
        Some(delta_seconds / count as f64 * 1000.0)
    }

    fn fmt_ms(v: Option<f64>) -> String {
        match v {
            Some(v) => format!("{v:8.2}"),
            None => "     n/a".to_string(),
        }
    }

    /// Joins a subscriber peer to `room_name` in this process and starts
    /// recording decoded-frame observations. Returns the room (kept alive by
    /// the caller) and the shared observation buffer.
    pub async fn spawn_subscriber(
        url: &str,
        room_name: &str,
        pin_lowest: bool,
        stop: Arc<AtomicBool>,
        observations: Arc<Mutex<Vec<FrameObservation>>>,
        remote_track: Arc<Mutex<Option<RemoteVideoTrack>>>,
    ) -> Room {
        let token = desktop_lib::transport::mint_access_token(
            "petal-613-sub",
            room_name,
            /* can_publish */ false,
            /* can_subscribe */ true,
        )
        .unwrap_or_else(|e| {
            eprintln!("Failed to mint subscriber token: {e}");
            std::process::exit(1);
        });

        let (room, mut events) = Room::connect(url, &token, RoomOptions::default())
            .await
            .unwrap_or_else(|e| {
                eprintln!("subscriber connect failed: {e}");
                std::process::exit(1);
            });

        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let RoomEvent::TrackSubscribed {
                    track, publication, ..
                } = event
                else {
                    continue;
                };
                let RemoteTrack::Video(video) = track else {
                    continue;
                };
                // Match on the share-track prefix, not on a parsed window id:
                // `publish_window_at` names the track `petal-window-<id>` when
                // it is given an id and `petal-window-capture` when it is not,
                // and only the first parses. Print the name so the log itself
                // shows which track was measured rather than leaving it to be
                // assumed.
                if !video.name().starts_with("petal-window") {
                    continue;
                }
                println!("  [sub] subscribed to track '{}'", video.name());

                let obs = observations.clone();
                let s = stop.clone();
                let subscribed_at = Instant::now();
                *remote_track.lock().unwrap() = Some(video.clone());
                tokio::spawn(async move {
                    let mut stream = NativeVideoStream::new(video.rtc_track());
                    while let Some(frame) = stream.next().await {
                        if s.load(Ordering::Relaxed) {
                            break;
                        }
                        let now = now_us();
                        let latency_ms = frame
                            .frame_metadata
                            .as_ref()
                            .and_then(|m| m.user_timestamp)
                            .map(|stamped| (now.saturating_sub(stamped)) as f64 / 1000.0);
                        obs.lock().unwrap().push(FrameObservation {
                            at_ms: subscribed_at.elapsed().as_millis(),
                            width: frame.buffer.width(),
                            height: frame.buffer.height(),
                            latency_ms,
                        });
                    }
                });

                if pin_lowest {
                    // POSITIVE CONTROL. The SFU serves the largest published
                    // layer at or below the requested size, falling back to
                    // the lowest when none fits. 160x90 is below every rung of
                    // every ladder at any capture size we measure, so it can
                    // only resolve to the BOTTOM rung of whatever ladder is
                    // live -- and the requested size is a FIXED constant, not
                    // derived from the ladder, so the check is not circular.
                    // (A larger constant silently breaks this: 640x360 sits
                    // ABOVE the legacy ladder's bottom rung once the captured
                    // window is narrower than ~2560px, and then resolves one
                    // rung too high.)
                    println!("  [sub] CONTROL: update_video_dimensions(160x90)");
                    publication.update_video_dimensions(TrackDimension(160, 90));
                } else {
                    // What the real receiver asks for on subscribe
                    // (`initial_window_subscription_plan_for_track`).
                    publication.set_video_quality(VideoQuality::High);
                }
            }
        });

        room
    }

    #[allow(clippy::too_many_arguments)]
    pub fn report(
        obs: &[FrameObservation],
        rungs: &[(String, u32, u32)],
        steady_after_ms: u128,
        inject_delay_ms: u64,
        pin_lowest: bool,
        start: &StageCounters,
        end: &StageCounters,
        pushed_start: u64,
        pushed_end: u64,
        overwrites_start: u64,
        overwrites_end: u64,
    ) {
        println!(
            "\n=== #613 REAL-CAPTURE capture->decode latency (capture stamp -> decoded frame in-process) ==="
        );
        println!(
            "  LOWER BOUND on glass-to-glass: excludes compositor enqueue + display presentation."
        );
        if obs.is_empty() {
            println!("  <no frames decoded>  -- nothing measured, run is void.");
            return;
        }
        // #613 bimodality question: mode occupancy cannot be recovered from
        // p50/p95/min/max, so dump every observation and compute the shape
        // offline. Deliberately a raw dump and not an in-probe histogram --
        // bucket edges chosen before seeing the data are their own way to
        // manufacture a mode. PETAL_PROBE_DUMP names the CSV path.
        //
        // VERIFY IT: p50/p95 recomputed from the CSV must equal the printed
        // percentiles below. A dump that disagrees with the line above it is
        // wrong and nothing downstream of it counts.
        if let Ok(path) = std::env::var("PETAL_PROBE_DUMP") {
            use std::io::Write;
            match std::fs::File::create(&path) {
                Ok(mut f) => {
                    let _ = writeln!(f, "at_ms,width,height,latency_ms");
                    for o in obs {
                        let _ = writeln!(
                            f,
                            "{},{},{},{}",
                            o.at_ms,
                            o.width,
                            o.height,
                            o.latency_ms.map(|v| v.to_string()).unwrap_or_default()
                        );
                    }
                    println!("  raw samples dumped to {path} ({} rows)", obs.len());
                }
                Err(e) => println!("  WARNING: could not write {path}: {e}"),
            }
        }
        let missing = obs.iter().filter(|o| o.latency_ms.is_none()).count();
        if missing > 0 {
            println!(
                "  WARNING: {missing}/{} frames carried NO user_timestamp. The frame-metadata \
                 path is not fully engaged; treat every number below as suspect.",
                obs.len()
            );
        }
        println!("  (single process, one wall clock -- no cross-machine offset correction needed)");

        let ramp: Vec<f64> = obs
            .iter()
            .filter(|o| o.at_ms < steady_after_ms)
            .filter_map(|o| o.latency_ms)
            .collect();
        let steady: Vec<f64> = obs
            .iter()
            .filter(|o| o.at_ms >= steady_after_ms)
            .filter_map(|o| o.latency_ms)
            .collect();
        summarize(&format!("ramp <{steady_after_ms}ms"), ramp);
        summarize(&format!("STEADY >={steady_after_ms}ms"), steady);

        // Per-layer buckets from the LIVE ladder's rung widths at the REAL
        // capture size -- never a fixed ratio against an assumed source.
        for (i, (rid, w, h)) in rungs.iter().enumerate() {
            let lo = *w;
            let hi = rungs
                .get(i + 1)
                .map(|next| next.1.saturating_sub(1))
                .unwrap_or(u32::MAX);
            let v: Vec<f64> = obs
                .iter()
                .filter(|o| o.width >= lo && o.width <= hi)
                .filter_map(|o| o.latency_ms)
                .collect();
            summarize(&format!("layer {rid} ({w}x{h})"), v);
        }

        // ---- decoded-layer timeline + the --pin-lowest control readout ----
        println!("\n=== decoded-layer timeline ===");
        let mut last = (0u32, 0u32);
        for o in obs {
            if (o.width, o.height) != last {
                println!(
                    "  t={:>6}ms  decoded={}x{}  layer={}",
                    o.at_ms,
                    o.width,
                    o.height,
                    layer_name(rungs, o.width)
                );
                last = (o.width, o.height);
            }
        }
        let first = obs[0];
        println!(
            "  initial decoded layer     : {}  ({}x{})",
            layer_name(rungs, first.width),
            first.width,
            first.height
        );
        if pin_lowest {
            // The control's whole point: it must name the bottom rung of the
            // ladder that is live NOW, at the real capture size -- not a
            // remembered geometry from whichever ladder used to be default.
            let bottom = rungs.first().cloned().unwrap_or_default();
            let steady_mode = obs
                .iter()
                .filter(|o| o.at_ms >= steady_after_ms)
                .map(|o| (o.width, o.height))
                .last();
            println!(
                "  CONTROL --pin-lowest      : live ladder bottom rung is {} {}x{}; \
                 steady-state decoded {:?}",
                bottom.0, bottom.1, bottom.2, steady_mode
            );
            // Tolerance, not sloppiness: H.264 requires even dimensions, so a
            // rung computed as an odd width (421 at this capture size) is
            // published as 420. An exact-equality check reads that as a stale
            // build. 2px is far below the gap between any two rungs.
            match steady_mode {
                Some((w, _)) if w.abs_diff(bottom.1) <= 2 => {
                    println!("  CONTROL RESULT            : PASS -- resolves to the live ladder's bottom rung.")
                }
                _ => println!(
                    "  CONTROL RESULT            : FAIL -- did NOT resolve to the live ladder's \
                     bottom rung. Build is stale or the ladder is not applied; RUN IS VOID."
                ),
            }
        }

        // ---- per-stage attribution over the steady window -----------------
        println!("\n=== #613 per-stage attribution (RTCStats deltas over the steady window) ===");
        let window_s = match (start.at, end.at) {
            (Some(a), Some(b)) => b.duration_since(a).as_secs_f64(),
            _ => f64::NAN,
        };
        println!("  window                  : {window_s:.1}s");
        if let Some(imp) = &end.encoder_impl {
            println!("  encoder implementation  : {imp}");
        }
        if let Some(q) = &end.quality_limitation {
            println!("  quality_limitation      : {q}");
        }

        let d_enc = end.frames_encoded.saturating_sub(start.frames_encoded) as u64;
        let d_dec = end.frames_decoded.saturating_sub(start.frames_decoded) as u64;
        let d_dropped = end.frames_dropped.saturating_sub(start.frames_dropped) as u64;
        let d_pkts = end.packets_sent.saturating_sub(start.packets_sent);
        let d_jb = end
            .jitter_buffer_emitted_count
            .saturating_sub(start.jitter_buffer_emitted_count);
        let d_asm = end
            .frames_assembled_from_multiple_packets
            .saturating_sub(start.frames_assembled_from_multiple_packets);
        let d_pushed = pushed_end.saturating_sub(pushed_start);
        let d_overwrites = overwrites_end.saturating_sub(overwrites_start);
        let d_encode_s = end.total_encode_time - start.total_encode_time;

        println!(
            "  encode      (ms/layer-frame, summed over simulcast layers) : {}",
            fmt_ms(per_unit_ms(d_encode_s, d_enc))
        );
        println!(
            "  packet send (ms/packet, pacer hold)                        : {}",
            fmt_ms(per_unit_ms(
                end.total_packet_send_delay - start.total_packet_send_delay,
                d_pkts
            ))
        );
        println!(
            "  assembly    (ms/multi-packet frame)                        : {}",
            fmt_ms(per_unit_ms(
                end.total_assembly_time - start.total_assembly_time,
                d_asm
            ))
        );
        println!(
            "  jitter buf  (ms/frame, actual)                             : {}",
            fmt_ms(per_unit_ms(
                end.jitter_buffer_delay - start.jitter_buffer_delay,
                d_jb
            ))
        );
        println!(
            "  jitter buf  (ms/frame, target)                             : {}",
            fmt_ms(per_unit_ms(
                end.jitter_buffer_target_delay - start.jitter_buffer_target_delay,
                d_jb
            ))
        );
        println!(
            "  jitter buf  (ms/frame, minimum)                            : {}",
            fmt_ms(per_unit_ms(
                end.jitter_buffer_minimum_delay - start.jitter_buffer_minimum_delay,
                d_jb
            ))
        );
        println!(
            "  decode      (ms/frame)                                     : {}",
            fmt_ms(per_unit_ms(
                end.total_decode_time - start.total_decode_time,
                d_dec
            ))
        );
        println!(
            "  processing  (ms/frame, pkt-received -> decoded)            : {}",
            fmt_ms(per_unit_ms(
                end.total_processing_delay - start.total_processing_delay,
                d_dec
            ))
        );
        println!(
            "  inter-frame (ms/frame, cadence not latency)                : {}",
            fmt_ms(per_unit_ms(
                end.total_inter_frame_delay - start.total_inter_frame_delay,
                d_dec
            ))
        );

        println!("\n=== encoder work and frame accounting (same window) ===");
        println!(
            "  source frames PUSHED to the encoder : {d_pushed}  ({:.2} fps)",
            d_pushed as f64 / window_s
        );
        println!("  capture frames OVERWRITTEN (dropped before encode) : {d_overwrites}");
        println!("  layer-frames encoded                : {d_enc}");
        if d_pushed > 0 {
            println!(
                "  layer-frames per source frame       : {:.2}",
                d_enc as f64 / d_pushed as f64
            );
        }
        println!("  frames decoded                      : {d_dec}");
        println!("  frames dropped (receiver)           : {d_dropped}");
        println!("  packets sent                        : {d_pkts}");
        if d_pushed > 0 {
            println!(
                "  ENCODE WORK per source frame        : {:.2} ms",
                d_encode_s / d_pushed as f64 * 1000.0
            );
        }
        // Utilisation measured against WALL time, not an assumed 30fps
        // budget: this is the fraction of one encoder-thread-second the
        // encoder actually consumed, and it needs no cadence assumption.
        println!(
            "  ENCODER UTILISATION (encode s / wall s) : {:.1}%",
            d_encode_s / window_s * 100.0
        );
        if d_pushed > 0 && d_enc > 0 {
            let delivered = d_dec as f64 / d_pushed as f64;
            println!(
                "  DELIVERY RATIO (decoded / pushed)   : {delivered:.2}  \
                 (1.00 = every captured frame reached the receiver)"
            );
        }

        if inject_delay_ms > 0 {
            println!(
                "\n  CONTROL EXPECTATION: steady-state latency should exceed an uninjected run \
                 by ~{inject_delay_ms}ms. If it does not, the instrument is blind."
            );
        }
    }

    pub fn arg_u64(args: &[String], flag: &str, default: u64) -> u64 {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() {
    use probe::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    env_logger::init();
    // Load apps/desktop/.env without ever printing its contents.
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));

    let args: Vec<String> = std::env::args().skip(1).collect();
    // Skip values that belong to a preceding flag, so `--seconds 20` never
    // reads as a positional window id.
    let flag_values: Vec<String> = ["--inject-delay-ms", "--seconds", "--steady-after-ms"]
        .iter()
        .filter_map(|f| {
            args.iter()
                .position(|a| a == f)
                .and_then(|i| args.get(i + 1))
                .cloned()
        })
        .collect();
    let positional: Vec<&String> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .filter(|a| !flag_values.contains(a))
        .collect();

    let window_id: Option<u32> = positional.first().and_then(|s| s.parse().ok());
    let room_name = positional
        .get(1)
        .map(|s| s.to_string())
        .unwrap_or_else(|| "petal-613-latency".to_string());

    let pin_lowest = args.iter().any(|a| a == "--pin-lowest");
    let inject_delay_ms = arg_u64(&args, "--inject-delay-ms", 0);
    let seconds = arg_u64(&args, "--seconds", 30);
    let steady_after_ms = arg_u64(&args, "--steady-after-ms", 8_000);

    if !desktop_lib::window_source::has_screen_recording_access() {
        eprintln!("BLOCKED: Screen Recording permission not granted to this binary.");
        std::process::exit(1);
    }

    let windows = desktop_lib::window_source::list().unwrap_or_else(|e| {
        eprintln!("Failed to enumerate windows: {e}");
        std::process::exit(1);
    });

    let target = match window_id {
        Some(id) => windows.iter().find(|w| w.window_id == id).cloned(),
        None => windows.first().cloned(),
    };
    let Some(target) = target else {
        eprintln!("No matching window found. Available:");
        for w in &windows {
            eprintln!("  {} - {}", w.window_id, w.app_name);
        }
        std::process::exit(1);
    };

    println!(
        "Publishing window {} ({} - {:?}) into room '{}'",
        target.window_id, target.app_name, target.title, room_name
    );

    let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let token = desktop_lib::transport::mint_access_token("petal-613-pub", &room_name, true, false)
        .unwrap_or_else(|e| {
            eprintln!("Failed to mint access token: {e}");
            std::process::exit(1);
        });

    // First captured frame tells us the real (window-backing-store) size;
    // we can't publish a track before knowing this, so wait for frame #1
    // synchronously before connecting to LiveKit.
    let (size_tx, size_rx) = std::sync::mpsc::channel::<(u32, u32)>();
    let first_frame_sent = Arc::new(AtomicBool::new(false));

    // #613: LATEST-WINS SLOT, not a queue.
    //
    // An earlier revision used a `tokio::sync::mpsc::unbounded_channel`,
    // whose comment claimed it would "drop the frame rather than block" when
    // the receiver fell behind -- but an UNBOUNDED channel is never full, so
    // nothing was ever dropped and slow consumption showed up as an
    // ever-growing backlog of stale frames instead. Each frame still carried
    // its own capture timestamp, so the backlog was billed to latency:
    // measured p50 143.6ms against 21.3ms for a synthetic source on the
    // identical SFU/encoder, i.e. ~122ms of pure probe-side queueing
    // masquerading as pipeline cost.
    //
    // `session/share.rs` (the REAL share path) has never worked this way: it
    // keeps a single `latest_frame` slot behind a mutex and counts overwrites
    // in `latest_frame_overwrites`. Mirroring that here is what makes this
    // probe's latency number comparable to the product's.
    let latest_frame: Arc<Mutex<Option<(desktop_lib::capture::CapturedFrame, u64)>>> =
        Arc::new(Mutex::new(None));
    let latest_frame_notify = Arc::new(tokio::sync::Notify::new());
    let overwrites = Arc::new(AtomicU64::new(0));
    let pushed = Arc::new(AtomicU64::new(0));

    let latest_frame_cb = latest_frame.clone();
    let notify_cb = latest_frame_notify.clone();
    let overwrites_cb = overwrites.clone();
    let first_frame_sent_cb = first_frame_sent.clone();
    let capture = desktop_lib::capture::WindowCapture::start(target.window_id, move |frame| {
        let capture_wall_time_us = now_us();
        if !first_frame_sent_cb.swap(true, Ordering::SeqCst) {
            let _ = size_tx.send((frame.width, frame.height));
        }
        if let Ok(mut slot) = latest_frame_cb.lock() {
            if slot.replace((frame, capture_wall_time_us)).is_some() {
                overwrites_cb.fetch_add(1, Ordering::Relaxed);
            }
        }
        notify_cb.notify_one();
    });
    let capture = capture.unwrap_or_else(|e| {
        eprintln!("Failed to start capture: {e}");
        std::process::exit(1);
    });

    let (width, height) = size_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap_or_else(|_| {
            eprintln!("Timed out waiting for first captured frame.");
            std::process::exit(1);
        });
    println!("First frame received: {width}x{height}. Connecting to LiveKit...");

    // AS COMPUTED, not as intended, and at the REAL capture size. This is the
    // same string `publish_window_track` logs, resolved from PETAL_SHARE_LADDER
    // here. Never trust the env var you set -- read this line.
    let ladder_rungs = desktop_lib::transport::publisher::full_share_ladder_rungs(width, height);
    println!(
        "  LADDER  : {}",
        desktop_lib::transport::publisher::full_share_ladder_description(width, height)
    );
    println!("  source  : {width}x{height} (real ScreenCaptureKit capture)");
    if inject_delay_ms > 0 {
        println!("  CONTROL : injecting {inject_delay_ms}ms between capture stamp and push_frame");
    }

    // ---- subscriber peer, BEFORE publishing so it sees the subscription ----
    let stop = Arc::new(AtomicBool::new(false));
    let observations: Arc<Mutex<Vec<FrameObservation>>> = Arc::new(Mutex::new(Vec::new()));
    let remote_track: Arc<Mutex<Option<livekit::track::RemoteVideoTrack>>> =
        Arc::new(Mutex::new(None));
    let sub_room = spawn_subscriber(
        &url,
        &room_name,
        pin_lowest,
        stop.clone(),
        observations.clone(),
        remote_track.clone(),
    )
    .await;

    // Publish through the SAME call the product uses, with the real window id
    // travelling in the track name (`connect_and_publish` omits the id and
    // names the track `petal-window-capture`, which is not what a real share
    // looks like on the wire).
    let room_connection = desktop_lib::transport::RoomConnection::connect(&url, &token)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to connect: {e}");
            std::process::exit(1);
        });
    room_connection.discard_compositor_events();
    let published_track = room_connection
        .publish_window_at(
            width,
            height,
            desktop_lib::transport::publisher::ShareQuality::Full,
            Some(target.window_id),
        )
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to publish: {e}");
            std::process::exit(1);
        });

    println!(
        "Published. {}ms ramp, then a {seconds}s measured window...",
        steady_after_ms
    );

    let published_track = Arc::new(published_track);
    let pub_for_loop = published_track.clone();
    let pushed_loop = pushed.clone();
    let pump = tokio::spawn(async move {
        loop {
            latest_frame_notify.notified().await;
            let Some((frame, ts)) = latest_frame.lock().ok().and_then(|mut s| s.take()) else {
                continue;
            };
            // POSITIVE CONTROL: withhold the frame from the pipeline for
            // `inject_delay_ms` AFTER its capture stamp, so measured latency
            // must rise by that amount. A run that does not move is an
            // instrument that cannot see latency at all.
            //
            // It is a DELAY LINE, not a sleep in the pump loop. Sleeping here
            // serialises the pump: a 60ms sleep caps throughput at ~16fps, the
            // cadence collapse grows the receiver jitter buffer on its own, and
            // the measured shift comes out at ~200ms for a 60ms injection --
            // right direction, wrong magnitude, and for a reason that has
            // nothing to do with the injection. Each frame gets its own timer
            // instead, so throughput is untouched and the shift is the
            // injection alone. Equal delays preserve push order.
            if inject_delay_ms > 0 {
                let t = pub_for_loop.clone();
                let c = pushed_loop.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(inject_delay_ms)).await;
                    if t.push_frame(&frame, ts).is_some() {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                });
                continue;
            }
            if pub_for_loop.push_frame(&frame, ts).is_some() {
                pushed_loop.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    // Sample the stage counters at the steady-state boundary and again at end
    // of run, so every per-stage number is a delta over the same window the
    // steady latency percentiles are taken from (aligned to within the
    // publish->subscribe handshake, a fraction of the 8s default ramp).
    tokio::time::sleep(Duration::from_millis(steady_after_ms)).await;
    let mut stats_start = StageCounters {
        at: Some(std::time::Instant::now()),
        ..Default::default()
    };
    sample_outbound(&published_track.track(), &mut stats_start).await;
    if let Some(rt) = remote_track.lock().unwrap().clone() {
        sample_inbound(&rt, &mut stats_start).await;
    }
    let pushed_start = pushed.load(Ordering::Relaxed);
    let overwrites_start = overwrites.load(Ordering::Relaxed);

    tokio::time::sleep(Duration::from_secs(seconds)).await;

    let mut stats_end = StageCounters {
        at: Some(std::time::Instant::now()),
        ..Default::default()
    };
    sample_outbound(&published_track.track(), &mut stats_end).await;
    if let Some(rt) = remote_track.lock().unwrap().clone() {
        sample_inbound(&rt, &mut stats_end).await;
    }
    let pushed_end = pushed.load(Ordering::Relaxed);
    let overwrites_end = overwrites.load(Ordering::Relaxed);
    stop.store(true, Ordering::Relaxed);

    let obs = observations.lock().unwrap().clone();
    report(
        &obs,
        &ladder_rungs,
        steady_after_ms as u128,
        inject_delay_ms,
        pin_lowest,
        &stats_start,
        &stats_end,
        pushed_start,
        pushed_end,
        overwrites_start,
        overwrites_end,
    );

    let _ = capture.stop();
    pump.abort();
    sub_room.close().await.ok();
    println!("RoomConnection done.");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("share_latency_probe is macOS-only.");
    std::process::exit(1);
}
