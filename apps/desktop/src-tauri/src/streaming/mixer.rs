//! The meeting-audio mix for client mode: every audio track in the room
//! (local mic, local app audio, remote tracks) pushes 48 kHz stereo PCM
//! here, and the AAC encoder pulls fixed blocks of 1024 frames: the sum of
//! all sources through a peak limiter.
//!
//! A muted or silent source simply pushes nothing (or zeros) and contributes
//! silence. Each source is jitter-buffered: it joins the mix once it has
//! [`PRIME_FRAMES`] queued, drops back out on underrun rather than clicking,
//! and sheds its oldest audio past [`MAX_BUFFER_FRAMES`] to bound latency.

use std::collections::{HashMap, VecDeque};

pub const MIX_SAMPLE_RATE: u32 = 48_000;
pub const MIX_CHANNELS: usize = 2;
/// One AAC-LC frame.
pub const MIX_BLOCK_FRAMES: usize = 1024;
/// 40 ms of audio before a source joins the mix.
pub const PRIME_FRAMES: usize = 1_920;
/// 200 ms ceiling per source.
pub const MAX_BUFFER_FRAMES: usize = 9_600;
/// The limiter keeps peaks at or below this fraction of full scale.
pub const LIMITER_THRESHOLD: f32 = 0.89;
/// Per-sample envelope decay; roughly 100 ms to recover.
const LIMITER_RELEASE: f32 = 0.9998;

#[derive(Debug, Default)]
struct Source {
    /// Interleaved stereo samples.
    samples: VecDeque<i16>,
    primed: bool,
}

#[derive(Debug)]
pub struct AudioMixer {
    sources: HashMap<String, Source>,
    envelope: f32,
    prime_frames: usize,
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::with_prime(PRIME_FRAMES)
    }
}

impl AudioMixer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A mixer whose sources join after `prime_frames` queued frames. The
    /// stream mic's echo reference uses 0: it must never lag the echo.
    pub fn with_prime(prime_frames: usize) -> Self {
        Self { sources: HashMap::new(), envelope: 0.0, prime_frames }
    }

    /// Append interleaved stereo PCM for `source`. Mono input is accepted
    /// with `channels == 1` and duplicated to both sides.
    pub fn push(&mut self, source: &str, pcm: &[i16], channels: usize) {
        let entry = self.sources.entry(source.to_string()).or_default();
        match channels {
            1 => {
                for sample in pcm {
                    entry.samples.push_back(*sample);
                    entry.samples.push_back(*sample);
                }
            }
            _ => {
                for frame in pcm.chunks_exact(channels.max(1)) {
                    entry.samples.push_back(frame[0]);
                    entry.samples.push_back(frame.get(1).copied().unwrap_or(frame[0]));
                }
            }
        }
        let max_samples = MAX_BUFFER_FRAMES * MIX_CHANNELS;
        if entry.samples.len() > max_samples {
            let excess = entry.samples.len() - max_samples;
            entry.samples.drain(..excess);
        }
        if entry.samples.len() >= self.prime_frames * MIX_CHANNELS {
            entry.primed = true;
        }
    }

    pub fn remove_source(&mut self, source: &str) {
        self.sources.remove(source);
    }

    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// The next block: 1024 interleaved stereo frames.
    pub fn pull_block(&mut self) -> Vec<i16> {
        self.pull(MIX_BLOCK_FRAMES)
    }

    /// The next `frames` interleaved stereo frames.
    pub fn pull(&mut self, frames: usize) -> Vec<i16> {
        let block_samples = frames * MIX_CHANNELS;
        let mut mix = vec![0f32; block_samples];
        for source in self.sources.values_mut() {
            if !source.primed {
                continue;
            }
            if source.samples.len() < block_samples {
                // Underrun: sit out until re-primed instead of a partial block.
                source.primed = false;
                continue;
            }
            for (out, sample) in mix.iter_mut().zip(source.samples.drain(..block_samples)) {
                *out += f32::from(sample) / 32768.0;
            }
        }
        mix.into_iter()
            .map(|sample| {
                // Instant attack, slow release: the gain always brings the
                // current sample to or under the threshold.
                self.envelope = (self.envelope * LIMITER_RELEASE).max(sample.abs());
                let gain = if self.envelope > LIMITER_THRESHOLD {
                    LIMITER_THRESHOLD / self.envelope
                } else {
                    1.0
                };
                (sample * gain * 32767.0).round().clamp(-32768.0, 32767.0) as i16
            })
            .collect()
    }
}

/// Level of a run of PCM, for the periodic log lines that tell where audio
/// goes missing (raw mic, processed mic, stream mix).
#[derive(Debug, Default)]
pub struct LevelMeter {
    peak: u16,
    sum: i64,
    sum_sq: f64,
    samples: u64,
}

