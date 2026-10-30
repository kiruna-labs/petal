//! Windows client-mode encoders:
//!
//! * [`Converter`]: the D3D11 Video Processor turns a BGRA frame into NV12
//!   at the output size, scaled to fit and letterboxed in black, on the GPU.
//! * [`H264Encoder`]: the hardware Media Foundation H.264 MFT (async, fed
//!   DXGI surfaces through a DXGI device manager), falling back to
//!   Microsoft's software MFT (sync, fed system memory). High profile, no
//!   B-frames, a keyframe every 2 s, peak-constrained VBR, low latency.
//! * [`AacEncoder`]: the Media Foundation AAC-LC MFT, 48 kHz stereo 128 kbps.
//!
//! Everything here runs on one encoder thread that owns its own D3D11 device
//! (never the capture device, but on the capture's adapter when it is
//! known), inside a COM MTA with Media Foundation started. Frames arrive
//! either as shared textures from the capture thread's GPU tap
//! ([`super::gpu_tap`], opened here with `OpenSharedResource1`) or, as the
//! fallback, as CPU BGRA from the share's capture callback; either way they
//! are copied in once per new frame.

use std::mem::ManuallyDrop;

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{HMODULE, RECT, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_YCbCrA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIKeyedMutex};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_PeakConstrainedVBR, eAVEncH264VProfile_High, ICodecAPI,
    IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFShutdown, IMFTransform, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Audio, MFMediaType_Video, MFNominalRange_16_235, MFShutdown, MFStartup,
    MFTEnumEx, MFT_CATEGORY_AUDIO_ENCODER, MFT_CATEGORY_VIDEO_ENCODER, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709,
    MFVideoTransFunc_709, MFVideoTransferMatrix_BT709, MFSTARTUP_FULL,
    MFSampleExtension_CleanPoint, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MF_VERSION, CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

use super::gpu_tap::{acquire_sync, luid_to_i64, GpuFrame, CONSUMER_ACQUIRE_MS};
use super::plan::{STREAM_AUDIO_BITRATE_BPS, STREAM_AUDIO_SAMPLE_RATE, STREAM_KEYFRAME_INTERVAL_S};

/// METransformNeedInput / METransformHaveOutput.
const ME_TRANSFORM_NEED_INPUT: u32 = 601;
const ME_TRANSFORM_HAVE_OUTPUT: u32 = 602;
/// Input surfaces in flight to an async encoder.
const NV12_RING: usize = 6;

fn err(context: &str, error: windows::core::Error) -> String {
    format!("{context}: {error}")
}

/// COM (MTA) and Media Foundation for the encoder thread's lifetime.
pub struct MediaRuntime {
    uninit_com: bool,
}

impl MediaRuntime {
    pub fn start() -> Result<Self, String> {
        // SAFETY: balanced by Drop on this same thread.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let uninit_com = if hr == RPC_E_CHANGED_MODE {
            false
        } else {
            hr.ok().map_err(|e| err("CoInitializeEx", e))?;
            true
        };
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            if uninit_com {
                unsafe { CoUninitialize() };
            }
            return Err(err("MFStartup", error));
        }
        Ok(Self { uninit_com })
    }
}

impl Drop for MediaRuntime {
    fn drop(&mut self) {
        // SAFETY: paired with `start`.
        unsafe {
            let _ = MFShutdown();
            if self.uninit_com {
                CoUninitialize();
            }
        }
    }
}

/// The encoder's own D3D11 device, video-capable and multithread-protected
/// (Media Foundation touches it from its worker threads).
pub struct EncoderDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    manager: Option<(IMFDXGIDeviceManager, u32)>,
}

/// The DXGI adapter with LUID `luid`, if it is still there.
fn find_adapter(luid: i64) -> Option<IDXGIAdapter> {
    // SAFETY: plain factory and adapter queries.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    (0..)
        .map_while(|index| unsafe { factory.EnumAdapters1(index) }.ok())
        .find(|adapter| unsafe { adapter.GetDesc1() }.is_ok_and(|desc| luid_to_i64(desc.AdapterLuid) == luid))
        .and_then(|adapter| adapter.cast().ok())
}

impl EncoderDevice {
    pub fn create() -> Result<Self, String> {
        Self::create_on(None)
    }

