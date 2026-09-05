//! AI chat audio: push-to-talk capture up to Gemini, assistant voice back down.
//!
//! ## This module is the LOCAL half only
//!
//! Everything here concerns THIS machine's own hardware: its microphone, its
//! speakers. A REMOTE participant's push-to-talk is served by
//! [`super::remote_audio`], which taps that participant's already-subscribed
//! LiveKit track — it never comes through here. The distinction is the whole
//! point of the split, so the two are impossible to confuse at a call site:
//! opening the host's microphone because a *peer* pressed their key streams the
//! host's room to Google while the peer reaches nothing (#657).
//!
//! ## Why this is not LiveKit's audio path
//!
//! Petal's meeting mic is owned by LiveKit's audio device module, which exposes
//! no per-frame tap, and the assistant's voice must not be silenced by the
//! meeting mute button. So AI chat runs its own `cpal` streams, opened only
//! while they are needed.
//!
//! ## Push-to-talk changes the problem
//!
//! The takt reference held the microphone open for the whole session and leaned
//! on server-side voice activity detection, which forced hardware echo
//! cancellation (otherwise the model hears itself and self-interrupts). Petal
//! is push-to-talk: the capture stream exists **only while a turn is open**, so
//! there is no open mic to echo into, and the simple path is correct here.
//!
//! Two lessons from that reference are still load-bearing and are honoured:
//! - **Resampler phase carries across chunks.** Resampling each chunk from
//!   phase zero produces an audible click at every boundary.
//! - **Prime the playback buffer (~120 ms) before starting**, and re-prime after
//!   an underrun, or the assistant's voice stutters on the first syllable.
//!
//! Open question #654 Q5: whether locally-played assistant audio leaks into the
//! meeting mic that the ADM is publishing. If it does, the sharer's mic needs
//! ducking while [`is_playing`] is true — the hook is here for that.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// What Gemini Live expects on the uplink.
pub const UPLINK_RATE: u32 = 16_000;
/// What Gemini Live sends on the downlink.
pub const DOWNLINK_RATE: u32 = 24_000;
/// Samples of assistant audio buffered before playback starts (~120 ms at the
/// downlink rate). Below this the first syllable stutters.
const PRIME_SAMPLES: usize = (DOWNLINK_RATE as usize * 120) / 1000;

/// Linear resampler that carries its phase across calls.
///
/// Kept as a struct rather than a free function precisely so the phase survives
/// between chunks; a stateless version clicks at every boundary.
pub struct Resampler {
    /// Input samples consumed per output sample.
    ratio: f64,
    /// Read cursor in input-sample space. Always >= 0; whatever fraction is
    /// left over at the end of a chunk starts the next one, which is the whole
    /// point of keeping this in a struct.
    phase: f64,
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            ratio: from_rate as f64 / to_rate as f64,
            phase: 0.0,
        }
    }

    /// Resample one chunk, continuing from wherever the previous chunk ended.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        let len = input.len();
        let mut out = Vec::with_capacity((len as f64 / self.ratio) as usize + 2);
        while self.phase < len as f64 {
            let idx = self.phase.floor() as usize;
            let frac = (self.phase - idx as f64) as f32;
            let left = input[idx];
            // At the chunk's trailing edge the next sample lives in the chunk we
            // do not have yet, so hold. That is a far smaller artifact than
            // resetting phase would be.
            let right = if idx + 1 < len { input[idx + 1] } else { left };
            out.push(left + (right - left) * frac);
            self.phase += self.ratio;
        }
        // Carry the remainder into the next chunk.
        self.phase -= len as f64;
        out
    }
}

