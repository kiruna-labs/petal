//! Non-macOS capture surface used while native capture backends are ported.
//!
//! The transport layer consumes these platform-neutral copied-frame shapes.
//! Native capture itself deliberately remains unavailable until a Windows
//! implementation is added.

use crate::video_color::VideoColorProfile;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

pub type CaptureBufferPool = Arc<Mutex<Vec<Vec<u8>>>>;

pub struct PooledFrameData {
    bytes: Vec<u8>,
}

impl PooledFrameData {
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl From<Vec<u8>> for PooledFrameData {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_vec(bytes)
    }
}

impl Deref for PooledFrameData {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

pub struct NativeCapturedPixelBuffer;

impl NativeCapturedPixelBuffer {
    pub(crate) fn copy_nv12_payload_with_pool(
        &self,
        _pool: Option<&CaptureBufferPool>,
    ) -> Result<CapturedFramePayload, CaptureError> {
        Err(CaptureError::UnsupportedPlatform)
    }
}

pub enum CapturedFramePayload {
    Bgra {
        data: PooledFrameData,
        bytes_per_row: usize,
    },
    Nv12 {
        y: PooledFrameData,
        y_stride: u32,
        uv: PooledFrameData,
        uv_stride: u32,
    },
    Native {
        pixel_buffer: NativeCapturedPixelBuffer,
    },
}

impl CapturedFramePayload {
    pub fn primary_plane(&self) -> Option<(&[u8], usize)> {
        match self {
            Self::Bgra {
                data,
                bytes_per_row,
            } => Some((data, *bytes_per_row)),
            Self::Nv12 { y, y_stride, .. } => Some((y, *y_stride as usize)),
            Self::Native { .. } => None,
        }
    }

    pub fn payload_kind(&self) -> &'static str {
        match self {
            Self::Bgra { .. } => "BGRA",
            Self::Nv12 { .. } => "NV12",
            Self::Native { .. } => "Native",
        }
    }
}

pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub payload: CapturedFramePayload,
    pub source_scale: f64,
    pub layout_validated: bool,
    pub color_profile: VideoColorProfile,
    pub sequence: u64,
    pub dirty_rect_count: usize,
    pub dirty_area_px: u64,
    pub dirty_rects_known: bool,
    pub lock_copy_ms: f64,
    pub region_generation: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("Screen Recording permission has not been granted")]
    PermissionDenied,
    #[error("window {0} not found (closed, or invalid id)")]
    WindowNotFound(u32),
    #[error("native screen capture is not implemented for this platform")]
    UnsupportedPlatform,
}
