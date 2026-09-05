//! Verification harness for the audio task (SPEC.md §4.9), mirroring
//! `publish_probe.rs`/`subscribe_probe.rs`'s own M0 dual-role pattern
//! (kept as a committed example, same precedent those two set -- not
//! deleted after use).
//!
//! Two roles, run as two separate processes joining the same LiveKit room:
//!
//!   cargo run --example audio_probe -- publish <room_name>
//!   cargo run --example audio_probe -- subscribe <room_name>
//!
//! `publish` pushes a KNOWN synthetic 440Hz tone into a `NativeAudioSource`
//! (NOT the real mic/`PlatformAudio` path -- see below for why) as a real
//! LiveKit audio track. `subscribe` joins the same room, subscribes to that
//! track, pulls decoded PCM frames off it via `NativeAudioStream`, and
//! verifies the received audio is: (a) actually arriving (frame count > 0,
//! at the right sample rate), (b) non-silent (RMS well above zero), and (c)
//! actually matches the known 440Hz pattern (dominant-frequency check via a
//! simple Goertzel-style correlation against 440Hz, not just "not silent") --
//! the same "real numbers, not just compiles" rigor as `subscribe_probe.rs`'s
//! embedded-timestamp latency measurement.
//!
//! ## Why a synthetic tone via `NativeAudioSource`, not real mic capture via
//! `PlatformAudio`, for this end-to-end round-trip check
//!
//! This proves the thing that's actually in question -- the LiveKit
//! transport path (Opus encode -> RTP -> SFU forward -> Opus decode -> PCM
//! frames delivered to the app) -- deterministically and without depending
//! on this environment having a real, permitted microphone with known
//! content (ambient noise isn't a verifiable signal). `PlatformAudio`
//! mic-capture itself (the actual shipped code path in
//! `transport::audio::publish_microphone`) is separately smoke-tested by
//! `mic_capture_probe.rs` (device enumeration + a few seconds of capture),
//! which this same directory also has -- see that file for why that one
//! IS exercised against real hardware where available.
//!
//! Reads LIVEKIT_URL/LIVEKIT_API_KEY/LIVEKIT_API_SECRET directly from the
//! process environment (export them in the shell before running -- this
//! probe deliberately does NOT read `apps/desktop/.env`, since the task
//! this probe verifies was explicitly told not to touch that file).

use std::f32::consts::PI;

use futures::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;

const SAMPLE_RATE: u32 = 48_000;
const NUM_CHANNELS: u32 = 1;
const TONE_HZ: f32 = 440.0;
const FRAME_MS: u32 = 10; // WebRTC's standard 10ms audio frame size
const SAMPLES_PER_FRAME: u32 = SAMPLE_RATE / 1000 * FRAME_MS;

fn env_or_exit(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("Missing required env var {name} (export it before running this probe).");
        std::process::exit(1);
    })
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let mut args = std::env::args().skip(1);
    let role = args.next().unwrap_or_default();
    let room_name = args
        .next()
        .unwrap_or_else(|| "petal-audio-probe".to_string());

    let url = env_or_exit("LIVEKIT_URL");
    let api_key = env_or_exit("LIVEKIT_API_KEY");
    let api_secret = env_or_exit("LIVEKIT_API_SECRET");

    match role.as_str() {
        "publish" => run_publish(&url, &api_key, &api_secret, &room_name).await,
        "subscribe" => run_subscribe(&url, &api_key, &api_secret, &room_name).await,
        _ => {
            eprintln!("Usage: audio_probe <publish|subscribe> [room_name]");
            std::process::exit(1);
        }
    }
}

fn mint_token(
    api_key: &str,
    api_secret: &str,
    identity: &str,
    room: &str,
    can_publish: bool,
    can_subscribe: bool,
) -> String {
    use livekit_api::access_token::{AccessToken, VideoGrants};
    AccessToken::with_api_key(api_key, api_secret)
        .with_identity(identity)
        .with_name(identity)
        .with_grants(VideoGrants {
            room_join: true,
            room: room.to_string(),
            can_publish,
            can_subscribe,
            ..Default::default()
        })
        .to_jwt()
        .expect("token mint failed")
}

