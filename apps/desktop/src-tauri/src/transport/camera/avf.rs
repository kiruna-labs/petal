#![cfg(target_os = "macos")]

//! Native webcam capture — AVFoundation
//! `AVCaptureSession` + `AVCaptureVideoDataOutput` delivering NV12
//! `CVPixelBuffer`s to a Rust callback, for publish via
//! `RoomConnection::publish_camera` / `PublishedTrack::push_nv12`.
//!
//! This replaces the former DESIGN-NOTE-ONLY stub that lived in this file
//! (the pinned `livekit` 0.7.49 Rust SDK has NO camera capture surface —
//! verified against its source; only audio has a device module). The
//! capture pipeline is therefore hand-rolled here.
//!
//! ## Linker discipline (non-negotiable — see `transport/mod.rs`'s M0
//! blocker writeup and CLAUDE.md's `-ObjC`/duplicate-Swift-metadata notes)
//!
//! NO new ObjC/Swift binding crates. Everything below is:
//! - raw `extern "C"` for C ABIs (CoreMedia/CoreVideo/libdispatch), the
//!   exact pattern `native_display.rs` proved link-safe;
//! - `objc2` `class!`/`msg_send!` message sends + one `define_class!`
//!   delegate (the pattern `menubar.rs`'s `PetalMenubarTarget` proved links
//!   clean under `-ObjC`);
//! - the `AVFoundation` framework link + `AVMediaTypeVideo` extern static,
//!   same declarations `permissions.rs` already uses.
//!
//! ## Pixel format
//!
//! We PIN `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` ('420v', NV12)
//! in the output's videoSettings — same "pin the format explicitly, don't
//! trust OS defaults" reasoning as `capture.rs`'s BGRA pin. The delegate
//! verifies the delivered format on every frame and drops (log-once)
//! anything else rather than mis-converting — the #24 lesson (libyuv naming
//! trap) applies doubly here: NV12's interleaved UV order is pinned by a
//! unit test on `push_nv12`'s `rs_NV12ToI420` call.
//!
//! ## Threading
//!
//! Frames arrive on a private serial dispatch queue (NOT the main thread).
//! The delegate callback does only: lock → copy planes → invoke the
//! caller's `on_frame` → unlock. `camera_session.rs`'s pump does the
//! NV12→I420 convert + LiveKit push on a tokio task. `start()`/`stop()` can
//! block for hundreds of ms (`startRunning`/`stopRunning`) — callers must
//! invoke them via `spawn_blocking`, never directly on the async runtime.

use std::ffi::{c_char, c_void};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{class, define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_foundation::{NSDictionary, NSNumber, NSString};

use super::{
    CameraBackend, CameraDeviceInfo, CameraError, CameraFrame, CameraStatus, CameraStatusSource,
};

// ---- C FFI ------------------------------------------------------------------

// CoreMedia / CoreVideo — same raw-FFI style as native_display.rs (duplicate
// declarations are fine; the symbols resolve to the same framework exports).
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMSampleBufferGetImageBuffer(sbuf: *mut c_void) -> *mut c_void;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferLockBaseAddress(pb: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetWidth(pb: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(pb: *mut c_void) -> usize;
    fn CVPixelBufferGetPixelFormatType(pb: *mut c_void) -> u32;
    fn CVPixelBufferGetPlaneCount(pb: *mut c_void) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: *mut c_void, plane: usize) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: *mut c_void, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pb: *mut c_void, plane: usize) -> usize;

    static kCVPixelBufferPixelFormatTypeKey: *const NSString;
}

// AVFoundation — framework link + media-type constant, same shape as
// permissions.rs's declarations.
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {
    static AVMediaTypeVideo: *const NSString;
    static AVCaptureSessionPreset1280x720: *const NSString;
    static AVCaptureSessionPreset640x480: *const NSString;
}

// libdispatch (libSystem — linked implicitly, no #[link] attribute needed).
extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
}

const K_CV_PIXEL_LOCK_READ_ONLY: u64 = 1;
/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` ('420v').
const FMT_NV12_VIDEO_RANGE: u32 = 0x3432_3076;
/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange` ('420f') — accepted too
/// (range affects levels slightly, not channel order; the pinned request is
/// '420v' so this arm is defensive).
const FMT_NV12_FULL_RANGE: u32 = 0x3432_3066;

