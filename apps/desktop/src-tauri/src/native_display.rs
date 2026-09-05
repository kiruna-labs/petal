//! Zero-copy decode-to-display: wrap a decoder-owned `CVPixelBufferRef`
//! (IOSurface-backed) in a `CMSampleBuffer` and enqueue it directly on an
//! `AVSampleBufferDisplayLayer`, with NO CPU copy anywhere in this path.
//!
//! SPEC.md §4.4: "Decode path: VideoToolbox HW decode -> `CVPixelBuffer`
//! (IOSurface) -> `CAMetalLayer`/`AVSampleBufferDisplayLayer`. Avoid CPU
//! copies -- the whole point of the IOSurface pipeline is decode-to-display
//! without round-tripping through main memory."
//!
//! ## Where the `CVPixelBufferRef` actually comes from
//!
//! `transport::subscriber` (see that module) receives frames off a
//! `NativeVideoStream`. Confirmed by reading the `livekit`/`libwebrtc`
//! 0.3.38 source directly (not assumed): on macOS, libwebrtc's default video
//! decoder factory is `RTCDefaultVideoDecoderFactory` (an Objective-C
//! wrapper around VideoToolbox HW decode -- see
//! `webrtc-sys-0.3.36/src/objc_video_factory.mm`), and its decoded frames
//! come back as `RTCCVPixelBuffer`-backed buffers, which
//! `webrtc-sys`'s `VideoFrameBuffer::buffer_type()` reports as
//! `VideoFrameBufferType::Native` (see
//! `webrtc-sys-0.3.36/src/objc_video_frame_buffer.mm`'s
//! `native_buffer_to_platform_image_buffer`, which unwraps the ObjC
//! `RTCCVPixelBuffer` back to its raw `CVPixelBufferRef` with NO copy). The
//! `libwebrtc` Rust crate exposes exactly this as safe-ish public API:
//! `NativeBuffer::get_cv_pixel_buffer(&self) -> *mut c_void` (macOS/iOS only,
//! `libwebrtc-0.3.38/src/video_frame.rs`). So the chain
//! `VideoToolbox decode -> RTCCVPixelBuffer -> CVPixelBufferRef -> here` is
//! real, verified against the actual dependency source in this workspace's
//! own `~/.cargo/registry`, not assumed from documentation.
//!
//! ## Why `AVSampleBufferDisplayLayer` over hand-rolled `CAMetalLayer`
//!
//! Both are zero-copy-capable (a `CVPixelBuffer`'s backing `IOSurface` can
//! feed either), but `AVSampleBufferDisplayLayer` is a complete, Apple-
//! maintained decode-to-display sink: handed a `CMSampleBuffer` wrapping the
//! pixel buffer, it does its own internal Metal-backed compositing,
//! colorspace handling, and display-timed presentation -- no render loop,
//! shader, or `MTLCommandQueue` to hand-write. A `CAMetalLayer` path would
//! require a `CVMetalTextureCache` + a per-frame draw call on a dedicated
//! render thread, which is real work this task doesn't need to hand-roll
//! when AVFoundation already does exactly this job over the same IOSurface
//! substrate. This is a straight zero-CPU-copy path either way; the choice
//! here is "which zero-copy sink," not "copy vs. no-copy."
//!
//! ## Why raw `extern "C"` FFI instead of an `objc2-av-foundation` /
//! `objc2-core-media` dependency
//!
//! `Cargo.toml`'s own `screencapturekit`/`livekit` doc comments record a real
//! prior linker fight in this exact codebase (`-ObjC` whole-archive-loading
//! forcing duplicate Swift/ObjC type-metadata symbols to collide -- see
//! `transport/mod.rs`'s "KNOWN BLOCKER" writeup and
//! `vendor/screencapturekit/PETAL_PATCH.md`). Every new crate that ships its
//! own Objective-C class metadata is a new surface for that fight to
//! recur. `AVSampleBufferDisplayLayer` and the handful of `CMSampleBuffer`/
//! `CMVideoFormatDescription` C functions used here have stable, versioned C
// ABIs callable via plain `extern "C"` linkage against the `AVFoundation`/
//! `CoreMedia`/`QuartzCore` frameworks (same pattern `hover_tab.rs` already
//! uses for raw CoreGraphics calls, and `resilience.rs` for
//! `SystemConfiguration`) -- no new Objective-C class metadata is linked in
//! by this module at all (the ONE Objective-C message send needed --
//! allocating an `AVSampleBufferDisplayLayer` instance and calling
//! `enqueueSampleBuffer:` on it -- goes through `objc2`'s existing
//! `msg_send!`/`AnyObject`, already a direct dependency with no new class
//! metadata of its own).

#![cfg(target_os = "macos")]

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use crate::video_color::{
    ColorPrimaries, MatrixCoefficients, PixelRange, TransferFunction, VideoColorProfile,
};

const PIXEL_MATCH_EPSILON: f64 = 0.5;

// =============================================================================
// Raw CoreVideo / CoreMedia / QuartzCore FFI (C ABI, no ObjC class metadata)
// =============================================================================

type CVPixelBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMVideoFormatDescriptionRef = *mut c_void;
type CMTimeValue = i64;
type CMTimeScale = i32;
type CMTimeFlags = u32;
type CMTimeEpoch = i64;
type OSStatus = i32;

/// Mirrors `CMTime` (CoreMedia.framework) -- a POD struct passed by value
/// across this FFI boundary, per the real C ABI signature of
/// `CMSampleBufferCreateForImageBuffer`/`CMTimingInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: CMTimeValue,
    timescale: CMTimeScale,
    flags: CMTimeFlags,
    epoch: CMTimeEpoch,
}

const K_CM_TIME_FLAGS_VALID: CMTimeFlags = 1;

fn cm_time_invalid() -> CMTime {
    CMTime {
        value: 0,
        timescale: 0,
        flags: 0,
        epoch: 0,
    }
}

