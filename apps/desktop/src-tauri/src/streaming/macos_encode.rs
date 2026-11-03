//! macOS client-mode encoders, offering the same types as `windows_encode`:
//!
//! * [`Converter`]: a VideoToolbox pixel transfer session turns the share's
//!   frame (or the BGRA slate) into NV12 at the output size, scaled to fit and
//!   letterboxed in black, on the GPU.
//! * [`H264Encoder`]: a VideoToolbox compression session, hardware where the
//!   Mac has it. High profile, no B-frames, a keyframe every 2 s,
//!   peak-constrained VBR, real time.
//! * [`AacEncoder`]: an AudioToolbox converter, AAC-LC 48 kHz stereo 128 kbps.
//!
//! Share frames reach the tap as [`PixelBuffer`] copies made on the capture
//! thread, only while the share is the live one. ScreenCaptureKit hands out
//! its buffers from a three-deep pool, so the stream never holds on to one.
//!
//! Raw C FFI in the style of `native_display.rs`; no Objective-C.

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;

use super::flv::split_avcc;
use super::plan::{STREAM_AUDIO_BITRATE_BPS, STREAM_AUDIO_SAMPLE_RATE, STREAM_KEYFRAME_INTERVAL_S};
use crate::sync_ext::MutexExt;

type OSStatus = i32;
type CFRef = *const c_void;
type CVPixelBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// `kCMTimeFlags_Valid`.
const CM_TIME_VALID: u32 = 1;
/// Every timestamp here is in 100 ns units, like `windows_encode`.
const TIMESCALE_100NS: i32 = 10_000_000;

impl CMTime {
    /// `kCMTimeInvalid`.
    const INVALID: Self = Self { value: 0, timescale: 0, flags: 0, epoch: 0 };

    fn from_100ns(value: i64) -> Self {
        Self { value, timescale: TIMESCALE_100NS, flags: CM_TIME_VALID, epoch: 0 }
    }

    fn to_100ns(self) -> i64 {
        if self.flags & CM_TIME_VALID == 0 || self.timescale <= 0 {
            return 0;
        }
        let value = i128::from(self.value) * i128::from(TIMESCALE_100NS) / i128::from(self.timescale);
        i64::try_from(value).unwrap_or(0)
    }
}

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

#[repr(C)]
struct AudioStreamPacketDescription {
    start_offset: i64,
    variable_frames_in_packet: u32,
    data_byte_size: u32,
}

type CompressionOutputCallback =
    unsafe extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CMSampleBufferRef);
type AudioConverterInputProc = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *mut AudioBufferList,
    *mut *mut AudioStreamPacketDescription,
    *mut c_void,
) -> OSStatus;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFRef);
    fn CFArrayGetCount(array: CFRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFRef, index: isize) -> CFRef;
    fn CFDictionaryGetValue(dictionary: CFRef, key: CFRef) -> CFRef;
    fn CFBooleanGetValue(boolean: CFRef) -> bool;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferCreate(
        allocator: CFRef,
        width: usize,
        height: usize,
        pixel_format: u32,
        attributes: CFRef,
        buffer_out: *mut CVPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferRetain(buffer: CVPixelBufferRef) -> CVPixelBufferRef;
    fn CVPixelBufferRelease(buffer: CVPixelBufferRef);
    fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetWidth(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetPixelFormatType(buffer: CVPixelBufferRef) -> u32;
    fn CVPixelBufferGetPlaneCount(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddress(buffer: CVPixelBufferRef) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVPixelBufferRef, plane: usize) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferPoolCreate(
        allocator: CFRef,
        pool_attributes: CFRef,
        buffer_attributes: CFRef,
        pool_out: *mut *mut c_void,
    ) -> i32;
    fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: CFRef,
        pool: *mut c_void,
        buffer_out: *mut CVPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferPoolRelease(pool: *mut c_void);
    fn CVBufferPropagateAttachments(source: CVPixelBufferRef, destination: CVPixelBufferRef);

    static kCVPixelBufferPixelFormatTypeKey: CFRef;
    static kCVPixelBufferWidthKey: CFRef;
    static kCVPixelBufferHeightKey: CFRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFRef;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: CFRef;
    static kCVImageBufferTransferFunction_ITU_R_709_2: CFRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFRef;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMSampleBufferGetDataBuffer(sample: CMSampleBufferRef) -> *mut c_void;
    fn CMBlockBufferGetDataLength(block: *mut c_void) -> usize;
    fn CMBlockBufferCopyDataBytes(
        block: *mut c_void,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    fn CMSampleBufferGetFormatDescription(sample: CMSampleBufferRef) -> *mut c_void;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        description: *mut c_void,
        index: usize,
        parameter_set_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_length_out: *mut i32,
    ) -> OSStatus;
    fn CMSampleBufferGetSampleAttachmentsArray(sample: CMSampleBufferRef, create_if_necessary: u8) -> CFRef;
    fn CMSampleBufferGetPresentationTimeStamp(sample: CMSampleBufferRef) -> CMTime;

    static kCMSampleAttachmentKey_NotSync: CFRef;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTCompressionSessionCreate(
        allocator: CFRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFRef,
        source_image_buffer_attributes: CFRef,
        compressed_data_allocator: CFRef,
        output_callback: CompressionOutputCallback,
        output_callback_refcon: *mut c_void,
        session_out: *mut *mut c_void,
    ) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: *mut c_void) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: *mut c_void,
        image_buffer: CVPixelBufferRef,
        presentation_time: CMTime,
        duration: CMTime,
        frame_properties: CFRef,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: *mut c_void, complete_until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: *mut c_void);
    fn VTSessionSetProperty(session: *mut c_void, key: CFRef, value: CFRef) -> OSStatus;
    fn VTSessionCopyProperty(session: *mut c_void, key: CFRef, allocator: CFRef, value_out: *mut c_void) -> OSStatus;
    fn VTPixelTransferSessionCreate(allocator: CFRef, session_out: *mut *mut c_void) -> OSStatus;
    fn VTPixelTransferSessionTransferImage(
        session: *mut c_void,
        source: CVPixelBufferRef,
        destination: CVPixelBufferRef,
    ) -> OSStatus;
    fn VTPixelTransferSessionInvalidate(session: *mut c_void);

    static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFRef;
    static kVTCompressionPropertyKey_RealTime: CFRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFRef;
    static kVTProfileLevel_H264_High_AutoLevel: CFRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFRef;
    static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFRef;
    static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFRef;
    static kVTCompressionPropertyKey_ColorPrimaries: CFRef;
    static kVTCompressionPropertyKey_TransferFunction: CFRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFRef;
    static kVTPixelTransferPropertyKey_ScalingMode: CFRef;
    static kVTScalingMode_Letterbox: CFRef;
    static kVTPixelTransferPropertyKey_DestinationColorPrimaries: CFRef;
    static kVTPixelTransferPropertyKey_DestinationTransferFunction: CFRef;
    static kVTPixelTransferPropertyKey_DestinationYCbCrMatrix: CFRef;
}

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioConverterNew(
        source: *const AudioStreamBasicDescription,
        destination: *const AudioStreamBasicDescription,
        converter_out: *mut *mut c_void,
    ) -> OSStatus;
    fn AudioConverterDispose(converter: *mut c_void) -> OSStatus;
    fn AudioConverterSetProperty(converter: *mut c_void, property: u32, size: u32, data: *const c_void) -> OSStatus;
    fn AudioConverterGetProperty(converter: *mut c_void, property: u32, size: *mut u32, data: *mut c_void) -> OSStatus;
    fn AudioConverterFillComplexBuffer(
        converter: *mut c_void,
        input: AudioConverterInputProc,
        user_data: *mut c_void,
        packets: *mut u32,
        output: *mut AudioBufferList,
        packet_descriptions: *mut AudioStreamPacketDescription,
    ) -> OSStatus;
}

