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
use super::tap::TapPixels;
#[cfg(target_os = "windows")]
use super::windows_encode::{
    AacEncoder, BgraView, Converter, EncodedVideo, EncoderDevice, H264Encoder, MediaRuntime,
};
use crate::sync_ext::MutexExt;

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
    let (commands, command_rx) = std::sync::mpsc::channel();
    let stats = Arc::new(PipelineStats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stats = stats.clone();
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("petal-stream-encode".into())
        .spawn(move || {
            let exit = match run(config, &tap, &mixer, &queue, &command_rx, &thread_stats, &thread_stop) {
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

/// Hand a share frame to the converter: a GPU tap texture or CPU BGRA on
/// Windows, the share's own pixel buffer on macOS. `Ok(false)`: not uploaded
/// (a GPU frame after the GPU tap was turned off; the CPU copy replaces it
/// with the capture's next frame).
#[cfg(target_os = "windows")]
fn upload_share(converter: &mut Converter, frame: &TapFrame, tap: &LiveTap) -> Result<bool, String> {
    match &frame.pixels {
        TapPixels::Bgra { bgra, bytes_per_row } => converter
            .upload(&BgraView { bgra, bytes_per_row: *bytes_per_row, width: frame.width, height: frame.height })
            .map(|()| true),
        TapPixels::Gpu(_) if tap.gpu_disabled() => Ok(false),
        TapPixels::Gpu(gpu) => match converter.upload_texture(gpu) {
            Ok(()) => Ok(true),
            Err(error) => {
                tap.disable_gpu(&error);
                Ok(false)
            }
        },
    }
}

#[cfg(target_os = "macos")]
fn upload_share(converter: &mut Converter, frame: &TapFrame, _tap: &LiveTap) -> Result<bool, String> {
    converter.upload_buffer(&frame.buffer).map(|()| true)
}

/// The encoder device. On Windows it goes on the share's capture adapter, so
/// the capture thread's shared textures open on it (the GPU tap); if it
/// can't, the stream keeps the CPU copy.
#[cfg(target_os = "windows")]
fn create_device(tap: &LiveTap) -> Result<EncoderDevice, String> {
    let capture = tap.capture_adapter();
    let device = EncoderDevice::create_on(capture)?;
    match (capture, device.adapter_luid()) {
        (Some(capture), Some(encoder)) if capture == encoder => tap.accept_gpu(encoder),
        (None, _) => tap.gpu_fell_back("the capture adapter is unknown"),
        _ => tap.gpu_fell_back("the encoder couldn't open a device on the capture adapter"),
    }
    Ok(device)
}

#[cfg(target_os = "macos")]
fn create_device(_tap: &LiveTap) -> Result<EncoderDevice, String> {
    EncoderDevice::create()
}

fn to_100ns(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos() / 100).unwrap_or(i64::MAX)
}

struct Video {
    converter: Converter,
    encoder: H264Encoder,
    packager: AvcPackager,
    canvas: (u32, u32),
}

impl Video {
    fn open(device: &EncoderDevice, canvas: (u32, u32), fps: u32, bitrate: u32, max: u32) -> Result<Self, String> {
        let converter = Converter::new(device, canvas)?;
        let encoder = H264Encoder::new(device, canvas, fps, bitrate, max)?;
        let mut packager = AvcPackager::new();
        if let Some(header) = encoder.sequence_header() {
            let nalus = split_annex_b(header);
            packager.package(&nalus, 0, false);
        }
        Ok(Self { converter, encoder, packager, canvas })
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

fn run(
    config: PipelineConfig,
    tap: &LiveTap,
    mixer: &Mutex<AudioMixer>,
    queue: &MediaQueue,
    commands: &Receiver<PipelineCommand>,
    stats: &PipelineStats,
    stop: &AtomicBool,
) -> Result<PipelineExit, String> {
    let _runtime = MediaRuntime::start()?;
    let device = create_device(tap)?;
    let fps = config.fps.max(1);
    let mut video = Video::open(&device, config.canvas, fps, config.bitrate_bps, config.max_bitrate_bps)?;
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
            let (pcm, sources) = {
                let mut mixer = mixer.lock_unpoisoned();
                (mixer.pull_block(), mixer.source_count())
            };
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
        if let Some(tick) = schedule.poll(elapsed) {
            stats.frames_dropped.fetch_add(tick.skipped, Ordering::Relaxed);
            let latest = if tap.share_available() { tap.mailbox.latest() } else { None };
            match slate_clock.tick(elapsed, latest.is_some()) {
                SlateDecision::End => return Ok(PipelineExit::SlateTimeout),
                SlateDecision::Share => {
                    let (frame, generation) = latest.ok_or("frame vanished")?;
                    let size = (frame.width, frame.height);
                    if (last_generation != Some(generation) || showing_slate)
                        && upload_share(&mut video.converter, &frame, tap)?
                    {
                        last_generation = Some(generation);
                        showing_slate = false;
                        source_size = size;
                    }
                }
                SlateDecision::Slate => {
                    if !showing_slate {
                        let slate = render_slate(video.canvas.0, video.canvas.1);
                        video.converter.upload(&BgraView {
                            bgra: &slate.bgra,
                            bytes_per_row: slate.width * 4,
                            width: slate.width,
                            height: slate.height,
                        })?;
                        showing_slate = true;
                        last_generation = None;
                        source_size = (slate.width, slate.height);
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
            let nv12 = video.converter.convert(dest)?;
            let (frames, accepted) = video.encoder.encode(&device, &nv12, to_100ns(tick.pts))?;
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

    /// Tap -> slate/CFR -> GPU convert -> hardware H.264 + AAC -> FLV ->
    /// RTMP, into an in-process server. Needs a GPU: `-- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a GPU and the platform's hardware encoders"]
    // The tap's test lock serializes the whole test, awaits included.
    #[allow(clippy::await_holding_lock)]
    async fn streams_slate_then_share_end_to_end() {
        let _guard = tap::test_lock();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::Mutex::new(Received::default()));
        let server = tokio::spawn(serve(listener, received.clone(), None));

        let live_tap = tap::start(77);
        let mixer = Arc::new(Mutex::new(AudioMixer::new()));
        let queue = Arc::new(MediaQueue::new());
        let pipeline = spawn(
            PipelineConfig {
                canvas: (1280, 720),
                fps: 30,
                bitrate_bps: 3_000_000,
                max_bitrate_bps: 6_000_000,
            },
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
            tap::offer_frame(77, || Some(share_frame(i)));
            mixer.lock_unpoisoned().push("mic", &vec![1000i16; 960 * 2], 2);
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
        assert!(!pipeline.stats.on_slate.load(Ordering::Relaxed));
        tokio::time::sleep(Duration::from_millis(500)).await;

        pipeline.stop.store(true, Ordering::Release);
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
        producer.offer(77, &device, &context, &source, (width, height));
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
            producer.offer(77, &device, &context, &source, (width, height));
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