fn should_fallback_to_default(preferred_id: Option<&str>, lookup_succeeded: bool) -> bool {
    preferred_id.is_some() && !lookup_succeeded
}

fn now_wall_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

// ---- Delegate ---------------------------------------------------------------

/// State shared with the ObjC delegate: the frame callback + a "stopped"
/// latch (frames already in flight on the dispatch queue after `stop()` are
/// dropped) + a log-once guard for unexpected pixel formats + the shared
/// status counters backing [`CameraStatus`].
struct DelegateShared {
    on_frame: Box<dyn Fn(CameraFrame) + Send + Sync>,
    stopped: AtomicBool,
    logged_format: AtomicU32,
    terminal_error: Mutex<Option<String>>,
    frames_delivered: AtomicU64,
    /// Packed `(width << 32) | height` of the most recent delivered frame —
    /// backs the `CameraBackend::dimensions()` query (the session learns
    /// real dimensions from the first frame anyway).
    dimensions: AtomicU64,
}

impl CameraStatusSource for DelegateShared {
    fn terminal_error(&self) -> Option<String> {
        // Runtime AVFoundation failures are not surfaced today; the
        // session's first-frame timeout covers startup failure, and this
        // stays `None` to preserve macOS behavior exactly.
        self.terminal_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn frames_delivered(&self) -> u64 {
        self.frames_delivered.load(Ordering::Relaxed)
    }
}

impl DelegateShared {
    fn dimensions(&self) -> (u32, u32) {
        let packed = self.dimensions.load(Ordering::Relaxed);
        ((packed >> 32) as u32, packed as u32)
    }
}

struct CameraDelegateIvars {
    shared: Arc<DelegateShared>,
}

define_class!(
    // SAFETY: NSObject subclass with no super overrides; ivars live in the
    // Rust struct above. Callbacks arrive on the capture dispatch queue —
    // deliberately NOT MainThreadOnly (contrast menubar's MenubarTarget).
    #[unsafe(super(NSObject))]
    #[name = "PetalCameraDelegate"]
    #[ivars = CameraDelegateIvars]
    struct CameraDelegate;

    unsafe impl NSObjectProtocol for CameraDelegate {}

    impl CameraDelegate {
        /// `AVCaptureVideoDataOutputSampleBufferDelegate` callback. Kept
        /// minimal: verify format, lock, copy planes, hand off, unlock.
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn capture_output(
            &self,
            _output: *mut AnyObject,
            sample_buffer: *mut c_void,
            _connection: *mut AnyObject,
        ) {
            let shared = &self.ivars().shared;
            if shared.stopped.load(Ordering::Acquire) {
                return;
            }
            unsafe {
                let pb = CMSampleBufferGetImageBuffer(sample_buffer);
                if pb.is_null() {
                    return;
                }
                let fmt = CVPixelBufferGetPixelFormatType(pb);
                if fmt != FMT_NV12_VIDEO_RANGE && fmt != FMT_NV12_FULL_RANGE {
                    // Not the pinned NV12: drop rather than mis-convert (the
                    // #24 lesson). Log once per distinct format.
                    if shared.logged_format.swap(fmt, Ordering::Relaxed) != fmt {
                        log::warn!(
                            "camera: unexpected pixel format 0x{fmt:08x} (wanted '420v' NV12) -- dropping frames of this format"
                        );
                    }
                    return;
                }
                if CVPixelBufferGetPlaneCount(pb) < 2 {
                    return;
                }
                if CVPixelBufferLockBaseAddress(pb, K_CV_PIXEL_LOCK_READ_ONLY) != 0 {
                    return;
                }

                let width = CVPixelBufferGetWidth(pb) as u32;
                let height = CVPixelBufferGetHeight(pb) as u32;
                let y_ptr = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *const u8;
                let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
                let y_rows = CVPixelBufferGetHeightOfPlane(pb, 0);
                let uv_ptr = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *const u8;
                let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
                let uv_rows = CVPixelBufferGetHeightOfPlane(pb, 1);

                if !y_ptr.is_null() && !uv_ptr.is_null() && width > 0 && height > 0 {
                    let y = std::slice::from_raw_parts(y_ptr, y_stride * y_rows).to_vec();
                    let uv = std::slice::from_raw_parts(uv_ptr, uv_stride * uv_rows).to_vec();
                    let frame = CameraFrame {
                        width,
                        height,
                        y,
                        y_stride: y_stride as u32,
                        uv,
                        uv_stride: uv_stride as u32,
                        capture_wall_time_us: now_wall_us(),
                    };
                    shared
                        .dimensions
                        .store(((width as u64) << 32) | height as u64, Ordering::Relaxed);
                    CVPixelBufferUnlockBaseAddress(pb, K_CV_PIXEL_LOCK_READ_ONLY);
                    (shared.on_frame)(frame);
                    shared.frames_delivered.fetch_add(1, Ordering::Relaxed);
                } else {
                    CVPixelBufferUnlockBaseAddress(pb, K_CV_PIXEL_LOCK_READ_ONLY);
                }
            }
        }
    }
);

// ---- Capture ----------------------------------------------------------------

/// A running webcam capture. `stop()` (or Drop) stops the session and
/// releases the camera (the green light goes off).
pub struct CameraCapture {
    /// `AVCaptureSession`, ours at +1 (alloc/init) — released on Drop.
    session: *mut AnyObject,
    /// Keep the delegate alive: `setSampleBufferDelegate:` does NOT
    /// reliably retain it.
    _delegate: Retained<CameraDelegate>,
    /// The private serial dispatch queue frames arrive on, ours at +1.
    queue: *mut c_void,
    shared: Arc<DelegateShared>,
    /// The native AVFoundation id requested by the caller, if any.
    requested_device_id: Option<String>,
    /// True when the requested native id was absent and AVFoundation's
    /// default device was used instead.
    used_default_fallback: bool,
}

// SAFETY: AVCaptureSession's start/stop/configuration API is documented
// thread-safe (Apple: "you can call these methods from any thread"); the
// raw pointers are only used for startRunning/stopRunning/release, and the
// delegate's shared state is Arc + atomics.
unsafe impl Send for CameraCapture {}
unsafe impl Sync for CameraCapture {}

impl CameraBackend for CameraCapture {
    fn stop(&mut self) {
        CameraCapture::stop(self);
    }

