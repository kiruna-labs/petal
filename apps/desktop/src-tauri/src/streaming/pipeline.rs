//! The client-mode encode thread, over the platform encoders:
//! `windows_encode` (Media Foundation) or `macos_encode` (VideoToolbox and
//! AudioToolbox), which offer the same types. One thread owns the encoder
//! device and both encoders and runs two clocks off one start instant, which
//! is what keeps A/V in sync:
//!
//! * video: the constant-frame-rate tick encodes the tap's latest frame (or
//!   the slate) every `1/fps`, repeating it when the source is idle;
//! * audio: every 1024 samples (21.3 ms) one mixer block goes to AAC.
//!
//! Output is packaged to FLV, interleaved, and pushed to the RTMP queue.
//!
//! On a Windows computer with no D3D11 video device, frames are converted on
//! the CPU ([`super::cpu_convert`]) for Microsoft's software H.264 encoder,
//! and the tap keeps sending CPU copies.
//!
//! If the encoder's D3D11 device is lost (`DXGI_ERROR_DEVICE_REMOVED` or
//! `_RESET`: a driver update or crash, a GPU reset), the device, converter
//! and video encoder are recreated on this thread and the stream carries on,
//! re-presenting the share's latest frame on the new device (the slate if it
//! can't be opened there), like the other Windows D3D11 paths; it ends only
//! if that keeps happening ([`DeviceRecovery`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::clock::CfrSchedule;
use super::flv::{split_annex_b, AacPackager, AvcPackager, Interleaver};
use super::mixer::{AudioMixer, LevelMeter, MIX_BLOCK_FRAMES, MIX_SAMPLE_RATE};
use super::plan::letterbox_rect;
use super::queue::MediaQueue;
use super::slate::{render_slate, SlateClock, SlateDecision, STREAM_SLATE_MAX};
#[cfg(target_os = "macos")]
use super::macos_encode::{
    AacEncoder, BgraView, Converter, EncodedVideo, EncoderDevice, H264Encoder, MediaRuntime,
};
use super::tap::{LiveTap, TapFrame};
#[cfg(target_os = "windows")]
use super::cpu_convert::CpuConverter;
#[cfg(target_os = "windows")]
use super::tap::TapPixels;
#[cfg(target_os = "windows")]
use super::windows_encode::{
    AacEncoder, BgraView, Converter, EncodedVideo, EncoderDevice, H264Encoder, MediaRuntime,
};

#[derive(Debug, Clone, Copy)]
pub struct PipelineConfig {
    pub canvas: (u32, u32),
    pub fps: u32,
    pub bitrate_bps: u32,
    pub max_bitrate_bps: u32,
}