/// Mono f32 → interleaved little-endian PCM16, clamped so a hot sample cannot
/// wrap to the opposite polarity.
pub fn f32_to_pcm16(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Interleaved little-endian PCM16 → mono f32 in [-1, 1].
pub fn pcm16_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

/// Already-decoded PCM16 samples → f32 in [-1, 1]. LiveKit's `AudioFrame`
/// hands over `[i16]` directly, so this is the byte-free sibling of
/// [`pcm16_to_f32`] — going via bytes just to come straight back would be a
/// pointless copy on every decoded frame.
pub fn i16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// Downmix an interleaved multi-channel frame buffer to mono by averaging.
pub fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

// ---- playback ---------------------------------------------------------------

struct Playback {
    /// Assistant audio waiting to be rendered, at the device's rate.
    queue: Arc<Mutex<std::collections::VecDeque<f32>>>,
    /// Kept alive to keep the stream running; dropping it stops playback.
    _stream: cpal::Stream,
    resampler: Resampler,
    playing: Arc<AtomicBool>,
}

// cpal::Stream is not Send on some platforms; the whole struct lives behind a
// mutex on one owning thread and is never moved across threads while running.
unsafe impl Send for Playback {}

fn playback() -> &'static Mutex<Option<Playback>> {
    static PLAYBACK: OnceLock<Mutex<Option<Playback>>> = OnceLock::new();
    PLAYBACK.get_or_init(|| Mutex::new(None))
}

fn playing_flag() -> &'static AtomicBool {
    static PLAYING: AtomicBool = AtomicBool::new(false);
    &PLAYING
}

/// True while assistant audio is actually being rendered. #654 Q5 decides
/// whether the meeting mic must duck while this holds.
pub fn is_playing() -> bool {
    playing_flag().load(Ordering::Relaxed)
}

/// #845: the assistant's local render (this module) is deliberately a raw
/// cpal stream, invisible to the mic capture's WebRTC APM/AEC (see this
/// module's top doc comment and `transport/audio.rs`'s "Echo cancellation /
/// APM" section) -- so a sharer's mic re-captures the assistant's own voice
/// and republishes it, and peers hear it twice. The preferred fix (route the
/// local render through the ADM so the APM's far-end reference sees it,
/// e.g. via loopback-subscribing the host's own published `petal-ai-*`
/// track) is not available: `session.rs`'s own `ServerEvent::Audio` handler
/// already documents why both `play()` and `voice::push()` are called --
/// "LiveKit does not loop a participant's own track back". With no far-end
/// reference reachable, the fallback is ducking the mic while the assistant
/// talks (costs full-duplex barge-in while ducked -- a user-accepted
/// tradeoff, not silently dropped).
///
/// `MicDuckGate` is the pure hysteresis decision: instant duck the moment
/// playback starts (no echo window), but hold the duck for `release_delay`
/// after playback last sampled active, so a brief inter-sentence gap doesn't
/// un-duck and re-duck every tick (which would chop the trailing consonant
/// of one sentence and the leading one of the next back into the room) and
/// doesn't machine-gun the underlying `LocalAudioTrack::mute()`/`unmute()`
/// SDK calls. Fed by `is_playing()` polls, not events, because playback end
/// has no callback exposed to this module -- only the queue-drain state the
/// render callback owns.
#[derive(Debug)]
pub(crate) struct MicDuckGate {
    release_delay: Duration,
    last_active_at: Option<Instant>,
}

impl MicDuckGate {
    pub(crate) fn new(release_delay: Duration) -> Self {
        Self {
            release_delay,
            last_active_at: None,
        }
    }

    /// Feed the latest `is_playing()` sample; returns whether the mic should
    /// be ducked right now.
    pub(crate) fn sample(&mut self, playing: bool, now: Instant) -> bool {
        if playing {
            self.last_active_at = Some(now);
            return true;
        }
        match self.last_active_at {
            Some(last) if now.saturating_duration_since(last) < self.release_delay => true,
            _ => false,
        }
    }
}

/// Test-only: mark playback as active without opening a real output device.
/// `play()` itself always opens real hardware (unlike capture, it has no
/// `cfg!(test)` short-circuit), so a test proving something reacts to
/// "assistant is currently talking" needs a hardware-free way to get there.
#[cfg(test)]
pub(crate) fn set_playing_for_test(value: bool) {
    playing_flag().store(value, Ordering::Relaxed);
}