    fn dimensions(&self) -> (u32, u32) {
        self.shared.dimensions()
    }

    fn frame_rate(&self) -> (u32, u32) {
        // macOS camera stays pinned at the 30 fps publish default (no
        // resolution/FPS selector on macOS yet) — behavior preserved.
        (30, 1)
    }

    fn device_id(&self) -> &str {
        self.requested_device_id.as_deref().unwrap_or("")
    }

    fn used_default_fallback(&self) -> bool {
        self.used_default_fallback
    }

    fn status_handle(&self) -> CameraStatus {
        CameraStatus::new(self.shared.clone())
    }
}

impl CameraCapture {
    /// Open the default camera and start delivering NV12 frames to
    /// `on_frame` (called on a private dispatch queue — must be cheap and
    /// thread-safe). BLOCKS for up to a few hundred ms (`startRunning`);
    /// call via `spawn_blocking` from async contexts.
    pub fn start(
        on_frame: impl Fn(CameraFrame) + Send + Sync + 'static,
    ) -> Result<Self, CameraError> {
        Self::start_with_device(None, on_frame)
    }

    /// Open the requested native AVFoundation camera, falling back to the
    /// default camera if it has disappeared since the user selected it.
    pub fn start_with_device(
        preferred_device_id: Option<&str>,
        on_frame: impl Fn(CameraFrame) + Send + Sync + 'static,
    ) -> Result<Self, CameraError> {
        // Loud, typed permission preflight (same reasoning as capture.rs's
        // Screen Recording preflight): without it AVFoundation just delivers
        // zero frames.
        let status = crate::permissions::check_camera();
        if status != "authorized" {
            return Err(CameraError::PermissionDenied(status));
        }

        let shared = Arc::new(DelegateShared {
            on_frame: Box::new(on_frame),
            stopped: AtomicBool::new(false),
            logged_format: AtomicU32::new(0),
            terminal_error: Mutex::new(None),
            frames_delivered: AtomicU64::new(0),
            dimensions: AtomicU64::new(0),
        });

        // Autorelease pool: several calls below return autoreleased objects
        // and this runs on a non-main thread with no ambient pool.
        autoreleasepool(|_| unsafe {
            let (device, used_default_fallback) = match preferred_device_id {
                Some(id) => {
                    let id_string = NSString::from_str(id);
                    let selected: *mut AnyObject = msg_send![
                        class!(AVCaptureDevice),
                        deviceWithUniqueID: &*id_string
                    ];
                    if should_fallback_to_default(preferred_device_id, selected.is_null() == false)
                    {
                        log::warn!(
                            "camera: preferred native device '{}' was not found -- falling back to default",
                            id
                        );
                        let default: *mut AnyObject = msg_send![
                            class!(AVCaptureDevice),
                            defaultDeviceWithMediaType: &*AVMediaTypeVideo
                        ];
                        (default, true)
                    } else {
                        (selected, false)
                    }
                }
                None => {
                    let default: *mut AnyObject = msg_send![
                        class!(AVCaptureDevice),
                        defaultDeviceWithMediaType: &*AVMediaTypeVideo
                    ];
                    (default, false)
                }
            };
            if device.is_null() {
                return Err(CameraError::NoCamera);
            }

            let mut ns_error: *mut AnyObject = null_mut();
            let input: *mut AnyObject = msg_send![
                class!(AVCaptureDeviceInput),
                deviceInputWithDevice: device,
                error: &mut ns_error
            ];
            if input.is_null() {
                return Err(CameraError::Configuration(format!(
                    "AVCaptureDeviceInput failed (NSError {:p})",
                    ns_error
                )));
            }

            let session: *mut AnyObject = msg_send![class!(AVCaptureSession), alloc];
            let session: *mut AnyObject = msg_send![session, init];
            if session.is_null() {
                return Err(CameraError::Configuration(
                    "AVCaptureSession init failed".into(),
                ));
            }

            let _: () = msg_send![session, beginConfiguration];

            // 720p if the device supports it, else 640x480 — a webcam tile
            // doesn't need more, and smaller frames keep the CPU copy cheap.
            for preset in [
                AVCaptureSessionPreset1280x720,
                AVCaptureSessionPreset640x480,
            ] {
                let ok: bool = msg_send![session, canSetSessionPreset: &*preset];
                if ok {
                    let _: () = msg_send![session, setSessionPreset: &*preset];
                    break;
                }
            }

            let can_input: bool = msg_send![session, canAddInput: input];
            if !can_input {
                let _: () = msg_send![session, release];
                return Err(CameraError::Configuration("canAddInput refused".into()));
            }
            let _: () = msg_send![session, addInput: input];

            let output: *mut AnyObject = msg_send![class!(AVCaptureVideoDataOutput), alloc];
            let output: *mut AnyObject = msg_send![output, init];

            // Pin NV12 video-range explicitly (see module doc).
            let fmt = NSNumber::new_u32(FMT_NV12_VIDEO_RANGE);
            let key: &NSString = &*kCVPixelBufferPixelFormatTypeKey;
            let settings: Retained<NSDictionary<NSString, NSNumber>> =
                NSDictionary::from_slices(&[key], &[&*fmt]);
            let _: () = msg_send![output, setVideoSettings: &*settings];
            let _: () = msg_send![output, setAlwaysDiscardsLateVideoFrames: true];

            let delegate = CameraDelegate::alloc().set_ivars(CameraDelegateIvars {
                shared: shared.clone(),
            });
            let delegate: Retained<CameraDelegate> = msg_send![super(delegate), init];

            let queue = dispatch_queue_create(c"petal.camera.capture".as_ptr(), null_mut());
            let _: () = msg_send![
                output,
                setSampleBufferDelegate: &*delegate,
                queue: queue
            ];

            let can_output: bool = msg_send![session, canAddOutput: output];
            if !can_output {
                let _: () = msg_send![output, release];
                let _: () = msg_send![session, release];
                dispatch_release(queue);
                return Err(CameraError::Configuration("canAddOutput refused".into()));
            }
            let _: () = msg_send![session, addOutput: output];
            // The session retains the output; drop our alloc/init +1.
            let _: () = msg_send![output, release];

            let _: () = msg_send![session, commitConfiguration];

            // Blocking (hundreds of ms) — documented in this fn's contract.
            let _: () = msg_send![session, startRunning];

            log::info!("camera: AVCaptureSession running (NV12 pinned, delegate on private queue)");

            Ok(CameraCapture {
                session,
                _delegate: delegate,
                queue,
                shared,
                requested_device_id: preferred_device_id.map(str::to_owned),
                used_default_fallback,
            })
        })
    }

