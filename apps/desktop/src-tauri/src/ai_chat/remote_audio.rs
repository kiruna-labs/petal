//! A REMOTE participant's push-to-talk audio, tapped off their LiveKit track
//! and forwarded to Gemini (#657).
//!
//! ## The bug this module exists to prevent
//!
//! `wire.rs`'s authorization matrix promises that a peer may only claim the
//! floor "for themselves. Otherwise a peer could make the host tap someone
//! else's microphone." The first cut of the feature honoured the *authorization*
//! and then routed the granted floor to `audio::start_capture()` — the HOST's
//! microphone. So when Bob pressed his key, Alice's room was recorded and
//! streamed to Google, Bob's voice reached nothing, and Alice was never asked.
//!
//! The fix is structural, not a condition: a remote speaker's audio can only
//! ever come from *their own already-subscribed audio track*, which is what
//! this module reads. [`super::audio`] owns the local microphone and nothing
//! else. There is no path from here to `cpal`.
//!
//! ## Failing is mandatory
//!
//! If the tap cannot be established the floor claim must FAIL and say so.
//! Falling back to the local microphone would reintroduce exactly the bug
//! above, quietly, at the worst possible moment.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use livekit::prelude::{RemoteAudioTrack, RemoteTrack};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use tauri::{AppHandle, Manager};

use super::audio::{downmix_to_mono, f32_to_pcm16, i16_to_f32, Resampler, UPLINK_RATE};

/// Rate we ask the SDK's sink to deliver at. It is a *request*, not a promise:
/// every frame reports its own `sample_rate` and the resampler is built from
/// that, so a sink that hands back something else still lands at 16 kHz.
const REQUESTED_RATE: i32 = 48_000;
/// How often the pump wakes to notice a stop while the track is silent. Without
/// it a muted holder would leave the task parked on `next()` forever.
const STOP_POLL: Duration = Duration::from_millis(200);

/// Why a remote speaker's audio could not be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapError {
    /// Not joined to a room, so there is no remote track to tap at all.
    NotInRoom,
    /// Nobody in the room answers to that identity.
    ParticipantUnknown,
    /// They publish no microphone track, or it is not subscribed yet.
    NoSubscribedAudioTrack,
}

impl TapError {
    /// Stable token for logs. Deliberately not a user-facing string — the room
    /// learns about this as an `EndReason`, not as prose from here.
    pub fn reason(self) -> &'static str {
        match self {
            TapError::NotInRoom => "not-in-room",
            TapError::ParticipantUnknown => "participant-unknown",
            TapError::NoSubscribedAudioTrack => "no-subscribed-audio-track",
        }
    }
}

struct ActiveTap {
    identity: String,
    stop: Arc<AtomicBool>,
}

fn tap() -> &'static Mutex<Option<ActiveTap>> {
    static TAP: OnceLock<Mutex<Option<ActiveTap>>> = OnceLock::new();
    TAP.get_or_init(|| Mutex::new(None))
}

/// Frames forwarded since process start. Read by diagnostics and by the tests
/// that need to prove audio actually moved rather than that a task was spawned.
fn forwarded() -> &'static AtomicU64 {
    static FORWARDED: AtomicU64 = AtomicU64::new(0);
    &FORWARDED
}

/// Number of decoded remote frames forwarded to the model.
pub fn frames_forwarded() -> u64 {
    forwarded().load(Ordering::SeqCst)
}

/// Whose track is currently being forwarded, if anyone.
pub fn tapped_identity() -> Option<String> {
    tap().lock().ok().and_then(|g| g.as_ref().map(|t| t.identity.clone()))
}

/// Pick which of a participant's audio publications to tap.
///
/// Pure so the one rule that matters is testable: the assistant's own voice
/// (`petal-ai-*`, published by this host into the same room) must never be
/// selected. Tapping it would feed Gemini its own output — an echo loop with a
/// per-token bill attached.
pub fn choose_audio_track<'a, I>(track_names: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    track_names
        .into_iter()
        .find(|name| !super::wire::is_ai_track(name))
}