/// Queue a chunk of assistant audio (PCM16, 24 kHz mono, exactly as Gemini
/// sends it). Opens the output stream on first use.
pub fn play(pcm16: &[u8]) {
    let samples = pcm16_to_f32(pcm16);
    if samples.is_empty() {
        return;
    }
    let mut guard = match playback().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if guard.is_none() {
        match open_output() {
            Some(p) => *guard = Some(p),
            None => return,
        }
    }
    // Resample under the playback lock, then release it BEFORE touching the
    // queue: the render callback wants the queue lock on the audio thread, and
    // holding two locks across it invites contention (and would outlive the
    // guard's borrow anyway).
    let (queue, playing, resampled) = {
        let Some(p) = guard.as_mut() else { return };
        let resampled = p.resampler.process(&samples);
        (p.queue.clone(), p.playing.clone(), resampled)
    };
    drop(guard);

    // Bind the guard directly rather than `if let`: an `if let` temporary lives
    // to the end of the enclosing block, which here is after `queue` itself is
    // dropped.
    let mut q = match queue.lock() {
        Ok(q) => q,
        Err(_) => return,
    };
    q.extend(resampled);
    if !playing.load(Ordering::Relaxed) && q.len() >= PRIME_SAMPLES {
        playing.store(true, Ordering::Relaxed);
        playing_flag().store(true, Ordering::Relaxed);
    }
}

fn open_output() -> Option<Playback> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let config = device.default_output_config().ok()?;
    let channels = config.channels() as usize;
    let device_rate = config.sample_rate().0;

    let queue = Arc::new(Mutex::new(std::collections::VecDeque::<f32>::new()));
    let playing = Arc::new(AtomicBool::new(false));

    let cb_queue = queue.clone();
    let cb_playing = playing.clone();
    let stream = device
        .build_output_stream(
            &config.config(),
            move |out: &mut [f32], _| {
                let mut q = match cb_queue.lock() {
                    Ok(q) => q,
                    Err(_) => {
                        out.fill(0.0);
                        return;
                    }
                };
                if !cb_playing.load(Ordering::Relaxed) {
                    out.fill(0.0);
                    return;
                }
                for frame in out.chunks_mut(channels) {
                    let sample = q.pop_front().unwrap_or(0.0);
                    for slot in frame.iter_mut() {
                        *slot = sample;
                    }
                }
                if q.is_empty() {
                    // Underrun: stop and re-prime rather than dribbling.
                    cb_playing.store(false, Ordering::Relaxed);
                    playing_flag().store(false, Ordering::Relaxed);
                }
            },
            |err| log::debug!("ai_chat: audio output error: {err}"),
            None,
        )
        .ok()?;
    stream.play().ok()?;

    Some(Playback {
        queue,
        _stream: stream,
        resampler: Resampler::new(DOWNLINK_RATE, device_rate),
        playing,
    })
}

/// Stop playback immediately and discard anything queued. Called on barge-in
/// and on teardown — the assistant must not still be talking after the session
/// reports that it ended.
pub fn stop_playback() {
    playing_flag().store(false, Ordering::Relaxed);
    if let Ok(mut guard) = playback().lock() {
        if let Some(p) = guard.as_ref() {
            if let Ok(mut q) = p.queue.lock() {
                q.clear();
            }
        }
        *guard = None; // dropping the stream stops the device
    }
}

// ---- capture (THIS MACHINE'S microphone, and nothing else) -------------------

struct Capture {
    _stream: cpal::Stream,
}

unsafe impl Send for Capture {}

fn capture() -> &'static Mutex<Option<Capture>> {
    static CAPTURE: OnceLock<Mutex<Option<Capture>>> = OnceLock::new();
    CAPTURE.get_or_init(|| Mutex::new(None))
}

/// How many times this machine's microphone has been asked to open for AI chat.
///
/// Exists so a test can prove the negative that matters: a REMOTE speaker's
/// push-to-talk must never reach this path (#657). Counting the *request*
/// rather than the successful open keeps the assertion meaningful on a machine
/// with no input device.
fn mic_open_requests() -> &'static AtomicU64 {
    static REQUESTS: AtomicU64 = AtomicU64::new(0);
    &REQUESTS
}

