//! #654 spike, questions 3 and 5: does AI chat's own `cpal` mic capture
//! coexist with LiveKit's audio device module (ADM) holding the real
//! microphone (Q3), and does AI chat's local speaker playback leak into the
//! ADM's published mic track (Q5)?
//!
//! Neither question needs a live Gemini session — both are pure local-audio
//! hardware questions — so this probe drives the REAL production code on
//! both sides directly rather than waiting on a model:
//!   - the ADM mic publish is the exact `PlatformAudio` + `publish_track`
//!     sequence `mic_capture_probe.rs` already validates (Q3's baseline);
//!   - the AI-chat side calls `desktop_lib::ai_chat::audio::
//!     start_local_microphone_capture` / `play` directly — the same
//!     functions `session.rs` calls when a real turn opens.
//!
//! Two roles, two processes, one local room:
//!
//!   cargo run --example q3_q5_coexistence_probe -- publish <room_name>
//!   cargo run --example q3_q5_coexistence_probe -- listen  <room_name>
//!
//! `publish` runs a fixed timeline against real hardware:
//!   t=0    PlatformAudio::new() + publish a REAL mic track (ADM)
//!   t=3s   start_local_microphone_capture() -- Q3: does this error, and
//!          does the ADM's own outbound packet count keep climbing after?
//!   t=6s   ai_chat::audio::play(<known 880Hz tone, ~4s>) through real
//!          speakers -- Q5: does the ADM's PUBLISHED mic track pick it up?
//!   t=12s  stop_local_microphone_capture(), stop_playback(), report ADM
//!          stats one last time, close.
//!
//! `listen` subscribes to the publisher's REAL mic track for the whole
//! window and prints per-second RMS + 880Hz Goertzel power, so the tone's
//! appearance (or absence) in the PUBLISHED MIC is directly readable against
//! the publisher's own timeline -- if the tone shows up starting ~t=6s,
//! that is Q5's leak signal.
//!
//! Needs LIVEKIT_URL/LIVEKIT_API_KEY/LIVEKIT_API_SECRET in the environment
//! (same convention as `audio_probe.rs`/`mic_capture_probe.rs`) and REAL
//! microphone + speaker hardware -- run it on the actual machine, not CI.
//! Q5 in particular is an ACOUSTIC test: run it with real speakers, not
//! headphones, or a "no leak" result proves nothing (headphone output can
//! never reach the mic).

use std::f32::consts::PI;
use std::time::Duration;

use futures::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::stats::RtcStats;
use livekit::PlatformAudio;
use livekit_api::access_token::{AccessToken, VideoGrants};

const TONE_HZ: f32 = 880.0;
const AI_CHAT_DOWNLINK_RATE: u32 = 24_000; // desktop_lib::ai_chat::audio::DOWNLINK_RATE

fn env_or_exit(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("Missing {name}");
        std::process::exit(1);
    })
}

fn mint(api_key: &str, api_secret: &str, room: &str, identity: &str, publish: bool) -> String {
    AccessToken::with_api_key(api_key, api_secret)
        .with_identity(identity)
        .with_name(identity)
        .with_grants(VideoGrants {
            room_join: true,
            room: room.to_string(),
            can_publish: publish,
            can_subscribe: !publish,
            ..Default::default()
        })
        .to_jwt()
        .expect("token mint failed")
}

async fn print_outbound_audio_stats(track: &LocalAudioTrack, label: &str) -> u64 {
    let stats = track.get_stats().await.unwrap_or_default();
    for stat in &stats {
        if let RtcStats::OutboundRtp(outbound) = stat {
            if outbound.stream.kind == "audio" {
                println!(
                    "  [{label}] ADM outbound audio: packets_sent={}",
                    outbound.sent.packets_sent
                );
                return outbound.sent.packets_sent;
            }
        }
    }
    println!("  [{label}] ADM outbound audio: NO STATS FOUND");
    0
}

