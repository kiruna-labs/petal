//! Shared screen-audio policy and real-time PCM primitives.
//!
//! Platform adapters only produce owned PCM; this module owns the small
//! contract that keeps duplicate visual shares from producing duplicate audio
//! tracks. Display and region shares intentionally map to one system-output
//! source because neither platform promises monitor-isolated audio here.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::transport::publisher::SharedSourceKind;
use serde::Serialize;

#[cfg(target_os = "macos")]
use crate::macos_screen_audio::{MacScreenAudioCapture, MacScreenAudioTarget};
#[cfg(target_os = "windows")]
use crate::windows_screen_audio::WindowsScreenAudioCapture;

pub const SCREEN_AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const SCREEN_AUDIO_CHANNELS: usize = 2;
pub const SCREEN_AUDIO_FRAME_MS: u32 = 10;
pub const SCREEN_AUDIO_SAMPLES_PER_CHANNEL: usize =
    (SCREEN_AUDIO_SAMPLE_RATE as usize * SCREEN_AUDIO_FRAME_MS as usize) / 1_000;
pub const SCREEN_AUDIO_SAMPLES_PER_FRAME: usize =
    SCREEN_AUDIO_SAMPLES_PER_CHANNEL * SCREEN_AUDIO_CHANNELS;
/// Keep capture callbacks non-blocking while bounding end-to-end latency.
/// 400 ms is inside the agreed 250–500 ms range.
pub const SCREEN_AUDIO_QUEUE_MS: usize = 400;
pub const SCREEN_AUDIO_QUEUE_FRAMES: usize = SCREEN_AUDIO_QUEUE_MS / SCREEN_AUDIO_FRAME_MS as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum AudioSourceKey {
    SystemOutput,
    Process(u32),
}

impl AudioSourceKey {
    pub(crate) fn for_share(kind: SharedSourceKind, owner_pid: Option<u32>) -> Option<Self> {
        match kind {
            SharedSourceKind::Window => owner_pid.filter(|pid| *pid != 0).map(Self::Process),
            SharedSourceKind::Display | SharedSourceKind::DisplayRegion => Some(Self::SystemOutput),
        }
    }

    pub(crate) fn label(self) -> String {
        match self {
            Self::SystemOutput => "petal-window-audio-system".to_string(),
            Self::Process(pid) => format!("petal-window-audio-process-{pid}"),
        }
    }

    pub(crate) fn track_name(self) -> String {
        self.label()
    }
}

/// Rust-authoritative state returned by both query and mutation commands.
/// `enabled` is consent for this exact share incarnation; `publishing` is the
/// current companion-track outcome. A failed audio attempt never changes the
/// visual share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareAudioState {
    pub window_id: u32,
    pub enabled: bool,
    pub available: bool,
    pub publishing: bool,
    pub scope: Option<String>,
    pub error: Option<String>,
}

impl ShareAudioState {
    pub(crate) fn inactive(window_id: u32) -> Self {
        Self {
            window_id,
            enabled: false,
            available: false,
            publishing: false,
            scope: None,
            error: Some("Share this source before enabling audio".to_string()),
        }
    }
}

/// A platform-independent target for the native screen-audio adapters. A
/// display id is only a capture-context hint on macOS; system output remains
/// one logical source and is never advertised as monitor-isolated audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScreenAudioTarget {
    SystemOutput { display_id: Option<u32> },
    Process { pid: u32, display_id: Option<u32> },
}

impl ScreenAudioTarget {
    pub(crate) fn for_source(source: AudioSourceKey) -> Self {
        match source {
            AudioSourceKey::SystemOutput => Self::SystemOutput { display_id: None },
            AudioSourceKey::Process(pid) => Self::Process {
                pid,
                display_id: None,
            },
        }
    }
}

/// The native owner is kept behind this small enum so the session owns one
/// capture resource per logical source on every desktop platform.
pub(crate) enum ScreenAudioCapture {
    #[cfg(target_os = "macos")]
    Mac(MacScreenAudioCapture),
    #[cfg(target_os = "windows")]
    Windows(WindowsScreenAudioCapture),
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Unsupported,
}

