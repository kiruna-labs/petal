#![cfg(target_os = "windows")]

//! Windows WASAPI render-loopback adapter for screen-audio tracks.
//!
//! The worker owns COM and every WASAPI interface. Native callbacks are not
//! used: an event-driven render-loopback worker copies each packet into the
//! shared bounded `ScreenAudioIngress`, and the LiveKit pump consumes that
//! queue independently. Process loopback is used for window shares on Windows
//! 10 build 20348+; older builds degrade to video-only for process sources.

use std::mem::{size_of, ManuallyDrop};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE, STILL_ACTIVE};
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, PROCESS_LOOPBACK_MODE,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoTaskMemFree, CoUninitialize, BLOB,
    CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, GetExitCodeProcess, OpenProcess, SetEvent,
    WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows_core::{implement, Error as WindowsError, IUnknown, Interface, Ref, HRESULT};

use crate::screen_audio::{
    AudioSourceKey, PcmChunk, ScreenAudioIngress, ScreenAudioQueue, SCREEN_AUDIO_CHANNELS,
    SCREEN_AUDIO_SAMPLE_RATE,
};

const PROCESS_LOOPBACK_MIN_BUILD: u32 = 20_348;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_24BIT_PACKED: u16 = 24;
const WAVE_FORMAT_32BIT_INTEGER: u16 = 32;
const WAVE_FORMAT_32BIT_FLOAT: u16 = 32;
// Shared event-driven WASAPI streams use the engine-selected buffer period.
const CAPTURE_BUFFER_DURATION_HNS: i64 = 0;
const WAIT_TIMEOUT_MS: u32 = 100;
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);