fn cm_time(value: i64, timescale: i32) -> CMTime {
    CMTime {
        value,
        timescale,
        flags: K_CM_TIME_FLAGS_VALID,
        epoch: 0,
    }
}

/// Mirrors `CMSampleTimingInfo` (CoreMedia.framework).
#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_time_stamp: CMTime,
    decode_time_stamp: CMTime,
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferCreate(
        allocator: *const c_void,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        pixel_buffer_attributes: *const c_void,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> OSStatus;
    fn CVPixelBufferRetain(buffer: CVPixelBufferRef) -> CVPixelBufferRef;
    fn CVPixelBufferRelease(buffer: CVPixelBufferRef);
    fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, lock_flags: u64) -> OSStatus;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, unlock_flags: u64) -> OSStatus;
    fn CVPixelBufferGetPlaneCount(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(
        buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetWidthOfPlane(buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVBufferSetAttachment(
        buffer: CVPixelBufferRef,
        key: *const c_void,
        value: *const c_void,
        attachment_mode: u32,
    );

    static kCVImageBufferColorPrimariesKey: *const c_void;
    static kCVImageBufferTransferFunctionKey: *const c_void;
    static kCVImageBufferYCbCrMatrixKey: *const c_void;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: *const c_void;
    static kCVImageBufferColorPrimaries_EBU_3213: *const c_void;
    static kCVImageBufferColorPrimaries_SMPTE_C: *const c_void;
    static kCVImageBufferColorPrimaries_P3_D65: *const c_void;
    static kCVImageBufferTransferFunction_ITU_R_709_2: *const c_void;
    static kCVImageBufferTransferFunction_sRGB: *const c_void;
    static kCVImageBufferYCbCrMatrix_ITU_R_601_4: *const c_void;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: *const c_void;
}

const K_CV_PIXEL_FORMAT_TYPE_420YPCBCR8_BIPLANAR_VIDEO_RANGE: u32 = 0x3432_3076; // '420v'
const K_CV_PIXEL_FORMAT_TYPE_420YPCBCR8_BIPLANAR_FULL_RANGE: u32 = 0x3432_3066; // '420f'
const K_CV_PIXEL_LOCK_NONE: u64 = 0;
const K_CV_ATTACHMENT_MODE_SHOULD_PROPAGATE: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeColorMapping {
    pub pixel_format_type: u32,
    pub color_primaries: &'static str,
    pub transfer_function: &'static str,
    pub ycbcr_matrix: &'static str,
}

pub(crate) fn native_color_mapping(profile: VideoColorProfile) -> NativeColorMapping {
    NativeColorMapping {
        pixel_format_type: match profile.range {
            PixelRange::Video => K_CV_PIXEL_FORMAT_TYPE_420YPCBCR8_BIPLANAR_VIDEO_RANGE,
            PixelRange::Full => K_CV_PIXEL_FORMAT_TYPE_420YPCBCR8_BIPLANAR_FULL_RANGE,
        },
        color_primaries: match profile.primaries {
            ColorPrimaries::Bt709 => "ITU_R_709_2",
            ColorPrimaries::Bt601Pal => "EBU_3213",
            ColorPrimaries::Bt601Ntsc => "SMPTE_C",
            ColorPrimaries::DisplayP3 => "P3_D65",
        },
        transfer_function: match profile.transfer {
            TransferFunction::Bt709 => "ITU_R_709_2",
            TransferFunction::Srgb => "sRGB",
        },
        ycbcr_matrix: match profile.matrix {
            MatrixCoefficients::Bt601 => "ITU_R_601_4",
            MatrixCoefficients::Bt709 => "ITU_R_709_2",
        },
    }
}

pub(crate) fn attach_video_color_profile_to_cv_pixel_buffer(
    pixel_buffer: *mut c_void,
    profile: VideoColorProfile,
) {
    attach_video_color_profile(pixel_buffer, profile);
}

fn attach_video_color_profile(pixel_buffer: CVPixelBufferRef, profile: VideoColorProfile) {
    if pixel_buffer.is_null() {
        return;
    }

    unsafe {
        CVBufferSetAttachment(
            pixel_buffer,
            kCVImageBufferColorPrimariesKey,
            color_primaries_attachment_value(profile.primaries),
            K_CV_ATTACHMENT_MODE_SHOULD_PROPAGATE,
        );
        CVBufferSetAttachment(
            pixel_buffer,
            kCVImageBufferTransferFunctionKey,
            transfer_attachment_value(profile.transfer),
            K_CV_ATTACHMENT_MODE_SHOULD_PROPAGATE,
        );
        CVBufferSetAttachment(
            pixel_buffer,
            kCVImageBufferYCbCrMatrixKey,
            ycbcr_matrix_attachment_value(profile.matrix),
            K_CV_ATTACHMENT_MODE_SHOULD_PROPAGATE,
        );
    }
}

fn color_primaries_attachment_value(primaries: ColorPrimaries) -> *const c_void {
    unsafe {
        match primaries {
            ColorPrimaries::Bt709 => kCVImageBufferColorPrimaries_ITU_R_709_2,
            ColorPrimaries::Bt601Pal => kCVImageBufferColorPrimaries_EBU_3213,
            ColorPrimaries::Bt601Ntsc => kCVImageBufferColorPrimaries_SMPTE_C,
            ColorPrimaries::DisplayP3 => kCVImageBufferColorPrimaries_P3_D65,
        }
    }
}

fn transfer_attachment_value(transfer: TransferFunction) -> *const c_void {
    unsafe {
        match transfer {
            TransferFunction::Bt709 => kCVImageBufferTransferFunction_ITU_R_709_2,
            TransferFunction::Srgb => kCVImageBufferTransferFunction_sRGB,
        }
    }
}

fn ycbcr_matrix_attachment_value(matrix: MatrixCoefficients) -> *const c_void {
    unsafe {
        match matrix {
            MatrixCoefficients::Bt601 => kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            MatrixCoefficients::Bt709 => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
        }
    }
}

/// One CoreVideo-owned NV12 `CVPixelBuffer`, used only for software-decoded
/// remote frames that cannot take the H.264/Native zero-copy path.
pub struct OwnedCVPixelBuffer(CVPixelBufferRef);

impl OwnedCVPixelBuffer {
    pub fn as_ptr(&self) -> CVPixelBufferRef {
        self.0
    }
}

impl Drop for OwnedCVPixelBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CVPixelBufferRelease(self.0) };
            // #683: paired with the increment at construction below (the
            // ONLY place this tuple struct is built). This is deliberately
            // NOT paired with the transient `CVPixelBufferRetain`/
            // `CVPixelBufferRelease` further down in this file
            // (`create_sample_buffer`) -- that pair is released inside the
            // same function call and would double-count every displayed
            // frame if touched here.
            crate::platform::mem::LIVE_PIXEL_BUFFERS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

// SAFETY: This wrapper owns one retained CVPixelBufferRef. We fill it before
// publication, then only hand the immutable pixel buffer to CoreMedia for
// sample wrapping; CoreVideo buffers are retain-counted references designed to
// cross framework/thread boundaries under that ownership model.
unsafe impl Send for OwnedCVPixelBuffer {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I420ToCvPixelBufferError {
    InvalidDimensions,
    CreateFailed(OSStatus),
    LockFailed(OSStatus),
    UnexpectedPlaneLayout { plane_count: usize },
    MissingPlaneAddress,
}

/// Borrowed I420 planes with explicit strides. Kept as plain slices so the
/// subscriber can feed either a real I420 buffer or a `to_i420()` conversion
/// of I422/I444/etc. without tying this module to LiveKit's concrete buffer
/// type.
pub struct I420Planes<'a> {
    pub y: &'a [u8],
    pub y_stride: u32,
    pub u: &'a [u8],
    pub u_stride: u32,
    pub v: &'a [u8],
    pub v_stride: u32,
    pub width: u32,
    pub height: u32,
}

pub fn i420_to_cv_pixel_buffer(
    planes: I420Planes<'_>,
) -> Result<OwnedCVPixelBuffer, I420ToCvPixelBufferError> {
    i420_to_cv_pixel_buffer_with_color_profile(planes, VideoColorProfile::BT601_VIDEO)
}

pub fn i420_to_cv_pixel_buffer_with_color_profile(
    planes: I420Planes<'_>,
    color_profile: VideoColorProfile,
) -> Result<OwnedCVPixelBuffer, I420ToCvPixelBufferError> {
    validate_i420_planes(&planes)?;
    let color_mapping = native_color_mapping(color_profile);

    let mut pixel_buffer: CVPixelBufferRef = std::ptr::null_mut();
    let create_status = unsafe {
        CVPixelBufferCreate(
            std::ptr::null(),
            planes.width as usize,
            planes.height as usize,
            color_mapping.pixel_format_type,
            std::ptr::null(),
            &mut pixel_buffer,
        )
    };
    if create_status != 0 || pixel_buffer.is_null() {
        return Err(I420ToCvPixelBufferError::CreateFailed(create_status));
    }

    let owned = OwnedCVPixelBuffer(pixel_buffer);
    // #683: paired with the decrement in `Drop` above -- the ONLY increment
    // site, matching the ONLY construction site of this tuple struct.
    crate::platform::mem::LIVE_PIXEL_BUFFERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    attach_video_color_profile(owned.0, color_profile);
    let lock_status = unsafe { CVPixelBufferLockBaseAddress(owned.0, K_CV_PIXEL_LOCK_NONE) };
    if lock_status != 0 {
        return Err(I420ToCvPixelBufferError::LockFailed(lock_status));
    }

    let result = fill_locked_nv12_buffer(owned.0, &planes);
    unsafe {
        let unlock_status = CVPixelBufferUnlockBaseAddress(owned.0, K_CV_PIXEL_LOCK_NONE);
        if unlock_status != 0 {
            log::warn!("native_display: CVPixelBufferUnlockBaseAddress failed: {unlock_status}");
        }
    }
    result?;

    Ok(owned)
}

fn validate_i420_planes(planes: &I420Planes<'_>) -> Result<(), I420ToCvPixelBufferError> {
    if planes.width == 0 || planes.height == 0 {
        return Err(I420ToCvPixelBufferError::InvalidDimensions);
    }
    let chroma_w = chroma_extent(planes.width);
    let chroma_h = chroma_extent(planes.height);
    if planes.y_stride < planes.width
        || planes.u_stride < chroma_w
        || planes.v_stride < chroma_w
        || planes.y.len() < strided_len(planes.y_stride, planes.height, planes.width)
        || planes.u.len() < strided_len(planes.u_stride, chroma_h, chroma_w)
        || planes.v.len() < strided_len(planes.v_stride, chroma_h, chroma_w)
    {
        return Err(I420ToCvPixelBufferError::InvalidDimensions);
    }
    Ok(())
}

fn fill_locked_nv12_buffer(
    pixel_buffer: CVPixelBufferRef,
    planes: &I420Planes<'_>,
) -> Result<(), I420ToCvPixelBufferError> {
    let plane_count = unsafe { CVPixelBufferGetPlaneCount(pixel_buffer) };
    if plane_count < 2 {
        return Err(I420ToCvPixelBufferError::UnexpectedPlaneLayout { plane_count });
    }

    let dst_y = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0) as *mut u8 };
    let dst_uv = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1) as *mut u8 };
    if dst_y.is_null() || dst_uv.is_null() {
        return Err(I420ToCvPixelBufferError::MissingPlaneAddress);
    }

    let dst_y_stride = unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0) };
    let dst_uv_stride = unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1) };
    let dst_y_rows = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, 0) };
    let dst_uv_rows = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, 1) };
    let dst_y_width = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, 0) };
    let dst_uv_width = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, 1) };
    if !nv12_layout_can_hold(
        planes.width,
        planes.height,
        dst_y_width,
        dst_y_rows,
        dst_y_stride,
        dst_uv_width,
        dst_uv_rows,
        dst_uv_stride,
    ) {
        return Err(I420ToCvPixelBufferError::UnexpectedPlaneLayout { plane_count });
    }

    unsafe {
        yuv_sys::rs_I420ToNV12(
            planes.y.as_ptr(),
            planes.y_stride as i32,
            planes.u.as_ptr(),
            planes.u_stride as i32,
            planes.v.as_ptr(),
            planes.v_stride as i32,
            dst_y,
            dst_y_stride as i32,
            dst_uv,
            dst_uv_stride as i32,
            planes.width as i32,
            planes.height as i32,
        );
    }
    Ok(())
}

