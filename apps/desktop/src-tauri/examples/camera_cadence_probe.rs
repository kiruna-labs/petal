//! #549 camera-cadence probe: measure the published-webcam pipeline's
//! per-stage cadence with a *synthetic* NV12 source, so the same measurement
//! can be taken from an aarch64 process and from an x86_64 process under
//! Rosetta and compared directly.
//!
//! It drives the real product path — `RoomConnection::publish_camera` +
//! `PublishedTrack::push_nv12` — and never touches AVFoundation, so it needs
//! no camera hardware and no camera TCC grant. That also makes the input
//! byte-identical across architectures, which is the whole point: any cadence
//! difference between the two runs is the pipeline, not the webcam.
//!
//! Measure in `--release`; a debug build's unoptimized plane copies dominate
//! every number here. Release builds cannot mint their own token (that is
//! `debug_assertions`-only in the product), so pass one in:
//!
//! ```sh
//! # receiver (aarch64)
//! PETAL_PROBE_SUBSCRIBE_TOKEN=<jwt> \
//!   cargo run --release --example camera_cadence_probe -- subscribe petal-549 40
//! # sender, aarch64 baseline
//! PETAL_PROBE_PUBLISH_TOKEN=<jwt> \
//!   cargo run --release --example camera_cadence_probe -- publish petal-549 30
//! # sender, x86_64 code path under Rosetta (see docs/TESTING.md)
//! PETAL_PROBE_PUBLISH_TOKEN=<jwt> LIVEKIT_URL=<url> \
//!   ./target/x86_64-apple-darwin/release/examples/camera_cadence_probe publish petal-549 30
//! ```
//!
//! Reads `LIVEKIT_URL` / `LIVEKIT_API_KEY` / `LIVEKIT_API_SECRET` from
//! `apps/desktop/.env` (via `dotenvy`) or the process environment; never logs
//! their values. Throwaway experiment loop for #549, not cockpit apparatus.
//!
//! ## Subscribed-layer timeline (#592)
//!
//! `subscribe` takes an optional 4th argument -- `none` (default), `low`,
//! `medium` or `high` -- the explicit `UpdateTrackSettings` quality request
//! the receiver sends, and reports which simulcast layer the SFU actually
//! forwarded, when. Decoded frame geometry is the only receiver-visible
//! signal for that (the SDK exposes no current-spatial-layer readback), so a
//! resolution transition IS a layer transition.
//!
//! `none` is the honest baseline: it is what both real receive paths send
//! for a camera track (`start_compositor_feed` requests HIGH only for
//! `petal-window-*`, and the webview gallery bridge sends no track settings
//! at all). `low` is the POSITIVE CONTROL -- it pins the receiver to the
//! bottom rung for the whole run, which is what proves this measurement can
//! observe the low-layer state and that the settings path reaches the SFU at
//! all. Without it, a fast `none` run is indistinguishable from a broken
//! measurement.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const TARGET_FPS: u64 = 30;
const DEFAULT_SECONDS: u64 = 20;

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn arch_label() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "other"
    }
}

/// Local token minting is `debug_assertions`-only in the product, but cadence
/// must be measured in `--release` (an unoptimized plane copy would dominate
/// every number here). So a release build takes its token from the
/// environment: mint one with `cargo run --example mint_token`.
fn token_for(identity: &str, room: &str, publish: bool, subscribe: bool) -> String {
    let env_key = if publish {
        "PETAL_PROBE_PUBLISH_TOKEN"
    } else {
        "PETAL_PROBE_SUBSCRIBE_TOKEN"
    };
    if let Ok(token) = std::env::var(env_key) {
        if !token.is_empty() {
            return token;
        }
    }
    #[cfg(debug_assertions)]
    {
        return desktop_lib::transport::mint_access_token(identity, room, publish, subscribe)
            .unwrap_or_else(|e| {
                eprintln!("Failed to mint access token: {e}");
                std::process::exit(1);
            });
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (identity, room, subscribe);
        eprintln!(
            "{env_key} is required for a release build (local token minting is debug-only).\n\
             Mint one with: cargo run --example mint_token -- --room <room> --identity <id> \
             --publish {publish} --subscribe {subscribe}"
        );
        std::process::exit(1);
    }
}

