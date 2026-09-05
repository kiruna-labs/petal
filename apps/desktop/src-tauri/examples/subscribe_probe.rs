//! Production-path receiver probe for a paired `publish_probe` run.
//!
//! Usage: `cargo run --example subscribe_probe -- [room_name] [FLAGS]`
//!
//! Flags:
//!   `--seconds N`                 seconds measured after the first frame (default 30)
//!   `--steady-after-ms N`         discard this startup interval (default 8000)
//!   `--first-frame-timeout-ms N`  bound the wait for a publisher (default 15000)
//!   `--measurement-window-file P` use absolute epoch-us start/end boundaries from P
//!
//! `PETAL_PROBE_DUMP=/path.csv` writes every timestamp observation. The probe
//! deliberately connects through `transport::Subscriber`, so receiver settings
//! such as `PETAL_PLAYOUT_DELAY_MS` exercise the same path as product code.

use livekit::prelude::RemoteTrack;
use livekit::webrtc::stats::RtcStats;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
struct Observation {
    at_ms: u128,
    width: u32,
    height: u32,
    frame_id: Option<u32>,
    capture_us: Option<u64>,
    receive_us: u64,
    latency_us: Option<i64>,
}

#[derive(Clone, Debug, Default)]
struct InboundCounters {
    frames_decoded: u32,
    frames_dropped: u32,
    total_decode_time: f64,
    total_processing_delay: f64,
    total_assembly_time: f64,
    frames_assembled_from_multiple_packets: u64,
    jitter_buffer_delay: f64,
    jitter_buffer_target_delay: f64,
    jitter_buffer_minimum_delay: f64,
    jitter_buffer_emitted_count: u64,
    total_inter_frame_delay: f64,
}

#[derive(Debug)]
struct Args {
    room_name: String,
    seconds: u64,
    steady_after_ms: u64,
    first_frame_timeout_ms: u64,
    measurement_window_file: Option<String>,
}

fn flag_u64(args: &[String], name: &str, default: u64) -> u64 {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .and_then(|pair| pair[1].parse().ok())
        .unwrap_or(default)
}