fn chroma_extent(value: u32) -> u32 {
    (value + 1) / 2
}

fn strided_len(stride: u32, rows: u32, row_bytes: u32) -> usize {
    if rows == 0 {
        0
    } else {
        (stride as usize) * ((rows - 1) as usize) + row_bytes as usize
    }
}

fn nv12_layout_can_hold(
    width: u32,
    height: u32,
    y_width: usize,
    y_rows: usize,
    y_stride: usize,
    uv_width: usize,
    uv_rows: usize,
    uv_stride: usize,
) -> bool {
    let width = width as usize;
    let height = height as usize;
    let chroma_rows = chroma_extent(height as u32) as usize;
    y_width >= width
        && y_rows >= height
        && y_stride >= width
        && uv_width >= chroma_extent(width as u32) as usize
        && uv_rows >= chroma_rows
        && uv_stride >= width + (width % 2)
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMVideoFormatDescriptionCreateForImageBuffer(
        allocator: *const c_void,
        image_buffer: CVPixelBufferRef,
        format_description_out: *mut CMVideoFormatDescriptionRef,
    ) -> OSStatus;

    fn CMSampleBufferCreateForImageBuffer(
        allocator: *const c_void,
        image_buffer: CVPixelBufferRef,
        data_ready: bool,
        make_data_ready_callback: *const c_void,
        make_data_ready_refcon: *const c_void,
        format_description: CMVideoFormatDescriptionRef,
        sample_timing: *const CMSampleTimingInfo,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;

    fn CFRelease(cf: *const c_void);

    /// `CMAttachmentBearerRef`, `CFStringRef`, `CFTypeRef` are all opaque
    /// pointers in this module's raw-C-ABI style (see module doc comment) --
    /// `target` here is the `CMSampleBufferRef` returned by
    /// `CMSampleBufferCreateForImageBuffer` above (a `CMSampleBufferRef`
    /// bears attachments per CoreMedia's own type hierarchy).
    fn CMSetAttachment(
        target: *mut c_void,
        key: *const c_void,
        value: *const c_void,
        attachment_mode: u32,
    );

    /// Read-only counterpart of `CMSetAttachment`, used only to verify the
    /// attachment landed (see this module's tests).
    fn CMGetAttachment(
        target: *mut c_void,
        key: *const c_void,
        attachment_mode_out: *mut u32,
    ) -> *const c_void;

    /// `CFStringRef` (CFBoolean key) -- verified against this Mac's SDK
    /// header (`CMSampleBuffer.h`): "CM_EXPORT const CFStringRef
    /// kCMSampleAttachmentKey_DisplayImmediately  // CFBoolean". Setting this
    /// true on every sample tells `AVSampleBufferDisplayLayer` to show the
    /// frame the instant it's dequeued, bypassing its internal PTS-based
    /// scheduling entirely (see `create_sample_buffer`'s doc comment for why
    /// that scheduling was never configured to mean anything here).
    static kCMSampleAttachmentKey_DisplayImmediately: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    /// `CFBooleanRef` singleton (verified against `CFNumber.h`:
    /// "const CFBooleanRef kCFBooleanTrue").
    static kCFBooleanTrue: *const c_void;
}

/// `kCMAttachmentMode_ShouldPropagate` (`CMAttachment.h`) -- the attachment
/// should carry over if this sample buffer is ever copied, matching Apple's
/// own sample code for per-sample display-immediately attachments.
const K_CM_ATTACHMENT_MODE_SHOULD_PROPAGATE: u32 = 1;

/// Build a `CMSampleBuffer` wrapping `pixel_buffer` with NO pixel data copy
/// -- `CMSampleBufferCreateForImageBuffer` just wraps the existing
/// IOSurface-backed `CVPixelBufferRef` with CoreMedia's timing/format
/// envelope around it. Returns the sample buffer as an owned, retained
/// pointer (caller must `CFRelease` -- wrapped by `OwnedCMSampleBuffer`
/// below) or `None` on failure.
///
/// `pixel_buffer` is borrowed (not consumed) -- this function retains its
/// own reference via `CVPixelBufferRetain` for the format-description call,
/// matching Apple's documented ownership contract for these APIs.
fn create_sample_buffer(
    pixel_buffer: CVPixelBufferRef,
    pts: CMTime,
) -> Option<OwnedCMSampleBuffer> {
    if pixel_buffer.is_null() {
        return None;
    }

    unsafe {
        let retained_pixel_buffer = CVPixelBufferRetain(pixel_buffer);

        let mut format_description: CMVideoFormatDescriptionRef = std::ptr::null_mut();
        let status = CMVideoFormatDescriptionCreateForImageBuffer(
            std::ptr::null(),
            retained_pixel_buffer,
            &mut format_description,
        );
        if status != 0 || format_description.is_null() {
            log::warn!(
                "native_display: CMVideoFormatDescriptionCreateForImageBuffer failed: {status}"
            );
            CVPixelBufferRelease(retained_pixel_buffer);
            return None;
        }

        let timing = CMSampleTimingInfo {
            duration: cm_time_invalid(),
            presentation_time_stamp: pts,
            decode_time_stamp: cm_time_invalid(),
        };

        let mut sample_buffer: CMSampleBufferRef = std::ptr::null_mut();
        let status = CMSampleBufferCreateForImageBuffer(
            std::ptr::null(),
            retained_pixel_buffer,
            true,
            std::ptr::null(),
            std::ptr::null(),
            format_description,
            &timing,
            &mut sample_buffer,
        );

        // The sample buffer (once created) retains the image buffer and
        // format description itself; release our extra local references.
        CFRelease(format_description as *const c_void);
        CVPixelBufferRelease(retained_pixel_buffer);

        if status != 0 || sample_buffer.is_null() {
            log::warn!("native_display: CMSampleBufferCreateForImageBuffer failed: {status}");
            return None;
        }

        // #110: this is a live "always show the newest frame now" preview,
        // not scheduled file playback, but nothing here configures a real
        // `controlTimebase` for the display layer -- PTS is a synthetic,
        // arbitrarily-scaled counter (see this fn's `pts` doc comment above).
        // Without DisplayImmediately, the layer is free to interpret that PTS
        // as real presentation timing and hold a frame until its internal
        // clock says it's "due" -- which can degenerate into a stall that
        // looks exactly like "only the first frame gets through before
        // freezing." Marking every sample DisplayImmediately makes the
        // layer show it the instant it's dequeued, bypassing PTS-based
        // scheduling entirely (Apple's documented mechanism for exactly this
        // live-preview use case).
        CMSetAttachment(
            sample_buffer,
            kCMSampleAttachmentKey_DisplayImmediately,
            kCFBooleanTrue,
            K_CM_ATTACHMENT_MODE_SHOULD_PROPAGATE,
        );

        Some(OwnedCMSampleBuffer(sample_buffer))
    }
}

/// RAII wrapper releasing the `CMSampleBufferRef` on drop. Public so the
/// compositor can build one on the decode thread (where the source
/// `CVPixelBuffer` is still alive) and then move it to the main thread just
/// for the `enqueueSampleBuffer:` call -- the sample buffer retains the pixel
/// buffer internally at creation time, so it's self-contained and safe to
/// carry across the thread hop.
pub struct OwnedCMSampleBuffer(CMSampleBufferRef);

impl Drop for OwnedCMSampleBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0 as *const c_void) };
        }
    }
}