impl ScreenAudioCapture {
    pub(crate) fn start(
        source: AudioSourceKey,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            let target = match ScreenAudioTarget::for_source(source) {
                ScreenAudioTarget::SystemOutput { display_id } => {
                    MacScreenAudioTarget::SystemOutput { display_id }
                }
                ScreenAudioTarget::Process { pid, display_id } => {
                    MacScreenAudioTarget::Process { pid, display_id }
                }
            };
            let callback = move |error| on_error(error);
            return MacScreenAudioCapture::start(target, callback)
                .map(Self::Mac)
                .map_err(|error| error.to_string());
        }
        #[cfg(target_os = "windows")]
        {
            return WindowsScreenAudioCapture::start(source, on_error)
                .map(Self::Windows)
                .map_err(|error| error.to_string());
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = on_error;
            Err("screen audio is unavailable on this platform".to_string())
        }
    }

    pub(crate) fn queue(&self) -> ScreenAudioQueue {
        match self {
            #[cfg(target_os = "macos")]
            Self::Mac(capture) => capture.queue(),
            #[cfg(target_os = "windows")]
            Self::Windows(capture) => capture.queue(),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Self::Unsupported => ScreenAudioQueue::new(),
        }
    }

    pub(crate) fn close_queue(&self) {
        match self {
            #[cfg(target_os = "macos")]
            Self::Mac(capture) => capture.close_queue(),
            #[cfg(target_os = "windows")]
            Self::Windows(capture) => capture.close_queue(),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Self::Unsupported => {}
        }
    }

    pub(crate) fn stop(&self) -> Result<(), String> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Mac(capture) => capture.stop().map_err(|error| error.to_string()),
            #[cfg(target_os = "windows")]
            Self::Windows(capture) => capture.stop().map_err(|error| error.to_string()),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Self::Unsupported => Ok(()),
        }
    }

    pub(crate) fn failed(&self) -> bool {
        match self {
            #[cfg(target_os = "macos")]
            Self::Mac(capture) => capture.failed(),
            #[cfg(target_os = "windows")]
            Self::Windows(capture) => capture.failed(),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Self::Unsupported => true,
        }
    }
}

/// The only source transitions a session needs to execute. The registry
/// computes a complete before/after diff, so a source replacement never
/// briefly starts a process stream that system audio should suppress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioSourceTransition {
    Start(AudioSourceKey),
    Stop(AudioSourceKey),
}

/// Pure desired-source policy. Capture/publication handles live outside this
/// type; this map only answers which logical sources should be running.
#[derive(Debug, Default)]
pub(crate) struct AudioSourceRegistry {
    shares: BTreeMap<u64, AudioSourceKey>,
    references: BTreeMap<AudioSourceKey, usize>,
}

impl AudioSourceRegistry {
    pub(crate) fn acquire(
        &mut self,
        share_id: u64,
        source: AudioSourceKey,
    ) -> Vec<AudioSourceTransition> {
        if self.shares.get(&share_id) == Some(&source) {
            return Vec::new();
        }

        let before = self.active_sources();
        if let Some(previous) = self.shares.insert(share_id, source) {
            self.decrement(previous);
        }
        *self.references.entry(source).or_default() += 1;
        self.transitions(before)
    }

    pub(crate) fn release(&mut self, share_id: u64) -> Vec<AudioSourceTransition> {
        let Some(source) = self.shares.remove(&share_id) else {
            return Vec::new();
        };

        let before = self.active_sources();
        self.decrement(source);
        self.transitions(before)
    }

    pub(crate) fn source_for_share(&self, share_id: u64) -> Option<AudioSourceKey> {
        self.shares.get(&share_id).copied()
    }

    pub(crate) fn is_active(&self, source: AudioSourceKey) -> bool {
        self.active_sources().contains(&source)
    }

    pub(crate) fn reference_count(&self, source: AudioSourceKey) -> usize {
        self.references.get(&source).copied().unwrap_or(0)
    }

    pub(crate) fn active_source_keys(&self) -> Vec<AudioSourceKey> {
        self.active_sources().into_iter().collect()
    }

    pub(crate) fn clear(&mut self) {
        self.shares.clear();
        self.references.clear();
    }

