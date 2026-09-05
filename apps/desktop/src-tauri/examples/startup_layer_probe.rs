//! #299 startup-layer probe: how long does a freshly started share take to
//! become sharp, and what cadence does it start at?
//!
//! The symptom #299 reports -- "a new share takes too long to appear, starts
//! blurry, and the receiver shows ~6fps" -- is a single measurable quantity
//! pair: the simulcast layer the receiver decodes at t=0, and how long it
//! stays there. The default Full window share publishes two layers
//! (`publisher::full_share_simulcast_layers` plus the source preset):
//!
//!   `q`  3W/4 x 3H/4  capped at 30fps
//!   `h`  W    x H     up to 60fps
//!
//! The probe resolves every selected ladder at runtime, so its decoded buffer
//! dimensions name the layer exactly without assuming this default shape.
//!
//! The probe publishes through the REAL
//! [`desktop_lib::transport::publisher::full_share_publish_options`] and drives
//! the REAL [`desktop_lib::viewer_demand::startup_demand_decision`] -- the same
//! function `demand_for_window` calls, with its real occlusion-hysteresis
//! state -- replaying the receiver lifecycle the compositor actually produces:
//!
//!   t=0     `ensure_window` creates the panel HIDDEN and publishes Open.
//!           AppKit reports a hidden window as fully occluded.
//!   t=150ms a geometry/DPI settle publishes a Heartbeat; panel still hidden.
//!   first frame -> the panel is revealed. Nothing publishes demand here.
//!   t=2s    the heartbeat publishes again; panel now visible.
//!
//! Usage (needs a LiveKit server; a local dev one is fine):
//!
//! ```sh
//! LIVEKIT_URL=ws://localhost:7897 LIVEKIT_API_KEY=devkey \
//! LIVEKIT_API_SECRET=... \
//!   cargo run --example startup_layer_probe -- [--seconds 12] [MODE]
//! ```
//!
//! Modes:
//!
//!   (default)      replay the real startup demand sequence
//!   `--pin-lowest` POSITIVE CONTROL: request the selected ladder's bottom
//!                  rung for the whole run, so a clean run is believable only
//!                  once this control demonstrably reaches that rung
//!   `--no-demand`  send no track settings at all -- what the browser peer
//!                  does before its tile exists (`adaptiveStream` is off in
//!                  web-harness, and its only `setVideoQuality`/
//!                  `setVideoDimensions` call is tile-backed, post-subscribe).
//!                  Measures the SFU's own default initial layer.
//!   `--quality-then-dimensions`
//!                  #590 part 2: `set_video_quality(High)` followed by
//!                  `update_video_dimensions`. The vendored SDK's
//!                  `on_video_dimensions_changed` builds `UpdateTrackSettings`
//!                  with `..Default::default()`, so the dimensions message
//!                  carries an implicit `quality: LOW`. Does that undo the
//!                  HIGH request in practice, or does the SFU prefer the
//!                  dimensions? Measured, not assumed.
//!
//! Experiment loop for #299 work -- not cockpit apparatus and not a runtime
//! diagnostic subsystem (the project history §2.1).

use futures::StreamExt;
use livekit::prelude::*;
use livekit::track::{LocalTrack, LocalVideoTrack, RemoteTrack, RemoteVideoTrack, VideoQuality};
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::webrtc::video_stream::native::NativeVideoStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use desktop_lib::viewer_demand::{startup_demand_decision, StartupDemandInputs};

const SOURCE_WIDTH: u32 = 1920;
const SOURCE_HEIGHT: u32 = 1080;
/// The source is driven at exactly this rate, so decoded fps is measured
/// against a known ceiling rather than an assumed one.
const SOURCE_FPS: f64 = 30.0;
const WINDOW_ID: u32 = 299;

/// `viewer_demand.rs`'s lower-bound request dimensions.
const LOWEST_W: u32 = 640;
const LOWEST_H: u32 = 360;