impl LevelMeter {
    pub fn add(&mut self, pcm: &[i16]) {
        for sample in pcm {
            self.peak = self.peak.max(sample.unsigned_abs());
            self.sum += i64::from(*sample);
            self.sum_sq += f64::from(*sample).powi(2);
        }
        self.samples += pcm.len() as u64;
    }

    /// "peak=… rms=… dc=…" since the last call, then starts over.
    pub fn take(&mut self) -> String {
        let n = self.samples.max(1) as f64;
        let line = format!(
            "peak={} rms={:.0} dc={:.0}",
            self.peak,
            (self.sum_sq / n).sqrt(),
            self.sum as f64 / n
        );
        *self = Self::default();
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unprimed_mixer_joins_at_once_and_pulls_any_length() {
        let mut mixer = AudioMixer::with_prime(0);
        mixer.push("far", &constant(1000, 480), MIX_CHANNELS);
        let block = mixer.pull(480);
        assert_eq!(block.len(), 480 * MIX_CHANNELS);
        assert!(block.iter().all(|sample| (999..=1001).contains(sample)), "{:?}", &block[..4]);
        // Nothing queued: silence, and the source rejoins with its next push.
        assert!(mixer.pull(480).iter().all(|sample| *sample == 0));
        mixer.push("far", &constant(500, 480), MIX_CHANNELS);
        assert!(mixer.pull(480).iter().all(|sample| (499..=501).contains(sample)));
    }

    fn constant(value: i16, frames: usize) -> Vec<i16> {
        vec![value; frames * MIX_CHANNELS]
    }

    #[test]
    fn sums_sources() {
        let mut mixer = AudioMixer::new();
        mixer.push("mic", &constant(1000, PRIME_FRAMES), 2);
        mixer.push("remote", &constant(2000, PRIME_FRAMES), 2);
        let block = mixer.pull_block();
        assert_eq!(block.len(), MIX_BLOCK_FRAMES * MIX_CHANNELS);
        assert!(block.iter().all(|s| (*s - 3000).abs() <= 1), "{:?}", &block[..4]);
    }

    #[test]
    fn mono_is_duplicated_to_both_sides() {
        let mut mixer = AudioMixer::new();
        let mono: Vec<i16> = (0..PRIME_FRAMES).map(|i| if i % 2 == 0 { 500 } else { -500 }).collect();
        mixer.push("mic", &mono, 1);
        let block = mixer.pull_block();
        assert_eq!(&block[..4], &[500, 500, -500, -500]);
    }

    #[test]
    fn limiter_holds_loud_sums_under_full_scale() {
        let mut mixer = AudioMixer::new();
        for source in ["a", "b", "c"] {
            mixer.push(source, &constant(30_000, PRIME_FRAMES), 2);
        }
        let block = mixer.pull_block();
        let ceiling = (LIMITER_THRESHOLD * 32767.0).round() as i16 + 1;
        // 90 000 would wrap an i16; the limiter keeps it at the threshold.
        assert!(block.iter().all(|s| *s > 0 && *s <= ceiling), "{:?}", &block[..4]);
    }

    #[test]
    fn a_muted_mic_contributes_silence() {
        let mut mixer = AudioMixer::new();
        // Muted: the mic pushes nothing; the remote is all that's heard.
        mixer.push("remote", &constant(1234, PRIME_FRAMES), 2);
        let block = mixer.pull_block();
        assert!(block.iter().all(|s| *s == 1234));
        // Nothing at all: silence, not an error.
        let mut empty = AudioMixer::new();
        assert!(empty.pull_block().iter().all(|s| *s == 0));
    }

    #[test]
    fn sources_join_once_primed_and_sit_out_underruns() {
        let mut mixer = AudioMixer::new();
        mixer.push("mic", &constant(100, PRIME_FRAMES - 1), 2);
        assert!(mixer.pull_block().iter().all(|s| *s == 0), "not primed yet");
        mixer.push("mic", &constant(100, 1), 2);
        assert!(mixer.pull_block().iter().all(|s| *s == 100));
        // 896 frames left: less than a block, so the source sits out.
        assert!(mixer.pull_block().iter().all(|s| *s == 0));
    }

    #[test]
    fn buffers_are_bounded() {
        let mut mixer = AudioMixer::new();
        mixer.push("slow-consumer", &constant(1, MAX_BUFFER_FRAMES * 3), 2);
        let queued = mixer.sources["slow-consumer"].samples.len();
        assert_eq!(queued, MAX_BUFFER_FRAMES * MIX_CHANNELS);
    }
}
