// PETAL PATCH (#410): see PETAL_PATCH.md "Patch: H.264 encoder level that fits
// the frame". Pure helpers, exposed so the desktop crate's `cargo test --lib`
// exercises the exact C++ the ObjC encoder factory runs.

#[cxx::bridge(namespace = "livekit_ffi")]
pub mod ffi {
    unsafe extern "C++" {
        include!("livekit/h264_encoder_level.h");

        /// The H.264 `profile-level-id` the macOS encoder is CREATED with for a
        /// negotiated one: same profile, level token that makes
        /// RTCVideoEncoderH264 request VideoToolbox AutoLevel and skip its
        /// level-derived frame-rate clamp. Unparsable input is returned as-is.
        fn EncoderH264ProfileLevelId(negotiated: &str) -> String;

        /// Upstream RTCVideoEncoderH264's level-derived frame-rate cap for
        /// `profile_level_id` at `width`x`height` (0 = no cap, VideoToolbox
        /// decides). Mirrors `GetMaxSampleRate(level) / (align16(w) * align16(h))`.
        fn H264EncoderMaxFrameRate(profile_level_id: &str, width: u32, height: u32) -> u32;
    }
}