/// `viewer_demand.rs`'s GEOMETRY_REFRESH_DEBOUNCE -- when a DPI/geometry
/// settle republishes demand while the panel is still a hidden placeholder.
const GEOMETRY_REFRESH_AT: Duration = Duration::from_millis(150);
/// `viewer_demand.rs`'s HEARTBEAT_INTERVAL.
const HEARTBEAT_AT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    RealSequence,
    PinLowest,
    NoDemand,
    QualityThenDimensions,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::RealSequence => "real startup demand sequence",
            Self::PinLowest => "POSITIVE CONTROL: pinned to 640x360 for the whole run",
            Self::NoDemand => "no track settings at all (browser peer pre-tile)",
            Self::QualityThenDimensions => "set_video_quality(High) then update_video_dimensions",
        }
    }

    /// A control run is expected to look bad; a clean reading from it means
    /// the harness cannot see the failure and no pass can be trusted.
    fn is_control(self) -> bool {
        self == Self::PinLowest
    }
}

#[derive(Debug, Clone, Copy)]
struct FrameObservation {
    at_ms: u128,
    width: u32,
    height: u32,
    /// #613: capture_frame() call -> decoded frame delivered to this process,
    /// from LiveKit's own packet-trailer `user_timestamp`. Publisher and
    /// subscriber are the same process here, so this is one clock and needs
    /// no offset correction. `None` means the frame carried no metadata,
    /// which is a harness fault and is reported as one rather than skipped.
    latency_ms: Option<f64>,
}

/// Names the simulcast layer from the decoded buffer's own dimensions,
/// against the ladder that is ACTUALLY live rather than an assumed q/h/f
/// geometry. The native SDK does not surface the RID on decoded frames, but
/// rung widths are distinct within any one ladder, so the nearest rung at or
/// below the decoded width names it exactly.
///
/// This must not be a hand-copied mirror of a fixed ladder: under
/// `PETAL_SHARE_LADDER=raised` both lower rungs would otherwise print as `h`,
/// and under either two-rung ladder the source rung is `h`, not `f`.
fn layer_name(rungs: &[(String, u32, u32)], width: u32) -> String {
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

/// #613: microseconds since the Unix epoch. Mirrors `desktop_lib`'s internal
/// `time_util::now_us`, which is not `pub` outside the crate.
fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn token_for(identity: &str, room: &str) -> String {
    desktop_lib::transport::mint_access_token(identity, room, true, true).unwrap_or_else(|e| {
        eprintln!("failed to mint access token: {e}");
        std::process::exit(1);
    })
}

/// Publishes one `petal-window-299` track through the real Full-share publish
/// options and drives genuinely changing content into it at `SOURCE_FPS`. A
/// static frame lets every layer's encoder coast, which would make a cadence
/// measurement meaningless.
async fn publish_share(
    room: &Room,
    stop: Arc<AtomicBool>,
    inject_delay_ms: u64,
    noise: bool,
) -> LocalVideoTrack {
    let source = NativeVideoSource::new(
        VideoResolution {
            width: SOURCE_WIDTH,
            height: SOURCE_HEIGHT,
        },
        true,
    );
    let track = LocalVideoTrack::create_video_track(
        &format!("petal-window-{WINDOW_ID}"),
        RtcVideoSource::Native(source.clone()),
    );

    room.local_participant()
        .publish_track(
            LocalTrack::Video(track.clone()),
            desktop_lib::transport::publisher::full_share_publish_options(
                SOURCE_WIDTH,
                SOURCE_HEIGHT,
            ),
        )
        .await
        .unwrap_or_else(|e| {
            eprintln!("publish failed: {e}");
            std::process::exit(1);
        });

    tokio::spawn(async move {
        let mut tick =
            tokio::time::interval(Duration::from_micros((1_000_000.0 / SOURCE_FPS) as u64));
        let mut n: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            tick.tick().await;
            let mut buf = I420Buffer::new(SOURCE_WIDTH, SOURCE_HEIGHT);
            let band = (n * 7) % SOURCE_HEIGHT;
            {
                let (y, u, v) = buf.data_mut();
                if noise {
                    // #613: HIGH-ENTROPY content. The two-tone band below
                    // compresses to almost nothing, which makes the encoder and
                    // the pacer look free. A real shared window (text, UI
                    // chrome) does not. Cheap xorshift so the cost stays in the
                    // encoder rather than in this fill loop.
                    let mut st: u32 = 0x2545_f491 ^ n.wrapping_mul(2_654_435_761);
                    for b in y.iter_mut() {
                        st ^= st << 13;
                        st ^= st >> 17;
                        st ^= st << 5;
                        *b = (st >> 24) as u8;
                    }
                    for b in u.iter_mut() {
                        st ^= st << 13;
                        st ^= st >> 17;
                        st ^= st << 5;
                        *b = (st >> 24) as u8;
                    }
                    for b in v.iter_mut() {
                        st ^= st << 13;
                        st ^= st >> 17;
                        st ^= st << 5;
                        *b = (st >> 24) as u8;
                    }
                } else {
                    for row in 0..SOURCE_HEIGHT as usize {
                        let luma = if row.abs_diff(band as usize) < 80 {
                            235
                        } else {
                            16
                        };
                        let start = row * SOURCE_WIDTH as usize;
                        y[start..start + SOURCE_WIDTH as usize].fill(luma);
                    }
                    u.fill(128);
                    v.fill(128);
                }
            }
            // #613 POSITIVE CONTROL: `--inject-delay-ms N` stamps the frame,
            // then sleeps N ms before handing it to the pipeline. The reported
            // latency MUST rise by ~N. A run that does not move is an
            // instrument that cannot see latency at all, and no clean reading
            // from it is believable.
            let stamped_at_us = now_us();
            if inject_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(inject_delay_ms)).await;
            }
            source.capture_frame(&VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: 0,
                frame_metadata: Some(livekit::webrtc::video_frame::FrameMetadata {
                    user_timestamp: Some(stamped_at_us),
                    frame_id: Some(n),
                }),
                buffer: &buf,
            });
            n = n.wrapping_add(1);
        }
    });

    track
}