async fn run_publish(url: &str, api_key: &str, api_secret: &str, room_name: &str) {
    println!("[publish] t=0  Acquiring PlatformAudio (real ADM)...");
    let audio = match PlatformAudio::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("PlatformAudio::new() failed: {e} -- no audio hardware in this environment.");
            std::process::exit(1);
        }
    };
    if audio.recording_devices().count() == 0 {
        eprintln!("FAILED: zero recording devices -- Q3/Q5 need real mic hardware.");
        std::process::exit(1);
    }

    let token = mint(api_key, api_secret, room_name, "q3q5-publisher", true);
    let mut room_options = RoomOptions::default();
    room_options.auto_subscribe = false;
    let (room, _events) = Room::connect(url, &token, room_options)
        .await
        .unwrap_or_else(|e| {
            eprintln!("connect failed: {e}");
            std::process::exit(1);
        });
    println!("[publish] connected: room='{}'", room.name());

    let track = LocalAudioTrack::create_audio_track("petal-q3q5-mic", audio.rtc_source());
    room.local_participant()
        .publish_track(
            LocalTrack::Audio(track.clone()),
            TrackPublishOptions {
                source: TrackSource::Microphone,
                dtx: true,
                red: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| {
            eprintln!("publish failed: {e}");
            std::process::exit(1);
        });
    println!("[publish] REAL mic track published via ADM (this is the meeting mic path).");

    tokio::time::sleep(Duration::from_secs(3)).await;
    let baseline = print_outbound_audio_stats(&track, "t=3s pre-ai_chat-capture").await;
    if baseline == 0 {
        eprintln!("FAILED: ADM produced zero packets before AI chat even touched audio.");
        std::process::exit(1);
    }

    // Q3 (does AI chat's own cpal capture coexist with the ADM's live mic
    // publish?) is macOS-only: `start_local_microphone_capture` feeds the AI
    // session engine, which is gated to macOS in `ai_chat/mod.rs`. On other
    // platforms the publish timeline skips straight to Q5 (speaker leak).
    #[cfg(target_os = "macos")]
    {
        println!("[publish] t=3s  Q3: starting desktop_lib::ai_chat::audio::start_local_microphone_capture() alongside the live ADM publish...");
        desktop_lib::ai_chat::audio::start_local_microphone_capture();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let after_capture =
            print_outbound_audio_stats(&track, "t=5s post-ai_chat-capture-start").await;
        if after_capture <= baseline {
            eprintln!(
                "Q3 CONCERN: ADM outbound packet count did not increase after AI chat opened its own \
                 capture stream ({baseline} -> {after_capture}) -- possible starvation/contention."
            );
        } else {
            println!(
                "[publish] Q3: ADM kept producing packets after AI chat's capture opened \
                 ({baseline} -> {after_capture}). No observed contention."
            );
        }
    }

    println!(
        "[publish] t=6s  Q5: playing a {}s Hz tone through REAL SPEAKERS via \
         ai_chat::audio::play() while the ADM mic is still publishing and AI chat's \
         own capture is still open...",
        TONE_HZ
    );
    let tone_pcm = generate_tone_pcm16(TONE_HZ, AI_CHAT_DOWNLINK_RATE, Duration::from_secs(4));
    desktop_lib::ai_chat::audio::play(&tone_pcm);

    // Hold the whole scenario open long enough for the listener to capture
    // clearly-separated before/during/after windows.
    for elapsed in [7u64, 9, 11] {
        tokio::time::sleep(Duration::from_secs(2)).await;
        print_outbound_audio_stats(&track, &format!("t={elapsed}s")).await;
    }

    println!("[publish] t=13s  tearing down: stop_playback(), stop_local_microphone_capture()");
    desktop_lib::ai_chat::audio::stop_playback();
    desktop_lib::ai_chat::audio::stop_local_microphone_capture();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let final_count = print_outbound_audio_stats(&track, "t=13.5s final").await;
    println!("[publish] Done. Final ADM packet count: {final_count}");

    let _ = room.close().await;
}