fn livekit_url_or_exit() -> String {
    if let Ok(url) = std::env::var("LIVEKIT_URL") {
        if !url.is_empty() {
            return url;
        }
    }
    eprintln!("LIVEKIT_URL is not set (checked the process env and apps/desktop/.env).");
    std::process::exit(1);
}

fn percentile(sorted: &[f64], pct: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[(sorted.len() * pct / 100).min(sorted.len() - 1)]
}

/// A moving hard-edge pattern: high-contrast vertical bars that translate one
/// pixel per frame, plus a sweeping horizontal band. Hard edges in constant
/// motion are the content that makes conversion, encode, and pacing defects
/// visible; a flat gradient would hide all three.
///
/// Every frame is rendered up front and then replayed from the ring, so the
/// measured loop contains only the product path (convert + push) and not this
/// probe's own pattern generator.
struct SyntheticCamera {
    frames: Vec<(Vec<u8>, Vec<u8>)>,
    y_stride: usize,
    uv_stride: usize,
}

const RING_FRAMES: usize = 32;

impl SyntheticCamera {
    fn new(width: usize, height: usize) -> Self {
        // Mirror a real CoreVideo camera buffer: rows padded to 64 bytes.
        let y_stride = width.div_ceil(64) * 64;
        let uv_stride = (width.div_ceil(2) * 2).div_ceil(64) * 64;
        let frames = (0..RING_FRAMES)
            .map(|frame| {
                let mut y = vec![0u8; y_stride * height];
                let mut uv = vec![0u8; uv_stride * height.div_ceil(2)];
                let phase = frame % 64;
                let band = (frame * 7) % height;
                for row in 0..height {
                    let row_base = row * y_stride;
                    let in_band = row.abs_diff(band) < 16;
                    for col in 0..width {
                        let bar = ((col + phase) / 8) % 2 == 0;
                        y[row_base + col] = match (bar, in_band) {
                            (true, true) => 235,
                            (true, false) => 200,
                            (false, true) => 90,
                            (false, false) => 16,
                        };
                    }
                }
                for row in 0..height.div_ceil(2) {
                    let row_base = row * uv_stride;
                    for col in 0..width.div_ceil(2) {
                        let base = row_base + (col * 2);
                        uv[base] = ((col + phase) % 256) as u8;
                        uv[base + 1] = ((row + band) % 256) as u8;
                    }
                }
                (y, uv)
            })
            .collect();
        Self {
            frames,
            y_stride,
            uv_stride,
        }
    }

    fn frame(&self, index: u64) -> &(Vec<u8>, Vec<u8>) {
        &self.frames[(index as usize) % RING_FRAMES]
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));

    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "publish".to_string());
    let room_name = args.next().unwrap_or_else(|| "petal-549-camera".to_string());
    let seconds: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SECONDS);

    let quality_arg = args.next().unwrap_or_else(|| "none".to_string());
    let quality = QualityRequest::parse(&quality_arg).unwrap_or_else(|| {
        eprintln!("unknown quality request '{quality_arg}' (want 'none', 'low', 'medium' or 'high')");
        std::process::exit(2);
    });

    let url = livekit_url_or_exit();

    match mode.as_str() {
        "publish" => publish(&url, &room_name, seconds).await,
        "subscribe" => subscribe(&url, &room_name, seconds, quality).await,
        other => {
            eprintln!("unknown mode '{other}' (want 'publish' or 'subscribe')");
            std::process::exit(2);
        }
    }
}