    /// On the adapter with LUID `luid` (the capture device's, so its shared
    /// frames open here), or the default hardware adapter.
    pub fn create_on(luid: Option<i64>) -> Result<Self, String> {
        let adapter = luid.and_then(find_adapter);
        // An explicit adapter requires the UNKNOWN driver type.
        let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
        let mut device = None;
        let mut context = None;
        // SAFETY: out-params are valid Options.
        unsafe {
            D3D11CreateDevice(
                adapter.as_ref(),
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|e| err("D3D11CreateDevice (video)", e))?;
        let device: ID3D11Device = device.ok_or("no D3D11 device")?;
        let context: ID3D11DeviceContext = context.ok_or("no D3D11 context")?;
        if let Ok(multithread) = context.cast::<ID3D11Multithread>() {
            // SAFETY: plain setter.
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }
        let manager = (|| {
            let mut token = 0u32;
            let mut manager = None;
            // SAFETY: out-params are valid.
            unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }.ok()?;
            let manager: IMFDXGIDeviceManager = manager?;
            unsafe { manager.ResetDevice(&device, token) }.ok()?;
            Some((manager, token))
        })();
        Ok(Self { device, context, manager })
    }

    pub fn adapter_luid(&self) -> Option<i64> {
        super::gpu_tap::adapter_luid(&self.device)
    }
}

fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    staging: bool,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
        BindFlags: if staging {
            0
        } else if format == DXGI_FORMAT_NV12 {
            D3D11_BIND_RENDER_TARGET.0 as u32
        } else {
            (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32
        },
        CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: desc is fully initialized; no initial data.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| err("CreateTexture2D", e))?;
    texture.ok_or_else(|| "CreateTexture2D returned nothing".into())
}

/// A borrowed BGRA frame from the capture callback.
pub struct BgraView<'a> {
    pub bgra: &'a [u8],
    pub bytes_per_row: u32,
    pub width: u32,
    pub height: u32,
}

struct Processor {
    input_size: (u32, u32),
    output_size: (u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
}

/// A GPU tap slot opened on the encoder device.
struct OpenedSlot {
    pool_id: u64,
    index: usize,
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
}

/// BGRA -> NV12 with scale-to-fit and black letterboxing.
pub struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    input: Option<(ID3D11Texture2D, (u32, u32), Option<ID3D11VideoProcessorInputView>)>,
    /// The current GPU tap pool's slots, opened as they first arrive.
    shared: Vec<OpenedSlot>,
    processor: Option<Processor>,
    outputs: Vec<(ID3D11Texture2D, Option<ID3D11VideoProcessorOutputView>)>,
    output_size: (u32, u32),
    next_output: usize,
}

impl Converter {
    pub fn new(device: &EncoderDevice, output_size: (u32, u32)) -> Result<Self, String> {
        let video_device = device.device.cast::<ID3D11VideoDevice>().map_err(|e| err("ID3D11VideoDevice", e))?;
        let video_context =
            device.context.cast::<ID3D11VideoContext>().map_err(|e| err("ID3D11VideoContext", e))?;
        let mut converter = Self {
            device: device.device.clone(),
            context: device.context.clone(),
            video_device,
            video_context,
            input: None,
            shared: Vec::new(),
            processor: None,
            outputs: Vec::new(),
            output_size,
            next_output: 0,
        };
        converter.resize_output(output_size)?;
        Ok(converter)
    }

    /// Change the canvas (follow the source). Invalidates the processor.
    pub fn resize_output(&mut self, size: (u32, u32)) -> Result<(), String> {
        self.output_size = size;
        self.processor = None;
        self.outputs = (0..NV12_RING)
            .map(|_| texture(&self.device, size.0, size.1, DXGI_FORMAT_NV12, false).map(|t| (t, None)))
            .collect::<Result<_, _>>()?;
        Ok(())
    }

    /// Upload a new source frame (only when the capture delivered one).
    pub fn upload(&mut self, frame: &BgraView<'_>) -> Result<(), String> {
        let size = (frame.width, frame.height);
        if frame.width == 0 || frame.height == 0 {
            return Err("empty frame".into());
        }
        let needed = frame.bytes_per_row as usize * (frame.height as usize - 1) + frame.width as usize * 4;
        if frame.bgra.len() < needed {
            return Err("short frame buffer".into());
        }
        let tex = self.ensure_input(size)?;
        // SAFETY: the source buffer covers `height` rows of `bytes_per_row`.
        unsafe {
            self.context.UpdateSubresource(
                &tex,
                0,
                None,
                frame.bgra.as_ptr().cast(),
                frame.bytes_per_row,
                0,
            );
        }
        Ok(())
    }

    /// The input texture at `size`, recreated (with the processor) when the
    /// source size changes.
    fn ensure_input(&mut self, size: (u32, u32)) -> Result<ID3D11Texture2D, String> {
        if self.input.as_ref().is_none_or(|(_, existing, _)| *existing != size) {
            let tex = texture(&self.device, size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, false)?;
            self.input = Some((tex, size, None));
            self.processor = None;
        }
        self.input.as_ref().map(|(tex, _, _)| tex.clone()).ok_or_else(|| "no input texture".into())
    }

