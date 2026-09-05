//! The assistant's voice, published into the room as a real LiveKit audio
//! track (#657).
//!
//! ## Why a published track and not just local playback
//!
//! [`super::audio::play`] renders Gemini's reply on the HOST's speakers. That is
//! the whole story for the host and no story at all for everyone else: a peer
//! who asked the question hears silence while the host hears the answer. The
//! contract has always said otherwise — `contracts/petal-contracts.json` pins
//! `petal-ai-window-<id>` vectors and five consumer sites classify a track that,
//! until now, no code published.
//!
//! Local playback stays: LiveKit does not loop a participant's own published
//! track back to them, so the host would otherwise go deaf to the assistant it
//! is hosting.
//!
//! ## Track source is deliberately not `Microphone`
//!
//! `presence::remote_mic_muted` reports a participant's microphone state from
//! the FIRST `TrackSource::Microphone` publication it finds. Publishing the
//! assistant under that source would let it masquerade as the host's mic —
//! their roster entry would follow the assistant's mute state, and the rule that
//! "muting your microphone must not mute the assistant" (`wire::AI_TRACK_PREFIX`)
//! would break from the other end.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use livekit::options::TrackPublishOptions;
use livekit::prelude::{LocalAudioTrack, LocalTrack, Room, TrackSource};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc;

use super::audio::DOWNLINK_RATE;

/// Buffering inside the source, in ms. Must be a multiple of 10 (the SDK
/// asserts). Deep enough that a scheduling hiccup does not gap the voice,
/// shallow enough that a barge-in clears quickly.
const QUEUE_SIZE_MS: u32 = 200;
/// Chunks of assistant audio waiting for the pump. Gemini emits a reply faster
/// than real time and `capture_frame` paces at real time, so this has to hold a
/// whole utterance; at Gemini's typical chunk size that is minutes of speech.
const CHUNK_QUEUE: usize = 512;

struct Voice {
    window_id: u32,
    track_name: String,
    track: LocalAudioTrack,
    room: Arc<Room>,
    chunks: mpsc::Sender<(u64, Vec<i16>)>,
    source: NativeAudioSource,
    /// Bumped on barge-in. Chunks tagged with a superseded epoch are discarded
    /// by the pump instead of played after the user has interrupted.
    epoch: Arc<AtomicU64>,
}

fn voice() -> &'static Mutex<Option<Voice>> {
    static VOICE: OnceLock<Mutex<Option<Voice>>> = OnceLock::new();
    VOICE.get_or_init(|| Mutex::new(None))
}

/// The track name currently published, if any. `None` when the assistant is
/// audible only on this machine (no room).
pub fn published_track_name() -> Option<String> {
    voice()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|v| v.track_name.clone()))
}

fn publish_options() -> TrackPublishOptions {
    TrackPublishOptions {
        // See the module docs: NOT `Microphone`.
        source: TrackSource::ScreenshareAudio,
        // The assistant's voice IS the payload; DTX clipping the attack of a
        // syllable costs more than the bandwidth it saves.
        dtx: false,
        // Matches the microphone path — RED is off for subscriber interop.
        red: false,
        ..Default::default()
    }
}

/// Start publishing the assistant's voice for `window_id`. Called when the
/// session goes live. A no-op when a track for the same window is already up.
///
/// Not being in a room is not an error: the session still runs and the host
/// still hears the assistant locally.
pub fn start(app: &AppHandle, window_id: u32) {
    let already = voice()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|v| v.window_id));
    if already == Some(window_id) {
        return;
    }
    stop();

    let Some((connection, _identity)) = app
        .try_state::<crate::session::SessionState>()
        .and_then(|state| state.control_channel_snapshot())
    else {
        log::info!("ai_chat: no room -- the assistant's voice stays on this machine");
        return;
    };
    let room = connection.room();
    let track_name = super::wire::ai_track_name(window_id);

    // Gemini sends 24 kHz mono PCM16, so the source is created at exactly that
    // rate and nothing is resampled on the way out. No echo cancellation /
    // noise suppression / AGC: this is already clean synthesized speech, and
    // the APM would chew it.
    let source = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        },
        DOWNLINK_RATE,
        1,
        QUEUE_SIZE_MS,
    );
    let track = LocalAudioTrack::create_audio_track(
        &track_name,
        RtcAudioSource::Native(source.clone()),
    );
    let (chunks_tx, chunks_rx) = mpsc::channel::<(u64, Vec<i16>)>(CHUNK_QUEUE);
    let epoch = Arc::new(AtomicU64::new(0));

    let publish_room = room.clone();
    let publish_track = track.clone();
    let pump_source = source.clone();
    let pump_epoch = epoch.clone();
    let publish_name = track_name.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = publish_room
            .local_participant()
            .publish_track(LocalTrack::Audio(publish_track), publish_options())
            .await
        {
            log::warn!("ai_chat: could not publish the assistant's voice: {e}");
            return;
        }
        log::info!("ai_chat: publishing the assistant's voice as '{publish_name}'");
        pump(pump_source, chunks_rx, pump_epoch).await;
        log::debug!("ai_chat: assistant voice pump for '{publish_name}' exited");
    });

    if let Ok(mut guard) = voice().lock() {
        *guard = Some(Voice {
            window_id,
            track_name,
            track,
            room,
            chunks: chunks_tx,
            source,
            epoch,
        });
    }
}

