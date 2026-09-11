//! ScreenCaptureKit audio capture for the common screen-audio boundary.
//!
//! The stream is deliberately separate from visual capture. The source
//! registry can therefore keep one elected audio owner alive while several
//! visual shares refer to the same process, and can hand that owner off
//! without tearing down video. ScreenCaptureKit's application filter is a
//! process/application scope, not an individual-window audio guarantee.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use screencapturekit::cm::{AudioBuffer, CMFormatDescription, CMSampleBuffer, CMSampleBufferExt};
use screencapturekit::prelude::*;
use screencapturekit::shareable_content::SCDisplay;
use screencapturekit::stream::configuration::{
    AudioChannelCount, AudioSampleRate, PixelFormat, SCStreamConfiguration,
};
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::delegate_trait::ErrorHandler;

use crate::screen_audio::{
    AudioSourceKey, PcmChunk, ScreenAudioIngress, ScreenAudioIngressStats, ScreenAudioQueue,
};

const AUDIO_QUEUE_DEPTH: u32 = 3;
const AUDIO_BITS_PER_F32: usize = 32;
const AUDIO_BITS_PER_S16: usize = 16;

const AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN: u32 = 1 << 1;
const AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;
const AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacScreenAudioTarget {
    /// ScreenCaptureKit requires a display filter even though the logical
    /// source is the process-wide system output. `None` selects the first
    /// available display; it does not promise monitor-isolated audio.
    SystemOutput { display_id: Option<u32> },
    /// ScreenCaptureKit's app filter captures the selected process/application
    /// scope. `display_id` only selects the display context required by that
    /// filter and must not be interpreted as per-window isolation.
    Process { pid: u32, display_id: Option<u32> },
}