    /// Copy a GPU tap frame into the input: the capture device's shared
    /// texture, opened here once per slot and read under its keyed mutex.
    pub fn upload_texture(&mut self, frame: &GpuFrame) -> Result<(), String> {
        let (width, height) = frame.size();
        if width == 0 || height == 0 {
            return Err("empty frame".into());
        }
        // A new pool (the share resized, or another share): the old one's
        // textures are done with.
        self.shared.retain(|slot| slot.pool_id == frame.pool_id());
        let position = self.shared.iter().position(|slot| slot.index == frame.index());
        let position = match position {
            Some(position) => position,
            None => {
                let device1: ID3D11Device1 = self.device.cast().map_err(|e| err("ID3D11Device1", e))?;
                // SAFETY: the handle stays open while `frame` holds its pool.
                let texture: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(frame.shared_handle()) }
                    .map_err(|e| err("OpenSharedResource1", e))?;
                let mutex: IDXGIKeyedMutex = texture.cast().map_err(|e| err("IDXGIKeyedMutex", e))?;
                self.shared.push(OpenedSlot { pool_id: frame.pool_id(), index: frame.index(), texture, mutex });
                self.shared.len() - 1
            }
        };
        let input = self.ensure_input((width, height))?;
        let slot = &self.shared[position];
        if !acquire_sync(&slot.mutex, 0, CONSUMER_ACQUIRE_MS)? {
            return Err("timed out waiting for a shared frame".into());
        }
        // SAFETY: same size and format; the keyed mutex is held for the copy.
        unsafe {
            self.context.CopySubresourceRegion(&input, 0, 0, 0, 0, &slot.texture, 0, None);
            slot.mutex.ReleaseSync(0).map_err(|e| err("ReleaseSync", e))?;
        }
        Ok(())
    }

    /// Fill a solid frame (black) into the input, for when no share frame
    /// exists yet.
    pub fn has_input(&self) -> bool {
        self.input.is_some()
    }

    fn ensure_processor(&mut self) -> Result<(), String> {
        let input_size = self.input.as_ref().map(|(_, size, _)| *size).ok_or("no input")?;
        if self
            .processor
            .as_ref()
            .is_some_and(|p| p.input_size == input_size && p.output_size == self.output_size)
        {
            return Ok(());
        }
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: input_size.0,
            InputHeight: input_size.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: self.output_size.0,
            OutputHeight: self.output_size.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: desc is valid for the call.
        let enumerator = unsafe { self.video_device.CreateVideoProcessorEnumerator(&desc) }
            .map_err(|e| err("CreateVideoProcessorEnumerator", e))?;
        let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|e| err("CreateVideoProcessor", e))?;
        // Views are bound to an enumerator: rebuild them.
        if let Some((_, _, view)) = self.input.as_mut() {
            *view = None;
        }
        for (_, view) in &mut self.outputs {
            *view = None;
        }
        // RGB full range in; BT.709 limited-range YCbCr out.
        let input_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        let output_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                YCbCr: D3D11_VIDEO_COLOR_YCbCrA { Y: 16.0 / 255.0, Cb: 0.5, Cr: 0.5, A: 1.0 },
            },
        };
        // SAFETY: all pointers reference locals valid for each call.
        unsafe {
            self.video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            self.video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input_space);
            self.video_context.VideoProcessorSetOutputColorSpace(&processor, &output_space);
            self.video_context.VideoProcessorSetOutputBackgroundColor(&processor, true, &black);
            self.video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        }
        self.processor = Some(Processor {
            input_size,
            output_size: self.output_size,
            enumerator,
            processor,
        });
        Ok(())
    }

    /// Render the current input into the next ring texture, letterboxed into
    /// `dest` (x, y, w, h) on the canvas. Returns that texture.
    pub fn convert(&mut self, dest: (u32, u32, u32, u32)) -> Result<ID3D11Texture2D, String> {
        self.ensure_processor()?;
        let processor = self.processor.as_ref().ok_or("no processor")?;
        let (input_tex, input_size, input_view) = self.input.as_mut().ok_or("no input")?;
        if input_view.is_none() {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut view = None;
            // SAFETY: desc and out-param valid for the call.
            unsafe {
                self.video_device.CreateVideoProcessorInputView(
                    &*input_tex,
                    &processor.enumerator,
                    &desc,
                    Some(&mut view),
                )
            }
            .map_err(|e| err("CreateVideoProcessorInputView", e))?;
            *input_view = view;
        }
        let index = self.next_output;
        self.next_output = (self.next_output + 1) % self.outputs.len();
        let (output_tex, output_view) = &mut self.outputs[index];
        if output_view.is_none() {
            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut view = None;
            // SAFETY: desc and out-param valid for the call.
            unsafe {
                self.video_device.CreateVideoProcessorOutputView(
                    &*output_tex,
                    &processor.enumerator,
                    &desc,
                    Some(&mut view),
                )
            }
            .map_err(|e| err("CreateVideoProcessorOutputView", e))?;
            *output_view = view;
        }
        let source = RECT { left: 0, top: 0, right: input_size.0 as i32, bottom: input_size.1 as i32 };
        let target = RECT {
            left: 0,
            top: 0,
            right: self.output_size.0 as i32,
            bottom: self.output_size.1 as i32,
        };
        let dest = RECT {
            left: dest.0 as i32,
            top: dest.1 as i32,
            right: (dest.0 + dest.2) as i32,
            bottom: (dest.1 + dest.3) as i32,
        };
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(input_view.clone()),
            ..Default::default()
        };
        // SAFETY: rects are locals; the stream's view reference is released
        // below once the call returns.
        let result = unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(&processor.processor, 0, true, Some(&source));
            self.video_context.VideoProcessorSetStreamDestRect(&processor.processor, 0, true, Some(&dest));
            self.video_context.VideoProcessorSetOutputTargetRect(&processor.processor, true, Some(&target));
            self.video_context.VideoProcessorBlt(
                &processor.processor,
                output_view.as_ref(),
                0,
                std::slice::from_ref(&stream),
            )
        };
        drop(ManuallyDrop::into_inner(stream.pInputSurface));
        result.map_err(|e| err("VideoProcessorBlt", e))?;
        Ok(output_tex.clone())
    }
}

