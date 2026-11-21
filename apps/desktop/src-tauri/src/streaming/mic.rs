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
//! switch) and mute. Mute is applied twice: at the mix, from the next block
//! (a mute gate on [`MIC_SOURCE`], so audio already captured or queued never
//! goes out), and here, read per chunk: silence while muted, and after an
//! unmute, silence for every chunk captured before it (each chunk carries
//! its capture time), so a backlog or a partial block from the muted span
//! never goes out late. #487.
//! Platform-neutral; client mode uses it on Windows and macOS.
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
use crate::transport::audio::MuteRecord;

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
/// Margin on top of a chunk's reported capture latency and its own length
/// when deciding whether any of it predates an unmute.
const CAPTURE_SLACK: Duration = Duration::from_millis(20);

/// One device callback's samples (mono, 48 kHz).
struct Chunk {
    /// When the callback ran.
    at: Instant,
    /// How long before the callback the device captured the data (cpal's
    /// capture vs. callback timestamps).
    latency: Duration,
    samples: Vec<i16>,
}

/// The meeting's microphone, as the stream sees it. Read-only.
pub trait MeetingMic: Send + Sync {
    /// The meeting's mute and when it was last unmuted, read as one value
    /// (an unmute the stream mic never got to see still bounds what it may
    /// send). `None`: the meeting has no microphone (audio disabled, or left).
    fn mute(&self) -> Option<MuteRecord>;
    /// The selected recording device's name; `None` means the system default.
    fn device_name(&self) -> Option<String>;
}

/// Mono 48 kHz capture in, processed 10 ms blocks out to the stream mix.
pub struct MicProcessor {
    apm: Option<AudioProcessingModule>,
    pending: VecDeque<i16>,
    /// `pending` holds samples fed as muted: the block they complete is muted.
    pending_muted: bool,
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
            pending_muted: false,
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
        let mut carried_muted = std::mem::take(&mut self.pending_muted);
        while self.pending.len() >= APM_FRAMES {
            let block_muted = muted || std::mem::take(&mut carried_muted);
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
            if block_muted {
                block.fill(0);
            }
            self.processed.add(&block);
            peak = peak.max(block.iter().map(|sample| sample.unsigned_abs()).max().unwrap_or(0));
            mixer.lock_unpoisoned().push(MIC_SOURCE, &block, 1);
        }
        self.pending_muted = carried_muted || (muted && !self.pending.is_empty());
        peak
    }
}

/// Follows the meeting's mute for captured chunks: a chunk is fed muted
/// while the meeting is muted, and after an unmute, if any of it may have
/// been captured before the unmute was seen.
#[derive(Debug)]
struct MuteFollower {
    muted: bool,
    /// When the last unmute was seen.
    unmuted_at: Option<Instant>,
}

impl MuteFollower {
    fn new(muted: bool) -> Self {
        Self { muted, unmuted_at: None }
    }

    /// The meeting's mute now (`now`; last unmuted at `meeting_unmuted`, as
    /// the meeting recorded it), for a chunk of `samples` delivered by a
    /// callback at `stamp`, `latency` after the device captured it: whether
    /// to feed it as muted.
    #[allow(clippy::too_many_arguments)]
    fn chunk_muted(
        &mut self,
        muted: bool,
        meeting_unmuted: Option<Instant>,
        now: Instant,
        stamp: Instant,
        latency: Duration,
        samples: usize,
    ) -> bool {
        if self.muted && !muted {
            self.unmuted_at = Some(now);
        }
        // A whole mute this thread never saw (it was busy or stalled).
        self.unmuted_at = self.unmuted_at.max(meeting_unmuted);
        self.muted = muted;
        if muted {
            return true;
        }
        let length = Duration::from_secs_f64(samples as f64 / f64::from(MIX_SAMPLE_RATE));
        // Its earliest sample, conservatively; before the unmute: muted.
        let reach = latency + length + CAPTURE_SLACK;
        self.unmuted_at.is_some_and(|unmuted| stamp.checked_sub(reach).is_none_or(|first| first < unmuted))
    }