fn flag_string(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value_flags = [
        "--seconds",
        "--steady-after-ms",
        "--first-frame-timeout-ms",
        "--measurement-window-file",
    ];
    let mut skip_next = false;
    let mut positional = Vec::new();
    for arg in &args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if value_flags.contains(&arg.as_str()) {
            skip_next = true;
        } else if !arg.starts_with("--") {
            positional.push(arg.clone());
        }
    }
    let seconds = flag_u64(&args, "--seconds", 30);
    let steady_after_ms = flag_u64(&args, "--steady-after-ms", 8_000);
    if seconds.saturating_mul(1_000) <= steady_after_ms {
        eprintln!("--seconds must exceed --steady-after-ms");
        std::process::exit(2);
    }
    Args {
        room_name: positional
            .first()
            .cloned()
            .unwrap_or_else(|| "petal-m0-spike".to_string()),
        seconds,
        steady_after_ms,
        first_frame_timeout_ms: flag_u64(&args, "--first-frame-timeout-ms", 15_000),
        measurement_window_file: flag_string(&args, "--measurement-window-file"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MeasurementWindow {
    start_us: u64,
    end_us: u64,
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn parse_measurement_window(contents: &str) -> Result<MeasurementWindow, String> {
    let mut values = contents.split_whitespace();
    let start_us = values
        .next()
        .ok_or_else(|| "missing measurement start".to_string())?
        .parse::<u64>()
        .map_err(|error| format!("invalid measurement start: {error}"))?;
    let end_us = values
        .next()
        .ok_or_else(|| "missing measurement end".to_string())?
        .parse::<u64>()
        .map_err(|error| format!("invalid measurement end: {error}"))?;
    if values.next().is_some() {
        return Err("measurement window must contain exactly two epoch-us values".to_string());
    }
    if end_us <= start_us {
        return Err("measurement end must be after start".to_string());
    }
    Ok(MeasurementWindow { start_us, end_us })
}

fn first_frame_deadline_epoch_us(
    wait_started_us: u64,
    configured_timeout_ms: u64,
    aligned_window: Option<MeasurementWindow>,
) -> u64 {
    aligned_window
        .map(|window| window.start_us)
        .unwrap_or_else(|| wait_started_us + configured_timeout_ms * 1_000)
}

async fn wait_for_measurement_window(
    path: &str,
    timeout: Duration,
) -> Result<MeasurementWindow, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match std::fs::read_to_string(path) {
            Ok(contents) => return parse_measurement_window(&contents),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(format!("could not read measurement window {path}: {error}")),
        }
    }
}

async fn sleep_until_epoch_us(epoch_us: u64) {
    let remaining_us = epoch_us.saturating_sub(now_us());
    if remaining_us > 0 {
        tokio::time::sleep(Duration::from_micros(remaining_us)).await;
    }
}

fn subscribed_video_track(room: &livekit::Room) -> Option<RemoteTrack> {
    room.remote_participants().values().find_map(|participant| {
        participant
            .track_publications()
            .values()
            .filter_map(|publication| publication.track())
            .find(|track| matches!(track, RemoteTrack::Video(_)))
    })
}

async fn sample_inbound(track: &RemoteTrack) -> Result<InboundCounters, String> {
    let stats = track
        .get_stats()
        .await
        .map_err(|error| format!("get_stats failed: {error}"))?;
    let mut out = InboundCounters::default();
    for stat in &stats {
        if let RtcStats::InboundRtp(inbound) = stat {
            out.frames_decoded += inbound.inbound.frames_decoded;
            out.frames_dropped += inbound.inbound.frames_dropped;
            out.total_decode_time += inbound.inbound.total_decode_time;
            out.total_processing_delay += inbound.inbound.total_processing_delay;
            out.total_assembly_time += inbound.inbound.total_assembly_time;
            out.frames_assembled_from_multiple_packets +=
                inbound.inbound.frames_assembled_from_multiple_packets;
            out.jitter_buffer_delay += inbound.inbound.jitter_buffer_delay;
            out.jitter_buffer_target_delay += inbound.inbound.jitter_buffer_target_delay;
            out.jitter_buffer_minimum_delay += inbound.inbound.jitter_buffer_minimum_delay;
            out.jitter_buffer_emitted_count += inbound.inbound.jitter_buffer_emitted_count;
            out.total_inter_frame_delay += inbound.inbound.total_inter_frame_delay;
        }
    }
    Ok(out)
}

fn per_unit_ms(delta_seconds: f64, count: u64) -> Option<f64> {
    (count > 0).then(|| delta_seconds / count as f64 * 1_000.0)
}

fn percentile(sorted: &[i64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    Some(sorted[index] as f64 / 1_000.0)
}

fn in_measurement_window(observation: &Observation, window: MeasurementWindow) -> bool {
    // The aligned arm is selected on this receiver's decoded-callback wall
    // clock. `capture_us` is embedded sender/source wall time; it reports
    // source age for the capture->decode lower bound but never decides whether
    // a decoded callback belongs to the 16-28s receiver evidence window.
    observation.receive_us >= window.start_us && observation.receive_us < window.end_us
}

fn end_to_end_publisher_frame_gaps(observations: &[Observation], window: MeasurementWindow) -> u64 {
    let mut previous = None;
    let mut gaps = 0u64;
    for frame_id in observations
        .iter()
        .filter(|observation| in_measurement_window(observation, window))
        .filter_map(|observation| observation.frame_id)
    {
        if let Some(prior) = previous {
            gaps += u64::from(frame_id.saturating_sub(prior).saturating_sub(1));
        }
        previous = Some(frame_id);
    }
    gaps
}

fn write_dump(
    path: &str,
    observations: &[Observation],
    window: MeasurementWindow,
) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    writeln!(
        file,
        "at_ms,in_measurement_window,width,height,frame_id,capture_us,receive_us,latency_us"
    )
    .map_err(|error| error.to_string())?;
    for observation in observations {
        writeln!(
            file,
            "{},{},{},{},{},{},{},{}",
            observation.at_ms,
            in_measurement_window(observation, window),
            observation.width,
            observation.height,
            observation
                .frame_id
                .map(|value| value.to_string())
                .unwrap_or_default(),
            observation
                .capture_us
                .map(|value| value.to_string())
                .unwrap_or_default(),
            observation.receive_us,
            observation
                .latency_us
                .map(|value| value.to_string())
                .unwrap_or_default(),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_the_same_rounded_index_as_raw_dump_recomputation() {
        let values = [10_000, 20_000, 30_000, 40_000, 50_000];
        assert_eq!(percentile(&values, 0.50), Some(30.0));
        assert_eq!(percentile(&values, 0.95), Some(50.0));
    }

    #[test]
    fn per_unit_ms_rejects_zero_denominator() {
        assert_eq!(per_unit_ms(1.0, 0), None);
        assert_eq!(per_unit_ms(1.0, 10), Some(100.0));
    }

    #[test]
    fn numeric_flags_use_defaults_for_missing_or_invalid_values() {
        let args = vec!["--seconds".to_string(), "29".to_string()];
        assert_eq!(flag_u64(&args, "--seconds", 30), 29);
        assert_eq!(flag_u64(&args, "--steady-after-ms", 8_000), 8_000);
        let invalid = vec!["--seconds".to_string(), "nope".to_string()];
        assert_eq!(flag_u64(&invalid, "--seconds", 30), 30);
    }

    #[test]
    fn publisher_frame_gap_count_uses_only_the_aligned_epoch_window() {
        let observation = |receive_us, frame_id| Observation {
            at_ms: 0,
            width: 1,
            height: 1,
            frame_id: Some(frame_id),
            capture_us: None,
            receive_us,
            latency_us: None,
        };
        let observations = [
            observation(999, 1),
            observation(1_000, 20),
            observation(1_033, 21),
            observation(1_066, 23),
            observation(2_000, 40),
        ];
        let window = MeasurementWindow {
            start_us: 1_000,
            end_us: 2_000,
        };
        assert_eq!(end_to_end_publisher_frame_gaps(&observations, window), 1);
    }

    #[test]
    fn aligned_window_requires_exactly_two_ordered_epoch_values() {
        assert_eq!(
            parse_measurement_window("1000 2000\n"),
            Ok(MeasurementWindow {
                start_us: 1_000,
                end_us: 2_000
            })
        );
        assert!(parse_measurement_window("2000 1000").is_err());
        assert!(parse_measurement_window("1000 2000 extra").is_err());
    }

    #[test]
    fn aligned_first_frame_deadline_cannot_precede_measurement_start() {
        let window = MeasurementWindow {
            start_us: 30_000_000,
            end_us: 42_000_000,
        };
        assert_eq!(
            first_frame_deadline_epoch_us(1_000_000, 15_000, Some(window)),
            30_000_000
        );
        assert_eq!(
            first_frame_deadline_epoch_us(1_000_000, 15_000, None),
            16_000_000
        );
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
    let args = parse_args();

    let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(1);
    });
    let token =
        desktop_lib::transport::mint_access_token("petal-subscriber", &args.room_name, false, true)
            .unwrap_or_else(|error| {
                eprintln!("Failed to mint access token: {error}");
                std::process::exit(1);
            });

    let observations = Arc::new(Mutex::new(Vec::<Observation>::new()));
    let first_frame_at = Arc::new(Mutex::new(None::<Instant>));

    let observations_cb = observations.clone();
    let first_frame_cb = first_frame_at.clone();
    let subscriber = desktop_lib::transport::Subscriber::connect(&url, &token, move |frame| {
        let mut first = first_frame_cb.lock().unwrap();
        let started = *first.get_or_insert_with(Instant::now);
        drop(first);

        observations_cb.lock().unwrap().push(Observation {
            at_ms: started.elapsed().as_millis(),
            width: frame.width,
            height: frame.height,
            frame_id: frame.frame_id,
            capture_us: frame.capture_timestamp_us,
            receive_us: frame.receive_timestamp_us,
            latency_us: frame
                .capture_timestamp_us
                .map(|capture| frame.receive_timestamp_us as i64 - capture as i64),
        });
    })
    .await
    .unwrap_or_else(|error| {
        eprintln!("Failed to connect: {error}");
        std::process::exit(1);
    });
    println!("PROBE_SUBSCRIBER_CONNECTED room={}", args.room_name);

    let aligned_window = if let Some(path) = args.measurement_window_file.as_deref() {
        Some(
            wait_for_measurement_window(path, Duration::from_secs(20))
                .await
                .unwrap_or_else(|error| {
                    eprintln!("FAILED: {error}");
                    std::process::exit(1);
                }),
        )
    } else {
        None
    };
    let first_deadline_us =
        first_frame_deadline_epoch_us(now_us(), args.first_frame_timeout_ms, aligned_window);
    while first_frame_at.lock().unwrap().is_none() && now_us() < first_deadline_us {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if first_frame_at.lock().unwrap().is_none() {
        if aligned_window.is_some() {
            eprintln!("FAILED: no first frame before aligned measurement start");
        } else {
            eprintln!(
                "FAILED: no first frame within {} ms",
                args.first_frame_timeout_ms
            );
        }
        std::process::exit(1);
    }
    println!("PROBE_FIRST_FRAME");

    let window = if let Some(window) = aligned_window {
        if now_us() >= window.start_us {
            eprintln!(
                "FAILED: aligned measurement boundary was already reached (now={} start={})",
                now_us(),
                window.start_us
            );
            std::process::exit(1);
        }
        sleep_until_epoch_us(window.start_us).await;
        window
    } else {
        tokio::time::sleep(Duration::from_millis(args.steady_after_ms)).await;
        let start_us = now_us();
        MeasurementWindow {
            start_us,
            end_us: start_us + (args.seconds * 1_000 - args.steady_after_ms) * 1_000,
        }
    };
    let track = subscribed_video_track(&subscriber.room).unwrap_or_else(|| {
        eprintln!("FAILED: decoded frames arrived but no subscribed video track is discoverable");
        std::process::exit(1);
    });
    let start = sample_inbound(&track).await.unwrap_or_else(|error| {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    });
    let measured_started = Instant::now();
    sleep_until_epoch_us(window.end_us).await;
    let measured_seconds = measured_started.elapsed().as_secs_f64();
    let end = sample_inbound(&track).await.unwrap_or_else(|error| {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    });

    let observations = observations.lock().unwrap().clone();
    if let Ok(path) = std::env::var("PETAL_PROBE_DUMP") {
        write_dump(&path, &observations, window).unwrap_or_else(|error| {
            eprintln!("FAILED: could not write {path}: {error}");
            std::process::exit(1);
        });
        println!("PROBE_DUMP path={path} rows={}", observations.len());
    }

    let mut measurement_latencies: Vec<i64> = observations
        .iter()
        .filter(|observation| in_measurement_window(observation, window))
        .filter_map(|observation| observation.latency_us)
        .collect();
    measurement_latencies.sort_unstable();
    let missing_timestamps = observations
        .iter()
        .filter(|observation| in_measurement_window(observation, window))
        .filter(|observation| observation.latency_us.is_none())
        .count();
    let decoded = end.frames_decoded.saturating_sub(start.frames_decoded) as u64;
    let dropped = end.frames_dropped.saturating_sub(start.frames_dropped) as u64;
    let emitted = end
        .jitter_buffer_emitted_count
        .saturating_sub(start.jitter_buffer_emitted_count);
    let assembled = end
        .frames_assembled_from_multiple_packets
        .saturating_sub(start.frames_assembled_from_multiple_packets);
    let decoded_fps = decoded as f64 / measured_seconds;
    let jitter_actual_ms =
        per_unit_ms(end.jitter_buffer_delay - start.jitter_buffer_delay, emitted);
    let jitter_target_ms = per_unit_ms(
        end.jitter_buffer_target_delay - start.jitter_buffer_target_delay,
        emitted,
    );
    let jitter_minimum_ms = per_unit_ms(
        end.jitter_buffer_minimum_delay - start.jitter_buffer_minimum_delay,
        emitted,
    );
    let p50_ms = percentile(&measurement_latencies, 0.50);
    let p95_ms = percentile(&measurement_latencies, 0.95);
    let publisher_frame_gaps = end_to_end_publisher_frame_gaps(&observations, window);

    println!("\n=== #613 production-path receiver delta ===");
    println!(
        "measurement_samples={} missing_timestamps={missing_timestamps}",
        measurement_latencies.len()
    );
    println!(
        "decoded_fps={decoded_fps:.2} decoded={decoded} dropped={dropped} gaps={}",
        publisher_frame_gaps
    );
    println!("jitter_actual_ms={jitter_actual_ms:?} jitter_target_ms={jitter_target_ms:?} jitter_minimum_ms={jitter_minimum_ms:?}");
    println!(
        "capture_callback_to_decoded_callback_lower_bound_p50_ms={p50_ms:?} p95_ms={p95_ms:?}"
    );
    println!(
        "decode_ms={:?} processing_ms={:?} assembly_ms={:?} inter_frame_ms={:?}",
        per_unit_ms(end.total_decode_time - start.total_decode_time, decoded),
        per_unit_ms(
            end.total_processing_delay - start.total_processing_delay,
            decoded
        ),
        per_unit_ms(
            end.total_assembly_time - start.total_assembly_time,
            assembled
        ),
        per_unit_ms(
            end.total_inter_frame_delay - start.total_inter_frame_delay,
            decoded
        ),
    );

    let result = serde_json::json!({
        "status": "ok",
        "room": args.room_name,
        "measurement_n": measurement_latencies.len(),
        "missing_timestamps": missing_timestamps,
        "decoded_fps": decoded_fps,
        "decoded": decoded,
        "receiver_frames_dropped": dropped,
        "end_to_end_publisher_frame_gaps": publisher_frame_gaps,
        "jitter_actual_ms": jitter_actual_ms,
        "jitter_target_ms": jitter_target_ms,
        "jitter_minimum_ms": jitter_minimum_ms,
        "capture_callback_to_decoded_callback_p50_ms": p50_ms,
        "capture_callback_to_decoded_callback_p95_ms": p95_ms,
        "measurement_seconds": measured_seconds,
        "measurement_start_epoch_us": window.start_us,
        "measurement_end_epoch_us": window.end_us,
    });
    println!("PROBE_RESULT_JSON {result}");
}