/// One encoded H.264 access unit (Annex B).
#[derive(Debug, Clone)]
pub struct EncodedVideo {
    pub data: Vec<u8>,
    pub pts_100ns: i64,
    pub keyframe: bool,
}

/// One raw AAC frame.
#[derive(Debug, Clone)]
pub struct EncodedAudio {
    pub data: Vec<u8>,
    pub pts_100ns: i64,
}

fn enum_transforms(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    input: MFT_REGISTER_TYPE_INFO,
    output: MFT_REGISTER_TYPE_INFO,
) -> Vec<IMFActivate> {
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates an array we take ownership of and free.
    let ok = unsafe { MFTEnumEx(category, flags, Some(&input), Some(&output), &mut activates, &mut count) }
        .is_ok();
    if !ok || activates.is_null() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count as usize);
    // SAFETY: `count` initialized entries; each is moved out exactly once.
    unsafe {
        for index in 0..count as usize {
            if let Some(activate) = std::ptr::read(activates.add(index)) {
                out.push(activate);
            }
        }
        CoTaskMemFree(Some(activates.cast()));
    }
    out
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    // SAFETY: buffer and length are valid out-params.
    if unsafe { activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)) }.is_ok() {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        "unknown".into()
    }
}

fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

fn set_codec_value(codec: &ICodecAPI, api: &GUID, value: VARIANT) {
    // SAFETY: both pointers are valid for the call. Unsupported settings are
    // best-effort: encoders differ in what they accept.
    let _ = unsafe { codec.SetValue(api, &value) };
}

enum Drive {
    /// Hardware: event-driven, DXGI surface input.
    Async { events: IMFMediaEventGenerator, credits: u32, pending_output: u32 },
    /// Software: call-driven, system-memory input.
    Sync { staging: Option<ID3D11Texture2D> },
}

pub struct H264Encoder {
    transform: IMFTransform,
    activate: IMFActivate,
    codec: Option<ICodecAPI>,
    drive: Drive,
    size: (u32, u32),
    fps: u32,
    provides_samples: bool,
    output_buffer_size: u32,
    pub name: String,
    pub hardware: bool,
    sequence_header: Option<Vec<u8>>,
}

impl H264Encoder {
    /// Hardware first, then Microsoft's software encoder.
    pub fn new(device: &EncoderDevice, size: (u32, u32), fps: u32, bitrate_bps: u32, max_bitrate_bps: u32) -> Result<Self, String> {
        Self::new_with(device, size, fps, bitrate_bps, max_bitrate_bps, true)
    }