const KSDATAFORMAT_SUBTYPE_PCM: windows_core::GUID =
    windows_core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows_core::GUID =
    windows_core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Result<Self, String> {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        initialized
            .ok()
            .map_err(|error| format!("failed to initialize COM for screen audio: {error}"))?;
        Ok(Self(true))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// A raw COM pointer is transferred only once from the activation completion
/// callback to the thread that requested activation. The receiver immediately
/// reconstructs the owning typed interface; all subsequent COM calls stay on
/// that worker thread.
struct SendComInterface(*mut std::ffi::c_void);

unsafe impl Send for SendComInterface {}

impl SendComInterface {
    unsafe fn into_audio_client(self) -> IAudioClient {
        IAudioClient::from_raw(self.0)
    }
}

/// Owns the native process-loopback activation blob until the asynchronous
/// activation callback has finished. `PROPVARIANT::drop` can clear a BLOB, so
/// its payload must use the COM allocator rather than borrowed Rust storage.
struct ProcessLoopbackActivation {
    params: PROPVARIANT,
}

unsafe impl Send for ProcessLoopbackActivation {}
unsafe impl Sync for ProcessLoopbackActivation {}

impl ProcessLoopbackActivation {
    fn new(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<Self, String> {
        let activation = unsafe {
            CoTaskMemAlloc(size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>())
                .cast::<AUDIOCLIENT_ACTIVATION_PARAMS>()
        };
        if activation.is_null() {
            return Err("failed to allocate process-loopback activation parameters".to_string());
        }
        unsafe {
            ptr::write(
                activation,
                AUDIOCLIENT_ACTIVATION_PARAMS {
                    ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                    Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                        ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                            TargetProcessId: pid,
                            ProcessLoopbackMode: mode,
                        },
                    },
                },
            );
        }
        let blob = BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: activation.cast(),
        };
        Ok(Self {
            params: PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_BLOB,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 { blob },
                    }),
                },
            },
        })
    }

    fn as_ptr(&self) -> *const PROPVARIANT {
        &self.params
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationCompletion {
    result: Mutex<Option<mpsc::SyncSender<Result<SendComInterface, String>>>>,
    // Keep the BLOB and its payload alive for the whole async operation,
    // including the timeout path where the requesting worker returns early.
    activation: Arc<ProcessLoopbackActivation>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationCompletion_Impl {
    fn ActivateCompleted(
        &self,
        activate_operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows_core::Result<()> {
        let result = (|| {
            let Some(operation) = activate_operation.as_ref() else {
                return Err(
                    "process-loopback activation completed without an operation".to_string()
                );
            };
            let mut activation_result = HRESULT(0);
            let mut activated_interface: Option<IUnknown> = None;
            unsafe {
                operation
                    .GetActivateResult(&mut activation_result, &mut activated_interface)
                    .map_err(|error| {
                        format!("process-loopback activation result failed: {error}")
                    })?;
            }
            if activation_result.is_err() {
                return Err(format!(
                    "process-loopback activation failed: {}",
                    WindowsError::from(activation_result)
                ));
            }
            let activated_interface = activated_interface
                .ok_or_else(|| "process-loopback activation returned no interface".to_string())?;
            let audio_client = activated_interface
                .cast::<IAudioClient>()
                .map_err(|error| format!("activated interface is not an audio client: {error}"))?;
            Ok(SendComInterface(audio_client.into_raw()))
        })();

        let sender = self
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(sender) = sender {
            if let Err(error) = sender.send(result) {
                if let Ok(interface) = error.0 {
                    // If the waiting thread timed out, the successful result
                    // still owns a COM reference. Release it rather than
                    // leaking it.
                    drop(unsafe { IAudioClient::from_raw(interface.0) });
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleFormat {
    U8,
    I16,
    I24,
    I32,
    F32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PcmFormat {
    sample_rate: u32,
    channels: usize,
    block_align: usize,
    bytes_per_sample: usize,
    sample_format: SampleFormat,
}

/// Read the packed mix format returned by `IAudioClient::GetMixFormat`.
/// Windows render endpoints are little-endian; the decoder still goes through
/// explicit little-endian conversions so byte order is not implicit in casts.
unsafe fn pcm_format_from_ptr(format: *const WAVEFORMATEX) -> Result<PcmFormat, String> {
    if format.is_null() {
        return Err("WASAPI returned a null mix format".to_string());
    }
    let base = ptr::read_unaligned(format);
    let format_tag = base.wFormatTag;
    let channels_raw = base.nChannels;
    let sample_rate = base.nSamplesPerSec;
    let bits_per_sample_raw = base.wBitsPerSample;
    let block_align_raw = base.nBlockAlign;
    let extra_size = base.cbSize;
    if channels_raw == 0 || sample_rate == 0 {
        return Err("WASAPI returned an invalid channel count or sample rate".to_string());
    }

    let (sample_format, bits_per_sample) = if format_tag == WAVE_FORMAT_EXTENSIBLE {
        if extra_size < (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16 {
            return Err("WASAPI extensible format is missing its subtype".to_string());
        }
        let extended = ptr::read_unaligned(format.cast::<WAVEFORMATEXTENSIBLE>());
        let valid_bits = ptr::read_unaligned(ptr::addr_of!(extended.Samples.wValidBitsPerSample));
        let sub_format = ptr::read_unaligned(ptr::addr_of!(extended.SubFormat));
        let sample_format = if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
            match bits_per_sample_raw {
                8 => SampleFormat::U8,
                16 => SampleFormat::I16,
                24 => SampleFormat::I24,
                32 => SampleFormat::I32,
                bits => {
                    return Err(format!(
                        "unsupported WASAPI PCM container width {bits} (valid bits {valid_bits})"
                    ));
                }
            }
        } else if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            && bits_per_sample_raw == WAVE_FORMAT_32BIT_FLOAT
        {
            SampleFormat::F32
        } else {
            return Err(format!(
                "unsupported WASAPI extensible subtype {:?} with {} bits",
                sub_format, bits_per_sample_raw
            ));
        };
        (sample_format, bits_per_sample_raw)
    } else {
        match (format_tag, bits_per_sample_raw) {
            (tag, 8) if tag == WAVE_FORMAT_PCM as u16 => (SampleFormat::U8, 8),
            (tag, 16) if tag == WAVE_FORMAT_PCM as u16 => (SampleFormat::I16, 16),
            (tag, WAVE_FORMAT_24BIT_PACKED) if tag == WAVE_FORMAT_PCM as u16 => {
                (SampleFormat::I24, 24)
            }
            (tag, WAVE_FORMAT_32BIT_INTEGER) if tag == WAVE_FORMAT_PCM as u16 => {
                (SampleFormat::I32, 32)
            }
            (WAVE_FORMAT_IEEE_FLOAT, WAVE_FORMAT_32BIT_FLOAT) => (SampleFormat::F32, 32),
            _ => {
                return Err(format!(
                    "unsupported WASAPI PCM format tag {} / {} bits",
                    format_tag, bits_per_sample_raw
                ));
            }
        }
    };

    let bytes_per_sample = usize::from(bits_per_sample / 8);
    let channels = usize::from(channels_raw);
    let minimum_block_align = channels
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| "WASAPI block alignment overflow".to_string())?;
    let block_align = usize::from(block_align_raw);
    if block_align < minimum_block_align || block_align == 0 {
        return Err(format!(
            "WASAPI block alignment {block_align} is smaller than {minimum_block_align}"
        ));
    }
    Ok(PcmFormat {
        sample_rate,
        channels,
        block_align,
        bytes_per_sample,
        sample_format,
    })
}

fn decode_pcm_buffer(
    format: PcmFormat,
    data: *const u8,
    frames: u32,
    silent: bool,
) -> Result<PcmChunk, String> {
    let frame_count =
        usize::try_from(frames).map_err(|_| "WASAPI frame count overflow".to_string())?;
    let sample_count = frame_count
        .checked_mul(format.channels)
        .ok_or_else(|| "WASAPI sample count overflow".to_string())?;
    if silent {
        return PcmChunk::silence(frame_count, format.sample_rate, format.channels)
            .map_err(|error| error.to_string());
    }
    let byte_count = frame_count
        .checked_mul(format.block_align)
        .ok_or_else(|| "WASAPI byte count overflow".to_string())?;
    if data.is_null() {
        return Err("WASAPI returned a null non-silent audio buffer".to_string());
    }
    let bytes = unsafe { slice::from_raw_parts(data, byte_count) };
    let mut samples = Vec::with_capacity(sample_count);
    for frame in 0..frame_count {
        let frame_offset = frame * format.block_align;
        for channel in 0..format.channels {
            let offset = frame_offset + channel * format.bytes_per_sample;
            let sample = match format.sample_format {
                SampleFormat::U8 => (f32::from(bytes[offset]) - 128.0) / 128.0,
                SampleFormat::I16 => {
                    let value = i16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
                    if value == i16::MIN {
                        -1.0
                    } else {
                        value as f32 / i16::MAX as f32
                    }
                }
                SampleFormat::I24 => {
                    let raw = i32::from(bytes[offset])
                        | (i32::from(bytes[offset + 1]) << 8)
                        | (i32::from(bytes[offset + 2]) << 16);
                    let signed = if raw & 0x0080_0000 != 0 {
                        raw | !0x00ff_ffff
                    } else {
                        raw
                    };
                    signed as f32 / 8_388_607.0
                }
                SampleFormat::I32 => {
                    let value = i32::from_le_bytes([
                        bytes[offset],
                        bytes[offset + 1],
                        bytes[offset + 2],
                        bytes[offset + 3],
                    ]);
                    value as f32 / i32::MAX as f32
                }
                SampleFormat::F32 => f32::from_le_bytes([
                    bytes[offset],
                    bytes[offset + 1],
                    bytes[offset + 2],
                    bytes[offset + 3],
                ]),
            };
            samples.push(sample);
        }
    }
    PcmChunk::from_f32(samples, format.sample_rate, format.channels)
        .map_err(|error| error.to_string())
}

fn windows_build_number() -> Option<u32> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    let status = unsafe { RtlGetVersion(&mut version) };
    (status.0 >= 0).then_some(version.dwBuildNumber)
}

fn process_loopback_supported() -> bool {
    windows_build_number().is_some_and(|build| build >= PROCESS_LOOPBACK_MIN_BUILD)
}

fn process_is_alive(pid: u32) -> Result<ProcessHandle, String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|error| format!("process {pid} is not available for WASAPI loopback: {error}"))?;
    let mut exit_code = 0;
    unsafe { GetExitCodeProcess(handle, &mut exit_code) }
        .map_err(|error| format!("failed to query process {pid} liveness: {error}"))?;
    if exit_code != STILL_ACTIVE.0 as u32 {
        unsafe { CloseHandle(handle) }.ok();
        return Err(format!("process {pid} is not running"));
    }
    Ok(ProcessHandle(handle))
}

struct ProcessHandle(HANDLE);

impl ProcessHandle {
    fn is_alive(&self) -> Result<bool, String> {
        let mut exit_code = 0;
        unsafe { GetExitCodeProcess(self.0, &mut exit_code) }
            .map_err(|error| format!("failed to query process liveness: {error}"))?;
        Ok(exit_code == STILL_ACTIVE.0 as u32)
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) }.ok();
    }
}

fn default_render_audio_client() -> Result<IAudioClient, String> {
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|error| format!("failed to create audio endpoint enumerator: {error}"))?
    };
    let endpoint = unsafe {
        enumerator
            .GetDefaultAudioEndpoint(eRender, eMultimedia)
            .map_err(|error| format!("failed to open default render endpoint: {error}"))?
    };
    unsafe { endpoint.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|error| format!("failed to activate render loopback client: {error}"))
}

fn process_loopback_audio_client(
    pid: u32,
    mode: PROCESS_LOOPBACK_MODE,
) -> Result<IAudioClient, String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let activation = Arc::new(ProcessLoopbackActivation::new(pid, mode)?);
    let completion = ActivationCompletion {
        result: Mutex::new(Some(sender)),
        activation: Arc::clone(&activation),
    };
    let completion: IActivateAudioInterfaceCompletionHandler = completion.into();
    let _operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(activation.as_ptr()),
            &completion,
        )
        .map_err(|error| format!("failed to start process-loopback activation: {error}"))?
    };
    let activated = receiver
        .recv_timeout(ACTIVATION_TIMEOUT)
        .map_err(|error| format!("timed out waiting for process-loopback activation: {error}"))??;
    Ok(unsafe { activated.into_audio_client() })
}