// SAFETY: CMSampleBufferRef is a CoreFoundation-style retain-counted
// pointer; passing ownership across threads (e.g. handing a frame from the
// LiveKit decode callback thread to the main-thread AVSampleBufferDisplayLayer
// enqueue call, done via `enqueue_on_main`) is safe as long as no two
// threads mutate it concurrently -- we only ever read/enqueue it once, then
// drop it, matching Apple's own documented thread-safety contract for
// CMSampleBuffer (immutable after creation).
unsafe impl Send for OwnedCMSampleBuffer {}

// =============================================================================
// AVSampleBufferDisplayLayer (the one real Objective-C message-send surface)
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayLayerFilter {
    Nearest,
    Linear,
}

impl DisplayLayerFilter {
    fn ca_filter_name(self) -> &'static str {
        match self {
            Self::Nearest => "nearest",
            Self::Linear => "linear",
        }
    }
}

pub(crate) fn display_filter_for_geometry(
    source_width_px: u32,
    source_height_px: u32,
    displayed_width_points: f64,
    displayed_height_points: f64,
    receiver_scale: f64,
) -> DisplayLayerFilter {
    if source_width_px == 0
        || source_height_px == 0
        || !displayed_width_points.is_finite()
        || !displayed_height_points.is_finite()
        || !receiver_scale.is_finite()
        || displayed_width_points <= 0.0
        || displayed_height_points <= 0.0
        || receiver_scale <= 0.0
    {
        return DisplayLayerFilter::Linear;
    }

    let displayed_width_px = displayed_width_points * receiver_scale;
    let displayed_height_px = displayed_height_points * receiver_scale;
    match (
        integer_display_scale(source_width_px as f64, displayed_width_px),
        integer_display_scale(source_height_px as f64, displayed_height_px),
    ) {
        (Some(width_scale), Some(height_scale)) if width_scale == height_scale => {
            DisplayLayerFilter::Nearest
        }
        _ => DisplayLayerFilter::Linear,
    }
}