    /// Whether to feed `chunk` as muted, by the meeting's mute as of now.
    fn chunk_muted_now(&mut self, meeting: &dyn MeetingMic, chunk: &Chunk) -> bool {
        let record = meeting.mute().unwrap_or(MuteRecord::new(true));
        self.chunk_muted(record.muted, record.unmuted_at, Instant::now(), chunk.at, chunk.latency, chunk.samples.len())
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
    chunks: Receiver<Chunk>,
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

/// How long before its callback a chunk's data was captured.
fn capture_latency(info: &cpal::InputCallbackInfo) -> Duration {
    let timestamp = info.timestamp();
    timestamp.callback.duration_since(timestamp.capture)
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
    let (sender, chunks) = sync_channel::<Chunk>(CAPTURE_QUEUE);
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
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                // Never block the device's thread: a full queue drops audio.
                let _ = sender.try_send(Chunk { at: Instant::now(), latency: capture_latency(info), samples: converter.run(data) });
            },
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            config.config(),
            move |data: &[i16], info: &cpal::InputCallbackInfo| {
                let as_f32: Vec<f32> = data.iter().map(|sample| f32::from(*sample) / 32768.0).collect();
                let _ = sender.try_send(Chunk { at: Instant::now(), latency: capture_latency(info), samples: converter.run(&as_f32) });
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
        let gate_meeting = meeting.clone();
        // No meeting mic (`None`) counts as muted.
        mixer.lock_unpoisoned().set_mute_gate(MIC_SOURCE, Arc::new(move || gate_meeting.mute().is_none_or(|record| record.muted)));
        let thread_mixer = mixer.clone();
        let thread = std::thread::Builder::new()
            .name("petal-stream-mic".into())
            .spawn(move || {
                run(meeting.as_ref(), &thread_mixer, &reference, &thread_stop);
                let mut mixer = thread_mixer.lock_unpoisoned();
                mixer.remove_source(MIC_SOURCE);
                mixer.clear_mute_gate(MIC_SOURCE);
            })
            .map_err(|error| {
                log::warn!("streaming: couldn't start the stream mic thread: {error}");
                mixer.lock_unpoisoned().clear_mute_gate(MIC_SOURCE);
            })
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
    let mut muted = meeting.mute().map(|record| record.muted);
    let mut follower = MuteFollower::new(muted.unwrap_or(true));
    let mut device = meeting.device_name();
    let mut muted_at = Instant::now();
    let mut device_at = Instant::now();
    let mut retry_at = Instant::now();
    // The last open error, so a machine with no mic logs it once, not every retry.
    let mut open_error: Option<String> = None;
    let mut level_at = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= muted_at + MUTE_POLL {
            muted = meeting.mute().map(|record| record.muted);
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
            // No meeting mic: nothing to capture, and the next mic starts
            // from a mute boundary.
            capture = None;
            follower = MuteFollower::new(true);
            std::thread::sleep(MUTE_POLL);
            continue;
        };
        if capture.as_ref().is_some_and(|open| open.failed.load(Ordering::Acquire)) {
            capture = None;
            retry_at = now + RETRY_AFTER;
        }
        if capture.is_none() && now >= retry_at {
            match open_capture(device.as_deref()) {
                Ok(open) => {
                    capture = Some(open);
                    open_error = None;
                }
                Err(error) => {
                    if open_error.as_deref() != Some(error.as_str()) {
                        log::warn!("streaming: stream mic unavailable: {error}");
                        open_error = Some(error);
                    }
                    retry_at = now + RETRY_AFTER;
                }
            }
        }
        match capture.as_ref() {
            Some(open) => match open.chunks.recv_timeout(MUTE_POLL) {
                Ok(chunk) => {
                    // Mute as of this chunk, not the last poll.
                    let chunk_muted = follower.chunk_muted_now(meeting, &chunk);
                    processor.feed(&chunk.samples, chunk_muted, reference, mixer);
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

    /// #487: after an unmute, chunks captured before it (a backlog read
    /// late, or one queued just after the unmute was seen) go out silent.
    #[test]
    fn chunks_captured_before_an_unmute_stay_silent() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let none = Duration::ZERO;
        let mut follower = MuteFollower::new(true);
        assert!(follower.chunk_muted(true, None, at(100), at(100), none, 480), "muted");
        // Unmute seen at 500 ms while reading a backlog captured at 300 ms.
        assert!(follower.chunk_muted(false, None, at(500), at(300), none, 480));
        // Captured around the unmute, queued after it was seen.
        assert!(follower.chunk_muted(false, None, at(510), at(505), none, 480));
        // Delivered well after, but the device reports it captured 60 ms
        // before its callback: from before the unmute.
        assert!(follower.chunk_muted(false, None, at(560), at(560), Duration::from_millis(60), 480));
        // Captured wholly after it.
        assert!(!follower.chunk_muted(false, None, at(540), at(535), none, 480));
        assert!(!follower.chunk_muted(false, None, at(600), at(600), Duration::from_millis(60), 480));
        // Mute again: silent at once, and the next unmute starts over.
        assert!(follower.chunk_muted(true, None, at(700), at(700), none, 480));
        assert!(follower.chunk_muted(false, None, at(800), at(750), none, 480));
        assert!(!follower.chunk_muted(false, None, at(850), at(845), none, 480));
        // A whole mute the thread never saw: the meeting's record still
        // bounds it (captured at 880-900 ms, unmuted at 950 ms, read at 990).
        let mut stalled = MuteFollower::new(false);
        assert!(stalled.chunk_muted(false, Some(at(950)), at(990), at(900), none, 960));
        assert!(!stalled.chunk_muted(false, Some(at(950)), at(1000), at(1000), none, 480));
        // Never muted: nothing held back.
        let mut open = MuteFollower::new(false);
        assert!(!open.chunk_muted(false, None, at(10), at(0), none, 480));
    }

    /// The meeting's mute as macOS's stream mic reads it
    /// (`streaming::session::MeetingMicState`): the session's request record.
    #[cfg(target_os = "macos")]
    struct SessionMic(Arc<crate::session::SessionState>);

    #[cfg(target_os = "macos")]
    impl MeetingMic for SessionMic {
        fn mute(&self) -> Option<MuteRecord> {
            Some(self.0.stream_mute())
        }
        fn device_name(&self) -> Option<String> {
            None
        }
    }

    #[cfg(target_os = "macos")]
    fn chunk_at(at: Instant) -> Chunk {
        Chunk { at, latency: Duration::ZERO, samples: vec![0; APM_FRAMES] }
    }

    /// A mute followed at once by an unmute, through the session's own entry
    /// points, before the stream mic reads anything (and before the live
    /// track's coalesced apply would ever run): what was captured in between
    /// stays silent.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_coalesced_mute_still_silences_the_stream() {
        let state = Arc::new(crate::session::SessionState::default());
        let meeting = SessionMic(state.clone());
        state.set_mic_muted(false);
        std::thread::sleep(Duration::from_millis(50));
        let mut follower = MuteFollower::new(false);
        assert!(!follower.chunk_muted_now(&meeting, &chunk_at(Instant::now())), "heard while unmuted");

        state.set_mic_muted(true);
        let spoken = Instant::now();
        state.set_mic_muted(false);
        assert!(follower.chunk_muted_now(&meeting, &chunk_at(spoken)), "captured while muted");

        state.set_ai_chat_ducking(true);
        let ducked = Instant::now();
        state.set_ai_chat_ducking(false);
        assert!(follower.chunk_muted_now(&meeting, &chunk_at(ducked)), "captured while ducked");

        std::thread::sleep(Duration::from_millis(50));
        assert!(!follower.chunk_muted_now(&meeting, &chunk_at(Instant::now())), "heard after");
    }

    /// The stream mic never reads "unmuted" together with an unmute time from
    /// before the latest mute, however the reads and requests interleave.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_stream_reads_mute_and_unmute_time_together() {
        let state = Arc::new(crate::session::SessionState::default());
        let base = Instant::now();
        // When the latest mute request returned, in ns after `base`.
        let muted_by = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let writer = {
            let (state, muted_by, done) = (state.clone(), muted_by.clone(), done.clone());
            std::thread::spawn(move || {
                for _ in 0..20_000 {
                    state.set_mic_muted(true);
                    muted_by.store(base.elapsed().as_nanos() as u64, Ordering::SeqCst);
                    state.set_mic_muted(false);
                }
                done.store(true, Ordering::SeqCst);
            })
        };
        let meeting = SessionMic(state);
        let mut unmuted_reads = 0u32;
        while !done.load(Ordering::SeqCst) {
            let floor = base + Duration::from_nanos(muted_by.load(Ordering::SeqCst));
            let record = meeting.mute().expect("a meeting mic");
            if !record.muted && floor > base {
                unmuted_reads += 1;
                assert!(
                    record.unmuted_at.is_some_and(|unmuted| unmuted >= floor),
                    "read unmuted with an unmute time from before the latest mute: {record:?}"
                );
            }
        }
        writer.join().expect("writer");
        assert!(unmuted_reads > 0, "the reader saw the mic unmuted at least once");
    }

    /// Mutes and unmutes requested from two threads at once: every mute the
    /// session ever reported asked for is recorded, so the stream mic never
    /// reads "unmuted" with an unmute time from before it.
    #[cfg(target_os = "macos")]
    #[test]
    fn racing_mute_requests_are_each_recorded() {
        let state = Arc::new(crate::session::SessionState::default());
        let done = Arc::new(AtomicBool::new(false));
        let writers: Vec<_> = [true, false]
            .into_iter()
            .map(|muted| {
                let (state, done) = (state.clone(), done.clone());
                std::thread::spawn(move || {
                    while !done.load(Ordering::SeqCst) {
                        state.set_mic_muted(muted);
                    }
                })
            })
            .collect();
        let meeting = SessionMic(state.clone());
        let mut checked = 0u32;
        for _ in 0..200_000 {
            let before = Instant::now();
            let asked = state.mic_muted();
            let record = meeting.mute().expect("a meeting mic");
            if asked && !record.muted {
                checked += 1;
                assert!(
                    record.unmuted_at.is_some_and(|unmuted| unmuted >= before),
                    "a mute asked for at or after {before:?} went unrecorded: {record:?}"
                );
            }
        }
        done.store(true, Ordering::SeqCst);
        for writer in writers {
            writer.join().expect("writer");
        }
        assert!(checked > 0, "the reader caught a mute being lifted");
    }

    /// A partial block left over from a muted chunk never goes out with the
    /// next, unmuted one, and the echo reference stays aligned.
    #[test]
    fn a_partial_block_fed_muted_stays_muted() {
        let mut processor = MicProcessor::new(false);
        let reference = Mutex::new(AudioMixer::with_prime(0));
        let mixer = Mutex::new(AudioMixer::with_prime(0));
        // 15 ms muted: one silent block, 5 ms held over.
        processor.feed(&[9000; 720], true, &reference, &mixer);
        // 5 ms unmuted completes the held block: still silent.
        processor.feed(&[1200; 240], false, &reference, &mixer);
        assert!(mixer.lock_unpoisoned().pull(2 * APM_FRAMES).iter().all(|s| *s == 0));
        // The next whole block is heard.
        processor.feed(&[1200; 480], false, &reference, &mixer);
        assert!(mixer.lock_unpoisoned().pull(APM_FRAMES).iter().all(|s| (1199..=1201).contains(s)));
    }

    #[test]
    fn the_stream_mic_gates_its_source_at_the_mix() {
        // No meeting mic: nothing is opened, and it counts as muted.
        struct NoMic;
        impl MeetingMic for NoMic {
            fn mute(&self) -> Option<MuteRecord> {
                None
            }
            fn device_name(&self) -> Option<String> {
                None
            }
        }
        let loud = || vec![5000i16; 4 * 1024 * MIX_CHANNELS];
        let mixer = Arc::new(Mutex::new(AudioMixer::new()));
        let reference = Arc::new(Mutex::new(AudioMixer::with_prime(0)));
        let mic = StreamMic::start(Arc::new(NoMic), mixer.clone(), reference);
        // Speech in the mic's slot, as a capture behind the mute would push.
        mixer.lock_unpoisoned().push(MIC_SOURCE, &loud(), MIX_CHANNELS);
        let (block, _) = crate::streaming::mixer::pull_block_gated(&mixer);
        assert!(block.iter().all(|s| *s == 0), "muted at the mix");
        drop(mic);
        assert_eq!(mixer.lock_unpoisoned().source_count(), 0, "removed on stop");
        // The gate leaves with the mic.
        mixer.lock_unpoisoned().push(MIC_SOURCE, &loud(), MIX_CHANNELS);
        let (block, _) = crate::streaming::mixer::pull_block_gated(&mixer);
        assert!(block.iter().all(|s| *s == 5000));
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
        fn mute(&self) -> Option<MuteRecord> {
            Some(MuteRecord::new(false))
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