fn open_audio_client(
    source: AudioSourceKey,
) -> Result<(IAudioClient, Option<ProcessHandle>, bool), String> {
    match source {
        AudioSourceKey::SystemOutput if process_loopback_supported() => {
            // Excluding Petal's own process prevents remote playback from being
            // captured and fed back into the room. If this optional activation
            // fails, the ordinary endpoint loopback is still useful and keeps
            // system audio available on partially supported drivers.
            match process_loopback_audio_client(
                unsafe { GetCurrentProcessId() },
                PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            ) {
                Ok(client) => Ok((client, None, true)),
                Err(error) => {
                    log::warn!(
                        "audio: process-excluding system loopback unavailable ({error}); using ordinary render loopback"
                    );
                    Ok((default_render_audio_client()?, None, false))
                }
            }
        }
        AudioSourceKey::SystemOutput => Ok((default_render_audio_client()?, None, false)),
        AudioSourceKey::Process(pid) => {
            let process = process_is_alive(pid)?;
            if !process_loopback_supported() {
                return Err(format!(
                    "process audio requires Windows 10 build {PROCESS_LOOPBACK_MIN_BUILD} or newer"
                ));
            }
            let client = process_loopback_audio_client(
                pid,
                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            )?;
            Ok((client, Some(process), true))
        }
    }
}

