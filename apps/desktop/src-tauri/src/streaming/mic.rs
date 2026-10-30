//! The sharer's microphone for client mode.
//!
//! The meeting mic is published through LiveKit's platform audio device
//! module, which hands audio straight to the call's encoder: the track has no
//! frame tap. So the stream opens its own shared-mode capture of the same
//! device with `cpal`, as AI chat does (`ai_chat/audio.rs`), and the call's
//! audio path is untouched. Every 10 ms block goes through WebRTC's audio
//! processing (echo cancellation against what Petal plays out, noise
//! suppression, digital-only gain that never touches the device's input
//! level) and then into the stream mix.
//!
//! It follows the meeting: the selected recording device (reopened on a
//! switch) and mute (silence while muted). Platform-neutral; client mode uses
//! it on Windows and macOS.
#![cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use livekit::webrtc::native::apm::AudioProcessingModule;

use super::mixer::{AudioMixer, LevelMeter, MIX_CHANNELS, MIX_SAMPLE_RATE};
use crate::ai_chat::audio::{downmix_to_mono, Resampler};
use crate::sync_ext::MutexExt;

/// The stream mix's name for this source.
pub const MIC_SOURCE: &str = "local-mic";
/// One audio-processing block: 10 ms at 48 kHz.
const APM_FRAMES: usize = (MIX_SAMPLE_RATE / 100) as usize;
/// How often the meeting's mute state is re-read.
const MUTE_POLL: Duration = Duration::from_millis(250);
/// How often the meeting's recording device is re-read (enumeration is not
/// free; the call's own hot-swap poll runs at the same pace).
const DEVICE_POLL: Duration = Duration::from_secs(2);
/// After a capture error or a failed open, wait before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(2);
const LEVEL_LOG_EVERY: Duration = Duration::from_secs(30);
/// Captured chunks waiting for the processing thread (~1 s at 10 ms).
const CAPTURE_QUEUE: usize = 100;

/// The meeting's microphone, as the stream sees it. Read-only.
pub trait MeetingMic: Send + Sync {
    /// `None`: the meeting has no microphone (audio disabled, or left).
    fn muted(&self) -> Option<bool>;
    /// The selected recording device's name; `None` means the system default.
    fn device_name(&self) -> Option<String>;
}

/// Mono 48 kHz capture in, processed 10 ms blocks out to the stream mix.
pub struct MicProcessor {
    apm: Option<AudioProcessingModule>,
    pending: VecDeque<i16>,
    /// What the device delivered, and what went to the mix.
    raw: LevelMeter,
    processed: LevelMeter,
}

impl MicProcessor {
    /// `processing`: run WebRTC's audio processing (off only in tests).
    pub fn new(processing: bool) -> Self {
        Self {
            apm: processing.then(|| AudioProcessingModule::new(true, true, true, true)),
            pending: VecDeque::new(),
            raw: LevelMeter::default(),
            processed: LevelMeter::default(),
        }
    }

    /// "raw … processed …" levels since the last call.
    pub fn take_levels(&mut self) -> String {
        format!("raw {} processed {}", self.raw.take(), self.processed.take())
    }

    /// Queue captured samples; every complete block is processed against the
    /// matching slice of `reference` (what Petal plays out) and pushed to
    /// `mixer`, as silence while `muted`. Returns the loudest processed sample.
    pub fn feed(
        &mut self,
        samples: &[i16],
        muted: bool,
        reference: &Mutex<AudioMixer>,
        mixer: &Mutex<AudioMixer>,
    ) -> u16 {
        self.pending.extend(samples);
        let mut peak = 0u16;
        while self.pending.len() >= APM_FRAMES {
            let mut block: Vec<i16> = self.pending.drain(..APM_FRAMES).collect();
            self.raw.add(&block);
            let mut far = reference.lock_unpoisoned().pull(APM_FRAMES);
            if let Some(apm) = self.apm.as_mut() {
                // The far end first: echo cancellation needs it to lead.
                if let Err(error) = apm.process_reverse_stream(&mut far, MIX_SAMPLE_RATE as i32, MIX_CHANNELS as i32) {
                    log::debug!("streaming: echo reference rejected: {}", error.message);
                }
                if let Err(error) = apm.process_stream(&mut block, MIX_SAMPLE_RATE as i32, 1) {
                    log::debug!("streaming: mic processing failed: {}", error.message);
                }
            }
            if muted {
                block.fill(0);
            }
            self.processed.add(&block);
            peak = peak.max(block.iter().map(|sample| sample.unsigned_abs()).max().unwrap_or(0));
            mixer.lock_unpoisoned().push(MIC_SOURCE, &block, 1);
        }
        peak
    }
}