/// `kCVPixelFormatType_32BGRA`.
const FMT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`, what the encoder takes.
const FMT_NV12_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"420v");
/// `kCMVideoCodecType_H264`.
const CODEC_H264: u32 = u32::from_be_bytes(*b"avc1");
/// `kVTEncodeInfo_FrameDropped`.
const ENCODE_INFO_FRAME_DROPPED: u32 = 1 << 1;
/// `kCVPixelBufferLock_ReadOnly`.
const LOCK_READ_ONLY: u64 = 1;
/// `kAudioFormatLinearPCM`, `kAudioFormatMPEG4AAC`.
const AUDIO_FORMAT_LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const AUDIO_FORMAT_AAC: u32 = u32::from_be_bytes(*b"aac ");
/// `kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked`.
const PCM_SIGNED_PACKED: u32 = (1 << 2) | (1 << 3);
/// `kMPEG4Object_AAC_LC`.
const AAC_LC: u32 = 2;
/// `kAudioConverterEncodeBitRate`, `kAudioConverterPropertyMaximumOutputPacketSize`.
const CONVERTER_ENCODE_BIT_RATE: u32 = u32::from_be_bytes(*b"brat");
const CONVERTER_MAX_OUTPUT_PACKET_SIZE: u32 = u32::from_be_bytes(*b"xops");
/// Our input callback's "no more PCM for now"; the converter keeps its state.
const AAC_INPUT_DRY: OSStatus = i32::from_be_bytes(*b"dry!");
/// AAC frames per packet.
const AAC_FRAMES_PER_PACKET: i64 = 1024;
const ANNEX_B_START_CODE: [u8; 4] = [0, 0, 0, 1];

fn check(status: i32, what: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed ({status})"))
    }
}

/// A CoreFoundation constant (a framework `static`) as a `CFType`.
fn cf_static(value: CFRef) -> CFType {
    // SAFETY: framework constants are valid, immortal CF objects.
    unsafe { CFType::wrap_under_get_rule(value) }
}

fn cf_number(value: i64) -> CFType {
    CFNumber::from(value).as_CFType()
}

fn cf_bool(value: bool) -> CFType {
    let boolean = if value { CFBoolean::true_value() } else { CFBoolean::false_value() };
    boolean.as_CFType()
}

fn cf_dict(pairs: &[(CFRef, CFType)]) -> CFDictionary<CFType, CFType> {
    let pairs: Vec<(CFType, CFType)> = pairs.iter().map(|(key, value)| (cf_static(*key), value.clone())).collect();
    CFDictionary::from_CFType_pairs(&pairs)
}

/// The BT.709 tags for the stream's NV12 (what HD H.264 players assume).
fn bt709() -> [CFType; 3] {
    // SAFETY: reading framework constants.
    unsafe {
        [
            cf_static(kCVImageBufferColorPrimaries_ITU_R_709_2),
            cf_static(kCVImageBufferTransferFunction_ITU_R_709_2),
            cf_static(kCVImageBufferYCbCrMatrix_ITU_R_709_2),
        ]
    }
}

/// IOSurface-backed buffers, so VideoToolbox reads them on the GPU.
fn buffer_attributes(width: u32, height: u32, pixel_format: u32) -> CFDictionary<CFType, CFType> {
    let no_properties: CFDictionary<CFType, CFType> = CFDictionary::from_CFType_pairs(&[]);
    // SAFETY: reading framework constants.
    unsafe {
        cf_dict(&[
            (kCVPixelBufferPixelFormatTypeKey, cf_number(i64::from(pixel_format))),
            (kCVPixelBufferWidthKey, cf_number(i64::from(width))),
            (kCVPixelBufferHeightKey, cf_number(i64::from(height))),
            (kCVPixelBufferIOSurfacePropertiesKey, no_properties.as_CFType()),
        ])
    }
}

/// One retained `CVPixelBuffer`.
pub struct PixelBuffer(NonNull<c_void>);

// SAFETY: CoreVideo reference counting is thread-safe, and a `PixelBuffer`'s
// pixels are only written before it is shared.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

impl Clone for PixelBuffer {
    fn clone(&self) -> Self {
        // SAFETY: retaining a live buffer.
        unsafe { CVPixelBufferRetain(self.0.as_ptr()) };
        Self(self.0)
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        // SAFETY: releases the one reference this value owns.
        unsafe { CVPixelBufferRelease(self.0.as_ptr()) };
    }
}

impl std::fmt::Debug for PixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PixelBuffer").field(&self.0).finish()
    }
}

/// One plane of a locked buffer (or of a caller's slice).
struct Plane {
    base: *mut u8,
    bytes_per_row: usize,
    rows: usize,
}

/// Copies the rows two planes share, each up to the shorter stride.
///
/// # Safety
/// Both planes must be valid for `bytes_per_row * rows` bytes.
unsafe fn copy_plane(source: &Plane, destination: &Plane) {
    let row = source.bytes_per_row.min(destination.bytes_per_row);
    for index in 0..source.rows.min(destination.rows) {
        ptr::copy_nonoverlapping(
            source.base.add(index * source.bytes_per_row),
            destination.base.add(index * destination.bytes_per_row),
            row,
        );
    }
}

/// A base-address lock, released on drop.
struct Locked<'a> {
    buffer: &'a PixelBuffer,
    flags: u64,
}

impl<'a> Locked<'a> {
    fn new(buffer: &'a PixelBuffer, flags: u64) -> Result<Self, String> {
        // SAFETY: locking a live buffer; unlocked in `drop`.
        check(unsafe { CVPixelBufferLockBaseAddress(buffer.as_ptr(), flags) }, "CVPixelBufferLockBaseAddress")?;
        Ok(Self { buffer, flags })
    }

    fn planes(&self) -> Result<Vec<Plane>, String> {
        let buffer = self.buffer.as_ptr();
        // SAFETY: the buffer is locked, so its addresses are valid.
        let planes = unsafe {
            match CVPixelBufferGetPlaneCount(buffer) {
                0 => vec![Plane {
                    base: CVPixelBufferGetBaseAddress(buffer).cast(),
                    bytes_per_row: CVPixelBufferGetBytesPerRow(buffer),
                    rows: CVPixelBufferGetHeight(buffer),
                }],
                count => (0..count)
                    .map(|plane| Plane {
                        base: CVPixelBufferGetBaseAddressOfPlane(buffer, plane).cast(),
                        bytes_per_row: CVPixelBufferGetBytesPerRowOfPlane(buffer, plane),
                        rows: CVPixelBufferGetHeightOfPlane(buffer, plane),
                    })
                    .collect(),
            }
        };
        if planes.iter().any(|plane| plane.base.is_null()) {
            return Err("pixel buffer has no CPU address".into());
        }
        Ok(planes)
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: balances the lock taken in `new`.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer.as_ptr(), self.flags) };
    }
}

impl PixelBuffer {
    /// Takes over a +1 reference.
    fn from_owned(buffer: CVPixelBufferRef, what: &str) -> Result<Self, String> {
        NonNull::new(buffer).map(Self).ok_or_else(|| format!("{what} returned no buffer"))
    }

    fn create(width: u32, height: u32, pixel_format: u32) -> Result<Self, String> {
        let attributes = buffer_attributes(width, height, pixel_format);
        let mut buffer = ptr::null_mut();
        // SAFETY: valid attributes and out-param.
        let status = unsafe {
            CVPixelBufferCreate(
                ptr::null(),
                width as usize,
                height as usize,
                pixel_format,
                attributes.as_CFTypeRef(),
                &mut buffer,
            )
        };
        check(status, "CVPixelBufferCreate")?;
        Self::from_owned(buffer, "CVPixelBufferCreate")
    }

    fn as_ptr(&self) -> CVPixelBufferRef {
        self.0.as_ptr()
    }

    fn size(&self) -> (u32, u32) {
        // SAFETY: reading a live buffer's geometry.
        unsafe { (CVPixelBufferGetWidth(self.as_ptr()) as u32, CVPixelBufferGetHeight(self.as_ptr()) as u32) }
    }

    /// Copies `sources` (one per plane) into this freshly made buffer.
    fn fill_from(self, sources: &[Plane]) -> Result<Self, String> {
        {
            let locked = Locked::new(&self, 0)?;
            let planes = locked.planes()?;
            if planes.len() != sources.len() {
                return Err(format!("{} planes into {}", sources.len(), planes.len()));
            }
            for (source, destination) in sources.iter().zip(&planes) {
                // SAFETY: callers size `sources`; `planes` are locked.
                unsafe { copy_plane(source, destination) };
            }
        }
        Ok(self)
    }

    /// A copy of a BGRA image (the slate).
    pub fn from_bgra(bgra: &[u8], bytes_per_row: u32, width: u32, height: u32) -> Result<Self, String> {
        let source = slice_plane(bgra, bytes_per_row as usize, height as usize)?;
        Self::create(width, height, FMT_BGRA)?.fill_from(&[source])
    }

    /// A copy of NV12 planes (a share's snapshot fallback frame).
    pub fn from_nv12(y: &[u8], y_stride: u32, uv: &[u8], uv_stride: u32, width: u32, height: u32) -> Result<Self, String> {
        let y = slice_plane(y, y_stride as usize, height as usize)?;
        let uv = slice_plane(uv, uv_stride as usize, height.div_ceil(2) as usize)?;
        Self::create(width, height, FMT_NV12_VIDEO_RANGE)?.fill_from(&[y, uv])
    }

    /// A copy of a capture buffer, any format, keeping its color tags.
    ///
    /// # Safety
    /// `source` must be a live `CVPixelBuffer` for the duration of the call.
    pub unsafe fn copy_of(source: CVPixelBufferRef) -> Result<Self, String> {
        // SAFETY: the caller keeps `source` alive; this takes our own
        // reference for the copy's duration.
        let source = Self::from_owned(unsafe { CVPixelBufferRetain(source) }, "CVPixelBufferRetain")?;
        let (width, height) = source.size();
        // SAFETY: reading a live buffer's format.
        let pixel_format = unsafe { CVPixelBufferGetPixelFormatType(source.as_ptr()) };
        let copy = pooled_copy_buffer(width, height, pixel_format)?;
        let copy = {
            let locked = Locked::new(&source, LOCK_READ_ONLY)?;
            copy.fill_from(&locked.planes()?)?
        };
        // SAFETY: both buffers are live.
        unsafe { CVBufferPropagateAttachments(source.as_ptr(), copy.as_ptr()) };
        Ok(copy)
    }
}

fn slice_plane(data: &[u8], bytes_per_row: usize, rows: usize) -> Result<Plane, String> {
    if data.len() < bytes_per_row * rows {
        return Err(format!("{} bytes for {rows} rows of {bytes_per_row}", data.len()));
    }
    // Read-only: `copy_plane` only reads its source.
    Ok(Plane { base: data.as_ptr().cast_mut(), bytes_per_row, rows })
}

/// A `CVPixelBufferPool` of one size and format.
struct BufferPool {
    pool: NonNull<c_void>,
    key: (u32, u32, u32),
}

// SAFETY: CoreVideo pools are thread-safe.
unsafe impl Send for BufferPool {}

impl BufferPool {
    fn new(width: u32, height: u32, pixel_format: u32) -> Result<Self, String> {
        let attributes = buffer_attributes(width, height, pixel_format);
        let mut pool = ptr::null_mut();
        // SAFETY: valid attributes and out-param.
        let status = unsafe { CVPixelBufferPoolCreate(ptr::null(), ptr::null(), attributes.as_CFTypeRef(), &mut pool) };
        check(status, "CVPixelBufferPoolCreate")?;
        let pool = NonNull::new(pool).ok_or("CVPixelBufferPoolCreate returned no pool")?;
        Ok(Self { pool, key: (width, height, pixel_format) })
    }

    fn buffer(&self) -> Result<PixelBuffer, String> {
        let mut buffer = ptr::null_mut();
        // SAFETY: a live pool and a valid out-param.
        let status = unsafe { CVPixelBufferPoolCreatePixelBuffer(ptr::null(), self.pool.as_ptr(), &mut buffer) };
        check(status, "CVPixelBufferPoolCreatePixelBuffer")?;
        PixelBuffer::from_owned(buffer, "CVPixelBufferPoolCreatePixelBuffer")
    }
}

impl Drop for BufferPool {
    fn drop(&mut self) {
        // SAFETY: releases the pool this value created.
        unsafe { CVPixelBufferPoolRelease(self.pool.as_ptr()) };
    }
}

/// Tap copies come from one pool, remade when the share's size changes.
fn pooled_copy_buffer(width: u32, height: u32, pixel_format: u32) -> Result<PixelBuffer, String> {
    static COPY_POOL: Mutex<Option<BufferPool>> = Mutex::new(None);
    let key = (width, height, pixel_format);
    let mut pool = COPY_POOL.lock_unpoisoned();
    if !matches!(&*pool, Some(current) if current.key == key) {
        *pool = Some(BufferPool::new(width, height, pixel_format)?);
    }
    pool.as_ref().ok_or("no copy pool")?.buffer()
}

/// Paints a whole NV12 buffer video-range black (the letterbox bars).
fn fill_black(buffer: &PixelBuffer) -> Result<(), String> {
    let locked = Locked::new(buffer, 0)?;
    for (index, plane) in locked.planes()?.iter().enumerate() {
        let value = if index == 0 { 16 } else { 128 };
        // SAFETY: the plane is locked and `bytes_per_row * rows` long.
        unsafe { ptr::write_bytes(plane.base, value, plane.bytes_per_row * plane.rows) };
    }
    Ok(())
}

/// Nothing to start on macOS; kept so the pipeline reads the same on both.
pub struct MediaRuntime;

impl MediaRuntime {
    pub fn start() -> Result<Self, String> {
        Ok(Self)
    }
}

/// VideoToolbox picks its own device; kept so the pipeline reads the same on
/// both platforms.
pub struct EncoderDevice;

impl EncoderDevice {
    pub fn create() -> Result<Self, String> {
        Ok(Self)
    }
}

/// A BGRA frame in CPU memory.
pub struct BgraView<'a> {
    pub bgra: &'a [u8],
    pub bytes_per_row: u32,
    pub width: u32,
    pub height: u32,
}

/// Scales and letterboxes the current source into canvas-sized NV12.
pub struct Converter {
    session: NonNull<c_void>,
    pool: BufferPool,
    size: (u32, u32),
    source: Option<PixelBuffer>,
    /// The last output and the rectangle it was drawn at, reused while
    /// nothing changes: the encoder only reads it.
    output: Option<(PixelBuffer, (u32, u32, u32, u32))>,
}

impl Converter {
    pub fn new(_device: &EncoderDevice, output_size: (u32, u32)) -> Result<Self, String> {
        let pool = BufferPool::new(output_size.0, output_size.1, FMT_NV12_VIDEO_RANGE)?;
        let mut session = ptr::null_mut();
        // SAFETY: a valid out-param.
        check(unsafe { VTPixelTransferSessionCreate(ptr::null(), &mut session) }, "VTPixelTransferSessionCreate")?;
        let session = NonNull::new(session).ok_or("VTPixelTransferSessionCreate returned no session")?;
        let converter = Self { session, pool, size: output_size, source: None, output: None };
        let [primaries, transfer, matrix] = bt709();
        // SAFETY: reading framework constants.
        let properties = unsafe {
            [
                (kVTPixelTransferPropertyKey_ScalingMode, cf_static(kVTScalingMode_Letterbox)),
                (kVTPixelTransferPropertyKey_DestinationColorPrimaries, primaries),
                (kVTPixelTransferPropertyKey_DestinationTransferFunction, transfer),
                (kVTPixelTransferPropertyKey_DestinationYCbCrMatrix, matrix),
            ]
        };
        for (key, value) in properties {
            // SAFETY: a live session and valid CF objects.
            let status = unsafe { VTSessionSetProperty(session.as_ptr(), key, value.as_CFTypeRef()) };
            check(status, "pixel transfer setup")?;
        }
        Ok(converter)
    }

    /// Upload a CPU BGRA frame (the slate).
    pub fn upload(&mut self, frame: &BgraView<'_>) -> Result<(), String> {
        let buffer = PixelBuffer::from_bgra(frame.bgra, frame.bytes_per_row, frame.width, frame.height)?;
        self.upload_buffer(&buffer)
    }

    /// Use a share frame's buffer as the source.
    pub fn upload_buffer(&mut self, buffer: &PixelBuffer) -> Result<(), String> {
        self.source = Some(buffer.clone());
        self.output = None;
        Ok(())
    }

    /// Draw the source into a fresh canvas buffer. VideoToolbox letterboxes
    /// by itself; `dest` (from `plan::letterbox_rect`, the same centered fit)
    /// tells whether there are bars to paint black first.
    pub fn convert(&mut self, dest: (u32, u32, u32, u32)) -> Result<PixelBuffer, String> {
        if let Some((output, at)) = &self.output {
            if *at == dest {
                return Ok(output.clone());
            }
        }
        let output = self.pool.buffer()?;
        let drawable = self.source.as_ref().filter(|_| dest.2 > 0 && dest.3 > 0);
        if drawable.is_none() || dest != (0, 0, self.size.0, self.size.1) {
            fill_black(&output)?;
        }
        if let Some(source) = drawable {
            // SAFETY: a live session and two live buffers.
            let status = unsafe {
                VTPixelTransferSessionTransferImage(self.session.as_ptr(), source.as_ptr(), output.as_ptr())
            };
            check(status, "VTPixelTransferSessionTransferImage")?;
        }
        self.output = Some((output.clone(), dest));
        Ok(output)
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: tears down the session this value created.
        unsafe {
            VTPixelTransferSessionInvalidate(self.session.as_ptr());
            CFRelease(self.session.as_ptr());
        }
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

/// What the compression session's callback hands back to the encoder thread.
#[derive(Default)]
struct EncoderOutput {
    frames: VecDeque<EncodedVideo>,
    error: Option<String>,
}

pub struct H264Encoder {
    session: NonNull<c_void>,
    /// The callback's refcon: outlives the session (dropped after `drop`
    /// invalidates it).
    output: Box<Mutex<EncoderOutput>>,
    size: (u32, u32),
    fps: u32,
    keyframe_requested: Cell<bool>,
    pub name: String,
    pub hardware: bool,
}

impl H264Encoder {
    pub fn new(
        _device: &EncoderDevice,
        size: (u32, u32),
        fps: u32,
        bitrate_bps: u32,
        max_bitrate_bps: u32,
    ) -> Result<Self, String> {
        let fps = fps.max(1);
        let output = Box::new(Mutex::new(EncoderOutput::default()));
        // SAFETY: reading a framework constant.
        let specification = cf_dict(&[(
            unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder },
            cf_bool(true),
        )]);
        let width = i32::try_from(size.0).map_err(|_| "canvas too wide")?;
        let height = i32::try_from(size.1).map_err(|_| "canvas too tall")?;
        let refcon = ptr::from_ref::<Mutex<EncoderOutput>>(&*output).cast_mut().cast::<c_void>();
        let mut session = ptr::null_mut();
        // SAFETY: valid arguments; `refcon` stays alive until the session is
        // invalidated in `drop`.
        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                width,
                height,
                CODEC_H264,
                specification.as_CFTypeRef(),
                ptr::null(),
                ptr::null(),
                compression_output,
                refcon,
                &mut session,
            )
        };
        check(status, "VTCompressionSessionCreate")?;
        let session = NonNull::new(session).ok_or("VTCompressionSessionCreate returned no session")?;
        let mut encoder = Self {
            session,
            output,
            size,
            fps,
            keyframe_requested: Cell::new(false),
            name: String::new(),
            hardware: false,
        };

        let max_bitrate_bps = max_bitrate_bps.max(bitrate_bps);
        // One second at the peak rate, like the Windows encoder's VBV.
        let data_rate_limits =
            CFArray::from_CFTypes(&[cf_number(i64::from(max_bitrate_bps / 8)), CFNumber::from(1.0f64).as_CFType()]);
        let [primaries, transfer, matrix] = bt709();
        // SAFETY: reading framework constants.
        let (required, preferred) = unsafe {
            (
                [
                    (kVTCompressionPropertyKey_RealTime, cf_bool(true)),
                    // FLV here carries no composition offsets: no B-frames.
                    (kVTCompressionPropertyKey_AllowFrameReordering, cf_bool(false)),
                    (kVTCompressionPropertyKey_ProfileLevel, cf_static(kVTProfileLevel_H264_High_AutoLevel)),
                    (kVTCompressionPropertyKey_AverageBitRate, cf_number(i64::from(bitrate_bps))),
                    (
                        kVTCompressionPropertyKey_MaxKeyFrameInterval,
                        cf_number(i64::from(fps * STREAM_KEYFRAME_INTERVAL_S)),
                    ),
                ],
                [
                    (
                        kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                        CFNumber::from(f64::from(STREAM_KEYFRAME_INTERVAL_S)).as_CFType(),
                    ),
                    (kVTCompressionPropertyKey_DataRateLimits, data_rate_limits.as_CFType()),
                    (kVTCompressionPropertyKey_ExpectedFrameRate, cf_number(i64::from(fps))),
                    (kVTCompressionPropertyKey_ColorPrimaries, primaries),
                    (kVTCompressionPropertyKey_TransferFunction, transfer),
                    (kVTCompressionPropertyKey_YCbCrMatrix, matrix),
                ],
            )
        };
        for (key, value) in required {
            check(encoder.set_property(key, &value), "H.264 encoder setup")?;
        }
        for (key, value) in preferred {
            let status = encoder.set_property(key, &value);
            if status != 0 {
                log::warn!("streaming: VideoToolbox ignored an H.264 setting ({status})");
            }
        }
        encoder.hardware = encoder.using_hardware();
        encoder.name = format!("VideoToolbox H.264 ({})", if encoder.hardware { "hardware" } else { "software" });
        // SAFETY: a live session.
        check(unsafe { VTCompressionSessionPrepareToEncodeFrames(session.as_ptr()) }, "PrepareToEncodeFrames")?;
        log::info!(
            "streaming: H.264 encoder \"{}\" {}x{} @ {fps} fps, {bitrate_bps} bps",
            encoder.name,
            size.0,
            size.1
        );
        Ok(encoder)
    }

    fn set_property(&self, key: CFRef, value: &CFType) -> OSStatus {
        // SAFETY: a live session and valid CF objects.
        unsafe { VTSessionSetProperty(self.session.as_ptr(), key, value.as_CFTypeRef()) }
    }

    fn using_hardware(&self) -> bool {
        let mut value: CFRef = ptr::null();
        // SAFETY: a live session; a returned value is +1 and released here.
        unsafe {
            let status = VTSessionCopyProperty(
                self.session.as_ptr(),
                kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                ptr::null(),
                ptr::from_mut(&mut value).cast(),
            );
            if status != 0 || value.is_null() {
                return false;
            }
            let hardware = CFBooleanGetValue(value);
            CFRelease(value);
            hardware
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// VideoToolbox gives SPS/PPS only with the first frame; they arrive
    /// in-band before every keyframe, and the packager lifts them out.
    pub fn sequence_header(&self) -> Option<&[u8]> {
        None
    }

    pub fn force_keyframe(&self) {
        self.keyframe_requested.set(true);
    }

    /// Submit one NV12 frame. Returns any output ready now and whether the
    /// frame was accepted.
    pub fn encode(
        &mut self,
        _device: &EncoderDevice,
        nv12: &PixelBuffer,
        pts_100ns: i64,
    ) -> Result<(Vec<EncodedVideo>, bool), String> {
        let forced = self.keyframe_requested.take();
        // SAFETY: reading a framework constant.
        let properties = forced.then(|| cf_dict(&[(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }, cf_bool(true))]));
        let mut flags = 0u32;
        // SAFETY: a live session and buffer; VideoToolbox retains the buffer.
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session.as_ptr(),
                nv12.as_ptr(),
                CMTime::from_100ns(pts_100ns),
                CMTime::from_100ns(i64::from(TIMESCALE_100NS) / i64::from(self.fps)),
                properties.as_ref().map_or(ptr::null(), |properties| properties.as_CFTypeRef()),
                ptr::null_mut(),
                &mut flags,
            )
        };
        check(status, "VTCompressionSessionEncodeFrame")?;
        let accepted = flags & ENCODE_INFO_FRAME_DROPPED == 0;
        if forced && !accepted {
            self.keyframe_requested.set(true);
        }
        Ok((self.poll()?, accepted))
    }

    /// Whatever the session has finished since the last call.
    pub fn poll(&mut self) -> Result<Vec<EncodedVideo>, String> {
        let mut output = self.output.lock_unpoisoned();
        if let Some(error) = output.error.take() {
            return Err(error);
        }
        Ok(output.frames.drain(..).collect())
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: flushes, then tears down the session this value created;
        // no callback runs after `Invalidate`, so `output` may drop after.
        unsafe {
            VTCompressionSessionCompleteFrames(self.session.as_ptr(), CMTime::INVALID);
            VTCompressionSessionInvalidate(self.session.as_ptr());
            CFRelease(self.session.as_ptr());
        }
    }
}

/// The compression session's output callback, on a VideoToolbox thread.
unsafe extern "C" fn compression_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: OSStatus,
    _flags: u32,
    sample: CMSampleBufferRef,
) {
    // SAFETY: `refcon` is the encoder's boxed output, alive until the
    // session is invalidated.
    let output = unsafe { &*refcon.cast::<Mutex<EncoderOutput>>() };
    let result = if status != 0 {
        Err(format!("VideoToolbox couldn't encode a frame ({status})"))
    } else if sample.is_null() {
        // Dropped by the encoder: nothing to send.
        return;
    } else {
        // SAFETY: a live sample buffer for the callback's duration.
        unsafe { encoded_frame(sample) }
    };
    let mut output = output.lock_unpoisoned();
    match result {
        Ok(Some(frame)) => output.frames.push_back(frame),
        Ok(None) => {}
        Err(error) => {
            output.error.get_or_insert(error);
        }
    }
}

/// One encoded sample as Annex B, SPS/PPS in front of keyframes.
///
/// # Safety
/// `sample` must be a live `CMSampleBuffer` from the compression session.
unsafe fn encoded_frame(sample: CMSampleBufferRef) -> Result<Option<EncodedVideo>, String> {
    let block = CMSampleBufferGetDataBuffer(sample);
    if block.is_null() {
        return Ok(None);
    }
    let length = CMBlockBufferGetDataLength(block);
    let mut avcc = vec![0u8; length];
    check(CMBlockBufferCopyDataBytes(block, 0, length, avcc.as_mut_ptr().cast()), "CMBlockBufferCopyDataBytes")?;
    let keyframe = is_sync_sample(sample);
    let description = CMSampleBufferGetFormatDescription(sample);
    if description.is_null() {
        return Err("encoded frame has no format description".into());
    }
    let mut count = 0usize;
    let mut length_size = 0i32;
    check(
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            &mut length_size,
        ),
        "H.264 parameter sets",
    )?;
    let mut data = Vec::with_capacity(length + 128);
    if keyframe {
        for index in 0..count {
            let mut set: *const u8 = ptr::null();
            let mut size = 0usize;
            check(
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    description,
                    index,
                    &mut set,
                    &mut size,
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                "H.264 parameter set",
            )?;
            if !set.is_null() {
                data.extend_from_slice(&ANNEX_B_START_CODE);
                data.extend_from_slice(std::slice::from_raw_parts(set, size));
            }
        }
    }
    let length_size = usize::try_from(length_size).ok().filter(|size| (1..=4).contains(size)).unwrap_or(4);
    for nalu in split_avcc(&avcc, length_size) {
        data.extend_from_slice(&ANNEX_B_START_CODE);
        data.extend_from_slice(nalu);
    }
    let pts_100ns = CMSampleBufferGetPresentationTimeStamp(sample).to_100ns();
    Ok(Some(EncodedVideo { data, pts_100ns, keyframe }))
}

/// A sample is a keyframe unless it carries `NotSync = true`.
///
/// # Safety
/// `sample` must be a live `CMSampleBuffer`.
unsafe fn is_sync_sample(sample: CMSampleBufferRef) -> bool {
    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
    if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
        return true;
    }
    let first = CFArrayGetValueAtIndex(attachments, 0);
    if first.is_null() {
        return true;
    }
    let not_sync = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync);
    not_sync.is_null() || !CFBooleanGetValue(not_sync)
}

/// PCM waiting for the converter. `in_flight` is the slice handed out last:
/// the converter may read it until it asks for more.
#[derive(Default)]
struct AacInput {
    pending: Vec<i16>,
    in_flight: Vec<i16>,
}

/// AAC-LC encoder: 48 kHz stereo i16 in, raw AAC out.
pub struct AacEncoder {
    converter: NonNull<c_void>,
    input: Box<AacInput>,
    max_packet_size: usize,
    packets_out: i64,
    first_pts_100ns: Option<i64>,
}

impl AacEncoder {
    pub fn new() -> Result<Self, String> {
        let rate = f64::from(STREAM_AUDIO_SAMPLE_RATE);
        let pcm = AudioStreamBasicDescription {
            sample_rate: rate,
            format_id: AUDIO_FORMAT_LPCM,
            format_flags: PCM_SIGNED_PACKED,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels_per_frame: 2,
            bits_per_channel: 16,
            reserved: 0,
        };
        let aac = AudioStreamBasicDescription {
            sample_rate: rate,
            format_id: AUDIO_FORMAT_AAC,
            format_flags: AAC_LC,
            bytes_per_packet: 0,
            frames_per_packet: AAC_FRAMES_PER_PACKET as u32,
            bytes_per_frame: 0,
            channels_per_frame: 2,
            bits_per_channel: 0,
            reserved: 0,
        };
        let mut converter = ptr::null_mut();
        // SAFETY: valid formats and out-param.
        check(unsafe { AudioConverterNew(&pcm, &aac, &mut converter) }, "AudioConverterNew")?;
        let converter = NonNull::new(converter).ok_or("AudioConverterNew returned no converter")?;
        let mut encoder = Self {
            converter,
            input: Box::default(),
            max_packet_size: 0,
            packets_out: 0,
            first_pts_100ns: None,
        };
        let bitrate = STREAM_AUDIO_BITRATE_BPS;
        let mut max_packet_size = 0u32;
        let mut size = 4u32;
        // SAFETY: a live converter; both values are u32.
        unsafe {
            check(
                AudioConverterSetProperty(
                    converter.as_ptr(),
                    CONVERTER_ENCODE_BIT_RATE,
                    4,
                    ptr::from_ref(&bitrate).cast(),
                ),
                "AAC bitrate",
            )?;
            check(
                AudioConverterGetProperty(
                    converter.as_ptr(),
                    CONVERTER_MAX_OUTPUT_PACKET_SIZE,
                    &mut size,
                    ptr::from_mut(&mut max_packet_size).cast(),
                ),
                "AAC packet size",
            )?;
        }
        encoder.max_packet_size = (max_packet_size as usize).max(2048);
        Ok(encoder)
    }

    /// Encode interleaved stereo PCM stamped `pts_100ns`; returns whatever
    /// complete AAC frames came out. Blocks arrive back to back, so each
    /// packet is stamped from the first block's time plus its index.
    pub fn encode(&mut self, pcm: &[i16], pts_100ns: i64) -> Result<Vec<EncodedAudio>, String> {
        let first_pts = *self.first_pts_100ns.get_or_insert(pts_100ns);
        self.input.pending.extend_from_slice(pcm);
        let mut out = Vec::new();
        loop {
            let mut packet = vec![0u8; self.max_packet_size];
            let mut list = AudioBufferList {
                number_buffers: 1,
                buffers: [AudioBuffer {
                    number_channels: 2,
                    data_byte_size: u32::try_from(packet.len()).unwrap_or(u32::MAX),
                    data: packet.as_mut_ptr().cast(),
                }],
            };
            let mut description = AudioStreamPacketDescription {
                start_offset: 0,
                variable_frames_in_packet: 0,
                data_byte_size: 0,
            };
            let mut packets = 1u32;
            let input = ptr::from_mut::<AacInput>(&mut *self.input).cast::<c_void>();
            // SAFETY: a live converter; `list` points into `packet`, and
            // `input` outlives the call.
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter.as_ptr(),
                    aac_input,
                    input,
                    &mut packets,
                    &mut list,
                    &mut description,
                )
            };
            if status != 0 && status != AAC_INPUT_DRY {
                return Err(format!("AAC encode failed ({status})"));
            }
            let size = list.buffers[0].data_byte_size as usize;
            if packets == 0 || size == 0 {
                break;
            }
            packet.truncate(size);
            let pts = first_pts
                + self.packets_out * AAC_FRAMES_PER_PACKET * i64::from(TIMESCALE_100NS)
                    / i64::from(STREAM_AUDIO_SAMPLE_RATE);
            self.packets_out += 1;
            out.push(EncodedAudio { data: packet, pts_100ns: pts });
            if status == AAC_INPUT_DRY {
                break;
            }
        }
        Ok(out)
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        // SAFETY: disposes the converter this value created.
        unsafe { AudioConverterDispose(self.converter.as_ptr()) };
    }
}

/// The converter asks for PCM: hand it what's pending, or say "dry".
unsafe extern "C" fn aac_input(
    _converter: *mut c_void,
    packets: *mut u32,
    data: *mut AudioBufferList,
    descriptions: *mut *mut AudioStreamPacketDescription,
    user_data: *mut c_void,
) -> OSStatus {
    // SAFETY: `user_data` is the encoder's boxed input, borrowed for the
    // `AudioConverterFillComplexBuffer` call that invokes this.
    let input = unsafe { &mut *user_data.cast::<AacInput>() };
    let buffer = &mut (*data).buffers[0];
    if !descriptions.is_null() {
        *descriptions = ptr::null_mut();
    }
    let frames = (input.pending.len() / 2).min(*packets as usize);
    if frames == 0 {
        *packets = 0;
        buffer.data = ptr::null_mut();
        buffer.data_byte_size = 0;
        return AAC_INPUT_DRY;
    }
    input.in_flight.clear();
    input.in_flight.extend(input.pending.drain(..frames * 2));
    buffer.number_channels = 2;
    buffer.data = input.in_flight.as_mut_ptr().cast();
    buffer.data_byte_size = (frames * 4) as u32;
    *packets = frames as u32;
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::flv::{split_annex_b, AvcPackager};

    /// Encodes synthetic frames on this Mac's real encoders. Opt-in like
    /// the Windows one: `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs VideoToolbox and AudioToolbox"]
    fn encodes_h264_and_aac_on_this_machine() {
        let _runtime = MediaRuntime::start().unwrap();
        let device = EncoderDevice::create().unwrap();
        encode_video(&device);
        encode_audio();
    }

    fn encode_video(device: &EncoderDevice) {
        let canvas = (1280, 720);
        let mut converter = Converter::new(device, canvas).unwrap();
        let mut encoder = H264Encoder::new(device, canvas, 30, 4_000_000, 6_000_000).unwrap();
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
        // 1024 frames is 213_333.3 x 100 ns, so steps alternate 213_333/213_334
        // and never drift.
        let pts: Vec<i64> = audio.iter().map(|f| f.pts_100ns).collect();
        let expected: Vec<i64> = (0..pts.len() as i64).map(|i| i * 1024 * 10_000_000 / 48_000).collect();
        assert_eq!(pts, expected, "1024 frames apart");
    }

    #[test]
    #[ignore = "needs CoreVideo"]
    fn copies_a_buffer_with_its_planes() {
        let y = vec![200u8; 64 * 32];
        let uv = vec![90u8; 64 * 16];
        let source = PixelBuffer::from_nv12(&y, 64, &uv, 64, 64, 32).unwrap();
        // SAFETY: `source` is live for the call.
        let copy = unsafe { PixelBuffer::copy_of(source.as_ptr()) }.unwrap();
        assert_eq!(copy.size(), (64, 32));
        let locked = Locked::new(&copy, LOCK_READ_ONLY).unwrap();
        let planes = locked.planes().unwrap();
        assert_eq!(planes.len(), 2);
        // SAFETY: locked planes, first byte of each.
        unsafe {
            assert_eq!(*planes[0].base, 200);
            assert_eq!(*planes[1].base, 90);
        }
    }
}