fn integer_display_scale(source_px: f64, displayed_px: f64) -> Option<u32> {
    if source_px <= 0.0 || displayed_px <= 0.0 {
        return None;
    }
    let ratio = displayed_px / source_px;
    let scale = ratio.round();
    if !(1.0..=2.0).contains(&scale) {
        return None;
    }
    let scale = scale as u32;
    let expected_px = source_px * scale as f64;
    ((displayed_px - expected_px).abs() <= PIXEL_MATCH_EPSILON).then_some(scale)
}

fn set_layer_filter(layer: &AnyObject, filter: DisplayLayerFilter) {
    unsafe {
        let filter = objc2_foundation::NSString::from_str(filter.ca_filter_name());
        let _: () = msg_send![layer, setMagnificationFilter: &*filter];
        let _: () = msg_send![layer, setMinificationFilter: &*filter];
    }
}

/// A single `AVSampleBufferDisplayLayer`, hosting real decoded video frames
/// with no CPU copy from the `CVPixelBuffer` handed in. Owns a `Retained<
/// AnyObject>` (the layer itself) so it can be attached as a sublayer of a
/// panel's content view and outlives individual frame pushes.
pub struct DisplayLayer {
    layer: Retained<AnyObject>,
    /// A dedicated layer-HOSTING `NSView` whose backing layer IS `layer`.
    /// Added as a real subview ON TOP of the panel's WKWebView (see
    /// `compositor::attach_display_layer`). This is load-bearing: adding the
    /// display layer merely as a *sublayer* of the content view's backing
    /// layer put it UNDERNEATH the opaque WKWebView, which composited over it
    /// -> the window rendered fully black even though frames were enqueuing.
    /// A sibling subview added after the webview renders above it.
    view: Retained<AnyObject>,
    frame_seq: std::sync::atomic::AtomicI64,
}