/// Which of `names` is the meeting's device: an exact name, then the same
/// name ignoring case, then one name containing the other (APIs differ in
/// how much of the friendly name they report). `None`: use the default.
pub fn pick_device(names: &[String], wanted: Option<&str>) -> Option<usize> {
    let wanted = wanted?.trim();
    if wanted.is_empty() {
        return None;
    }
    let lower = wanted.to_lowercase();
    names
        .iter()
        .position(|name| name == wanted)
        .or_else(|| names.iter().position(|name| name.to_lowercase() == lower))
        .or_else(|| {
            names.iter().position(|name| {
                let name = name.to_lowercase();
                !name.is_empty() && (name.contains(&lower) || lower.contains(&name))
            })
        })
}

/// Interleaved device audio to mono 48 kHz PCM.
struct Converter {
    channels: usize,
    resampler: Resampler,
}

impl Converter {
    fn run(&mut self, interleaved: &[f32]) -> Vec<i16> {
        let mono = downmix_to_mono(interleaved, self.channels);
        self.resampler
            .process(&mono)
            .into_iter()
            .map(|sample| (sample * 32767.0).round().clamp(-32768.0, 32767.0) as i16)
            .collect()
    }
}

struct OpenCapture {
    _stream: cpal::Stream,
    chunks: Receiver<Vec<i16>>,
    failed: Arc<AtomicBool>,
    /// Buffer under/overruns: a glitch at worst, the stream keeps running.
    glitches: Arc<AtomicU64>,
}

/// Whether a capture error means the stream is gone and must be reopened.
/// An under/overrun (WASAPI flags one on a fresh stream's first buffer), a
/// followed route change or a refused real-time boost leave it running.
fn is_fatal(kind: cpal::ErrorKind) -> bool {
    !matches!(kind, cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied)
}

fn device_label(device: &cpal::Device) -> String {
    device.description().map(|d| d.name().to_string()).unwrap_or_default()
}

fn open_capture(wanted: Option<&str>) -> Result<OpenCapture, String> {
    let host = cpal::default_host();
    let devices: Vec<cpal::Device> = host
        .input_devices()
        .map_err(|error| format!("couldn't list input devices: {error}"))?
        .collect();
    let names: Vec<String> = devices.iter().map(device_label).collect();
    let device = match pick_device(&names, wanted) {
        Some(index) => devices.into_iter().nth(index).ok_or("input device vanished")?,
        None => {
            if let Some(wanted) = wanted {
                log::warn!("streaming: stream mic: '{wanted}' not found; using the system default");
            }
            host.default_input_device().ok_or("no input device")?
        }
    };
    let name = device_label(&device);
    let config = device
        .default_input_config()
        .map_err(|error| format!("no input format for '{name}': {error}"))?;
    let channels = usize::from(config.channels()).max(1);
    let rate = config.sample_rate();
    let (sender, chunks) = sync_channel::<Vec<i16>>(CAPTURE_QUEUE);
    let failed = Arc::new(AtomicBool::new(false));
    let glitches = Arc::new(AtomicU64::new(0));
    let on_error = {
        let failed = failed.clone();
        let glitches = glitches.clone();
        move |error: cpal::Error| {
            if is_fatal(error.kind()) {
                log::warn!("streaming: stream mic capture failed: {error}");
                failed.store(true, Ordering::Release);
            } else {
                glitches.fetch_add(1, Ordering::Relaxed);
            }
        }
    };
    let mut converter = Converter { channels, resampler: Resampler::new(rate, MIX_SAMPLE_RATE) };
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config.config(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                // Never block the device's thread: a full queue drops audio.
                let _ = sender.try_send(converter.run(data));
            },
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            config.config(),
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let as_f32: Vec<f32> = data.iter().map(|sample| f32::from(*sample) / 32768.0).collect();
                let _ = sender.try_send(converter.run(&as_f32));
            },
            on_error,
            None,
        ),
        other => return Err(format!("unsupported input format {other:?} on '{name}'")),
    }
    .map_err(|error| format!("couldn't open '{name}': {error}"))?;
    stream.play().map_err(|error| format!("couldn't start '{name}': {error}"))?;
    log::info!("streaming: stream mic capturing '{name}' ({rate} Hz, {channels} ch)");
    Ok(OpenCapture { _stream: stream, chunks, failed, glitches })
}

