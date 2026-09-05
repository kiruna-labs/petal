//! Smoke test for the REAL shipped mic-capture path
//! (`transport::audio::publish_microphone`'s `PlatformAudio` -- see that
//! module's doc comment for why `PlatformAudio`, not `cpal`/hand-rolled
//! CoreAudio, was chosen). Kept as a committed example, same precedent as
//! `publish_probe.rs`/`subscribe_probe.rs`.
//!
//! `audio_probe.rs` (the sibling example in this directory) verifies the
//! LiveKit transport path end-to-end with a deterministic synthetic tone via
//! `NativeAudioSource`, deliberately NOT real mic hardware (ambient mic
//! content isn't a verifiable signal). This probe instead exercises the
//! REAL mic-capture code path this task actually ships
//! (`PlatformAudio::new()` -> device enumeration -> publish a track sourced
//! from `RtcAudioSource::Device`) against whatever real microphone hardware
//! is present, confirming: the platform ADM initializes, at least one real
//! recording device is enumerated, and a live LiveKit audio track publishes
//! from it without erroring -- i.e. the exact call `publish_microphone`
//! makes in the shipped app, not a stand-in.
//!
//! This does NOT assert anything about the captured audio's *content*
//! (no known signal to check against -- whatever's ambient in the room when
//! this runs), only that the real capture+publish pipeline is live and
//! producing packets (checked via `LocalAudioTrack::get_stats()`'s
//! `OutboundRtp` packet counter, the same "read back real stats, don't just
//! trust a preference took effect" rigor `publisher.rs`'s
//! `log_encoder_once` already established for video).
//!
//! Usage: `cargo run --example mic_capture_probe -- <room_name>` (needs
//! LIVEKIT_URL/LIVEKIT_API_KEY/LIVEKIT_API_SECRET in the environment, same
//! as `audio_probe.rs` -- exported directly, not read from `.env`).

use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::stats::RtcStats;
use livekit::PlatformAudio;

#[tokio::main]
async fn main() {
    env_logger::init();

    let room_name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "petal-mic-capture-probe".to_string());
    let url = std::env::var("LIVEKIT_URL").unwrap_or_else(|_| {
        eprintln!("Missing LIVEKIT_URL");
        std::process::exit(1);
    });
    let api_key = std::env::var("LIVEKIT_API_KEY").unwrap_or_else(|_| {
        eprintln!("Missing LIVEKIT_API_KEY");
        std::process::exit(1);
    });
    let api_secret = std::env::var("LIVEKIT_API_SECRET").unwrap_or_else(|_| {
        eprintln!("Missing LIVEKIT_API_SECRET");
        std::process::exit(1);
    });

    println!("Acquiring platform audio (real ADM, real device enumeration)...");
    let audio = match PlatformAudio::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("PlatformAudio::new() failed: {e} -- no audio hardware available in this environment.");
            std::process::exit(1);
        }
    };

    let recording_devices: Vec<_> = audio.recording_devices().collect();
    let playout_devices: Vec<_> = audio.playout_devices().collect();
    println!("Recording devices ({}):", recording_devices.len());
    for d in &recording_devices {
        println!("  [{}] {}", d.index, d.name);
    }
    println!("Playout devices ({}):", playout_devices.len());
    for d in &playout_devices {
        println!("  [{}] {}", d.index, d.name);
    }

    if recording_devices.is_empty() {
        eprintln!("FAILED: PlatformAudio initialized but zero recording devices enumerated.");
        std::process::exit(1);
    }

    use livekit_api::access_token::{AccessToken, VideoGrants};
    let token = AccessToken::with_api_key(&api_key, &api_secret)
        .with_identity("mic-capture-probe")
        .with_name("mic-capture-probe")
        .with_grants(VideoGrants {
            room_join: true,
            room: room_name.clone(),
            can_publish: true,
            can_subscribe: false,
            ..Default::default()
        })
        .to_jwt()
        .expect("token mint failed");

    let mut room_options = RoomOptions::default();
    room_options.auto_subscribe = false;
    let (room, _events) = Room::connect(&url, &token, room_options)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to connect: {e}");
            std::process::exit(1);
        });
    println!("Connected: room='{}' sid={}", room.name(), room.sid().await);

    let track = LocalAudioTrack::create_audio_track("petal-mic-probe", audio.rtc_source());
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
            eprintln!("Failed to publish real mic track: {e}");
            std::process::exit(1);
        });
    println!("Published a REAL microphone track from device capture. Capturing for 5s...");

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let stats = track.get_stats().await.unwrap_or_else(|e| {
        eprintln!("get_stats() failed: {e}");
        std::process::exit(1);
    });

    let mut found_outbound = false;
    for stat in &stats {
        if let RtcStats::OutboundRtp(outbound) = stat {
            if outbound.stream.kind == "audio" {
                found_outbound = true;
                println!(
                    "OutboundRtp(audio): packets_sent={} bytes_sent={}",
                    outbound.sent.packets_sent, outbound.sent.bytes_sent
                );
                if outbound.sent.packets_sent == 0 {
                    eprintln!("FAILED: outbound audio stats exist but zero packets were sent.");
                    std::process::exit(1);
                }
            }
        }
    }

    if !found_outbound {
        eprintln!("FAILED: no OutboundRtp(audio) stats observed after 5s -- capture pipeline not producing packets.");
        std::process::exit(1);
    }

    println!("PASSED: real PlatformAudio device capture is live and producing real outbound RTP audio packets.");

    // Also confirm mute/unmute (the same real call `MicTrack::set_muted`
    // makes) doesn't error against a live track.
    track.mute();
    println!("track.mute() called -- is_muted()={}", track.is_muted());
    track.unmute();
    println!("track.unmute() called -- is_muted()={}", track.is_muted());

    let _ = room.close().await;
}