#[derive(Debug)]
pub enum PipelineCommand {
    /// The tap moved to another share ("Resume/Switch live stream here"):
    /// upload its first frame even if the generation number repeats.
    Rebound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineExit {
    Stopped,
    SlateTimeout,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct PipelineStats {
    pub frames_encoded: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub on_slate: AtomicBool,
    pub slate_remaining_s: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub fps: AtomicU32,
    pub hardware: AtomicBool,
}

pub struct PipelineHandle {
    pub thread: std::thread::JoinHandle<PipelineExit>,
    pub commands: std::sync::mpsc::Sender<PipelineCommand>,
    pub stats: Arc<PipelineStats>,
    pub stop: Arc<AtomicBool>,
}

pub fn spawn(
    config: PipelineConfig,
    tap: Arc<LiveTap>,
    mixer: Arc<Mutex<AudioMixer>>,
    queue: Arc<MediaQueue>,
) -> Result<PipelineHandle, String> {
    spawn_with(config, tap, mixer, queue, false)
}

/// `force_cpu`: the CPU path even where a GPU is present (its tests only).
fn spawn_with(
    config: PipelineConfig,
    tap: Arc<LiveTap>,
    mixer: Arc<Mutex<AudioMixer>>,
    queue: Arc<MediaQueue>,
    force_cpu: bool,
) -> Result<PipelineHandle, String> {
    let (commands, command_rx) = std::sync::mpsc::channel();
    let stats = Arc::new(PipelineStats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stats = stats.clone();
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("petal-stream-encode".into())
        .spawn(move || {
            let exit = match run(config, &tap, &mixer, &queue, &command_rx, &thread_stats, &thread_stop, force_cpu)
            {
                Ok(exit) => exit,
                Err(error) => PipelineExit::Failed(error),
            };
            // Unblock the publisher whatever happened here.
            queue.close();
            exit
        })
        .map_err(|error| format!("couldn't start the encoder thread: {error}"))?;
    Ok(PipelineHandle { thread, commands, stats, stop })
}

/// How many times within [`DEVICE_LOSS_WINDOW`] the encoder's device may be
/// lost and recreated before the stream ends.
const DEVICE_LOSS_LIMIT: usize = 3;
const DEVICE_LOSS_WINDOW: Duration = Duration::from_secs(60);

/// Whether a lost encoder device may be recreated: up to
/// [`DEVICE_LOSS_LIMIT`] times in any [`DEVICE_LOSS_WINDOW`].
#[derive(Debug, Default)]
struct DeviceRecovery {
    recent: VecDeque<Instant>,
}

impl DeviceRecovery {
    fn allow(&mut self, now: Instant) -> bool {
        while self.recent.front().is_some_and(|at| now.saturating_duration_since(*at) >= DEVICE_LOSS_WINDOW) {
            self.recent.pop_front();
        }
        if self.recent.len() >= DEVICE_LOSS_LIMIT {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

/// Test hook: the next video step fails as if the device had been lost.
#[cfg(test)]
static INJECT_DEVICE_LOSS: AtomicU32 = AtomicU32::new(0);

/// Why the video path's device is gone, if it is.
fn device_lost(video: &Video) -> Option<String> {
    #[cfg(test)]
    if INJECT_DEVICE_LOSS.load(Ordering::Acquire) > 0 {
        INJECT_DEVICE_LOSS.fetch_sub(1, Ordering::AcqRel);
        return Some("injected for a test".into());
    }
    match &video.frames {
        Frames::Gpu { device, .. } => device.lost(),
        #[cfg(target_os = "windows")]
        Frames::Cpu(_) => None,
    }
}

/// Where frames become NV12: on the encoder's D3D11 device, or (Windows, no
/// video device) on the CPU.
enum Frames {
    Gpu { device: EncoderDevice, converter: Converter },
    #[cfg(target_os = "windows")]
    Cpu(CpuConverter),
}

impl Frames {
    /// A BGRA frame: the slate, or a share's CPU copy.
    fn upload(&mut self, frame: &BgraView<'_>) -> Result<(), String> {
        match self {
            Self::Gpu { converter, .. } => converter.upload(frame),
            #[cfg(target_os = "windows")]
            Self::Cpu(converter) => converter.upload(frame),
        }
    }

    /// Hand a share frame to the converter: a GPU tap texture or CPU BGRA.
    /// `Ok(false)`: not uploaded (a GPU frame after the GPU tap was turned
    /// off, or on the CPU path, which never accepts them; the CPU copy
    /// replaces it with the capture's next frame).
    #[cfg(target_os = "windows")]
    fn upload_share(&mut self, frame: &TapFrame, tap: &LiveTap) -> Result<bool, String> {
        let gpu = match &frame.pixels {
            TapPixels::Bgra { bgra, bytes_per_row } => {
                let view = BgraView { bgra, bytes_per_row: *bytes_per_row, width: frame.width, height: frame.height };
                return self.upload(&view).map(|()| true);
            }
            TapPixels::Gpu(gpu) => gpu,
        };
        match self {
            Self::Gpu { device, converter } if !tap.gpu_disabled() => match converter.upload_texture(gpu) {
                Ok(()) => Ok(true),
                // A lost device is recovered (and the frame retried on the
                // new one), not a reason to give up on GPU frames.
                Err(error) => match device.lost() {
                    Some(reason) => Err(format!("{error} (device lost: {reason})")),
                    None => {
                        tap.disable_gpu(&error);
                        Ok(false)
                    }
                },
            },
            _ => Ok(false),
        }
    }

    /// The share's own pixel buffer.
    #[cfg(target_os = "macos")]
    fn upload_share(&mut self, frame: &TapFrame, _tap: &LiveTap) -> Result<bool, String> {
        let Self::Gpu { converter, .. } = self;
        converter.upload_buffer(&frame.buffer).map(|()| true)
    }
}

/// The encoder device and converter. On Windows the device goes on the
/// share's capture adapter, so the capture thread's shared textures open on
/// it (the GPU tap); if it can't, the stream keeps the CPU copy. With no
/// D3D11 video device at all (or `force_cpu`), frames are converted on the
/// CPU for the software encoder.
#[cfg(target_os = "windows")]
fn open_frames(tap: &LiveTap, canvas: (u32, u32), force_cpu: bool) -> Result<Frames, String> {
    if !force_cpu {
        let capture = tap.capture_adapter();
        let opened = EncoderDevice::create_on(capture)
            .and_then(|device| Converter::new(&device, canvas).map(|converter| (device, converter)));
        match opened {
            Ok((device, converter)) => {
                // `decline_gpu`: a device recreated after a loss must not
                // inherit the previous one's acceptance.
                match (capture, device.adapter_luid()) {
                    (Some(capture), Some(encoder)) if capture == encoder => tap.accept_gpu(encoder),
                    (None, _) => tap.decline_gpu("the capture adapter is unknown"),
                    _ => tap.decline_gpu("the encoder couldn't open a device on the capture adapter"),
                }
                return Ok(Frames::Gpu { device, converter });
            }
            Err(error) => log::warn!(
                "streaming: no D3D11 video device ({error}); converting on the CPU for the software H.264 encoder"
            ),
        }
    }
    tap.decline_gpu("no D3D11 video device");
    Ok(Frames::Cpu(CpuConverter::new(canvas)?))
}

#[cfg(target_os = "macos")]
fn open_frames(_tap: &LiveTap, canvas: (u32, u32), _force_cpu: bool) -> Result<Frames, String> {
    let device = EncoderDevice::create()?;
    let converter = Converter::new(&device, canvas)?;
    Ok(Frames::Gpu { device, converter })
}

/// After the device was recreated: the share's latest frame again, on the
/// new device, or the slate if that frame can't be opened there (until the
/// capture's next CPU copy). Returns the picture's size, its generation (the
/// share's) and whether it is the slate.
fn hold_picture(video: &mut Video, tap: &LiveTap) -> Result<((u32, u32), Option<u64>, bool), String> {
    if let Some((frame, generation)) = tap.share_available().then(|| tap.mailbox.latest()).flatten() {
        if video.frames.upload_share(&frame, tap)? {
            return Ok(((frame.width, frame.height), Some(generation), false));
        }
    }
    Ok((upload_slate(video)?, None, true))
}

/// Put the slate in the converter; its size.
fn upload_slate(video: &mut Video) -> Result<(u32, u32), String> {
    let slate = render_slate(video.canvas.0, video.canvas.1);
    video.frames.upload(&BgraView {
        bgra: &slate.bgra,
        bytes_per_row: slate.width * 4,
        width: slate.width,
        height: slate.height,
    })?;
    Ok((slate.width, slate.height))
}

fn to_100ns(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos() / 100).unwrap_or(i64::MAX)
}

struct Video {
    frames: Frames,
    encoder: H264Encoder,
    packager: AvcPackager,
    canvas: (u32, u32),
}

impl Video {
    fn open(frames: Frames, canvas: (u32, u32), fps: u32, bitrate: u32, max: u32) -> Result<Self, String> {
        Self::open_with(frames, AvcPackager::new(), canvas, fps, bitrate, max)
    }

    /// Over `packager`: a reopened encoder keeps the stream's timestamps
    /// monotonic, and a changed sequence header goes out with its first
    /// keyframe.
    fn open_with(
        frames: Frames,
        mut packager: AvcPackager,
        canvas: (u32, u32),
        fps: u32,
        bitrate: u32,
        max: u32,
    ) -> Result<Self, String> {
        let encoder = match &frames {
            Frames::Gpu { device, .. } => H264Encoder::new(device, canvas, fps, bitrate, max)?,
            #[cfg(target_os = "windows")]
            Frames::Cpu(_) => H264Encoder::new_software(canvas, fps, bitrate, max)?,
        };
        if let Some(header) = encoder.sequence_header() {
            let nalus = split_annex_b(header);
            packager.package(&nalus, 0, false);
        }
        Ok(Self { frames, encoder, packager, canvas })
    }

    /// Release the encoder, then the device it runs on; the packager goes on.
    fn retire(self) -> AvcPackager {
        let Self { frames, encoder, packager, .. } = self;
        drop(encoder);
        drop(frames);
        packager
    }

    /// Convert the current frame into `dest` and submit it at `pts`.
    fn encode(&mut self, dest: (u32, u32, u32, u32), pts: i64) -> Result<(Vec<EncodedVideo>, bool), String> {
        match &mut self.frames {
            Frames::Gpu { device, converter } => {
                let nv12 = converter.convert(dest)?;
                self.encoder.encode(device, &nv12, pts)
            }
            #[cfg(target_os = "windows")]
            Frames::Cpu(converter) => {
                let nv12 = converter.convert(dest)?;
                self.encoder.encode_memory(nv12, pts)
            }
        }
    }

    fn package(&mut self, frames: Vec<EncodedVideo>, out: &mut Interleaver) {
        for frame in frames {
            let nalus = split_annex_b(&frame.data);
            let ts_ms = u32::try_from(frame.pts_100ns.max(0) / 10_000).unwrap_or(u32::MAX);
            for packet in self.packager.package(&nalus, ts_ms, frame.keyframe) {
                out.push(packet);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    config: PipelineConfig,
    tap: &LiveTap,
    mixer: &Mutex<AudioMixer>,
    queue: &MediaQueue,
    commands: &Receiver<PipelineCommand>,
    stats: &PipelineStats,
    stop: &AtomicBool,
    force_cpu: bool,
) -> Result<PipelineExit, String> {
    let _runtime = MediaRuntime::start()?;
    let frames = open_frames(tap, config.canvas, force_cpu)?;
    let fps = config.fps.max(1);
    let mut video = Video::open(frames, config.canvas, fps, config.bitrate_bps, config.max_bitrate_bps)?;
    stats.hardware.store(video.encoder.hardware, Ordering::Relaxed);
    let mut aac = AacEncoder::new()?;
    let mut aac_packager = AacPackager::default();
    let mut interleaver = Interleaver::new(300);
    let mut schedule = CfrSchedule::new(fps);
    let mut slate_clock = SlateClock::new();
    let mut last_generation: Option<u64> = None;
    let mut showing_slate = false;
    let mut source_size = config.canvas;
    let mut audio_blocks: u64 = 0;
    let mut mix_level = LevelMeter::default();
    let mut recovery = DeviceRecovery::default();
    // The converter holds a picture to encode (the share's or the slate).
    let mut has_picture = false;
    let block = Duration::from_nanos(MIX_BLOCK_FRAMES as u64 * 1_000_000_000 / u64::from(MIX_SAMPLE_RATE));
    let audio_pts = |blocks: u64| {
        Duration::from_nanos(blocks * MIX_BLOCK_FRAMES as u64 * 1_000_000_000 / u64::from(MIX_SAMPLE_RATE))
    };
    // The canvas is fixed for the stream; shares of any size fit inside it.
    stats.width.store(video.canvas.0, Ordering::Relaxed);
    stats.height.store(video.canvas.1, Ordering::Relaxed);
    stats.fps.store(fps, Ordering::Relaxed);
    let start = Instant::now();

    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(PipelineExit::Stopped);
        }
        match commands.try_recv() {
            Ok(PipelineCommand::Rebound) => last_generation = None,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(PipelineExit::Stopped),
        }
        let elapsed = start.elapsed();

        // Audio: every due block, in order.
        while audio_pts(audio_blocks) <= elapsed {
            // Mutes apply here, at the mix (#487): `pull_block_gated`.
            let (pcm, sources) = super::mixer::pull_block_gated(mixer);
            mix_level.add(&pcm);
            // ~30 s: what the stream actually carries.
            if audio_blocks % 1_400 == 1_399 {
                log::info!("streaming: stream mix {} sources={sources}", mix_level.take());
            }
            let pts = audio_pts(audio_blocks);
            for frame in aac.encode(&pcm, to_100ns(pts))? {
                let ts_ms = u32::try_from(frame.pts_100ns.max(0) / 10_000).unwrap_or(u32::MAX);
                for packet in aac_packager.package(frame.data, ts_ms) {
                    interleaver.push(packet);
                }
            }
            audio_blocks += 1;
        }

        // Video: one constant-rate tick.
        let step: Result<Option<PipelineExit>, String> = (|| {
            #[cfg(test)]
            if INJECT_DEVICE_LOSS.load(Ordering::Acquire) > 0 {
                return Err("injected device loss".into());
            }
            if let Some(tick) = schedule.poll(elapsed) {
                stats.frames_dropped.fetch_add(tick.skipped, Ordering::Relaxed);
                let latest = if tap.share_available() { tap.mailbox.latest() } else { None };
                match slate_clock.tick(elapsed, latest.is_some()) {
                    SlateDecision::End => return Ok(Some(PipelineExit::SlateTimeout)),
                    SlateDecision::Share => {
                        let (frame, generation) = latest.ok_or("frame vanished")?;
                        let size = (frame.width, frame.height);
                        if (last_generation != Some(generation) || showing_slate)
                            && video.frames.upload_share(&frame, tap)?
                        {
                            last_generation = Some(generation);
                            showing_slate = false;
                            source_size = size;
                            has_picture = true;
                        } else if !has_picture {
                            // Nothing uploaded yet and this frame can't be
                            // (a GPU frame the encoder can't open): the slate
                            // until the capture's next CPU copy.
                            source_size = upload_slate(&mut video)?;
                            showing_slate = true;
                            last_generation = None;
                            has_picture = true;
                        }
                    }
                    SlateDecision::Slate => {
                        if !showing_slate {
                            source_size = upload_slate(&mut video)?;
                            showing_slate = true;
                            last_generation = None;
                            has_picture = true;
                        }
                    }
                }
                stats.on_slate.store(showing_slate, Ordering::Relaxed);
                stats.slate_remaining_s.store(
                    slate_clock.remaining(elapsed).unwrap_or(STREAM_SLATE_MAX).as_secs() as u32,
                    Ordering::Relaxed,
                );
                if queue.take_keyframe_request() {
                    video.encoder.force_keyframe();
                }
                let dest = letterbox_rect(source_size.0, source_size.1, video.canvas.0, video.canvas.1);
                let (frames, accepted) = video.encode(dest, to_100ns(tick.pts))?;
                if accepted {
                    stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                }
                video.package(frames, &mut interleaver);
            } else {
                let frames = video.encoder.poll()?;
                video.package(frames, &mut interleaver);
            }
            Ok(None)
        })();
        match step {
            Ok(Some(exit)) => return Ok(exit),
            Ok(None) => {}
            Err(mut error) => loop {
                // Only a lost device is recovered; any other failure ends
                // the stream as before. A loss while recovering counts again.
                let Some(reason) = device_lost(&video) else { return Err(error) };
                if !recovery.allow(Instant::now()) {
                    return Err(format!(
                        "{error} (the graphics device was lost {DEVICE_LOSS_LIMIT} times within a minute: {reason})"
                    ));
                }
                log::warn!("streaming: the encoder's graphics device was lost ({reason}): recreating it ({error})");
                let packager = video.retire();
                let frames = open_frames(tap, config.canvas, force_cpu)?;
                video = Video::open_with(frames, packager, config.canvas, fps, config.bitrate_bps, config.max_bitrate_bps)?;
                stats.hardware.store(video.encoder.hardware, Ordering::Relaxed);
                match hold_picture(&mut video, tap) {
                    Ok(held) => {
                        (source_size, last_generation, showing_slate) = held;
                        has_picture = true;
                        stats.on_slate.store(showing_slate, Ordering::Relaxed);
                        break;
                    }
                    Err(next) => error = next,
                }
            },
        }

        for packet in interleaver.drain_ready() {
            queue.push(packet);
        }

        // Sleep to the next due event, polling the async encoder at least
        // every 2 ms.
        let now = start.elapsed();
        let next = schedule.next_due().min(audio_pts(audio_blocks));
        let wait = next.saturating_sub(now).min(Duration::from_millis(2)).min(block);
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::rtmp::test_server::{serve, Received};
    use crate::sync_ext::MutexExt;
    use crate::streaming::rtmp::{run_publisher, PublisherState, PublisherStats, RtmpDestination};
    use crate::streaming::tap::{self, TapFrame};
    use shiguredo_rtmp::{AvcPacketType, AvcSequenceHeader, VideoCodec};
    use tokio_util::sync::CancellationToken;

    /// One flat-grey 800x600 share frame.
    #[cfg(target_os = "windows")]
    fn share_frame(shade: u8) -> TapFrame {
        let pixels = TapPixels::Bgra { bgra: vec![shade; 800 * 600 * 4], bytes_per_row: 800 * 4 };
        TapFrame { pixels, width: 800, height: 600 }
    }

    #[cfg(target_os = "macos")]
    fn share_frame(shade: u8) -> TapFrame {
        let buffer =
            crate::streaming::macos_encode::PixelBuffer::from_bgra(&vec![shade; 800 * 600 * 4], 800 * 4, 800, 600)
                .unwrap();
        TapFrame { buffer, width: 800, height: 600 }
    }

    /// Tap -> slate/CFR -> convert -> H.264 + AAC -> FLV -> RTMP, into an
    /// in-process server, on whichever path this machine takes (on Windows
    /// with no D3D11 video device, the CPU one). `-- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs the platform's encoders"]
    async fn streams_slate_then_share_end_to_end() {
        slate_then_share(false).await;
    }

    /// The same on the CPU converter and the software encoder, GPU or not.
    #[cfg(target_os = "windows")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs Media Foundation encoders"]
    async fn streams_slate_then_share_end_to_end_on_the_cpu_path() {
        assert!(!slate_then_share(true).await, "the software encoder");
    }

    #[test]
    fn a_lost_device_is_recreated_a_bounded_number_of_times() {
        let mut recovery = DeviceRecovery::default();
        let start = Instant::now();
        for i in 0..DEVICE_LOSS_LIMIT as u64 {
            assert!(recovery.allow(start + Duration::from_secs(i)), "loss {i} recovered");
        }
        assert!(!recovery.allow(start + Duration::from_secs(10)), "too many within a minute: the stream ends");
        // A minute after the first, one more is allowed again, and only one.
        assert!(recovery.allow(start + DEVICE_LOSS_WINDOW));
        assert!(!recovery.allow(start + DEVICE_LOSS_WINDOW));
    }

    /// The encode thread's device-loss path, end to end: a video step fails
    /// with the device gone, the device, converter and encoder are recreated
    /// on the thread, and the stream carries on (slate, then the share) with
    /// timestamps still monotonic. `-- --ignored` (the platform's encoders).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs the platform's encoders"]
    #[allow(clippy::await_holding_lock)]
    async fn a_lost_device_is_recreated_and_the_stream_carries_on() {
        let _guard = tap::test_lock();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::Mutex::new(Received::default()));
        let server = tokio::spawn(serve(listener, received.clone(), None));
        let live_tap = tap::start(78);
        let mixer = Arc::new(Mutex::new(AudioMixer::new()));
        let queue = Arc::new(MediaQueue::new());
        let config = PipelineConfig { canvas: (1280, 720), fps: 30, bitrate_bps: 3_000_000, max_bitrate_bps: 6_000_000 };
        let pipeline = spawn_with(config, live_tap.clone(), mixer.clone(), queue.clone(), false).unwrap();
        let destination = RtmpDestination::new(&format!("rtmp://127.0.0.1:{port}/live"), "k").unwrap();
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(PublisherState::Connecting);
        let cancel = CancellationToken::new();
        let publisher = tokio::spawn(run_publisher(
            destination,
            queue.clone(),
            Arc::new(PublisherStats::default()),
            state_tx,
            cancel.clone(),
            None,
        ));
        state_rx.wait_for(|s| *s == PublisherState::Live).await.unwrap();
        for i in 0..15u8 {
            tap::offer_frame(78, true, || Some(share_frame(i)));
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
        let before = pipeline.stats.frames_encoded.load(Ordering::Relaxed);
        assert!(before > 5, "streaming before the loss ({before} frames)");
        INJECT_DEVICE_LOSS.store(1, Ordering::Release);
        for i in 0..60u8 {
            tap::offer_frame(78, true, || Some(share_frame(100 + i)));
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
        assert_eq!(INJECT_DEVICE_LOSS.load(Ordering::Acquire), 0, "the loss was handled");
        let after = pipeline.stats.frames_encoded.load(Ordering::Relaxed);
        let dropped = pipeline.stats.frames_dropped.load(Ordering::Relaxed);
        eprintln!("frames encoded {before} -> {after}, dropped {dropped}");
        assert!(!pipeline.thread.is_finished(), "the stream carried on");
        assert!(after >= before + 30, "frames kept coming after the loss ({before} -> {after})");
        assert!(!pipeline.stats.on_slate.load(Ordering::Relaxed), "back on the share");

        pipeline.stop.store(true, Ordering::Release);
        let exit = tokio::task::spawn_blocking(move || pipeline.thread.join().unwrap()).await.unwrap();
        assert_eq!(exit, PipelineExit::Stopped);
        cancel.cancel();
        let _ = publisher.await;
        server.abort();
        tap::stop();
        let r = received.lock().unwrap();
        let ts: Vec<u32> = r.video.iter().map(|f| f.timestamp.as_millis()).collect();
        assert!(ts.windows(2).all(|w| w[0] <= w[1]), "monotonic across the recovery: {ts:?}");
        assert!(r.video.iter().filter(|f| f.is_keyframe()).count() >= 2, "the new encoder starts on a keyframe");
    }

    /// Returns whether the stream used a hardware encoder.
    // The tap's test lock serializes the whole test, awaits included.
    #[allow(clippy::await_holding_lock)]
    async fn slate_then_share(force_cpu: bool) -> bool {
        let _guard = tap::test_lock();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::Mutex::new(Received::default()));
        let server = tokio::spawn(serve(listener, received.clone(), None));

        let live_tap = tap::start(77);
        let mixer = Arc::new(Mutex::new(AudioMixer::new()));
        let queue = Arc::new(MediaQueue::new());
        let pipeline = spawn_with(
            PipelineConfig {
                canvas: (1280, 720),
                fps: 30,
                bitrate_bps: 3_000_000,
                max_bitrate_bps: 6_000_000,
            },
            live_tap.clone(),
            mixer.clone(),
            queue.clone(),
            force_cpu,
        )
        .unwrap();
        let destination = RtmpDestination::new(&format!("rtmp://127.0.0.1:{port}/live"), "k").unwrap();
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(PublisherState::Connecting);
        let cancel = CancellationToken::new();
        let publisher = tokio::spawn(run_publisher(
            destination,
            queue.clone(),
            Arc::new(PublisherStats::default()),
            state_tx,
            cancel.clone(),
            None,
        ));
        state_rx.wait_for(|s| *s == PublisherState::Live).await.unwrap();

        // No share frames yet: the slate streams, with audio. Encoder start-up
        // is slower when another GPU test runs alongside, so wait for it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pipeline.stats.on_slate.load(Ordering::Relaxed) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(pipeline.stats.on_slate.load(Ordering::Relaxed));
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Now the share delivers frames (a 4:3 window into the 16:9 canvas).
        for i in 0..30u8 {
            tap::offer_frame(77, true, || Some(share_frame(i)));
            mixer.lock_unpoisoned().push("mic", &vec![1000i16; 960 * 2], 2);
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
        assert!(!pipeline.stats.on_slate.load(Ordering::Relaxed));
        tokio::time::sleep(Duration::from_millis(500)).await;

        pipeline.stop.store(true, Ordering::Release);
        let hardware = pipeline.stats.hardware.load(Ordering::Relaxed);
        let exit = tokio::task::spawn_blocking(move || pipeline.thread.join().unwrap()).await.unwrap();
        assert_eq!(exit, PipelineExit::Stopped);
        cancel.cancel();
        let _ = publisher.await;
        server.abort();
        tap::stop();

        let r = received.lock().unwrap();
        let video = &r.video;
        assert!(video.len() > 45, "{} video messages for ~2.5 s at 30 fps", video.len());
        assert!(video.iter().all(|f| f.codec == VideoCodec::Avc));
        assert_eq!(video[0].avc_packet_type, Some(AvcPacketType::SequenceHeader));
        let header = AvcSequenceHeader::from_bytes(&video[0].data).unwrap();
        assert_eq!(header.avc_profile_indication, 100, "High profile");
        assert!(video[1].is_keyframe());
        let ts: Vec<u32> = video.iter().map(|f| f.timestamp.as_millis()).collect();
        assert!(ts.windows(2).all(|w| w[0] <= w[1]), "{ts:?}");
        // Constant frame rate: ~33 ms apart (the slate was static the whole time).
        let gaps: Vec<u32> = ts.windows(2).skip(2).map(|w| w[1] - w[0]).collect();
        let steady = gaps.iter().filter(|g| (30..=37).contains(*g)).count();
        assert!(steady * 10 >= gaps.len() * 9, "gaps {gaps:?}");
        assert!(r.audio.first().is_some_and(|f| f.is_aac_sequence_header));
        assert!(r.audio.len() > 80, "{} audio messages", r.audio.len());
        let audio_end = r.audio.last().unwrap().timestamp.as_millis();
        let video_end = *ts.last().unwrap();
        assert!(audio_end.abs_diff(video_end) < 200, "A/V ends {audio_end} vs {video_end}");
        eprintln!("hardware encoder: {hardware}");
        hardware
    }

    /// A texture on a "capture" device -> GPU tap pool -> the encoder device
    /// on the same adapter -> H.264 -> RTMP, into an in-process server, with
    /// no CPU copy anywhere. Needs a GPU: `-- --ignored`.
    #[cfg(target_os = "windows")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a GPU and the hardware encoder"]
    #[allow(clippy::await_holding_lock)]
    async fn streams_gpu_tap_frames_end_to_end() {
        use crate::streaming::gpu_tap::{test_device, Producer};
        use windows::core::Interface;
        use windows::Win32::Graphics::Direct3D11::{
            ID3D11Resource, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

        let _guard = tap::test_lock();
        let (device, context) = test_device();
        let (width, height) = (800u32, 600u32);
        let pixels = vec![0x80u8; (width * height * 4) as usize];
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: width * 4, SysMemSlicePitch: 0 };
        let mut source = None;
        // SAFETY: desc and data describe `pixels`, alive for the call.
        unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut source)) }.unwrap();
        let source: ID3D11Resource = source.unwrap().cast().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::Mutex::new(Received::default()));
        let server = tokio::spawn(serve(listener, received.clone(), None));

        let live_tap = tap::start(77);
        // The capture's first frame tells the tap its adapter; the encoder
        // hasn't taken GPU frames yet, so nothing is copied.
        let mut producer = Producer::default();
        producer.offer(77, &device, &context, &source, (width, height), true);
        assert!(live_tap.capture_adapter().is_some());
        assert!(live_tap.mailbox.latest().is_none());

        let mixer = Arc::new(Mutex::new(AudioMixer::new()));
        let queue = Arc::new(MediaQueue::new());
        let pipeline = spawn(
            PipelineConfig { canvas: (1280, 720), fps: 30, bitrate_bps: 3_000_000, max_bitrate_bps: 6_000_000 },
            live_tap.clone(),
            mixer.clone(),
            queue.clone(),
        )
        .unwrap();
        let destination = RtmpDestination::new(&format!("rtmp://127.0.0.1:{port}/live"), "k").unwrap();
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(PublisherState::Connecting);
        let cancel = CancellationToken::new();
        let publisher = tokio::spawn(run_publisher(
            destination,
            queue.clone(),
            Arc::new(PublisherStats::default()),
            state_tx,
            cancel.clone(),
            None,
        ));
        state_rx.wait_for(|s| *s == PublisherState::Live).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while live_tap.gpu_encoder_adapter().is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(live_tap.gpu_encoder_adapter(), live_tap.capture_adapter(), "encoder on the capture adapter");

        for _ in 0..60 {
            producer.offer(77, &device, &context, &source, (width, height), true);
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
        assert!(!pipeline.stats.on_slate.load(Ordering::Relaxed));
        assert!(!live_tap.gpu_disabled(), "the encoder opened the shared frames");
        let (frame, _) = live_tap.mailbox.latest().unwrap();
        assert!(matches!(frame.pixels, TapPixels::Gpu(_)), "no CPU copy");
        assert_eq!((frame.width, frame.height), (width, height));
        drop(frame);

        pipeline.stop.store(true, Ordering::Release);
        let exit = tokio::task::spawn_blocking(move || pipeline.thread.join().unwrap()).await.unwrap();
        assert_eq!(exit, PipelineExit::Stopped);
        assert_eq!(
            (pipeline.stats.width.load(Ordering::Relaxed), pipeline.stats.height.load(Ordering::Relaxed)),
            (1280, 720)
        );
        assert_eq!(pipeline.stats.fps.load(Ordering::Relaxed), 30);
        cancel.cancel();
        let _ = publisher.await;
        server.abort();
        tap::stop();

        let r = received.lock().unwrap();
        let video = &r.video;
        assert!(video.len() > 45, "{} video messages for ~2 s at 30 fps", video.len());
        assert!(video.iter().all(|f| f.codec == VideoCodec::Avc));
        assert_eq!(video[0].avc_packet_type, Some(AvcPacketType::SequenceHeader));
        let ts: Vec<u32> = video.iter().map(|f| f.timestamp.as_millis()).collect();
        let gaps: Vec<u32> = ts.windows(2).skip(2).map(|w| w[1] - w[0]).collect();
        let steady = gaps.iter().filter(|g| (30..=37).contains(*g)).count();
        assert!(steady * 10 >= gaps.len() * 9, "30 fps: gaps {gaps:?}");
    }
}