/// The process-loopback endpoint is virtual and does not expose a mix format:
/// `IAudioClient::GetMixFormat` returns `E_NOTIMPL` for it. Ask WASAPI for a
/// fixed format instead and let the audio engine convert from each rendered
/// stream with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`.
fn process_loopback_format() -> (WAVEFORMATEX, PcmFormat) {
    let channels = SCREEN_AUDIO_CHANNELS;
    let bytes_per_sample = size_of::<i16>();
    let block_align = channels * bytes_per_sample;
    let wave_format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: channels as u16,
        nSamplesPerSec: SCREEN_AUDIO_SAMPLE_RATE,
        nAvgBytesPerSec: SCREEN_AUDIO_SAMPLE_RATE * block_align as u32,
        nBlockAlign: block_align as u16,
        wBitsPerSample: (bytes_per_sample * 8) as u16,
        cbSize: 0,
    };
    let pcm_format = PcmFormat {
        sample_rate: SCREEN_AUDIO_SAMPLE_RATE,
        channels,
        block_align,
        bytes_per_sample,
        sample_format: SampleFormat::I16,
    };
    (wave_format, pcm_format)
}

fn initialize_audio_client(
    client: &IAudioClient,
    event: HANDLE,
    wave_format: *const WAVEFORMATEX,
    format: PcmFormat,
    stream_flags: u32,
) -> Result<(IAudioCaptureClient, PcmFormat), String> {
    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                stream_flags,
                CAPTURE_BUFFER_DURATION_HNS,
                0,
                wave_format,
                None,
            )
            .map_err(|error| format!("failed to initialize WASAPI loopback: {error}"))?;
        client
            .SetEventHandle(event)
            .map_err(|error| format!("failed to attach WASAPI event: {error}"))?;
        let capture = client
            .GetService::<IAudioCaptureClient>()
            .map_err(|error| format!("failed to acquire WASAPI capture client: {error}"))?;
        client
            .Start()
            .map_err(|error| format!("failed to start WASAPI loopback: {error}"))?;
        Ok((capture, format))
    }
}