async fn run_publish(url: &str, api_key: &str, api_secret: &str, room_name: &str) {
    let token = mint_token(
        api_key,
        api_secret,
        "audio-probe-publisher",
        room_name,
        true,
        false,
    );

    let mut room_options = RoomOptions::default();
    room_options.auto_subscribe = false;
    let (room, _events) = Room::connect(url, &token, room_options)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to connect: {e}");
            std::process::exit(1);
        });
    println!(
        "Publisher connected: room='{}' sid={}",
        room.name(),
        room.sid().await
    );

    let source = NativeAudioSource::new(
        AudioSourceOptions::default(),
        SAMPLE_RATE,
        NUM_CHANNELS,
        1000, // queue_size_ms
    );
    let track = LocalAudioTrack::create_audio_track(
        "audio-probe-tone",
        RtcAudioSource::Native(source.clone()),
    );

    room.local_participant()
        .publish_track(
            LocalTrack::Audio(track),
            TrackPublishOptions {
                source: TrackSource::Microphone,
                dtx: true,
                red: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to publish audio track: {e}");
            std::process::exit(1);
        });
    println!(
        "Published a real LiveKit audio track (Opus). Streaming a known {TONE_HZ}Hz tone at {SAMPLE_RATE}Hz for 12s..."
    );

    // Generate and push a continuous 440Hz sine wave, 10ms frames, for 12s --
    // long enough for the subscriber to gather a solid sample and for Opus's
    // encoder/DTX/jitter-buffer warmup to settle out of the measurement.
    let total_frames = (12_000 / FRAME_MS) as usize;
    let mut phase: f32 = 0.0;
    let phase_step = 2.0 * PI * TONE_HZ / SAMPLE_RATE as f32;

    for i in 0..total_frames {
        let mut frame = AudioFrame::new(SAMPLE_RATE, NUM_CHANNELS, SAMPLES_PER_FRAME);
        {
            let data = frame.data.to_mut();
            for sample in data.iter_mut() {
                // Half-scale amplitude sine wave -> i16 PCM.
                let v = (phase.sin() * (i16::MAX as f32) * 0.5) as i16;
                *sample = v;
                phase += phase_step;
                if phase > 2.0 * PI {
                    phase -= 2.0 * PI;
                }
            }
        }
        source.capture_frame(&frame).await.unwrap_or_else(|e| {
            eprintln!("capture_frame failed: {e:?}");
        });
        if i % 100 == 0 {
            println!(
                "  pushed {i}/{total_frames} 10ms frames ({:.1}s elapsed)",
                i as f32 * FRAME_MS as f32 / 1000.0
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(FRAME_MS as u64)).await;
    }

    println!("Publisher done pushing tone. Holding connection open 3 more seconds for drain...");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let _ = room.close().await;
}