/// The stream's microphone capture; stops when dropped.
pub struct StreamMic {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StreamMic {
    pub fn start(
        meeting: Arc<dyn MeetingMic>,
        mixer: Arc<Mutex<AudioMixer>>,
        reference: Arc<Mutex<AudioMixer>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("petal-stream-mic".into())
            .spawn(move || {
                run(meeting.as_ref(), &mixer, &reference, &thread_stop);
                mixer.lock_unpoisoned().remove_source(MIC_SOURCE);
            })
            .map_err(|error| log::warn!("streaming: couldn't start the stream mic thread: {error}"))
            .ok();
        Self { stop, thread }
    }
}

impl Drop for StreamMic {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Owns the capture (a `cpal::Stream` is not `Send` everywhere) and the
/// audio processing; re-reads the meeting's mute and device as it goes.
fn run(meeting: &dyn MeetingMic, mixer: &Mutex<AudioMixer>, reference: &Mutex<AudioMixer>, stop: &AtomicBool) {
    let mut processor = MicProcessor::new(true);
    let mut capture: Option<OpenCapture> = None;
    let mut muted = meeting.muted();
    let mut device = meeting.device_name();
    let mut muted_at = Instant::now();
    let mut device_at = Instant::now();
    let mut retry_at = Instant::now();
    let mut level_at = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= muted_at + MUTE_POLL {
            muted = meeting.muted();
            muted_at = now;
        }
        if now >= device_at + DEVICE_POLL {
            let current = meeting.device_name();
            if current != device {
                log::info!("streaming: stream mic following the meeting's device switch");
                device = current;
                capture = None;
                retry_at = now;
            }
            device_at = now;
        }
        let Some(is_muted) = muted else {
            // No meeting mic: nothing to capture.
            capture = None;
            std::thread::sleep(MUTE_POLL);
            continue;
        };
        if capture.as_ref().is_some_and(|open| open.failed.load(Ordering::Acquire)) {
            capture = None;
            retry_at = now + RETRY_AFTER;
        }
        if capture.is_none() && now >= retry_at {
            match open_capture(device.as_deref()) {
                Ok(open) => capture = Some(open),
                Err(error) => {
                    log::warn!("streaming: stream mic unavailable: {error}");
                    retry_at = now + RETRY_AFTER;
                }
            }
        }
        match capture.as_ref() {
            Some(open) => match open.chunks.recv_timeout(MUTE_POLL) {
                Ok(chunk) => {
                    processor.feed(&chunk, is_muted, reference, mixer);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => capture = None,
            },
            None => std::thread::sleep(MUTE_POLL),
        }
        if level_at.elapsed() >= LEVEL_LOG_EVERY {
            let glitches = capture.as_ref().map_or(0, |open| open.glitches.swap(0, Ordering::Relaxed));
            log::info!(
                "streaming: stream mic {} muted={is_muted} capturing={} glitches={glitches}",
                processor.take_levels(),
                capture.is_some()
            );
            level_at = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn finds_the_meetings_device_by_name() {
        let devices = names(&[
            "Microphone (High Definition Audio Device)",
            "Headset Microphone (CORSAIR VOID WIRELESS v2 Gaming Receiver)",
            "Microphone (Webcam AC410)",
        ]);
        assert_eq!(pick_device(&devices, Some("Microphone (Webcam AC410)")), Some(2));
        assert_eq!(pick_device(&devices, Some("microphone (webcam ac410)")), Some(2));
        assert_eq!(pick_device(&devices, Some("CORSAIR VOID WIRELESS v2 Gaming Receiver")), Some(1));
        assert_eq!(pick_device(&devices, Some("USB Podcast Mic")), None, "unknown: default");
        assert_eq!(pick_device(&devices, None), None);
        assert_eq!(pick_device(&devices, Some("  ")), None);
    }

    /// logs/004.log: every open hit an under/overrun within milliseconds and
    /// was torn down, so the stream never heard the mic.
    #[test]
    fn only_a_lost_stream_reopens_the_capture() {
        assert!(!is_fatal(cpal::ErrorKind::Xrun));
        assert!(!is_fatal(cpal::ErrorKind::DeviceChanged));
        assert!(!is_fatal(cpal::ErrorKind::RealtimeDenied));
        assert!(is_fatal(cpal::ErrorKind::DeviceNotAvailable));
        assert!(is_fatal(cpal::ErrorKind::StreamInvalidated));
    }

    #[test]
    fn feeds_ten_millisecond_blocks_and_silence_while_muted() {
        let mut processor = MicProcessor::new(false);
        let reference = Mutex::new(AudioMixer::with_prime(0));
        let mixer = Mutex::new(AudioMixer::with_prime(0));
        // 25 ms: two blocks out, 5 ms held for the next feed.
        assert_eq!(processor.feed(&[1200; 1200], false, &reference, &mixer), 1200);
        let out = mixer.lock_unpoisoned().pull(2 * APM_FRAMES);
        assert!(out.iter().all(|sample| (1199..=1201).contains(sample)), "{:?}", &out[..4]);
        assert_eq!(processor.pending.len(), 240);
        // Muted: the stream gets silence, whatever the mic hears.
        assert_eq!(processor.feed(&[9000; 720], true, &reference, &mixer), 0);
        assert!(mixer.lock_unpoisoned().pull(2 * APM_FRAMES).iter().all(|sample| *sample == 0));
    }

    #[test]
    fn processes_against_the_echo_reference() {
        let mut processor = MicProcessor::new(true);
        let reference = Mutex::new(AudioMixer::with_prime(0));
        let mixer = Mutex::new(AudioMixer::with_prime(0));
        let tone = |phase: usize| ((phase as f32 * 0.06).sin() * 8000.0) as i16;
        for block in 0..50usize {
            let far: Vec<i16> = (0..APM_FRAMES).flat_map(|i| [tone(block * APM_FRAMES + i); 2]).collect();
            reference.lock_unpoisoned().push("far", &far, MIX_CHANNELS);
            let near: Vec<i16> = (0..APM_FRAMES).map(|i| tone(block * APM_FRAMES + i) / 4).collect();
            processor.feed(&near, false, &reference, &mixer);
            assert!(processor.pending.is_empty(), "block {block} processed");
            assert_eq!(mixer.lock_unpoisoned().pull(APM_FRAMES).len(), APM_FRAMES * MIX_CHANNELS);
        }
        assert_eq!(mixer.lock_unpoisoned().source_count(), 1, "the mic is in the mix");
    }

    struct DefaultMic;

    impl MeetingMic for DefaultMic {
        fn muted(&self) -> Option<bool> {
            Some(false)
        }

        fn device_name(&self) -> Option<String> {
            None
        }
    }

    /// Opens this machine's default microphone for two seconds (nothing
    /// leaves the process): capture, resampling and processing on a real
    /// device. Opt-in: `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a microphone"]
    fn captures_this_machines_microphone() {
        let mixer = Arc::new(Mutex::new(AudioMixer::with_prime(0)));
        let reference = Arc::new(Mutex::new(AudioMixer::with_prime(0)));
        let mic = StreamMic::start(Arc::new(DefaultMic), mixer.clone(), reference);
        let mut frames = 0usize;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(50));
            let mut mixer = mixer.lock_unpoisoned();
            if mixer.source_count() == 1 {
                frames += mixer.pull(APM_FRAMES).len() / MIX_CHANNELS;
            }
        }
        drop(mic);
        assert!(frames >= 10 * APM_FRAMES, "only {frames} frames reached the mix");
        assert_eq!(mixer.lock_unpoisoned().source_count(), 0, "removed on stop");
    }
}
