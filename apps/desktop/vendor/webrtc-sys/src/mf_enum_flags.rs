// PETAL PATCH (#504): see PETAL_PATCH.md "Patch 6". Pure helpers, exposed so
// the desktop crate's `cargo test --lib` exercises the exact flag policy the
// Windows Media Foundation H.264 encoder factory enumerates with, on every
// host (the MF code itself only compiles on Windows).

#[cxx::bridge(namespace = "livekit_ffi")]
pub mod ffi {
    unsafe extern "C++" {
        include!("livekit/mf_enum_flags.h");

        /// MFTEnumEx flags for the MF H.264 encoder search. `hardware_only`
        /// must never include MFT_ENUM_FLAG_SYNCMFT.
        fn MfH264EncoderEnumFlags(hardware_only: bool) -> u32;

        /// True when an MFTEnumEx call with `flags` can return a synchronous
        /// (software) MFT.
        fn MfEnumIncludesSyncMfts(flags: u32) -> bool;
    }
}