    /// Stop the session and release the camera. Idempotent. BLOCKS
    /// (`stopRunning`) — call via `spawn_blocking` from async contexts.
    pub fn stop(&self) {
        if self.shared.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        unsafe {
            let _: () = msg_send![self.session, stopRunning];
        }
        log::info!("camera: AVCaptureSession stopped");
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            let _: () = msg_send![self.session, release];
            dispatch_release(self.queue);
        }
    }
}

/// Enumerate real AVFoundation video devices (the shared `list_devices`
/// provider). These ids are native uniqueIDs, deliberately not WebKit's
/// per-origin salted MediaDeviceInfo.deviceIds.
///
/// Uses `AVCaptureDeviceDiscoverySession`, NOT the older
/// `+[AVCaptureDevice devicesForMediaType:]` class method: that method is
/// unavailable on macOS 26.3.1, confirmed by a live crash (Objective-C
/// `NSInvalidArgumentException` -- "unrecognized selector sent to class") the
/// instant Settings opened and this command ran (shipped as part of 0.7.6,
/// hotfixed same-day). Deprecated Apple APIs are not a "probably still
/// works" bet -- verify liveness on the current OS, don't assume prior
/// deprecation-era behavior holds. The whole body also runs inside
/// `objc2::exception::catch` (the same defensive pattern `compositor.rs`
/// uses around other ObjC calls) so ANY future unrecognized-selector/ObjC
/// exception here degrades to an error response instead of aborting the
/// process -- this runs on every Settings-page open and must never crash
/// the app.
pub(super) fn list_devices() -> Result<Vec<CameraDeviceInfo>, CameraError> {
    let outcome = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
        autoreleasepool(|_| unsafe {
            let device_types: *mut AnyObject = msg_send![class!(NSMutableArray), array];
            for device_type in [
                "AVCaptureDeviceTypeBuiltInWideAngleCamera",
                "AVCaptureDeviceTypeExternal",
                "AVCaptureDeviceTypeContinuityCamera",
                "AVCaptureDeviceTypeDeskViewCamera",
            ] {
                let ns_type = NSString::from_str(device_type);
                let _: () = msg_send![device_types, addObject: &*ns_type];
            }
            let session: *mut AnyObject = msg_send![
                class!(AVCaptureDeviceDiscoverySession),
                discoverySessionWithDeviceTypes: device_types,
                mediaType: &*AVMediaTypeVideo,
                position: 0i64
            ];
            if session.is_null() {
                return Err(CameraError::Operation(
                    "camera discovery session unavailable".into(),
                ));
            }
            let devices: *mut AnyObject = msg_send![session, devices];
            if devices.is_null() {
                return Err(CameraError::Operation(
                    "camera devices unavailable".into(),
                ));
            }
            let count: usize = msg_send![devices, count];
            let mut result = Vec::with_capacity(count);
            for index in 0..count {
                let device: *mut AnyObject = msg_send![devices, objectAtIndex: index];
                if device.is_null() {
                    continue;
                }
                let id: Retained<NSString> = msg_send![device, uniqueID];
                let name: Retained<NSString> = msg_send![device, localizedName];
                result.push(CameraDeviceInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                });
            }
            Ok(result)
        })
    }));
    match outcome {
        Ok(result) => result,
        Err(exception) => {
            log::error!("camera: list_devices caught an Objective-C exception: {exception:?}");
            Err(CameraError::Operation(
                "camera device enumeration failed".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_preferred_native_id_falls_back_to_default() {
        assert!(should_fallback_to_default(Some("missing-native-id"), false));
        assert!(!should_fallback_to_default(Some("present-native-id"), true));
        assert!(!should_fallback_to_default(None, false));
    }
}