    fn new_with(
        device: &EncoderDevice,
        size: (u32, u32),
        fps: u32,
        bitrate_bps: u32,
        max_bitrate_bps: u32,
        allow_hardware: bool,
    ) -> Result<Self, String> {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut last_error = String::from("no H.264 encoder found");
        if allow_hardware && device.manager.is_some() {
            for activate in enum_transforms(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                input,
                output,
            ) {
                match Self::open(device, activate, true, size, fps, bitrate_bps, max_bitrate_bps) {
                    Ok(encoder) => return Ok(encoder),
                    Err(error) => {
                        log::warn!("streaming: hardware H.264 encoder unavailable: {error}");
                        last_error = error;
                    }
                }
            }
        }
        for activate in enum_transforms(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            input,
            output,
        ) {
            match Self::open(device, activate, false, size, fps, bitrate_bps, max_bitrate_bps) {
                Ok(encoder) => return Ok(encoder),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn open(
        device: &EncoderDevice,
        activate: IMFActivate,
        hardware: bool,
        size: (u32, u32),
        fps: u32,
        bitrate_bps: u32,
        max_bitrate_bps: u32,
    ) -> Result<Self, String> {
        let name = friendly_name(&activate);
        // SAFETY: COM calls on a live activation object.
        let transform: IMFTransform = unsafe { activate.ActivateObject() }.map_err(|e| err(&name, e))?;
        let fail = |error: String| {
            unsafe {
                let _ = activate.ShutdownObject();
            }
            error
        };
        let drive = if hardware {
            let attributes = unsafe { transform.GetAttributes() }.map_err(|e| fail(err("GetAttributes", e)))?;
            if unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0 {
                unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                    .map_err(|e| fail(err("async unlock", e)))?;
            }
            let (manager, _) = device.manager.as_ref().ok_or_else(|| fail("no DXGI manager".into()))?;
            unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
                .map_err(|e| fail(err("SET_D3D_MANAGER", e)))?;
            let events = transform
                .cast::<IMFMediaEventGenerator>()
                .map_err(|e| fail(err("event generator", e)))?;
            Drive::Async { events, credits: 0, pending_output: 0 }
        } else {
            Drive::Sync { staging: None }
        };
        let codec = transform.cast::<ICodecAPI>().ok();
        if let Some(codec) = &codec {
            set_codec_value(
                codec,
                &CODECAPI_AVEncCommonRateControlMode,
                VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
            );
            set_codec_value(codec, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate_bps));
            set_codec_value(codec, &CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(max_bitrate_bps.max(bitrate_bps)));
            // A one-second VBV buffer at the mean rate.
            set_codec_value(codec, &CODECAPI_AVEncCommonBufferSize, VARIANT::from(bitrate_bps));
            set_codec_value(codec, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps * STREAM_KEYFRAME_INTERVAL_S));
            set_codec_value(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
            set_codec_value(codec, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
        }
        let output_type = video_type(MFVideoFormat_H264, size, fps).map_err(fail)?;
        unsafe {
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps).map_err(|e| fail(err("bitrate", e)))?;
            output_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .map_err(|e| fail(err("profile", e)))?;
            transform.SetOutputType(0, &output_type, 0).map_err(|e| fail(err("SetOutputType", e)))?;
        }
        let input_type = video_type(MFVideoFormat_NV12, size, fps).map_err(fail)?;
        unsafe { transform.SetInputType(0, &input_type, 0) }.map_err(|e| fail(err("SetInputType", e)))?;
        let info = unsafe { transform.GetOutputStreamInfo(0) }.map_err(|e| fail(err("GetOutputStreamInfo", e)))?;
        let provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        let sequence_header = unsafe { transform.GetOutputCurrentType(0) }
            .ok()
            .and_then(|current| blob(&current, &MF_MT_MPEG_SEQUENCE_HEADER));
        unsafe {
            // Microsoft's software encoder rejects a flush before any input.
            let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| fail(err("begin streaming", e)))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| fail(err("start of stream", e)))?;
        }
        log::info!(
            "streaming: H.264 encoder \"{name}\" ({}) {}x{} @ {fps} fps, {bitrate_bps} bps",
            if hardware { "hardware" } else { "software" },
            size.0,
            size.1
        );
        Ok(Self {
            transform,
            activate,
            codec,
            drive,
            size,
            fps,
            provides_samples,
            output_buffer_size: info.cbSize.max(size.0 * size.1 * 3 / 2),
            name,
            hardware,
            sequence_header,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// SPS/PPS (Annex B) from the output type, when the encoder offers them
    /// out of band. Most also repeat them in-band before every IDR.
    pub fn sequence_header(&self) -> Option<&[u8]> {
        self.sequence_header.as_deref()
    }

    pub fn force_keyframe(&self) {
        if let Some(codec) = &self.codec {
            set_codec_value(codec, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32));
        }
    }

    /// Submit one NV12 frame. Returns any output ready now and whether the
    /// frame was accepted (an async encoder that hasn't asked for input
    /// drops it).
    pub fn encode(
        &mut self,
        device: &EncoderDevice,
        nv12: &ID3D11Texture2D,
        pts_100ns: i64,
    ) -> Result<(Vec<EncodedVideo>, bool), String> {
        let mut out = self.poll()?;
        let duration = 10_000_000 / i64::from(self.fps.max(1));
        let sample = match &mut self.drive {
            Drive::Async { credits, .. } => {
                if *credits == 0 {
                    return Ok((out, false));
                }
                *credits -= 1;
                // SAFETY: the texture outlives the sample's use (ring of 6).
                let buffer = unsafe {
                    MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)
                }
                .map_err(|e| err("MFCreateDXGISurfaceBuffer", e))?;
                let sample = unsafe { MFCreateSample() }.map_err(|e| err("MFCreateSample", e))?;
                unsafe { sample.AddBuffer(&buffer) }.map_err(|e| err("AddBuffer", e))?;
                sample
            }
            Drive::Sync { staging } => {
                let (width, height) = self.size;
                if staging.is_none() {
                    *staging = Some(texture(&device.device, width, height, DXGI_FORMAT_NV12, true)?);
                }
                let staging = staging.as_ref().ok_or("no staging")?;
                let frame_len = width * height * 3 / 2;
                let buffer = unsafe { MFCreateMemoryBuffer(frame_len) }.map_err(|e| err("MFCreateMemoryBuffer", e))?;
                // SAFETY: CopyResource between same-size NV12 textures; the
                // mapped rows are read within the map's lifetime and the
                // locked MF buffer holds `frame_len` bytes.
                unsafe {
                    device.context.CopyResource(staging, nv12);
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    device
                        .context
                        .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                        .map_err(|e| err("Map NV12", e))?;
                    let mut dst: *mut u8 = std::ptr::null_mut();
                    let lock = buffer.Lock(&mut dst, None, None);
                    if lock.is_ok() {
                        let src = mapped.pData as *const u8;
                        let pitch = mapped.RowPitch as usize;
                        let w = width as usize;
                        for row in 0..(height as usize * 3 / 2) {
                            std::ptr::copy_nonoverlapping(src.add(row * pitch), dst.add(row * w), w);
                        }
                        let _ = buffer.Unlock();
                    }
                    device.context.Unmap(staging, 0);
                    lock.map_err(|e| err("Lock", e))?;
                    buffer.SetCurrentLength(frame_len).map_err(|e| err("SetCurrentLength", e))?;
                }
                let sample = unsafe { MFCreateSample() }.map_err(|e| err("MFCreateSample", e))?;
                unsafe { sample.AddBuffer(&buffer) }.map_err(|e| err("AddBuffer", e))?;
                sample
            }
        };
        unsafe {
            sample.SetSampleTime(pts_100ns).map_err(|e| err("SetSampleTime", e))?;
            sample.SetSampleDuration(duration).map_err(|e| err("SetSampleDuration", e))?;
            self.transform.ProcessInput(0, &sample, 0).map_err(|e| err("ProcessInput", e))?;
        }
        out.extend(match self.drive {
            Drive::Sync { .. } => self.drain_sync()?,
            Drive::Async { .. } => self.poll()?,
        });
        Ok((out, true))
    }

    /// Collect output an async encoder produced since the last call.
    pub fn poll(&mut self) -> Result<Vec<EncodedVideo>, String> {
        let Drive::Async { events, credits, pending_output } = &mut self.drive else {
            return Ok(Vec::new());
        };
        loop {
            // SAFETY: non-blocking event fetch.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => match unsafe { event.GetType() }.unwrap_or(0) {
                    ME_TRANSFORM_NEED_INPUT => *credits += 1,
                    ME_TRANSFORM_HAVE_OUTPUT => *pending_output += 1,
                    _ => {}
                },
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(err("GetEvent", error)),
            }
        }
        let mut out = Vec::new();
        let pending = std::mem::take(pending_output);
        for _ in 0..pending {
            if let Some(frame) = self.process_output()? {
                out.push(frame);
            }
        }
        Ok(out)
    }

    fn drain_sync(&mut self) -> Result<Vec<EncodedVideo>, String> {
        let mut out = Vec::new();
        while let Some(frame) = self.process_output()? {
            out.push(frame);
        }
        Ok(out)
    }

    fn process_output(&mut self) -> Result<Option<EncodedVideo>, String> {
        let own_sample = if self.provides_samples {
            None
        } else {
            let sample = unsafe { MFCreateSample() }.map_err(|e| err("MFCreateSample", e))?;
            let buffer = unsafe { MFCreateMemoryBuffer(self.output_buffer_size) }
                .map_err(|e| err("MFCreateMemoryBuffer", e))?;
            unsafe { sample.AddBuffer(&buffer) }.map_err(|e| err("AddBuffer", e))?;
            Some(sample)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(own_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        // SAFETY: one output buffer; ownership of pSample/pEvents is taken
        // back below whatever the result.
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
        drop(unsafe { ManuallyDrop::take(&mut buffers[0].pEvents) });
        match result {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // Re-assert our output type and carry on.
                let output_type = video_type(MFVideoFormat_H264, self.size, self.fps)?;
                unsafe { self.transform.SetOutputType(0, &output_type, 0) }
                    .map_err(|e| err("SetOutputType after stream change", e))?;
                return Ok(None);
            }
            Err(error) => return Err(err("ProcessOutput", error)),
        }
        let Some(sample) = sample else { return Ok(None) };
        sample_to_video(&sample).map(Some)
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: orderly MFT teardown; errors are irrelevant at this point.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

fn video_type(subtype: GUID, size: (u32, u32), fps: u32) -> Result<IMFMediaType, String> {
    // SAFETY: attribute setters on a fresh media type.
    unsafe {
        let media_type = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| err("major", e))?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(|e| err("subtype", e))?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(size.0, size.1)).map_err(|e| err("size", e))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| err("rate", e))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| err("par", e))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| err("interlace", e))?;
        let _ = media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
        let _ = media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
        Ok(media_type)
    }
}