/// Number of times [`start_local_microphone_capture`] has been called.
pub fn local_microphone_open_requests() -> u64 {
    mic_open_requests().load(Ordering::SeqCst)
}

/// Open **this machine's** microphone for one push-to-talk turn.
///
/// Captured audio is resampled to 16 kHz mono PCM16 and handed to the session,
/// which drops it unless a turn is genuinely open — so nothing can reach the
/// model outside a held PTT.
///
/// The only legitimate caller is `session::begin_capture`'s
/// `PttSource::LocalMicrophone` arm. A remote participant's turn is served by
/// [`super::remote_audio`] instead; routing one here would open the host's mic
/// for someone else's key press.
///
/// Cross-platform (`cpal`); the AI session engine is compiled on every
/// platform now, so Windows hosts open their own mic exactly like macOS hosts.
pub fn start_local_microphone_capture() {
    mic_open_requests().fetch_add(1, Ordering::SeqCst);
    // A unit test must never actually open the machine's microphone: it would
    // raise the macOS TCC prompt and record the room to prove a routing rule.
    // The counter above is the observable the routing test reads, and it is
    // incremented at the real call site, so the test still fails if the
    // local/remote branches are swapped.
    if cfg!(test) {
        return;
    }
    let mut guard = match capture().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if guard.is_some() {
        return;
    }
    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        log::warn!("ai_chat: no input device -- push-to-talk cannot capture");
        return;
    };
    let Ok(config) = device.default_input_config() else {
        log::warn!("ai_chat: no default input config");
        return;
    };
    let channels = config.channels() as usize;
    let mut resampler = Resampler::new(config.sample_rate().0, UPLINK_RATE);

    let stream = device.build_input_stream(
        &config.config(),
        move |input: &[f32], _| {
            let mono = downmix_to_mono(input, channels);
            let resampled = resampler.process(&mono);
            if !resampled.is_empty() {
                super::session::push_audio(&f32_to_pcm16(&resampled));
            }
        },
        |err| log::debug!("ai_chat: audio input error: {err}"),
        None,
    );
    match stream {
        Ok(s) => {
            if s.play().is_ok() {
                *guard = Some(Capture { _stream: s });
            }
        }
        Err(e) => log::warn!("ai_chat: could not open microphone: {e}"),
    }
}

