//! M0 spike, stage (b): capture a real window and publish it to a LiveKit
//! Cloud room as an H.264 video track (VideoToolbox HW encode requested).
//!
//! Usage: `cargo run --example publish_probe -- [window_id] [room_name] [FLAGS]`
//!
//! Flags:
//!   `--source real|synthetic`  real ScreenCaptureKit window or fixed 30fps source
//!   `--seconds N`              bounded publisher lifetime (default 30)
//!   `--fps N`                  synthetic source cadence (default 30)
//!   `--width N --height N`     synthetic raster dimensions (default 1600x900)
//!   `--measurement-window-file P` absolute epoch-us start/end boundary from P
//!   `--expected-capture-width N --expected-capture-height N`
//!       abort a real capture before publish unless frame #1 is exactly NxN
//!   `--capture-preflight-only`  verify accepted real capture delivery and
//!       exit before loading LiveKit credentials or connecting to a room
//!
//! Reads LIVEKIT_URL/LIVEKIT_API_KEY/LIVEKIT_API_SECRET from
//! `apps/desktop/.env` (via `dotenvy`) -- never logs their values.
//!
//! Pairs with `subscribe_probe`, run as a second process on the same
//! machine joining the same room, per the M0 task's single-Mac dual-role
//! test design (no second Mac available; validates the real LiveKit Cloud
//! network path, just not a second physical machine).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceMode {
    Real,
    Synthetic,
}

const PRESENTATION_DELAY_QUEUE_CAPACITY: usize = 16;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScheduledDelayFrame {
    sequence: u64,
    due_us: u64,
}

#[cfg(test)]
#[derive(Debug)]
struct PresentationDelaySchedule {
    capacity: usize,
    queued: std::collections::VecDeque<ScheduledDelayFrame>,
    overflowed: bool,
}

#[cfg(test)]
impl PresentationDelaySchedule {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queued: std::collections::VecDeque::new(),
            overflowed: false,
        }
    }

    fn enqueue(&mut self, sequence: u64, captured_us: u64, delay_us: u64) -> Result<(), ()> {
        if self.queued.len() >= self.capacity {
            self.overflowed = true;
            return Err(());
        }
        self.queued.push_back(ScheduledDelayFrame {
            sequence,
            due_us: captured_us + delay_us,
        });
        Ok(())
    }

    fn pop_due(&mut self, now_us: u64) -> Option<ScheduledDelayFrame> {
        (self.queued.front()?.due_us <= now_us)
            .then(|| self.queued.pop_front())
            .flatten()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    window_id: Option<u32>,
    room_name: String,
    source: SourceMode,
    seconds: u64,
    fps: u32,
    width: u32,
    height: u32,
    expected_capture_size: Option<(u32, u32)>,
    measurement_window_file: Option<String>,
    /// #613 control only: hold an already captured/stamped frame immediately
    /// before this example pushes it to LiveKit.  Production publishing does
    /// not read this flag.
    presentation_delay_ms: u64,
    /// #613 apparatus-only: exercise the real direct-window capture path and
    /// stop after its first accepted frame (or a bounded diagnostic failure).
    capture_preflight_only: bool,
}

fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].as_str())
}

fn flag_u64(args: &[String], name: &str, default: u64) -> Result<u64, String> {
    flag_value(args, name)
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|error| format!("invalid {name}: {error}"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn parse_args_from(args: &[String]) -> Result<Args, String> {
    let value_flags = [
        "--source",
        "--seconds",
        "--fps",
        "--width",
        "--height",
        "--expected-capture-width",
        "--expected-capture-height",
        "--measurement-window-file",
        "--presentation-delay-ms",
    ];
    let mut positional = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
        } else if value_flags.contains(&arg.as_str()) {
            skip_next = true;
        } else if !arg.starts_with("--") {
            positional.push(arg.as_str());
        }
    }
    let source = match flag_value(args, "--source").unwrap_or("real") {
        "real" => SourceMode::Real,
        "synthetic" => SourceMode::Synthetic,
        value => {
            return Err(format!(
                "invalid --source {value}; expected real or synthetic"
            ))
        }
    };
    let fps = flag_u64(args, "--fps", 30)? as u32;
    let width = flag_u64(args, "--width", 1_600)? as u32;
    let height = flag_u64(args, "--height", 900)? as u32;
    if fps == 0 || width == 0 || height == 0 {
        return Err("--fps, --width, and --height must be positive".to_string());
    }
    let expected_capture_width = flag_value(args, "--expected-capture-width")
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|error| format!("invalid --expected-capture-width: {error}"))
        })
        .transpose()?;
    let expected_capture_height = flag_value(args, "--expected-capture-height")
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|error| format!("invalid --expected-capture-height: {error}"))
        })
        .transpose()?;
    let expected_capture_size = match (expected_capture_width, expected_capture_height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Some((width, height)),
        (None, None) => None,
        _ => {
            return Err(
                "--expected-capture-width and --expected-capture-height must be positive and supplied together"
                    .to_string(),
            )
        }
    };
    Ok(Args {
        window_id: positional.first().and_then(|value| value.parse().ok()),
        room_name: positional
            .get(1)
            .map(|value| (*value).to_string())
            .unwrap_or_else(|| "petal-m0-spike".to_string()),
        source,
        seconds: flag_u64(args, "--seconds", 30)?,
        fps,
        width,
        height,
        expected_capture_size,
        measurement_window_file: flag_value(args, "--measurement-window-file").map(str::to_string),
        presentation_delay_ms: flag_u64(args, "--presentation-delay-ms", 0)?,
        capture_preflight_only: args.iter().any(|arg| arg == "--capture-preflight-only"),
    })
}