/// #613: the RTCStats fields the pinned SDK already exposes but nothing in
/// Petal reads. Sampled as a pair (start of the steady-state window, end of
/// run) so every number below is a DELTA over the measured window, never a
/// since-connect cumulative average that the startup ramp is baked into.
#[derive(Debug, Clone, Copy, Default)]
struct StageCounters {
    // outbound
    /// Frame count summed across simulcast layers.
    layer_frames_encoded: u32,
    /// The greatest cumulative frame count from one simulcast layer: each
    /// source frame can be encoded into every active layer, but is one source
    /// frame for the sender's total encode-work denominator.
    source_frames_encoded: u32,
    total_encode_time: f64,
    packets_sent: u64,
    total_packet_send_delay: f64,
    // inbound
    frames_decoded: u32,
    total_decode_time: f64,
    total_processing_delay: f64,
    total_assembly_time: f64,
    frames_assembled_from_multiple_packets: u64,
    jitter_buffer_delay: f64,
    jitter_buffer_target_delay: f64,
    jitter_buffer_minimum_delay: f64,
    jitter_buffer_emitted_count: u64,
    total_inter_frame_delay: f64,
    encoder_impl: Option<&'static str>,
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

async fn sample_outbound(track: &LocalVideoTrack, into: &mut StageCounters) {
    let Ok(stats) = track.get_stats().await else {
        return;
    };
    for stat in &stats {
        if let livekit::webrtc::stats::RtcStats::OutboundRtp(o) = stat {
            // Simulcast: encode time is paid on every active layer, so sum
            // that cost. The maximum per-layer frame count represents the
            // distinct source frames that incurred the summed encode work.
            into.layer_frames_encoded += o.outbound.frames_encoded;
            into.source_frames_encoded = into.source_frames_encoded.max(o.outbound.frames_encoded);
            into.total_encode_time += o.outbound.total_encode_time;
            into.packets_sent += o.sent.packets_sent;
            into.total_packet_send_delay += o.outbound.total_packet_send_delay;
            if into.encoder_impl.is_none() && !o.outbound.encoder_implementation.is_empty() {
                into.encoder_impl = Some(Box::leak(
                    o.outbound.encoder_implementation.clone().into_boxed_str(),
                ));
            }
        }
    }
}

async fn sample_inbound(track: &RemoteVideoTrack, into: &mut StageCounters) {
    let Ok(stats) = track.get_stats().await else {
        return;
    };
    for stat in &stats {
        if let livekit::webrtc::stats::RtcStats::InboundRtp(i) = stat {
            into.frames_decoded += i.inbound.frames_decoded;
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

/// One step of the receiver lifecycle, expressed exactly as
/// `demand_for_window` sees it.
struct LifecycleStep {
    at: Duration,
    what: &'static str,
    inputs: StartupDemandInputs,
}

/// The demand publications a brand-new receiver window really makes, in
/// order. Note what is NOT here: the first-frame reveal publishes nothing, so
/// whatever the pre-frame steps requested stands until the 2s heartbeat.
fn real_startup_sequence() -> Vec<LifecycleStep> {
    // Pre-first-frame, `demand_pixel_dimensions` substitutes
    // MAX_DEMAND_DIMENSION_PX for the placeholder's meaningless size.
    let max = desktop_lib::transport::publisher::VIDEO_TOOLBOX_H264_MAX_LONG_EDGE;
    vec![
        LifecycleStep {
            at: Duration::ZERO,
            what: "ensure_window Open (panel created HIDDEN, no frame yet)",
            inputs: StartupDemandInputs {
                closing: false,
                geometry_visible: true,
                appkit_reports_occluded: true,
                first_frame_seen: false,
                pixel_width: max,
                pixel_height: max,
            },
        },
        LifecycleStep {
            at: GEOMETRY_REFRESH_AT,
            what: "geometry/DPI settle Heartbeat (panel STILL hidden)",
            inputs: StartupDemandInputs {
                closing: false,
                geometry_visible: true,
                appkit_reports_occluded: true,
                first_frame_seen: false,
                pixel_width: max,
                pixel_height: max,
            },
        },
        LifecycleStep {
            at: HEARTBEAT_AT,
            what: "2s Heartbeat (panel revealed and visible)",
            inputs: StartupDemandInputs {
                closing: false,
                geometry_visible: true,
                appkit_reports_occluded: false,
                first_frame_seen: true,
                pixel_width: SOURCE_WIDTH,
                pixel_height: SOURCE_HEIGHT,
            },
        },
    ]
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = if args.iter().any(|a| a == "--pin-lowest") {
        Mode::PinLowest
    } else if args.iter().any(|a| a == "--no-demand") {
        Mode::NoDemand
    } else if args.iter().any(|a| a == "--quality-then-dimensions") {
        Mode::QualityThenDimensions
    } else {
        Mode::RealSequence
    };
    let seconds: u64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(12)
        // Must outlast the 2s heartbeat plus a full 10s cadence window.
        .max(12);
    // #613 positive control. Stamps the frame, then withholds it from the
    // pipeline for this long. Measured latency must rise by ~this much.
    // #613: high-entropy source, to separate "the pipeline is fast" from
    // "the test pattern compresses to nothing".
    let noise = args.iter().any(|a| a == "--noise");
    let inject_delay_ms: u64 = args
        .iter()
        .position(|a| a == "--inject-delay-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // #613: everything before this instant is discarded from the latency
    // summary, so the #299 subscription ramp cannot contaminate a steady-state
    // number. Reported separately rather than silently dropped.
    let steady_state_after_ms: u128 = args
        .iter()
        .position(|a| a == "--steady-after-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(8_000);

    let url = desktop_lib::transport::token::livekit_url().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let room_name = format!(
        "petal-299-probe-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );

    println!("=== #299 startup-layer probe ===");
    println!("  room     : {room_name}");
    println!(
        "  source   : {SOURCE_WIDTH}x{SOURCE_HEIGHT} @ {SOURCE_FPS}fps, real full-share options"
    );
    // AS COMPUTED, not as intended: this is the same string the real
    // `publish_window_track` logs, resolved from PETAL_SHARE_LADDER here.
    // Never trust the env var you set -- read this line.
    let ladder_rungs =
        desktop_lib::transport::publisher::full_share_ladder_rungs(SOURCE_WIDTH, SOURCE_HEIGHT);
    let (pin_lowest_w, pin_lowest_h) = ladder_rungs
        .first()
        .map(|(_, width, height)| (*width, *height))
        .expect("full-share ladder always has a bottom rung");
    println!(
        "  LADDER   : {}",
        desktop_lib::transport::publisher::full_share_ladder_description(
            SOURCE_WIDTH,
            SOURCE_HEIGHT
        )
    );
    println!("  mode     : {}", mode.label());
    println!("  content  : {}", if noise { "HIGH-ENTROPY noise" } else { "two-tone band (compresses to ~nothing)" });
    println!("  duration : {seconds}s\n");

    let stop = Arc::new(AtomicBool::new(false));
    let observations: Arc<Mutex<Vec<FrameObservation>>> = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    // #613: kept so RTCStats can be sampled on the real subscribed track.
    let remote_track: Arc<Mutex<Option<RemoteVideoTrack>>> = Arc::new(Mutex::new(None));

    // ---- subscriber peer -------------------------------------------------
    let (sub_room, mut sub_events) = Room::connect(
        &url,
        &token_for("petal-299-sub", &room_name),
        RoomOptions::default(),
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("subscriber connect failed: {e}");
        std::process::exit(1);
    });

    let ev_obs = observations.clone();
    let ev_stop = stop.clone();
    let ev_remote_track = remote_track.clone();
    tokio::spawn(async move {
        while let Some(event) = sub_events.recv().await {
            if ev_stop.load(Ordering::Relaxed) {
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
            if desktop_lib::transport::publisher::window_id_from_track_name(&video.name()).is_none()
            {
                continue;
            }

            // Start observing BEFORE any track settings, so the very first
            // decoded frame really is the initial layer.
            let obs = ev_obs.clone();
            let s = ev_stop.clone();
            let subscribed_at = Instant::now();
            *ev_remote_track.lock().unwrap() = Some(video.clone());
            tokio::spawn(async move {
                let mut stream = NativeVideoStream::new(video.rtc_track());
                while let Some(frame) = stream.next().await {
                    if s.load(Ordering::Relaxed) {
                        break;
                    }
                    let now_us = now_us();
                    let latency_ms = frame
                        .frame_metadata
                        .as_ref()
                        .and_then(|m| m.user_timestamp)
                        .map(|stamped| (now_us.saturating_sub(stamped)) as f64 / 1000.0);
                    obs.lock().unwrap().push(FrameObservation {
                        at_ms: subscribed_at.elapsed().as_millis(),
                        width: frame.buffer.width(),
                        height: frame.buffer.height(),
                        latency_ms,
                    });
                }
            });

            // ---- the demand sequence under measurement -------------------
            match mode {
                Mode::NoDemand => {
                    println!("[t=0ms] no track settings sent (browser peer pre-tile)");
                }
                Mode::PinLowest => {
                    println!(
                        "[t=0ms] CONTROL: update_video_dimensions({pin_lowest_w}x{pin_lowest_h})"
                    );
                    publication.update_video_dimensions(TrackDimension(pin_lowest_w, pin_lowest_h));
                }
                Mode::QualityThenDimensions => {
                    println!("[t=0ms] set_video_quality(High)");
                    publication.set_video_quality(VideoQuality::High);
                    let pubc = publication.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        println!(
                            "[t=500ms] update_video_dimensions({SOURCE_WIDTH}x{SOURCE_HEIGHT}) \
                             -- implicit quality:LOW rides along"
                        );
                        pubc.update_video_dimensions(TrackDimension(SOURCE_WIDTH, SOURCE_HEIGHT));
                    });
                }
                Mode::RealSequence => {
                    // The real receiver also asks for HIGH on subscribe
                    // (`initial_window_subscription_plan_for_track`).
                    publication.set_video_quality(VideoQuality::High);
                    println!("[t=0ms] set_video_quality(High)  (initial subscription plan)");
                    for step in real_startup_sequence() {
                        let pubc = publication.clone();
                        tokio::spawn(async move {
                            if !step.at.is_zero() {
                                tokio::time::sleep(step.at).await;
                            }
                            // THE decision under test, driven through the
                            // real production function.
                            let (w, h) = startup_demand_decision(WINDOW_ID, step.inputs);
                            let flag = if (w, h) == (LOWEST_W, LOWEST_H) {
                                "   <-- LOWEST LAYER"
                            } else {
                                ""
                            };
                            println!(
                                "[t={:>5}ms] {} -> requests {w}x{h}{flag}",
                                step.at.as_millis(),
                                step.what
                            );
                            pubc.update_video_dimensions(TrackDimension(w, h));
                        });
                    }
                }
            }
        }
    });

    // ---- publisher peer --------------------------------------------------
    let (pub_room, mut pub_events) = Room::connect(
        &url,
        &token_for("petal-299-pub", &room_name),
        RoomOptions::default(),
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("publisher connect failed: {e}");
        std::process::exit(1);
    });
    tokio::spawn(async move { while pub_events.recv().await.is_some() {} });
    let local_track = publish_share(&pub_room, stop.clone(), inject_delay_ms, noise).await;
    println!(
        "[t={:>5}ms] published petal-window-{WINDOW_ID}\n",
        started.elapsed().as_millis()
    );
    if inject_delay_ms > 0 {
        println!("  CONTROL: injecting {inject_delay_ms}ms between stamp and capture_frame\n");
    }

    // #613: sample the stage counters at the steady-state boundary and again
    // at the end, so every per-stage number is a delta over the same window
    // the latency percentiles are taken from.
    tokio::time::sleep(Duration::from_millis(steady_state_after_ms as u64)).await;
    let mut stats_start = StageCounters::default();
    sample_outbound(&local_track, &mut stats_start).await;
    if let Some(rt) = remote_track.lock().unwrap().clone() {
        sample_inbound(&rt, &mut stats_start).await;
    }

    tokio::time::sleep(Duration::from_secs(seconds)).await;

    let mut stats_end = StageCounters::default();
    sample_outbound(&local_track, &mut stats_end).await;
    if let Some(rt) = remote_track.lock().unwrap().clone() {
        sample_inbound(&rt, &mut stats_end).await;
    }
    stop.store(true, Ordering::Relaxed);

    let obs = observations.lock().unwrap().clone();
    let ok = report(&obs, mode, &ladder_rungs);
    report_latency(
        &obs,
        steady_state_after_ms,
        inject_delay_ms,
        &stats_start,
        &stats_end,
        &ladder_rungs,
    );

    sub_room.close().await.ok();
    pub_room.close().await.ok();

    if !ok {
        std::process::exit(1);
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn summarize(label: &str, mut v: Vec<f64>) {
    if v.is_empty() {
        println!("  {label:<22}      n=0   <no samples>");
        return;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let avg = v.iter().sum::<f64>() / n as f64;
    println!(
        "  {label:<22} n={n:<5} avg={avg:7.1}  p50={:7.1}  p95={:7.1}  min={:7.1}  max={:7.1}",
        pct(&v, 0.50),
        pct(&v, 0.95),
        v[0],
        v[n - 1]
    );
}

/// #613: the deliverable. Every line names its own measurement method, and
/// anything obtained by subtraction is labelled RESIDUAL rather than
/// presented as a measured stage.
fn report_latency(
    obs: &[FrameObservation],
    steady_after_ms: u128,
    inject_delay_ms: u64,
    start: &StageCounters,
    end: &StageCounters,
    rungs: &[(String, u32, u32)],
) {
    println!("\n=== #613 end-to-end latency (capture_frame stamp -> decoded frame in-process) ===");
    let missing = obs.iter().filter(|o| o.latency_ms.is_none()).count();
    if missing > 0 {
        println!(
            "  WARNING: {missing}/{} frames carried NO user_timestamp. The frame-metadata \
             path is not fully engaged; treat every number below as suspect.",
            obs.len()
        );
    }

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
    println!("  (single process, one wall clock -- no cross-machine offset correction needed)");
    summarize(&format!("ramp <{steady_after_ms}ms"), ramp);
    summarize(&format!("STEADY >={steady_after_ms}ms"), steady);

    // Split by decoded layer: a latency difference between layers is
    // pixel-proportional cost, and it is measured here rather than assumed.
    // Buckets come from the LIVE ladder's rung widths, so alternate ladders
    // are not silently bucketed against a fixed q/h/f geometry.
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

    println!("\n=== #613 per-stage attribution (RTCStats deltas over the steady window) ===");
    println!("  method: livekit `track.get_stats()` cumulative counters, sampled at the");
    println!("          steady-state boundary and at end of run, differenced, divided by");
    println!("          the matching event count over the same window.");
    if let Some(imp) = end.encoder_impl {
        println!("  encoder implementation  : {imp}");
    }
    let d_layer_frames_enc = end
        .layer_frames_encoded
        .saturating_sub(start.layer_frames_encoded) as u64;
    let d_source_frames_enc = end
        .source_frames_encoded
        .saturating_sub(start.source_frames_encoded) as u64;
    let d_frames_dec = end.frames_decoded.saturating_sub(start.frames_decoded) as u64;
    let d_pkts = end.packets_sent.saturating_sub(start.packets_sent);
    let d_jb_emitted = end
        .jitter_buffer_emitted_count
        .saturating_sub(start.jitter_buffer_emitted_count);
    let d_assembled = end
        .frames_assembled_from_multiple_packets
        .saturating_sub(start.frames_assembled_from_multiple_packets);

    let encode = per_unit_ms(
        end.total_encode_time - start.total_encode_time,
        d_source_frames_enc,
    );
    let send_delay = per_unit_ms(
        end.total_packet_send_delay - start.total_packet_send_delay,
        d_pkts,
    );
    let assembly = per_unit_ms(
        end.total_assembly_time - start.total_assembly_time,
        d_assembled,
    );
    let jitter_buf = per_unit_ms(
        end.jitter_buffer_delay - start.jitter_buffer_delay,
        d_jb_emitted,
    );
    let jitter_target = per_unit_ms(
        end.jitter_buffer_target_delay - start.jitter_buffer_target_delay,
        d_jb_emitted,
    );
    let jitter_min = per_unit_ms(
        end.jitter_buffer_minimum_delay - start.jitter_buffer_minimum_delay,
        d_jb_emitted,
    );
    let decode = per_unit_ms(end.total_decode_time - start.total_decode_time, d_frames_dec);
    let processing = per_unit_ms(
        end.total_processing_delay - start.total_processing_delay,
        d_frames_dec,
    );
    let inter_frame = per_unit_ms(
        end.total_inter_frame_delay - start.total_inter_frame_delay,
        d_frames_dec,
    );

    println!(
        "  encode      (ms/source frame, total across simulcast layers) : {}",
        fmt_ms(encode)
    );
    println!("  packet send (ms/packet)                             : {}", fmt_ms(send_delay));
    println!("  assembly    (ms/multi-packet frame)                 : {}", fmt_ms(assembly));
    println!("  jitter buf  (ms/frame, actual)                      : {}", fmt_ms(jitter_buf));
    println!("  jitter buf  (ms/frame, target)                      : {}", fmt_ms(jitter_target));
    println!("  jitter buf  (ms/frame, minimum)                     : {}", fmt_ms(jitter_min));
    println!("  decode      (ms/frame)                              : {}", fmt_ms(decode));
    println!("  processing  (ms/frame, pkt-received -> decoded)     : {}", fmt_ms(processing));
    println!("  inter-frame (ms/frame, cadence not latency)         : {}", fmt_ms(inter_frame));
    println!(
        "  counts: layer_frames_encoded={d_layer_frames_enc} \
         source_frames_encoded={d_source_frames_enc} packets_sent={d_pkts} \
         frames_decoded={d_frames_dec} jb_emitted={d_jb_emitted} assembled={d_assembled}"
    );

    if inject_delay_ms > 0 {
        println!(
            "\n  CONTROL EXPECTATION: steady-state latency should exceed an uninjected run \
             by ~{inject_delay_ms}ms. If it does not, the instrument is blind."
        );
    }
}

fn report(obs: &[FrameObservation], mode: Mode, rungs: &[(String, u32, u32)]) -> bool {
    println!("\n=== decoded-layer timeline ===");
    if obs.is_empty() {
        println!("  <no frames decoded>");
        println!("\n=== RESULT ===\n  INCONCLUSIVE -- no frames, nothing measured.");
        return false;
    }

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
    let initial_layer = layer_name(rungs, first.width);
    // "Sharp" is the source resolution -- the `f` layer, what the user is
    // actually waiting for.
    let first_sharp = obs.iter().find(|o| o.width >= SOURCE_WIDTH);

    // Cadence over the first 10s, which is the window #299 asks about.
    let window_ms = 10_000u128;
    let in_window: Vec<&FrameObservation> = obs.iter().filter(|o| o.at_ms <= window_ms).collect();
    let fps_10s = match (in_window.first(), in_window.last()) {
        (Some(first), Some(last)) if in_window.len() >= 2 => {
            let span_ms = last.at_ms.saturating_sub(first.at_ms);
            if span_ms > 0 {
                (in_window.len() - 1) as f64 * 1000.0 / span_ms as f64
            } else {
                0.0
            }
        }
        _ => 0.0,
    };

    println!("\n=== MEASUREMENTS ===");
    println!(
        "  first presented frame     : t={}ms  {}x{}",
        first.at_ms, first.width, first.height
    );
    println!(
        "  initial layer             : {initial_layer}  ({}x{})",
        first.width, first.height
    );
    match first_sharp {
        Some(o) => println!(
            "  time to first SHARP frame : {}ms  (source {SOURCE_WIDTH}x{SOURCE_HEIGHT})",
            o.at_ms
        ),
        None => println!(
            "  time to first SHARP frame : NEVER within the run (source {SOURCE_WIDTH}x{SOURCE_HEIGHT})"
        ),
    }
    println!("  decoded fps over 10s      : {fps_10s:.1}  (source drives {SOURCE_FPS:.0})");

    // ---- assertions ------------------------------------------------------
    // #299's own gate: the initial layer must not be the ladder's BOTTOM rung
    // (whatever that rung is under the live ladder -- not hard-coded to a
    // fixed RID), and the startup cadence must meet the 30fps floor every
    // non-control mode promises. The positive control instead verifies only
    // that its pin selected the bottom layer. Allow one frame period of slack
    // on the cadence -- the source ceiling is SOURCE_FPS, so demanding
    // strictly more is unmeetable.
    let fps_floor = SOURCE_FPS - 1.5;
    let bottom_width = rungs.first().map(|r| r.1).unwrap_or(0);
    let rid_ok = first.width > bottom_width;
    let pin_lowest_ok = first.width == bottom_width;
    let fps_ok = fps_10s >= fps_floor;

    println!("\n=== ASSERTIONS ===");
    if mode.is_control() {
        println!(
            "  initial layer == bottom   : {}  (got {initial_layer}, bottom rung width {bottom_width})",
            if pin_lowest_ok { "PASS" } else { "FAIL" }
        );
        println!("  fps over 10s              : {fps_10s:.1}  (reported, not a control gate)");
    } else {
        println!(
            "  initial layer != bottom   : {}  (got {initial_layer}, bottom rung width {bottom_width})",
            if rid_ok { "PASS" } else { "FAIL" }
        );
        println!(
            "  fps over 10s >= {fps_floor:.1}      : {}  (got {fps_10s:.1})",
            if fps_ok { "PASS" } else { "FAIL" }
        );
    }

    println!("\n=== RESULT ===");
    if mode.is_control() {
        if !pin_lowest_ok {
            println!(
                "  CONTROL DID NOT TRIP -- the pin did not select the bottom layer, so no\n  \
                 clean reading from any other mode is trustworthy."
            );
            return false;
        }
        println!("  CONTROL OK -- the pin selected the bottom layer.");
        return true;
    }
    if rid_ok && fps_ok {
        println!("  PASS -- startup begins on a usable layer at the promised cadence.");
        true
    } else {
        println!("  FAIL -- #299 reproduces: startup begins degraded.");
        false
    }
}