/// Begin forwarding `identity`'s subscribed audio track to the model.
///
/// Idempotent for the same identity. Returns an error rather than doing
/// anything approximate — see the module docs on why there is no fallback.
pub fn start(app: &AppHandle, identity: &str) -> Result<(), TapError> {
    if tapped_identity().as_deref() == Some(identity) {
        return Ok(());
    }
    stop();

    let (connection, _local) = app
        .try_state::<crate::session::SessionState>()
        .and_then(|state| state.control_channel_snapshot())
        .ok_or(TapError::NotInRoom)?;
    let room = connection.room();

    let participant = room
        .remote_participants()
        .into_iter()
        .find(|(id, _)| id.as_str() == identity)
        .map(|(_, participant)| participant)
        .ok_or(TapError::ParticipantUnknown)?;

    // Resolve the publication by the same rule `choose_audio_track` states, then
    // take the decoded track behind it. `auto_subscribe` is on for every Petal
    // room, so a live audio publication is normally already subscribed; if it
    // is not, that is a genuine failure and the claim has to fail with it.
    let track = participant
        .track_publications()
        .values()
        .filter(|publication| !super::wire::is_ai_track(&publication.name()))
        .find_map(|publication| match publication.track() {
            Some(RemoteTrack::Audio(audio)) => Some(audio),
            _ => None,
        })
        .ok_or(TapError::NoSubscribedAudioTrack)?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let pump_stop = stop_flag.clone();
    let pump_identity = identity.to_string();
    // `NativeAudioStream` attaches a native sink and the pump awaits it, so both
    // need an ambient tokio runtime (crash class 3). Tauri's global runtime is
    // available from every caller of this function.
    tauri::async_runtime::spawn(async move {
        pump(track, pump_identity, pump_stop).await;
    });

    if let Ok(mut guard) = tap().lock() {
        *guard = Some(ActiveTap {
            identity: identity.to_string(),
            stop: stop_flag,
        });
    }
    log::info!("ai_chat: forwarding '{identity}' audio to the model");
    Ok(())
}

/// Stop forwarding. Idempotent; safe to call when nothing is running.
pub fn stop() {
    let previous = match tap().lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => return,
    };
    if let Some(previous) = previous {
        previous.stop.store(true, Ordering::SeqCst);
        log::info!("ai_chat: stopped forwarding '{}' audio", previous.identity);
    }
}

/// Read decoded PCM off the remote track, resample it to what Gemini wants, and
/// hand it to the session.
///
/// The session drops anything that arrives with no turn open, so a tap that
/// outlives its floor by a few milliseconds cannot leak audio to the model.
async fn pump(track: RemoteAudioTrack, identity: String, stop: Arc<AtomicBool>) {
    let mut stream = NativeAudioStream::new(track.rtc_track(), REQUESTED_RATE, 1);
    // Rebuilt only when the OBSERVED rate changes: the resampler carries its
    // phase across chunks (that is why it is a struct), and rebuilding it per
    // frame would click at every boundary.
    let mut resampler: Option<(u32, Resampler)> = None;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let frame = tokio::select! {
            frame = stream.next() => match frame {
                Some(frame) => frame,
                None => break,
            },
            _ = tokio::time::sleep(STOP_POLL) => continue,
        };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Read the rate off the frame. Assuming 48 kHz and being handed 16 kHz
        // would pitch the speaker down by a third and Gemini would transcribe
        // gibberish — a failure that looks like a bad model, not a bad cast.
        let rate = frame.sample_rate;
        if rate == 0 {
            continue;
        }
        let channels = (frame.num_channels as usize).max(1);
        let mono = downmix_to_mono(&i16_to_f32(&frame.data), channels);
        if resampler.as_ref().map(|(observed, _)| *observed) != Some(rate) {
            log::debug!("ai_chat: '{identity}' audio arriving at {rate}Hz -> {UPLINK_RATE}Hz");
            resampler = Some((rate, Resampler::new(rate, UPLINK_RATE)));
        }
        let Some((_, resampler)) = resampler.as_mut() else {
            continue;
        };
        let resampled = resampler.process(&mono);
        if resampled.is_empty() {
            continue;
        }
        forwarded().fetch_add(1, Ordering::SeqCst);
        super::session::push_audio(&f32_to_pcm16(&resampled));
    }

    log::debug!("ai_chat: remote audio pump for '{identity}' exited");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_assistants_own_voice_is_never_tapped() {
        // Feeding Gemini its own published output is an echo loop that bills
        // per token. The mic must win even when the AI track is listed first.
        assert_eq!(
            choose_audio_track(["petal-ai-window-42", "petal-mic"]),
            Some("petal-mic")
        );
        assert_eq!(
            choose_audio_track(["petal-mic", "petal-ai-window-42"]),
            Some("petal-mic")
        );
        // A participant publishing ONLY an assistant voice has nothing to tap.
        assert_eq!(choose_audio_track(["petal-ai-window-42"]), None);
        assert_eq!(choose_audio_track([]), None);
    }

    #[test]
    fn tap_errors_carry_distinct_tokens() {
        let all = [
            TapError::NotInRoom,
            TapError::ParticipantUnknown,
            TapError::NoSubscribedAudioTrack,
        ];
        let mut seen = std::collections::HashSet::new();
        for error in all {
            assert!(seen.insert(error.reason()), "duplicate token {error:?}");
        }
    }

    #[test]
    fn stopping_an_idle_tap_is_harmless() {
        stop();
        assert_eq!(tapped_identity(), None);
    }
}