/// Close this machine's microphone at the end of a turn (or on teardown).
///
/// NEVER call this while holding the `session::sessions()` lock: dropping the
/// `cpal::Stream` waits for its render callback to finish, and that callback
/// calls `session::push_audio`, which wants the same lock.
pub fn stop_local_microphone_capture() {
    if let Ok(mut guard) = capture().lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm16_roundtrip_preserves_signal() {
        let original = vec![0.0f32, 0.5, -0.5, 0.25];
        let round = pcm16_to_f32(&f32_to_pcm16(&original));
        for (a, b) in original.iter().zip(round.iter()) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    #[test]
    fn pcm16_clamps_instead_of_wrapping() {
        // A hot sample must saturate, not flip polarity.
        let bytes = f32_to_pcm16(&[2.0, -2.0]);
        assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), 32767);
        assert_eq!(i16::from_le_bytes([bytes[2], bytes[3]]), -32767);
    }

    // #845: MicDuckGate's hysteresis -- instant duck, delayed release.
    #[test]
    fn duck_gate_engages_instantly_when_playback_starts() {
        let mut gate = MicDuckGate::new(Duration::from_millis(500));
        let t0 = Instant::now();
        assert!(!gate.sample(false, t0), "must not duck before any playback");
        assert!(
            gate.sample(true, t0),
            "must duck the instant playback starts -- no echo window"
        );
    }

    #[test]
    fn duck_gate_holds_through_a_brief_gap_then_releases() {
        let mut gate = MicDuckGate::new(Duration::from_millis(500));
        let t0 = Instant::now();
        assert!(gate.sample(true, t0));
        // A short inter-sentence gap: still within the release delay.
        assert!(
            gate.sample(false, t0 + Duration::from_millis(200)),
            "a brief silence inside the release delay must stay ducked"
        );
        // Playback resumes: still ducked, and the release clock resets from here.
        assert!(gate.sample(true, t0 + Duration::from_millis(250)));
        assert!(
            gate.sample(false, t0 + Duration::from_millis(600)),
            "still within 500ms of the LATEST active sample (t0+250), not the first"
        );
        // Now past the release delay from the last active sample (t0+250).
        assert!(
            !gate.sample(false, t0 + Duration::from_millis(800)),
            "must release once silence has held past the release delay"
        );
    }

    #[test]
    fn duck_gate_never_ducks_before_the_first_playback() {
        let mut gate = MicDuckGate::new(Duration::from_millis(500));
        let t0 = Instant::now();
        for step in 0..5 {
            assert!(!gate.sample(false, t0 + Duration::from_millis(step * 100)));
        }
    }

    #[test]
    fn downmix_averages_channels() {
        let stereo = [1.0f32, 0.0, 0.5, 0.5];
        assert_eq!(downmix_to_mono(&stereo, 2), vec![0.5, 0.5]);
        // Mono passes through untouched.
        assert_eq!(downmix_to_mono(&[0.3, 0.7], 1), vec![0.3, 0.7]);
    }

    #[test]
    fn resampler_downsamples_to_roughly_the_target_rate() {
        let mut r = Resampler::new(48_000, 16_000);
        let input: Vec<f32> = (0..4800).map(|i| (i as f32 / 100.0).sin()).collect();
        let out = r.process(&input);
        // 48k -> 16k is a third of the samples, within a sample of rounding.
        assert!(
            (out.len() as i64 - 1600).abs() <= 2,
            "expected ~1600, got {}",
            out.len()
        );
    }

    #[test]
    fn resampler_upsamples_for_playback() {
        let mut r = Resampler::new(24_000, 48_000);
        let input: Vec<f32> = (0..2400).map(|i| (i as f32 / 50.0).sin()).collect();
        let out = r.process(&input);
        assert!(
            (out.len() as i64 - 4800).abs() <= 2,
            "expected ~4800, got {}",
            out.len()
        );
    }

    #[test]
    fn resampler_phase_carries_across_chunks() {
        // The whole reason this is a struct: resampling chunk-by-chunk must
        // produce the same total sample count as one contiguous pass, or every
        // chunk boundary is an audible click.
        let whole: Vec<f32> = (0..4800).map(|i| (i as f32 / 100.0).sin()).collect();
        let mut one_pass = Resampler::new(48_000, 16_000);
        let expected = one_pass.process(&whole).len();

        let mut chunked = Resampler::new(48_000, 16_000);
        let produced: usize = whole.chunks(480).map(|c| chunked.process(c).len()).sum();

        assert!(
            (produced as i64 - expected as i64).abs() <= 1,
            "chunked {produced} vs contiguous {expected} -- phase is not carrying"
        );
    }

    #[test]
    fn resampler_handles_empty_input() {
        let mut r = Resampler::new(48_000, 16_000);
        assert!(r.process(&[]).is_empty());
    }

    #[test]
    fn i16_and_byte_conversions_agree() {
        // The remote-tap path takes `AudioFrame`'s `[i16]` directly; it must
        // land on exactly the same floats the byte path produces, or a remote
        // speaker's audio would be scaled differently from the local mic's.
        let samples = [0i16, 16_384, -16_384, 32_767, -32_768];
        let via_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(i16_to_f32(&samples), pcm16_to_f32(&via_bytes));
    }

    #[test]
    fn prime_threshold_is_about_120ms() {
        // ~120ms at 24kHz. A much smaller value stutters the first syllable.
        assert_eq!(PRIME_SAMPLES, 2_880);
    }
}