async fn publish(url: &str, room_name: &str, seconds: u64) {
    let arch = arch_label();
    let identity = format!("petal-549-pub-{arch}");
    let token = token_for(&identity, room_name, true, false);

    println!("[{arch}] publishing synthetic camera {WIDTH}x{HEIGHT}@{TARGET_FPS} into '{room_name}' for {seconds}s");

    let room = desktop_lib::transport::RoomConnection::connect(url, &token)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to connect: {e}");
            std::process::exit(1);
        });
    room.discard_compositor_events();

    let track = room
        .publish_camera(WIDTH, HEIGHT, TARGET_FPS as f64, &identity)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to publish camera track: {e}");
            std::process::exit(1);
        });

    let source = SyntheticCamera::new(WIDTH as usize, HEIGHT as usize);
    let mut convert_ms = Vec::<f64>::new();
    let mut capture_ms = Vec::<f64>::new();
    let mut pushed = 0u64;
    let mut dropped = 0u64;
    let mut late_ticks = 0u64;

    let frame_interval = Duration::from_nanos(1_000_000_000 / TARGET_FPS);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut next_tick = started;
    let mut frame: u64 = 0;

    while Instant::now() < deadline {
        next_tick += frame_interval;
        let (y, uv) = source.frame(frame);

        match track.push_nv12(
            y,
            source.y_stride as u32,
            uv,
            source.uv_stride as u32,
            WIDTH,
            HEIGHT,
            now_us(),
        ) {
            Some(timing) => {
                pushed += 1;
                convert_ms.push(timing.convert_ms);
                capture_ms.push(timing.capture_frame_return_ms);
            }
            None => dropped += 1,
        }
        frame += 1;

        let now = Instant::now();
        if now < next_tick {
            tokio::time::sleep(next_tick - now).await;
        } else {
            // The pipeline could not keep the 30fps budget for this frame.
            late_ticks += 1;
            next_tick = now;
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    convert_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    capture_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());

    println!("\n=== [{arch}] camera publish stages ({pushed} frames in {elapsed:.1}s) ===");
    println!("  push_fps        {:.2}", pushed as f64 / elapsed);
    println!(
        "  convert_ms      p50={:.3} p95={:.3} max={:.3}",
        percentile(&convert_ms, 50),
        percentile(&convert_ms, 95),
        convert_ms.last().copied().unwrap_or(0.0)
    );
    println!(
        "  capture_frame_ms p50={:.3} p95={:.3} max={:.3}",
        percentile(&capture_ms, 50),
        percentile(&capture_ms, 95),
        capture_ms.last().copied().unwrap_or(0.0)
    );
    println!("  dropped_push    {dropped}");
    println!("  late_ticks      {late_ticks}  (frames that missed the 30fps budget)");

    let _ = track.unpublish().await;
}