impl MacScreenAudioTarget {
    pub(crate) fn source_key(self) -> Option<AudioSourceKey> {
        match self {
            Self::SystemOutput { .. } => Some(AudioSourceKey::SystemOutput),
            Self::Process { pid, .. } if pid != 0 => Some(AudioSourceKey::Process(pid)),
            Self::Process { .. } => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MacScreenAudioError {
    #[error("Screen Recording permission has not been granted")]
    PermissionDenied,
    #[error("ScreenCaptureKit content enumeration failed: {0}")]
    Content(String),
    #[error("display {0} is not available for screen audio")]
    DisplayNotFound(u32),
    #[error("process {0} is not available for ScreenCaptureKit audio")]
    ProcessNotFound(u32),
    #[error("invalid screen-audio target")]
    InvalidTarget,
    #[error("ScreenCaptureKit audio output registration failed")]
    OutputRegistration,
    #[error("ScreenCaptureKit audio stream failed: {0}")]
    Stream(String),
    #[error("unsupported ScreenCaptureKit PCM format: {0}")]
    UnsupportedFormat(String),
    #[error("malformed ScreenCaptureKit PCM buffer: {0}")]
    MalformedBuffer(String),
}

/// A running ScreenCaptureKit audio owner. The callback copies CoreMedia data
/// into the common owned PCM boundary before the callback returns; no native
/// buffer pointer is retained by this type.
pub(crate) struct MacScreenAudioCapture {
    stream: SCStream,
    source: AudioSourceKey,
    ingress: ScreenAudioIngress,
    stopped: AtomicBool,
    failed: Arc<AtomicBool>,
}

impl MacScreenAudioCapture {
    pub(crate) fn start(
        target: MacScreenAudioTarget,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, MacScreenAudioError> {
        if !crate::window_source::has_screen_recording_access() {
            return Err(MacScreenAudioError::PermissionDenied);
        }
        let source = target
            .source_key()
            .ok_or(MacScreenAudioError::InvalidTarget)?;
        let filter = filter_for_target(target)?;
        let config = audio_stream_configuration();
        let ingress = ScreenAudioIngress::new();
        let failed = Arc::new(AtomicBool::new(false));
        let on_error: Arc<dyn Fn(String) + Send + Sync> = Arc::new(on_error);

        let delegate_ingress = ingress.clone();
        let delegate_failed = failed.clone();
        let delegate_on_error = on_error.clone();
        let mut stream = SCStream::new_with_delegate(
            &filter,
            &config,
            ErrorHandler::new(move |error| {
                report_failure(
                    &delegate_failed,
                    &delegate_ingress,
                    &delegate_on_error,
                    error.to_string(),
                );
            }),
        );

        let handler_ingress = ingress.clone();
        let handler_failed = failed.clone();
        let handler_on_error = on_error.clone();
        if stream
            .add_output_handler(
                move |sample: CMSampleBuffer, output_type| {
                    if output_type != SCStreamOutputType::Audio {
                        return;
                    }
                    let chunk = match pcm_chunk_from_sample(&sample) {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            report_failure(
                                &handler_failed,
                                &handler_ingress,
                                &handler_on_error,
                                error.to_string(),
                            );
                            return;
                        }
                    };
                    if let Err(error) = handler_ingress.push(chunk) {
                        report_failure(
                            &handler_failed,
                            &handler_ingress,
                            &handler_on_error,
                            format!("PCM conversion failed: {error}"),
                        );
                    }
                },
                SCStreamOutputType::Audio,
            )
            .is_none()
        {
            ingress.close();
            return Err(MacScreenAudioError::OutputRegistration);
        }

        if let Err(error) = stream.start_capture() {
            ingress.close();
            return Err(MacScreenAudioError::Stream(error.to_string()));
        }

        log::info!(
            "screen-audio: started macOS {:?} owner (system output is not monitor-isolated)",
            source
        );
        Ok(Self {
            stream,
            source,
            ingress,
            stopped: AtomicBool::new(false),
            failed,
        })
    }

    pub(crate) fn source(&self) -> AudioSourceKey {
        self.source
    }

    pub(crate) fn queue(&self) -> ScreenAudioQueue {
        self.ingress.queue()
    }

    pub(crate) fn ingress_stats(&self) -> ScreenAudioIngressStats {
        self.ingress.stats()
    }

    pub(crate) fn close_queue(&self) {
        self.ingress.close();
    }

    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub(crate) fn stop(&self) -> Result<(), MacScreenAudioError> {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // Close first so an in-flight callback can copy safely but cannot put
        // another frame behind the stopped owner. The queue wake-up also lets
        // a waiting publisher exit without depending on CoreMedia callbacks.
        self.ingress.close();
        self.stream
            .stop_capture()
            .map_err(|error| MacScreenAudioError::Stream(error.to_string()))
    }
}

fn report_failure(
    failed: &AtomicBool,
    ingress: &ScreenAudioIngress,
    on_error: &Arc<dyn Fn(String) + Send + Sync>,
    error: String,
) {
    if failed.swap(true, Ordering::AcqRel) {
        return;
    }
    ingress.close();
    on_error(error);
}

fn audio_stream_configuration() -> SCStreamConfiguration {
    // The dimensions/pixel format are irrelevant to an audio-only output, but
    // ScreenCaptureKit still requires a valid stream configuration object.
    SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_pixel_format(PixelFormat::BGRA)
        .with_captures_audio(true)
        .with_sample_rate(AudioSampleRate::Rate48000)
        .with_channel_count(AudioChannelCount::Stereo)
        .with_excludes_current_process_audio(true)
        .with_queue_depth(AUDIO_QUEUE_DEPTH)
        .with_fps(1)
}

fn content() -> Result<screencapturekit::shareable_content::SCShareableContent, MacScreenAudioError>
{
    SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|error| MacScreenAudioError::Content(error.to_string()))
}

fn select_display(
    displays: &[SCDisplay],
    display_id: Option<u32>,
) -> Result<SCDisplay, MacScreenAudioError> {
    if let Some(display_id) = display_id {
        return displays
            .iter()
            .find(|display| display.display_id() == display_id)
            .cloned()
            .ok_or(MacScreenAudioError::DisplayNotFound(display_id));
    }
    displays
        .first()
        .cloned()
        .ok_or(MacScreenAudioError::Content(
            "no displays are available for screen audio".to_string(),
        ))
}

fn filter_for_target(target: MacScreenAudioTarget) -> Result<SCContentFilter, MacScreenAudioError> {
    let content = content()?;
    let displays = content.displays();
    match target {
        MacScreenAudioTarget::SystemOutput { display_id } => {
            let display = select_display(&displays, display_id)?;
            Ok(SCContentFilter::create()
                .with_display(&display)
                .with_excluding_windows(&[])
                .build())
        }
        MacScreenAudioTarget::Process { pid, display_id } => {
            let app = content
                .applications()
                .into_iter()
                .find(|application| application.process_id() == pid as i32)
                .ok_or(MacScreenAudioError::ProcessNotFound(pid))?;
            let display = select_display(&displays, display_id)?;
            let applications = [&app];
            Ok(SCContentFilter::create()
                .with_display(&display)
                .with_including_applications(&applications, &[])
                .build())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinearPcmFormat {
    sample_rate: u32,
    channels: usize,
    bits_per_sample: usize,
    is_float: bool,
    big_endian: bool,
    non_interleaved: bool,
}

fn linear_pcm_format(
    description: &CMFormatDescription,
) -> Result<LinearPcmFormat, MacScreenAudioError> {
    if !description.is_audio() || !description.is_pcm() {
        return Err(MacScreenAudioError::UnsupportedFormat(format!(
            "media type/subtype are not linear PCM ({description})"
        )));
    }
    let sample_rate = description
        .audio_sample_rate()
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .map(|rate| rate.round())
        .and_then(|rate| u32::try_from(rate as u64).ok())
        .ok_or_else(|| MacScreenAudioError::UnsupportedFormat("invalid sample rate".to_string()))?;
    let channels = description
        .audio_channel_count()
        .map(|channels| channels as usize)
        .filter(|channels| *channels > 0)
        .ok_or_else(|| {
            MacScreenAudioError::UnsupportedFormat("invalid channel count".to_string())
        })?;
    let bits_per_sample = description
        .audio_bits_per_channel()
        .map(|bits| bits as usize)
        .ok_or_else(|| MacScreenAudioError::UnsupportedFormat("invalid bit depth".to_string()))?;
    let flags = description.audio_format_flags().unwrap_or(0);
    let is_float = flags & AUDIO_FORMAT_FLAG_IS_FLOAT != 0;
    if !is_float && flags != 0 && flags & AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER == 0 {
        return Err(MacScreenAudioError::UnsupportedFormat(
            "PCM is neither float nor signed integer".to_string(),
        ));
    }
    if (is_float && bits_per_sample != AUDIO_BITS_PER_F32)
        || (!is_float && bits_per_sample != AUDIO_BITS_PER_S16)
    {
        return Err(MacScreenAudioError::UnsupportedFormat(format!(
            "{bits_per_sample}-bit {} PCM",
            if is_float { "float" } else { "integer" }
        )));
    }
    Ok(LinearPcmFormat {
        sample_rate,
        channels,
        bits_per_sample,
        is_float,
        big_endian: flags & AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN != 0,
        non_interleaved: flags & AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED != 0,
    })
}

fn pcm_chunk_from_sample(sample: &CMSampleBuffer) -> Result<PcmChunk, MacScreenAudioError> {
    if !sample.is_valid() {
        return Err(MacScreenAudioError::MalformedBuffer(
            "sample buffer is invalid".to_string(),
        ));
    }
    if !sample.data_is_ready() {
        sample.make_data_ready().map_err(|status| {
            MacScreenAudioError::MalformedBuffer(format!("data not ready ({status})"))
        })?;
    }
    let description = sample.format_description().ok_or_else(|| {
        MacScreenAudioError::MalformedBuffer("missing format description".to_string())
    })?;
    let format = linear_pcm_format(&description)?;
    let buffers = sample.audio_buffer_list().ok_or_else(|| {
        MacScreenAudioError::MalformedBuffer("missing audio buffer list".to_string())
    })?;
    let native_buffers = buffers.iter().collect::<Vec<_>>();
    if native_buffers.is_empty() {
        return Err(MacScreenAudioError::MalformedBuffer(
            "audio buffer list is empty".to_string(),
        ));
    }

    if native_buffers.len() == 1
        && !format.non_interleaved
        && native_buffers[0].number_channels as usize == format.channels
    {
        let samples = decode_bytes(native_buffers[0], format)?;
        return PcmChunk::from_f32(samples, format.sample_rate, format.channels)
            .map_err(|error| MacScreenAudioError::MalformedBuffer(error.to_string()));
    }

    // CoreAudio represents non-interleaved PCM as one AudioBuffer per channel.
    // Copy each plane first, then interleave in Rust-owned memory.
    if native_buffers.len() != format.channels
        || native_buffers
            .iter()
            .any(|buffer| buffer.number_channels != 1)
    {
        return Err(MacScreenAudioError::MalformedBuffer(format!(
            "{} buffers for {} channels (non_interleaved={})",
            native_buffers.len(),
            format.channels,
            format.non_interleaved
        )));
    }
    let planes = native_buffers
        .iter()
        .map(|buffer| decode_bytes(buffer, format))
        .collect::<Result<Vec<_>, _>>()?;
    let frame_count = planes.first().map_or(0, Vec::len);
    if frame_count == 0 || planes.iter().any(|plane| plane.len() != frame_count) {
        return Err(MacScreenAudioError::MalformedBuffer(
            "non-interleaved planes have different lengths".to_string(),
        ));
    }
    let mut interleaved = Vec::with_capacity(frame_count * format.channels);
    for frame in 0..frame_count {
        for plane in &planes {
            interleaved.push(plane[frame]);
        }
    }
    PcmChunk::from_f32(interleaved, format.sample_rate, format.channels)
        .map_err(|error| MacScreenAudioError::MalformedBuffer(error.to_string()))
}

fn decode_bytes(
    buffer: &AudioBuffer,
    format: LinearPcmFormat,
) -> Result<Vec<f32>, MacScreenAudioError> {
    let bytes_per_sample = format.bits_per_sample / 8;
    if bytes_per_sample == 0 || buffer.data().len() % bytes_per_sample != 0 {
        return Err(MacScreenAudioError::MalformedBuffer(format!(
            "{} bytes is not aligned to {}-byte samples",
            buffer.data().len(),
            bytes_per_sample
        )));
    }
    buffer
        .data()
        .chunks_exact(bytes_per_sample)
        .map(|sample| decode_one_sample(sample, format))
        .collect()
}

fn decode_one_sample(bytes: &[u8], format: LinearPcmFormat) -> Result<f32, MacScreenAudioError> {
    match (format.is_float, format.bits_per_sample) {
        (true, AUDIO_BITS_PER_F32) => {
            let bytes: [u8; 4] = bytes.try_into().map_err(|_| {
                MacScreenAudioError::MalformedBuffer("invalid float sample size".to_string())
            })?;
            let bits = if format.big_endian {
                u32::from_be_bytes(bytes)
            } else {
                u32::from_le_bytes(bytes)
            };
            Ok(f32::from_bits(bits).clamp(-1.0, 1.0))
        }
        (false, AUDIO_BITS_PER_S16) => {
            let bytes: [u8; 2] = bytes.try_into().map_err(|_| {
                MacScreenAudioError::MalformedBuffer("invalid integer sample size".to_string())
            })?;
            let sample = if format.big_endian {
                i16::from_be_bytes(bytes)
            } else {
                i16::from_le_bytes(bytes)
            };
            Ok(if sample == i16::MIN {
                -1.0
            } else {
                sample as f32 / i16::MAX as f32
            })
        }
        _ => Err(MacScreenAudioError::UnsupportedFormat(
            "only packed 16-bit integer and 32-bit float PCM are supported".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(bits_per_sample: usize, is_float: bool, big_endian: bool) -> LinearPcmFormat {
        LinearPcmFormat {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample,
            is_float,
            big_endian,
            non_interleaved: false,
        }
    }

    #[test]
    fn decodes_little_endian_s16_and_clamps_at_pcm_boundary() {
        let pcm = format(AUDIO_BITS_PER_S16, false, false);
        assert_eq!(decode_one_sample(&[0, 0], pcm).unwrap(), 0.0);
        assert_eq!(decode_one_sample(&[0xff, 0x7f], pcm).unwrap(), 1.0);
        assert_eq!(decode_one_sample(&[0, 0x80], pcm).unwrap(), -1.0);
        assert!(
            (decode_one_sample(&[0xff, 0xff], pcm).unwrap() + 1.0 / 32767.0).abs() < f32::EPSILON
        );
    }

    #[test]
    fn decodes_big_endian_float() {
        let pcm = format(AUDIO_BITS_PER_F32, true, true);
        assert_eq!(
            decode_one_sample(&0.25_f32.to_bits().to_be_bytes(), pcm).unwrap(),
            0.25
        );
        assert_eq!(
            decode_one_sample(&(-0.5_f32).to_bits().to_be_bytes(), pcm).unwrap(),
            -0.5
        );
    }

    #[test]
    fn source_target_never_maps_zero_process_to_a_real_source() {
        assert_eq!(
            MacScreenAudioTarget::Process {
                pid: 0,
                display_id: None
            }
            .source_key(),
            None
        );
        assert_eq!(
            MacScreenAudioTarget::SystemOutput { display_id: None }.source_key(),
            Some(AudioSourceKey::SystemOutput)
        );
    }
}