fn configure_audio_client(
    client: &IAudioClient,
    event: HANDLE,
    process_loopback: bool,
) -> Result<(IAudioCaptureClient, PcmFormat), String> {
    if process_loopback {
        let (wave_format, format) = process_loopback_format();
        return initialize_audio_client(
            client,
            event,
            &wave_format,
            format,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        );
    }

    let mix_format = unsafe { client.GetMixFormat() }
        .map_err(|error| format!("failed to read WASAPI mix format: {error}"))?;
    let configured = (|| {
        let format = unsafe { pcm_format_from_ptr(mix_format)? };
        initialize_audio_client(
            client,
            event,
            mix_format,
            format,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        )
    })();
    unsafe { CoTaskMemFree(Some(mix_format.cast())) };
    configured
}

fn stop_requested(requested: &AtomicBool) -> bool {
    requested.load(Ordering::Acquire)
}

fn run_capture(
    client: IAudioClient,
    capture: IAudioCaptureClient,
    format: PcmFormat,
    event: HANDLE,
    stop: Arc<AtomicBool>,
    process: Option<ProcessHandle>,
    ingress: ScreenAudioIngress,
) -> Result<(), String> {
    loop {
        if stop_requested(&stop) {
            break;
        }
        let wait = unsafe { WaitForSingleObject(event, WAIT_TIMEOUT_MS) };
        if wait.0 == u32::MAX {
            return Err("WASAPI event wait failed".to_string());
        }
        if let Some(process) = process.as_ref() {
            if !process.is_alive()? {
                return Err("captured process exited".to_string());
            }
        }

        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let packet_frames = unsafe { capture.GetNextPacketSize() }
                .map_err(|error| format!("failed to read WASAPI packet size: {error}"))?;
            if packet_frames == 0 {
                break;
            }
            let mut data = ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            unsafe {
                capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(|error| format!("failed to read WASAPI packet: {error}"))?;
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let decoded = decode_pcm_buffer(format, data, frames, silent);
            let released = unsafe { capture.ReleaseBuffer(frames) };
            released.map_err(|error| format!("failed to release WASAPI packet: {error}"))?;
            let chunk = decoded?;
            ingress.push(chunk).map_err(|error| error.to_string())?;
        }
    }
    unsafe { client.Stop() }.map_err(|error| format!("failed to stop WASAPI loopback: {error}"))?;
    Ok(())
}

