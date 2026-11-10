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

    /// Result of starting one RTCVideoEncoderH264 (screenshare mode).
    #[derive(Debug, Clone, Copy)]
    struct VtH264EncoderProbe {
        /// `startEncodeWithSettings:` return code (0 = WEBRTC_VIDEO_CODEC_OK).
        start_result: i64,
        before: VtH264SessionCounters,
        after: VtH264SessionCounters,
    }

    unsafe extern "C++" {
        include!("livekit/vt_h264_encoder_spec.h");

        fn VtH264SessionCountersSnapshot() -> VtH264SessionCounters;

        /// Starts and releases a real RTCVideoEncoderH264 at `width`x`height`,
        /// returning the session counters around it.
        fn VtH264StartEncoderProbe(width: u32, height: u32) -> VtH264EncoderProbe;
    }
}