async fn run_subscribe(url: &str, api_key: &str, api_secret: &str, room_name: &str) {
    let token = mint_token(
        api_key,
        api_secret,
        "audio-probe-subscriber",
        room_name,
        false,
        true,
    );

    let mut room_options = RoomOptions::default();
    room_options.auto_subscribe = true;
    let (room, mut events) = Room::connect(url, &token, room_options)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to connect: {e}");
            std::process::exit(1);
        });
    println!(
        "Subscriber connected: room='{}' sid={}",
        room.name(),
        room.sid().await
    );
    println!("Waiting for a published audio track (up to 30s)...");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut audio_track: Option<RemoteAudioTrack> = None;

    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                if let RoomEvent::TrackSubscribed { track, participant, .. } = event {
                    if let RemoteTrack::Audio(a) = track {
                        println!("Subscribed to audio track from '{}' (sid={})", participant.identity(), a.sid());
                        audio_track = Some(a);
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
        }
        if audio_track.is_some() {
            break;
        }
    }

    let Some(audio_track) = audio_track else {
        eprintln!("FAILED: no audio track subscribed within 30s.");
        std::process::exit(1);
    };

    let rtc_track = audio_track.rtc_track();
    let mut stream = NativeAudioStream::new(rtc_track, SAMPLE_RATE as i32, NUM_CHANNELS as i32);

    println!("Pulling decoded PCM frames for up to 10s...");
    let mut all_samples: Vec<i16> = Vec::new();
    let mut frame_count: u64 = 0;
    let mut observed_sample_rate: u32 = 0;
    let mut observed_channels: u32 = 0;
    let collect_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break };
                frame_count += 1;
                observed_sample_rate = frame.sample_rate;
                observed_channels = frame.num_channels;
                all_samples.extend_from_slice(&frame.data);
            }
            _ = tokio::time::sleep_until(collect_deadline) => break,
        }
    }

    if frame_count == 0 || all_samples.is_empty() {
        eprintln!("FAILED: subscribed to the track but received zero decoded audio frames.");
        std::process::exit(1);
    }

    // --- Measurement 1: non-silence (RMS) ---
    let sum_sq: f64 = all_samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_sq / all_samples.len() as f64).sqrt();

    // --- Measurement 2: dominant-frequency correlation against 440Hz
    // (Goertzel algorithm -- detects the target frequency's energy without
    // a full FFT, standard technique for "is this one known tone present").
    let sr = if observed_sample_rate > 0 {
        observed_sample_rate
    } else {
        SAMPLE_RATE
    };
    let goertzel_power = |samples: &[i16], target_hz: f32, sample_rate: u32| -> f64 {
        let n = samples.len();
        if n == 0 {
            return 0.0;
        }
        let k = (0.5 + (n as f32 * target_hz) / sample_rate as f32).floor();
        let w = 2.0 * PI as f64 * (k as f64) / (n as f64);
        let cw = w.cos();
        let coeff = 2.0 * cw;
        let (mut q1, mut q2) = (0.0f64, 0.0f64);
        for &s in samples {
            let q0 = coeff * q1 - q2 + s as f64;
            q2 = q1;
            q1 = q0;
        }
        q1 * q1 + q2 * q2 - q1 * q2 * coeff
    };

    // Use a representative analysis window (last 2s of audio collected, so
    // Opus/jitter-buffer startup transients are excluded).
    let window_len = (sr as usize * 2).min(all_samples.len());
    let window = &all_samples[all_samples.len() - window_len..];
    let power_440 = goertzel_power(window, 440.0, sr);
    let power_1000 = goertzel_power(window, 1000.0, sr); // an off-target reference frequency
    let power_2500 = goertzel_power(window, 2500.0, sr);

    println!("\n=== Audio round-trip results ===");
    println!("  frames received: {frame_count}");
    println!("  total samples: {}", all_samples.len());
    println!("  observed sample_rate={sr}Hz channels={observed_channels}");
    println!("  RMS amplitude: {rms:.1} (i16 full-scale = 32767)");
    println!("  Goertzel power @440Hz (target): {power_440:.3e}");
    println!("  Goertzel power @1000Hz (off-target ref): {power_1000:.3e}");
    println!("  Goertzel power @2500Hz (off-target ref): {power_2500:.3e}");

    let non_silent = rms > 500.0;
    let dominant_440 = power_440 > power_1000 * 5.0 && power_440 > power_2500 * 5.0;

    println!(
        "\n  non-silent: {} | dominant frequency matches injected 440Hz tone: {}",
        if non_silent { "YES" } else { "NO" },
        if dominant_440 { "YES" } else { "NO" }
    );

    if !non_silent || !dominant_440 {
        eprintln!("FAILED: received audio did not match the expected injected tone.");
        std::process::exit(1);
    }
    println!("PASSED: real audio frames, non-silent, matching the known 440Hz injected pattern.");

    let _ = room.close().await;
}