fn worker(
    source: AudioSourceKey,
    event: HANDLE,
    stop: Arc<AtomicBool>,
    ingress: ScreenAudioIngress,
    failed: Arc<AtomicBool>,
    on_error: Arc<dyn Fn(String) + Send + Sync>,
    ready: mpsc::SyncSender<Result<(), String>>,
) {
    let _apartment = match ComApartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let (client, process, process_loopback) = match open_audio_client(source) {
        Ok(opened) => opened,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let (capture, format) = match configure_audio_client(&client, event, process_loopback) {
        Ok(configured) => configured,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    if let Err(error) = run_capture(
        client,
        capture,
        format,
        event,
        stop.clone(),
        process,
        ingress,
    ) {
        if !stop.load(Ordering::Acquire) && !failed.swap(true, Ordering::AcqRel) {
            on_error(error);
        }
    }
}

pub(crate) struct WindowsScreenAudioCapture {
    source: AudioSourceKey,
    ingress: ScreenAudioIngress,
    // Keep the opaque kernel handle as an integer so this owner remains
    // naturally Send + Sync. All COM/WASAPI interfaces stay on the worker;
    // the owner only signals/closes this process-wide event and joins that
    // worker after it has stopped using the handle.
    stop_event: usize,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    stopped: AtomicBool,
}

impl WindowsScreenAudioCapture {
    pub(crate) fn start(
        source: AudioSourceKey,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        if let AudioSourceKey::Process(pid) = source {
            if pid == 0 {
                return Err("invalid process id for screen audio".to_string());
            }
            if !process_loopback_supported() {
                return Err(format!(
                    "process audio requires Windows 10 build {PROCESS_LOOPBACK_MIN_BUILD} or newer"
                ));
            }
            let _ = process_is_alive(pid)?;
        }

        let stop_event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| format!("failed to create screen-audio stop event: {error}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let ingress = ScreenAudioIngress::new();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let callback: Arc<dyn Fn(String) + Send + Sync> = Arc::new(on_error);
        let thread_stop = stop.clone();
        let thread_ingress = ingress.clone();
        let thread_failed = failed.clone();
        let thread_name = format!("petal-screen-audio-{}", source.label());
        let worker_event = stop_event.0 as usize;
        let worker = match thread::Builder::new().name(thread_name).spawn(move || {
            let worker_event = HANDLE(worker_event as *mut std::ffi::c_void);
            worker(
                source,
                worker_event,
                thread_stop,
                thread_ingress,
                thread_failed,
                callback,
                ready_sender,
            )
        }) {
            Ok(worker) => worker,
            Err(error) => {
                unsafe { CloseHandle(stop_event) }.ok();
                return Err(format!("failed to start screen-audio worker: {error}"));
            }
        };

        match ready_receiver.recv_timeout(ACTIVATION_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                source,
                ingress,
                stop_event: stop_event.0 as usize,
                stop,
                failed,
                worker: Mutex::new(Some(worker)),
                stopped: AtomicBool::new(false),
            }),
            Ok(Err(error)) => {
                let _ = unsafe { SetEvent(stop_event) };
                let _ = worker.join();
                unsafe { CloseHandle(stop_event) }.ok();
                Err(error)
            }
            Err(error) => {
                stop.store(true, Ordering::Release);
                unsafe { SetEvent(stop_event) }.ok();
                let _ = worker.join();
                unsafe { CloseHandle(stop_event) }.ok();
                Err(format!("screen-audio worker startup failed: {error}"))
            }
        }
    }

    pub(crate) fn queue(&self) -> ScreenAudioQueue {
        self.ingress.queue()
    }

    pub(crate) fn close_queue(&self) {
        self.ingress.close();
    }

    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub(crate) fn stop(&self) -> Result<(), String> {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.stop.store(true, Ordering::Release);
        self.ingress.close();
        let stop_event = HANDLE(self.stop_event as *mut std::ffi::c_void);
        let signal_error = unsafe { SetEvent(stop_event) }
            .err()
            .map(|error| format!("failed to signal screen-audio stop: {error}"));
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let join_error = worker.and_then(|worker| {
            worker
                .join()
                .err()
                .map(|_| format!("screen-audio worker '{}' panicked", self.source.label()))
        });
        let close_error = unsafe { CloseHandle(stop_event) }
            .err()
            .map(|error| format!("failed to close screen-audio stop event: {error}"));
        signal_error
            .or(join_error)
            .or(close_error)
            .map_or(Ok(()), Err)
    }
}

impl Drop for WindowsScreenAudioCapture {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::debug!(
                "audio: Windows screen-audio source '{}' drop failed: {error}",
                self.source.label()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(sample_format: SampleFormat, channels: usize, rate: u32) -> PcmFormat {
        let bytes_per_sample = match sample_format {
            SampleFormat::U8 => 1,
            SampleFormat::I16 => 2,
            SampleFormat::I24 => 3,
            SampleFormat::I32 | SampleFormat::F32 => 4,
        };
        PcmFormat {
            sample_rate: rate,
            channels,
            block_align: bytes_per_sample * channels,
            bytes_per_sample,
            sample_format,
        }
    }

    #[test]
    fn process_loopback_activation_owns_its_blob_before_drop() {
        // Regression: a VT_BLOB PROPVARIANT owns its payload on drop. The
        // old helper pointed it at stack storage, so PropVariantClear called
        // RtlFreeHeap on that stack address and crashed the worker.
        let activation =
            ProcessLoopbackActivation::new(42, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE)
                .unwrap();
        assert_eq!(activation.params.vt(), VT_BLOB);
        let blob = unsafe { activation.params.Anonymous.Anonymous.Anonymous.blob };
        assert_eq!(
            blob.cbSize,
            size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32
        );
        assert!(!blob.pBlobData.is_null());
        drop(activation);
    }

    #[test]
    fn process_loopback_uses_explicit_common_pcm_format() {
        let (wave_format, pcm_format) = process_loopback_format();
        let format_tag = unsafe { ptr::read_unaligned(ptr::addr_of!(wave_format.wFormatTag)) };
        let channels = unsafe { ptr::read_unaligned(ptr::addr_of!(wave_format.nChannels)) };
        let sample_rate = unsafe { ptr::read_unaligned(ptr::addr_of!(wave_format.nSamplesPerSec)) };
        let bits_per_sample =
            unsafe { ptr::read_unaligned(ptr::addr_of!(wave_format.wBitsPerSample)) };
        let block_align = unsafe { ptr::read_unaligned(ptr::addr_of!(wave_format.nBlockAlign)) };
        let avg_bytes_per_sec =
            unsafe { ptr::read_unaligned(ptr::addr_of!(wave_format.nAvgBytesPerSec)) };
        let extra_size = unsafe { ptr::read_unaligned(ptr::addr_of!(wave_format.cbSize)) };
        assert_eq!(format_tag, WAVE_FORMAT_PCM as u16);
        assert_eq!(channels, SCREEN_AUDIO_CHANNELS as u16);
        assert_eq!(sample_rate, SCREEN_AUDIO_SAMPLE_RATE);
        assert_eq!(bits_per_sample, 16);
        assert_eq!(block_align, 4);
        assert_eq!(avg_bytes_per_sec, SCREEN_AUDIO_SAMPLE_RATE * 4);
        assert_eq!(extra_size, 0);
        assert_eq!(pcm_format.sample_rate, SCREEN_AUDIO_SAMPLE_RATE);
        assert_eq!(pcm_format.channels, SCREEN_AUDIO_CHANNELS);
        assert_eq!(pcm_format.block_align, 4);
        assert_eq!(pcm_format.bytes_per_sample, 2);
        assert_eq!(pcm_format.sample_format, SampleFormat::I16);
        assert_eq!(
            unsafe { pcm_format_from_ptr(&wave_format).unwrap() },
            pcm_format
        );
    }

    #[test]
    fn decodes_little_endian_stereo_s16() {
        let bytes = [0x00, 0x20, 0x00, 0xe0, 0xff, 0x7f, 0x00, 0x80];
        let chunk = decode_pcm_buffer(
            format(SampleFormat::I16, 2, 48_000),
            bytes.as_ptr(),
            2,
            false,
        )
        .unwrap();
        let frames = crate::screen_audio::PcmFrameAssembler::default()
            .push(chunk)
            .unwrap();
        assert_eq!(frames.len(), 0); // less than one 10 ms frame
    }

    #[test]
    fn silent_packet_does_not_dereference_null_data() {
        let chunk = decode_pcm_buffer(
            format(SampleFormat::I16, SCREEN_AUDIO_CHANNELS, 48_000),
            ptr::null(),
            480,
            true,
        )
        .unwrap();
        let frames = crate::screen_audio::PcmFrameAssembler::default()
            .push(chunk)
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].samples().iter().all(|sample| *sample == 0));
    }
}