async fn subscribe(url: &str, room_name: &str, seconds: u64, quality: QualityRequest) {
    let arch = arch_label();
    let identity = format!("petal-549-sub-{arch}");
    let token = token_for(&identity, room_name, false, true);

    println!("[{arch}] subscribing in '{room_name}' for {seconds}s (quality request: {quality})");

    let frames = Arc::new(AtomicU64::new(0));
    let id_gaps = Arc::new(AtomicU64::new(0));
    let last_id = Arc::new(std::sync::atomic::AtomicI64::new(-1));
    let arrivals_us = Arc::new(Mutex::new(Vec::<u64>::new()));
    let size = Arc::new(Mutex::new((0u32, 0u32)));
    let layers = Arc::new(Mutex::new(Vec::<LayerSample>::new()));
    let subscribed_at = Instant::now();

    let frames_cb = frames.clone();
    let gaps_cb = id_gaps.clone();
    let last_id_cb = last_id.clone();
    let arrivals_cb = arrivals_us.clone();
    let size_cb = size.clone();
    let layers_cb = layers.clone();

    let _subscriber = desktop_lib::transport::Subscriber::connect_with_quality_request(
        url,
        &token,
        quality.as_video_quality(),
        move |frame| {
            frames_cb.fetch_add(1, Ordering::Relaxed);
            let mut current = size_cb.lock().unwrap();
            if *current != (frame.width, frame.height) {
                *current = (frame.width, frame.height);
                layers_cb.lock().unwrap().push(LayerSample {
                    at_ms: subscribed_at.elapsed().as_millis() as u64,
                    width: frame.width,
                    height: frame.height,
                });
            }
            drop(current);
            arrivals_cb.lock().unwrap().push(frame.receive_timestamp_us);
            if let Some(id) = frame.frame_id {
                let prev = last_id_cb.swap(id as i64, Ordering::Relaxed);
                if prev >= 0 && (id as i64) > prev + 1 {
                    gaps_cb.fetch_add((id as i64 - prev - 1) as u64, Ordering::Relaxed);
                }
            }
        },
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("Failed to connect: {e}");
        std::process::exit(1);
    });

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last_count = 0u64;
    let mut last_at = Instant::now();
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let count = frames.load(Ordering::Relaxed);
        let window = last_at.elapsed().as_secs_f64();
        let (w, h) = *size.lock().unwrap();
        println!(
            "  [{arch}] frames={count} size={w}x{h} window_fps={:.2} id_gaps={}",
            (count - last_count) as f64 / window,
            id_gaps.load(Ordering::Relaxed)
        );
        last_count = count;
        last_at = Instant::now();
    }

    let arrivals = arrivals_us.lock().unwrap();
    if arrivals.len() < 2 {
        eprintln!("[{arch}] FAILED: fewer than two decoded frames received.");
        std::process::exit(1);
    }
    let mut gaps_ms: Vec<f64> = arrivals
        .windows(2)
        .map(|w| (w[1].saturating_sub(w[0])) as f64 / 1000.0)
        .collect();
    gaps_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let span_s = (arrivals.last().unwrap() - arrivals.first().unwrap()) as f64 / 1_000_000.0;

    println!("\n=== [{arch}] decoded cadence (n={} frames) ===", arrivals.len());
    println!("  decoded_fps     {:.2}", (arrivals.len() - 1) as f64 / span_s);
    println!(
        "  interframe_ms   p50={:.1} p95={:.1} p99={:.1} max={:.1}",
        percentile(&gaps_ms, 50),
        percentile(&gaps_ms, 95),
        percentile(&gaps_ms, 99),
        gaps_ms.last().copied().unwrap_or(0.0)
    );
    println!(
        "  stalls>100ms    {}",
        gaps_ms.iter().filter(|g| **g > 100.0).count()
    );
    println!("  sender_id_gaps  {}", id_gaps.load(Ordering::Relaxed));

    report_layer_timeline(arch, &layers.lock().unwrap());
}

/// #592: which simulcast layer the SFU is actually forwarding, and when it
/// changed. Decoded frame geometry is the only receiver-visible signal for
/// this -- the SDK exposes no "current spatial layer" readback -- so a
/// resolution transition IS the layer transition.
#[derive(Debug, Clone, Copy)]
struct LayerSample {
    at_ms: u64,
    width: u32,
    height: u32,
}

fn report_layer_timeline(arch: &str, layers: &[LayerSample]) {
    println!("\n=== [{arch}] subscribed layer timeline ===");
    for sample in layers {
        println!(
            "  t+{:>6}ms  {}x{}",
            sample.at_ms, sample.width, sample.height
        );
    }
    match layers.iter().find(|s| s.height >= HEIGHT) {
        Some(sample) => println!("  time_to_high_layer_ms  {}", sample.at_ms),
        None => println!("  time_to_high_layer_ms  NEVER (stayed below {WIDTH}x{HEIGHT})"),
    }
}

/// The subscriber's explicit `UpdateTrackSettings` quality request, if any.
/// `None` is what both real receive paths send today (the JS gallery bridge
/// sends no track settings at all), so it is the honest baseline arm; `Low`
/// is the positive control that proves this measurement can observe the
/// low-layer state at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QualityRequest {
    None,
    Low,
    Medium,
    High,
}

impl QualityRequest {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "none" => Some(Self::None),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    fn as_video_quality(self) -> Option<livekit::track::VideoQuality> {
        match self {
            Self::None => None,
            Self::Low => Some(livekit::track::VideoQuality::Low),
            Self::Medium => Some(livekit::track::VideoQuality::Medium),
            Self::High => Some(livekit::track::VideoQuality::High),
        }
    }
}

impl std::fmt::Display for QualityRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        })
    }
}