// SAFETY: All actual use of `layer` (construction, `enqueue`, geometry
// updates) happens on the main thread via `tauri::async_runtime`'s
// main-thread dispatch (`run_on_main_thread` at the call sites in
// `compositor.rs`) -- mirrors how `menubar.rs`/`hover_tab.rs` already treat
// their own AppKit object handles as main-thread-only despite being stored
// in structs that cross thread boundaries as plain data.
unsafe impl Send for DisplayLayer {}
unsafe impl Sync for DisplayLayer {}

impl DisplayLayer {
    /// Create a new, empty `AVSampleBufferDisplayLayer`. Must be called on
    /// the main thread (AppKit/CoreAnimation requirement).
    pub fn new() -> Self {
        // `[[AVSampleBufferDisplayLayer alloc] init]` -- the class is
        // resolved by name at link time from AVFoundation.framework (linked
        // via the `#[link(name = "AVFoundation", ...)]` below), no
        // `objc2-av-foundation` binding crate needed for this one call.
        let layer: Retained<AnyObject> = unsafe {
            let cls = class!(AVSampleBufferDisplayLayer);
            let obj: *mut AnyObject = msg_send![cls, alloc];
            let obj: *mut AnyObject = msg_send![obj, init];
            Retained::from_raw(obj).expect("AVSampleBufferDisplayLayer init returned nil")
        };

        // videoGravity = AVLayerVideoGravityResizeAspect -- letterbox to fit
        // rather than stretch/crop, matching "resizable with aspect lock"
        // (SPEC.md §4.4) at the pixel-presentation layer (the window's own
        // resize handling, in `compositor.rs`, separately keeps the window's
        // outer frame aspect-locked to the source).
        unsafe {
            let gravity = objc2_foundation::NSString::from_str("AVLayerVideoGravityResizeAspect");
            let _: () = msg_send![&*layer, setVideoGravity: &*gravity];
        }
        set_layer_filter(&layer, DisplayLayerFilter::Nearest);

        // Create the layer-hosting NSView. Setting `layer` as the view's layer
        // FIRST and then `wantsLayer = YES` makes this a layer-*hosting* view
        // (the AVSampleBufferDisplayLayer is the view's backing store), as
        // opposed to a layer-backed view (which would create its own layer and
        // relegate ours to a sublayer). Must be on the main thread (NSView).
        let view: Retained<AnyObject> = unsafe {
            let alloc: *mut AnyObject = msg_send![class!(NSView), alloc];
            let v: *mut AnyObject = msg_send![alloc, init];
            let v = Retained::from_raw(v).expect("NSView init returned nil");
            let _: () = msg_send![&*v, setLayer: Retained::as_ptr(&layer) as *mut AnyObject];
            let _: () = msg_send![&*v, setWantsLayer: true];
            v
        };

        Self {
            layer,
            view,
            frame_seq: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// The underlying `CALayer` (an `AVSampleBufferDisplayLayer` IS-A
    /// `CALayer`), as a raw pointer -- used for `enqueueSampleBuffer:`.
    pub fn as_layer_ptr(&self) -> *mut AnyObject {
        Retained::as_ptr(&self.layer) as *mut AnyObject
    }

    /// The layer-hosting `NSView`, for `[contentView addSubview:]` -- see
    /// `compositor.rs::attach_display_layer`.
    pub fn as_view_ptr(&self) -> *mut AnyObject {
        Retained::as_ptr(&self.view) as *mut AnyObject
    }

    /// Set the hosting VIEW's frame (in its superview's coordinate space --
    /// the panel content view, non-flipped/bottom-left origin). The hosted
    /// `AVSampleBufferDisplayLayer` automatically fills the view's bounds, so
    /// sizing the view sizes the video. Uses `objc2_foundation`'s `NSRect`
    /// (bit-identical to `CGRect`).
    pub fn set_frame(&self, x: f64, y: f64, width: f64, height: f64) {
        use objc2_foundation::{NSPoint, NSRect, NSSize};
        let rect = NSRect {
            origin: NSPoint::new(x, y),
            size: NSSize::new(width, height),
        };
        unsafe {
            let _: () = msg_send![&*self.view, setFrame: rect];
        }
    }

    pub fn set_contents_scale(&self, scale: f64) {
        unsafe {
            let _: () = msg_send![&*self.layer, setContentsScale: scale.max(1.0)];
        }
    }

    pub fn update_filter_for_geometry(
        &self,
        source_width_px: u32,
        source_height_px: u32,
        displayed_width_points: f64,
        displayed_height_points: f64,
        receiver_scale: f64,
    ) {
        let filter = display_filter_for_geometry(
            source_width_px,
            source_height_px,
            displayed_width_points,
            displayed_height_points,
            receiver_scale,
        );
        set_layer_filter(&self.layer, filter);
    }

    /// Enqueue one real decoded frame, wrapping `cv_pixel_buffer` (a raw
    /// `CVPixelBufferRef` -- see this module's doc comment for exactly where
    /// it comes from) in a `CMSampleBuffer` with NO pixel copy, then handing
    /// it to `AVSampleBufferDisplayLayer.enqueueSampleBuffer:`. Must be
    /// called on the main thread (AVFoundation/CoreAnimation requirement);
    /// `compositor.rs`'s frame-pump task hops to the main thread once per
    /// frame via `tauri::async_runtime::spawn` + a main-thread dispatch, same
    /// pattern already used for `menubar.rs` redraws.
    /// Build a `CMSampleBuffer` wrapping `cv_pixel_buffer` (NO pixel copy).
    /// Call this on whatever thread currently owns a live reference to the
    /// pixel buffer (the LiveKit decode thread) -- the returned
    /// `OwnedCMSampleBuffer` retains the pixel buffer internally, so it's safe
    /// to then hand to [`enqueue_prepared`] on the main thread even after the
    /// original frame is dropped.
    pub fn prepare_sample(&self, cv_pixel_buffer: *mut c_void) -> Option<OwnedCMSampleBuffer> {
        let seq = self
            .frame_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Monotonic synthetic presentation timestamps at an arbitrary
        // (but consistent) 90kHz timescale -- `AVSampleBufferDisplayLayer`
        // in `.immediate`-ish "just show me each frame ASAP" usage (no
        // `controlTimebase` configured) only needs monotonically
        // increasing PTS values, not real wall-clock alignment; the actual
        // display timing is driven by frames arriving off the network at
        // their own real cadence, matching how a live "always render the
        // newest frame" preview layer is meant to be driven (as opposed to
        // file playback, which needs true presentation-time scheduling).
        let pts = cm_time(seq, 90_000);
        create_sample_buffer(cv_pixel_buffer, pts)
    }

    /// Enqueue a previously-[`prepare_sample`]d frame onto the layer. **Must
    /// be called on the main thread** -- `AVSampleBufferDisplayLayer` is a
    /// `CALayer`, and enqueuing/compositing off the main thread (without an
    /// explicit `CATransaction`) reliably results in *nothing being displayed*
    /// (the layer stays black) even though the enqueue "succeeds". This was
    /// the real cause of the black compositor window: the frame pump enqueued
    /// from a background tokio task. `compositor::push_frame` now hops here via
    /// `run_on_main_thread`.
    pub fn enqueue_prepared(&self, sample: &OwnedCMSampleBuffer) {
        // #110: previously nothing ever checked `status`, so once the layer
        // entered `AVQueuedSampleBufferRenderingStatusFailed` (e.g. a decoder-
        // resource reclaim, or any other internal AVFoundation error) every
        // subsequent `enqueueSampleBuffer:` call kept "succeeding" (no return
        // value to fail) while nothing was ever actually displayed again --
        // exactly the "only the first frame gets through before freezing"
        // symptom, just triggered by a layer-side failure rather than a
        // decode/network stop. Apple's documented recovery is `-flush`, which
        // resets status back to `.unknown`; do that BEFORE this enqueue so
        // the very frame that revealed the failure still gets a fresh shot at
        // display instead of being silently dropped into a dead layer.
        // #886 investigation note: the one-IOSurface-per-frame retention that
        // looked like this layer holding samples was actually an autorelease
        // leak in webrtc-sys's `native_buffer_to_platform_image_buffer` (MRC
        // + pool-less decode threads); with that fixed, the layer holds
        // nothing measurable across thousands of enqueues (IOSurface gate
        // `grown=0`). A periodic flush here was tried and did NOT change the
        // un-fixed behavior -- do not re-add one without a measured need.
        const AV_QUEUED_SAMPLE_BUFFER_RENDERING_STATUS_FAILED: isize = 2;
        unsafe {
            let status: isize = msg_send![&*self.layer, status];
            if status == AV_QUEUED_SAMPLE_BUFFER_RENDERING_STATUS_FAILED {
                log::warn!(
                    "native_display: AVSampleBufferDisplayLayer status=failed -- flushing to recover"
                );
                let _: () = msg_send![&*self.layer, flush];
            }
            let _: () = msg_send![&*self.layer, enqueueSampleBuffer: sample.0];
        }
        // `enqueueSampleBuffer:` retains its own reference internally per
        // Apple's documented contract; the caller still owns `sample` and
        // releases it on drop, which does not free the buffer out from under
        // the layer.
    }
}

impl Default for DisplayLayer {
    fn default() -> Self {
        Self::new()
    }
}

// `AVSampleBufferDisplayLayer` is resolved via `objc2::class!` at runtime
// (Objective-C class lookup by name), but the symbol still needs the
// AVFoundation framework linked into the binary -- this empty `extern "C"`
// block is the standard way to force that link without pulling in an
// `objc2-av-foundation` binding crate (see this module's top doc comment).
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_filter_selects_nearest_only_for_integer_one_or_two_x_magnification() {
        let cases = [
            (
                "one_x",
                (1000, 500, 500.0, 250.0, 2.0),
                DisplayLayerFilter::Nearest,
            ),
            (
                "two_x",
                (1000, 500, 1000.0, 500.0, 2.0),
                DisplayLayerFilter::Nearest,
            ),
            (
                "three_x_stays_linear",
                (1000, 500, 1500.0, 750.0, 2.0),
                DisplayLayerFilter::Linear,
            ),
            (
                "fractional_magnification",
                (1000, 500, 750.0, 375.0, 2.0),
                DisplayLayerFilter::Linear,
            ),
            (
                "downscale",
                (1000, 500, 250.0, 125.0, 2.0),
                DisplayLayerFilter::Linear,
            ),
            (
                "near_integer_inside_half_pixel",
                (1000, 500, 1000.2, 500.1, 2.0),
                DisplayLayerFilter::Nearest,
            ),
            (
                "near_integer_outside_half_pixel",
                (1000, 500, 1000.3, 500.3, 2.0),
                DisplayLayerFilter::Linear,
            ),
            (
                "mismatched_axes",
                (1000, 500, 1000.0, 250.0, 2.0),
                DisplayLayerFilter::Linear,
            ),
            (
                "degenerate",
                (1000, 500, 0.0, 250.0, 2.0),
                DisplayLayerFilter::Linear,
            ),
        ];

        for (name, (source_w, source_h, content_w, content_h, receiver_scale), expected) in cases {
            assert_eq!(
                display_filter_for_geometry(
                    source_w,
                    source_h,
                    content_w,
                    content_h,
                    receiver_scale
                ),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn native_mapping_preserves_legacy_bt601_video_profile() {
        assert_eq!(
            native_color_mapping(VideoColorProfile::BT601_VIDEO),
            NativeColorMapping {
                pixel_format_type: K_CV_PIXEL_FORMAT_TYPE_420YPCBCR8_BIPLANAR_VIDEO_RANGE,
                color_primaries: "SMPTE_C",
                transfer_function: "ITU_R_709_2",
                ycbcr_matrix: "ITU_R_601_4",
            }
        );
    }

    #[test]
    fn native_mapping_uses_full_range_and_bt709_for_srgb_screenshare_profile() {
        assert_eq!(
            native_color_mapping(VideoColorProfile::SRGB_BT709_FULL),
            NativeColorMapping {
                pixel_format_type: K_CV_PIXEL_FORMAT_TYPE_420YPCBCR8_BIPLANAR_FULL_RANGE,
                color_primaries: "ITU_R_709_2",
                transfer_function: "sRGB",
                ycbcr_matrix: "ITU_R_709_2",
            }
        );
    }

    #[test]
    fn native_mapping_keeps_display_p3_primaries_with_bt709_ycbcr_matrix() {
        assert_eq!(
            native_color_mapping(VideoColorProfile::DISPLAY_P3_BT709_FULL),
            NativeColorMapping {
                pixel_format_type: K_CV_PIXEL_FORMAT_TYPE_420YPCBCR8_BIPLANAR_FULL_RANGE,
                color_primaries: "P3_D65",
                transfer_function: "sRGB",
                ycbcr_matrix: "ITU_R_709_2",
            }
        );
    }

    #[test]
    fn sample_buffer_is_marked_display_immediately() {
        // #110: real CVPixelBuffer -> real CMSampleBuffer round trip (pure
        // CoreVideo/CoreMedia, no AVFoundation layer involved -- safe to run
        // off the main thread under `cargo test`) verifying `create_sample_buffer`
        // actually attaches DisplayImmediately=true, not just that the code
        // compiles.
        let width = 16u32;
        let height = 16u32;
        let chroma = chroma_extent(width);
        let y = vec![128u8; (width * height) as usize];
        let u = vec![128u8; (chroma * chroma) as usize];
        let v = u.clone();
        let planes = I420Planes {
            y: &y,
            y_stride: width,
            u: &u,
            u_stride: chroma,
            v: &v,
            v_stride: chroma,
            width,
            height,
        };
        let pixel_buffer = i420_to_cv_pixel_buffer(planes)
            .expect("valid I420 planes must produce a CVPixelBuffer");

        let sample = create_sample_buffer(pixel_buffer.0, cm_time(0, 90_000))
            .expect("a valid pixel buffer must produce a sample buffer");

        unsafe {
            let value = CMGetAttachment(
                sample.0,
                kCMSampleAttachmentKey_DisplayImmediately,
                std::ptr::null_mut(),
            );
            assert!(
                !value.is_null(),
                "DisplayImmediately attachment must be set on every sample buffer (#110)"
            );
            assert_eq!(
                value, kCFBooleanTrue,
                "DisplayImmediately must be kCFBooleanTrue, not merely present"
            );
        }
    }

    #[test]
    fn owned_cv_pixel_buffer_construction_and_drop_are_counted() {
        // #683: real CVPixelBufferCreate -> real Drop round trip, verifying
        // the `platform::mem::LIVE_PIXEL_BUFFERS` pairing this type owns.
        //
        // The counter is process-global and `cargo test` runs concurrently, so
        // NO single-shot delta assertion is race-free: another test's
        // construction landing between our two reads offsets our own drop
        // (observed live as `during=1 after=1` -- the flake that repeatedly
        // broke ci-local). The old comment claimed the delta was strictly
        // attributable to this buffer; that only holds when no OTHER increment
        // lands in the same window, which parallelism does not guarantee.
        // Bounded retries fix the flake without weakening the oracle: a REAL
        // Drop-pairing regression fails every attempt (drop never decrements,
        // so `after >= during` deterministically), while the race clears with
        // overwhelming probability within a few attempts.
        let width = 4u32;
        let height = 4u32;
        let chroma = chroma_extent(width);
        let y = vec![128u8; (width * height) as usize];
        let u = vec![128u8; (chroma * chroma) as usize];
        let v = u.clone();
        const ATTEMPTS: u32 = 5;
        let mut construction_seen = false;
        let mut drop_seen = false;
        for _ in 0..ATTEMPTS {
            let before = crate::platform::mem::live_pixel_buffer_count()
                .expect("live_pixel_buffer_count() is Some on macOS, where this test runs");
            let owned = i420_to_cv_pixel_buffer(I420Planes {
                y: &y,
                y_stride: width,
                u: &u,
                u_stride: chroma,
                v: &v,
                v_stride: chroma,
                width,
                height,
            })
            .expect("valid I420 planes must produce a CVPixelBuffer");
            let during = crate::platform::mem::live_pixel_buffer_count().unwrap();
            drop(owned);
            let after = crate::platform::mem::live_pixel_buffer_count().unwrap();
            construction_seen |= during > before;
            drop_seen |= after < during;
            if construction_seen && drop_seen {
                return;
            }
        }
        assert!(
            construction_seen,
            "construction never incremented the live-buffer counter in {ATTEMPTS} attempts"
        );
        assert!(
            drop_seen,
            "Drop never decremented the live-buffer counter in {ATTEMPTS} attempts"
        );
    }
}