    fn decrement(&mut self, source: AudioSourceKey) {
        let Some(count) = self.references.get_mut(&source) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.references.remove(&source);
        }
    }

    fn active_sources(&self) -> BTreeSet<AudioSourceKey> {
        let system_active = self.reference_count(AudioSourceKey::SystemOutput) > 0;
        self.references
            .keys()
            .copied()
            .filter(|source| {
                *source == AudioSourceKey::SystemOutput
                    || (!system_active && matches!(source, AudioSourceKey::Process(_)))
            })
            .collect()
    }

    fn transitions(&self, before: BTreeSet<AudioSourceKey>) -> Vec<AudioSourceTransition> {
        let after = self.active_sources();
        let mut transitions = Vec::new();
        // Stop first: this is important when a display source takes over from
        // a process source, otherwise a receiver can briefly hear both.
        for source in before.difference(&after) {
            transitions.push(AudioSourceTransition::Stop(*source));
        }
        for source in after.difference(&before) {
            transitions.push(AudioSourceTransition::Start(*source));
        }
        transitions
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScreenAudioPublicationHealth {
    CurrentSidPresent,
    ReplacementAlreadyPresent,
    Missing,
}

pub(crate) fn screen_audio_publication_health<'a>(
    current_sid: &str,
    expected_track_name: &str,
    publications: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> ScreenAudioPublicationHealth {
    let mut replacement_present = false;
    for (sid, name) in publications {
        if sid == current_sid {
            return ScreenAudioPublicationHealth::CurrentSidPresent;
        }
        if name == expected_track_name {
            replacement_present = true;
        }
    }
    if replacement_present {
        ScreenAudioPublicationHealth::ReplacementAlreadyPresent
    } else {
        ScreenAudioPublicationHealth::Missing
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub(crate) enum PcmError {
    #[error("audio sample rate must be positive")]
    InvalidSampleRate,
    #[error("audio channel count must be positive")]
    InvalidChannelCount,
    #[error("audio sample count {samples} is not divisible by {channels} channels")]
    PartialInterleavedFrame { samples: usize, channels: usize },
}

/// Owned interleaved samples from a platform callback. Samples are normalized
/// to f32 here so platform adapters can copy native S16/float buffers once and
/// leave channel/rate conversion to the common frame assembler.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PcmChunk {
    samples: Vec<f32>,
    sample_rate: u32,
    channels: usize,
}

impl PcmChunk {
    pub(crate) fn from_i16(
        samples: Vec<i16>,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, PcmError> {
        validate_pcm_shape(samples.len(), sample_rate, channels)?;
        Ok(Self {
            samples: samples
                .into_iter()
                .map(|sample| {
                    if sample == i16::MIN {
                        -1.0
                    } else {
                        sample as f32 / i16::MAX as f32
                    }
                })
                .collect(),
            sample_rate,
            channels,
        })
    }

    pub(crate) fn from_f32(
        samples: Vec<f32>,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, PcmError> {
        validate_pcm_shape(samples.len(), sample_rate, channels)?;
        Ok(Self {
            samples: samples
                .into_iter()
                .map(|sample| sample.clamp(-1.0, 1.0))
                .collect(),
            sample_rate,
            channels,
        })
    }

    pub(crate) fn silence(
        frames: usize,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, PcmError> {
        Self::from_f32(
            vec![0.0; frames.saturating_mul(channels)],
            sample_rate,
            channels,
        )
    }
}

fn validate_pcm_shape(
    sample_count: usize,
    sample_rate: u32,
    channels: usize,
) -> Result<(), PcmError> {
    if sample_rate == 0 {
        return Err(PcmError::InvalidSampleRate);
    }
    if channels == 0 {
        return Err(PcmError::InvalidChannelCount);
    }
    if sample_count % channels != 0 {
        return Err(PcmError::PartialInterleavedFrame {
            samples: sample_count,
            channels,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScreenAudioFrame {
    samples: Vec<i16>,
}

impl ScreenAudioFrame {
    pub(crate) fn new(samples: Vec<i16>) -> Option<Self> {
        (samples.len() == SCREEN_AUDIO_SAMPLES_PER_FRAME).then_some(Self { samples })
    }

    pub(crate) fn silence() -> Self {
        Self {
            samples: vec![0; SCREEN_AUDIO_SAMPLES_PER_FRAME],
        }
    }

    pub(crate) fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub(crate) fn into_samples(self) -> Vec<i16> {
        self.samples
    }
}

/// Streaming channel conversion and linear resampling into LiveKit's fixed
/// 48 kHz stereo/10 ms frame shape. The one-sample lookahead is retained
/// across callback chunks, avoiding a discontinuity at normal buffer edges.
pub(crate) struct PcmFrameAssembler {
    source_rate: Option<u32>,
    source_channels: Option<usize>,
    source_phase: f64,
    source_buffer: VecDeque<[f32; 2]>,
    pending_samples: Vec<i16>,
}

impl Default for PcmFrameAssembler {
    fn default() -> Self {
        Self {
            source_rate: None,
            source_channels: None,
            source_phase: 0.0,
            source_buffer: VecDeque::new(),
            pending_samples: Vec::with_capacity(SCREEN_AUDIO_SAMPLES_PER_FRAME),
        }
    }
}

impl PcmFrameAssembler {
    pub(crate) fn push(&mut self, chunk: PcmChunk) -> Result<Vec<ScreenAudioFrame>, PcmError> {
        let PcmChunk {
            samples,
            sample_rate,
            channels,
        } = chunk;
        validate_pcm_shape(samples.len(), sample_rate, channels)?;
        if self.source_rate != Some(sample_rate) || self.source_channels != Some(channels) {
            self.source_rate = Some(sample_rate);
            self.source_channels = Some(channels);
            self.source_phase = 0.0;
            self.source_buffer.clear();
            // A format change is a discontinuity; never combine a partial
            // frame from the old layout with samples from the new one.
            self.pending_samples.clear();
        }

        for frame in samples.chunks_exact(channels) {
            self.source_buffer.push_back(to_stereo(frame));
        }

        let step = sample_rate as f64 / SCREEN_AUDIO_SAMPLE_RATE as f64;
        while !self.source_buffer.is_empty() {
            let index = self.source_phase.floor() as usize;
            let Some(left) = self.source_buffer.get(index).copied() else {
                break;
            };
            let fraction = self.source_phase.fract() as f32;
            let right = self.source_buffer.get(index + 1).copied();
            if right.is_none() && fraction > f32::EPSILON {
                break;
            }
            let right = right.unwrap_or(left);
            let stereo = [
                left[0] + (right[0] - left[0]) * fraction,
                left[1] + (right[1] - left[1]) * fraction,
            ];
            self.pending_samples.push(f32_to_i16(stereo[0]));
            self.pending_samples.push(f32_to_i16(stereo[1]));
            self.source_phase += step;

            // Retain the last consumed source sample for the next callback;
            // it is the left side of the first cross-buffer interpolation.
            let removable = (self.source_phase.floor() as usize)
                .min(self.source_buffer.len().saturating_sub(1));
            for _ in 0..removable {
                self.source_buffer.pop_front();
            }
            self.source_phase -= removable as f64;
        }

        let mut frames = Vec::new();
        while self.pending_samples.len() >= SCREEN_AUDIO_SAMPLES_PER_FRAME {
            let samples = self
                .pending_samples
                .drain(..SCREEN_AUDIO_SAMPLES_PER_FRAME)
                .collect();
            // The length is guaranteed by the loop; keeping the constructor
            // as the invariant check makes accidental future changes obvious.
            frames.push(ScreenAudioFrame::new(samples).expect("10 ms frame length"));
        }
        Ok(frames)
    }
}

fn to_stereo(frame: &[f32]) -> [f32; 2] {
    match frame {
        [] => [0.0, 0.0],
        [mono] => [*mono, *mono],
        [left, right, rest @ ..] => {
            if rest.is_empty() {
                [*left, *right]
            } else {
                // Preserve a conventional stereo pair and fold any surround
                // channels into the nearest side without adding a dependency.
                let mut left_sum = *left;
                let mut right_sum = *right;
                let mut left_count = 1.0;
                let mut right_count = 1.0;
                for (index, sample) in rest.iter().enumerate() {
                    if index % 2 == 0 {
                        left_sum += *sample;
                        left_count += 1.0;
                    } else {
                        right_sum += *sample;
                        right_count += 1.0;
                    }
                }
                [left_sum / left_count, right_sum / right_count]
            }
        }
    }
}

fn f32_to_i16(sample: f32) -> i16 {
    let sample = sample.clamp(-1.0, 1.0);
    if sample <= -1.0 {
        i16::MIN
    } else {
        (sample * i16::MAX as f32).round() as i16
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioQueueStats {
    pub(crate) depth: usize,
    pub(crate) pushed: u64,
    pub(crate) popped: u64,
    pub(crate) dropped: u64,
}

struct AudioQueueInner {
    frames: Mutex<VecDeque<ScreenAudioFrame>>,
    capacity: usize,
    closed: AtomicBool,
    pushed: AtomicU64,
    popped: AtomicU64,
    dropped: AtomicU64,
    wake: Notify,
}

/// A bounded, oldest-drop queue. `push` never waits for the LiveKit pump, so a
/// slow encoder cannot block an OS audio callback or grow process memory.
#[derive(Clone)]
pub(crate) struct ScreenAudioQueue {
    inner: Arc<AudioQueueInner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScreenAudioIngressStats {
    pub(crate) queue: AudioQueueStats,
    pub(crate) conversion_errors: u64,
    pub(crate) last_good_frame_us: u64,
}

struct ScreenAudioIngressInner {
    queue: ScreenAudioQueue,
    assembler: Mutex<PcmFrameAssembler>,
    conversion_errors: AtomicU64,
    last_good_frame_us: AtomicU64,
}

/// Platform adapters hand copied PCM chunks to this boundary. It keeps the
/// stateful resampler out of an OS callback's ownership model and exposes the
/// same bounded queue to the eventual LiveKit pump on every platform.
#[derive(Clone)]
pub(crate) struct ScreenAudioIngress {
    inner: Arc<ScreenAudioIngressInner>,
}

impl ScreenAudioIngress {
    pub(crate) fn new() -> Self {
        Self::with_queue(ScreenAudioQueue::new())
    }

    pub(crate) fn with_queue(queue: ScreenAudioQueue) -> Self {
        Self {
            inner: Arc::new(ScreenAudioIngressInner {
                queue,
                assembler: Mutex::new(PcmFrameAssembler::default()),
                conversion_errors: AtomicU64::new(0),
                last_good_frame_us: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn queue(&self) -> ScreenAudioQueue {
        self.inner.queue.clone()
    }

    pub(crate) fn push(&self, chunk: PcmChunk) -> Result<usize, PcmError> {
        let frames = self
            .inner
            .assembler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(chunk);
        let frames = match frames {
            Ok(frames) => frames,
            Err(error) => {
                self.inner.conversion_errors.fetch_add(1, Ordering::Relaxed);
                return Err(error);
            }
        };
        let count = frames.len();
        for frame in frames {
            self.inner.queue.push(frame);
        }
        if count != 0 {
            self.inner
                .last_good_frame_us
                .store(crate::time_util::now_us(), Ordering::Relaxed);
        }
        Ok(count)
    }

    pub(crate) fn close(&self) {
        self.inner.queue.close();
    }

    pub(crate) fn stats(&self) -> ScreenAudioIngressStats {
        ScreenAudioIngressStats {
            queue: self.inner.queue.stats(),
            conversion_errors: self.inner.conversion_errors.load(Ordering::Relaxed),
            last_good_frame_us: self.inner.last_good_frame_us.load(Ordering::Relaxed),
        }
    }
}

impl ScreenAudioQueue {
    pub(crate) fn new() -> Self {
        Self::with_capacity(SCREEN_AUDIO_QUEUE_FRAMES)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(AudioQueueInner {
                frames: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
                capacity: capacity.max(1),
                closed: AtomicBool::new(false),
                pushed: AtomicU64::new(0),
                popped: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                wake: Notify::new(),
            }),
        }
    }

    pub(crate) fn push(&self, frame: ScreenAudioFrame) -> bool {
        if self.inner.closed.load(Ordering::Acquire) {
            return false;
        }
        let mut frames = self
            .inner
            .frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.inner.closed.load(Ordering::Acquire) {
            return false;
        }
        if frames.len() == self.inner.capacity {
            frames.pop_front();
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
        }
        frames.push_back(frame);
        self.inner.pushed.fetch_add(1, Ordering::Relaxed);
        drop(frames);
        self.inner.wake.notify_one();
        true
    }

    pub(crate) fn try_pop(&self) -> Option<ScreenAudioFrame> {
        let frame = self
            .inner
            .frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        if frame.is_some() {
            self.inner.popped.fetch_add(1, Ordering::Relaxed);
        }
        frame
    }

    pub(crate) async fn pop(&self) -> Option<ScreenAudioFrame> {
        loop {
            let notified = self.inner.wake.notified();
            if let Some(frame) = self.try_pop() {
                return Some(frame);
            }
            if self.inner.closed.load(Ordering::Acquire) {
                return None;
            }
            notified.await;
        }
    }

    pub(crate) fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.wake.notify_waiters();
    }

    pub(crate) fn stats(&self) -> AudioQueueStats {
        AudioQueueStats {
            depth: self
                .inner
                .frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            pushed: self.inner.pushed.load(Ordering::Relaxed),
            popped: self.inner.popped.load(Ordering::Relaxed),
            dropped: self.inner.dropped.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32) -> AudioSourceKey {
        AudioSourceKey::Process(pid)
    }

    fn frame(value: i16) -> ScreenAudioFrame {
        ScreenAudioFrame::new(vec![value; SCREEN_AUDIO_SAMPLES_PER_FRAME]).unwrap()
    }

    #[test]
    fn source_resolution_is_conservative() {
        assert_eq!(
            AudioSourceKey::for_share(SharedSourceKind::Display, None),
            Some(AudioSourceKey::SystemOutput)
        );
        assert_eq!(
            AudioSourceKey::for_share(SharedSourceKind::DisplayRegion, None),
            Some(AudioSourceKey::SystemOutput)
        );
        assert_eq!(
            AudioSourceKey::for_share(SharedSourceKind::Window, Some(42)),
            Some(process(42))
        );
        assert_eq!(
            AudioSourceKey::for_share(SharedSourceKind::Window, None),
            None
        );
        assert_eq!(
            AudioSourceKey::for_share(SharedSourceKind::Window, Some(0)),
            None
        );
    }

    #[test]
    fn labels_are_stable_and_source_specific() {
        assert_eq!(
            AudioSourceKey::SystemOutput.label(),
            "petal-window-audio-system"
        );
        assert_eq!(process(42).track_name(), "petal-window-audio-process-42");
    }

    #[test]
    fn fresh_registry_is_default_off_and_starts_nothing() {
        let registry = AudioSourceRegistry::default();
        assert!(registry.active_source_keys().is_empty());
        assert_eq!(registry.reference_count(AudioSourceKey::SystemOutput), 0);
    }

    #[test]
    fn duplicate_acquisition_does_not_start_a_second_source() {
        let mut registry = AudioSourceRegistry::default();
        assert_eq!(
            registry.acquire(1, process(42)),
            vec![AudioSourceTransition::Start(process(42))]
        );
        assert!(registry.acquire(2, process(42)).is_empty());
        assert_eq!(registry.reference_count(process(42)), 2);
        assert!(registry.is_active(process(42)));
    }

    #[test]
    fn system_output_suppresses_and_then_restores_process_audio() {
        let mut registry = AudioSourceRegistry::default();
        registry.acquire(1, process(42));
        assert_eq!(
            registry.acquire(2, AudioSourceKey::SystemOutput),
            vec![
                AudioSourceTransition::Stop(process(42)),
                AudioSourceTransition::Start(AudioSourceKey::SystemOutput),
            ]
        );
        assert!(!registry.is_active(process(42)));
        assert_eq!(
            registry.release(2),
            vec![
                AudioSourceTransition::Stop(AudioSourceKey::SystemOutput),
                AudioSourceTransition::Start(process(42))
            ]
        );
        assert!(registry.is_active(process(42)));
    }

    #[test]
    fn replacing_a_share_is_atomic_with_respect_to_precedence() {
        let mut registry = AudioSourceRegistry::default();
        registry.acquire(1, process(42));
        registry.acquire(2, AudioSourceKey::SystemOutput);
        assert!(registry.acquire(1, process(43)).is_empty());
        assert_eq!(registry.reference_count(process(42)), 0);
        assert_eq!(registry.source_for_share(1), Some(process(43)));
    }

    #[test]
    fn stale_incarnation_release_cannot_touch_a_new_share() {
        let mut registry = AudioSourceRegistry::default();
        registry.acquire(10, process(42));
        assert_eq!(
            registry.release(10),
            vec![AudioSourceTransition::Stop(process(42))]
        );
        assert_eq!(
            registry.acquire(11, process(42)),
            vec![AudioSourceTransition::Start(process(42))]
        );
        assert!(registry.release(10).is_empty());
        assert_eq!(registry.reference_count(process(42)), 1);
        assert!(registry.is_active(process(42)));
    }

    #[test]
    fn unknown_release_is_idempotent() {
        let mut registry = AudioSourceRegistry::default();
        assert!(registry.release(99).is_empty());
        assert_eq!(registry.reference_count(AudioSourceKey::SystemOutput), 0);
    }

    #[test]
    fn publication_health_prefers_sid_then_stable_name() {
        assert_eq!(
            screen_audio_publication_health(
                "sid-1",
                "petal-window-audio-system",
                [
                    ("sid-1", "old-name"),
                    ("sid-2", "petal-window-audio-system"),
                ],
            ),
            ScreenAudioPublicationHealth::CurrentSidPresent
        );
        assert_eq!(
            screen_audio_publication_health(
                "sid-1",
                "petal-window-audio-system",
                [("sid-2", "petal-window-audio-system")],
            ),
            ScreenAudioPublicationHealth::ReplacementAlreadyPresent
        );
        assert_eq!(
            screen_audio_publication_health(
                "sid-1",
                "petal-window-audio-system",
                [("sid-2", "other")],
            ),
            ScreenAudioPublicationHealth::Missing
        );
    }

    #[test]
    fn pcm_converts_and_upmixes_mono() {
        let mut samples = Vec::new();
        for _ in 0..160 {
            samples.extend_from_slice(&[0, i16::MAX, i16::MIN]);
        }
        let mut assembler = PcmFrameAssembler::default();
        let frames = assembler
            .push(PcmChunk::from_i16(samples, 48_000, 1).unwrap())
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].samples()[0], 0);
        assert_eq!(frames[0].samples()[1], 0);
        assert_eq!(frames[0].samples()[2], i16::MAX);
        assert_eq!(frames[0].samples()[3], i16::MAX);
        assert_eq!(frames[0].samples()[4], i16::MIN);
        assert_eq!(frames[0].samples()[5], i16::MIN);
    }

    #[test]
    fn pcm_rejects_malformed_shapes() {
        assert_eq!(
            PcmChunk::from_i16(vec![1], 48_000, 0),
            Err(PcmError::InvalidChannelCount)
        );
        assert_eq!(
            PcmChunk::from_i16(vec![1], 0, 1),
            Err(PcmError::InvalidSampleRate)
        );
        assert_eq!(
            PcmChunk::from_i16(vec![1, 2, 3], 48_000, 2),
            Err(PcmError::PartialInterleavedFrame {
                samples: 3,
                channels: 2
            })
        );
    }

    #[test]
    fn assembler_resamples_44_1khz_into_fixed_10ms_frames() {
        let input = vec![0.25; 4_410 * SCREEN_AUDIO_CHANNELS];
        let mut assembler = PcmFrameAssembler::default();
        let first = assembler
            .push(
                PcmChunk::from_f32(input[..4_410 * SCREEN_AUDIO_CHANNELS].to_vec(), 44_100, 2)
                    .unwrap(),
            )
            .unwrap();
        let second = assembler
            .push(PcmChunk::from_f32(input, 44_100, 2).unwrap())
            .unwrap();
        let frames = first.into_iter().chain(second).collect::<Vec<_>>();
        // The final source sample is retained as lookahead; the incomplete
        // tail is intentionally held/dropped rather than padded on stop.
        assert_eq!(frames.len(), 19);
        assert!(frames
            .iter()
            .all(|frame| frame.samples().len() == SCREEN_AUDIO_SAMPLES_PER_FRAME));
        assert_eq!(frames[0].samples()[0], 8_192);
        assert_eq!(frames[0].samples()[1], 8_192);
    }

    #[test]
    fn queue_drops_oldest_frame_and_rejects_after_close() {
        let queue = ScreenAudioQueue::with_capacity(2);
        assert!(queue.push(frame(1)));
        assert!(queue.push(frame(2)));
        assert!(queue.push(frame(3)));
        assert_eq!(queue.try_pop().unwrap().samples()[0], 2);
        assert_eq!(queue.try_pop().unwrap().samples()[0], 3);
        assert_eq!(queue.try_pop(), None);
        let stats = queue.stats();
        assert_eq!(stats.pushed, 3);
        assert_eq!(stats.popped, 2);
        assert_eq!(stats.dropped, 1);
        queue.close();
        assert!(!queue.push(frame(4)));
    }

    #[tokio::test]
    async fn closing_queue_wakes_waiting_consumer() {
        let queue = ScreenAudioQueue::with_capacity(1);
        let waiter = queue.clone();
        let task = tokio::spawn(async move { waiter.pop().await });
        tokio::task::yield_now().await;
        queue.close();
        assert_eq!(task.await.unwrap(), None);
    }
}