/// Queue a chunk of assistant audio, exactly as Gemini sends it (PCM16, 24 kHz
/// mono). Silently does nothing when no track is published.
pub fn push(pcm16: &[u8]) {
    let samples: Vec<i16> = pcm16
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    if samples.is_empty() {
        return;
    }
    let queued = {
        let guard = match voice().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(v) = guard.as_ref() else {
            return;
        };
        (v.chunks.clone(), v.epoch.load(Ordering::SeqCst))
    };
    let (chunks, epoch) = queued;
    if chunks.try_send((epoch, samples)).is_err() {
        log::debug!("ai_chat: assistant voice queue full -- dropped a chunk");
    }
}

/// Drop everything queued and in flight. Barge-in: the assistant must stop
/// talking on every listener's machine, not only on the host's.
pub fn clear() {
    if let Ok(guard) = voice().lock() {
        if let Some(v) = guard.as_ref() {
            v.epoch.fetch_add(1, Ordering::SeqCst);
            v.source.clear_buffer();
        }
    }
}

/// Unpublish and stop. Idempotent; safe when nothing is published.
///
/// Dropping the sender is what ends the pump task, so the source's buffer
/// cannot keep draining into the room after teardown.
pub fn stop() {
    let previous = match voice().lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => return,
    };
    let Some(previous) = previous else {
        return;
    };
    previous.epoch.fetch_add(1, Ordering::SeqCst);
    previous.source.clear_buffer();
    let room = previous.room.clone();
    let sid = previous.track.sid();
    let name = previous.track_name.clone();
    drop(previous); // closes `chunks`, which ends the pump
    tauri::async_runtime::spawn(async move {
        if let Err(e) = room.local_participant().unpublish_track(&sid).await {
            log::debug!("ai_chat: unpublishing '{name}' failed: {e}");
        } else {
            log::info!("ai_chat: unpublished the assistant's voice ('{name}')");
        }
    });
}

/// Feed queued PCM into the source. `capture_frame` paces itself against the
/// source's queue, which is what keeps the assistant speaking at real time
/// rather than emptying an entire reply into the encoder at once.
async fn pump(
    source: NativeAudioSource,
    mut chunks: mpsc::Receiver<(u64, Vec<i16>)>,
    epoch: Arc<AtomicU64>,
) {
    while let Some((tagged, samples)) = chunks.recv().await {
        if tagged != epoch.load(Ordering::SeqCst) {
            continue; // superseded by a barge-in
        }
        let samples_per_channel = samples.len() as u32;
        let frame = AudioFrame {
            data: Cow::Owned(samples),
            sample_rate: DOWNLINK_RATE,
            num_channels: 1,
            samples_per_channel,
        };
        if let Err(e) = source.capture_frame(&frame).await {
            log::debug!("ai_chat: assistant voice capture_frame failed: {e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_assistant_is_not_published_as_a_microphone() {
        // `presence::remote_mic_muted` picks the first Microphone-source
        // publication as the participant's mic. If the assistant claimed that
        // source, the host's roster entry would track the assistant's mute
        // state instead of their own.
        let options = publish_options();
        assert_ne!(options.source, TrackSource::Microphone);
        assert_eq!(options.source, TrackSource::ScreenshareAudio);
    }

    #[test]
    fn dtx_is_off_so_the_first_syllable_is_not_clipped() {
        assert!(!publish_options().dtx);
        assert!(!publish_options().red, "RED breaks subscriber interop");
    }

    #[test]
    fn the_queue_size_satisfies_the_sdks_multiple_of_ten_assert() {
        assert_eq!(QUEUE_SIZE_MS % 10, 0);
    }

    #[test]
    fn nothing_is_published_before_a_session_starts() {
        stop();
        assert_eq!(published_track_name(), None);
    }
}