fn blob(media_type: &IMFMediaType, key: &GUID) -> Option<Vec<u8>> {
    // SAFETY: size query then a read into a buffer of exactly that size.
    unsafe {
        let size = media_type.GetBlobSize(key).ok()?;
        let mut buf = vec![0u8; size as usize];
        media_type.GetBlob(key, &mut buf, None).ok()?;
        Some(buf)
    }
}

fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, String> {
    // SAFETY: the contiguous buffer is locked for the duration of the copy.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().map_err(|e| err("ConvertToContiguousBuffer", e))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| err("Lock output", e))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = buffer.Unlock();
        Ok(data)
    }
}

fn sample_to_video(sample: &IMFSample) -> Result<EncodedVideo, String> {
    let data = sample_bytes(sample)?;
    // SAFETY: attribute getters.
    let pts_100ns = unsafe { sample.GetSampleTime() }.unwrap_or(0);
    let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
    Ok(EncodedVideo { data, pts_100ns, keyframe })
}

/// AAC-LC encoder: 1024-frame blocks of 48 kHz stereo i16 in, raw AAC out.
pub struct AacEncoder {
    transform: IMFTransform,
    activate: IMFActivate,
    output_buffer_size: u32,
}

impl AacEncoder {
    pub fn new() -> Result<Self, String> {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
        let activate = enum_transforms(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            input,
            output,
        )
        .into_iter()
        .next()
        .ok_or("no AAC encoder found")?;
        // SAFETY: COM calls on live objects; types are fully specified.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(|e| err("AAC activate", e))?;
            let rate = STREAM_AUDIO_SAMPLE_RATE;
            let output_type = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
            output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(|e| err("major", e))?;
            output_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).map_err(|e| err("subtype", e))?;
            output_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(|e| err("bits", e))?;
            output_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).map_err(|e| err("rate", e))?;
            output_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).map_err(|e| err("channels", e))?;
            output_type
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, STREAM_AUDIO_BITRATE_BPS / 8)
                .map_err(|e| err("bitrate", e))?;
            // Raw AAC (no ADTS): exactly what FLV carries.
            output_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(|e| err("payload", e))?;
            transform.SetOutputType(0, &output_type, 0).map_err(|e| err("AAC SetOutputType", e))?;
            let input_type = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(|e| err("major", e))?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).map_err(|e| err("subtype", e))?;
            input_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(|e| err("bits", e))?;
            input_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).map_err(|e| err("rate", e))?;
            input_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).map_err(|e| err("channels", e))?;
            input_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4).map_err(|e| err("align", e))?;
            input_type
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * 4)
                .map_err(|e| err("avg bytes", e))?;
            let _ = input_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1);
            transform.SetInputType(0, &input_type, 0).map_err(|e| err("AAC SetInputType", e))?;
            let info = transform.GetOutputStreamInfo(0).map_err(|e| err("AAC stream info", e))?;
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            Ok(Self { transform, activate, output_buffer_size: info.cbSize.max(8192) })
        }
    }

    /// Encode interleaved stereo PCM stamped `pts_100ns`; returns whatever
    /// complete AAC frames came out.
    pub fn encode(&mut self, pcm: &[i16], pts_100ns: i64) -> Result<Vec<EncodedAudio>, String> {
        let bytes = u32::try_from(pcm.len() * 2).map_err(|_| "audio block too large")?;
        let frames = pcm.len() as i64 / 2;
        // SAFETY: the locked buffer holds `bytes` bytes; the copy fits.
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes).map_err(|e| err("MFCreateMemoryBuffer", e))?;
            let mut dst: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None).map_err(|e| err("Lock", e))?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr().cast::<u8>(), dst, bytes as usize);
            let _ = buffer.Unlock();
            buffer.SetCurrentLength(bytes).map_err(|e| err("SetCurrentLength", e))?;
            let sample = MFCreateSample().map_err(|e| err("MFCreateSample", e))?;
            sample.AddBuffer(&buffer).map_err(|e| err("AddBuffer", e))?;
            sample.SetSampleTime(pts_100ns).map_err(|e| err("SetSampleTime", e))?;
            sample
                .SetSampleDuration(frames * 10_000_000 / i64::from(STREAM_AUDIO_SAMPLE_RATE))
                .map_err(|e| err("SetSampleDuration", e))?;
            self.transform.ProcessInput(0, &sample, 0).map_err(|e| err("AAC ProcessInput", e))?;
        }
        let mut out = Vec::new();
        loop {
            // SAFETY: as for the video encoder's ProcessOutput.
            let sample = unsafe { MFCreateSample() }.map_err(|e| err("MFCreateSample", e))?;
            let buffer = unsafe { MFCreateMemoryBuffer(self.output_buffer_size) }
                .map_err(|e| err("MFCreateMemoryBuffer", e))?;
            unsafe { sample.AddBuffer(&buffer) }.map_err(|e| err("AddBuffer", e))?;
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(Some(sample)),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
            let sample = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
            drop(unsafe { ManuallyDrop::take(&mut buffers[0].pEvents) });
            match result {
                Ok(()) => {}
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(error) => return Err(err("AAC ProcessOutput", error)),
            }
            if let Some(sample) = sample {
                let data = sample_bytes(&sample)?;
                let pts_100ns = unsafe { sample.GetSampleTime() }.unwrap_or(0);
                if !data.is_empty() {
                    out.push(EncodedAudio { data, pts_100ns });
                }
            }
        }
        Ok(out)
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        // SAFETY: orderly teardown.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.activate.ShutdownObject();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::flv::{split_annex_b, AvcPackager};

    /// Encodes synthetic frames on this machine's real encoders. Needs a
    /// D3D11 video device, so it is opt-in: `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a GPU and Media Foundation encoders"]
    fn encodes_h264_and_aac_on_this_machine() {
        let _runtime = MediaRuntime::start().unwrap();
        let device = EncoderDevice::create().unwrap();
        for allow_hardware in [true, false] {
            encode_video(&device, allow_hardware);
        }
        encode_audio();
    }

    fn encode_video(device: &EncoderDevice, allow_hardware: bool) {
        let canvas = (1280, 720);
        let mut converter = Converter::new(device, canvas).unwrap();
        let mut encoder =
            H264Encoder::new_with(device, canvas, 30, 4_000_000, 6_000_000, allow_hardware).unwrap();
        assert!(allow_hardware || !encoder.hardware);
        eprintln!("encoder: {} hardware={}", encoder.name, encoder.hardware);
        // A 4:3 source letterboxed into 16:9.
        let (src_w, src_h) = (800u32, 600u32);
        let mut out = Vec::new();
        for frame_index in 0..90i64 {
            let shade = (frame_index * 3 % 255) as u8;
            let bgra: Vec<u8> = (0..src_w * src_h).flat_map(|i| [shade, (i % 255) as u8, 255 - shade, 255]).collect();
            converter
                .upload(&BgraView { bgra: &bgra, bytes_per_row: src_w * 4, width: src_w, height: src_h })
                .unwrap();
            let dest = crate::streaming::plan::letterbox_rect(src_w, src_h, canvas.0, canvas.1);
            let nv12 = converter.convert(dest).unwrap();
            if frame_index == 45 {
                encoder.force_keyframe();
            }
            let (frames, _accepted) = encoder.encode(device, &nv12, frame_index * 333_333).unwrap();
            out.extend(frames);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for _ in 0..50 {
            out.extend(encoder.poll().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(out.len() > 30, "only {} frames came out", out.len());
        assert!(out[0].keyframe, "stream starts on a keyframe");
        let mut packager = AvcPackager::new();
        if let Some(header) = encoder.sequence_header() {
            let nalus = split_annex_b(header);
            packager.package(&nalus, 0, false);
        }
        let mut packets = Vec::new();
        for frame in &out {
            let nalus = split_annex_b(&frame.data);
            packets.extend(packager.package(&nalus, (frame.pts_100ns / 10_000) as u32, frame.keyframe));
        }
        assert!(packets[0].is_sequence_header(), "SPS/PPS found");
        assert!(packets.iter().filter(|p| p.is_video_keyframe()).count() >= 2, "forced/GOP keyframes");
        let pts: Vec<i64> = out.iter().map(|f| f.pts_100ns).collect();
        assert!(pts.windows(2).all(|w| w[0] < w[1]), "no B-frame reordering: {pts:?}");
    }

    fn encode_audio() {
        let mut aac = AacEncoder::new().unwrap();
        let mut audio = Vec::new();
        for block in 0..50i64 {
            let pcm: Vec<i16> = (0..2048).map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16).collect();
            audio.extend(aac.encode(&pcm, block * 1024 * 10_000_000 / 48_000).unwrap());
        }
        assert!(audio.len() >= 45, "{} AAC frames", audio.len());
        assert!(audio.iter().all(|f| f.data.len() < 2048 && f.data[0] != 0xff), "raw AAC, no ADTS");
    }
}