fn capture_preflight_reason(
    snapshot: &desktop_lib::capture::CaptureDiagnosticsSnapshot,
) -> &'static str {
    if snapshot.accepted_frames > 0 {
        "accepted-frame"
    } else if snapshot.stream_errors > 0 {
        "stream-error"
    } else if snapshot.layout_rejections > 0 {
        "layout-rejection"
    } else if snapshot.pixel_format_rejections > 0 {
        "pixel-format-rejection"
    } else if snapshot.no_buffer_frames > 0 {
        "no-image-buffer"
    } else {
        "no-sck-output"
    }
}

fn print_capture_preflight_result(
    status: &str,
    window_id: u32,
    frame: Option<(u32, u32)>,
    snapshot: desktop_lib::capture::CaptureDiagnosticsSnapshot,
) {
    // One JSON line makes the preflight falsifiable without retaining pixels.
    println!(
        "CAPTURE_PREFLIGHT_RESULT {}",
        serde_json::json!({
            "status": status,
            "reason": capture_preflight_reason(&snapshot),
            "window_id": window_id,
            "frame_width": frame.map(|value| value.0),
            "frame_height": frame.map(|value| value.1),
            "accepted_frames": snapshot.accepted_frames,
            "no_buffer_frames": snapshot.no_buffer_frames,
            "layout_rejections": snapshot.layout_rejections,
            "pixel_format_rejections": snapshot.pixel_format_rejections,
            "last_pixel_format": snapshot.last_pixel_format.map(|format| format!("0x{format:08x}")),
            "stream_errors": snapshot.stream_errors,
            "last_stream_error": snapshot.last_stream_error,
        })
    );
}

fn verify_capture_raster(actual: (u32, u32), expected: Option<(u32, u32)>) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "capture raster mismatch: expected {}x{}, got {}x{}",
            expected.0, expected.1, actual.0, actual.1
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MeasurementWindow {
    start_us: u64,
    end_us: u64,
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
    if values.next().is_some() || end_us <= start_us {
        return Err("measurement window must contain two ordered epoch-us values".to_string());
    }
    Ok(MeasurementWindow { start_us, end_us })
}

async fn wait_for_measurement_window(
    path: &str,
    timeout: std::time::Duration,
) -> Result<MeasurementWindow, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match std::fs::read_to_string(path) {
            Ok(contents) => return parse_measurement_window(&contents),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(error) => return Err(format!("could not read measurement window {path}: {error}")),
        }
    }
}

