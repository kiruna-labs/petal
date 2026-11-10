// PETAL PATCH (#500): see PETAL_PATCH.md "Patch 7". Exposes the macOS
// VTCompressionSessionCreate override's counters, and a probe that starts the
// prebuilt RTCVideoEncoderH264 for real, so the desktop crate's
// `cargo test --lib` pins that no H.264 session is created with
// kVTVideoEncoderSpecification_EnableLowLatencyRateControl.

#[cxx::bridge(namespace = "livekit_ffi")]
pub mod ffi {
    /// Process-wide counts of H.264 VTCompressionSessionCreate calls.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct VtH264SessionCounters {
        /// H.264 sessions created through the override.
        sessions: u64,
        /// Of those, how many the caller asked for low-latency rate control.
        low_latency_requested: u64,
        /// Of those, how many were passed to VideoToolbox with the key still
        /// set. Must stay 0.
        low_latency_forwarded: u64,
        /// Creation calls that returned an error status.
        failed: u64,
    }

    /// What one RTCVideoEncoderH264 start (screenshare mode) did, observed
    /// on the calling thread only.
    #[derive(Debug, Clone, Copy)]
    struct VtH264EncoderProbe {
        /// `startEncodeWithSettings:` return code (0 = WEBRTC_VIDEO_CODEC_OK).
        /// Non-zero when no encoder is available (e.g. some VMs); the session
        /// creation is still attempted and observed.
        start_result: i64,
        /// H.264 VTCompressionSessionCreate calls made by that start.
        sessions: u64,
        /// The last such call asked for low-latency rate control.
        low_latency_requested: bool,
        /// The last such call reached VideoToolbox with the key still set.
        low_latency_forwarded: bool,
        /// That call's OSStatus.
        create_status: i32,
    }

    unsafe extern "C++" {
        include!("livekit/vt_h264_encoder_spec.h");

        fn VtH264SessionCountersSnapshot() -> VtH264SessionCounters;

        /// Starts and releases a real RTCVideoEncoderH264 at `width`x`height`
        /// and reports the session creation it made.
        fn VtH264StartEncoderProbe(width: u32, height: u32) -> VtH264EncoderProbe;
    }
}