async fn run_listen(url: &str, api_key: &str, api_secret: &str, room_name: &str) {
    let token = mint(api_key, api_secret, room_name, "q3q5-listener", false);
    let (room, mut events) = Room::connect(url, &token, RoomOptions::default())
        .await
        .unwrap_or_else(|e| {
            eprintln!("connect failed: {e}");
            std::process::exit(1);
        });
    println!("[listen] connected, waiting for the publisher's mic track...");

    let track = loop {
        match tokio::time::timeout(Duration::from_secs(20), events.recv()).await {
            Ok(Some(RoomEvent::TrackSubscribed { track, .. })) => {
                if let RemoteTrack::Audio(audio_track) = track {
                    println!("[listen] subscribed to remote audio track.");
                    break audio_track;
                }
            }
            Ok(Some(_)) => continue,
            _ => {
                eprintln!("FAILED: no audio track subscribed within 20s -- is `publish` running?");
                std::process::exit(1);
            }
        }
    };

    let rtc_track = track.rtc_track();
    let mut stream = NativeAudioStream::new(rtc_track, 48_000, 1);

    println!("[listen] capturing for 15s, printing per-second RMS + {TONE_HZ}Hz power (this is the PUBLISHED MIC track -- any tone appearing here leaked in acoustically or otherwise)...");
    let mut second_buf: Vec<i16> = Vec::new();
    let mut second = 0u64;
    let mut sample_rate = 48_000u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut any_leak = false;

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), stream.next()).await {
            Ok(Some(frame)) => {
                sample_rate = frame.sample_rate;
                second_buf.extend_from_slice(&frame.data);
                let target = (sample_rate as usize).max(1);
                while second_buf.len() >= target {
                    let chunk: Vec<i16> = second_buf.drain(..target).collect();
                    let rms = rms_i16(&chunk);
                    let tone_power = goertzel_power(&chunk, TONE_HZ, sample_rate);
                    let leaking = tone_power > 5.0e11; // well above the noise floor seen in audio_probe.rs's off-target references
                    if leaking {
                        any_leak = true;
                    }
                    println!(
                        "  [listen] t={second}s  rms={rms:.1}  {TONE_HZ}Hz_power={tone_power:.3e}{}",
                        if leaking { "  <-- TONE PRESENT IN PUBLISHED MIC" } else { "" }
                    );
                    second += 1;
                }
            }
            Ok(None) => break,
            Err(_) => continue, // timeout tick, keep waiting for more frames
        }
    }

    println!("\n==== Q5 result ====");
    if any_leak {
        println!(
            "LEAK DETECTED: the {TONE_HZ}Hz tone played locally via ai_chat::audio::play() was \
             observed in the ADM's PUBLISHED mic track. The room would hear the assistant's \
             voice twice (direct + mic echo). #657 needs sharer-side mic ducking while the \
             assistant is speaking."
        );
    } else {
        println!(
            "NO LEAK observed in this run. Caveat: this is an acoustic test -- confirm this \
             machine used real speakers (not headphones) for the result to mean anything; \
             headphone output cannot reach the mic by definition and would give a false PASS."
        );
    }

    let _ = room.close().await;
}

fn generate_tone_pcm16(freq_hz: f32, sample_rate: u32, duration: Duration) -> Vec<u8> {
    let total = (sample_rate as f32 * duration.as_secs_f32()) as usize;
    let step = 2.0 * PI * freq_hz / sample_rate as f32;
    let samples: Vec<f32> = (0..total).map(|i| (i as f32 * step).sin() * 0.5).collect();
    desktop_lib::ai_chat::audio::f32_to_pcm16(&samples)
}

fn rms_i16(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt()
}

/// Same Goertzel-style dominant-frequency check `audio_probe.rs` uses.
fn goertzel_power(samples: &[i16], target_hz: f32, sample_rate: u32) -> f64 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = (0.5 + (n as f32 * target_hz) / sample_rate as f32) as usize;
    let omega = 2.0 * PI as f64 * k as f64 / n as f64;
    let coeff = 2.0 * omega.cos();
    let (mut s0, mut s1, mut s2);
    s1 = 0.0;
    s2 = 0.0;
    for &sample in samples {
        s0 = sample as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let role = std::env::args().nth(1).unwrap_or_default();
    let room_name = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "petal-q3-q5-probe".to_string());
    let url = env_or_exit("LIVEKIT_URL");
    let api_key = env_or_exit("LIVEKIT_API_KEY");
    let api_secret = env_or_exit("LIVEKIT_API_SECRET");

    match role.as_str() {
        "publish" => run_publish(&url, &api_key, &api_secret, &room_name).await,
        "listen" => run_listen(&url, &api_key, &api_secret, &room_name).await,
        _ => {
            eprintln!("usage: q3_q5_coexistence_probe <publish|listen> [room_name]");
            std::process::exit(2);
        }
    }
}