async fn sleep_until_epoch_us(epoch_us: u64) {
    let remaining = epoch_us.saturating_sub(now_us());
    if remaining > 0 {
        tokio::time::sleep(std::time::Duration::from_micros(remaining)).await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CounterSnapshot {
    at_us: u64,
    pushed_frames: u64,
    capture_slot_overwrites: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CounterDelta {
    seconds: f64,
    pushed_frames: u64,
    capture_slot_overwrites: u64,
    pushed_fps: f64,
    overwrite_ratio: f64,
}

fn counter_delta(start: CounterSnapshot, end: CounterSnapshot) -> Result<CounterDelta, String> {
    if end.at_us <= start.at_us
        || end.pushed_frames < start.pushed_frames
        || end.capture_slot_overwrites < start.capture_slot_overwrites
    {
        return Err("publisher counters are not monotonic".to_string());
    }
    let seconds = (end.at_us - start.at_us) as f64 / 1_000_000.0;
    let pushed_frames = end.pushed_frames - start.pushed_frames;
    let capture_slot_overwrites = end.capture_slot_overwrites - start.capture_slot_overwrites;
    let pushed_fps = pushed_frames as f64 / seconds;
    let overwrite_ratio = if pushed_frames == 0 {
        f64::INFINITY
    } else {
        capture_slot_overwrites as f64 / pushed_frames as f64
    };
    Ok(CounterDelta {
        seconds,
        pushed_frames,
        capture_slot_overwrites,
        pushed_fps,
        overwrite_ratio,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct OutboundVideoEncodingSnapshot {
    stats_id: String,
    ssrc: u32,
    rid: String,
    active: bool,
    frame_width: u32,
    frame_height: u32,
    frames_encoded: u32,
    packets_sent: u64,
    quality_limitation: String,
}

#[derive(Clone, Debug, Default)]
struct OutboundSnapshot {
    total_encode_time: f64,
    frames_encoded: u32,
    total_packet_send_delay: f64,
    packets_sent: u64,
    video_encodings: Vec<OutboundVideoEncodingSnapshot>,
}

fn quality_limitation_blocks_arm(reason: &str) -> bool {
    matches!(reason, "Cpu" | "Bandwidth")
}

fn outbound_quality_gate(start: &OutboundSnapshot, end: &OutboundSnapshot) -> bool {
    !start.video_encodings.is_empty()
        && !end.video_encodings.is_empty()
        && start
            .video_encodings
            .iter()
            .chain(&end.video_encodings)
            .all(|encoding| {
                !encoding.quality_limitation.is_empty()
                    && !quality_limitation_blocks_arm(&encoding.quality_limitation)
            })
}

async fn sample_outbound(track: &livekit::track::LocalVideoTrack) -> OutboundSnapshot {
    let mut out = OutboundSnapshot::default();
    if let Ok(stats) = track.get_stats().await {
        for stat in &stats {
            if let livekit::webrtc::stats::RtcStats::OutboundRtp(rtp) = stat {
                if rtp.stream.kind != "video" {
                    continue;
                }
                out.total_encode_time += rtp.outbound.total_encode_time;
                out.frames_encoded += rtp.outbound.frames_encoded;
                out.total_packet_send_delay += rtp.outbound.total_packet_send_delay;
                out.packets_sent += rtp.sent.packets_sent;
                out.video_encodings.push(OutboundVideoEncodingSnapshot {
                    stats_id: rtp.rtc.id.clone(),
                    ssrc: rtp.stream.ssrc,
                    rid: rtp.outbound.rid.clone(),
                    active: rtp.outbound.active,
                    frame_width: rtp.outbound.frame_width,
                    frame_height: rtp.outbound.frame_height,
                    frames_encoded: rtp.outbound.frames_encoded,
                    packets_sent: rtp.sent.packets_sent,
                    quality_limitation: format!("{:?}", rtp.outbound.quality_limitation_reason),
                });
            }
        }
    }
    out
}

fn spawn_aligned_window_evidence(
    published_track: std::sync::Arc<desktop_lib::transport::PublishedTrack>,
    pushed_frames: std::sync::Arc<std::sync::atomic::AtomicU64>,
    capture_slot_overwrites: std::sync::Arc<std::sync::atomic::AtomicU64>,
    source: SourceMode,
    published_at: std::time::Instant,
    measurement_window_file: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let window = if let Some(path) = measurement_window_file.as_deref() {
            wait_for_measurement_window(path, std::time::Duration::from_secs(20))
                .await
                .unwrap_or_else(|error| {
                    eprintln!("FAILED: {error}");
                    std::process::exit(1);
                })
        } else {
            let start_us = now_us()
                + (published_at + std::time::Duration::from_secs(16))
                    .saturating_duration_since(std::time::Instant::now())
                    .as_micros() as u64;
            MeasurementWindow {
                start_us,
                end_us: start_us + 12_000_000,
            }
        };
        sleep_until_epoch_us(window.start_us).await;
        let start = CounterSnapshot {
            at_us: now_us(),
            pushed_frames: pushed_frames.load(std::sync::atomic::Ordering::Relaxed),
            capture_slot_overwrites: capture_slot_overwrites
                .load(std::sync::atomic::Ordering::Relaxed),
        };
        let outbound_start = sample_outbound(&published_track.track()).await;

        sleep_until_epoch_us(window.end_us).await;
        let end = CounterSnapshot {
            at_us: now_us(),
            pushed_frames: pushed_frames.load(std::sync::atomic::Ordering::Relaxed),
            capture_slot_overwrites: capture_slot_overwrites
                .load(std::sync::atomic::Ordering::Relaxed),
        };
        let outbound_end = sample_outbound(&published_track.track()).await;
        let counters = counter_delta(start, end).unwrap_or_else(|error| {
            eprintln!("FAILED: {error}");
            std::process::exit(1);
        });
        let frames_encoded = outbound_end
            .frames_encoded
            .saturating_sub(outbound_start.frames_encoded);
        let packets_sent = outbound_end
            .packets_sent
            .saturating_sub(outbound_start.packets_sent);
        let encode_ms_per_frame = (frames_encoded > 0).then(|| {
            (outbound_end.total_encode_time - outbound_start.total_encode_time)
                / frames_encoded as f64
                * 1_000.0
        });
        let packet_send_ms_per_packet = (packets_sent > 0).then(|| {
            (outbound_end.total_packet_send_delay - outbound_start.total_packet_send_delay)
                / packets_sent as f64
                * 1_000.0
        });
        let publisher_quality_limitation_valid =
            outbound_quality_gate(&outbound_start, &outbound_end);
        let source_name = match source {
            SourceMode::Real => "real",
            SourceMode::Synthetic => "synthetic",
        };
        let result = serde_json::json!({
            "source": source_name,
            "publisher_scheduled_measurement_start_epoch_us": window.start_us,
            "publisher_scheduled_measurement_end_epoch_us": window.end_us,
            "publisher_measurement_start_epoch_us": start.at_us,
            "publisher_measurement_end_epoch_us": end.at_us,
            "publisher_measurement_seconds": counters.seconds,
            "publisher_pushed_frames": counters.pushed_frames,
            "publisher_pushed_fps": counters.pushed_fps,
            "capture_slot_overwrites": counters.capture_slot_overwrites,
            "capture_overwrite_ratio": counters.overwrite_ratio,
            "frames_encoded": frames_encoded,
            "packets_sent": packets_sent,
            "encode_ms_per_frame": encode_ms_per_frame,
            "packet_send_ms_per_packet": packet_send_ms_per_packet,
            "publisher_quality_limitation_valid": publisher_quality_limitation_valid,
            "outbound_video_encoding_snapshots": {
                "start": outbound_start.video_encodings,
                "end": outbound_end.video_encodings,
            },
        });
        println!("PUBLISHER_WINDOW_JSON {result}");
    })
}

fn synthetic_frame(width: u32, height: u32, sequence: u64) -> desktop_lib::capture::CapturedFrame {
    let y_stride = width;
    let uv_stride = width;
    let phase = (sequence % 180) as u8;
    let mut y = vec![0u8; (width * height) as usize];
    for (index, value) in y.iter_mut().enumerate() {
        let row = index / width as usize;
        let column = index % width as usize;
        *value = 32u8.saturating_add(((row + column + phase as usize) % 192) as u8);
    }
    let uv = vec![128u8; (width * height / 2) as usize];
    desktop_lib::capture::CapturedFrame {
        width,
        height,
        payload: desktop_lib::capture::CapturedFramePayload::Nv12 {
            y: desktop_lib::capture::PooledFrameData::from_vec(y),
            y_stride,
            uv: desktop_lib::capture::PooledFrameData::from_vec(uv),
            uv_stride,
        },
        source_scale: 1.0,
        region_generation: None,
        layout_validated: true,
        color_profile: desktop_lib::video_color::VideoColorProfile::BT601_VIDEO,
        sequence,
        frame_status: None,
        dirty_rect_count: 1,
        dirty_area_px: u64::from(width) * u64::from(height),
        dirty_rects_known: true,
        lock_copy_ms: 0.0,
    }
}

async fn run_synthetic(url: &str, token: &str, args: &Args) -> Result<(), String> {
    println!(
        "Publishing fixed-cadence synthetic {}x{}@{} into room '{}'",
        args.width, args.height, args.fps, args.room_name
    );
    // Honor the positional window_id in synthetic mode too: without it the
    // track publishes as the id-less `petal-window-capture`, which a real
    // receiver rejects ("not a recognized Petal window share") and never
    // renders -- so a synthetic run could not exercise the receiver's
    // compositor path at all.
    let published_track = std::sync::Arc::new(
        if let Some(window_id) = args.window_id {
            let connection = desktop_lib::transport::RoomConnection::connect(url, token)
                .await
                .map_err(|error| format!("Failed to connect: {error}"))?;
            connection.discard_compositor_events();
            connection
                .publish_window_at(
                    args.width,
                    args.height,
                    desktop_lib::transport::publisher::ShareQuality::Full,
                    Some(window_id),
                )
                .await
                .map_err(|error| format!("Failed to publish: {error}"))?
        } else {
            desktop_lib::transport::RoomConnection::connect_and_publish(
                url,
                token,
                args.width,
                args.height,
            )
            .await
            .map_err(|error| format!("Failed to connect/publish: {error}"))?
        },
    );
    println!(
        "Published. Streaming synthetic frames for {} seconds...",
        args.seconds
    );
    let published_at = std::time::Instant::now();
    let pushed_frames = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let capture_slot_overwrites = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let evidence = spawn_aligned_window_evidence(
        published_track.clone(),
        pushed_frames.clone(),
        capture_slot_overwrites,
        SourceMode::Synthetic,
        published_at,
        args.measurement_window_file.clone(),
    );
    let period = std::time::Duration::from_secs_f64(1.0 / f64::from(args.fps));
    let mut cadence = tokio::time::interval_at(tokio::time::Instant::now(), period);
    cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let end_at =
        tokio::time::Instant::from_std(published_at + std::time::Duration::from_secs(args.seconds));
    let frames = [
        synthetic_frame(args.width, args.height, 1),
        synthetic_frame(args.width, args.height, 91),
    ];
    let mut sequence = 0usize;
    while tokio::time::Instant::now() < end_at {
        cadence.tick().await;
        sequence += 1;
        let frame = &frames[sequence % frames.len()];
        if published_track.push_frame(frame, now_us()).is_some() {
            pushed_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    evidence
        .await
        .map_err(|error| format!("publisher evidence task failed: {error}"))?;
    println!("RoomConnection done.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_capture_preflight(args: &Args) {
    if args.source != SourceMode::Real {
        eprintln!("--capture-preflight-only requires --source real");
        std::process::exit(2);
    }
    let Some(requested_window_id) = args.window_id else {
        eprintln!("--capture-preflight-only requires an explicit window_id");
        std::process::exit(2);
    };
    if !desktop_lib::window_source::has_screen_recording_access() {
        eprintln!("BLOCKED: Screen Recording permission not granted to this binary.");
        std::process::exit(1);
    }
    let windows = desktop_lib::window_source::list().unwrap_or_else(|error| {
        eprintln!("Failed to enumerate windows: {error}");
        std::process::exit(1);
    });
    let Some(target) = windows
        .iter()
        .find(|window| window.window_id == requested_window_id)
        .cloned()
    else {
        eprintln!("No matching window found for capture preflight: {requested_window_id}");
        std::process::exit(1);
    };

    let diagnostics = desktop_lib::capture::CaptureDiagnostics::default();
    let diagnostics_for_result = diagnostics.clone();
    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<(u32, u32)>();
    let first_frame_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first_frame_sent_cb = first_frame_sent.clone();
    let capture = desktop_lib::capture::WindowCapture::start_with_error_handler_and_diagnostics(
        target.window_id,
        30,
        move |frame| {
            if !first_frame_sent_cb.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = frame_tx.send((frame.width, frame.height));
            }
        },
        |_| {},
        diagnostics,
    )
    .unwrap_or_else(|error| {
        eprintln!("Failed to start capture preflight: {error}");
        std::process::exit(1);
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let frame = loop {
        match frame_rx.try_recv() {
            Ok(frame) => break Some(frame),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let snapshot = diagnostics_for_result.snapshot();
    let _ = capture.stop();

    let Some(frame) = frame else {
        print_capture_preflight_result("failed", target.window_id, None, snapshot);
        std::process::exit(1);
    };
    if let Err(error) = verify_capture_raster(frame, args.expected_capture_size) {
        print_capture_preflight_result("invalid-raster", target.window_id, Some(frame), snapshot);
        eprintln!("INVALID_CAPTURE_RASTER: {error}; aborting before LiveKit publish");
        std::process::exit(3);
    }
    print_capture_preflight_result("ready", target.window_id, Some(frame), snapshot);
    println!(
        "CAPTURE_PREFLIGHT_READY window_id={} frame={}x{}",
        target.window_id, frame.0, frame.1
    );
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() {
    env_logger::init();
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args_from(&raw_args).unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2);
    });

    if args.capture_preflight_only {
        run_capture_preflight(&args);
        return;
    }

    // Load apps/desktop/.env without ever printing its contents. The opt-in
    // capture preflight above deliberately exits before touching LiveKit env.
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));

    let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let token =
        desktop_lib::transport::mint_access_token("petal-publisher", &args.room_name, true, false)
            .unwrap_or_else(|e| {
                eprintln!("Failed to mint access token: {e}");
                std::process::exit(1);
            });

    if args.source == SourceMode::Synthetic {
        run_synthetic(&url, &token, &args)
            .await
            .unwrap_or_else(|error| {
                eprintln!("{error}");
                std::process::exit(1);
            });
        return;
    }

    if !desktop_lib::window_source::has_screen_recording_access() {
        eprintln!("BLOCKED: Screen Recording permission not granted to this binary.");
        std::process::exit(1);
    }

    let windows = desktop_lib::window_source::list().unwrap_or_else(|e| {
        eprintln!("Failed to enumerate windows: {e}");
        std::process::exit(1);
    });

    let target = match args.window_id {
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
        target.window_id, target.app_name, target.title, args.room_name
    );

    // First captured frame tells us the real (window-backing-store) size;
    // we can't publish a track before knowing this, so wait for frame #1
    // synchronously before connecting to LiveKit.
    let (size_tx, size_rx) = std::sync::mpsc::channel::<(u32, u32)>();
    let first_frame_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // #613: LATEST-WINS SLOT, not a queue.
    //
    // This used to be a `tokio::sync::mpsc::unbounded_channel`, whose comment
    // claimed it would "drop the frame rather than block" when the receiver
    // fell behind -- but an UNBOUNDED channel is never full, so nothing was
    // ever dropped and slow consumption showed up as an ever-growing backlog
    // of stale frames instead. Each frame still carried its own capture
    // timestamp, so the backlog was billed to latency: measured p50 143.6ms
    // against 21.3ms for a synthetic source on the identical SFU/encoder,
    // i.e. ~122ms of pure probe-side queueing masquerading as pipeline cost.
    //
    // `session/share.rs` (the REAL share path) has never worked this way: it
    // keeps a single `latest_frame` slot behind a mutex and counts overwrites
    // in `latest_frame_overwrites`. Mirroring that here is what makes this
    // probe's latency number comparable to the product's.
    let latest_frame: std::sync::Arc<
        std::sync::Mutex<Option<(desktop_lib::capture::CapturedFrame, u64)>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let latest_frame_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let overwrites = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (delay_sender, mut delay_receiver) = tokio::sync::mpsc::channel::<(
        desktop_lib::capture::CapturedFrame,
        u64,
        tokio::time::Instant,
    )>(PRESENTATION_DELAY_QUEUE_CAPACITY);
    let delay_sender = (args.presentation_delay_ms > 0).then_some(delay_sender);
    let delay_overflows = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    let latest_frame_cb = latest_frame.clone();
    let notify_cb = latest_frame_notify.clone();
    let overwrites_cb = overwrites.clone();
    let delay_sender_cb = delay_sender.clone();
    let delay_overflows_cb = delay_overflows.clone();
    let first_frame_sent_cb = first_frame_sent.clone();
    let capture = desktop_lib::capture::WindowCapture::start(target.window_id, move |frame| {
        let capture_wall_time_us = now_us();
        if !first_frame_sent_cb.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let _ = size_tx.send((frame.width, frame.height));
        }
        if let Some(sender) = &delay_sender_cb {
            // The positive-control path is deliberately FIFO, bounded, and
            // timestamped at capture.  A full queue is an invalid instrument,
            // never a silent latest-wins overwrite.
            if sender
                .try_send((frame, capture_wall_time_us, tokio::time::Instant::now()))
                .is_err()
            {
                delay_overflows_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        } else if let Ok(mut slot) = latest_frame_cb.lock() {
            if slot.replace((frame, capture_wall_time_us)).is_some() {
                overwrites_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            notify_cb.notify_one();
        }
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
    verify_capture_raster((width, height), args.expected_capture_size).unwrap_or_else(|error| {
        let _ = capture.stop();
        eprintln!("INVALID_CAPTURE_RASTER: {error}; aborting before LiveKit publish");
        std::process::exit(3);
    });
    if let Some((expected_width, expected_height)) = args.expected_capture_size {
        println!("CAPTURE_RASTER_VERIFIED {expected_width}x{expected_height}");
    }
    println!("First frame received: {width}x{height}. Connecting to LiveKit...");

    let published_track =
        desktop_lib::transport::RoomConnection::connect_and_publish(&url, &token, width, height)
            .await
            .unwrap_or_else(|e| {
                eprintln!("Failed to connect/publish: {e}");
                std::process::exit(1);
            });

    println!(
        "Published. Streaming frames for {} seconds (Ctrl-C to stop earlier)...",
        args.seconds
    );

    let published_track = std::sync::Arc::new(published_track);
    let published_at = std::time::Instant::now();
    let pushed_frames = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let pub_for_loop = published_track.clone();
    let pushed_for_loop = pushed_frames.clone();
    let overwrites_for_loop = overwrites.clone();
    let presentation_delay_ms = args.presentation_delay_ms;
    let delay_overflows_for_loop = delay_overflows.clone();
    let pump = tokio::spawn(async move {
        let push = |frame: desktop_lib::capture::CapturedFrame, ts| {
            if pub_for_loop.push_frame(&frame, ts).is_some() {
                let count = pushed_for_loop.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if count % 60 == 0 {
                    println!(
                        "  published {} frames ({} overwritten overall)",
                        count,
                        overwrites_for_loop.load(std::sync::atomic::Ordering::Relaxed)
                    );
                }
            }
        };
        if presentation_delay_ms > 0 {
            while let Some((frame, ts, captured_at)) = delay_receiver.recv().await {
                if delay_overflows_for_loop.load(std::sync::atomic::Ordering::Relaxed) != 0 {
                    eprintln!("PRESENTATION_DELAY_QUEUE_OVERFLOW invalid_control=true");
                    std::process::exit(3);
                }
                tokio::time::sleep_until(
                    captured_at + std::time::Duration::from_millis(presentation_delay_ms),
                )
                .await;
                push(frame, ts);
            }
        } else {
            loop {
                latest_frame_notify.notified().await;
                if let Some((frame, ts)) = latest_frame.lock().ok().and_then(|mut s| s.take()) {
                    push(frame, ts);
                }
            }
        }
    });

    let evidence = spawn_aligned_window_evidence(
        published_track,
        pushed_frames,
        overwrites,
        SourceMode::Real,
        published_at,
        args.measurement_window_file.clone(),
    );

    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(args.seconds)) => {
            println!("{}s elapsed, stopping.", args.seconds);
        }
        _ = tokio::signal::ctrl_c() => {
            println!("Ctrl-C received, stopping.");
        }
    }

    let _ = capture.stop();
    pump.abort();
    if !evidence.is_finished() {
        eprintln!("FAILED: publisher stopped before aligned window evidence completed");
        std::process::exit(1);
    }
    evidence.await.unwrap_or_else(|error| {
        eprintln!("FAILED: publisher evidence task failed: {error}");
        std::process::exit(1);
    });
    println!("RoomConnection done.");
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoding(reason: &str, stats_id: &str) -> OutboundVideoEncodingSnapshot {
        OutboundVideoEncodingSnapshot {
            stats_id: stats_id.to_string(),
            ssrc: 1,
            rid: stats_id.to_string(),
            active: true,
            frame_width: 1_600,
            frame_height: 900,
            frames_encoded: 1,
            packets_sent: 1,
            quality_limitation: reason.to_string(),
        }
    }

    fn outbound_with_reasons(boundary: &str, reasons: &[&str]) -> OutboundSnapshot {
        OutboundSnapshot {
            video_encodings: reasons
                .iter()
                .enumerate()
                .map(|(index, reason)| encoding(reason, &format!("{boundary}-{index}")))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn source_mode_is_explicit_and_legacy_real_args_still_parse() {
        let legacy = vec!["42".to_string(), "room-a".to_string()];
        let parsed = parse_args_from(&legacy).unwrap();
        assert_eq!(parsed.source, SourceMode::Real);
        assert_eq!(parsed.window_id, Some(42));
        assert_eq!(parsed.room_name, "room-a");

        let synthetic = vec![
            "0".to_string(),
            "room-b".to_string(),
            "--source".to_string(),
            "synthetic".to_string(),
            "--fps".to_string(),
            "30".to_string(),
        ];
        assert_eq!(
            parse_args_from(&synthetic).unwrap().source,
            SourceMode::Synthetic
        );
        let invalid = vec!["--source".to_string(), "random".to_string()];
        assert!(parse_args_from(&invalid).is_err());
    }

    #[test]
    fn expected_real_capture_dimensions_must_be_supplied_as_a_positive_pair() {
        let exact = vec![
            "42".to_string(),
            "room".to_string(),
            "--expected-capture-width".to_string(),
            "1600".to_string(),
            "--expected-capture-height".to_string(),
            "900".to_string(),
        ];
        assert_eq!(
            parse_args_from(&exact).unwrap().expected_capture_size,
            Some((1_600, 900))
        );
        let partial = vec!["--expected-capture-width".to_string(), "1600".to_string()];
        assert!(parse_args_from(&partial).is_err());
        let zero = vec![
            "--expected-capture-width".to_string(),
            "0".to_string(),
            "--expected-capture-height".to_string(),
            "900".to_string(),
        ];
        assert!(parse_args_from(&zero).is_err());
    }

    #[test]
    fn presentation_delay_is_explicit_and_defaults_to_zero() {
        assert_eq!(parse_args_from(&[]).unwrap().presentation_delay_ms, 0);
        let args = vec!["--presentation-delay-ms".to_string(), "200".to_string()];
        assert_eq!(parse_args_from(&args).unwrap().presentation_delay_ms, 200);
    }

    #[test]
    fn capture_preflight_is_explicit_and_classifies_non_frame_outcomes() {
        let args = vec![
            "42".to_string(),
            "--capture-preflight-only".to_string(),
            "--expected-capture-width".to_string(),
            "960".to_string(),
            "--expected-capture-height".to_string(),
            "600".to_string(),
        ];
        let parsed = parse_args_from(&args).unwrap();
        assert!(parsed.capture_preflight_only);
        assert_eq!(parsed.window_id, Some(42));
        assert_eq!(parsed.expected_capture_size, Some((960, 600)));

        let no_output = desktop_lib::capture::CaptureDiagnosticsSnapshot::default();
        assert_eq!(capture_preflight_reason(&no_output), "no-sck-output");
        assert_eq!(
            capture_preflight_reason(&desktop_lib::capture::CaptureDiagnosticsSnapshot {
                no_buffer_frames: 1,
                ..Default::default()
            }),
            "no-image-buffer"
        );
        assert_eq!(
            capture_preflight_reason(&desktop_lib::capture::CaptureDiagnosticsSnapshot {
                pixel_format_rejections: 1,
                ..Default::default()
            }),
            "pixel-format-rejection"
        );
        assert_eq!(
            capture_preflight_reason(&desktop_lib::capture::CaptureDiagnosticsSnapshot {
                layout_rejections: 1,
                ..Default::default()
            }),
            "layout-rejection"
        );
        assert_eq!(
            capture_preflight_reason(&desktop_lib::capture::CaptureDiagnosticsSnapshot {
                stream_errors: 1,
                ..Default::default()
            }),
            "stream-error"
        );
        assert_eq!(
            capture_preflight_reason(&desktop_lib::capture::CaptureDiagnosticsSnapshot {
                accepted_frames: 1,
                ..Default::default()
            }),
            "accepted-frame"
        );
    }

    #[test]
    fn presentation_delay_fifo_retains_30fps_order_and_fails_overflow() {
        let mut queue = PresentationDelaySchedule::new(PRESENTATION_DELAY_QUEUE_CAPACITY);
        let cadence_us = 1_000_000 / 30;
        for sequence in 0..30 {
            queue
                .enqueue(sequence, sequence * cadence_us, 200_000)
                .unwrap();
            assert_eq!(queue.pop_due(sequence * cadence_us + 199_999), None);
            assert_eq!(
                queue
                    .pop_due(sequence * cadence_us + 200_000)
                    .unwrap()
                    .sequence,
                sequence
            );
        }
        assert!(!queue.overflowed);
        let mut tiny = PresentationDelaySchedule::new(2);
        assert!(tiny.enqueue(1, 0, 200_000).is_ok());
        assert!(tiny.enqueue(2, 1, 200_000).is_ok());
        assert!(tiny.enqueue(3, 2, 200_000).is_err());
        assert!(tiny.overflowed);
    }

    #[test]
    fn real_capture_raster_verification_accepts_only_exact_physical_dimensions() {
        assert!(verify_capture_raster((1_600, 900), Some((1_600, 900))).is_ok());
        assert!(verify_capture_raster((3_200, 1_864), Some((1_600, 900))).is_err());
        assert!(verify_capture_raster((1_600, 932), Some((1_600, 900))).is_err());
        assert!(verify_capture_raster((3_200, 1_800), Some((1_600, 900))).is_err());
    }

    #[test]
    fn publisher_validity_uses_only_aligned_counter_deltas() {
        let start = CounterSnapshot {
            at_us: 16_000_000,
            pushed_frames: 480,
            capture_slot_overwrites: 20,
        };
        let end = CounterSnapshot {
            at_us: 28_000_000,
            pushed_frames: 840,
            capture_slot_overwrites: 22,
        };
        let delta = counter_delta(start, end).unwrap();
        assert_eq!(delta.seconds, 12.0);
        assert_eq!(delta.pushed_frames, 360);
        assert_eq!(delta.capture_slot_overwrites, 2);
        assert_eq!(delta.pushed_fps, 30.0);
        assert!((delta.overwrite_ratio - (2.0 / 360.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn publisher_counter_delta_rejects_non_monotonic_or_empty_windows() {
        let start = CounterSnapshot {
            at_us: 20,
            pushed_frames: 5,
            capture_slot_overwrites: 1,
        };
        assert!(counter_delta(start, CounterSnapshot { at_us: 20, ..start }).is_err());
        assert!(counter_delta(
            start,
            CounterSnapshot {
                at_us: 30,
                pushed_frames: 4,
                capture_slot_overwrites: 1,
            }
        )
        .is_err());
    }

    #[test]
    fn publisher_and_receiver_can_share_the_exact_absolute_window() {
        let window = parse_measurement_window("16000000 28000000\n").unwrap();
        assert_eq!(window.start_us, 16_000_000);
        assert_eq!(window.end_us, 28_000_000);
        assert!(parse_measurement_window("28000000 16000000").is_err());
    }

    #[test]
    fn publisher_quality_gate_rejects_mixed_none_and_bandwidth_layers() {
        let start = outbound_with_reasons("start", &["None", "Bandwidth"]);
        let end = outbound_with_reasons("end", &["None", "None"]);
        assert!(!outbound_quality_gate(&start, &end));
    }

    #[test]
    fn publisher_quality_gate_rejects_mixed_none_and_cpu_layers() {
        let start = outbound_with_reasons("start", &["None", "None"]);
        let end = outbound_with_reasons("end", &["None", "Cpu"]);
        assert!(!outbound_quality_gate(&start, &end));
    }

    #[test]
    fn publisher_quality_gate_checks_both_boundary_snapshots() {
        let clean_start = outbound_with_reasons("start", &["None", "None"]);
        let clean_end = outbound_with_reasons("end", &["None", "None"]);
        assert!(outbound_quality_gate(&clean_start, &clean_end));

        let limited_start = outbound_with_reasons("start", &["Cpu", "None"]);
        assert!(!outbound_quality_gate(&limited_start, &clean_end));

        let limited_end = outbound_with_reasons("end", &["Bandwidth", "None"]);
        assert!(!outbound_quality_gate(&clean_start, &limited_end));
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("publish_probe is macOS-only.");
    std::process::exit(1);
}
