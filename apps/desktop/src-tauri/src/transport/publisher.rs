//! Publish side: connect to a LiveKit room and publish a custom video track
//! fed by captured window frames (SPEC.md §4.1 capture -> §4.3 transport).
//!
//! ## Pixel format / conversion
//!
//! `NativeVideoSource::capture_frame` is generic over the frame buffer type.
//! The screen path publishes SCK's owned `420v` `CVPixelBuffer` as a libwebrtc
//! `NativeBuffer`, so VideoToolbox can consume the IOSurface directly. The #178
//! NV12->I420 path remains as the automatic fallback when native delivery is
//! disabled or fails; the older Apple-BGRA -> I420 converter remains for
//! synthetic/test payloads.
//!
//! **libyuv naming trap (issue #24, real user-visible bug):** libyuv
//! names formats by the WORD layout, Apple by the BYTE order. Apple's
//! `kCVPixelFormatType_32BGRA` (bytes B,G,R,A) is libyuv's **"ARGB"**;
//! libyuv's "BGRA" means bytes A,R,G,B.
//! Using `rs_BGRAToI420` here shifted every channel by one byte -- the
//! opaque alpha (0xFF) landed in blue, tinting every share strongly
//! blue/purple on all receivers. The correct call for Apple-BGRA input is
//! `rs_ARGBToI420`.
//!
//! ## VideoToolbox hardware encode
//!
//! `TrackPublishOptions.video_encoder = VideoEncoderBackend::VideoToolbox`
//! requests Apple's hardware H.264 encoder explicitly. This is NOT custom
//! FFI code we wrote -- it's a one-line preference passed into libwebrtc's
//! (Rust-bound) encoder factory selection, confirming SPEC.md's "VideoToolbox
//! HW encode" requirement is satisfied by the transport/SDK layer itself on
//! macOS, not something the M0 spike needed to hand-roll. We verify this
//! isn't just a silently-ignored preference by reading back
//! `RtcStats::OutboundRtp.outbound.encoder_implementation` after publish
//! starts (see `log_encoder_once` below) -- if VideoToolbox isn't actually
//! in use, that stats field will say so (e.g. "libvpx"/"OpenH264"/generic
//! names) rather than an Apple-specific string, and we log it either way
//! instead of assuming.
//!
//! The deeper #182 VideoToolbox knobs (`RealTime`,
//! `AllowFrameReordering=false`, `EnableLowLatencyRateControl`, GOP/IDR
//! interval) are inside libwebrtc's prebuilt VideoToolbox encoder. The pinned
//! public LiveKit/libwebrtc Rust APIs expose encoder backend selection,
//! bitrate/fps, frame metadata, keyframe/PLI counters, and encoder stats, but
//! not those VTCompressionSession properties or a force-keyframe/PLI method.
//! Do not vendor the SDK to reach them; keep the public surface and use
//! the project history as the scoped gap record.
//!
//! ## No in-band H.264 VUI/SPS color signaling (#47, accepted limitation)
//!
//! Petal signals capture color space (`VideoColorProfile`) out-of-band via
//! `PETAL_WINDOW_COLOR_PROFILES_METADATA_KEY` participant metadata, read by
//! our own compositor (`native_display.rs`). The H.264 bitstream itself
//! carries no VUI/SPS color-primaries/matrix flags: `livekit`/`webrtc-sys`
//! (pinned versions, see #133) expose no `set_color_space()` binding and no
//! VideoToolbox `kVTCompressionPropertyKey_Color*` hook -- the encoder lives
//! inside the prebuilt libwebrtc binary with no Rust surface for this. There
//! is no pre-encoded publish path or encoded-frame transform hook to patch
//! the SPS NAL manually either. Since every real receiver in this product is
//! Petal's own compositor (no egress/recording/transcode exists), this is a
//! theoretical gap, not a live correctness bug -- closing it for real would
//! require forking/rebuilding libwebrtc itself.

use crate::sync_ext::MutexExt;
use crate::video_color::{self, VideoColorProfile};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
#[cfg(target_os = "macos")]
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use livekit::options::{
    H264ProfilePreference, TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoPreset,
};
use livekit::prelude::*;
#[cfg(target_os = "macos")]
use livekit::webrtc::video_frame::native::NativeBuffer;
use livekit::webrtc::video_frame::{FrameMetadata, I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};

use crate::capture::{CaptureBufferPool, CapturedFrame, CapturedFramePayload};

const I420_BUFFER_POOL_LIMIT: usize = 3;
/// A captured window's size must stay at ONE value for this long before the
/// sender re-anchors the published size to it (one encoder recreation per
/// resize gesture). Shorter than this and a slow drag would re-create the
/// encoder mid-gesture (the churn that froze the NVIDIA MF encoder).
const REANCHOR_SETTLE_DWELL: std::time::Duration = std::time::Duration::from_millis(2000);
/// A camera's resolution is fixed for a track's lifetime, so a size mismatch is a
/// genuine anomaly rather than an in-progress resize, and dropping a few frames is
/// the right response to a one-off glitch. Past this grace it must recover instead:
/// #866 saw 2190 consecutive drops over 73s after the camera restarted at a new size
/// while the published track kept the old one. Whichever limit is hit first ends it.
const CAMERA_SIZE_MISMATCH_GRACE_FRAMES: u32 = 30;
const CAMERA_SIZE_MISMATCH_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
/// At most one re-anchor per this interval (#841's lesson: an unbounded republish
/// path took a share down for 73s in the field -- do not create a second one).
const CAMERA_REANCHOR_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);
/// After this many re-anchors inside `CAMERA_REANCHOR_FLAP_WINDOW`, stop re-anchoring
/// for the track's lifetime and settle on letterboxing, so a flapping source can never
/// drive an endless re-anchor/encoder-recreation loop.
const CAMERA_REANCHOR_MAX_PER_WINDOW: u32 = 2;
const CAMERA_REANCHOR_FLAP_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);
const NATIVE_CAPTURE_FRAME_STALL_THRESHOLD_MS: f64 = 50.0;
const NATIVE_CAPTURE_FRAME_STALL_THRESHOLD: std::time::Duration =
    std::time::Duration::from_millis(50);
// Keep the warm-up threshold identical to steady state: first frames are more prone to
// scheduler/encoder warm-up delays, so three strikes in one second filters that noise
// without making a genuinely slow steady-state path harder to detect.
const NATIVE_ZERO_COPY_SLOW_FRAME_STRIKES: usize = 3;
const NATIVE_ZERO_COPY_SLOW_FRAME_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);
const NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF: std::time::Duration =
    std::time::Duration::from_secs(1);
const NATIVE_ZERO_COPY_REPROBE_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(8);
const PETAL_FORCE_CODEC_ENV: &str = "PETAL_FORCE_CODEC";
// Debug-only kill switch (see native_publish_disabled_by_env): a shipped
// build must not let the environment force the slower NV12->I420 fallback.
#[cfg(debug_assertions)]
const PETAL_DISABLE_NATIVE_PUBLISH_ENV: &str = "PETAL_DISABLE_NATIVE_PUBLISH";
const PETAL_SHARE_LADDER_ENV: &str = "PETAL_SHARE_LADDER";
static SOFTWARE_ENCODER_WARNED: AtomicBool = AtomicBool::new(false);
#[cfg(debug_assertions)]
static NATIVE_PUBLISH_ENV_WARNED: AtomicBool = AtomicBool::new(false);

type EncoderFallbackRecoveryFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// One deliberately-scoped recovery action for a window publication created
/// by the post-wake capture restart. Ordinary starts and every replacement
/// publication use `EncoderPublishOrigin::Ordinary`, so a software
/// encoder cannot turn into an unbounded republish loop (#769).
pub(crate) struct PostWakeEncoderFallbackRecovery {
    delay: std::time::Duration,
    action: Box<dyn FnOnce() -> EncoderFallbackRecoveryFuture + Send>,
}

impl PostWakeEncoderFallbackRecovery {
    pub(crate) fn new(
        delay: std::time::Duration,
        action: impl FnOnce() -> EncoderFallbackRecoveryFuture + Send + 'static,
    ) -> Self {
        Self {
            delay,
            action: Box::new(action),
        }
    }

    async fn run(self) {
        tokio::time::sleep(self.delay).await;
        (self.action)().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncoderPublishOrigin {
    Ordinary,
    PostWakeRestart,
}

struct EncoderObservation {
    implementation: String,
    power_efficient: bool,
    h264_profile: String,
    h264_fmtp: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SharedSourceKind {
    Window,
    Display,
    DisplayRegion,
}

impl SharedSourceKind {
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Display => "display",
            Self::DisplayRegion => "display_region",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "window" => Some(Self::Window),
            "display" | "screen" => Some(Self::Display),
            "display_region" | "region" => Some(Self::DisplayRegion),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RoomConnectionError {
    #[error("room connect failed: {0}")]
    Connect(#[from] livekit::RoomError),
    #[error("invalid video publish configuration: {0}")]
    InvalidVideoConfig(String),
}

/// Encoding quality tier for a published window track (SPEC.md §4.3
/// focus-weighted share cap: "only the focused shared window streams at
/// full fps/resolution; unfocused shares fall to a low-fps, lower-res
/// glanceable layer"). See `session.rs`'s `focus` module doc comment for the
/// full policy this feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareQuality {
    /// The focused share: full 30fps (#907/#383: lowered from 60 -- see
    /// `capture_fps` for why) with a resolution- and budget-based bitrate
    /// ceiling.
    Full,
    /// An unfocused, "glanceable" share: enough to read a paused screen or
    /// notice motion, not to comfortably watch video or follow fast typing.
    /// 4fps -- roughly a 7.5x frame-rate cut
    /// from `Full`, chosen to be obviously cheap without going so low the
    /// window looks broken (SPEC.md explicitly wants "glanceable", not
    /// "frozen").
    Reduced,
}

/// Capture-size cap selected for a shared window. These are maximum long-edge
/// caps only; they never upscale a smaller source.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureResolution {
    /// Match the source's native size, clamped to the H.264 guardrail.
    #[default]
    Auto,
    #[serde(rename = "p1080")]
    P1080,
    #[serde(rename = "p1440")]
    P1440,
    #[serde(rename = "uhd4k")]
    Uhd4k,
}

const CAMERA_MAX_BITRATE_BPS: u64 = 2_500_000;
const CAMERA_MAX_FRAMERATE_FPS: f64 = 30.0;
const FULL_SIMULCAST_HALF_MIN_BITRATE_BPS: u64 = 1_250_000;
const FULL_SIMULCAST_HALF_BASE_PIXELS: u64 = 960 * 540;
const FULL_SIMULCAST_HALF_MAX_BITRATE_BPS: u64 = 6_000_000;
const FULL_SIMULCAST_HALF_MAX_FRAMERATE_FPS: f64 = 30.0;
const FULL_SIMULCAST_QUARTER_MIN_BITRATE_BPS: u64 = 400_000;
const FULL_SIMULCAST_QUARTER_MAX_BITRATE_BPS: u64 = 2_500_000;
// Keep the quarter layer useful for thumbnails and bandwidth-constrained
// receivers without spending the full interaction cadence on its smallest
// spatial rung. The half layer remains capped at 30fps; the source/full
// layer also runs at 30fps as of #907/#383 (was 60), so this quarter rung is
// now the only one below the ladder's shared 30fps cadence.
const FULL_SIMULCAST_QUARTER_MAX_FRAMERATE_FPS: f64 = 15.0;
// Top (h/f) full-share rung bitrate: pixel-scaled,
// h_bps = 0.18 bits-per-pixel-per-frame * pixels * max_framerate, clamped to
// [4, 16] Mbps before budgeting (see `budgeted_top_bitrate` below). The
// pre-2026-08-06 bucket ladder (4/8/12/18 Mbps by pixel ranges) starved
// large high-motion shares — a 1708x732 window is only 1.25MP and fell in
// the "<=1.5MP -> 4Mbps" bucket, tripping webrtc's quality scaler (high QP
// -> fps dropped to 3-18; A11 + mftprobe measured the MFT sustaining 622fps
// at that size, so the GPU was never the limit).
//
// #907: this formula was Windows-only from 2026-08-06 to 2026-09-02 behind a
// "do not change macOS" note. That note traced back to `fcbfa4f4`, a
// Windows-focused PR where the split rode along as an unrelated blast-radius
// guard — not a product decision — and the user lifted it explicitly once
// that was found. macOS now shares this exact formula instead of the old
// flat pixel-bucket table; the two platforms take one code path so they
// cannot drift apart again. On its own this formula would have made #907's
// reported incident WORSE (a 1920x1080 top rung asks 11.2 Mbps at 30fps here
// vs the old flat bucket's 8 Mbps) -- it fixes a different, real problem
// (small shares starved at a flat 4 Mbps floor). `budgeted_top_bitrate`
// below is what keeps the two rungs' COMBINED ask in check; this raw formula
// is only ever a per-layer upper bound feeding into it, never the final
// published ceiling for a simulcast share.
// 0.18 (raised from 0.13 after the live encoder-stats diagnostic): a 1215x719
// share was encoding at ~3.5-4.5Mbps with avg QP 26 and limitation=None —
// 'readable, not crisp' small text. The archive's crispness C2 verdict is
// that the bitrate ceiling is the lever for QP ("the cheap proxy for QP too
// high is simply C2's bigger bitrate ceiling"), so give the rate controller
// ~40% more headroom to spend on text sharpness.
const FULL_SIMULCAST_TOP_BITS_PER_PIXEL_FRAME_NUM: u64 = 18; // 0.18 = 18/100
const FULL_SIMULCAST_TOP_BITS_PER_PIXEL_FRAME_DEN: u64 = 100;
const FULL_SIMULCAST_TOP_MIN_BITRATE_BPS: u64 = 4_000_000;
const FULL_SIMULCAST_TOP_MAX_BITRATE_BPS: u64 = 16_000_000;
/// #907: total ask ceiling for a full share's combined simulcast layers
/// (all lower rungs' `max_bitrate` plus the top rung's, after budgeting).
/// The field incident's ladder asked 10.8 Mbps (2.8 + 8.0) on a link that
/// carried 2.6 Mbps; independently-computed per-layer ceilings had no
/// awareness of each other at all.
///
/// 8 Mbps, not 6: an adversarial review (counselors #907, two independent
/// models) measured that a tighter 6 Mbps budget, combined with an earlier
/// version of this function's "never cap the top rung below a lower rung's
/// own ceiling" floor, produced a WORSE regression than the bug it fixed --
/// see `budgeted_top_bitrate`'s doc comment for the full accounting. 8 Mbps
/// is chosen so that, once that floor is gone, ordinary two-rung ladders
/// converge to exactly this number rather than being squeezed by the
/// absolute floor below, AND so the small-share case this formula's raw bpp
/// scaling was written to rescue (a 1708x732 share, previously starved at a
/// flat 4 Mbps bucket) keeps meaningfully more headroom than that 4 Mbps
/// floor (4.3 Mbps at 6 Mbps budget vs 6.3 Mbps at 8 Mbps budget) instead of
/// being squeezed back down toward the exact number this codebase already
/// diagnosed as insufficient (`publisher.rs`'s `FULL_SIMULCAST_TOP_BITS_PER_PIXEL_FRAME_NUM`
/// doc comment).
///
/// This is a per-layer ceiling HINT fed to the congestion controller, not a
/// guarantee of delivery, and for the exact field-reported 1080p/TwoRung
/// incident it is close to cosmetic: the lower rung alone (2.8125 Mbps)
/// already exceeds that day's measured 2.58 Mbps link and is funded first
/// regardless of what this budget sets the top rung's ceiling to -- shrinking
/// the top rung's ASK does not change the allocator's funding PRIORITY. What
/// actually recovers a link too constrained even for this budget is #907
/// steps 2/3 (the sender starvation guard and the receiver's rung
/// downgrade), not this number. Do not present this budget alone as "the
/// fix" for the reported incident in future work on this area.
const FULL_SIMULCAST_TOTAL_BUDGET_BPS: u64 = 8_000_000;
/// Absolute floor for the top rung once the total budget above has been
/// applied, for the case a single lower rung's own ceiling already consumes
/// the whole budget (e.g. a very large/4K source, where the lower rung alone
/// can hit `FULL_SIMULCAST_HALF_MAX_BITRATE_BPS`). Deliberately lower than
/// `FULL_SIMULCAST_TOP_MIN_BITRATE_BPS` (the raw formula's own pre-budget
/// floor): that 4 Mbps floor assumes an unconstrained ask, but a budgeted top
/// rung sharing a tight total with a healthy lower rung must still be
/// allowed to ask for less than that.
///
/// This is the ONLY floor `budgeted_top_bitrate` applies -- an earlier
/// version also floored the top rung at "never below the largest lower
/// rung's own ceiling," which sounds safer but is not: at 4K on the `Legacy`
/// ladder that floor forced the total ask to 10.625 Mbps (vs an 6 Mbps
/// budget at the time), and on the shipped default `TwoRung` ladder it forced
/// a 4K share to 12 Mbps -- a full 2x the intended budget, materially WORSE
/// than a merely-suboptimal top/lower ordering. Removed per adversarial
/// review (counselors #907): a two-rung ladder with a fixed total budget
/// cannot simultaneously guarantee "top >= every lower rung" AND "total <=
/// budget" once a single lower rung's own resolution-scaled ceiling meets or
/// exceeds the budget -- something has to give, and blowing the total budget
/// is the worse failure mode of the two, especially now that the runtime
/// starvation guards (steps 2/3) -- not a static ceiling ordering -- are what
/// actually protects a viewer from a badly-funded top rung. Accepted
/// consequence: at very large (4K-class) source sizes, the nominal top rung
/// can end up with a SMALLER configured ceiling than a lower rung. This is a
/// known, deliberate limitation for that size class, not an oversight.
const FULL_SIMULCAST_TOP_BUDGETED_MIN_BITRATE_BPS: u64 = 1_500_000;
pub const VIDEO_TOOLBOX_H264_MAX_LONG_EDGE: u32 = 4096;

/// Full-window-share simulcast layouts. `TwoRung` is the default: measured 89.7ms p95
/// end-to-end, below the 100ms target, with the least encoder work.
/// Do not accept `default`; silently remapping it reports the wrong ladder (#613).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullShareSimulcastLadder {
    Legacy,
    /// #613 measurement-only: `Legacy` with ONLY the bottom rung's framerate
    /// cap lifted 15->30fps. Never change any other field -- it exists to
    /// isolate bottom-rung cadence from bottom-rung size, and a second
    /// difference makes its comparison against `Legacy` uninterpretable.
    LegacyBottom30,
    Raised,
    TwoRung,
    TwoRungHalf,
}

impl FullShareSimulcastLadder {
    fn from_env() -> Result<Self, RoomConnectionError> {
        match std::env::var(PETAL_SHARE_LADDER_ENV) {
            Ok(value) => Self::from_env_value(&value),
            Err(std::env::VarError::NotPresent) => Ok(Self::TwoRung),
            Err(std::env::VarError::NotUnicode(_)) => Err(RoomConnectionError::InvalidVideoConfig(
                format!("{PETAL_SHARE_LADDER_ENV} must be unset, legacy, legacy-bottom30, raised, two-rung, or two-rung-half"),
            )),
        }
    }

    fn from_env_value(value: &str) -> Result<Self, RoomConnectionError> {
        match value.trim() {
            "legacy" => Ok(Self::Legacy),
            "legacy-bottom30" => Ok(Self::LegacyBottom30),
            "raised" => Ok(Self::Raised),
            "two-rung" => Ok(Self::TwoRung),
            "two-rung-half" => Ok(Self::TwoRungHalf),
            "default" => Err(RoomConnectionError::InvalidVideoConfig(format!(
                "{PETAL_SHARE_LADDER_ENV}=default is no longer accepted; pick legacy or two-rung explicitly"
            ))),
            _ => Err(RoomConnectionError::InvalidVideoConfig(format!(
                "unsupported {PETAL_SHARE_LADDER_ENV}={value:?}; expected unset, legacy, legacy-bottom30, raised, two-rung, or two-rung-half"
            ))),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::LegacyBottom30 => "legacy-bottom30",
            Self::Raised => "raised",
            Self::TwoRung => "two-rung",
            Self::TwoRungHalf => "two-rung-half",
        }
    }

    const fn lower_rids(self) -> &'static [&'static str] {
        match self {
            Self::Legacy | Self::LegacyBottom30 | Self::Raised => &["q", "h"],
            Self::TwoRung | Self::TwoRungHalf => &["q"],
        }
    }

    const fn top_rid(self) -> &'static str {
        match self {
            Self::Legacy | Self::LegacyBottom30 | Self::Raised => "f",
            Self::TwoRung | Self::TwoRungHalf => "h",
        }
    }
}

impl CaptureResolution {
    pub const fn explicit_long_edge_cap(self) -> Option<u32> {
        match self {
            Self::Auto => None,
            Self::P1080 => Some(1920),
            Self::P1440 => Some(2560),
            Self::Uhd4k => Some(3840),
        }
    }

    pub const fn long_edge_cap_for_native(self, native_long_edge: u32) -> u32 {
        match self.explicit_long_edge_cap() {
            Some(cap) if cap < VIDEO_TOOLBOX_H264_MAX_LONG_EDGE => cap,
            Some(_) => VIDEO_TOOLBOX_H264_MAX_LONG_EDGE,
            None if native_long_edge < VIDEO_TOOLBOX_H264_MAX_LONG_EDGE => native_long_edge,
            None => VIDEO_TOOLBOX_H264_MAX_LONG_EDGE,
        }
    }
}

impl ShareQuality {
    pub const fn capture_fps(self) -> u32 {
        match self {
            // #907/#383: 30fps is adequate for this product's content (small
            // engineering syncs, text/UI-heavy shares) and the prior 60fps
            // top-rung target was cosmetic, not a real perceptual gap --
            // while directly inflating the top rung's bitrate ask (a large
            // share of the field-measured 10.8 Mbps two-rung ask that
            // starved the top rung, see #907). Kept at 30 rather than
            // Reduced's 4 so a focused share still feels responsive.
            Self::Full => 30,
            Self::Reduced => 4,
        }
    }

    fn video_encoding(self, width: u32, height: u32) -> livekit::options::VideoEncoding {
        let pixels = u64::from(width) * u64::from(height);
        // #907: one formula on both platforms (previously Windows-only
        // behind a "do not change macOS" note that traced to an unrelated
        // Windows PR's blast-radius guard, not a product decision -- lifted
        // by explicit user sign-off). Pixel-scaled bits-per-pixel-per-frame,
        // clamped to a per-layer range; see the constants' doc comments for
        // the full history. This is a RAW per-layer ceiling only --
        // `budgeted_top_bitrate` (called from the two real call sites,
        // `window_publish_options_for_region` and `layer_parameters`) is
        // what keeps the combined two-rung ask in check.
        let full_bitrate = {
            let bps = (FULL_SIMULCAST_TOP_BITS_PER_PIXEL_FRAME_NUM
                .saturating_mul(pixels)
                .saturating_mul(u64::from(self.capture_fps())))
                / FULL_SIMULCAST_TOP_BITS_PER_PIXEL_FRAME_DEN;
            bps.clamp(
                FULL_SIMULCAST_TOP_MIN_BITRATE_BPS,
                FULL_SIMULCAST_TOP_MAX_BITRATE_BPS,
            )
        };
        match self {
            Self::Full => livekit::options::VideoEncoding {
                max_bitrate: full_bitrate,
                max_framerate: self.capture_fps() as f64,
            },
            Self::Reduced => livekit::options::VideoEncoding {
                max_bitrate: (full_bitrate / 2).max(2_000_000),
                max_framerate: self.capture_fps() as f64,
            },
        }
    }

    fn layer_parameters(
        self,
        width: u32,
        height: u32,
        ladder: FullShareSimulcastLadder,
    ) -> Vec<livekit::prelude::PublishingLayerParameters> {
        let layers = full_share_simulcast_layers(width, height, ladder);
        let reduced = self == Self::Reduced;
        let mut updates = Vec::with_capacity(layers.len() + 1);
        let mut lower_rungs_bitrate_bps: u64 = 0;
        for (rid, preset) in ladder.lower_rids().iter().zip(layers.iter()) {
            let max_bitrate = if reduced {
                (preset.encoding.max_bitrate / 2).max(FULL_SIMULCAST_QUARTER_MIN_BITRATE_BPS)
            } else {
                preset.encoding.max_bitrate
            };
            lower_rungs_bitrate_bps = lower_rungs_bitrate_bps.saturating_add(max_bitrate);
            updates.push(livekit::prelude::PublishingLayerParameters {
                rid: (*rid).to_string(),
                max_bitrate,
                max_framerate: if reduced {
                    self.capture_fps() as f64
                } else {
                    preset.encoding.max_framerate
                },
            });
        }
        // #907: this live layer-parameter update path (focus-quality
        // switches without a republish) must apply the SAME total-budget
        // clamp as the initial publish below, or a quality switch would
        // silently re-widen the top rung back to its raw, unbudgeted
        // ceiling.
        let top = self.video_encoding(width, height);
        updates.push(livekit::prelude::PublishingLayerParameters {
            rid: ladder.top_rid().to_string(),
            max_bitrate: budgeted_top_bitrate(top.max_bitrate, lower_rungs_bitrate_bps),
            max_framerate: top.max_framerate,
        });
        updates
    }
}

/// #907: cap the top simulcast rung's bitrate so the ladder's COMBINED ask
/// (this plus `lower_rungs_bitrate_bps`) stays inside
/// `FULL_SIMULCAST_TOTAL_BUDGET_BPS` (plus, in the worst case, the small
/// absolute floor below), rather than the two being computed fully
/// independently the way they were before this issue (2.8 + 8.0 = 10.8 Mbps
/// on a link that carried 2.6). Never raises `raw_top_bitrate_bps` -- only
/// ever reduces it, down to `FULL_SIMULCAST_TOP_BUDGETED_MIN_BITRATE_BPS` at
/// most, so a naturally small top rung (the #907-adjacent small-share case
/// this same formula was written for) is never inflated by slack in the
/// budget. This is a per-layer ceiling hint fed to LiveKit/webrtc's own
/// congestion controller, not a delivery guarantee -- a link too constrained
/// even for the budgeted total still needs the starvation guard and receiver
/// downgrade (#907 steps 2/3) to recover, and for the exact field-reported
/// case this budget's own effect is close to cosmetic -- see
/// `FULL_SIMULCAST_TOTAL_BUDGET_BPS`'s doc comment.
///
/// Deliberately does NOT also floor at "never below the largest lower rung's
/// own ceiling" -- an earlier version of this function did, and an
/// adversarial review (counselors #907) measured that this made the total
/// budget meaningless exactly when it mattered most: at 4K on the shipped
/// default `TwoRung` ladder the lower rung alone can reach
/// `FULL_SIMULCAST_HALF_MAX_BITRATE_BPS`, and that floor then forced the top
/// rung back up to match it -- doubling the total ask to 2x the intended
/// budget. A two-rung ladder with one fixed total budget cannot guarantee
/// BOTH "top never worse than a lower rung" AND "total stays inside budget"
/// once a single lower rung's own ceiling meets or exceeds the budget; this
/// function chooses to honor the total-budget invariant and accepts that, at
/// very large (4K-class) source sizes, the nominal top rung's configured
/// ceiling can end up smaller than a lower rung's. See #907's tests for the
/// exact before/after numbers this produced.
fn budgeted_top_bitrate(raw_top_bitrate_bps: u64, lower_rungs_bitrate_bps: u64) -> u64 {
    let available = FULL_SIMULCAST_TOTAL_BUDGET_BPS.saturating_sub(lower_rungs_bitrate_bps);
    let floor = available.max(FULL_SIMULCAST_TOP_BUDGETED_MIN_BITRATE_BPS);
    raw_top_bitrate_bps.min(floor)
}

/// A connected room, capable of publishing zero or more independent video
/// tracks (one per shared window -- see [`RoomConnection::publish_window`]).
///
/// Split from the M0 spike's original `connect_and_publish` (which did both
/// in one call, since the spike only ever published a single track) so the
/// in-app path (`session.rs`) can share ONE room connection across multiple
/// concurrently-shared windows per SPEC.md §4.3, publishing/unpublishing
/// individual tracks as windows are toggled on/off without reconnecting.
pub struct RoomConnection<R = Arc<Room>> {
    room: R,
    /// Connect-time fanout receivers. The SDK receiver returned by
    /// `Room::connect` is the sole registration and is fanned out before the
    /// join tail starts, so neither consumer can miss an initial event
    /// (#357/#584).
    compositor_events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>>>,
    resilience_events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>>>,
    /// Local shared-window metadata mirrored into LiveKit participant metadata.
    /// Track publish options in livekit 0.7.49 do not carry arbitrary
    /// per-track metadata, so this small participant-level object is the
    /// reliable late-joiner path for remote headers.
    share_metadata: Mutex<ShareMetadata>,
}

/// The `Room::connect` receiver is registered before the SDK can dispatch any
/// join-time event. Fan it out immediately, before any join-tail work, so the
/// compositor and resilience consumers have one ordered, zero-gap source.
/// A dropped consumer is removed on its next send; once both are gone this
/// task exits and retains no unconsumed event queue.
fn start_connect_event_fanout(
    source: tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>,
    compositor_tx: tokio::sync::mpsc::UnboundedSender<livekit::RoomEvent>,
    resilience_tx: tokio::sync::mpsc::UnboundedSender<livekit::RoomEvent>,
) {
    tauri::async_runtime::spawn(fanout_connect_events(source, compositor_tx, resilience_tx));
}

async fn fanout_connect_events(
    mut source: tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>,
    compositor_tx: tokio::sync::mpsc::UnboundedSender<livekit::RoomEvent>,
    resilience_tx: tokio::sync::mpsc::UnboundedSender<livekit::RoomEvent>,
) {
    let mut compositor_tx = Some(compositor_tx);
    let mut resilience_tx = Some(resilience_tx);
    loop {
        if compositor_tx.is_none() && resilience_tx.is_none() {
            break;
        }
        // Clone the sender handles for the close futures so the selected
        // branch can clear the owned slot without borrowing it across the
        // `select!` await point.
        let compositor_closed = compositor_tx.clone();
        let resilience_closed = resilience_tx.clone();
        tokio::select! {
            event = source.recv() => {
                let Some(event) = event else {
                    break;
                };
                if let Some(sender) = &compositor_tx {
                    if sender.send(event.clone()).is_err() {
                        compositor_tx = None;
                    }
                }
                if let Some(sender) = &resilience_tx {
                    if sender.send(event).is_err() {
                        resilience_tx = None;
                    }
                }
            }
            _ = async move { compositor_closed.expect("guarded compositor sender").closed().await }, if compositor_tx.is_some() => {
                compositor_tx = None;
            }
            _ = async move { resilience_closed.expect("guarded resilience sender").closed().await }, if resilience_tx.is_some() => {
                resilience_tx = None;
            }
        }
    }
}

impl<R> RoomConnection<R> {
    fn with_connect_event_source(
        room: R,
        events: tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>,
    ) -> Self {
        let (compositor_tx, compositor_events) = tokio::sync::mpsc::unbounded_channel();
        let (resilience_tx, resilience_events) = tokio::sync::mpsc::unbounded_channel();
        start_connect_event_fanout(events, compositor_tx, resilience_tx);
        Self {
            room,
            compositor_events: Mutex::new(Some(compositor_events)),
            resilience_events: Mutex::new(Some(resilience_events)),
            share_metadata: Mutex::new(ShareMetadata::default()),
        }
    }

    /// Take the event receiver registered during [`Room::connect`]. The app's
    /// compositor feed must consume this receiver rather than registering a
    /// new one after the join sequence, because LiveKit does not replay its
    /// initial events to late subscribers (#357).
    ///
    /// Public so the late-joiner harness can measure the same receiver the
    /// app consumes, rather than a re-registered stand-in that would not
    /// carry the connect-time events at all.
    pub fn take_compositor_events(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>> {
        self.compositor_events.lock_unpoisoned().take()
    }

    /// Take the resilience branch of the connect-time fanout. This is a
    /// one-shot ownership transfer; registering another SDK subscription here
    /// would reintroduce the join-time event gap fixed by #584.
    pub fn take_resilience_events(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>> {
        self.resilience_events.lock_unpoisoned().take()
    }

    /// Dispose of the resilience branch when a platform session does not run
    /// the resilience watcher. Keeping this explicit prevents the fanout from
    /// retaining an unconsumed unbounded queue for that room's lifetime.
    pub fn discard_resilience_events(&self) {
        drop(self.resilience_events.lock_unpoisoned().take());
    }

    /// Explicitly dispose of the connect-time event receiver for publisher-
    /// only paths that do not start a compositor feed. Without this, the
    /// unbounded receiver would retain every room event for the connection's
    /// lifetime (#357).
    pub fn discard_compositor_events(&self) {
        drop(self.compositor_events.lock_unpoisoned().take());
        self.discard_resilience_events();
    }
}

#[cfg(test)]
impl RoomConnection<()> {
    pub(crate) fn from_connect_event_source_for_test(
        events: tokio::sync::mpsc::UnboundedReceiver<livekit::RoomEvent>,
    ) -> Self {
        Self::with_connect_event_source((), events)
    }
}

/// One published video track fed by `push_frame`, independent lifecycle from
/// any other track published on the same [`RoomConnection`]'s room (SPEC.md
/// §4.1's "independent lifecycle" per shared window, now also true on the
/// publish side, not just capture).
pub struct PublishedTrack {
    room: Arc<Room>,
    rtc_source: NativeVideoSource,
    track: LocalVideoTrack,
    /// Published/encoded size. Kept constant while a resize is in progress
    /// (letterbox) so webrtc never re-creates the encoder; re-anchored to
    /// the real window size once it settles (~2s) — one encoder recreation
    /// per gesture.
    // #907 review finding 4: `Arc`-wrapped (not a plain `AtomicU32`/`Mutex`)
    // so the starvation-guard background task (`log_window_share_encoder_stats`)
    // can hold its OWN clone and read the CURRENT published size fresh on
    // every poll -- the same authoritative source `set_quality` below uses --
    // instead of a value snapshotted once at guard-construction time that
    // would go stale across a resize or a Full/Reduced quality flip. Every
    // existing `self.published_width`/`self.published_height` call site
    // (`.load()`, `&self.published_width` passed by reference) keeps
    // compiling unchanged via `Arc<T>`'s `Deref<Target = T>`.
    published_width: Arc<std::sync::atomic::AtomicU32>,
    published_height: Arc<std::sync::atomic::AtomicU32>,
    /// Frame rate the track was published at, so a reconnect republish
    /// (`republish_camera_after_reconnect`) rebuilds the identical encoding.
    /// Meaningful only for camera tracks; window shares derive their fps from
    /// `ShareQuality` and store 0.0 (unused).
    published_frame_rate: f64,
    /// (mismatched captured size, first-seen instant) while a resize has not
    /// yet settled; cleared when the captured size matches the published size.
    resize_settle: Mutex<Option<((u32, u32), std::time::Instant)>>,
    /// #866 camera size-mismatch recovery. Camera-only: window shares letterbox in
    /// `push_i420_letterboxed` and re-anchor via the session's own resize debounce.
    camera_size_recovery: Mutex<CameraSizeRecovery>,
    simulcast_ladder: FullShareSimulcastLadder,
    // #907 review finding 4: same `Arc` reasoning as `published_width` above
    // -- the starvation guard reads the CURRENT quality fresh every poll
    // rather than a stale snapshot from whenever it was constructed.
    quality: Arc<Mutex<ShareQuality>>,
    frame_seq: std::sync::atomic::AtomicU32,
    i420_pool: Mutex<I420BufferPool>,
    nv12_scratch: Mutex<Nv12Scratch>,
    native_fallback_pool: CaptureBufferPool,
    native_publish_disabled_by_env: bool,
    native_zero_copy_latch: Mutex<NativeZeroCopyLatch>,
    push_drop_streak: Mutex<crate::logging::DropStreakDetector>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PublishedFrameTiming {
    pub convert_ms: f64,
    pub capture_frame_return_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CameraPushVerdict {
    /// Sizes match, or recovery has engaged -- hand the frame to the normal
    /// conversion path, which letterboxes to the published size when needed.
    Push,
    /// Inside the brief grace window for a one-off anomaly.
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CameraRecoveryAction {
    Reanchor,
    Letterbox,
}

#[derive(Debug)]
struct CameraPushDecision {
    verdict: CameraPushVerdict,
    /// `Some` exactly ONCE per recovery episode, never once per frame. Carries the
    /// chosen `CameraRecoveryAction` as a closed tag, so tests read the action from
    /// here rather than from a field the shipped dispatcher would never look at.
    diagnostic: Option<crate::logging::SentryDiagnosticEvent>,
}

#[derive(Debug, Clone, Copy)]
struct CameraSizeMismatchRun {
    size: (u32, u32),
    first_seen: std::time::Instant,
    dropped: u32,
}

/// #866 camera size-mismatch recovery state. Owned by one `PublishedTrack`.
#[derive(Debug, Default)]
struct CameraSizeRecovery {
    /// The mismatched size currently being counted, when it was first seen, and how
    /// many consecutive frames have been dropped for it.
    mismatch: Option<CameraSizeMismatchRun>,
    /// True once the grace elapsed and recovery engaged; cleared when a frame finally
    /// arrives at the published size. Gates the one-per-episode diagnostic and warning.
    recovering: bool,
    /// (window start, re-anchors inside it) for the flap guard.
    reanchor_window: Option<(std::time::Instant, u32)>,
    last_reanchor_at: Option<std::time::Instant>,
    /// Sticky for the track's lifetime once the flap guard trips.
    letterbox_locked: bool,
}

/// Bucket a frame size for the closed diagnostic schema. No caller may send raw
/// dimensions off-device (#550), so classify before crossing that boundary.
fn camera_geometry_bucket(width: u32, height: u32) -> crate::logging::GeometryBucket {
    use crate::logging::GeometryBucket;
    if width == 0 || height == 0 {
        return GeometryBucket::Unknown;
    }
    match u64::from(width) * u64::from(height) {
        pixels if pixels < 320 * 240 => GeometryBucket::Tiny,
        pixels if pixels < 1280 * 720 => GeometryBucket::Small,
        pixels if pixels < 1920 * 1080 => GeometryBucket::Medium,
        pixels if pixels < 3840 * 2160 => GeometryBucket::Large,
        _ => GeometryBucket::VeryLarge,
    }
}

/// Decide what to do with a camera frame whose size may not match the published size,
/// re-anchoring the published size in place when that is the right answer (#866).
///
/// Returns `Drop` only inside the brief grace window for a one-off anomaly. Past that
/// the frame is ALWAYS pushed -- re-anchored to the incoming size when the cooldown and
/// flap guard allow, letterboxed to the current published size otherwise -- so a camera
/// that comes back at a new resolution can never freeze every peer indefinitely.
///
/// This owns the whole decision, including the atomic re-anchor store, precisely so a
/// unit test can drive the real thing: `PublishedTrack` holds an `Arc<Room>` and a
/// `NativeVideoSource` and cannot be constructed offline. `push_nv12` must stay a thin
/// dispatcher over this -- decision logic that leaks back up there is untested logic.
fn resolve_camera_push_size(
    recovery: &Mutex<CameraSizeRecovery>,
    published_width: &std::sync::atomic::AtomicU32,
    published_height: &std::sync::atomic::AtomicU32,
    captured: (u32, u32),
    now: std::time::Instant,
) -> CameraPushDecision {
    let published = (
        published_width.load(Ordering::Relaxed),
        published_height.load(Ordering::Relaxed),
    );
    let mut state = recovery.lock_unpoisoned();

    if captured == published {
        // Reset only the run state. The flap guard must survive a brief good patch --
        // otherwise a source that flaps through the published size clears the guard
        // every cycle and it never trips.
        state.mismatch = None;
        state.recovering = false;
        return CameraPushDecision {
            verdict: CameraPushVerdict::Push,
            diagnostic: None,
        };
    }

    // A size CHANGE restarts the grace window, but never disengages recovery already
    // under way: a camera flapping between two wrong sizes must not win back a fresh
    // 30-frame drop budget on every switch.
    let run = match state.mismatch {
        Some(run) if run.size == captured => run,
        _ => CameraSizeMismatchRun {
            size: captured,
            first_seen: now,
            dropped: 0,
        },
    };

    let within_grace = run.dropped < CAMERA_SIZE_MISMATCH_GRACE_FRAMES
        && now.duration_since(run.first_seen) < CAMERA_SIZE_MISMATCH_GRACE;
    if within_grace && !state.recovering && !state.letterbox_locked {
        state.mismatch = Some(CameraSizeMismatchRun {
            dropped: run.dropped.saturating_add(1),
            ..run
        });
        return CameraPushDecision {
            verdict: CameraPushVerdict::Drop,
            diagnostic: None,
        };
    }
    state.mismatch = Some(run);

    let cooling_down = state
        .last_reanchor_at
        .is_some_and(|last| now.duration_since(last) < CAMERA_REANCHOR_COOLDOWN);
    // A degenerate size must never become the published size: a 0-dimension
    // re-anchor makes every subsequent good frame fail letterboxing for the
    // whole cooldown and burns flap budget toward the sticky lock (#866
    // review). Letterbox to the last good size instead.
    let degenerate = captured.0 == 0 || captured.1 == 0;
    let action = if state.letterbox_locked || cooling_down || degenerate {
        CameraRecoveryAction::Letterbox
    } else {
        published_width.store(captured.0, Ordering::Relaxed);
        published_height.store(captured.1, Ordering::Relaxed);
        state.last_reanchor_at = Some(now);
        let count = match state.reanchor_window {
            Some((start, count)) if now.duration_since(start) < CAMERA_REANCHOR_FLAP_WINDOW => {
                state.reanchor_window = Some((start, count.saturating_add(1)));
                count.saturating_add(1)
            }
            _ => {
                state.reanchor_window = Some((now, 1));
                1
            }
        };
        // This re-anchor still goes through; the guard closes the NEXT one.
        if count > CAMERA_REANCHOR_MAX_PER_WINDOW {
            state.letterbox_locked = true;
        }
        CameraRecoveryAction::Reanchor
    };

    // One warning and one diagnostic per episode -- the #866 field log was 2190
    // identical warn lines, which is what made the freeze invisible in triage.
    let first_of_episode = !state.recovering;
    state.recovering = true;
    drop(state);

    if first_of_episode {
        log::warn!(
            "publisher: camera frame size {}x{} != published {}x{} past the drop grace; recovering via {action:?}",
            captured.0,
            captured.1,
            published.0,
            published.1,
        );
    }
    CameraPushDecision {
        verdict: CameraPushVerdict::Push,
        diagnostic: first_of_episode.then(|| {
            crate::logging::SentryDiagnosticEvent::CameraSizeMismatchRecovery(
                crate::logging::CameraSizeMismatchDiagnostic {
                    role: crate::logging::DiagnosticRole::Sharer,
                    direction: crate::logging::CameraDirection::Publish,
                    capture_geometry: camera_geometry_bucket(captured.0, captured.1),
                    configured_geometry: camera_geometry_bucket(published.0, published.1),
                    action: match action {
                        CameraRecoveryAction::Reanchor => {
                            crate::logging::CameraRecoveryActionTag::Reanchor
                        }
                        CameraRecoveryAction::Letterbox => {
                            crate::logging::CameraRecoveryActionTag::Letterbox
                        }
                    },
                },
            )
        }),
    }
}

/// Per-track circuit breaker for native publishing. A transient scheduler stall must not
/// pin a share to the costly copy path, but repeated stalls still protect playback.
#[derive(Debug)]
struct NativeZeroCopyLatch {
    slow_frame_times: VecDeque<std::time::Instant>,
    disabled_until: Option<std::time::Instant>,
    reprobe_backoff: std::time::Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeZeroCopyTransition {
    None,
    Disabled { fallback_for: std::time::Duration },
    ReprobeStillSlow { fallback_for: std::time::Duration },
    Reenabled,
}

impl NativeZeroCopyLatch {
    fn new() -> Self {
        Self {
            slow_frame_times: VecDeque::with_capacity(NATIVE_ZERO_COPY_SLOW_FRAME_STRIKES),
            disabled_until: None,
            reprobe_backoff: NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF,
        }
    }

    fn native_attempt_due(&self, now: std::time::Instant) -> bool {
        match self.disabled_until {
            Some(until) => now >= until,
            None => true,
        }
    }

    fn record_native_capture(
        &mut self,
        now: std::time::Instant,
        capture_frame_elapsed: std::time::Duration,
    ) -> NativeZeroCopyTransition {
        if self.disabled_until.is_some() {
            if capture_frame_elapsed > NATIVE_CAPTURE_FRAME_STALL_THRESHOLD {
                return NativeZeroCopyTransition::ReprobeStillSlow {
                    fallback_for: self.extend_reprobe_backoff(now),
                };
            }

            self.disabled_until = None;
            self.slow_frame_times.clear();
            self.reprobe_backoff = NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF;
            return NativeZeroCopyTransition::Reenabled;
        }

        if capture_frame_elapsed <= NATIVE_CAPTURE_FRAME_STALL_THRESHOLD {
            return NativeZeroCopyTransition::None;
        }

        while self.slow_frame_times.front().is_some_and(|slow_at| {
            now.duration_since(*slow_at) > NATIVE_ZERO_COPY_SLOW_FRAME_WINDOW
        }) {
            self.slow_frame_times.pop_front();
        }
        self.slow_frame_times.push_back(now);
        if self.slow_frame_times.len() < NATIVE_ZERO_COPY_SLOW_FRAME_STRIKES {
            return NativeZeroCopyTransition::None;
        }

        self.slow_frame_times.clear();
        self.disabled_until = Some(now + NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF);
        self.reprobe_backoff = NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF;
        NativeZeroCopyTransition::Disabled {
            fallback_for: NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF,
        }
    }

    fn disable_without_timing(&mut self, now: std::time::Instant) -> bool {
        if self.disabled_until.is_some() {
            return false;
        }
        self.slow_frame_times.clear();
        self.reprobe_backoff = NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF;
        self.disabled_until = Some(now + self.reprobe_backoff);
        true
    }

    fn record_native_capture_failure(
        &mut self,
        now: std::time::Instant,
    ) -> NativeZeroCopyTransition {
        if self.disabled_until.is_some() {
            return NativeZeroCopyTransition::ReprobeStillSlow {
                fallback_for: self.extend_reprobe_backoff(now),
            };
        }

        self.slow_frame_times.clear();
        self.reprobe_backoff = NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF;
        self.disabled_until = Some(now + self.reprobe_backoff);
        NativeZeroCopyTransition::Disabled {
            fallback_for: self.reprobe_backoff,
        }
    }

    fn extend_reprobe_backoff(&mut self, now: std::time::Instant) -> std::time::Duration {
        self.reprobe_backoff = self
            .reprobe_backoff
            .saturating_mul(2)
            .min(NATIVE_ZERO_COPY_REPROBE_MAX_BACKOFF);
        self.disabled_until = Some(now + self.reprobe_backoff);
        self.reprobe_backoff
    }
}

struct NativeCaptureFailure {
    reason: &'static str,
    capture_frame_elapsed: std::time::Duration,
}

struct I420BufferPool {
    width: u32,
    height: u32,
    next: usize,
    buffers: Vec<I420Buffer>,
}

impl I420BufferPool {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            next: 0,
            buffers: Vec::with_capacity(I420_BUFFER_POOL_LIMIT),
        }
    }

    fn buffer(&mut self, width: u32, height: u32) -> &mut I420Buffer {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.next = 0;
            self.buffers.clear();
        }

        if self.buffers.len() < I420_BUFFER_POOL_LIMIT {
            self.buffers.push(I420Buffer::new(width, height));
            self.next = self.buffers.len() % I420_BUFFER_POOL_LIMIT;
            return self.buffers.last_mut().expect("just pushed I420 buffer");
        }

        let index = self.next;
        self.next = (self.next + 1) % I420_BUFFER_POOL_LIMIT;
        &mut self.buffers[index]
    }
}

impl RoomConnection<Arc<Room>> {
    /// Connect to `url` as the identity encoded in `token`. Despite the
    /// name, this is ALSO the one real room connection the in-app path
    /// (`session::join_room`) uses to receive -- `auto_subscribe: true`
    /// (flipped on for this task; previously `false` when "subscribing to
    /// others' shared windows" was still "a separate, not-yet-built receiver
    /// path", per this doc comment's own prior wording -- that receiver path
    /// is `compositor.rs`, built this task). SPEC.md's model has no manual
    /// per-window "accept this share" UI -- every participant automatically
    /// sees every other participant's shared windows as real compositor
    /// windows (SPEC.md §4.4) -- so auto-subscribing to every remote track
    /// on this same connection is the correct behavior, not a stopgap: no
    /// second LiveKit connection is opened for receiving. The receiver
    /// returned by `Room::connect` is immediately fanned out to the compositor
    /// and resilience receivers before the join tail starts, preserving early
    /// events without a later `Room::subscribe()` race.
    pub async fn connect(url: &str, token: &str) -> Result<Self, RoomConnectionError> {
        let mut room_options = RoomOptions::default();
        room_options.auto_subscribe = true;

        let (room, events) = Room::connect(url, token, room_options).await?;
        let room = Arc::new(room);
        log::info!(
            "RoomConnection connected: room='{}' sid={}",
            room.name(),
            room.maybe_sid()
                .map(|sid| sid.to_string())
                .unwrap_or_else(|| "pending".to_string())
        );

        Ok(Self::with_connect_event_source(room, events))
    }

    pub fn room(&self) -> Arc<Room> {
        self.room.clone()
    }

    /// Return this participant's selected palette index from the local
    /// metadata cache. Reading LiveKit's local participant metadata can race
    /// with the asynchronous metadata publish, so local echoes use this
    /// authoritative in-process value instead (#419).
    pub fn identity_palette_index(&self) -> Option<u8> {
        self.share_metadata.lock_unpoisoned().identity_palette_index
    }

    pub async fn publish_identity_palette_index(
        &self,
        palette_index: Option<u8>,
    ) -> Result<Option<u8>, livekit::RoomError> {
        let palette_index = palette_index.filter(|index| *index < 6);
        let metadata = {
            let share_metadata = self.share_metadata.lock_unpoisoned();
            let mut staged = share_metadata.clone();
            staged.identity_palette_index = palette_index;
            encode_window_metadata(&staged)
        };
        self.room.local_participant().set_metadata(metadata).await?;
        Ok(palette_index)
    }

    pub fn commit_identity_palette_index(&self, palette_index: Option<u8>) {
        self.share_metadata.lock_unpoisoned().identity_palette_index = palette_index;
    }

    pub async fn set_shared_window_title(&self, window_id: u32, title: String) {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            share_metadata.titles.insert(window_id, title);
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!(
                "publisher: failed to publish source-title metadata for window {window_id}: {e}"
            );
        }
    }

    pub async fn set_shared_window_capture_scale(&self, window_id: u32, scale: f64) {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            share_metadata.scales.insert(window_id, scale);
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!(
                "publisher: failed to publish capture-scale metadata for window {window_id}: {e}"
            );
        }
    }

    pub async fn set_shared_window_color_profile(
        &self,
        window_id: u32,
        color_profile: VideoColorProfile,
    ) {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            share_metadata
                .color_profiles
                .insert(window_id, color_profile);
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!(
                "publisher: failed to publish color-profile metadata for window {window_id}: {e}"
            );
        }
    }

    pub async fn set_shared_window_info(
        &self,
        window_id: u32,
        title: String,
        scale: f64,
        url: Option<String>,
        color_profile: VideoColorProfile,
        source_kind: SharedSourceKind,
        share_instance_id: Option<String>,
    ) {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            share_metadata.titles.insert(window_id, title);
            share_metadata.scales.insert(window_id, scale);
            match url.and_then(|u| crate::browser_url::privacy_minimized_openable_url(&u)) {
                Some(url) => {
                    share_metadata.urls.insert(window_id, url);
                }
                None => {
                    share_metadata.urls.remove(&window_id);
                }
            }
            share_metadata
                .color_profiles
                .insert(window_id, color_profile);
            share_metadata.kinds.insert(window_id, source_kind);
            if source_kind == SharedSourceKind::DisplayRegion {
                if let Some(region) = region_descriptor(window_id) {
                    share_metadata.regions.insert(window_id, region);
                }
            } else {
                share_metadata.regions.remove(&window_id);
            }
            match share_instance_id.filter(|value| !value.is_empty()) {
                Some(value) => {
                    share_metadata.share_instances.insert(window_id, value);
                }
                None => {
                    share_metadata.share_instances.remove(&window_id);
                }
            }
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!("publisher: failed to publish source metadata for window {window_id}: {e}");
        }
    }

    /// Publish source metadata for one exact local share generation. The
    /// generation is intentionally local-only; it makes a delayed clear
    /// conditional at the metadata mutation boundary (#298).
    pub async fn set_shared_window_info_for_generation(
        &self,
        window_id: u32,
        started_seq: u64,
        title: String,
        scale: f64,
        url: Option<String>,
        color_profile: VideoColorProfile,
        source_kind: SharedSourceKind,
    ) {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            share_metadata.titles.insert(window_id, title);
            share_metadata.generations.insert(window_id, started_seq);
            share_metadata.scales.insert(window_id, scale);
            match url.and_then(|u| crate::browser_url::privacy_minimized_openable_url(&u)) {
                Some(url) => {
                    share_metadata.urls.insert(window_id, url);
                }
                None => {
                    share_metadata.urls.remove(&window_id);
                }
            }
            share_metadata
                .color_profiles
                .insert(window_id, color_profile);
            share_metadata.kinds.insert(window_id, source_kind);
            if source_kind == SharedSourceKind::DisplayRegion {
                if let Some(region) = region_descriptor(window_id) {
                    share_metadata.regions.insert(window_id, region);
                }
            } else {
                share_metadata.regions.remove(&window_id);
            }
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!("publisher: failed to publish source metadata for window {window_id}: {e}");
        }
    }

    /// #915: publish just the `url` for one exact local share generation.
    /// Used by the macOS browser-URL refresh poller
    /// (`session/share.rs`'s `spawn_share_url_refresh`), which runs long
    /// after `set_shared_window_info_for_generation`'s initial publish
    /// (started with `url: None` -- extraction no longer runs on the
    /// share-start path). Windows' own poller
    /// (`session_stub.rs`'s `start_share_url_refresh`) is a separate,
    /// unconverted caller of `set_shared_window_info`/
    /// `set_shared_window_info_for_generation` and does NOT use this
    /// setter. Generation-checked the same way
    /// `clear_shared_window_title_for_generation` is (#298): a no-op unless
    /// `started_seq` still matches, so a poll that lands after this window
    /// id's share has already stopped (or been replaced by a newer share of
    /// the same id) can never resurrect stale metadata.
    ///
    /// Returns whether the write actually landed: `true` only when the
    /// generation matched AND the `set_metadata` signaling call succeeded.
    /// `run_url_refresh` (`session/url_refresh.rs`) uses this to decide
    /// whether it may treat the URL as sent, or must offer it again on a
    /// later successful extraction -- a dropped write (stale generation or
    /// a failed publish) must never be mistaken for a delivered one.
    pub async fn set_shared_window_url_for_generation(
        &self,
        window_id: u32,
        started_seq: u64,
        url: Option<String>,
    ) -> bool {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            if !stage_shared_window_url(&mut share_metadata, window_id, started_seq, url) {
                return false;
            }
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!("publisher: failed to publish url metadata for window {window_id}: {e}");
            return false;
        }
        true
    }

    /// Publish the sharer-chosen remote-control mode for one window, so the
    /// receiver header can display it read-only. Host-side policy only; it
    /// never changes the controller replay wire.
    pub async fn set_shared_control_mode(
        &self,
        window_id: u32,
        mode: crate::remote_control_core::RemoteControlMode,
    ) {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            share_metadata.control_modes.insert(window_id, mode);
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!("publisher: failed to publish control mode for window {window_id}: {e}");
        }
    }

    /// Publish whether one shared window may be remote-controlled, so remote
    /// receivers can hide the affordance instead of offering a button that
    /// will be refused. Discoverability only -- the authorization itself is
    /// re-checked host-side on every input packet.
    pub async fn set_shared_remote_control_allowed(&self, window_id: u32, allowed: bool) {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            share_metadata
                .remote_control_allowed
                .insert(window_id, allowed);
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!(
                "publisher: failed to publish remote-control permission for window {window_id}: {e}"
            );
        }
    }

    /// Publish `petalWindowZOrder` (#875): the sharer's currently-shared
    /// window ids, front-to-back. Merges non-destructively with the rest of
    /// `ShareMetadata` via `encode_window_metadata`, and republishes only
    /// when the order actually changed -- `stage_shared_window_order` does
    /// the comparison so an unrelated reshuffle of unshared windows (or a
    /// repeated identical poll) never triggers a `set_metadata` round trip.
    /// Returns whether it actually published (useful for tests/logging; the
    /// caller does not need to react to it).
    pub async fn set_shared_window_order(&self, order: Vec<u32>) -> bool {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            if stage_shared_window_order(&mut share_metadata, order).is_none() {
                return false;
            }
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!("publisher: failed to publish window z-order metadata: {e}");
        }
        true
    }

    pub async fn clear_shared_window_title(&self, window_id: u32) {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            share_metadata.titles.remove(&window_id);
            share_metadata.generations.remove(&window_id);
            share_metadata.scales.remove(&window_id);
            share_metadata.urls.remove(&window_id);
            share_metadata.color_profiles.remove(&window_id);
            share_metadata.kinds.remove(&window_id);
            share_metadata.share_instances.remove(&window_id);
            share_metadata.regions.remove(&window_id);
            encode_window_metadata(&share_metadata)
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!(
                "publisher: failed to clear source-title metadata for window {window_id}: {e}"
            );
        }
    }

    /// Clear source metadata only while the exact stopped share generation
    /// still owns it. The match and removal happen before the signaling await,
    /// so a re-share cannot be erased by a delayed older stop (#298).
    pub async fn clear_shared_window_title_for_generation(
        &self,
        window_id: u32,
        started_seq: u64,
    ) -> bool {
        let metadata = {
            let mut share_metadata = self.share_metadata.lock_unpoisoned();
            if clear_share_metadata_for_generation(&mut share_metadata, window_id, started_seq) {
                Some(encode_window_metadata(&share_metadata))
            } else {
                None
            }
        };
        let Some(metadata) = metadata else {
            return false;
        };
        if let Err(e) = self.room.local_participant().set_metadata(metadata).await {
            log::warn!(
                "publisher: failed to clear source-title metadata for window {window_id}: {e}"
            );
        }
        true
    }

    /// Publish a new video track (H.264/VideoToolbox by default; codec may be
    /// temporarily overridden by `PETAL_FORCE_CODEC` for issue #184 readback)
    /// at `width`x`height` for one shared window, at `Full` quality. Frames
    /// are pushed via `PublishedTrack::push_frame`. Multiple calls on the
    /// same `RoomConnection` publish multiple independent tracks on the one room
    /// connection.
    pub async fn publish_window(
        &self,
        width: u32,
        height: u32,
    ) -> Result<PublishedTrack, RoomConnectionError> {
        self.publish_window_at(width, height, ShareQuality::Full, None)
            .await
    }

    /// Same as `publish_window`, but at an explicit `ShareQuality` tier --
    /// used both for the initial publish (always `Full`, see `session.rs`'s
    /// focus policy -- a newly shared window starts focused) and for
    /// `PublishedTrack::republish_at` when a share's focus state changes.
    ///
    /// `window_id`, when given, is encoded directly into the LiveKit track
    /// NAME (see `track_name_for_window` below) -- SPEC.md §4.1/§4.4's
    /// requirement that the source `CGWindowID` "travel as stream metadata
    /// end-to-end, so a receiver knows 'this stream is App X's window'."
    /// `TrackPublishOptions` has no free-form per-track metadata field in
    /// this SDK version (checked: `room/options.rs`'s `TrackPublishOptions`
    /// has no `metadata` member), so the track name is the one piece of
    /// identifying string LiveKit already threads from publish through to
    /// `RemoteVideoTrack::name()` on every subscriber -- reusing it avoids a
    /// second, redundant metadata channel just for this one integer.
    pub async fn publish_window_at(
        &self,
        width: u32,
        height: u32,
        quality: ShareQuality,
        window_id: Option<u32>,
    ) -> Result<PublishedTrack, RoomConnectionError> {
        self.publish_window_at_with_encoder_context(
            width,
            height,
            quality,
            window_id,
            EncoderPublishOrigin::Ordinary,
            None,
        )
        .await
    }

    /// Publish the replacement created specifically by the system-wake
    /// capture refresh. This is the only entry point allowed to attach a
    /// software-encoder recovery action (#769).
    pub(crate) async fn publish_window_at_after_wake(
        &self,
        width: u32,
        height: u32,
        quality: ShareQuality,
        window_id: u32,
        recovery: PostWakeEncoderFallbackRecovery,
    ) -> Result<PublishedTrack, RoomConnectionError> {
        self.publish_window_at_with_encoder_context(
            width,
            height,
            quality,
            Some(window_id),
            EncoderPublishOrigin::PostWakeRestart,
            Some(recovery),
        )
        .await
    }

    async fn publish_window_at_with_encoder_context(
        &self,
        width: u32,
        height: u32,
        quality: ShareQuality,
        window_id: Option<u32>,
        encoder_origin: EncoderPublishOrigin,
        encoder_recovery: Option<PostWakeEncoderFallbackRecovery>,
    ) -> Result<PublishedTrack, RoomConnectionError> {
        validate_video_toolbox_h264_size(width, height)?;
        // Resolve once per publication. The same value stays with the track
        // for later quality-only updates even if the process environment is
        // changed before a focus transition.
        let simulcast_ladder = FullShareSimulcastLadder::from_env()?;
        let rtc_source = NativeVideoSource::new(VideoResolution { width, height }, true);
        let track_name = match window_id {
            Some(id) => track_name_for_window(id),
            None => "petal-window-capture".to_string(),
        };
        let track = LocalVideoTrack::create_video_track(
            &track_name,
            RtcVideoSource::Native(rtc_source.clone()),
        );

        let is_display_region = window_id.and_then(crate::region_window::resolve).is_some();
        let publish_opts = window_publish_options_for_region(
            width,
            height,
            quality,
            simulcast_ladder,
            is_display_region,
        );
        let publish_codec = publish_opts.video_codec;
        let requested_encoder = publish_opts.video_encoder;
        let simulcast_ladder_log =
            full_share_ladder_log(simulcast_ladder, width, height, &publish_opts);

        let publication = self
            .room
            .local_participant()
            .publish_track(LocalTrack::Video(track.clone()), publish_opts)
            .await?;

        log::info!("publisher: full-share simulcast {simulcast_ladder_log}");

        // The publication SID is the SFU's own handle for the track (assigned
        // in the AddTrackRequest response). Logging it lets a failing
        // announcement be cross-referenced against SFU-side logs: if the
        // track never appears in other participants' TrackPublished events
        // (observed on macOS 26 x86_64 VM: mic announced, window track never
        // broadcast) the SFU's handling of this specific SID is the next
        // place to look.
        log::info!(
            "Published video track '{}' sid={} {}x{} ({:?}, requested encoder: {:?}, quality: {:?})",
            track_name,
            publication.sid(),
            width,
            height,
            publish_codec,
            requested_encoder,
            quality
        );

        // Background task: log the actual negotiated encoder implementation
        // once stats are available, so we *confirm* VideoToolbox rather than
        // assume the preference took effect (see module doc comment).
        {
            let track_for_stats = track.clone();
            tokio::spawn(async move {
                log_encoder_once(track_for_stats, encoder_origin, encoder_recovery).await;
            });
        }
        // #907 review finding 4/6: shared (not per-task-snapshotted) state so
        // the starvation guard below and `PublishedTrack::set_quality` always
        // agree on the CURRENT quality/size -- see the struct field doc
        // comments for why these are `Arc`-wrapped.
        let published_width = Arc::new(std::sync::atomic::AtomicU32::new(width));
        let published_height = Arc::new(std::sync::atomic::AtomicU32::new(height));
        let shared_quality = Arc::new(Mutex::new(quality));

        // Background task: periodically log the window share's ACTUAL encoder
        // output per simulcast layer (target bitrate, encoded resolution, fps,
        // average QP, quality-limitation reason). Text fuzz on the receiver
        // that survives a 1:1 window is almost always the encoder being
        // rate-limited/downscaled/QP-limited on the HOST; these numbers make
        // that visible instead of inferred. Also drives the #907 top-rung
        // starvation guard -- see that function's doc comment.
        {
            let track_for_stats = track.clone();
            let quality_for_stats = shared_quality.clone();
            let width_for_stats = published_width.clone();
            let height_for_stats = published_height.clone();
            tokio::spawn(async move {
                log_window_share_encoder_stats(
                    track_for_stats,
                    quality_for_stats,
                    width_for_stats,
                    height_for_stats,
                    simulcast_ladder,
                )
                .await;
            });
        }

        Ok(PublishedTrack {
            room: self.room.clone(),
            rtc_source,
            track,
            published_width,
            published_height,
            published_frame_rate: 0.0, // window shares derive fps from ShareQuality
            resize_settle: Mutex::new(None),
            camera_size_recovery: Mutex::new(CameraSizeRecovery::default()),
            simulcast_ladder,
            quality: shared_quality,
            frame_seq: std::sync::atomic::AtomicU32::new(0),
            i420_pool: Mutex::new(I420BufferPool::new(width, height)),
            nv12_scratch: Mutex::new(Nv12Scratch::default()),
            native_fallback_pool: Arc::new(Mutex::new(Vec::new())),
            native_publish_disabled_by_env: native_publish_disabled_by_env(),
            native_zero_copy_latch: Mutex::new(NativeZeroCopyLatch::new()),
            push_drop_streak: Mutex::new(crate::logging::DropStreakDetector::default()),
        })
    }

    /// Publish the local WEBCAM as `petal-camera-<identity-slug>` --
    /// H.264/VideoToolbox like window shares (the native
    /// compositor only renders Native-decoding streams, and the web client
    /// forces H.264 for the same reason), `TrackSource::Camera`, no
    /// simulcast, a fixed webcam tier below `ShareQuality::Full` (a face
    /// doesn't need a screenshare's 4 Mbps). Frames are pushed via
    /// [`PublishedTrack::push_nv12`] (AVFoundation delivers NV12, not BGRA).
    pub async fn publish_camera(
        &self,
        width: u32,
        height: u32,
        frame_rate: f64,
        identity: &str,
    ) -> Result<PublishedTrack, RoomConnectionError> {
        let rtc_source = NativeVideoSource::new(VideoResolution { width, height }, true);
        let track_name = camera_track_name(identity);
        let track = LocalVideoTrack::create_video_track(
            &track_name,
            RtcVideoSource::Native(rtc_source.clone()),
        );

        let publish_opts = camera_publish_options(width, height, frame_rate);
        let requested_encoder = publish_opts.video_encoder;

        self.room
            .local_participant()
            .publish_track(LocalTrack::Video(track.clone()), publish_opts)
            .await?;

        log::info!(
            "Published camera track '{track_name}' {width}x{height} (H.264, requested encoder: {requested_encoder:?})"
        );

        {
            let track_for_stats = track.clone();
            tokio::spawn(async move {
                log_encoder_once(track_for_stats, EncoderPublishOrigin::Ordinary, None).await;
            });
        }

        Ok(PublishedTrack {
            room: self.room.clone(),
            rtc_source,
            track,
            published_width: Arc::new(std::sync::atomic::AtomicU32::new(width)),
            published_height: Arc::new(std::sync::atomic::AtomicU32::new(height)),
            published_frame_rate: frame_rate,
            resize_settle: Mutex::new(None),
            camera_size_recovery: Mutex::new(CameraSizeRecovery::default()),
            // Camera has one source encoding and never receives share-quality
            // updates; this required field is therefore inert for camera tracks.
            simulcast_ladder: FullShareSimulcastLadder::Legacy,
            quality: Arc::new(Mutex::new(ShareQuality::Full)),
            frame_seq: std::sync::atomic::AtomicU32::new(0),
            i420_pool: Mutex::new(I420BufferPool::new(width, height)),
            nv12_scratch: Mutex::new(Nv12Scratch::default()),
            native_fallback_pool: Arc::new(Mutex::new(Vec::new())),
            native_publish_disabled_by_env: false,
            native_zero_copy_latch: Mutex::new(NativeZeroCopyLatch::new()),
            push_drop_streak: Mutex::new(crate::logging::DropStreakDetector::default()),
        })
    }

    /// Convenience for the M0 example harnesses (`publish_probe.rs`): connect
    /// and publish a single track in one call. Not used by the in-app path
    /// (`session.rs`), which needs `connect`/`publish_window` split so it can
    /// share one room across multiple windows.
    pub async fn connect_and_publish(
        url: &str,
        token: &str,
        width: u32,
        height: u32,
    ) -> Result<PublishedTrack, RoomConnectionError> {
        let room_connection = Self::connect(url, token).await?;
        room_connection.discard_compositor_events();
        room_connection.publish_window(width, height).await
    }
}

fn validate_video_toolbox_h264_size(width: u32, height: u32) -> Result<(), RoomConnectionError> {
    if width.max(height) <= VIDEO_TOOLBOX_H264_MAX_LONG_EDGE {
        return Ok(());
    }
    Err(RoomConnectionError::InvalidVideoConfig(format!(
        "share capture size {width}x{height} exceeds VideoToolbox H.264 guardrail \
         ({VIDEO_TOOLBOX_H264_MAX_LONG_EDGE}px long edge)"
    )))
}

/// Camera encoding ceiling: the 720p30 baseline (2.5 Mbps, the pre-existing
/// fixed ceiling) scales with pixels and fps, clamped to [500 kbps, 16 Mbps].
/// 720p30 stays exactly 2.5 Mbps so defaults are unchanged; 1080p30 gets
/// ~5.6 Mbps, 4K30 clamps at 16 Mbps, and a low 480p15 request floors at
/// 500 kbps.
fn camera_video_encoding(
    width: u32,
    height: u32,
    frame_rate: f64,
) -> livekit::options::VideoEncoding {
    const CAMERA_BASELINE_PIXELS: u64 = 1280 * 720;
    const CAMERA_BASELINE_FPS: f64 = 30.0;
    const CAMERA_MIN_BITRATE_BPS: u64 = 500_000;
    const CAMERA_MAX_CEILING_BPS: u64 = 16_000_000;
    let pixels = u64::from(width) * u64::from(height);
    let scaled = CAMERA_MAX_BITRATE_BPS as f64
        * (pixels as f64 / CAMERA_BASELINE_PIXELS as f64)
        * (frame_rate / CAMERA_BASELINE_FPS);
    let max_bitrate = (scaled.round() as u64).clamp(CAMERA_MIN_BITRATE_BPS, CAMERA_MAX_CEILING_BPS);
    livekit::options::VideoEncoding {
        max_bitrate,
        max_framerate: frame_rate,
    }
}

/// Camera uses one explicit source encoding. Lower simulcast layers made fresh
/// subscriptions begin at 320x180 and climb through 640x360 even when every
/// receiver requested HIGH; a single 2.5 Mbps/30 fps encoding keeps startup at
/// source resolution on every native platform.
fn camera_publish_options(width: u32, height: u32, frame_rate: f64) -> TrackPublishOptions {
    TrackPublishOptions {
        source: TrackSource::Camera,
        video_codec: VideoCodec::H264,
        video_encoder: select_encoder_backend(),
        simulcast: false,
        simulcast_layers: None,
        video_encoding: Some(camera_video_encoding(width, height, frame_rate)),
        frame_metadata_features: {
            let mut f = livekit::options::FrameMetadataFeatures::default();
            f.user_timestamp = true;
            f.frame_id = true;
            f
        },
        ..Default::default()
    }
}

fn window_publish_options(
    width: u32,
    height: u32,
    quality: ShareQuality,
    simulcast_ladder: FullShareSimulcastLadder,
) -> TrackPublishOptions {
    window_publish_options_for_region(width, height, quality, simulcast_ladder, false)
}

fn window_publish_options_for_region(
    width: u32,
    height: u32,
    quality: ShareQuality,
    simulcast_ladder: FullShareSimulcastLadder,
    is_display_region: bool,
) -> TrackPublishOptions {
    // A Petal View track's dimensions are the selector's live ROI geometry.
    // Do not add a lower simulcast rung: its smaller frames are
    // indistinguishable from a legitimate selector shrink at the receiver.
    // Ordinary shares retain the explicit ladder for bandwidth adaptation.
    let simulcast = !is_display_region;
    let simulcast_layers = (!is_display_region)
        .then(|| share_simulcast_layers(width, height, quality, simulcast_ladder));
    let video_codec = window_video_codec();
    // #907: the top rung's published ceiling must reflect the SAME total
    // budget `layer_parameters` applies on a later live quality switch, or
    // the initial publish and a subsequent in-place update would disagree
    // about what "focused, full quality" is allowed to ask for.
    let raw_top_encoding = quality.video_encoding(width, height);
    let top_encoding = match &simulcast_layers {
        Some(layers) => {
            let lower_rungs_bitrate_bps: u64 =
                layers.iter().map(|layer| layer.encoding.max_bitrate).sum();
            livekit::options::VideoEncoding {
                max_bitrate: budgeted_top_bitrate(raw_top_encoding.max_bitrate, lower_rungs_bitrate_bps),
                max_framerate: raw_top_encoding.max_framerate,
            }
        }
        // A Petal View region has no lower rungs to budget against.
        None => raw_top_encoding,
    };

    TrackPublishOptions {
        source: TrackSource::Screenshare,
        video_codec,
        video_encoder: select_encoder_backend(),
        // LiveKit 0.7.49's public Rust API does not expose the native
        // contentHint or sender degradationPreference setters. Keep this
        // limitation scoped to #382; do not patch the vendored SDK here.
        // #181: macOS screencast low-latency RC/QP cap only engage on a
        // High-family H.264 profile; keep 42e01f after it as browser fallback.
        h264_profile_preference: H264ProfilePreference::HighFirst,
        // Full shares opt into simulcast with explicit layers; LiveKit's
        // screenshare defaults cap lower layers at 3-5fps and drop frames.
        simulcast,
        simulcast_layers,
        video_encoding: Some(top_encoding),
        frame_metadata_features: frame_metadata_features(),
        ..Default::default()
    }
}

/// The exact options a focused window share publishes with, exposed so
/// `examples/startup_layer_probe` measures the selected simulcast layout
/// instead of a hand-copied mirror that can drift away from what ships.
pub fn full_share_publish_options(width: u32, height: u32) -> TrackPublishOptions {
    let ladder = FullShareSimulcastLadder::from_env()
        .unwrap_or_else(|error| panic!("invalid full-share ladder configuration: {error}"));
    window_publish_options(width, height, ShareQuality::Full, ladder)
}

/// The resolved full-share ladder rendered exactly as `publish_window_track`
/// logs it, so a measurement reports the ladder that was **computed** rather
/// than the one the operator believed they selected. Exposed for
/// `examples/startup_layer_probe`; a hand-copied mirror in the probe banner is
/// the specific failure this exists to prevent (#613/#299 joint measurement).
pub fn full_share_ladder_description(width: u32, height: u32) -> String {
    let ladder = FullShareSimulcastLadder::from_env()
        .unwrap_or_else(|error| panic!("invalid full-share ladder configuration: {error}"));
    let options = window_publish_options(width, height, ShareQuality::Full, ladder);
    full_share_ladder_log(ladder, width, height, &options)
}

/// The resolved ladder's rungs as `(rid, width, height)`, smallest first, so a
/// consumer can name a decoded frame's layer from the ladder that is actually
/// live instead of assuming a q/h/f geometry.
pub fn full_share_ladder_rungs(width: u32, height: u32) -> Vec<(String, u32, u32)> {
    let ladder = FullShareSimulcastLadder::from_env()
        .unwrap_or_else(|error| panic!("invalid full-share ladder configuration: {error}"));
    let options = window_publish_options(width, height, ShareQuality::Full, ladder);
    let lower = options
        .simulcast_layers
        .as_ref()
        .expect("full-share publish options always include simulcast layers");
    let mut rungs: Vec<(String, u32, u32)> = ladder
        .lower_rids()
        .iter()
        .zip(lower.iter())
        .map(|(rid, layer)| ((*rid).to_string(), layer.width, layer.height))
        .collect();
    rungs.push((ladder.top_rid().to_string(), width, height));
    rungs
}

fn select_encoder_backend() -> VideoEncoderBackend {
    #[cfg(target_os = "macos")]
    {
        // Debug-only verification knob (same contract as the Windows branch
        // below): force the libwebrtc software H.264 encoder (OpenH264)
        // instead of VideoToolbox. Live observation on macOS 26 x86_64 (VM):
        // a window track published with the VideoToolbox backend
        // (`x-livekit-video-encoder-backend=videotoolbox` in the H.264 fmtp)
        // was accepted by the SFU but NEVER announced to any other room
        // participant, while the identical publish from a Windows client
        // (`x-livekit-video-encoder-backend=hardware`, OpenH264) was
        // announced and received. Setting this env var on a debug build
        // switches the macOS track to the known-good software path so the
        // SFU-side behavior can be confirmed.
        #[cfg(debug_assertions)]
        if std::env::var("PETAL_FORCE_SOFTWARE_ENCODE").is_ok() {
            log::warn!("PETAL_FORCE_SOFTWARE_ENCODE set: forcing the software video encoder");
            return VideoEncoderBackend::Software;
        }
        VideoEncoderBackend::VideoToolbox
    }

    #[cfg(target_os = "windows")]
    {
        // Verification knob (mirrors `native_publish_disabled_by_env`): force
        // the software encoder (OpenH264) from the environment. Used to A/B
        // encoder quality — the MF hardware encoder holds a fixed QP 26 on
        // this host regardless of the bitrate budget, so the only way to
        // change its QP policy is a different backend. Available in release
        // too (an explicit env override; default is unchanged). `log_encoder_once`
        // later confirms the actual negotiated encoder implementation.
        if std::env::var("PETAL_FORCE_SOFTWARE_ENCODE").is_ok() {
            log::warn!("PETAL_FORCE_SOFTWARE_ENCODE set: forcing the software video encoder");
            return VideoEncoderBackend::Software;
        }
        VideoEncoderBackend::Hardware
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        VideoEncoderBackend::Auto
    }
}

/// #386 breadcrumb for #188 (AV1/H.264 toggle): Apple Silicon has NO
/// hardware AV1 ENCODE through at least M4 (decode-only since M3) --
/// https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/apple/,
/// Apple dev forums thread 722933. `VideoCodec::AV1` below will route to a
/// software (libaom) encoder on this hardware, matching #184's spike intent
/// and #188's own mandatory SW-fallback plan. #386 evaluated HEVC as the
/// real hardware efficiency tier instead but found the vendored LiveKit Rust
/// SDK's `TrackPublishOptions`/`AddTrackRequest` do not expose a backup-codec
/// mechanism (NO-GO for now -- see that issue's commit for the full finding).
fn window_video_codec() -> VideoCodec {
    let default = VideoCodec::H264;
    let Ok(value) = std::env::var(PETAL_FORCE_CODEC_ENV) else {
        return default;
    };
    let Some(codec) = forced_video_codec_from_value(&value) else {
        log::warn!(
            "ignoring unsupported {PETAL_FORCE_CODEC_ENV}={:?}; expected h264, av1, or h265",
            value
        );
        return default;
    };
    if codec != default {
        log::warn!(
            "{PETAL_FORCE_CODEC_ENV}={} set: publishing window share with {:?} for issue #184 encoder readback spike",
            value.trim(),
            codec
        );
    }
    codec
}

/// Debug-only. Compiled out of release builds so a shipped app cannot be put
/// into the slower, less-tested NV12->I420 publish path from the environment.
fn native_publish_disabled_by_env() -> bool {
    #[cfg(not(debug_assertions))]
    {
        false
    }
    #[cfg(debug_assertions)]
    {
        if std::env::var(PETAL_DISABLE_NATIVE_PUBLISH_ENV).is_err() {
            return false;
        }
        if !NATIVE_PUBLISH_ENV_WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "{PETAL_DISABLE_NATIVE_PUBLISH_ENV} set: native CVPixelBuffer publish path disabled; using NV12->I420 fallback from first frame"
            );
        }
        true
    }
}

fn forced_video_codec_from_value(value: &str) -> Option<VideoCodec> {
    match value.trim().to_ascii_lowercase().as_str() {
        "h264" => Some(VideoCodec::H264),
        "av1" => Some(VideoCodec::AV1),
        "h265" | "hevc" => Some(VideoCodec::H265),
        _ => None,
    }
}

fn full_share_simulcast_layers(
    width: u32,
    height: u32,
    ladder: FullShareSimulcastLadder,
) -> Vec<VideoPreset> {
    let half_width = half_dimension(width);
    let half_height = half_dimension(height);
    let quarter_width = half_dimension(half_width);
    let quarter_height = half_dimension(half_height);
    let three_quarter_width = three_quarter_dimension(width);
    let three_quarter_height = three_quarter_dimension(height);

    match ladder {
        // Keep this branch byte-for-byte equivalent in values to the former
        // fixed ladder: a quarter rung at 15fps and a half rung at 30fps.
        FullShareSimulcastLadder::Legacy => vec![
            VideoPreset::new(
                quarter_width,
                quarter_height,
                full_share_quarter_layer_max_bitrate(quarter_width, quarter_height),
                FULL_SIMULCAST_QUARTER_MAX_FRAMERATE_FPS,
            ),
            VideoPreset::new(
                half_width,
                half_height,
                full_share_half_layer_max_bitrate(half_width, half_height),
                FULL_SIMULCAST_HALF_MAX_FRAMERATE_FPS,
            ),
        ],
        // Identical to Legacy except the bottom rung's framerate cap.
        // Measured 2026-07-28 (#613, n=6): p50 175.2ms vs legacy's 138.0ms
        // and encoder utilisation 74%->~90% -- cadence is NOT the lever, the
        // rung spread is. Kept so that verdict stays re-measurable.
        FullShareSimulcastLadder::LegacyBottom30 => vec![
            VideoPreset::new(
                quarter_width,
                quarter_height,
                full_share_quarter_layer_max_bitrate(quarter_width, quarter_height),
                FULL_SIMULCAST_HALF_MAX_FRAMERATE_FPS,
            ),
            VideoPreset::new(
                half_width,
                half_height,
                full_share_half_layer_max_bitrate(half_width, half_height),
                FULL_SIMULCAST_HALF_MAX_FRAMERATE_FPS,
            ),
        ],
        FullShareSimulcastLadder::Raised => vec![
            VideoPreset::new(
                half_width,
                half_height,
                full_share_half_layer_max_bitrate(half_width, half_height),
                FULL_SIMULCAST_HALF_MAX_FRAMERATE_FPS,
            ),
            VideoPreset::new(
                three_quarter_width,
                three_quarter_height,
                full_share_half_layer_max_bitrate(three_quarter_width, three_quarter_height),
                FULL_SIMULCAST_HALF_MAX_FRAMERATE_FPS,
            ),
        ],
        FullShareSimulcastLadder::TwoRung => vec![VideoPreset::new(
            three_quarter_width,
            three_quarter_height,
            full_share_half_layer_max_bitrate(three_quarter_width, three_quarter_height),
            FULL_SIMULCAST_HALF_MAX_FRAMERATE_FPS,
        )],
        FullShareSimulcastLadder::TwoRungHalf => vec![VideoPreset::new(
            half_width,
            half_height,
            full_share_half_layer_max_bitrate(half_width, half_height),
            FULL_SIMULCAST_HALF_MAX_FRAMERATE_FPS,
        )],
    }
}

fn share_simulcast_layers(
    width: u32,
    height: u32,
    quality: ShareQuality,
    ladder: FullShareSimulcastLadder,
) -> Vec<VideoPreset> {
    let mut layers = full_share_simulcast_layers(width, height, ladder);
    if quality == ShareQuality::Reduced {
        for layer in &mut layers {
            layer.encoding.max_bitrate =
                (layer.encoding.max_bitrate / 2).max(FULL_SIMULCAST_QUARTER_MIN_BITRATE_BPS);
            layer.encoding.max_framerate = quality.capture_fps() as f64;
        }
    }
    layers
}

fn full_share_quarter_layer_max_bitrate(width: u32, height: u32) -> u64 {
    (full_share_half_layer_max_bitrate(width, height) / 2)
        .max(FULL_SIMULCAST_QUARTER_MIN_BITRATE_BPS)
        .min(FULL_SIMULCAST_QUARTER_MAX_BITRATE_BPS)
}

fn full_share_half_layer_max_bitrate(width: u32, height: u32) -> u64 {
    let pixels = u64::from(width) * u64::from(height);
    let scaled = FULL_SIMULCAST_HALF_MIN_BITRATE_BPS.saturating_mul(pixels)
        / FULL_SIMULCAST_HALF_BASE_PIXELS;

    scaled
        .max(FULL_SIMULCAST_HALF_MIN_BITRATE_BPS)
        .min(FULL_SIMULCAST_HALF_MAX_BITRATE_BPS)
}

fn half_dimension(value: u32) -> u32 {
    (value / 2).max(1)
}

fn three_quarter_dimension(value: u32) -> u32 {
    ((u64::from(value) * 3 / 4) as u32).max(1)
}

fn full_share_ladder_log(
    ladder: FullShareSimulcastLadder,
    width: u32,
    height: u32,
    options: &TrackPublishOptions,
) -> String {
    let mut rungs = options
        .simulcast_layers
        .as_ref()
        .map(|lower_layers| {
            ladder
                .lower_rids()
                .iter()
                .zip(lower_layers.iter())
                .map(|(rid, layer)| {
                    format!(
                        "rid={rid} {}x{} {:.0}fps {}bps",
                        layer.width,
                        layer.height,
                        layer.encoding.max_framerate,
                        layer.encoding.max_bitrate
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let top = options
        .video_encoding
        .as_ref()
        .expect("full-share publish options always include a top encoding");
    rungs.push(format!(
        "rid={} {}x{} {:.0}fps {}bps",
        if options.simulcast {
            ladder.top_rid()
        } else {
            "native"
        },
        width,
        height,
        top.max_framerate,
        top.max_bitrate
    ));
    format!("ladder={} {}", ladder.as_str(), rungs.join("; "))
}

fn frame_metadata_features() -> livekit::options::FrameMetadataFeatures {
    let mut features = livekit::options::FrameMetadataFeatures::default();
    features.user_timestamp = true;
    features.frame_id = true;
    features
}

/// The LiveKit track name a shared window's video track is published under
/// -- `"petal-window-{window_id}"`. Shared with the subscriber side
/// (`compositor.rs`'s `window_id_from_track_name`, the inverse of this) so
/// there is exactly one place the naming scheme is defined, not two that
/// could drift out of sync.
pub fn track_name_for_window(window_id: u32) -> String {
    format!("petal-window-{window_id}")
}

/// Parse a `window_id` back out of a track name produced by
/// `track_name_for_window`, or `None` if it doesn't match that shape (e.g. a
/// non-Petal publisher, or the M0 spike's plain `"petal-window-capture"`
/// name from an unspecified-`window_id` publish).
pub fn window_id_from_track_name(name: &str) -> Option<u32> {
    name.strip_prefix("petal-window-")?.parse().ok()
}

/// Track-name prefix a remote WEBCAM feed is published under
/// (`"petal-camera-<identity-slug>"`) -- produced by the web harness's
/// `trackNameForCamera()` (web-harness/src/main.ts), which must stay in
/// lockstep with this constant. Unlike window shares there is no numeric id
/// embedded in the name; the compositor still needs a stable u32 window key,
/// so `camera_window_id` below derives a synthetic one from the name itself.
pub const CAMERA_TRACK_PREFIX: &str = "petal-camera-";

/// Derive a synthetic, stable u32 compositor-window id for a camera track
/// name: FNV-1a over the full track name, with the top bit forced on
/// (`| 0x8000_0000`) so it can never collide with a real CGWindowID (those
/// are small kernel-assigned integers, nowhere near 2^31).
pub fn camera_window_id(track_name: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5; // FNV offset basis
    for b in track_name.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x0100_0193); // FNV prime
    }
    hash | 0x8000_0000
}

/// Rust twin of the web client's `trackNameForCamera()`
/// (web-harness/src/trackNames.ts) -- MUST stay in lockstep: lowercase,
/// every run of non-[a-z0-9] collapsed to a single '-', leading/trailing '-'
/// trimmed, empty result falls back to "anon", prefixed with
/// [`CAMERA_TRACK_PREFIX`]. Same lockstep discipline as `rooms::slugify` /
/// the harness's `slugify` (commit 24c05a7): change one, change the other.
pub fn camera_track_name(identity: &str) -> String {
    let mut slug = String::with_capacity(identity.len());
    let mut last_dash = true; // suppress a leading '-'
    for c in identity.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("anon");
    }
    format!("{CAMERA_TRACK_PREFIX}{slug}")
}

const PETAL_WINDOW_TITLES_METADATA_KEY: &str = "petalWindowTitles";
const PETAL_WINDOW_SCALES_METADATA_KEY: &str = "petalWindowScales";
const PETAL_WINDOW_URLS_METADATA_KEY: &str = "petalWindowUrls";
const PETAL_WINDOW_COLOR_PROFILES_METADATA_KEY: &str = "petalWindowColorProfiles";
pub const PETAL_WINDOW_KINDS_METADATA_KEY: &str = "petalWindowKinds";
const PETAL_WINDOW_SHARE_INSTANCES_METADATA_KEY: &str = "petalWindowShareInstances";
pub const PETAL_WINDOW_CONTROL_MODES_METADATA_KEY: &str = "petalWindowControlModes";
const PETAL_WINDOW_REGIONS_METADATA_KEY: &str = "petalWindowRegions";
/// Per-share remote-control permission, host-owned. ONLY `false` entries are
/// encoded, so absence means "allowed" and a sharer that predates this key
/// keeps behaving exactly as before. This is a DISCOVERABILITY signal for the
/// receiver's affordance -- never the authorization itself, which stays on the
/// host (`remote_control.rs`) where a peer cannot influence it.
pub const PETAL_WINDOW_REMOTE_CONTROL_METADATA_KEY: &str = "petalWindowRemoteControl";
pub const PETAL_IDENTITY_PALETTE_INDEX_METADATA_KEY: &str = "petalIdentityPaletteIndex";
/// #875: the sharer's currently-shared window ids, front-to-back (index 0 =
/// frontmost), as a JSON array. Older sharers omit this key entirely --
/// receivers must treat absence as "no rank data," not as "empty order."
pub const PETAL_WINDOW_Z_ORDER_METADATA_KEY: &str = "petalWindowZOrder";

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SharedRegionDescriptor {
    display_id: u32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    /// The same selector ROI in the owning display's local coordinate space.
    /// Keep these beside the global origin so native consumers can construct
    /// ScreenCaptureKit's `sourceRect` without reconstructing display bounds.
    display_local_x: f64,
    display_local_y: f64,
    display_local_width: f64,
    display_local_height: f64,
    physical_width: u32,
    physical_height: u32,
    scale: f64,
    generation: u64,
}

#[derive(Clone, Default)]
struct ShareMetadata {
    titles: HashMap<u32, String>,
    // Local-only ownership of per-window metadata. This is never encoded for
    // receivers; it prevents an old async stop from clearing a re-share (#298).
    generations: HashMap<u32, u64>,
    scales: HashMap<u32, f64>,
    urls: HashMap<u32, String>,
    color_profiles: HashMap<u32, VideoColorProfile>,
    kinds: HashMap<u32, SharedSourceKind>,
    share_instances: HashMap<u32, String>,
    control_modes: HashMap<u32, crate::remote_control_core::RemoteControlMode>,
    regions: HashMap<u32, SharedRegionDescriptor>,
    /// Per-share remote-control permission. Absent == allowed (see the key's
    /// doc); only `false` is ever encoded.
    remote_control_allowed: HashMap<u32, bool>,
    identity_palette_index: Option<u8>,
    /// #875: front-to-back order of the currently-shared window subset (index
    /// 0 = frontmost). Not per-window like the maps above -- this is a single
    /// ordered snapshot of "what's shared, in what stacking order," refreshed
    /// externally (see `telepointer.rs`'s sender loop) whenever it changes.
    window_order: Vec<u32>,
}

fn region_descriptor(window_id: u32) -> Option<SharedRegionDescriptor> {
    let source = crate::region_window::resolve(window_id)?;
    let display = source.display?;
    let physical = display.clipped_physical_roi(source.frame)?;
    Some(SharedRegionDescriptor {
        display_id: display.id,
        x: source.frame.x,
        y: source.frame.y,
        width: source.frame.width,
        height: source.frame.height,
        display_local_x: source.frame.x - display.frame.x,
        display_local_y: source.frame.y - display.frame.y,
        display_local_width: source.frame.width,
        display_local_height: source.frame.height,
        // Metadata describes the selector canvas, not only the currently
        // visible overlap. The receiver uses this geometry for FullControl.
        physical_width: physical.output_width,
        physical_height: physical.output_height,
        scale: display.scale,
        generation: source.generation.0,
    })
}

fn clear_share_metadata_for_generation(
    metadata: &mut ShareMetadata,
    window_id: u32,
    started_seq: u64,
) -> bool {
    if metadata.generations.get(&window_id) != Some(&started_seq) {
        return false;
    }
    metadata.titles.remove(&window_id);
    metadata.generations.remove(&window_id);
    metadata.scales.remove(&window_id);
    metadata.urls.remove(&window_id);
    metadata.color_profiles.remove(&window_id);
    metadata.kinds.remove(&window_id);
    metadata.share_instances.remove(&window_id);
    metadata.control_modes.remove(&window_id);
    metadata.regions.remove(&window_id);
    metadata.remote_control_allowed.remove(&window_id);
    true
}

/// Pure generation-checked `urls` mutation (#915), extracted so it can be
/// unit tested directly the same way `clear_share_metadata_for_generation`
/// is -- `RoomConnection::set_shared_window_url_for_generation` wraps this
/// with the signaling `set_metadata` publish, which needs a live LiveKit
/// room and isn't constructible in a unit test. Returns whether the
/// generation matched (and the mutation was applied).
fn stage_shared_window_url(
    metadata: &mut ShareMetadata,
    window_id: u32,
    started_seq: u64,
    url: Option<String>,
) -> bool {
    if metadata.generations.get(&window_id) != Some(&started_seq) {
        return false;
    }
    match url.and_then(|u| crate::browser_url::privacy_minimized_openable_url(&u)) {
        Some(url) => {
            metadata.urls.insert(window_id, url);
        }
        None => {
            metadata.urls.remove(&window_id);
        }
    }
    true
}

fn encode_window_metadata(metadata: &ShareMetadata) -> String {
    let mut root = serde_json::Map::new();
    let mut encoded_titles = serde_json::Map::new();
    for (window_id, title) in &metadata.titles {
        encoded_titles.insert(
            window_id.to_string(),
            serde_json::Value::String(title.clone()),
        );
    }
    let mut encoded_scales = serde_json::Map::new();
    for (window_id, scale) in &metadata.scales {
        if scale.is_finite() && *scale > 0.0 {
            let Some(n) = serde_json::Number::from_f64(*scale) else {
                continue;
            };
            encoded_scales.insert(window_id.to_string(), serde_json::Value::Number(n));
        }
    }
    root.insert(
        PETAL_WINDOW_TITLES_METADATA_KEY.to_string(),
        serde_json::Value::Object(encoded_titles),
    );
    root.insert(
        PETAL_WINDOW_SCALES_METADATA_KEY.to_string(),
        serde_json::Value::Object(encoded_scales),
    );
    let mut encoded_urls = serde_json::Map::new();
    for (window_id, url) in &metadata.urls {
        if let Some(url) = crate::browser_url::privacy_minimized_openable_url(url) {
            encoded_urls.insert(window_id.to_string(), serde_json::Value::String(url));
        }
    }
    root.insert(
        PETAL_WINDOW_URLS_METADATA_KEY.to_string(),
        serde_json::Value::Object(encoded_urls),
    );
    let mut encoded_color_profiles = serde_json::Map::new();
    for (window_id, color_profile) in &metadata.color_profiles {
        match serde_json::to_value(color_profile) {
            Ok(value) if value.is_object() => {
                encoded_color_profiles.insert(window_id.to_string(), value);
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!(
                    "publisher: failed to encode color profile metadata for window {window_id}: {e}"
                );
            }
        }
    }
    root.insert(
        PETAL_WINDOW_COLOR_PROFILES_METADATA_KEY.to_string(),
        serde_json::Value::Object(encoded_color_profiles),
    );
    let mut encoded_kinds = serde_json::Map::new();
    for (window_id, kind) in &metadata.kinds {
        encoded_kinds.insert(
            window_id.to_string(),
            serde_json::Value::String(kind.as_wire().to_string()),
        );
    }
    root.insert(
        PETAL_WINDOW_KINDS_METADATA_KEY.to_string(),
        serde_json::Value::Object(encoded_kinds),
    );
    let mut encoded_share_instances = serde_json::Map::new();
    for (window_id, share_instance_id) in &metadata.share_instances {
        if !share_instance_id.is_empty() {
            encoded_share_instances.insert(
                window_id.to_string(),
                serde_json::Value::String(share_instance_id.clone()),
            );
        }
    }
    root.insert(
        PETAL_WINDOW_SHARE_INSTANCES_METADATA_KEY.to_string(),
        serde_json::Value::Object(encoded_share_instances),
    );
    let mut encoded_regions = serde_json::Map::new();
    for (window_id, region) in &metadata.regions {
        if let Ok(value) = serde_json::to_value(region) {
            encoded_regions.insert(window_id.to_string(), value);
        }
    }
    root.insert(
        PETAL_WINDOW_REGIONS_METADATA_KEY.to_string(),
        serde_json::Value::Object(encoded_regions),
    );
    let mut encoded_control_modes = serde_json::Map::new();
    for (window_id, mode) in &metadata.control_modes {
        if let Ok(value) = serde_json::to_value(mode) {
            encoded_control_modes.insert(window_id.to_string(), value);
        }
    }
    root.insert(
        PETAL_WINDOW_CONTROL_MODES_METADATA_KEY.to_string(),
        serde_json::Value::Object(encoded_control_modes),
    );
    // Only DENIALS are encoded. Absence therefore means "allowed", which is
    // what keeps a pre-key sharer behaving as before, and keeps the common
    // case (everything allowed) off the wire entirely.
    let mut encoded_remote_control = serde_json::Map::new();
    for (window_id, allowed) in &metadata.remote_control_allowed {
        if !*allowed {
            encoded_remote_control.insert(window_id.to_string(), serde_json::Value::Bool(false));
        }
    }
    if !encoded_remote_control.is_empty() {
        root.insert(
            PETAL_WINDOW_REMOTE_CONTROL_METADATA_KEY.to_string(),
            serde_json::Value::Object(encoded_remote_control),
        );
    }
    if let Some(index) = metadata.identity_palette_index.filter(|index| *index < 6) {
        root.insert(
            PETAL_IDENTITY_PALETTE_INDEX_METADATA_KEY.to_string(),
            serde_json::Value::Number(serde_json::Number::from(index)),
        );
    }
    let encoded_window_order: Vec<serde_json::Value> = metadata
        .window_order
        .iter()
        .map(|window_id| serde_json::Value::Number(serde_json::Number::from(*window_id)))
        .collect();
    root.insert(
        PETAL_WINDOW_Z_ORDER_METADATA_KEY.to_string(),
        serde_json::Value::Array(encoded_window_order),
    );
    serde_json::Value::Object(root).to_string()
}

/// If `order` differs from the currently staged z-order, stage it and return
/// the new value for the caller to encode + publish; `None` means "unchanged,
/// skip the republish entirely." Kept as a pure, synchronous helper (rather
/// than folded into the async `set_shared_window_order` below) so the
/// republish-only-on-change behavior is directly unit-testable without a live
/// `Room`/`LocalParticipant`.
fn stage_shared_window_order(metadata: &mut ShareMetadata, order: Vec<u32>) -> Option<Vec<u32>> {
    if metadata.window_order == order {
        None
    } else {
        metadata.window_order = order.clone();
        Some(order)
    }
}

/// Decode the `petalWindowZOrder` participant-metadata key (#875): the
/// sharer's currently-shared window ids, front-to-back (index 0 = frontmost).
/// `None` means either the key is absent (an older sharer, or this sharer has
/// not yet published an order) or the value is malformed -- callers must not
/// distinguish those two cases, since both mean "no rank data available."
pub fn shared_window_z_order_from_metadata(metadata: &str) -> Option<Vec<u32>> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    let array = value.get(PETAL_WINDOW_Z_ORDER_METADATA_KEY)?.as_array()?;
    let mut order = Vec::with_capacity(array.len());
    for entry in array {
        let id = entry.as_u64()?;
        order.push(u32::try_from(id).ok()?);
    }
    Some(order)
}

/// This window's front-to-back rank within `shared_window_z_order_from_metadata`'s
/// order (0 = frontmost), or `None` if the key is absent/malformed or this
/// window id is not present in the order.
pub fn shared_window_z_rank_from_metadata(metadata: &str, window_id: u32) -> Option<u32> {
    let order = shared_window_z_order_from_metadata(metadata)?;
    order
        .iter()
        .position(|&id| id == window_id)
        .map(|index| index as u32)
}

pub fn identity_palette_index_from_metadata(metadata: &str) -> Option<u8> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    let raw = value
        .get(PETAL_IDENTITY_PALETTE_INDEX_METADATA_KEY)?
        .as_u64()?;
    (raw < 6).then_some(raw as u8)
}

pub fn shared_window_title_from_metadata(metadata: &str, window_id: u32) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    value
        .get(PETAL_WINDOW_TITLES_METADATA_KEY)?
        .get(window_id.to_string())?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

pub fn shared_window_scale_from_metadata(metadata: &str, window_id: u32) -> Option<f64> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    value
        .get(PETAL_WINDOW_SCALES_METADATA_KEY)?
        .get(window_id.to_string())?
        .as_f64()
        .filter(|scale| scale.is_finite() && *scale > 0.0)
}

pub fn shared_window_url_from_metadata(metadata: &str, window_id: u32) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    value
        .get(PETAL_WINDOW_URLS_METADATA_KEY)?
        .get(window_id.to_string())?
        .as_str()
        .and_then(crate::browser_url::privacy_minimized_openable_url)
}

pub fn shared_window_kind_from_metadata(metadata: &str, window_id: u32) -> SharedSourceKind {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return SharedSourceKind::Window;
    };
    value
        .get(PETAL_WINDOW_KINDS_METADATA_KEY)
        .and_then(|kinds| kinds.get(window_id.to_string()))
        .and_then(|value| value.as_str())
        .and_then(SharedSourceKind::from_wire)
        .unwrap_or(SharedSourceKind::Window)
}

/// Initial selector canvas dimensions for Petal View. The publication
/// dimension can be unavailable briefly when the first subscription lands,
/// while the metadata already carries the native ROI geometry. Receivers use
/// this to avoid resizing to the first low simulcast frame.
pub fn shared_window_region_physical_size_from_metadata(
    metadata: &str,
    window_id: u32,
) -> Option<(u32, u32)> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    let region = value
        .get(PETAL_WINDOW_REGIONS_METADATA_KEY)?
        .get(window_id.to_string())?;
    let width = region.get("physicalWidth")?.as_u64()?.try_into().ok()?;
    let height = region.get("physicalHeight")?.as_u64()?.try_into().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

pub fn shared_window_share_instance_from_metadata(
    metadata: &str,
    window_id: u32,
) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    value
        .get(PETAL_WINDOW_SHARE_INSTANCES_METADATA_KEY)?
        .get(window_id.to_string())?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Receiver side of the sharer-chosen control mode (`petalWindowControlModes`
/// Whether the sharer permits remote control of one window.
///
/// Defaults to `true` on absent, unparseable, or unexpected values: only an
/// explicit `false` denies. That fail-OPEN default is safe precisely because
/// this is not the security gate -- it drives the receiver's affordance, while
/// `remote_control.rs` independently re-checks the host's own state before any
/// input reaches the OS. Failing closed here would instead hide the button for
/// every sharer that predates the key.
pub fn shared_window_remote_control_allowed_from_metadata(metadata: &str, window_id: u32) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return true;
    };
    value
        .get(PETAL_WINDOW_REMOTE_CONTROL_METADATA_KEY)
        .and_then(|entries| entries.get(window_id.to_string()))
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

/// metadata). Unknown/absent defaults to FullControl for displays and
/// cursor-preserving for windows.
pub fn shared_window_control_mode_from_metadata(
    metadata: &str,
    window_id: u32,
) -> crate::remote_control_core::RemoteControlMode {
    let display = matches!(
        shared_window_kind_from_metadata(metadata, window_id),
        SharedSourceKind::Display | SharedSourceKind::DisplayRegion
    );
    let default = if display {
        crate::remote_control_core::RemoteControlMode::FullControl
    } else {
        crate::remote_control_core::RemoteControlMode::CursorPreserving
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return default;
    };
    let Some(mode) = value
        .get(PETAL_WINDOW_CONTROL_MODES_METADATA_KEY)
        .and_then(|modes| modes.get(window_id.to_string()))
        .and_then(|value| value.as_str())
    else {
        return default;
    };
    match mode.trim() {
        "fullControl" | "full-control" => {
            crate::remote_control_core::RemoteControlMode::FullControl
        }
        "cursorPreserving" | "cursor-preserving" => {
            crate::remote_control_core::RemoteControlMode::CursorPreserving
        }
        _ => default,
    }
}

pub(crate) fn shared_window_color_profile_from_metadata(
    metadata: &str,
    window_id: u32,
) -> Option<VideoColorProfile> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    serde_json::from_value(
        value
            .get(PETAL_WINDOW_COLOR_PROFILES_METADATA_KEY)?
            .get(window_id.to_string())?
            .clone(),
    )
    .ok()
}

pub(crate) fn published_window_color_profile(
    capture_color_profile: VideoColorProfile,
) -> VideoColorProfile {
    capture_color_profile
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppleBgraToI420Conversion {
    Bt601VideoRange,
    Bt601FullRange,
    Bt709VideoRange,
    Bt709FullRange,
}

fn apple_bgra_to_i420_conversion(profile: VideoColorProfile) -> AppleBgraToI420Conversion {
    use crate::video_color::{MatrixCoefficients, PixelRange};

    match (profile.matrix, profile.range) {
        (MatrixCoefficients::Bt601, PixelRange::Video) => {
            AppleBgraToI420Conversion::Bt601VideoRange
        }
        (MatrixCoefficients::Bt601, PixelRange::Full) => AppleBgraToI420Conversion::Bt601FullRange,
        (MatrixCoefficients::Bt709, PixelRange::Video) => {
            AppleBgraToI420Conversion::Bt709VideoRange
        }
        (MatrixCoefficients::Bt709, PixelRange::Full) => AppleBgraToI420Conversion::Bt709FullRange,
    }
}

#[allow(clippy::too_many_arguments)]
fn convert_apple_bgra_to_i420(
    src_bgra: &[u8],
    src_stride: usize,
    dst_y: &mut [u8],
    dst_stride_y: u32,
    dst_u: &mut [u8],
    dst_stride_u: u32,
    dst_v: &mut [u8],
    dst_stride_v: u32,
    width: u32,
    height: u32,
    profile: VideoColorProfile,
) -> bool {
    if validated_bgra_layout(
        src_bgra,
        src_stride,
        dst_y,
        dst_stride_y,
        dst_u,
        dst_stride_u,
        dst_v,
        dst_stride_v,
        width,
        height,
    )
    .is_none()
    {
        return false;
    }

    #[cfg(target_os = "macos")]
    {
        match apple_bgra_to_i420_conversion(profile) {
            AppleBgraToI420Conversion::Bt601VideoRange => unsafe {
                yuv_sys::rs_ARGBToI420(
                    src_bgra.as_ptr(),
                    src_stride as i32,
                    dst_y.as_mut_ptr(),
                    dst_stride_y as i32,
                    dst_u.as_mut_ptr(),
                    dst_stride_u as i32,
                    dst_v.as_mut_ptr(),
                    dst_stride_v as i32,
                    width as i32,
                    height as i32,
                ) == 0
            },
            AppleBgraToI420Conversion::Bt601FullRange => unsafe {
                yuv_sys::rs_ARGBToJ420(
                    src_bgra.as_ptr(),
                    src_stride as i32,
                    dst_y.as_mut_ptr(),
                    dst_stride_y as i32,
                    dst_u.as_mut_ptr(),
                    dst_stride_u as i32,
                    dst_v.as_mut_ptr(),
                    dst_stride_v as i32,
                    width as i32,
                    height as i32,
                ) == 0
            },
            AppleBgraToI420Conversion::Bt709VideoRange
            | AppleBgraToI420Conversion::Bt709FullRange => convert_apple_bgra_to_i420_rust(
                src_bgra,
                src_stride,
                dst_y,
                dst_stride_y,
                dst_u,
                dst_stride_u,
                dst_v,
                dst_stride_v,
                width,
                height,
                profile,
            ),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        convert_apple_bgra_to_i420_rust(
            src_bgra,
            src_stride,
            dst_y,
            dst_stride_y,
            dst_u,
            dst_stride_u,
            dst_v,
            dst_stride_v,
            width,
            height,
            profile,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn validated_bgra_layout(
    src_bgra: &[u8],
    src_stride: usize,
    dst_y: &[u8],
    dst_stride_y: u32,
    dst_u: &[u8],
    dst_stride_u: u32,
    dst_v: &[u8],
    dst_stride_v: u32,
    width: u32,
    height: u32,
) -> Option<()> {
    if width == 0
        || height == 0
        || width > i32::MAX as u32
        || height > i32::MAX as u32
        || src_stride > i32::MAX as usize
        || dst_stride_y > i32::MAX as u32
        || dst_stride_u > i32::MAX as u32
        || dst_stride_v > i32::MAX as u32
        // libyuv's ARGB row-pair loop doubles these two strides in signed
        // int arithmetic before advancing to the second row.
        || (height >= 2
            && (src_stride > (i32::MAX / 2) as usize
                || dst_stride_y > (i32::MAX / 2) as u32))
    {
        return None;
    }

    let width = width as usize;
    let height = height as usize;
    let src_row_bytes = width.checked_mul(4)?;
    let chroma_width = width.checked_add(1)?.checked_div(2)?;
    let chroma_height = height.checked_add(1)?.checked_div(2)?;
    let dst_stride_y = dst_stride_y as usize;
    let dst_stride_u = dst_stride_u as usize;
    let dst_stride_v = dst_stride_v as usize;

    if src_stride < src_row_bytes
        || dst_stride_y < width
        || dst_stride_u < chroma_width
        || dst_stride_v < chroma_width
        // Captured BGRA contains the complete bytes-per-row * height extent.
        // Reject both shorter and larger buffers so stale geometry cannot be
        // paired with a payload from another resolution before libyuv (#500).
        || src_bgra.len() != strided_plane_full_len(src_stride, height)?
        // libyuv advances destination pointers by whole strides after its
        // final row pair, so every destination needs full-stride backing.
        || dst_y.len() < strided_plane_full_len(dst_stride_y, height)?
        || dst_u.len() < strided_plane_full_len(dst_stride_u, chroma_height)?
        || dst_v.len() < strided_plane_full_len(dst_stride_v, chroma_height)?
    {
        return None;
    }

    Some(())
}

#[allow(clippy::too_many_arguments)]
fn convert_apple_bgra_to_i420_rust(
    src_bgra: &[u8],
    src_stride: usize,
    dst_y: &mut [u8],
    dst_stride_y: u32,
    dst_u: &mut [u8],
    dst_stride_u: u32,
    dst_v: &mut [u8],
    dst_stride_v: u32,
    width: u32,
    height: u32,
    profile: VideoColorProfile,
) -> bool {
    let width = width as usize;
    let height = height as usize;
    let dst_stride_y = dst_stride_y as usize;
    let dst_stride_u = dst_stride_u as usize;
    let dst_stride_v = dst_stride_v as usize;
    if width == 0 || height == 0 {
        return true;
    }
    if src_stride < width.saturating_mul(4)
        || src_bgra.len() < src_stride.saturating_mul(height)
        || dst_y.len() < dst_stride_y.saturating_mul(height)
    {
        log::warn!("publisher: BGRA->I420 input/output buffer is too small for {width}x{height}");
        return false;
    }

    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    if dst_u.len() < dst_stride_u.saturating_mul(chroma_height)
        || dst_v.len() < dst_stride_v.saturating_mul(chroma_height)
        || dst_stride_y < width
        || dst_stride_u < chroma_width
        || dst_stride_v < chroma_width
    {
        log::warn!("publisher: I420 destination strides are too small for {width}x{height}");
        return false;
    }

    for chroma_y in 0..chroma_height {
        let src_y0 = chroma_y * 2;
        for chroma_x in 0..chroma_width {
            let src_x0 = chroma_x * 2;
            let mut cb_sum = 0u16;
            let mut cr_sum = 0u16;
            let mut samples = 0u16;

            for dy in 0..2 {
                let y = src_y0 + dy;
                if y >= height {
                    continue;
                }
                let src_row = y * src_stride;
                let dst_y_row = y * dst_stride_y;
                for dx in 0..2 {
                    let x = src_x0 + dx;
                    if x >= width {
                        continue;
                    }
                    let src_offset = src_row + (x * 4);
                    let pixel = [
                        src_bgra[src_offset],
                        src_bgra[src_offset + 1],
                        src_bgra[src_offset + 2],
                        src_bgra[src_offset + 3],
                    ];
                    let ycbcr = video_color::rgb_to_ycbcr_8bit(
                        video_color::apple_bgra_to_rgb8(pixel),
                        profile,
                    );
                    dst_y[dst_y_row + x] = ycbcr.y;
                    cb_sum += u16::from(ycbcr.cb);
                    cr_sum += u16::from(ycbcr.cr);
                    samples += 1;
                }
            }

            let rounding = samples / 2;
            dst_u[(chroma_y * dst_stride_u) + chroma_x] = ((cb_sum + rounding) / samples) as u8;
            dst_v[(chroma_y * dst_stride_v) + chroma_x] = ((cr_sum + rounding) / samples) as u8;
        }
    }

    true
}

#[allow(clippy::too_many_arguments)]
fn convert_nv12_to_i420(
    y: &[u8],
    y_stride: u32,
    uv: &[u8],
    uv_stride: u32,
    dst_y: &mut [u8],
    dst_stride_y: u32,
    dst_u: &mut [u8],
    dst_stride_u: u32,
    dst_v: &mut [u8],
    dst_stride_v: u32,
    width: u32,
    height: u32,
) -> bool {
    convert_nv12_to_i420_with_scratch(
        &mut Nv12Scratch::default(),
        y,
        y_stride,
        uv,
        uv_stride,
        dst_y,
        dst_stride_y,
        dst_u,
        dst_stride_u,
        dst_v,
        dst_stride_v,
        width,
        height,
    )
}

#[allow(clippy::too_many_arguments)]
fn convert_nv12_to_i420_with_scratch(
    scratch: &mut Nv12Scratch,
    y: &[u8],
    y_stride: u32,
    uv: &[u8],
    uv_stride: u32,
    dst_y: &mut [u8],
    dst_stride_y: u32,
    dst_u: &mut [u8],
    dst_stride_u: u32,
    dst_v: &mut [u8],
    dst_stride_v: u32,
    width: u32,
    height: u32,
) -> bool {
    let Some(layout) = validated_nv12_layout(
        y,
        y_stride,
        uv,
        uv_stride,
        dst_y,
        dst_stride_y,
        dst_u,
        dst_stride_u,
        dst_v,
        dst_stride_v,
        width,
        height,
    ) else {
        return false;
    };

    // CoreVideo owns padding implied by bytes-per-row, but the copied plane
    // ends exactly at its last reported byte. libyuv's vectorized row kernels
    // may read a full vector at the visible tail. Normalize into initialized,
    // over-padded rows before crossing the unsafe FFI boundary (#328).
    let Some(y_padded_stride) = fill_padded_nv12_plane(
        &mut scratch.y,
        y,
        y_stride as usize,
        layout.width,
        layout.height,
    ) else {
        return false;
    };
    let Some(uv_padded_stride) = fill_padded_nv12_plane(
        &mut scratch.uv,
        uv,
        uv_stride as usize,
        layout.uv_row_bytes,
        layout.chroma_height,
    ) else {
        return false;
    };

    #[cfg(target_os = "macos")]
    {
        unsafe {
            yuv_sys::rs_NV12ToI420(
                scratch.y.as_ptr(),
                y_padded_stride as i32,
                scratch.uv.as_ptr(),
                uv_padded_stride as i32,
                dst_y.as_mut_ptr(),
                dst_stride_y as i32,
                dst_u.as_mut_ptr(),
                dst_stride_u as i32,
                dst_v.as_mut_ptr(),
                dst_stride_v as i32,
                width as i32,
                height as i32,
            ) == 0
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        copy_nv12_to_i420_rust(
            &scratch.y,
            y_padded_stride,
            &scratch.uv,
            uv_padded_stride,
            dst_y,
            dst_stride_y as usize,
            dst_u,
            dst_stride_u as usize,
            dst_v,
            dst_stride_v as usize,
            layout,
        )
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn copy_nv12_to_i420_rust(
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
    dst_y: &mut [u8],
    dst_stride_y: usize,
    dst_u: &mut [u8],
    dst_stride_u: usize,
    dst_v: &mut [u8],
    dst_stride_v: usize,
    layout: ValidatedNv12Layout,
) -> bool {
    for row in 0..layout.height {
        let source = &y[row * y_stride..row * y_stride + layout.width];
        let destination = &mut dst_y[row * dst_stride_y..row * dst_stride_y + layout.width];
        destination.copy_from_slice(source);
    }

    let chroma_width = layout.width.div_ceil(2);
    for row in 0..layout.chroma_height {
        let source = &uv[row * uv_stride..row * uv_stride + layout.uv_row_bytes];
        let u_row = &mut dst_u[row * dst_stride_u..row * dst_stride_u + chroma_width];
        let v_row = &mut dst_v[row * dst_stride_v..row * dst_stride_v + chroma_width];
        for column in 0..chroma_width {
            u_row[column] = source[column * 2];
            v_row[column] = source[column * 2 + 1];
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValidatedNv12Layout {
    width: usize,
    height: usize,
    chroma_height: usize,
    uv_row_bytes: usize,
}

#[derive(Debug, Default)]
struct Nv12Scratch {
    y: Vec<u8>,
    uv: Vec<u8>,
}

#[allow(clippy::too_many_arguments)]
fn validated_nv12_layout(
    y: &[u8],
    y_stride: u32,
    uv: &[u8],
    uv_stride: u32,
    dst_y: &[u8],
    dst_stride_y: u32,
    dst_u: &[u8],
    dst_stride_u: u32,
    dst_v: &[u8],
    dst_stride_v: u32,
    width: u32,
    height: u32,
) -> Option<ValidatedNv12Layout> {
    if width == 0
        || height == 0
        || width > i32::MAX as u32
        || height > i32::MAX as u32
        || y_stride > i32::MAX as u32
        || uv_stride > i32::MAX as u32
        || dst_stride_y > i32::MAX as u32
        || dst_stride_u > i32::MAX as u32
        || dst_stride_v > i32::MAX as u32
    {
        return None;
    }

    let width = width as usize;
    let height = height as usize;
    let chroma_width = width.checked_add(1)?.checked_div(2)?;
    let chroma_height = height.checked_add(1)?.checked_div(2)?;
    let uv_row_bytes = chroma_width.checked_mul(2)?;
    let y_stride = y_stride as usize;
    let uv_stride = uv_stride as usize;
    let dst_stride_y = dst_stride_y as usize;
    let dst_stride_u = dst_stride_u as usize;
    let dst_stride_v = dst_stride_v as usize;

    if y_stride < width
        || uv_stride < uv_row_bytes
        || dst_stride_y < width
        || dst_stride_u < chroma_width
        || dst_stride_v < chroma_width
        // The source slices come from one locked CVPixelBuffer plane and are
        // copied with its complete bytes-per-row * plane-height extent. Do
        // not accept a larger plane here: that can mean dimensions/strides
        // from a previous resolution were paired with the current buffer,
        // causing each following row to be read at the wrong vertical offset
        // (#495). A mismatched frame must be dropped before libyuv sees it.
        || y.len() != strided_plane_full_len(y_stride, height)?
        || uv.len() != strided_plane_full_len(uv_stride, chroma_height)?
        || dst_y.len() < strided_plane_len(dst_stride_y, height, width)?
        || dst_u.len() < strided_plane_len(dst_stride_u, chroma_height, chroma_width)?
        || dst_v.len() < strided_plane_len(dst_stride_v, chroma_height, chroma_width)?
    {
        return None;
    }

    Some(ValidatedNv12Layout {
        width,
        height,
        chroma_height,
        uv_row_bytes,
    })
}

fn strided_plane_len(stride: usize, rows: usize, visible_row_bytes: usize) -> Option<usize> {
    rows.checked_sub(1)?
        .checked_mul(stride)?
        .checked_add(visible_row_bytes)
}

fn strided_plane_full_len(stride: usize, rows: usize) -> Option<usize> {
    stride.checked_mul(rows)
}

fn fill_padded_nv12_plane(
    bytes: &mut Vec<u8>,
    src: &[u8],
    src_stride: usize,
    visible_row_bytes: usize,
    rows: usize,
) -> Option<usize> {
    const SIMD_ALIGNMENT: usize = 64;
    const SIMD_TAIL_PADDING: usize = 64;

    let aligned = visible_row_bytes
        .checked_add(SIMD_ALIGNMENT - 1)?
        .checked_div(SIMD_ALIGNMENT)?
        .checked_mul(SIMD_ALIGNMENT)?;
    let stride = aligned.checked_add(SIMD_TAIL_PADDING)?;
    if stride > i32::MAX as usize {
        return None;
    }
    let len = stride.checked_mul(rows)?;
    if bytes.capacity() < len {
        bytes.try_reserve_exact(len - bytes.capacity()).ok()?;
    }
    bytes.resize(len, 0);
    bytes.fill(0);
    for row in 0..rows {
        let src_start = row.checked_mul(src_stride)?;
        let src_end = src_start.checked_add(visible_row_bytes)?;
        let dst_start = row.checked_mul(stride)?;
        let dst_end = dst_start.checked_add(visible_row_bytes)?;
        bytes
            .get_mut(dst_start..dst_end)?
            .copy_from_slice(src.get(src_start..src_end)?);
    }
    Some(stride)
}

#[cfg(test)]
mod track_name_tests {
    use super::*;

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ContractFixture {
        window_tracks: Vec<WindowTrackVector>,
        camera_tracks: Vec<CameraTrackVector>,
        source_kind_metadata: SourceKindMetadataVector,
        source_scale_metadata: SourceScaleMetadataVector,
        identity_palette_metadata: IdentityPaletteMetadataVector,
        window_z_order_metadata: WindowZOrderMetadataVector,
        window_url_metadata: WindowUrlMetadataVector,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WindowTrackVector {
        window_id: u32,
        track_name: String,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CameraTrackVector {
        identity: String,
        track_name: String,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SourceKindMetadataVector {
        metadata: String,
        display_window_id: u32,
        window_window_id: u32,
        missing_window_id: u32,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SourceScaleMetadataVector {
        metadata: String,
        downscaled_window_id: u32,
        retina_window_id: u32,
        invalid_window_id: u32,
        missing_window_id: u32,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct IdentityPaletteMetadataVector {
        metadata: String,
        palette_index: u8,
        window_id: u32,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WindowZOrderMetadataVector {
        metadata: String,
        ordered_window_ids: Vec<u32>,
        frontmost_window_id: u32,
        middle_window_id: u32,
        backmost_window_id: u32,
        missing_window_id: u32,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WindowUrlMetadataVector {
        metadata: String,
        plain_window_id: u32,
        plain_url: String,
        minimized_window_id: u32,
        minimized_url: String,
        non_http_window_id: u32,
        missing_window_id: u32,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!(
            "../../../../../contracts/petal-contracts.json"
        ))
        .unwrap()
    }

    #[test]
    fn round_trips_window_id_through_track_name() {
        let name = track_name_for_window(4242);
        assert_eq!(name, "petal-window-4242");
        assert_eq!(window_id_from_track_name(&name), Some(4242));
    }

    #[test]
    fn non_matching_track_names_parse_to_none() {
        assert_eq!(window_id_from_track_name("petal-window-capture"), None);
        assert_eq!(window_id_from_track_name("camera"), None);
        assert_eq!(window_id_from_track_name(""), None);
    }

    #[test]
    fn camera_window_id_is_stable_and_high_bit_tagged() {
        let a = camera_window_id("petal-camera-web-tester");
        let b = camera_window_id("petal-camera-web-tester");
        assert_eq!(a, b, "same name must hash to the same id");
        assert!(
            a & 0x8000_0000 != 0,
            "high bit must be set to avoid CGWindowID collisions"
        );
        let c = camera_window_id("petal-camera-other");
        assert_ne!(a, c, "different names should (practically) not collide");
    }

    #[test]
    fn shared_window_title_round_trips_through_participant_metadata() {
        let mut metadata = ShareMetadata::default();
        metadata.identity_palette_index = Some(2);
        metadata.titles.insert(4242, "SPEC.md - Petal".to_string());
        metadata.titles.insert(7, "Terminal".to_string());
        metadata.scales.insert(4242, 1.6);
        metadata.urls.insert(
            4242,
            "https://example.com/spec?token=secret#section".to_string(),
        );
        metadata
            .color_profiles
            .insert(4242, VideoColorProfile::SRGB_BT709_FULL);
        metadata
            .color_profiles
            .insert(7, VideoColorProfile::BT601_VIDEO);
        metadata.kinds.insert(4242, SharedSourceKind::DisplayRegion);
        metadata.regions.insert(
            4242,
            SharedRegionDescriptor {
                display_id: 7,
                x: 10.0,
                y: 20.0,
                width: 640.0,
                height: 480.0,
                display_local_x: 30.0,
                display_local_y: 40.0,
                display_local_width: 640.0,
                display_local_height: 480.0,
                physical_width: 1280,
                physical_height: 960,
                scale: 2.0,
                generation: 3,
            },
        );

        let metadata = encode_window_metadata(&metadata);

        assert_eq!(
            shared_window_title_from_metadata(&metadata, 4242),
            Some("SPEC.md - Petal".to_string())
        );
        assert_eq!(
            shared_window_scale_from_metadata(&metadata, 4242),
            Some(1.6)
        );
        assert_eq!(
            shared_window_title_from_metadata(&metadata, 7),
            Some("Terminal".to_string())
        );
        assert_eq!(
            shared_window_url_from_metadata(&metadata, 4242),
            Some("https://example.com/spec".to_string())
        );
        assert_eq!(
            shared_window_color_profile_from_metadata(&metadata, 4242),
            Some(VideoColorProfile::SRGB_BT709_FULL)
        );
        assert_eq!(
            shared_window_color_profile_from_metadata(&metadata, 7),
            Some(VideoColorProfile::BT601_VIDEO)
        );
        assert_eq!(
            shared_window_kind_from_metadata(&metadata, 4242),
            SharedSourceKind::DisplayRegion
        );
        assert_eq!(
            shared_window_region_physical_size_from_metadata(&metadata, 4242),
            Some((1280, 960))
        );
        let region = serde_json::from_str::<serde_json::Value>(&metadata)
            .unwrap()[PETAL_WINDOW_REGIONS_METADATA_KEY]["4242"]
            .clone();
        assert_eq!(region["displayLocalX"], 30.0);
        assert_eq!(region["displayLocalY"], 40.0);
        assert_eq!(region["displayLocalWidth"], 640.0);
        assert_eq!(region["displayLocalHeight"], 480.0);
        assert_eq!(identity_palette_index_from_metadata(&metadata), Some(2));
        assert_eq!(
            shared_window_kind_from_metadata(&metadata, 7),
            SharedSourceKind::Window
        );
        assert_eq!(shared_window_title_from_metadata(&metadata, 999), None);
        assert_eq!(
            shared_window_color_profile_from_metadata(&metadata, 999),
            None
        );
    }

    #[test]
    fn display_control_mode_defaults_to_full_control_when_metadata_is_missing() {
        let display = r#"{"petalWindowKinds":{"7":"display"}}"#;
        assert_eq!(
            shared_window_control_mode_from_metadata(display, 7),
            crate::remote_control_core::RemoteControlMode::FullControl
        );
        let window = r#"{"petalWindowKinds":{"7":"window"}}"#;
        assert_eq!(
            shared_window_control_mode_from_metadata(window, 7),
            crate::remote_control_core::RemoteControlMode::CursorPreserving
        );
    }

    #[test]
    fn identity_palette_metadata_matches_shared_contract_fixture() {
        let fixture = contract_fixture().identity_palette_metadata;
        assert_eq!(
            identity_palette_index_from_metadata(&fixture.metadata),
            Some(fixture.palette_index)
        );
        assert_eq!(
            shared_window_kind_from_metadata(&fixture.metadata, fixture.window_id),
            SharedSourceKind::Display
        );
    }

    #[test]
    fn window_z_order_round_trips_through_participant_metadata_and_merges_non_destructively() {
        let mut metadata = ShareMetadata::default();
        metadata.titles.insert(42, "SPEC.md - Petal".to_string());
        metadata.window_order = vec![42, 7, 99];

        let encoded = encode_window_metadata(&metadata);

        assert_eq!(
            shared_window_z_order_from_metadata(&encoded),
            Some(vec![42, 7, 99])
        );
        assert_eq!(shared_window_z_rank_from_metadata(&encoded, 42), Some(0));
        assert_eq!(shared_window_z_rank_from_metadata(&encoded, 7), Some(1));
        assert_eq!(shared_window_z_rank_from_metadata(&encoded, 99), Some(2));
        assert_eq!(shared_window_z_rank_from_metadata(&encoded, 12345), None);
        // The merge-preserving contract (docs/CONTRACTS.md): an unrelated key
        // published earlier must survive encoding the z-order alongside it.
        assert_eq!(
            shared_window_title_from_metadata(&encoded, 42),
            Some("SPEC.md - Petal".to_string())
        );
    }

    #[test]
    fn window_z_order_metadata_matches_shared_contract_fixture() {
        let fixture = contract_fixture().window_z_order_metadata;
        assert_eq!(
            shared_window_z_order_from_metadata(&fixture.metadata),
            Some(fixture.ordered_window_ids.clone())
        );
        assert_eq!(
            shared_window_z_rank_from_metadata(&fixture.metadata, fixture.frontmost_window_id),
            Some(0)
        );
        assert_eq!(
            shared_window_z_rank_from_metadata(&fixture.metadata, fixture.middle_window_id),
            Some(1)
        );
        assert_eq!(
            shared_window_z_rank_from_metadata(&fixture.metadata, fixture.backmost_window_id),
            Some(2)
        );
        assert_eq!(
            shared_window_z_rank_from_metadata(&fixture.metadata, fixture.missing_window_id),
            None
        );
    }

    #[test]
    fn window_z_order_metadata_is_absent_for_an_older_sharer() {
        // An older sharer's metadata never carries the key at all -- this
        // must decode as None (no rank data), not as Some(vec![]).
        assert_eq!(
            shared_window_z_order_from_metadata(r#"{"petalWindowKinds":{"42":"window"}}"#),
            None
        );
        assert_eq!(shared_window_z_rank_from_metadata("{}", 42), None);
        assert_eq!(shared_window_z_order_from_metadata("not json"), None);
    }

    #[test]
    fn window_z_order_metadata_rejects_malformed_entries() {
        assert_eq!(
            shared_window_z_order_from_metadata(r#"{"petalWindowZOrder":"not-an-array"}"#),
            None
        );
        assert_eq!(
            shared_window_z_order_from_metadata(r#"{"petalWindowZOrder":[42,-1,99]}"#),
            None
        );
        assert_eq!(
            shared_window_z_order_from_metadata(r#"{"petalWindowZOrder":[42,"seven",99]}"#),
            None
        );
        // An explicitly-published empty order (nothing currently shared) is
        // valid and distinct from "key absent".
        assert_eq!(
            shared_window_z_order_from_metadata(r#"{"petalWindowZOrder":[]}"#),
            Some(vec![])
        );
    }

    #[test]
    fn window_z_order_republishes_only_when_the_order_actually_changes() {
        let mut metadata = ShareMetadata::default();

        // First publish of a real order is a change.
        assert_eq!(
            stage_shared_window_order(&mut metadata, vec![42, 7]),
            Some(vec![42, 7])
        );
        assert_eq!(metadata.window_order, vec![42, 7]);

        // Same order again (e.g. an unrelated unshared-window reshuffle
        // elsewhere on screen that doesn't touch the shared subset) must be
        // a no-op -- no republish.
        assert_eq!(stage_shared_window_order(&mut metadata, vec![42, 7]), None);
        assert_eq!(metadata.window_order, vec![42, 7]);

        // The shared subset's own order changing IS a change.
        assert_eq!(
            stage_shared_window_order(&mut metadata, vec![7, 42]),
            Some(vec![7, 42])
        );
        assert_eq!(metadata.window_order, vec![7, 42]);

        // A membership change (window added/removed from the shared subset)
        // is also a change even if it happens to keep a shared prefix.
        assert_eq!(
            stage_shared_window_order(&mut metadata, vec![7, 42, 99]),
            Some(vec![7, 42, 99])
        );
    }

    #[test]
    fn shared_window_scale_metadata_preserves_downscaled_capture_scale() {
        let mut metadata = ShareMetadata::default();
        metadata.scales.insert(4242, 0.64);

        let metadata = encode_window_metadata(&metadata);

        assert_eq!(
            shared_window_scale_from_metadata(&metadata, 4242),
            Some(0.64)
        );
    }

    #[test]
    fn post_unpublish_reshare_cannot_clear_new_generation_metadata() {
        let window_id = 42;
        let old_generation = 7;
        let new_generation = 8;
        let mut metadata = ShareMetadata::default();

        // The old stop completed its SDK unpublish and already decided to
        // clear. Before the actual metadata mutation runs, the same native
        // window is re-shared and publishes its new title generation.
        metadata.titles.insert(window_id, "New title".to_string());
        metadata.generations.insert(window_id, new_generation);
        metadata.scales.insert(window_id, 2.0);

        assert!(!clear_share_metadata_for_generation(
            &mut metadata,
            window_id,
            old_generation,
        ));
        assert_eq!(
            metadata.titles.get(&window_id).map(String::as_str),
            Some("New title")
        );
        assert_eq!(metadata.generations.get(&window_id), Some(&new_generation));
        assert_eq!(metadata.scales.get(&window_id), Some(&2.0));
    }

    #[test]
    fn shared_window_scale_metadata_matches_shared_contract_fixture() {
        let fixture = contract_fixture().source_scale_metadata;
        assert_eq!(
            shared_window_scale_from_metadata(&fixture.metadata, fixture.downscaled_window_id),
            Some(0.64)
        );
        assert_eq!(
            shared_window_scale_from_metadata(&fixture.metadata, fixture.retina_window_id),
            Some(1.5)
        );
        assert_eq!(
            shared_window_scale_from_metadata(&fixture.metadata, fixture.invalid_window_id),
            None
        );
        assert_eq!(
            shared_window_scale_from_metadata(&fixture.metadata, fixture.missing_window_id),
            None
        );
    }

    #[test]
    fn shared_window_title_metadata_ignores_bad_or_blank_values() {
        assert_eq!(shared_window_title_from_metadata("not json", 42), None);
        assert_eq!(
            shared_window_title_from_metadata(r#"{"petalWindowTitles":{"42":"   "}}"#, 42),
            None
        );
        assert_eq!(
            shared_window_title_from_metadata(r#"{"petalWindowTitles":{"42":123}}"#, 42),
            None
        );
        assert_eq!(
            shared_window_url_from_metadata(r#"{"petalWindowUrls":{"42":"file:///tmp/nope"}}"#, 42),
            None
        );
        assert_eq!(
            shared_window_color_profile_from_metadata(
                r#"{"petalWindowColorProfiles":{"42":{"primaries":"bt709","transfer":"srgb","matrix":"made-up","range":"full"}}}"#,
                42
            ),
            None
        );
        assert_eq!(
            shared_window_kind_from_metadata(r#"{"petalWindowKinds":{"42":"screen"}}"#, 42),
            SharedSourceKind::Display
        );
        assert_eq!(
            shared_window_kind_from_metadata(r#"{"petalWindowKinds":{"42":"spaceship"}}"#, 42),
            SharedSourceKind::Window
        );
    }

    #[test]
    fn shared_window_url_metadata_strips_query_and_fragment() {
        let mut metadata = ShareMetadata::default();
        metadata.urls.insert(
            42,
            "https://example.com/docs/path?token=secret#section".to_string(),
        );

        let encoded = encode_window_metadata(&metadata);

        assert_eq!(
            shared_window_url_from_metadata(&encoded, 42),
            Some("https://example.com/docs/path".to_string())
        );
        assert!(!encoded.contains("token=secret"));
        assert!(!encoded.contains("#section"));
    }

    #[test]
    fn shared_window_url_metadata_reader_minimizes_full_urls_from_wire() {
        assert_eq!(
            shared_window_url_from_metadata(
                r#"{"petalWindowUrls":{"42":"https://example.com/docs?token=secret#section"}}"#,
                42
            ),
            Some("https://example.com/docs".to_string())
        );
    }

    // Contract pin (#915): shared/web-harness must minimize the same way
    // native does -- query/fragment stripped, non-http(s) dropped to None.
    // See contracts/petal-contracts.json's `windowUrlMetadata` and
    // web-harness/tests/contracts.test.ts's matching read of the same
    // fixture entry.
    #[test]
    fn window_url_metadata_matches_shared_contract_fixture() {
        let fixture = contract_fixture().window_url_metadata;
        assert_eq!(
            shared_window_url_from_metadata(&fixture.metadata, fixture.plain_window_id),
            Some(fixture.plain_url.clone())
        );
        assert_eq!(
            shared_window_url_from_metadata(&fixture.metadata, fixture.minimized_window_id),
            Some(fixture.minimized_url.clone())
        );
        assert_eq!(
            shared_window_url_from_metadata(&fixture.metadata, fixture.non_http_window_id),
            None
        );
        assert_eq!(
            shared_window_url_from_metadata(&fixture.metadata, fixture.missing_window_id),
            None
        );
    }

    #[test]
    fn share_quality_capture_fps_matches_encoder_tier() {
        // #907/#383: dropped from 60 -> 30. 60fps on the top rung was
        // cosmetic for this product's content and directly inflated the
        // top-rung bitrate ask that starved on a constrained link.
        assert_eq!(ShareQuality::Full.capture_fps(), 30);
        assert_eq!(ShareQuality::Reduced.capture_fps(), 4);

        assert_eq!(
            ShareQuality::Full.video_encoding(1280, 720).max_framerate,
            30.0
        );
        assert_eq!(
            ShareQuality::Reduced
                .video_encoding(1280, 720)
                .max_framerate,
            4.0
        );
    }

    #[test]
    fn full_window_publish_options_cover_every_share_ladder() {
        for (ladder, expected_layers) in [
            (
                FullShareSimulcastLadder::Legacy,
                vec![(480, 270, 625_000, 15.0), (960, 540, 1_250_000, 30.0)],
            ),
            (
                FullShareSimulcastLadder::LegacyBottom30,
                vec![(480, 270, 625_000, 30.0), (960, 540, 1_250_000, 30.0)],
            ),
            (
                FullShareSimulcastLadder::Raised,
                vec![(960, 540, 1_250_000, 30.0), (1440, 810, 2_812_500, 30.0)],
            ),
            (
                FullShareSimulcastLadder::TwoRung,
                vec![(1440, 810, 2_812_500, 30.0)],
            ),
            (
                FullShareSimulcastLadder::TwoRungHalf,
                vec![(960, 540, 1_250_000, 30.0)],
            ),
        ] {
            let options = window_publish_options(1920, 1080, ShareQuality::Full, ladder);

            assert_eq!(options.video_codec, VideoCodec::H264);
            assert_eq!(options.video_encoder, select_encoder_backend());
            assert_eq!(
                options.h264_profile_preference,
                H264ProfilePreference::HighFirst
            );
            assert!(!options.preconnect_buffer);
            assert!(options.frame_metadata_features.user_timestamp);
            assert!(options.frame_metadata_features.frame_id);
            assert!(options.simulcast);

            let full_encoding = options
                .video_encoding
                .as_ref()
                .expect("full share must keep an explicit top-layer ceiling");
            // #907: top-layer ceiling is now one formula on both platforms,
            // then budgeted against this ladder's own lower rungs so the
            // COMBINED ask stays inside `FULL_SIMULCAST_TOTAL_BUDGET_BPS`
            // (8 Mbps) -- see `budgeted_top_bitrate`. Raw (pre-budget) top at
            // 1920x1080/30fps is 11,197,440; every ladder below converges to
            // exactly the 8 Mbps budget (none of these lower-rung totals are
            // large enough to hit the absolute floor).
            let expected_top: u64 = match ladder {
                FullShareSimulcastLadder::Legacy | FullShareSimulcastLadder::LegacyBottom30 => {
                    6_125_000
                }
                FullShareSimulcastLadder::Raised => 3_937_500,
                FullShareSimulcastLadder::TwoRung => 5_187_500,
                FullShareSimulcastLadder::TwoRungHalf => 6_750_000,
            };
            assert_eq!(full_encoding.max_bitrate, expected_top, "{ladder:?}");
            assert_eq!(full_encoding.max_framerate, 30.0);

            let layers = options
                .simulcast_layers
                .as_ref()
                .expect("full share must provide explicit simulcast layers");
            assert_eq!(layers.len(), expected_layers.len(), "{ladder:?}");
            for (layer, (width, height, bitrate, fps)) in layers.iter().zip(expected_layers) {
                assert_eq!(layer.width, width, "{ladder:?}");
                assert_eq!(layer.height, height, "{ladder:?}");
                assert_eq!(layer.encoding.max_bitrate, bitrate, "{ladder:?}");
                assert_eq!(layer.encoding.max_framerate, fps, "{ladder:?}");
            }
        }
    }

    #[test]
    fn display_region_publish_options_use_one_native_geometry_layer() {
        let options = window_publish_options_for_region(
            752,
            852,
            ShareQuality::Full,
            FullShareSimulcastLadder::TwoRung,
            true,
        );
        assert!(!options.simulcast);
        assert!(options.simulcast_layers.is_none());
        let log = full_share_ladder_log(FullShareSimulcastLadder::TwoRung, 752, 852, &options);
        assert!(log.contains("rid=native 752x852"));
        assert!(options.frame_metadata_features.user_timestamp);
        assert!(options.frame_metadata_features.frame_id);
    }

    #[test]
    fn two_rung_half_publish_log_uses_the_resolved_shape() {
        let options = window_publish_options(
            1920,
            1080,
            ShareQuality::Full,
            FullShareSimulcastLadder::TwoRungHalf,
        );
        let log =
            full_share_ladder_log(FullShareSimulcastLadder::TwoRungHalf, 1920, 1080, &options);

        assert!(log.contains("ladder=two-rung-half"));
        assert!(log.contains("rid=q 960x540 30fps 1250000bps"));
        // #907: one formula on both platforms now, budgeted against the
        // 1,250,000bps lower rung (8,000,000 - 1,250,000 = 6,750,000).
        assert!(log.contains("rid=h 1920x1080 30fps 6750000bps"));
    }

    #[test]
    fn share_quality_video_encoding_scales_bitrate_with_pixels_and_fps() {
        // #907: one formula on both platforms now -- h_bps = 0.18 bpp/frame
        // * pixels * max_framerate, clamped to [4, 16] Mbps (2026-08-12;
        // raised from 0.13 after the encoder-stats diagnostic showed a
        // 1215x719 share at ~4Mbps/QP 26 — 'readable, not crisp' — per the
        // crispness C2 lever). These are the RAW, pre-budget values that
        // `video_encoding` returns; `budgeted_top_bitrate` (exercised by
        // `full_window_publish_options_cover_every_share_ladder` and
        // `layer_parameters`) applies the total-ladder-budget clamp on top
        // of this. Full now runs at capture_fps()=30 (#907/#383), half the
        // values this formula produced at 60.
        assert_eq!(
            ShareQuality::Full.video_encoding(1280, 720).max_bitrate,
            4_976_640
        );
        assert_eq!(
            ShareQuality::Full.video_encoding(1708, 732).max_bitrate,
            6_751_382
        );
        assert_eq!(
            ShareQuality::Full.video_encoding(1920, 1080).max_bitrate,
            11_197_440
        );
        assert_eq!(
            ShareQuality::Full.video_encoding(2560, 1440).max_bitrate,
            16_000_000
        );
        assert_eq!(
            ShareQuality::Full.video_encoding(3840, 2160).max_bitrate,
            16_000_000
        );
        // A small share floors at the 4 Mbps minimum.
        assert_eq!(
            ShareQuality::Full.video_encoding(502, 735).max_bitrate,
            4_000_000
        );
        // Reduced still runs at capture_fps()=4 (unaffected by the Full-only
        // fps change) -> the formula yields ~5.97 Mbps for 4K, halved by the
        // Reduced branch.
        assert_eq!(
            ShareQuality::Reduced.video_encoding(3840, 2160).max_bitrate,
            2_985_984
        );
    }

    #[test]
    fn full_share_simulcast_half_layer_bitrate_scales_with_source_resolution() {
        let hd_options = window_publish_options(
            1920,
            1080,
            ShareQuality::Full,
            FullShareSimulcastLadder::Legacy,
        );
        let uhd_options = window_publish_options(
            3840,
            2160,
            ShareQuality::Full,
            FullShareSimulcastLadder::Legacy,
        );

        let hd_half = &hd_options
            .simulcast_layers
            .as_ref()
            .expect("1080p full share must provide explicit simulcast layers")[1];
        let uhd_half = &uhd_options
            .simulcast_layers
            .as_ref()
            .expect("4K full share must provide explicit simulcast layers")[1];
        let uhd_top = uhd_options
            .video_encoding
            .as_ref()
            .expect("4K full share must keep an explicit top-layer ceiling");

        assert_eq!(hd_half.encoding.max_bitrate, 1_250_000);
        assert_eq!(uhd_half.width, 1920);
        assert_eq!(uhd_half.height, 1080);
        assert_eq!(uhd_half.encoding.max_bitrate, 5_000_000);
        assert!(
            uhd_half.encoding.max_bitrate > hd_half.encoding.max_bitrate,
            "4K shares need a stronger half-res layer than 1080p shares"
        );
        // #907: 4K's Legacy ladder asks 625,000 + 5,000,000 = 5,625,000 from
        // its OWN lower rungs, leaving 2,375,000 of the 8 Mbps total budget
        // for the top rung -- comfortably above the absolute 1.5 Mbps floor,
        // so the budget is hit exactly rather than clamped.
        assert_eq!(uhd_top.max_bitrate, 2_375_000);
        // #907 review (counselors): the top rung is deliberately NOT floored
        // at "never below a lower rung's own ceiling" -- an earlier version
        // of `budgeted_top_bitrate` did exactly that and it doubled the
        // total ask at 4K on ladders including the shipped default
        // (`TwoRung`). At this size the nominal top rung's configured
        // ceiling (2,375,000) is genuinely BELOW the half rung's own
        // (5,000,000); this is a known, accepted limitation for 4K-class
        // shares now that the runtime starvation guards, not a static
        // ceiling ordering, are what protect a viewer from a badly-funded
        // top rung. See `budgeted_top_bitrate`'s doc comment.
        assert!(
            uhd_top.max_bitrate < uhd_half.encoding.max_bitrate,
            "documents the accepted 4K-class limitation, not a requirement"
        );
    }

    // ---- #907: total-ladder-budget clamp -----------------------------------
    //
    // The field incident: a two-rung ladder asked 2.8 + 8.0 = 10.8 Mbps on a
    // link that carried 2.6, because each rung's ceiling was computed with no
    // awareness of the other. `budgeted_top_bitrate` closes that gap.

    #[test]
    fn budgeted_top_bitrate_fits_inside_the_total_budget_when_room_allows() {
        // Plenty of budget left after the lower rung: top gets the smaller of
        // its raw ceiling and the remaining budget.
        assert_eq!(budgeted_top_bitrate(11_197_440, 2_812_500), 5_187_500);
    }

    #[test]
    fn budgeted_top_bitrate_never_raises_a_naturally_small_top_rung() {
        // The small-share case the bpp formula itself was written for
        // (#907's step 5): plenty of budget is left over, but the top rung
        // must never be inflated beyond its own raw ceiling.
        assert_eq!(budgeted_top_bitrate(4_000_000, 400_000), 4_000_000);
    }

    #[test]
    fn budgeted_top_bitrate_floors_at_the_budgeted_minimum() {
        // Lower rungs alone already consume the whole budget: top still gets
        // a usable floor rather than being squeezed toward zero.
        assert_eq!(budgeted_top_bitrate(16_000_000, 8_000_000), 1_500_000);
        assert_eq!(budgeted_top_bitrate(16_000_000, 7_800_000), 1_500_000);
    }

    #[test]
    fn budgeted_top_bitrate_can_land_below_a_lower_rungs_own_ceiling() {
        // #907 review (counselors): deliberately NOT floored at "never below
        // a lower rung's own ceiling" -- see `budgeted_top_bitrate`'s doc
        // comment for why an earlier version's floor there caused a WORSE
        // regression (a 2x total-budget blowout at 4K on the shipped default
        // ladder) than the top/lower ordering inversion it was meant to
        // prevent. This is the exact 4K Legacy-ladder case: a lower rung at
        // 5,000,000 leaves only 2,375,000 of the 8 Mbps budget for the top
        // rung, and that is what it gets, even though it is now nominally
        // "worse" than the lower rung.
        assert_eq!(budgeted_top_bitrate(16_000_000, 5_625_000), 2_375_000);
    }

    #[test]
    fn full_share_ladder_total_ask_is_bounded_for_the_field_incident_geometry() {
        // #907's exact reported geometry: a 1920x1080 full share on the
        // default TwoRung ladder. Before this fix the combined ask was
        // 2,812,500 + 8,000,000 = 10,812,500 (10.8 Mbps) on a link that
        // carried ~2.6 Mbps.
        //
        // #907 review (counselors, two independent models): for THIS exact
        // case, shrinking the top rung's ceiling alone is close to cosmetic
        // -- the lower rung (2,812,500) already exceeds that day's measured
        // 2.58 Mbps link on its own and is funded first regardless of what
        // the top rung's ceiling says, so this budget does not change
        // allocation PRIORITY. It still meaningfully reduces the total
        // WASTED ask (what gets asked for, not what gets funded), and it is
        // the receiver/sender starvation guards (#907 steps 2/3), not this
        // number, that actually recover a link this constrained. Both facts
        // are asserted here so neither gets lost: the total ask really did
        // shrink, and the lower rung's own share of that ask is unchanged
        // (still above the measured link on its own).
        let options = window_publish_options(
            1920,
            1080,
            ShareQuality::Full,
            FullShareSimulcastLadder::TwoRung,
        );
        let layers = options
            .simulcast_layers
            .as_ref()
            .expect("full share must provide explicit simulcast layers");
        let lower_sum: u64 = layers.iter().map(|l| l.encoding.max_bitrate).sum();
        let top = options
            .video_encoding
            .as_ref()
            .expect("full share must keep an explicit top-layer ceiling");
        let total_ask = lower_sum + top.max_bitrate;
        assert_eq!(lower_sum, 2_812_500);
        assert_eq!(top.max_bitrate, 5_187_500);
        assert_eq!(total_ask, FULL_SIMULCAST_TOTAL_BUDGET_BPS);
        assert!(
            total_ask < 10_812_500,
            "the ladder's combined ask must be well under the field-measured 10.8 Mbps overshoot"
        );
        const FIELD_MEASURED_LINK_BPS: u64 = 2_580_000;
        assert!(
            lower_sum > FIELD_MEASURED_LINK_BPS,
            "documents why step 1 alone is close to cosmetic for this exact case: \
             the lower rung already exceeds the measured link and is funded first \
             regardless of the top rung's budgeted ceiling"
        );
    }

    // ---- #907 step 2/7: top-rung starvation guard --------------------------

    #[test]
    fn rung_is_starved_below_the_guard_fraction() {
        // Field-observed: 288kbps of a configured 8,000,000bps ceiling
        // (3.6%) -- far below the 25% guard threshold.
        assert!(rung_is_starved(288_000, 8_000_000));
        // A healthy rung running near its ceiling is not starved.
        assert!(!rung_is_starved(7_500_000, 8_000_000));
        // Exactly at the threshold is not (yet) starved -- strictly below.
        assert!(!rung_is_starved(2_000_000, 8_000_000));
        assert!(rung_is_starved(1_999_999, 8_000_000));
        // An unconfigured (zero-ceiling) rung is never reported starved --
        // nothing to compare against.
        assert!(!rung_is_starved(0, 0));
    }

    #[test]
    fn starvation_guard_throttles_only_after_sustained_starvation() {
        let mut state = RungFundingState::Funded;
        let mut count = 0u32;
        // Two starved samples: not yet enough (trigger is 3).
        for _ in 0..2 {
            let (next_count, _, next_state) = rung_starvation_next_state(state, true, count, 0);
            count = next_count;
            state = next_state;
        }
        assert_eq!(state, RungFundingState::Funded);
        // A healthy sample in between resets the streak entirely.
        let (reset_count, _, reset_state) = rung_starvation_next_state(state, false, count, 0);
        assert_eq!(reset_state, RungFundingState::Funded);
        assert_eq!(reset_count, 0);

        // Three consecutive starved samples DO trip it.
        let mut state = RungFundingState::Funded;
        let mut count = 0u32;
        for _ in 0..RUNG_STARVATION_GUARD_TRIGGER_SAMPLES {
            let (next_count, _, next_state) = rung_starvation_next_state(state, true, count, 0);
            count = next_count;
            state = next_state;
        }
        assert_eq!(state, RungFundingState::Throttled);
    }

    #[test]
    fn starvation_guard_throttled_recognizes_external_recovery_immediately() {
        // #907 review finding 4: if something ELSE (a quality switch) already
        // restored healthy funding, the guard must not wait out the full
        // probe interval before noticing -- the very next sample already
        // proves it.
        let (count, failures, state) =
            rung_starvation_next_state(RungFundingState::Throttled, false, 3, 1);
        assert_eq!(state, RungFundingState::Funded);
        assert_eq!(count, 0);
        assert_eq!(failures, 0);
    }

    #[test]
    fn starvation_guard_reprobes_after_the_probe_interval_then_decides() {
        let mut state = RungFundingState::Throttled;
        let mut count = 0u32;
        for _ in 0..RUNG_STARVATION_GUARD_PROBE_BASE_SAMPLES {
            let (next_count, _, next_state) = rung_starvation_next_state(state, true, count, 0);
            count = next_count;
            state = next_state;
        }
        assert_eq!(state, RungFundingState::Probing, "should enter Probing after the interval");

        // Still starved once probed: back to Throttled, one more failure
        // recorded.
        let (_, failures, still_starved) =
            rung_starvation_next_state(RungFundingState::Probing, true, 0, 0);
        assert_eq!(still_starved, RungFundingState::Throttled);
        assert_eq!(failures, 1);

        // Healthy once probed: recovers to Funded immediately, failures reset.
        let (_, failures, recovered) =
            rung_starvation_next_state(RungFundingState::Probing, false, 0, 2);
        assert_eq!(recovered, RungFundingState::Funded);
        assert_eq!(failures, 0);
    }

    #[test]
    fn starvation_guard_probe_backoff_grows_then_gives_up() {
        // 30s, 60s, 120s (in samples), matching `transport::subscriber`'s
        // schedule by design.
        assert_eq!(
            rung_starvation_probe_interval_samples(0),
            RUNG_STARVATION_GUARD_PROBE_BASE_SAMPLES
        );
        assert_eq!(
            rung_starvation_probe_interval_samples(1),
            RUNG_STARVATION_GUARD_PROBE_BASE_SAMPLES * 2
        );
        assert_eq!(
            rung_starvation_probe_interval_samples(2),
            RUNG_STARVATION_GUARD_PROBE_MAX_SAMPLES
        );
        assert_eq!(
            rung_starvation_probe_interval_samples(100),
            RUNG_STARVATION_GUARD_PROBE_MAX_SAMPLES
        );

        // #907 review (Gemini, "the sender guard has no failure cap"): after
        // `RUNG_STARVATION_GUARD_PROBE_FAILURE_CAP` failed probes in a row,
        // the guard gives up rather than probing forever. Drives the
        // `Probing` arm directly (independent of the `Throttled` interval
        // wait between attempts, which `starvation_guard_reprobes_after_the_probe_interval_then_decides`
        // already covers) to isolate the escalation itself.
        let mut failures = 0u32;
        let mut last_state = RungFundingState::Probing;
        for _ in 0..RUNG_STARVATION_GUARD_PROBE_FAILURE_CAP {
            let (_, next_failures, next_state) =
                rung_starvation_next_state(RungFundingState::Probing, true, 0, failures);
            failures = next_failures;
            last_state = next_state;
        }
        assert_eq!(last_state, RungFundingState::GivenUp);
        assert_eq!(failures, RUNG_STARVATION_GUARD_PROBE_FAILURE_CAP);

        // GivenUp never probes again on its own, but still recognizes an
        // external recovery.
        let (_, _, still_given_up) =
            rung_starvation_next_state(RungFundingState::GivenUp, true, 0, failures);
        assert_eq!(still_given_up, RungFundingState::GivenUp);
        let (_, reset_failures, recovered) =
            rung_starvation_next_state(RungFundingState::GivenUp, false, 0, failures);
        assert_eq!(recovered, RungFundingState::Funded);
        assert_eq!(reset_failures, 0);
    }

    #[test]
    fn starvation_guard_never_protects_a_single_layer_track() {
        // A non-simulcast (single-layer) publish has no lower rung to free
        // capacity for by throttling its only layer.
        assert!(RungStarvationGuard::for_rid("f", 1).is_none());
    }

    #[test]
    fn starvation_guard_guards_the_ladders_top_rid_explicitly() {
        // #907 review finding 6: NOT "whichever configured layer has the
        // largest bitrate" -- at 4K Reduced quality `q`'s halved ceiling
        // (3,000,000) exceeds `h`'s (2,985,984), which a largest-bitrate
        // heuristic would have picked instead of the real top rung.
        let guard =
            RungStarvationGuard::for_rid(FullShareSimulcastLadder::TwoRung.top_rid(), 2)
                .expect("two layers must be guarded");
        assert_eq!(guard.rid, "h");
        assert_eq!(guard.state, RungFundingState::Funded);
    }

    #[test]
    fn starvation_guard_observe_reports_a_transition_only_once() {
        let mut guard = RungStarvationGuard {
            rid: "h".to_string(),
            state: RungFundingState::Funded,
            consecutive_samples: 0,
            consecutive_probe_failures: 0,
        };
        let configured_bitrate_bps = 8_000_000u64;
        assert_eq!(guard.observe(288_000, configured_bitrate_bps), None, "sample 1: not yet sustained");
        assert_eq!(guard.observe(288_000, configured_bitrate_bps), None, "sample 2: not yet sustained");
        assert_eq!(
            guard.observe(288_000, configured_bitrate_bps),
            Some(RungFundingState::Throttled),
            "sample 3: sustained -- transitions and reports it"
        );
        assert_eq!(
            RungStarvationGuard::live_parameters_for(RungFundingState::Throttled, configured_bitrate_bps, 30.0),
            (RUNG_STARVATION_GUARD_THROTTLED_BITRATE_BPS, 30.0)
        );
        // No further transition until the probe interval elapses.
        assert_eq!(guard.observe(50_000, configured_bitrate_bps), None);
    }

    #[test]
    fn current_top_rid_parameters_matches_layer_parameters_for_the_top_rid() {
        // This is the exact computation `PublishedTrack::set_quality` uses;
        // the guard must always agree with it (#907 review finding 4).
        let (rid, max_bitrate, max_framerate) = current_top_rid_parameters(
            ShareQuality::Full,
            1920,
            1080,
            FullShareSimulcastLadder::TwoRung,
        )
        .expect("TwoRung's top rid must be present");
        assert_eq!(rid, "h");
        assert_eq!(max_bitrate, 5_187_500);
        assert_eq!(max_framerate, 30.0);

        // Reduced quality must also be reflected, exercising the same path
        // the guard's `Arc<Mutex<ShareQuality>>` fresh-read would take after
        // a live Full -> Reduced switch.
        let (_, reduced_bitrate, reduced_framerate) = current_top_rid_parameters(
            ShareQuality::Reduced,
            1920,
            1080,
            FullShareSimulcastLadder::TwoRung,
        )
        .expect("TwoRung's top rid must be present for Reduced too");
        assert_eq!(reduced_bitrate, 2_000_000);
        assert_eq!(reduced_framerate, 4.0);
    }


    #[test]
    fn camera_publish_options_pin_single_source_encoding() {
        let options = camera_publish_options(1280, 720, 30.0);

        assert_eq!(options.source, TrackSource::Camera);
        assert_eq!(options.video_codec, VideoCodec::H264);
        assert_eq!(options.video_encoder, select_encoder_backend());
        assert!(options.frame_metadata_features.user_timestamp);
        assert!(options.frame_metadata_features.frame_id);
        assert!(!options.simulcast);
        assert!(options.simulcast_layers.is_none());

        let source_encoding = options
            .video_encoding
            .as_ref()
            .expect("camera publish must keep an explicit source ceiling");
        assert_eq!(source_encoding.max_bitrate, CAMERA_MAX_BITRATE_BPS);
        assert_eq!(source_encoding.max_framerate, CAMERA_MAX_FRAMERATE_FPS);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_camera_publish_options_request_hardware_720p30_source() {
        let options = camera_publish_options(1280, 720, 30.0);
        let source = options.video_encoding.expect("camera source encoding");

        assert_eq!(options.video_encoder, VideoEncoderBackend::Hardware);
        assert_eq!(source.max_bitrate, 2_500_000);
        assert_eq!(source.max_framerate, 30.0);
        assert!(!options.simulcast);
        assert!(options.simulcast_layers.is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_camera_publish_options_keep_videotoolbox_720p30_source() {
        let options = camera_publish_options(1280, 720, 30.0);
        let top = options.video_encoding.expect("camera top encoding");

        assert_eq!(options.video_encoder, VideoEncoderBackend::VideoToolbox);
        assert_eq!(top.max_bitrate, 2_500_000);
        assert_eq!(top.max_framerate, 30.0);
    }

    #[test]
    fn camera_and_share_publishes_keep_their_distinct_encoding_shapes() {
        let camera = camera_publish_options(1280, 720, 30.0);
        assert!(!camera.simulcast);
        assert!(camera.simulcast_layers.is_none());

        for (ladder, expected_len) in [
            (FullShareSimulcastLadder::Legacy, 2),
            (FullShareSimulcastLadder::Raised, 2),
            (FullShareSimulcastLadder::TwoRung, 1),
            (FullShareSimulcastLadder::TwoRungHalf, 1),
        ] {
            let layers = window_publish_options(1280, 720, ShareQuality::Full, ladder)
                .simulcast_layers
                .expect("full share must provide explicit simulcast layers");
            assert_eq!(layers.len(), expected_len, "{ladder:?}");
            assert!(
                layers
                    .iter()
                    .all(|layer| layer.width < 1280 && layer.height < 720),
                "{ladder:?}: every preset must sit below the source"
            );
            assert!(
                layers.windows(2).all(|pair| {
                    pair[0].width < pair[1].width && pair[0].height < pair[1].height
                }),
                "{ladder:?}: lower presets must be ordered smallest-first"
            );
        }
    }

    #[test]
    fn capture_resolution_caps_match_h264_guardrail() {
        assert_eq!(
            CaptureResolution::P1080.explicit_long_edge_cap(),
            Some(1920)
        );
        assert_eq!(
            CaptureResolution::P1440.explicit_long_edge_cap(),
            Some(2560)
        );
        assert_eq!(
            CaptureResolution::Uhd4k.explicit_long_edge_cap(),
            Some(3840)
        );
        assert!(CaptureResolution::Uhd4k.long_edge_cap_for_native(8192) < 4096);
        assert_eq!(
            CaptureResolution::Auto.long_edge_cap_for_native(8192),
            VIDEO_TOOLBOX_H264_MAX_LONG_EDGE
        );
    }

    #[test]
    fn reduced_window_publish_options_keep_the_stable_simulcast_layout() {
        for (ladder, expected_len) in [
            (FullShareSimulcastLadder::Legacy, 2),
            (FullShareSimulcastLadder::Raised, 2),
            (FullShareSimulcastLadder::TwoRung, 1),
            (FullShareSimulcastLadder::TwoRungHalf, 1),
        ] {
            let options = window_publish_options(1920, 1080, ShareQuality::Reduced, ladder);

            // #181: Reduced remains browser-compatible while High is preferred
            // whenever the remote advertises the High-family capability.
            assert_eq!(
                options.h264_profile_preference,
                H264ProfilePreference::HighFirst
            );
            assert!(options.simulcast);
            assert_eq!(
                options.simulcast_layers.as_ref().map(Vec::len),
                Some(expected_len)
            );

            let reduced_encoding = options
                .video_encoding
                .as_ref()
                .expect("reduced share still needs an explicit top-layer ceiling");
            // #907: one formula on both platforms now. At 1920x1080/4fps the
            // raw bpp formula floors at 4 Mbps (below its own minimum),
            // halved by the Reduced branch to 2 Mbps -- well under the
            // budget regardless of ladder, so `budgeted_top_bitrate` never
            // engages here.
            assert_eq!(reduced_encoding.max_bitrate, 2_000_000);
            assert_eq!(reduced_encoding.max_framerate, 4.0);
        }
    }

    #[test]
    fn live_quality_layer_parameters_preserve_rids_and_change_caps() {
        let full =
            ShareQuality::Full.layer_parameters(1920, 1080, FullShareSimulcastLadder::Legacy);
        let reduced =
            ShareQuality::Reduced.layer_parameters(1920, 1080, FullShareSimulcastLadder::Legacy);

        assert_eq!(
            full.iter()
                .map(|layer| layer.rid.as_str())
                .collect::<Vec<_>>(),
            vec!["q", "h", "f"]
        );
        assert_eq!(
            reduced
                .iter()
                .map(|layer| layer.max_framerate)
                .collect::<Vec<_>>(),
            vec![4.0, 4.0, 4.0]
        );
        assert!(reduced[2].max_bitrate < full[2].max_bitrate);
    }

    #[test]
    fn live_encoding_shape_check_rejects_a_collapsed_small_window_layer_set() {
        // The vendored SDK derives its real RID assignment from pixel
        // dimensions and can collapse to fewer live encodings for a small
        // window (e.g. only "q" and "h" survive, no "f") -- a live update
        // targeting all three RIDs must fail closed rather than silently
        // no-op the missing layer's cap.
        let updates =
            ShareQuality::Full.layer_parameters(1920, 1080, FullShareSimulcastLadder::Legacy);
        assert_eq!(
            updates.iter().map(|u| u.rid.as_str()).collect::<Vec<_>>(),
            vec!["q", "h", "f"]
        );

        let full_live_rids: std::collections::HashSet<String> =
            ["q".to_string(), "h".to_string(), "f".to_string()].into();
        assert!(super::live_encoding_shape_covers_updates(
            &full_live_rids,
            &updates
        ));

        let collapsed_live_rids: std::collections::HashSet<String> =
            ["q".to_string(), "h".to_string()].into();
        assert!(!super::live_encoding_shape_covers_updates(
            &collapsed_live_rids,
            &updates
        ));

        let single_layer_live_rids: std::collections::HashSet<String> = ["q".to_string()].into();
        assert!(!super::live_encoding_shape_covers_updates(
            &single_layer_live_rids,
            &updates
        ));
    }

    #[test]
    fn two_rung_quality_updates_only_name_published_rids() {
        for ladder in [
            FullShareSimulcastLadder::TwoRung,
            FullShareSimulcastLadder::TwoRungHalf,
        ] {
            let updates = ShareQuality::Full.layer_parameters(1920, 1080, ladder);
            assert_eq!(
                updates
                    .iter()
                    .map(|update| update.rid.as_str())
                    .collect::<Vec<_>>(),
                vec!["q", "h"],
                "{ladder:?}: with one lower preset, q is LOW and h is the top for both MEDIUM and HIGH"
            );

            let published_rids: std::collections::HashSet<String> =
                ["q".to_string(), "h".to_string()].into();
            assert!(super::live_encoding_shape_covers_updates(
                &published_rids,
                &updates
            ));
            assert!(
                !updates.iter().any(|update| update.rid == "f"),
                "{ladder:?}: a two-rung quality update must never name an unpublished f RID"
            );
        }
    }

    #[test]
    fn share_ladder_values_are_explicit_and_invalid_values_fail() {
        assert_eq!(
            FullShareSimulcastLadder::from_env_value("legacy").unwrap(),
            FullShareSimulcastLadder::Legacy
        );
        assert_eq!(
            FullShareSimulcastLadder::from_env_value("legacy-bottom30").unwrap(),
            FullShareSimulcastLadder::LegacyBottom30
        );
        assert_eq!(
            FullShareSimulcastLadder::from_env_value("raised").unwrap(),
            FullShareSimulcastLadder::Raised
        );
        assert_eq!(
            FullShareSimulcastLadder::from_env_value("two-rung").unwrap(),
            FullShareSimulcastLadder::TwoRung
        );
        assert_eq!(
            FullShareSimulcastLadder::from_env_value("two-rung-half").unwrap(),
            FullShareSimulcastLadder::TwoRungHalf
        );
        let default_error = FullShareSimulcastLadder::from_env_value("default")
            .expect_err("default must not silently select a different ladder");
        assert!(default_error
            .to_string()
            .contains("default is no longer accepted"));
        assert!(default_error
            .to_string()
            .contains("legacy or two-rung explicitly"));
        let error = FullShareSimulcastLadder::from_env_value("three-rung")
            .expect_err("an unsupported ladder must stop publication");
        assert!(error.to_string().contains(PETAL_SHARE_LADDER_ENV));
        assert!(error
            .to_string()
            .contains("legacy, legacy-bottom30, raised, two-rung, or two-rung-half"));
    }

    #[test]
    fn legacy_bottom30_differs_from_legacy_only_in_bottom_rung_framerate() {
        // #613: this variant exists to separate two confounded variables --
        // bottom-rung SIZE from bottom-rung CADENCE. If it ever differs from
        // `legacy` in any other field, it stops answering that question while
        // still looking like a valid comparison.
        let base = full_share_simulcast_layers(1684, 1000, FullShareSimulcastLadder::Legacy);
        let bumped =
            full_share_simulcast_layers(1684, 1000, FullShareSimulcastLadder::LegacyBottom30);
        assert_eq!(base.len(), bumped.len());
        for (i, (a, b)) in base.iter().zip(bumped.iter()).enumerate() {
            assert_eq!(a.width, b.width, "rung {i} width must match");
            assert_eq!(a.height, b.height, "rung {i} height must match");
            assert_eq!(
                a.encoding.max_bitrate, b.encoding.max_bitrate,
                "rung {i} bitrate must match -- only cadence may change"
            );
        }
        assert_eq!(
            base[0].encoding.max_framerate,
            FULL_SIMULCAST_QUARTER_MAX_FRAMERATE_FPS
        );
        assert_eq!(
            bumped[0].encoding.max_framerate,
            FULL_SIMULCAST_HALF_MAX_FRAMERATE_FPS
        );
    }

    #[test]
    fn unset_share_ladder_uses_the_two_rung_default() {
        let previous = std::env::var_os(PETAL_SHARE_LADDER_ENV);
        std::env::remove_var(PETAL_SHARE_LADDER_ENV);

        assert_eq!(
            FullShareSimulcastLadder::from_env().unwrap(),
            FullShareSimulcastLadder::TwoRung
        );

        if let Some(previous) = previous {
            std::env::set_var(PETAL_SHARE_LADDER_ENV, previous);
        }
    }

    #[test]
    fn video_toolbox_size_guard_allows_cap_and_rejects_oversize() {
        assert!(validate_video_toolbox_h264_size(4096, 2304).is_ok());
        assert!(matches!(
            validate_video_toolbox_h264_size(4097, 2304),
            Err(RoomConnectionError::InvalidVideoConfig(_))
        ));
    }

    /// Lockstep with the web client's `trackNameForCamera()`
    /// (web-harness/src/trackNames.ts) — same cases its own tests pin.
    #[test]
    fn camera_track_name_matches_web_client_slugging() {
        assert_eq!(camera_track_name("Jordan Kim!"), "petal-camera-jordan-kim");
        assert_eq!(
            camera_track_name("637511f2-851a-47f8-b043-823656bfc54b"),
            "petal-camera-637511f2-851a-47f8-b043-823656bfc54b"
        );
        assert_eq!(camera_track_name("___"), "petal-camera-anon");
        assert_eq!(camera_track_name(""), "petal-camera-anon");
        assert_eq!(
            camera_track_name("--Web User 9--"),
            "petal-camera-web-user-9"
        );
    }

    #[test]
    fn track_names_match_shared_contract_fixture() {
        let fixture = contract_fixture();
        for vector in fixture.window_tracks {
            assert_eq!(track_name_for_window(vector.window_id), vector.track_name);
            assert_eq!(
                window_id_from_track_name(&vector.track_name),
                Some(vector.window_id)
            );
        }
        for vector in fixture.camera_tracks {
            assert_eq!(camera_track_name(&vector.identity), vector.track_name);
            // Camera slugs must NEVER parse as window ids — the subscriber's
            // compositor feed keys off `window_id_from_track_name`'s EXACT
            // `petal-window-<id>` shape precisely so remote cameras stay on
            // the gallery bridge instead of becoming compositor windows.
            assert_eq!(
                window_id_from_track_name(&vector.track_name),
                None,
                "camera track {} must not parse as a window id",
                vector.track_name
            );
        }
        let source_kind = fixture.source_kind_metadata;
        assert_eq!(
            shared_window_kind_from_metadata(&source_kind.metadata, source_kind.display_window_id),
            SharedSourceKind::Display
        );
        assert_eq!(
            shared_window_kind_from_metadata(&source_kind.metadata, source_kind.window_window_id),
            SharedSourceKind::Window
        );
        assert_eq!(
            shared_window_kind_from_metadata(&source_kind.metadata, source_kind.missing_window_id),
            SharedSourceKind::Window
        );
    }

    #[test]
    fn display_region_source_kind_round_trips_without_becoming_a_display_or_window() {
        assert_eq!(SharedSourceKind::DisplayRegion.as_wire(), "display_region");
        assert_eq!(
            SharedSourceKind::from_wire("display_region"),
            Some(SharedSourceKind::DisplayRegion)
        );
        assert_ne!(SharedSourceKind::DisplayRegion, SharedSourceKind::Display);
        assert_ne!(SharedSourceKind::DisplayRegion, SharedSourceKind::Window);
    }

    #[test]
    fn camera_encoding_ceiling_scales_with_resolution_and_fps() {
        assert_eq!(
            camera_video_encoding(1280, 720, 30.0).max_bitrate,
            2_500_000
        );
        assert_eq!(
            camera_video_encoding(1920, 1080, 30.0).max_bitrate,
            5_625_000
        );
        assert_eq!(
            camera_video_encoding(1280, 720, 60.0).max_bitrate,
            5_000_000
        );
        assert_eq!(
            camera_video_encoding(1920, 1080, 60.0).max_bitrate,
            11_250_000
        );
        // 4K clamps at the 16 Mbps ceiling; a low 480p15 request floors at 500 kbps.
        assert_eq!(
            camera_video_encoding(3840, 2160, 30.0).max_bitrate,
            16_000_000
        );
        assert_eq!(camera_video_encoding(640, 480, 15.0).max_bitrate, 500_000);
        assert_eq!(camera_video_encoding(1920, 1080, 30.0).max_framerate, 30.0);
        assert_eq!(camera_video_encoding(1280, 720, 60.0).max_framerate, 60.0);
    }

    #[test]
    fn camera_encoding_is_720p_friendly_but_below_focused_share_budget() {
        let camera = camera_video_encoding(1280, 720, 30.0);
        let focused_share = ShareQuality::Full.video_encoding(1280, 720);

        assert_eq!(camera.max_bitrate, CAMERA_MAX_BITRATE_BPS);
        assert_eq!(camera.max_framerate, 30.0);
        assert!(
            camera.max_bitrate < focused_share.max_bitrate,
            "camera should stay below the focused screenshare budget"
        );
    }

    /// NV12 U/V interleave order pin (the NV analogue of the #24 trap):
    /// a solid color with DISTINCT U≠V must land on the right planes through
    /// `rs_NV12ToI420`; `rs_NV21ToI420` misuse would swap them and fail this.
    #[test]
    fn nv12_uv_order_lands_on_correct_i420_planes() {
        // 2x2 solid "red-ish": Y=82, U=90, V=240 (BT.601 red).
        let y = [82u8; 4];
        let uv = [90u8, 240u8]; // NV12: U first, then V
        let mut out_y = [0u8; 4];
        let mut out_u = [0u8; 1];
        let mut out_v = [0u8; 1];
        assert!(convert_nv12_to_i420(
            &y, 2, &uv, 2, &mut out_y, 2, &mut out_u, 1, &mut out_v, 1, 2, 2,
        ));
        assert_eq!(out_y[0], 82, "Y plane must copy through");
        assert_eq!(
            out_u[0], 90,
            "U must come from the FIRST interleaved byte (NV12)"
        );
        assert_eq!(
            out_v[0], 240,
            "V must come from the SECOND interleaved byte (NV12)"
        );
    }

    /// Same naming trap, pinned at the helper used by window-share NV12
    /// payloads (#178): this helper must keep calling `rs_NV12ToI420`.
    #[test]
    fn window_nv12_payload_helper_uses_nv12_not_nv21_order() {
        let y = [82u8; 4];
        let uv = [90u8, 240u8];
        let mut out_y = [0u8; 4];
        let mut out_u = [0u8; 1];
        let mut out_v = [0u8; 1];

        assert!(convert_nv12_to_i420(
            &y, 2, &uv, 2, &mut out_y, 2, &mut out_u, 1, &mut out_v, 1, 2, 2,
        ));

        assert_eq!(out_y, y);
        assert_eq!(out_u[0], 90, "window NV12 U byte must not be read as V");
        assert_eq!(out_v[0], 240, "window NV12 V byte must not be read as U");
    }

    #[test]
    fn nv12_layout_rejects_invalid_dimensions_strides_and_plane_extents() {
        let y = [1u8; 16];
        let uv = [2u8; 8];
        let mut out_y = [0u8; 16];
        let mut out_u = [0u8; 4];
        let mut out_v = [0u8; 4];

        let mut convert = |y: &[u8], ys, uv: &[u8], uvs, width, height| {
            convert_nv12_to_i420(
                y, ys, uv, uvs, &mut out_y, 4, &mut out_u, 2, &mut out_v, 2, width, height,
            )
        };

        assert!(!convert(&y, 4, &uv, 4, 0, 4));
        assert!(!convert(&y, 4, &uv, 4, 4, 0));
        assert!(!convert(&y, 3, &uv, 4, 4, 4));
        assert!(!convert(&y, 4, &uv, 3, 4, 4));
        assert!(!convert(&y[..15], 4, &uv, 4, 4, 4));
        assert!(!convert(&y, 4, &uv[..7], 4, 4, 4));
        // A larger plane with the old geometry must not be treated as a
        // valid smaller frame: using the old stride would offset every row
        // after the first in the interleaved UV plane (#495).
        assert!(!convert(&[1u8; 32], 4, &[2u8; 16], 4, 4, 4));
        assert!(!convert(&[], u32::MAX, &[], u32::MAX, u32::MAX - 1, 2));
    }

    #[test]
    fn nv12_layout_accepts_padded_source_rows_when_extent_matches() {
        let y = [82u8; 32];
        let uv = [
            90u8, 240, 90, 240, 90, 240, 90, 240, 90, 240, 90, 240, 90, 240, 90, 240,
        ];
        let mut out_y = [0u8; 16];
        let mut out_u = [0u8; 4];
        let mut out_v = [0u8; 4];

        assert!(convert_nv12_to_i420(
            &y, 8, &uv, 8, &mut out_y, 4, &mut out_u, 2, &mut out_v, 2, 4, 4,
        ));
        assert_eq!(out_y, [82; 16]);
        assert_eq!(out_u, [90; 4]);
        assert_eq!(out_v, [240; 4]);
    }

    #[test]
    fn nv12_layout_accepts_odd_dimensions_with_ceil_chroma_extents() {
        let y = [82u8; 9];
        let uv = [90u8, 240, 90, 240, 90, 240, 90, 240];
        let mut out_y = [0u8; 9];
        let mut out_u = [0u8; 4];
        let mut out_v = [0u8; 4];
        assert!(convert_nv12_to_i420(
            &y, 3, &uv, 4, &mut out_y, 3, &mut out_u, 2, &mut out_v, 2, 3, 3,
        ));
        assert_eq!(out_y, y);
        assert_eq!(out_u, [90; 4]);
        assert_eq!(out_v, [240; 4]);
    }

    #[test]
    fn nv12_layout_rejects_short_destination_planes() {
        let y = [1u8; 16];
        let uv = [2u8; 8];
        let mut short_y = [0u8; 15];
        let mut out_u = [0u8; 4];
        let mut out_v = [0u8; 4];
        assert!(!convert_nv12_to_i420(
            &y,
            4,
            &uv,
            4,
            &mut short_y,
            4,
            &mut out_u,
            2,
            &mut out_v,
            2,
            4,
            4,
        ));
    }

    #[test]
    fn nv12_normalization_initializes_vector_tail_after_tight_final_row() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut padded = Vec::new();
        let stride = fill_padded_nv12_plane(&mut padded, &src, 4, 4, 2).expect("valid tight plane");
        assert!(stride >= 4 + 64);
        assert_eq!(&padded[..4], &[1, 2, 3, 4]);
        assert_eq!(&padded[stride..stride + 4], &[5, 6, 7, 8]);
        assert!(padded[4..stride].iter().all(|byte| *byte == 0));
        assert!(padded[stride + 4..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn nv12_scratch_reuses_allocations_and_reinitializes_padding() {
        let mut scratch = Nv12Scratch::default();
        let src = [7u8; 16];
        let stride = fill_padded_nv12_plane(&mut scratch.y, &src, 4, 4, 4).unwrap();
        let ptr = scratch.y.as_ptr();
        let capacity = scratch.y.capacity();
        scratch.y[4] = 99;
        assert_eq!(
            fill_padded_nv12_plane(&mut scratch.y, &src, 4, 4, 4),
            Some(stride)
        );
        assert_eq!(scratch.y.as_ptr(), ptr);
        assert_eq!(scratch.y.capacity(), capacity);
        assert_eq!(scratch.y[4], 0, "reused SIMD padding must be reinitialized");
    }

    #[test]
    fn nv12_conversion_is_stable_under_concurrent_tight_plane_reuse() {
        let workers = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..500 {
                        let y = [82u8; 16];
                        let uv = [90u8, 240, 90, 240, 90, 240, 90, 240];
                        let mut out_y = [0u8; 16];
                        let mut out_u = [0u8; 4];
                        let mut out_v = [0u8; 4];
                        assert!(convert_nv12_to_i420(
                            &y, 4, &uv, 4, &mut out_y, 4, &mut out_u, 2, &mut out_v, 2, 4, 4,
                        ));
                        assert_eq!(out_y, y);
                        assert_eq!(out_u, [90; 4]);
                        assert_eq!(out_v, [240; 4]);
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().expect("conversion worker must not panic");
        }
    }

    /// Architecture-independent scalar model of NV12 -> I420: copy the visible
    /// Y rows, then deinterleave UV into separate U/V planes. `libyuv`'s
    /// `NV12ToI420` selects a different vector kernel per architecture (NEON on
    /// aarch64, SSE2/AVX2 on x86_64), so the only way to know both kernels
    /// agree on this codebase's real strides is to check both against one
    /// scalar definition. See #549.
    fn scalar_nv12_to_i420(
        y: &[u8],
        y_stride: usize,
        uv: &[u8],
        uv_stride: usize,
        width: usize,
        height: usize,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let mut out_y = vec![0u8; width * height];
        let mut out_u = vec![0u8; chroma_width * chroma_height];
        let mut out_v = vec![0u8; chroma_width * chroma_height];
        for row in 0..height {
            let src = row * y_stride;
            out_y[row * width..(row + 1) * width].copy_from_slice(&y[src..src + width]);
        }
        for row in 0..chroma_height {
            for col in 0..chroma_width {
                let src = (row * uv_stride) + (col * 2);
                out_u[(row * chroma_width) + col] = uv[src];
                out_v[(row * chroma_width) + col] = uv[src + 1];
            }
        }
        (out_y, out_u, out_v)
    }

    /// Single-pixel vertical stripes plus a diagonal: the hardest edge content
    /// for a vectorized deinterleave to get right, and the content whose
    /// corruption reads as "jaggy" video.
    fn hard_edge_nv12_source(
        width: usize,
        height: usize,
        y_stride: usize,
        uv_stride: usize,
    ) -> (Vec<u8>, Vec<u8>) {
        let chroma_height = height.div_ceil(2);
        let mut y = vec![0u8; y_stride * height];
        let mut uv = vec![0u8; uv_stride * chroma_height];
        for row in 0..height {
            for col in 0..width {
                let stripe = if col % 2 == 0 { 16u8 } else { 235u8 };
                let diagonal = if (row + col) % 32 == 0 { 128 } else { stripe };
                y[(row * y_stride) + col] = diagonal;
            }
        }
        for row in 0..chroma_height {
            for col in 0..width.div_ceil(2) {
                let base = (row * uv_stride) + (col * 2);
                uv[base] = ((col * 3) % 251) as u8;
                uv[base + 1] = ((row * 7) + 1) as u8;
            }
        }
        (y, uv)
    }

    fn i420_planes_from_conversion(
        y: &[u8],
        y_stride: usize,
        uv: &[u8],
        uv_stride: usize,
        width: usize,
        height: usize,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let mut out_y = vec![0u8; width * height];
        let mut out_u = vec![0u8; chroma_width * chroma_height];
        let mut out_v = vec![0u8; chroma_width * chroma_height];
        assert!(convert_nv12_to_i420(
            y,
            y_stride as u32,
            uv,
            uv_stride as u32,
            &mut out_y,
            width as u32,
            &mut out_u,
            chroma_width as u32,
            &mut out_v,
            chroma_width as u32,
            width as u32,
            height as u32,
        ));
        (out_y, out_u, out_v)
    }

    /// #549: the published camera path is NV12 -> I420 -> encoder. If the
    /// x86_64 `libyuv` kernel disagreed with the aarch64 one on any pixel, an
    /// Intel-origin webcam would carry exactly the comb/edge artifacts the
    /// issue describes. Run under both `--target aarch64-apple-darwin` and
    /// `--target x86_64-apple-darwin` to compare the two kernels.
    #[test]
    fn nv12_to_i420_matches_scalar_reference_for_hard_edges() {
        // Camera-realistic geometry, odd geometry, and a padded source stride
        // (CoreVideo pads rows; AVFoundation camera buffers routinely do).
        let cases: [(usize, usize, usize, usize); 4] = [
            (1280, 720, 1280, 1280),
            (1280, 720, 1344, 1344),
            (1281, 721, 1408, 1408),
            (17, 9, 32, 32),
        ];
        for (width, height, y_stride, uv_stride) in cases {
            let (y, uv) = hard_edge_nv12_source(width, height, y_stride, uv_stride);
            let actual = i420_planes_from_conversion(&y, y_stride, &uv, uv_stride, width, height);
            let expected = scalar_nv12_to_i420(&y, y_stride, &uv, uv_stride, width, height);
            assert_eq!(
                actual.0, expected.0,
                "Y plane mismatch at {width}x{height} (y_stride {y_stride})"
            );
            assert_eq!(
                actual.1, expected.1,
                "U plane mismatch at {width}x{height} (uv_stride {uv_stride})"
            );
            assert_eq!(
                actual.2, expected.2,
                "V plane mismatch at {width}x{height} (uv_stride {uv_stride})"
            );
        }
    }

    /// Positive control for the test above: prove the comparison actually
    /// detects a single corrupted chroma byte, so a passing run is evidence of
    /// correctness rather than evidence of a blind assertion.
    #[test]
    fn nv12_reference_comparison_detects_single_byte_corruption() {
        let (width, height, y_stride, uv_stride) = (1280usize, 720usize, 1280usize, 1280usize);
        let (y, uv) = hard_edge_nv12_source(width, height, y_stride, uv_stride);
        let baseline = scalar_nv12_to_i420(&y, y_stride, &uv, uv_stride, width, height);

        let mut corrupted_uv = uv.clone();
        let victim = (height / 4) * uv_stride + (width / 2);
        corrupted_uv[victim] = corrupted_uv[victim].wrapping_add(37);
        let corrupted =
            i420_planes_from_conversion(&y, y_stride, &corrupted_uv, uv_stride, width, height);
        assert_eq!(corrupted.0, baseline.0, "Y plane must be untouched");
        assert_ne!(
            corrupted.1, baseline.1,
            "a corrupted UV byte must change the U plane -- otherwise the \
             cross-architecture comparison above proves nothing"
        );
    }

    #[test]
    fn bgra_layout_accepts_tight_and_padded_source_rows() {
        let tight_src = [0u8; 32];
        let tight_y = [0u8; 8];
        let tight_u = [0u8; 2];
        let tight_v = [0u8; 2];
        assert!(
            validated_bgra_layout(&tight_src, 16, &tight_y, 4, &tight_u, 2, &tight_v, 2, 4, 2,)
                .is_some()
        );

        let padded_src = [0u8; 80];
        let padded_y = [0u8; 24];
        let padded_u = [0u8; 6];
        let padded_v = [0u8; 6];
        assert!(validated_bgra_layout(
            &padded_src,
            20,
            &padded_y,
            6,
            &padded_u,
            3,
            &padded_v,
            3,
            4,
            4,
        )
        .is_some());
    }

    #[test]
    fn bgra_layout_accepts_odd_dimensions_with_ceil_chroma_extents() {
        let src = [0u8; 36];
        let y = [0u8; 9];
        let u = [0u8; 4];
        let v = [0u8; 4];
        assert!(validated_bgra_layout(&src, 12, &y, 3, &u, 2, &v, 2, 3, 3,).is_some());
    }

    #[test]
    fn bgra_layout_rejects_byte_short_and_byte_long_source_extents() {
        let exact_src = [0u8; 16];
        let long_src = [0u8; 17];
        let y = [0u8; 4];
        let u = [0u8; 1];
        let v = [0u8; 1];

        assert!(validated_bgra_layout(&exact_src[..15], 8, &y, 2, &u, 1, &v, 1, 2, 2,).is_none());
        assert!(validated_bgra_layout(&long_src, 8, &y, 2, &u, 1, &v, 1, 2, 2).is_none());
    }

    #[test]
    fn bgra_layout_rejects_zero_and_out_of_range_dimensions_and_strides() {
        let src = [0u8; 4];
        let y = [0u8; 1];
        let u = [0u8; 1];
        let v = [0u8; 1];
        let validate = |src_stride, dst_stride_y, dst_stride_u, dst_stride_v, width, height| {
            validated_bgra_layout(
                &src,
                src_stride,
                &y,
                dst_stride_y,
                &u,
                dst_stride_u,
                &v,
                dst_stride_v,
                width,
                height,
            )
        };

        assert!(validate(4, 1, 1, 1, 0, 1).is_none());
        assert!(validate(4, 1, 1, 1, 1, 0).is_none());
        assert!(validate(4, 1, 1, 1, i32::MAX as u32 + 1, 1).is_none());
        assert!(validate(4, 1, 1, 1, 1, i32::MAX as u32 + 1).is_none());
        assert!(validate(i32::MAX as usize + 1, 1, 1, 1, 1, 1).is_none());
        assert!(validate(usize::MAX, 1, 1, 1, 1, 1).is_none());
        assert!(validate(4, u32::MAX, 1, 1, 1, 1).is_none());
        assert!(validate(4, 1, u32::MAX, 1, 1, 1).is_none());
        assert!(validate(4, 1, 1, u32::MAX, 1, 1).is_none());
        assert!(validate((i32::MAX / 2) as usize + 1, 1, 1, 1, 1, 2).is_none());
        assert!(validate(4, (i32::MAX / 2) as u32 + 1, 1, 1, 1, 2).is_none());
    }

    #[test]
    fn bgra_layout_rejects_source_and_destination_stride_mismatches() {
        let src = [0u8; 64];
        let short_stride_src = [0u8; 60];
        let y = [0u8; 16];
        let u = [0u8; 4];
        let v = [0u8; 4];

        assert!(validated_bgra_layout(&short_stride_src, 15, &y, 4, &u, 2, &v, 2, 4, 4,).is_none());
        assert!(validated_bgra_layout(&src, 16, &y, 3, &u, 2, &v, 2, 4, 4).is_none());
        assert!(validated_bgra_layout(&src, 16, &y, 4, &u, 1, &v, 2, 4, 4).is_none());
        assert!(validated_bgra_layout(&src, 16, &y, 4, &u, 2, &v, 1, 4, 4).is_none());
    }

    #[test]
    fn bgra_layout_rejects_each_short_destination_plane() {
        let src = [0u8; 64];
        let y = [0u8; 16];
        let u = [0u8; 4];
        let v = [0u8; 4];

        assert!(validated_bgra_layout(&src, 16, &y[..15], 4, &u, 2, &v, 2, 4, 4).is_none());
        assert!(validated_bgra_layout(&src, 16, &y, 4, &u[..3], 2, &v, 2, 4, 4).is_none());
        assert!(validated_bgra_layout(&src, 16, &y, 4, &u, 2, &v[..3], 2, 4, 4).is_none());

        let padded_src = [0u8; 80];
        let padded_y = [0u8; 24];
        let padded_u = [0u8; 6];
        let padded_v = [0u8; 6];
        assert!(validated_bgra_layout(
            &padded_src,
            20,
            &padded_y[..23],
            6,
            &padded_u,
            3,
            &padded_v,
            3,
            4,
            4,
        )
        .is_none());
        assert!(validated_bgra_layout(
            &padded_src,
            20,
            &padded_y,
            6,
            &padded_u[..5],
            3,
            &padded_v,
            3,
            4,
            4,
        )
        .is_none());
        assert!(validated_bgra_layout(
            &padded_src,
            20,
            &padded_y,
            6,
            &padded_u,
            3,
            &padded_v[..5],
            3,
            4,
            4,
        )
        .is_none());
    }

    #[test]
    fn malformed_bgra_conversion_is_rejected_before_both_bt601_ffi_paths() {
        let src = [0u8; 15];
        for profile in [
            VideoColorProfile::BT601_VIDEO,
            VideoColorProfile {
                range: video_color::PixelRange::Full,
                ..VideoColorProfile::BT601_VIDEO
            },
        ] {
            let mut y = [0x11u8; 4];
            let mut u = [0x22u8; 1];
            let mut v = [0x33u8; 1];
            assert!(!convert_apple_bgra_to_i420(
                &src, 8, &mut y, 2, &mut u, 1, &mut v, 1, 2, 2, profile,
            ));
            assert_eq!(y, [0x11; 4]);
            assert_eq!(u, [0x22; 1]);
            assert_eq!(v, [0x33; 1]);
        }
    }

    /// Pins the libyuv naming trap (issue #24): Apple-BGRA bytes
    /// (B,G,R,A) through `rs_ARGBToI420` must land on the correct YUV
    /// values. With the old `rs_BGRAToI420` call, the opaque alpha byte was
    /// read as BLUE, tinting every real share blue/purple — this test fails
    /// loudly if anyone "fixes" the call back by its misleading name.
    #[cfg(target_os = "macos")]
    #[test]
    fn apple_bgra_bytes_convert_to_correct_yuv_channels() {
        // 2x2 image, one solid color per conversion pass keeps the U/V
        // subsampling (one U+V per 2x2 block) exact.
        // (name, [B,G,R,A], expected (Y,U,V) per BT.601 studio swing ±2)
        let cases: [(&str, [u8; 4], (i32, i32, i32)); 4] = [
            ("red", [0, 0, 255, 255], (82, 90, 240)),
            ("green", [0, 255, 0, 255], (145, 54, 34)),
            ("blue", [255, 0, 0, 255], (41, 240, 110)),
            ("dark gray", [48, 48, 48, 255], (57, 128, 128)),
        ];
        for (name, px, (ey, eu, ev)) in cases {
            let src: Vec<u8> = px.repeat(4); // 2x2 pixels, stride 8
            let mut y = [0u8; 4];
            let mut u = [0u8; 1];
            let mut v = [0u8; 1];
            unsafe {
                yuv_sys::rs_ARGBToI420(
                    src.as_ptr(),
                    8,
                    y.as_mut_ptr(),
                    2,
                    u.as_mut_ptr(),
                    1,
                    v.as_mut_ptr(),
                    1,
                    2,
                    2,
                );
            }
            let (ay, au, av) = (y[0] as i32, u[0] as i32, v[0] as i32);
            for (chan, actual, expected) in [("Y", ay, ey), ("U", au, eu), ("V", av, ev)] {
                assert!(
                    (actual - expected).abs() <= 2,
                    "{name}: {chan} = {actual}, expected ~{expected} — channel order is wrong \
                     (libyuv-'ARGB' == Apple-BGRA; see module doc / issue #24)"
                );
            }
        }
    }

    #[test]
    fn apple_bgra_vectors_distinguish_bt601_and_bt709_matrices() {
        let red = video_color::apple_bgra_to_rgb8([0, 0, 255, 255]);
        assert_eq!(
            video_color::rgb_to_ycbcr_8bit(red, VideoColorProfile::BT601_VIDEO),
            video_color::YCbCr8 {
                y: 81,
                cb: 90,
                cr: 240
            }
        );
        assert_eq!(
            video_color::rgb_to_ycbcr_8bit(
                red,
                VideoColorProfile {
                    primaries: video_color::ColorPrimaries::Bt709,
                    transfer: video_color::TransferFunction::Bt709,
                    matrix: video_color::MatrixCoefficients::Bt709,
                    range: video_color::PixelRange::Video,
                }
            ),
            video_color::YCbCr8 {
                y: 63,
                cb: 102,
                cr: 240
            }
        );
    }

    #[test]
    fn conversion_selector_only_uses_supported_libyuv_paths() {
        assert_eq!(
            apple_bgra_to_i420_conversion(VideoColorProfile::BT601_VIDEO),
            AppleBgraToI420Conversion::Bt601VideoRange
        );
        assert_eq!(
            apple_bgra_to_i420_conversion(VideoColorProfile {
                range: video_color::PixelRange::Full,
                ..VideoColorProfile::BT601_VIDEO
            }),
            AppleBgraToI420Conversion::Bt601FullRange
        );
        assert_eq!(
            apple_bgra_to_i420_conversion(VideoColorProfile::SRGB_BT709_FULL),
            AppleBgraToI420Conversion::Bt709FullRange
        );
        assert_eq!(
            apple_bgra_to_i420_conversion(VideoColorProfile::DISPLAY_P3_BT709_FULL),
            AppleBgraToI420Conversion::Bt709FullRange
        );
        assert_eq!(
            apple_bgra_to_i420_conversion(VideoColorProfile {
                range: video_color::PixelRange::Video,
                ..VideoColorProfile::SRGB_BT709_FULL
            }),
            AppleBgraToI420Conversion::Bt709VideoRange
        );
    }

    #[test]
    fn bt709_full_range_bgra_to_i420_averages_chroma_blocks() {
        let width = 2;
        let height = 2;
        let src = [
            0, 0, 255, 255, // red
            0, 255, 0, 255, // green
            255, 0, 0, 255, // blue
            255, 255, 255, 255, // white
        ];
        let mut y = [0u8; 4];
        let mut u = [0u8; 1];
        let mut v = [0u8; 1];

        assert!(convert_apple_bgra_to_i420(
            &src,
            width * 4,
            &mut y,
            width as u32,
            &mut u,
            1,
            &mut v,
            1,
            width as u32,
            height as u32,
            VideoColorProfile::SRGB_BT709_FULL,
        ));

        let expected = [
            video_color::rgb_to_ycbcr_8bit(
                video_color::Rgb8 { r: 255, g: 0, b: 0 },
                VideoColorProfile::SRGB_BT709_FULL,
            ),
            video_color::rgb_to_ycbcr_8bit(
                video_color::Rgb8 { r: 0, g: 255, b: 0 },
                VideoColorProfile::SRGB_BT709_FULL,
            ),
            video_color::rgb_to_ycbcr_8bit(
                video_color::Rgb8 { r: 0, g: 0, b: 255 },
                VideoColorProfile::SRGB_BT709_FULL,
            ),
            video_color::rgb_to_ycbcr_8bit(
                video_color::Rgb8 {
                    r: 255,
                    g: 255,
                    b: 255,
                },
                VideoColorProfile::SRGB_BT709_FULL,
            ),
        ];
        assert_eq!(y, expected.map(|p| p.y));
        assert_eq!(
            u[0],
            ((expected.iter().map(|p| u16::from(p.cb)).sum::<u16>() + 2) / 4) as u8
        );
        assert_eq!(
            v[0],
            ((expected.iter().map(|p| u16::from(p.cr)).sum::<u16>() + 2) / 4) as u8
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bt709_video_range_round_trips_through_h420_decode() {
        let width = 6usize;
        let height = 4usize;
        let colors: [(&str, video_color::Rgb8); 6] = [
            ("red", video_color::Rgb8 { r: 255, g: 0, b: 0 }),
            ("green", video_color::Rgb8 { r: 0, g: 255, b: 0 }),
            ("blue", video_color::Rgb8 { r: 0, g: 0, b: 255 }),
            (
                "white",
                video_color::Rgb8 {
                    r: 255,
                    g: 255,
                    b: 255,
                },
            ),
            ("black", video_color::Rgb8 { r: 0, g: 0, b: 0 }),
            (
                "gray",
                video_color::Rgb8 {
                    r: 128,
                    g: 128,
                    b: 128,
                },
            ),
        ];
        let mut src = vec![0u8; width * height * 4];
        for block_y in 0..2 {
            for block_x in 0..3 {
                let (_, rgb) = colors[(block_y * 3) + block_x];
                for dy in 0..2 {
                    for dx in 0..2 {
                        let x = (block_x * 2) + dx;
                        let y = (block_y * 2) + dy;
                        let offset = ((y * width) + x) * 4;
                        src[offset] = rgb.b;
                        src[offset + 1] = rgb.g;
                        src[offset + 2] = rgb.r;
                        src[offset + 3] = 255;
                    }
                }
            }
        }

        let profile = VideoColorProfile {
            range: video_color::PixelRange::Video,
            ..VideoColorProfile::SRGB_BT709_FULL
        };
        let mut y = vec![0u8; width * height];
        let mut u = vec![0u8; (width / 2) * (height / 2)];
        let mut v = vec![0u8; (width / 2) * (height / 2)];
        assert!(convert_apple_bgra_to_i420(
            &src,
            width * 4,
            &mut y,
            width as u32,
            &mut u,
            (width / 2) as u32,
            &mut v,
            (width / 2) as u32,
            width as u32,
            height as u32,
            profile,
        ));

        let mut decoded = vec![0u8; width * height * 4];
        let rc = unsafe {
            yuv_sys::rs_H420ToARGB(
                y.as_ptr(),
                width as i32,
                u.as_ptr(),
                (width / 2) as i32,
                v.as_ptr(),
                (width / 2) as i32,
                decoded.as_mut_ptr(),
                (width * 4) as i32,
                width as i32,
                height as i32,
            )
        };
        assert_eq!(rc, 0);

        for block_y in 0..2 {
            for block_x in 0..3 {
                let (name, expected) = colors[(block_y * 3) + block_x];
                let x = block_x * 2;
                let y = block_y * 2;
                let offset = ((y * width) + x) * 4;
                let actual = video_color::apple_bgra_to_rgb8([
                    decoded[offset],
                    decoded[offset + 1],
                    decoded[offset + 2],
                    decoded[offset + 3],
                ]);
                for (channel, actual, expected) in [
                    ("r", actual.r, expected.r),
                    ("g", actual.g, expected.g),
                    ("b", actual.b, expected.b),
                ] {
                    // libyuv's fixed-point H420 inverse is not bit-exact
                    // against the floating-point BT.709 vectors pinned in
                    // video_color.rs; saturated blue/green components are
                    // the widest observed miss in yuv-sys 0.3.14.
                    assert!(
                        (i16::from(actual) - i16::from(expected)).abs() <= 12,
                        "{name} {channel}: decoded {actual}, expected {expected}"
                    );
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tagged_nv12_cv_pixel_buffer_round_trips_with_bt709_matrix() {
        let width = 6usize;
        let height = 4usize;
        let chroma_width = width / 2;
        let chroma_height = height / 2;
        let profile = VideoColorProfile {
            range: video_color::PixelRange::Video,
            ..VideoColorProfile::SRGB_BT709_FULL
        };
        let wrong_profile = VideoColorProfile::BT601_VIDEO;
        let colors: [(&str, video_color::Rgb8); 6] = [
            ("red", video_color::Rgb8 { r: 255, g: 0, b: 0 }),
            ("green", video_color::Rgb8 { r: 0, g: 255, b: 0 }),
            ("blue", video_color::Rgb8 { r: 0, g: 0, b: 255 }),
            (
                "white",
                video_color::Rgb8 {
                    r: 255,
                    g: 255,
                    b: 255,
                },
            ),
            ("black", video_color::Rgb8 { r: 0, g: 0, b: 0 }),
            (
                "gray",
                video_color::Rgb8 {
                    r: 128,
                    g: 128,
                    b: 128,
                },
            ),
        ];

        let mut expected_y = vec![0u8; width * height];
        let mut expected_u = vec![0u8; chroma_width * chroma_height];
        let mut expected_v = vec![0u8; chroma_width * chroma_height];
        for block_y in 0..chroma_height {
            for block_x in 0..chroma_width {
                let (_, rgb) = colors[(block_y * chroma_width) + block_x];
                let ycbcr = video_color::rgb_to_ycbcr_8bit(rgb, profile);
                expected_u[(block_y * chroma_width) + block_x] = ycbcr.cb;
                expected_v[(block_y * chroma_width) + block_x] = ycbcr.cr;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let x = (block_x * 2) + dx;
                        let y = (block_y * 2) + dy;
                        expected_y[(y * width) + x] = ycbcr.y;
                    }
                }
            }
        }

        let pixel_buffer = screencapturekit::cv::CVPixelBuffer::create(
            width,
            height,
            0x3432_3076, // '420v': NV12 video range, matching ScreenCaptureKit capture.
        )
        .expect("test must create a real NV12 CVPixelBuffer");
        crate::native_display::attach_video_color_profile_to_cv_pixel_buffer(
            pixel_buffer.as_ptr(),
            profile,
        );
        fill_nv12_cv_pixel_buffer(&pixel_buffer, &expected_y, &expected_u, &expected_v, width);

        let detected_profile =
            crate::capture::color_profile_for_nv12_pixel_buffer(&pixel_buffer, wrong_profile);
        assert_eq!(
            detected_profile, profile,
            "CoreVideo attachments must override the BT.601 fallback"
        );

        let payload = crate::capture::copy_nv12_payload(&pixel_buffer, None)
            .expect("real tagged CVPixelBuffer must copy through capture payload path");
        let crate::capture::CapturedFramePayload::Nv12 {
            y,
            y_stride,
            uv,
            uv_stride,
        } = payload
        else {
            panic!("NV12 pixel buffer must produce an NV12 payload");
        };

        let mut out_y = vec![0u8; width * height];
        let mut out_u = vec![0u8; chroma_width * chroma_height];
        let mut out_v = vec![0u8; chroma_width * chroma_height];
        assert!(convert_nv12_to_i420(
            &y,
            y_stride,
            &uv,
            uv_stride,
            &mut out_y,
            width as u32,
            &mut out_u,
            chroma_width as u32,
            &mut out_v,
            chroma_width as u32,
            width as u32,
            height as u32,
        ));

        assert_eq!(out_y, expected_y);
        assert_eq!(out_u, expected_u);
        assert_eq!(out_v, expected_v);

        for block_y in 0..chroma_height {
            for block_x in 0..chroma_width {
                let (name, expected_rgb) = colors[(block_y * chroma_width) + block_x];
                let i420_index = (block_y * 2 * width) + (block_x * 2);
                let chroma_index = (block_y * chroma_width) + block_x;
                let ycbcr = video_color::YCbCr8 {
                    y: out_y[i420_index],
                    cb: out_u[chroma_index],
                    cr: out_v[chroma_index],
                };
                let actual_rgb = ycbcr_to_rgb8(ycbcr, detected_profile);
                let wrong_rgb = ycbcr_to_rgb8(ycbcr, wrong_profile);

                assert_rgb_near(name, actual_rgb, expected_rgb, 2);
                if matches!(name, "red" | "green" | "blue") {
                    assert!(
                        rgb_channel_distance(wrong_rgb, expected_rgb) >= 8,
                        "{name}: BT.601 fallback decode should be measurably wrong; got {wrong_rgb:?}"
                    );
                }
            }
        }

        let mut wrong_y = vec![0u8; width * height];
        for block_y in 0..chroma_height {
            for block_x in 0..chroma_width {
                let (_, rgb) = colors[(block_y * chroma_width) + block_x];
                let ycbcr = video_color::rgb_to_ycbcr_8bit(rgb, wrong_profile);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let x = (block_x * 2) + dx;
                        let y = (block_y * 2) + dy;
                        wrong_y[(y * width) + x] = ycbcr.y;
                    }
                }
            }
        }
        assert_ne!(
            out_y, wrong_y,
            "BT.709-tagged saturated patches must not match BT.601 luma"
        );
    }

    #[cfg(target_os = "macos")]
    fn fill_nv12_cv_pixel_buffer(
        pixel_buffer: &screencapturekit::cv::CVPixelBuffer,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        width: usize,
    ) {
        let mut guard = pixel_buffer
            .lock(screencapturekit::cv::CVPixelBufferLockFlags::NONE)
            .expect("test CVPixelBuffer must lock for writing");
        assert!(guard.plane_count() >= 2);
        let y_stride = guard.bytes_per_row_of_plane(0);
        let uv_stride = guard.bytes_per_row_of_plane(1);
        let y_height = guard.height_of_plane(0);
        let chroma_height = guard.height_of_plane(1);
        let chroma_width = width / 2;
        let dst_y = guard
            .base_address_of_plane_mut(0)
            .expect("test NV12 buffer must expose a writable Y plane");
        let dst_uv = guard
            .base_address_of_plane_mut(1)
            .expect("test NV12 buffer must expose a writable UV plane");

        for row in 0..y_height {
            let src = &y[(row * width)..((row + 1) * width)];
            let dst = unsafe { std::slice::from_raw_parts_mut(dst_y.add(row * y_stride), width) };
            dst.copy_from_slice(src);
        }
        for row in 0..chroma_height {
            let dst = unsafe { std::slice::from_raw_parts_mut(dst_uv.add(row * uv_stride), width) };
            for x in 0..chroma_width {
                let index = (row * chroma_width) + x;
                dst[x * 2] = u[index];
                dst[(x * 2) + 1] = v[index];
            }
        }
    }

    fn ycbcr_to_rgb8(ycbcr: video_color::YCbCr8, profile: VideoColorProfile) -> video_color::Rgb8 {
        let (kr, kb) = match profile.matrix {
            video_color::MatrixCoefficients::Bt601 => (0.2990, 0.1140),
            video_color::MatrixCoefficients::Bt709 => (0.2126, 0.0722),
        };
        let kg = 1.0 - kr - kb;
        let (y, cb, cr) = match profile.range {
            video_color::PixelRange::Video => (
                (f64::from(ycbcr.y) - 16.0) / 219.0,
                (f64::from(ycbcr.cb) - 128.0) / 224.0,
                (f64::from(ycbcr.cr) - 128.0) / 224.0,
            ),
            video_color::PixelRange::Full => (
                f64::from(ycbcr.y) / 255.0,
                (f64::from(ycbcr.cb) - 128.0) / 255.0,
                (f64::from(ycbcr.cr) - 128.0) / 255.0,
            ),
        };
        let r = y + (2.0 * (1.0 - kr) * cr);
        let b = y + (2.0 * (1.0 - kb) * cb);
        let g = (y - (kr * r) - (kb * b)) / kg;

        video_color::Rgb8 {
            r: clamp_rgb8(r),
            g: clamp_rgb8(g),
            b: clamp_rgb8(b),
        }
    }

    fn clamp_rgb8(v: f64) -> u8 {
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    }

    fn assert_rgb_near(
        name: &str,
        actual: video_color::Rgb8,
        expected: video_color::Rgb8,
        tolerance: i16,
    ) {
        for (channel, actual, expected) in [
            ("r", actual.r, expected.r),
            ("g", actual.g, expected.g),
            ("b", actual.b, expected.b),
        ] {
            assert!(
                (i16::from(actual) - i16::from(expected)).abs() <= tolerance,
                "{name} {channel}: decoded {actual}, expected {expected}"
            );
        }
    }

    fn rgb_channel_distance(a: video_color::Rgb8, b: video_color::Rgb8) -> i16 {
        [
            (i16::from(a.r) - i16::from(b.r)).abs(),
            (i16::from(a.g) - i16::from(b.g)).abs(),
            (i16::from(a.b) - i16::from(b.b)).abs(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }

    #[test]
    #[ignore = "manual timing check; run with --ignored --nocapture when touching conversion speed"]
    fn bt709_bgra_to_i420_1080p_timing() {
        let width = 1920usize;
        let height = 1080usize;
        let mut src = vec![0u8; width * height * 4];
        for (i, pixel) in src.chunks_exact_mut(4).enumerate() {
            pixel[0] = (i & 0xff) as u8;
            pixel[1] = ((i >> 8) & 0xff) as u8;
            pixel[2] = ((i >> 16) & 0xff) as u8;
            pixel[3] = 255;
        }
        let mut y = vec![0u8; width * height];
        let mut u = vec![0u8; (width / 2) * (height / 2)];
        let mut v = vec![0u8; (width / 2) * (height / 2)];

        let start = std::time::Instant::now();
        assert!(convert_apple_bgra_to_i420(
            &src,
            width * 4,
            &mut y,
            width as u32,
            &mut u,
            (width / 2) as u32,
            &mut v,
            (width / 2) as u32,
            width as u32,
            height as u32,
            VideoColorProfile::SRGB_BT709_FULL,
        ));
        let elapsed = start.elapsed();
        println!(
            "BT.709 full-range BGRA->I420 1920x1080 conversion took {:.3} ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }
}

/// Whether the sender's actual live RTP encodings (by RID) cover every RID
/// a quality-only live-parameter update targets. The selected ladder owns
/// the target shape: legacy and raised use q/h/f, while two-rung variants use q/h.
/// Applying an update by a missing RID would silently no-op a cap or
/// misdirect it, so this check remains fail-closed.
fn live_encoding_shape_covers_updates(
    live_rids: &std::collections::HashSet<String>,
    updates: &[livekit::prelude::PublishingLayerParameters],
) -> bool {
    updates.iter().all(|update| live_rids.contains(&update.rid))
}

/// Core of `PublishedTrack::record_push_outcome`, extracted so a #866-shaped
/// storm (a long run of consecutive failed/mismatched pushes) is
/// unit-testable without a live `PublishedTrack` -- it holds an `Arc<Room>`
/// and a `NativeVideoSource` and cannot be built offline (see the
/// `RecoveryHarness` doc comment below). Returns the diagnostic to capture,
/// if the drop streak just tripped.
fn push_drop_streak_diagnostic(
    detector: &Mutex<crate::logging::DropStreakDetector>,
    track_name: &str,
    published: bool,
    now: std::time::Instant,
) -> Option<crate::logging::SentryDiagnosticEvent> {
    let mut guard = detector.lock_unpoisoned();
    let tripped = guard.record(published, now);
    drop(guard);
    if !tripped {
        return None;
    }
    let scope = if track_name.starts_with(CAMERA_TRACK_PREFIX) {
        crate::logging::StormScopeTag::Camera
    } else {
        crate::logging::StormScopeTag::WindowShare
    };
    Some(crate::logging::SentryDiagnosticEvent::PublishDropStreak(
        crate::logging::StormDiagnostic {
            role: crate::logging::DiagnosticRole::Sharer,
            scope,
        },
    ))
}

impl PublishedTrack {
    pub fn room(&self) -> Arc<Room> {
        self.room.clone()
    }

    pub fn quality(&self) -> ShareQuality {
        *self
            .quality
            .lock()
            .expect("published track quality mutex poisoned")
    }

    /// Apply a quality-tier change to the existing sender. The simulcast
    /// layout is fixed at publish time, so focus/dynacast flips only update
    /// live RTP encoding limits and retain the track SID.
    ///
    /// `layer_parameters` follows the shape resolved at publication: legacy
    /// and raised use q/h/f, while two-rung variants use q/h with their top on h. Verify
    /// the sender's actual live encodings cover every target before touching
    /// anything; otherwise fail closed so the caller can republish instead of
    /// applying a partial or wrong-layer update.
    pub async fn set_quality(&self, quality: ShareQuality) -> Result<(), RoomConnectionError> {
        if self.quality() == quality {
            return Ok(());
        }
        let updates = quality.layer_parameters(self.width(), self.height(), self.simulcast_ladder);
        let live_rids: std::collections::HashSet<String> = self
            .track
            .publishing_layer_parameters()
            .into_iter()
            .map(|layer| layer.rid)
            .collect();
        if !live_encoding_shape_covers_updates(&live_rids, &updates) {
            return Err(RoomConnectionError::Connect(RoomError::Internal(format!(
                "live sender encoding shape ({live_rids:?}) does not cover the {} ladder's expected update RIDs {:?} for track '{}' -- needs republish, not a live update",
                self.simulcast_ladder.as_str(),
                updates.iter().map(|update| update.rid.as_str()).collect::<Vec<_>>(),
                self.track.name()
            ))));
        }
        self.track
            .set_publishing_layer_parameters(&updates)
            .map_err(RoomConnectionError::Connect)?;
        *self
            .quality
            .lock()
            .expect("published track quality mutex poisoned") = quality;
        log::info!(
            "publisher: updated live sender parameters for track '{}' to {:?} ({:.0}fps)",
            self.track.name(),
            quality,
            quality.capture_fps()
        );
        Ok(())
    }

    pub fn width(&self) -> u32 {
        self.published_width
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn height(&self) -> u32 {
        self.published_height
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn published_size(&self) -> (u32, u32) {
        (self.width(), self.height())
    }

    pub fn track(&self) -> LocalVideoTrack {
        self.track.clone()
    }

    /// This track's `TrackSid`, needed to unpublish it later via
    /// `LocalParticipant::unpublish_track`.
    pub fn sid(&self) -> TrackSid {
        self.track.sid()
    }

    /// One place where a push outcome updates the drop-streak detector. A window
    /// share and a camera reach it from different entry points; the scope tag comes
    /// from the track name so the Sentry event says which one stopped (#788).
    fn record_push_outcome(&self, published: bool) {
        if let Some(event) = push_drop_streak_diagnostic(
            &self.push_drop_streak,
            &self.track.name(),
            published,
            std::time::Instant::now(),
        ) {
            crate::logging::capture_sentry_diagnostic(event);
        }
    }

    /// Republish this track's already-created `LocalVideoTrack` onto its
    /// room's local participant, rebuilding publish options from this
    /// track's published size and frame rate (#713 camera reconnect repair).
    /// Confirmed in
    /// the vendored SDK (`vendor/livekit/src/room/mod.rs`'s
    /// `handle_restarted`) that a full reconnect's own republish attempt
    /// UNPUBLISHES before it tries to republish -- so when that attempt
    /// times out, this participant is left with no publication at all, not a
    /// stale one; a plain `publish_track` call is correct here, no
    /// `unpublish` first. Only used for the camera: it has no quality ladder
    /// to reconcile (see `publish_camera`'s doc comment), unlike a window
    /// share, so this reuses the SAME `LocalVideoTrack`/`NativeVideoSource`
    /// rather than tearing down and recreating capture -- matching the
    /// SDK's own republish shape (same Track Arc, new server-assigned SID).
    pub(crate) async fn republish_camera_after_reconnect(&self) -> Result<(), RoomConnectionError> {
        let (width, height) = self.published_size();
        self.room
            .local_participant()
            .publish_track(
                LocalTrack::Video(self.track.clone()),
                camera_publish_options(width, height, self.published_frame_rate),
            )
            .await?;
        log::info!(
            "publisher: camera track '{}' republished after reconnect publication repair",
            self.track.name()
        );
        Ok(())
    }

    /// Unpublish this track from its room. Does NOT close/disconnect the
    /// room itself -- callers with multiple tracks published on one room
    /// (SPEC.md §4.3 multi-window sharing) decide separately whether the
    /// room connection should be torn down (see `session.rs`).
    ///
    /// Quality changes use `PublishedTrack::set_quality`, which updates the
    /// existing sender's encoding limits in place. Dimension changes do NOT
    /// republish on Windows — frames are letterboxed to a fixed published
    /// size during a resize and the published size is re-anchored once the
    /// window settles (~2s; see `push_bgra`). Only the macOS session still
    /// republishes on a stable resize (its VideoToolbox encoder lifecycle
    /// tolerates re-creation; the MF/NVENC path on Windows must not).
    pub async fn unpublish(&self) -> Result<(), RoomConnectionError> {
        // Debug/test-only fault injection for the real hover-tab toggle path.
        // This lets the cockpit hold or fail the network tail after the local
        // capture/border boundary without changing release behavior (#420).
        #[cfg(any(test, debug_assertions))]
        {
            if let Ok(delay_ms) = std::env::var("PETAL_TEST_UNPUBLISH_DELAY_MS") {
                if let Ok(delay_ms) = delay_ms.parse::<u64>() {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
            if std::env::var("PETAL_TEST_UNPUBLISH_FAIL").is_ok() {
                return Err(RoomConnectionError::InvalidVideoConfig(
                    "injected unpublish failure".to_string(),
                ));
            }
        }
        self.room
            .local_participant()
            .unpublish_track(&self.sid())
            .await?;
        Ok(())
    }

    pub fn disable_native_zero_copy(&self, reason: &str) {
        if self
            .native_zero_copy_latch
            .lock_unpoisoned()
            .disable_without_timing(std::time::Instant::now())
        {
            log::warn!(
                "publisher: disabling native CVPixelBuffer publish path for track '{}' ({reason}; no native frame timing available); future frames will use NV12->I420 fallback for {:.1}s before re-probing",
                self.track.name(),
                NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF.as_secs_f64(),
            );
        }
    }

    fn disable_native_zero_copy_after_capture_failure(
        &self,
        reason: &str,
        capture_frame_elapsed: std::time::Duration,
    ) {
        let transition = self
            .native_zero_copy_latch
            .lock_unpoisoned()
            .record_native_capture_failure(std::time::Instant::now());
        let capture_frame_return_ms = capture_frame_elapsed.as_secs_f64() * 1000.0;
        match transition {
            NativeZeroCopyTransition::Disabled { fallback_for } => log::warn!(
                "publisher: disabling native CVPixelBuffer publish path for track '{}' ({reason}; native capture_frame took {:.1}ms); future frames will use NV12->I420 fallback for {:.1}s before re-probing",
                self.track.name(),
                capture_frame_return_ms,
                fallback_for.as_secs_f64(),
            ),
            NativeZeroCopyTransition::ReprobeStillSlow { fallback_for } => log::warn!(
                "publisher: native zero-copy re-probe failed after {capture_frame_return_ms:.1}ms ({reason}); keeping NV12->I420 fallback for {:.1}s before the next re-probe",
                fallback_for.as_secs_f64(),
            ),
            NativeZeroCopyTransition::None | NativeZeroCopyTransition::Reenabled => {
                unreachable!("native capture failures never leave zero-copy enabled")
            }
        }
    }

    fn record_native_capture_transition(&self, capture_frame_elapsed: std::time::Duration) {
        let transition = self
            .native_zero_copy_latch
            .lock_unpoisoned()
            .record_native_capture(std::time::Instant::now(), capture_frame_elapsed);
        let capture_frame_return_ms = capture_frame_elapsed.as_secs_f64() * 1000.0;
        match transition {
            NativeZeroCopyTransition::None => {}
            NativeZeroCopyTransition::Disabled { fallback_for } => log::warn!(
                "publisher: native capture_frame took {capture_frame_return_ms:.1}ms (> {NATIVE_CAPTURE_FRAME_STALL_THRESHOLD_MS:.1}ms) on {NATIVE_ZERO_COPY_SLOW_FRAME_STRIKES} slow frames within {:.1}s; future frames will use NV12->I420 fallback for {:.1}s before re-probing",
                NATIVE_ZERO_COPY_SLOW_FRAME_WINDOW.as_secs_f64(),
                fallback_for.as_secs_f64(),
            ),
            NativeZeroCopyTransition::ReprobeStillSlow { fallback_for } => log::warn!(
                "publisher: native zero-copy re-probe took {capture_frame_return_ms:.1}ms (> {NATIVE_CAPTURE_FRAME_STALL_THRESHOLD_MS:.1}ms); keeping NV12->I420 fallback for {:.1}s before the next re-probe",
                fallback_for.as_secs_f64(),
            ),
            NativeZeroCopyTransition::Reenabled => log::warn!(
                "publisher: re-enabling native CVPixelBuffer publish path for track '{}' after zero-copy re-probe completed in {capture_frame_return_ms:.1}ms",
                self.track.name(),
            ),
        }
    }

    /// Convert a captured frame to I420 and push it into the LiveKit
    /// video source, stamping SPEC.md §7's embedded measurement metadata
    /// (capture wall-clock timestamp + monotonic frame id) via LiveKit's
    /// built-in frame metadata trailer.
    ///
    /// Sender-side size constancy (#714): a captured frame whose size
    /// doesn't match the published size (an in-progress window resize, on
    /// EITHER platform) is never silently dropped and never pushed at its
    /// raw mismatched size either -- both conversion paths letterbox-scale
    /// it to the published size before pushing, so a resize never produces
    /// a multi-second gap with nothing reaching the receiver:
    /// - `push_bgra` (Windows): letterboxes AND re-anchors the published
    ///   size once the window settles (~2s) -- Windows removed track
    ///   republish-on-resize entirely (fcbfa4f4), so the push-level
    ///   reanchor is the ONLY mechanism that ever changes the encoded size.
    /// - `push_nv12_with_convert_started` (macOS window shares + the NV12
    ///   camera fallback): letterboxes to the CURRENT published size but
    ///   never reanchors it itself -- macOS's session pump
    ///   (`session::share::ResizeDebounce`) still owns the republish
    ///   decision (its own debounce, `RESIZE_REPUBLISH_STABLE_FRAMES`) and
    ///   is the only thing that changes `self.width()`/`self.height()`, via
    ///   a full unpublish/republish once the resize settles. Duplicating a
    ///   second, independent reanchor clock at this layer (Windows' scheme)
    ///   would race the session's own debounce -- whichever one flips the
    ///   published size first would silently mask the other, and the
    ///   session's republish is still required on macOS to keep the
    ///   simulcast ladder correct for the settled resolution, not just the
    ///   encoded pixels.
    pub fn push_frame(
        &self,
        captured: &CapturedFrame,
        capture_wall_time_us: u64,
    ) -> Option<PublishedFrameTiming> {
        let result = self.push_frame_inner(captured, capture_wall_time_us);
        self.record_push_outcome(result.is_some());
        result
    }

    fn push_frame_inner(
        &self,
        captured: &CapturedFrame,
        capture_wall_time_us: u64,
    ) -> Option<PublishedFrameTiming> {
        match &captured.payload {
            CapturedFramePayload::Native { pixel_buffer } => {
                // The zero-copy CVPixelBuffer path hands the buffer straight
                // to webrtc with no conversion step -- there is no I420
                // buffer here to letterbox into (doing so would require an
                // IOSurface-level scale/copy, defeating the entire point of
                // "zero-copy"). While the captured size doesn't match the
                // published size (an in-progress resize), skip the
                // zero-copy attempt and fall through to
                // `push_native_fallback`, which converts to NV12 -> I420
                // and CAN letterbox (see `push_nv12_with_convert_started`).
                let (published_width, published_height) = self.published_size();
                let size_matches_published =
                    captured.width == published_width && captured.height == published_height;
                let native_attempt_due = size_matches_published
                    && !self.native_publish_disabled_by_env
                    && self
                        .native_zero_copy_latch
                        .lock_unpoisoned()
                        .native_attempt_due(std::time::Instant::now());
                if native_attempt_due {
                    match self.push_native(pixel_buffer, capture_wall_time_us) {
                        Ok(timing) => {
                            self.record_native_capture_transition(
                                std::time::Duration::from_secs_f64(
                                    timing.capture_frame_return_ms / 1000.0,
                                ),
                            );
                            return Some(timing);
                        }
                        Err(failure) => {
                            self.disable_native_zero_copy_after_capture_failure(
                                failure.reason,
                                failure.capture_frame_elapsed,
                            );
                        }
                    }
                }
                self.push_native_fallback(
                    pixel_buffer,
                    captured.width,
                    captured.height,
                    captured.color_profile,
                    capture_wall_time_us,
                )
            }
            payload => self.push_copied_payload(
                payload,
                captured.width,
                captured.height,
                captured.color_profile,
                capture_wall_time_us,
                std::time::Instant::now(),
            ),
        }
    }

    #[cfg(target_os = "macos")]
    fn push_native(
        &self,
        pixel_buffer: &crate::capture::NativeCapturedPixelBuffer,
        capture_wall_time_us: u64,
    ) -> Result<PublishedFrameTiming, NativeCaptureFailure> {
        let frame_id = self
            .frame_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let capture_frame_started = std::time::Instant::now();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            // #179: LiveKit's ObjC bridge consumes/releases the CVPixelBufferRef.
            // Hand it a fresh retain, never the frame's owned reference.
            let retained_ptr = unsafe { pixel_buffer.retained_ptr_for_consuming_native_buffer() };
            let native_buffer = unsafe { NativeBuffer::from_cv_pixel_buffer(retained_ptr) };
            let frame = VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: 0,
                frame_metadata: Some(FrameMetadata {
                    user_timestamp: Some(capture_wall_time_us),
                    frame_id: Some(frame_id),
                }),
                buffer: &native_buffer,
            };
            self.rtc_source.capture_frame(&frame);
        }));
        let capture_frame_return_ms = capture_frame_started.elapsed().as_secs_f64() * 1000.0;
        if result.is_err() {
            return Err(NativeCaptureFailure {
                reason: "native capture_frame panicked",
                capture_frame_elapsed: capture_frame_started.elapsed(),
            });
        }
        Ok(PublishedFrameTiming {
            convert_ms: 0.0,
            capture_frame_return_ms,
        })
    }

    #[cfg(not(target_os = "macos"))]
    fn push_native(
        &self,
        _pixel_buffer: &crate::capture::NativeCapturedPixelBuffer,
        _capture_wall_time_us: u64,
    ) -> Result<PublishedFrameTiming, NativeCaptureFailure> {
        Err(NativeCaptureFailure {
            reason: "native capture buffers are not implemented for this platform",
            capture_frame_elapsed: std::time::Duration::ZERO,
        })
    }

    fn push_native_fallback(
        &self,
        pixel_buffer: &crate::capture::NativeCapturedPixelBuffer,
        width: u32,
        height: u32,
        color_profile: VideoColorProfile,
        capture_wall_time_us: u64,
    ) -> Option<PublishedFrameTiming> {
        let convert_started = std::time::Instant::now();
        let payload =
            match pixel_buffer.copy_nv12_payload_with_pool(Some(&self.native_fallback_pool)) {
                Ok(payload) => payload,
                Err(e) => {
                    log::warn!("publisher: native fallback could not copy NV12 payload: {e}");
                    return None;
                }
            };
        self.push_copied_payload(
            &payload,
            width,
            height,
            color_profile,
            capture_wall_time_us,
            convert_started,
        )
    }

    fn push_copied_payload(
        &self,
        payload: &CapturedFramePayload,
        width: u32,
        height: u32,
        color_profile: VideoColorProfile,
        capture_wall_time_us: u64,
        convert_started: std::time::Instant,
    ) -> Option<PublishedFrameTiming> {
        match payload {
            CapturedFramePayload::Nv12 {
                y,
                y_stride,
                uv,
                uv_stride,
            } => self.push_nv12_with_convert_started(
                y,
                *y_stride,
                uv,
                *uv_stride,
                width,
                height,
                capture_wall_time_us,
                convert_started,
            ),
            CapturedFramePayload::Bgra {
                data,
                bytes_per_row,
            } => self.push_bgra(
                data,
                *bytes_per_row,
                width,
                height,
                color_profile,
                capture_wall_time_us,
                convert_started,
            ),
            CapturedFramePayload::Native { pixel_buffer } => self.push_native_fallback(
                pixel_buffer,
                width,
                height,
                color_profile,
                capture_wall_time_us,
            ),
        }
    }

    fn push_bgra(
        &self,
        data: &[u8],
        bytes_per_row: usize,
        width: u32,
        height: u32,
        color_profile: VideoColorProfile,
        capture_wall_time_us: u64,
        convert_started: std::time::Instant,
    ) -> Option<PublishedFrameTiming> {
        let mut i420_pool = self.i420_pool.lock_unpoisoned();
        let i420 = i420_pool.buffer(width, height);
        let (stride_y, stride_u, stride_v) = i420.strides();

        // convert_apple_bgra_to_i420 enforces the copied payload's exact
        // bytes-per-row extent before any raw-pointer libyuv dispatch (#500).
        // libyuv-"ARGB" == Apple-BGRA byte order; rs_BGRAToI420 was the
        // blue-tint bug (see module doc / issue #24).
        let (data_y, data_u, data_v) = i420.data_mut();
        if !convert_apple_bgra_to_i420(
            data,
            bytes_per_row,
            data_y,
            stride_y,
            data_u,
            stride_u,
            data_v,
            stride_v,
            width,
            height,
            published_window_color_profile(color_profile),
        ) {
            return None;
        }
        let convert_ms = convert_started.elapsed().as_secs_f64() * 1000.0;

        let frame_id = self
            .frame_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Sender-side size constancy: letterbox-scale the captured frame to
        // the published size so the ENCODED size stays constant. webrtc
        // re-creates the encoder on ANY frame-size change and that churn
        // breaks the NVIDIA MF encoder after a few generations (proven
        // cross-machine 2026-08-05: MF hardware -> OpenH264 fallback -> 0
        // RTP -> receiver freeze). When the window SETTLES (~2s at one
        // size), re-anchor the published size to it: the next frame then
        // flows at the new size -> exactly ONE encoder recreation per
        // gesture. The receiver crops the interim letterbox bars.
        let (published_width, published_height) = self.published_size();
        if width != published_width || height != published_height {
            let now = std::time::Instant::now();
            let should_reanchor = {
                let mut settle = self
                    .resize_settle
                    .lock()
                    .expect("published track resize settle mutex poisoned");
                Self::settle_decides_reanchor(
                    &mut settle,
                    (width, height),
                    now,
                    REANCHOR_SETTLE_DWELL,
                )
            };
            if should_reanchor {
                self.published_width
                    .store(width, std::sync::atomic::Ordering::Relaxed);
                self.published_height
                    .store(height, std::sync::atomic::Ordering::Relaxed);
                log::info!(
                    "publisher: re-anchored '{}' published size to {width}x{height} (window settled; one encoder recreation)",
                    self.track.name()
                );
            }
        } else {
            *self
                .resize_settle
                .lock()
                .expect("published track resize settle mutex poisoned") = None;
        }
        let (published_width, published_height) = self.published_size();
        let mut scaled;
        let frame = if width == published_width && height == published_height {
            VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: 0,
                frame_metadata: Some(FrameMetadata {
                    user_timestamp: Some(capture_wall_time_us),
                    frame_id: Some(frame_id),
                }),
                buffer: &*i420,
            }
        } else {
            scaled = I420Buffer::new(published_width, published_height);
            if !Self::letterbox_scale_i420(
                &*i420,
                width,
                height,
                &mut scaled,
                published_width,
                published_height,
            ) {
                // Never-black-frame hard rule: `scaled` is a black-filled
                // canvas on failure. Drop this frame; the receiver holds the
                // last good one, and the next capture gets a fresh attempt.
                return None;
            }
            VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: 0,
                frame_metadata: Some(FrameMetadata {
                    user_timestamp: Some(capture_wall_time_us),
                    frame_id: Some(frame_id),
                }),
                buffer: &scaled,
            }
        };

        let capture_frame_started = std::time::Instant::now();
        self.rtc_source.capture_frame(&frame);
        let capture_frame_return_ms = capture_frame_started.elapsed().as_secs_f64() * 1000.0;
        Some(PublishedFrameTiming {
            convert_ms,
            capture_frame_return_ms,
        })
    }

    /// Letterbox-scale an I420 frame to fit inside (dst_w x dst_h) preserving
    /// aspect ratio, padding with black bars (Y=16, U/V=128). Keeps the
    /// published/encoded size constant across window resizes so webrtc never
    /// re-creates the encoder (the resize-freeze root cause). The scaled
    /// region sits centered; libyuv I420Scale (bilinear) writes into the
    /// (sw x sh) sub-region of the black-padded destination.
    /// Returns `false` on an `I420Scale` failure -- `dst` is left black-filled
    /// in that case (never the scaled source), so the caller MUST drop the
    /// frame rather than push `dst`: this repo's hard rule is "never show a
    /// black frame," and a black-filled letterbox canvas is exactly that.
    fn letterbox_scale_i420(
        src: &I420Buffer,
        src_w: u32,
        src_h: u32,
        dst: &mut I420Buffer,
        dst_w: u32,
        dst_h: u32,
    ) -> bool {
        let fit = ((dst_w as f32) / (src_w as f32)).min((dst_h as f32) / (src_h as f32));
        let sw = ((src_w as f32) * fit).round().max(1.0) as i32;
        let sh = ((src_h as f32) * fit).round().max(1.0) as i32;
        let off_x = ((dst_w as i32 - sw) / 2).max(0) as usize;
        let off_y = ((dst_h as i32 - sh) / 2).max(0) as usize;

        let (src_y, src_u, src_v) = src.data();
        let (src_sy, src_su, src_sv) = src.strides();
        let (dst_sy, dst_su, dst_sv) = dst.strides();
        let (dst_y, dst_u, dst_v) = dst.data_mut();
        let dst_wu = dst_w as usize;
        let dst_hu = dst_h as usize;

        // Black fill (Y=16, U/V=128); the scaled region overwrites the center.
        // div_ceil: an odd dst leaves a last chroma row/column that must still
        // be filled (U=0/V=0 there would show as a green fringe).
        for row in 0..dst_hu {
            let base = row * dst_sy as usize;
            dst_y[base..base + dst_wu].fill(16);
        }
        let chroma_rows = (dst_hu + 1) / 2;
        let chroma_cols = (dst_wu + 1) / 2;
        for row in 0..chroma_rows {
            let u_base = row * dst_su as usize;
            dst_u[u_base..u_base + chroma_cols].fill(128);
            let v_base = row * dst_sv as usize;
            dst_v[v_base..v_base + chroma_cols].fill(128);
        }

        let dst_y_ptr = dst_y.as_mut_ptr();
        let dst_u_ptr = dst_u.as_mut_ptr();
        let dst_v_ptr = dst_v.as_mut_ptr();
        let scale_failed = unsafe {
            webrtc_sys::yuv_helper::ffi::i420_scale(
                src_y.as_ptr(),
                src_sy as i32,
                src_u.as_ptr(),
                src_su as i32,
                src_v.as_ptr(),
                src_sv as i32,
                src_w as i32,
                src_h as i32,
                // libyuv writes at the offset with the FULL stride, so the
                // letterbox borders around the region stay black.
                dst_y_ptr.add(off_y * dst_sy as usize + off_x),
                dst_sy as i32,
                dst_u_ptr.add((off_y / 2) * dst_su as usize + off_x / 2),
                dst_su as i32,
                dst_v_ptr.add((off_y / 2) * dst_sv as usize + off_x / 2),
                dst_sv as i32,
                sw,
                sh,
            )
            .is_err()
        };
        if scale_failed {
            // `dst` is left black-filled here -- the caller must NOT push it
            // (never-black-frame hard rule). Dropping this one frame is safe:
            // receivers hold the last good frame across a gap, and the next
            // captured frame gets a fresh attempt.
            log::warn!("letterbox_scale_i420: I420Scale failed; dropping this frame");
        }
        !scale_failed
    }

    /// Re-anchor state machine: returns true (and clears the state) when the
    /// captured size has been stable for `dwell`. CRITICAL: `first_seen` is
    /// only (re)started when the tracked size CHANGES — refreshing it every
    /// frame would make the dwell never elapse, so the published size would
    /// never re-anchor and the share would stay letterboxed forever.
    fn settle_decides_reanchor(
        settle: &mut Option<((u32, u32), std::time::Instant)>,
        size: (u32, u32),
        now: std::time::Instant,
        dwell: std::time::Duration,
    ) -> bool {
        match *settle {
            Some((tracked, first_seen)) if tracked == size => {
                if now.duration_since(first_seen) >= dwell {
                    *settle = None;
                    true
                } else {
                    false // keep first_seen; the dwell is still pending
                }
            }
            _ => {
                *settle = Some((size, now));
                false
            }
        }
    }

    /// Convert one NV12 (bi-planar Y + interleaved UV) frame to I420 and push
    /// it. `rs_NV12ToI420` is the correct libyuv call for U-first
    /// interleaving; `rs_NV21ToI420` would swap U/V (the NV analogue of the
    /// issue #24 naming trap -- pinned by a unit test below).
    ///
    /// PUBLIC-API note (#714): despite the doc comment this used to carry,
    /// this method's only real callers are the camera pumps
    /// (`crate::camera_session::start_camera_frame_pump` on both platforms) --
    /// grep `.push_nv12(` before assuming otherwise. Window shares do NOT go
    /// through here: `PublishedTrack::push_frame` dispatches NV12 window frames
    /// to `push_nv12_with_convert_started` directly (via `push_copied_payload`).
    ///
    /// A size mismatch is dropped only briefly (#866): a camera that restarted at
    /// a new resolution used to drop EVERY frame forever, freezing every peer.
    /// `resolve_camera_push_size` owns that decision -- keep this a thin
    /// dispatcher over it, since it is the tested unit.
    pub fn push_nv12(
        &self,
        y: &[u8],
        y_stride: u32,
        uv: &[u8],
        uv_stride: u32,
        width: u32,
        height: u32,
        capture_wall_time_us: u64,
    ) -> Option<PublishedFrameTiming> {
        let result = self.push_nv12_inner(
            y,
            y_stride,
            uv,
            uv_stride,
            width,
            height,
            capture_wall_time_us,
        );
        self.record_push_outcome(result.is_some());
        result
    }

    fn push_nv12_inner(
        &self,
        y: &[u8],
        y_stride: u32,
        uv: &[u8],
        uv_stride: u32,
        width: u32,
        height: u32,
        capture_wall_time_us: u64,
    ) -> Option<PublishedFrameTiming> {
        let decision = resolve_camera_push_size(
            &self.camera_size_recovery,
            &self.published_width,
            &self.published_height,
            (width, height),
            std::time::Instant::now(),
        );
        if let Some(event) = decision.diagnostic {
            crate::logging::capture_sentry_diagnostic(event);
        }
        if decision.verdict == CameraPushVerdict::Drop {
            return None;
        }

        self.push_nv12_with_convert_started(
            y,
            y_stride,
            uv,
            uv_stride,
            width,
            height,
            capture_wall_time_us,
            std::time::Instant::now(),
        )
    }

    fn push_nv12_with_convert_started(
        &self,
        y: &[u8],
        y_stride: u32,
        uv: &[u8],
        uv_stride: u32,
        width: u32,
        height: u32,
        capture_wall_time_us: u64,
        convert_started: std::time::Instant,
    ) -> Option<PublishedFrameTiming> {
        let mut i420_pool = self.i420_pool.lock_unpoisoned();
        let i420 = i420_pool.buffer(width, height);
        let (stride_y, stride_u, stride_v) = i420.strides();
        let (data_y, data_u, data_v) = i420.data_mut();

        let mut nv12_scratch = self.nv12_scratch.lock_unpoisoned();
        if !convert_nv12_to_i420_with_scratch(
            &mut nv12_scratch,
            y,
            y_stride,
            uv,
            uv_stride,
            data_y,
            stride_y,
            data_u,
            stride_u,
            data_v,
            stride_v,
            width,
            height,
        ) {
            log::warn!("publisher: NV12->I420 conversion failed for {width}x{height}");
            return None;
        }
        let convert_ms = convert_started.elapsed().as_secs_f64() * 1000.0;
        self.push_i420_letterboxed(&*i420, width, height, capture_wall_time_us, convert_ms)
    }

    /// Push an already-converted I420 frame, letterbox-scaling it to the
    /// PUBLISHED size (`self.width()`/`self.height()`) whenever `width`x
    /// `height` (the just-converted CAPTURED size) doesn't match it (#714).
    ///
    /// Deliberately does NOT reanchor the published size itself, unlike
    /// `push_bgra`'s Windows-only reanchor clock -- see `push_frame`'s doc
    /// comment for why a second independent reanchor here would race
    /// macOS's session-level resize debounce
    /// (`session::share::ResizeDebounce`), which remains the sole authority
    /// on when `self.width()`/`self.height()` actually change (a real
    /// unpublish/republish, needed to keep the simulcast ladder correct for
    /// the settled resolution). This function only guarantees that no
    /// resize, however long it takes to settle and republish, produces a
    /// gap with nothing reaching the receiver.
    fn push_i420_letterboxed(
        &self,
        i420: &I420Buffer,
        width: u32,
        height: u32,
        capture_wall_time_us: u64,
        convert_ms: f64,
    ) -> Option<PublishedFrameTiming> {
        let frame_id = self
            .frame_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (published_width, published_height) = self.published_size();
        let mut scaled;
        let frame = if width == published_width && height == published_height {
            VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: 0,
                frame_metadata: Some(FrameMetadata {
                    user_timestamp: Some(capture_wall_time_us),
                    frame_id: Some(frame_id),
                }),
                buffer: i420,
            }
        } else {
            scaled = I420Buffer::new(published_width, published_height);
            if !Self::letterbox_scale_i420(
                i420,
                width,
                height,
                &mut scaled,
                published_width,
                published_height,
            ) {
                // Never-black-frame hard rule: `scaled` is a black-filled
                // canvas on failure. Drop this frame; the receiver holds the
                // last good one, and the next capture gets a fresh attempt.
                return None;
            }
            VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: 0,
                frame_metadata: Some(FrameMetadata {
                    user_timestamp: Some(capture_wall_time_us),
                    frame_id: Some(frame_id),
                }),
                buffer: &scaled,
            }
        };
        let capture_frame_started = std::time::Instant::now();
        self.rtc_source.capture_frame(&frame);
        let capture_frame_return_ms = capture_frame_started.elapsed().as_secs_f64() * 1000.0;
        Some(PublishedFrameTiming {
            convert_ms,
            capture_frame_return_ms,
        })
    }
}

async fn log_encoder_once(
    track: LocalVideoTrack,
    origin: EncoderPublishOrigin,
    recovery: Option<PostWakeEncoderFallbackRecovery>,
) {
    let track_name = track.name().to_string();
    log_encoder_once_with(
        &track_name,
        || {
            let track = track.clone();
            async move {
                let Ok(stats) = track.get_stats().await else {
                    return None;
                };
                let sender_parameters = track.publishing_layer_parameters();
                log::info!(
                    "publisher: sender encodings for '{}': {:?}; backend-specific GOP, frame-reordering, and rate-control properties are not exposed by pinned libwebrtc",
                    track.name(),
                    sender_parameters
                );
                stats.iter().find_map(|stat| {
                    let livekit::webrtc::stats::RtcStats::OutboundRtp(outbound) = stat else {
                        return None;
                    };
                    if outbound.stream.kind != "video"
                        || outbound.outbound.encoder_implementation.is_empty()
                    {
                        return None;
                    }
                    let h264_fmtp =
                        codec_fmtp_for_id(&stats, &outbound.stream.codec_id).unwrap_or_default();
                    let h264_profile = h264_profile_level_id_from_fmtp(&h264_fmtp)
                        .unwrap_or("unknown")
                        .to_string();
                    Some(EncoderObservation {
                        implementation: outbound.outbound.encoder_implementation.clone(),
                        power_efficient: outbound.outbound.power_efficient_encoder,
                        h264_profile,
                        h264_fmtp,
                    })
                })
            }
        },
        || tokio::time::sleep(std::time::Duration::from_millis(500)),
        origin,
        recovery,
    )
    .await;
}

// ---- #907 step 2/7: top-rung starvation guard --------------------------
//
// The field incident: the top simulcast rung stayed published at its full
// configured ceiling while the allocator funded it at ~3.6% of that (288kbps
// of 8,000,000), and the receiver's unconditional HIGH request (fixed
// separately in `transport::subscriber`) landed it on exactly that rung. A
// starved rung sitting at its full ceiling keeps asking the congestion
// controller for bandwidth the healthy lower rung could use instead. This
// guard, driven from the SAME `OutboundRtpStreamStats` poll
// `log_window_share_encoder_stats` already runs, THROTTLES the top rung's
// live ceiling once sustained starvation is observed, and periodically
// re-probes whether it can be restored, giving up after repeated failed
// probes -- the sender-side counterpart to the receiver-side downgrade/
// re-probe/give-up guard in `transport::subscriber`
// (`starvation_action`/`starvation_action_for_macos`).
//
// #907 adversarial review (counselors, two independent models) changed this
// mechanism twice from its first version; both changes are load-bearing:
//
// 1. This does NOT stop the rung (no `TrackUnpublished`, no unsubscribe, no
//    `encoding.active = false` -- the vendored SDK's public
//    `set_publishing_layer_parameters` cannot touch `active` at all, only
//    `max_bitrate`/`max_framerate`; see `apps/desktop/vendor/livekit/src/room/track/local_video_track.rs`).
//    It only shrinks the live bitrate ceiling to a small floor. A viewer
//    still subscribed to this rid keeps receiving SOME frames on it, likely
//    at degraded quality -- this mechanism alone does NOT make that rid
//    watchable. What actually stops a viewer from watching a still-degraded
//    rung is the RECEIVER's own starvation guard (`transport::subscriber`),
//    which (once its own critical timer bug is fixed -- see that module) now
//    reliably detects sustained bad quality even while frames keep arriving
//    and downgrades away from this rid on its own. This guard's actual job
//    is narrower and more honest than "fix the rung": reduce how much of the
//    link a rung nobody can use anyway keeps asking for, freeing that
//    capacity for the rung a downgraded viewer is actually watching. Do not
//    read a name like "the guard fixes the starved rung" into this code --
//    it reduces contention; the receiver decides what's watchable.
// 2. It reads its "what SHOULD this rid's ceiling be right now" answer FRESH
//    on every single poll, from the CURRENT `ShareQuality`/published size
//    (`quality`/`published_width`/`published_height`, all `Arc`-shared with
//    the same `PublishedTrack` that `set_quality` mutates), and identifies
//    the rid to guard via `FullShareSimulcastLadder::top_rid()` -- NOT a
//    frozen snapshot taken at guard-construction time, and NOT "whichever
//    configured layer has the largest bitrate" (at 4K Reduced quality, `q`'s
//    halved ceiling can exceed `h`'s, which would have silently guarded the
//    WRONG rung). A frozen/heuristic version raced Full/Reduced quality
//    flips and could restore stale values or misidentify the rung entirely;
//    see #907's adversarial-review findings for the exact scenarios.

/// Below this fraction of its OWN configured ceiling, a rung's allocator-
/// granted bitrate is starvation, not noise. 25% keeps real transient dips
/// (a brief congestion blip) from tripping the guard while catching the
/// field-observed case by a wide margin (3.6%).
const RUNG_STARVATION_GUARD_FRACTION: f64 = 0.25;
/// Consecutive 5s samples of sustained starvation before throttling (~15s) --
/// long enough that one bad sample can't trip it.
const RUNG_STARVATION_GUARD_TRIGGER_SAMPLES: u32 = 3;
/// Base consecutive-5s-sample interval before the FIRST re-probe (~30s).
/// Subsequent probe intervals back off exponentially -- see
/// `rung_starvation_probe_interval_samples` -- deliberately matching
/// `transport::subscriber`'s `STARVATION_PROBE_BASE`/`STARVATION_PROBE_MAX`/
/// `STARVATION_PROBE_FAILURE_CAP` cadence and failure cap by design (not
/// shared code across the publisher/subscriber module boundary, but kept
/// numerically identical on purpose): #907's adversarial review found the
/// FIRST version of this guard had NO failure cap at all and re-probed on a
/// fixed 30s cadence forever, which can beat against the receiver's own
/// independent probe/give-up cycle indefinitely (neither side has a shared
/// clock -- there is no way to phase-lock two independently-triggered
/// cycles over an SFU). Giving BOTH sides a real give-up state does not
/// eliminate every possible beat, but it guarantees the system reaches a
/// stable end state (sender throttled + receiver on LOW) instead of
/// oscillating forever once neither side is still actively intervening.
const RUNG_STARVATION_GUARD_PROBE_BASE_SAMPLES: u32 = 6;
/// Exponential backoff cap for repeated probe failures (~120s), matching
/// `transport::subscriber::STARVATION_PROBE_MAX`.
const RUNG_STARVATION_GUARD_PROBE_MAX_SAMPLES: u32 = 24;
/// Consecutive failed probes before giving up on restoring this rung for the
/// rest of this publish's lifetime (a republish/reconnect creates a fresh
/// `PublishedTrack` and a fresh guard with clean state) -- matches
/// `transport::subscriber::STARVATION_PROBE_FAILURE_CAP`.
const RUNG_STARVATION_GUARD_PROBE_FAILURE_CAP: u32 = 3;
/// Live ceiling applied to a throttled rung. Not literally 0: a nonzero
/// floor keeps the encoding "throttled but not degenerate" for whatever
/// brief window it takes libwebrtc to actually stop spending bitrate on it.
const RUNG_STARVATION_GUARD_THROTTLED_BITRATE_BPS: u64 = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RungFundingState {
    /// Publishing at its full configured ceiling; funding looks fine (or
    /// hasn't yet been sustained-starved long enough to say otherwise).
    Funded,
    /// Ceiling shrunk to `RUNG_STARVATION_GUARD_THROTTLED_BITRATE_BPS` after
    /// sustained starvation at the configured ceiling. The rung is NOT
    /// stopped -- see this section's module doc comment for why that
    /// distinction matters.
    Throttled,
    /// Ceiling was just restored to its configured value for exactly one
    /// sample interval, to test whether funding has recovered.
    Probing,
    /// Gave up after `RUNG_STARVATION_GUARD_PROBE_FAILURE_CAP` failed
    /// probes: stays throttled and stops actively re-probing for the rest of
    /// this publish's lifetime. Still re-evaluated every sample so an
    /// EXTERNAL change (e.g. a quality switch that legitimately restores
    /// funding) is still recognized -- this guard just never initiates
    /// another probe of its own from here.
    GivenUp,
}

/// Is this one sample starved, relative to the rung's CURRENT configured
/// ceiling (looked up fresh every poll -- see `log_window_share_encoder_stats`)?
/// Comparing against the live, possibly-throttled ceiling instead would make
/// "starved" trivially true forever once throttled; the caller always passes
/// the fresh CONFIGURED value, never whatever this guard last wrote.
fn rung_is_starved(target_bitrate_bps: u64, configured_max_bitrate_bps: u64) -> bool {
    configured_max_bitrate_bps > 0
        && (target_bitrate_bps as f64) < RUNG_STARVATION_GUARD_FRACTION * configured_max_bitrate_bps as f64
}

/// Probe interval after `consecutive_probe_failures` (30s, 60s, 120s,
/// capped), in samples at the 5s poll cadence. Mirrors
/// `transport::subscriber::starvation_probe_delay`.
fn rung_starvation_probe_interval_samples(consecutive_probe_failures: u32) -> u32 {
    let multiplier = 1u32 << consecutive_probe_failures.min(2);
    RUNG_STARVATION_GUARD_PROBE_BASE_SAMPLES
        .saturating_mul(multiplier)
        .min(RUNG_STARVATION_GUARD_PROBE_MAX_SAMPLES)
}

/// One hysteresis step of the starvation guard's state machine. Pure and
/// unit-testable without a live encoder: takes the current state, whether
/// THIS sample was starved, and the running consecutive-sample and
/// consecutive-probe-failure counts; returns the next counts and next state.
/// The caller applies the live SDK call only on an actual state change (see
/// `log_window_share_encoder_stats`).
fn rung_starvation_next_state(
    current: RungFundingState,
    sample_starved: bool,
    consecutive_samples: u32,
    consecutive_probe_failures: u32,
) -> (u32, u32, RungFundingState) {
    match current {
        RungFundingState::Funded => {
            if sample_starved {
                let count = consecutive_samples + 1;
                if count >= RUNG_STARVATION_GUARD_TRIGGER_SAMPLES {
                    (0, 0, RungFundingState::Throttled)
                } else {
                    (count, 0, RungFundingState::Funded)
                }
            } else {
                (0, 0, RungFundingState::Funded)
            }
        }
        RungFundingState::Throttled => {
            if !sample_starved {
                // The fresh sample already shows healthy funding against the
                // CURRENT configured ceiling -- either an external reset
                // (e.g. a quality change re-wrote all layer parameters) or
                // genuine recovery. Either way there is no need to wait out
                // the probe interval; the evidence has already arrived.
                return (0, 0, RungFundingState::Funded);
            }
            let count = consecutive_samples + 1;
            let interval = rung_starvation_probe_interval_samples(consecutive_probe_failures);
            if count >= interval {
                (0, consecutive_probe_failures, RungFundingState::Probing)
            } else {
                (count, consecutive_probe_failures, RungFundingState::Throttled)
            }
        }
        // The probe sample decides immediately: no need to re-accumulate the
        // trigger threshold, since the probe itself IS the fresh evidence.
        RungFundingState::Probing => {
            if sample_starved {
                let failures = consecutive_probe_failures + 1;
                if failures >= RUNG_STARVATION_GUARD_PROBE_FAILURE_CAP {
                    (0, failures, RungFundingState::GivenUp)
                } else {
                    (0, failures, RungFundingState::Throttled)
                }
            } else {
                (0, 0, RungFundingState::Funded)
            }
        }
        RungFundingState::GivenUp => {
            // Still recognize an external recovery (e.g. a quality switch),
            // but never initiate another probe of our own from here.
            if !sample_starved {
                (0, 0, RungFundingState::Funded)
            } else {
                (0, consecutive_probe_failures, RungFundingState::GivenUp)
            }
        }
    }
}

/// Guard state for the one rung this track's guard protects (the ladder's
/// top rid, per `FullShareSimulcastLadder::top_rid()` -- never "whichever
/// layer has the largest bitrate," which picks the WRONG rung at 4K Reduced
/// quality; see this section's module doc comment).
struct RungStarvationGuard {
    rid: String,
    state: RungFundingState,
    consecutive_samples: u32,
    consecutive_probe_failures: u32,
}

impl RungStarvationGuard {
    /// Only guards a track with more than one published layer: a
    /// non-simulcast (single-layer) publish has no other rung to fall back
    /// to, so throttling its only layer would meaningfully degrade the whole
    /// share for no offsetting benefit (no lower rung to free capacity for).
    fn for_rid(rid: &str, layer_count: usize) -> Option<Self> {
        if layer_count < 2 {
            return None;
        }
        Some(Self {
            rid: rid.to_string(),
            state: RungFundingState::Funded,
            consecutive_samples: 0,
            consecutive_probe_failures: 0,
        })
    }

    /// Apply one new sample for this guard's rid, given the rid's CURRENT
    /// configured ceiling (looked up fresh by the caller every poll -- see
    /// `log_window_share_encoder_stats`). Returns `Some(new_state)` only
    /// when the state actually changed THIS sample, so the caller knows
    /// exactly when to touch the live sender (and to log the transition --
    /// #907 step 7: one line per state change, never per frame/sample).
    fn observe(&mut self, target_bitrate_bps: u64, configured_bitrate_bps: u64) -> Option<RungFundingState> {
        let starved = rung_is_starved(target_bitrate_bps, configured_bitrate_bps);
        let (count, failures, next_state) = rung_starvation_next_state(
            self.state,
            starved,
            self.consecutive_samples,
            self.consecutive_probe_failures,
        );
        self.consecutive_samples = count;
        self.consecutive_probe_failures = failures;
        if next_state == self.state {
            return None;
        }
        self.state = next_state;
        Some(next_state)
    }

    /// The live `(max_bitrate, max_framerate)` this guard's rid should carry
    /// after transitioning to `state`, given the CURRENT configured ceiling
    /// (fetched fresh by the caller). `Probing` and `Funded` both restore the
    /// current configured ceiling -- the only difference between them is
    /// bookkeeping (whether the NEXT sample is judged as a probe result).
    /// `GivenUp` keeps the throttled floor (it is only entered FROM
    /// `Probing` while still starved).
    fn live_parameters_for(
        state: RungFundingState,
        configured_bitrate_bps: u64,
        configured_framerate: f64,
    ) -> (u64, f64) {
        match state {
            RungFundingState::Funded | RungFundingState::Probing => {
                (configured_bitrate_bps, configured_framerate)
            }
            RungFundingState::Throttled | RungFundingState::GivenUp => {
                (RUNG_STARVATION_GUARD_THROTTLED_BITRATE_BPS, configured_framerate)
            }
        }
    }
}

/// The ladder's top rid's CURRENT configured `(max_bitrate, max_framerate)`,
/// computed fresh from live, authoritative state -- the exact same
/// computation `PublishedTrack::set_quality` uses to build its own updates,
/// so the guard and a normal quality switch can never disagree about what
/// "correct" looks like for this rid right now. `None` if the ladder's top
/// rid is missing from the computed layer list (should not happen; a defensive
/// guard against a future ladder/rid mismatch, not an expected runtime path).
fn current_top_rid_parameters(
    quality: ShareQuality,
    width: u32,
    height: u32,
    ladder: FullShareSimulcastLadder,
) -> Option<(String, u64, f64)> {
    let top_rid = ladder.top_rid();
    quality
        .layer_parameters(width, height, ladder)
        .into_iter()
        .find(|layer| layer.rid == top_rid)
        .map(|layer| (layer.rid, layer.max_bitrate, layer.max_framerate))
}

/// Periodic window-share encoder diagnostic (see its spawn site): every few
/// seconds, log each video simulcast layer's ACTUAL encoder output from
/// libwebrtc stats -- target bitrate, encoded resolution, fps, average QP,
/// and the quality-limitation reason. Receiver-side text fuzz at a 1:1
/// window is almost always the encoder being rate-limited/downscaled/
/// QP-limited on the HOST; these numbers make that visible instead of
/// inferred.
///
/// #907: also drives the top-rung starvation guard above off this exact same
/// poll -- one source of truth for "is this rung actually being funded,"
/// used for both the diagnostic log line (existing, per-sample, `info`) and
/// the guard's own state-change-triggered `warn` lines (new, NEVER
/// per-sample -- see #905, which is specifically about per-frame log floods).
/// `quality`/`published_width`/`published_height` are the SAME `Arc`-shared
/// state `PublishedTrack::set_quality` mutates, so the guard always compares
/// against (and restores) the CURRENT configured ceiling, never a stale one
/// from whenever this task started.
async fn log_window_share_encoder_stats(
    track: LocalVideoTrack,
    quality: Arc<Mutex<ShareQuality>>,
    published_width: Arc<std::sync::atomic::AtomicU32>,
    published_height: Arc<std::sync::atomic::AtomicU32>,
    ladder: FullShareSimulcastLadder,
) {
    let mut guard: Option<RungStarvationGuard> = None;
    let mut guard_initialized = false;
    // Throttled warning for a repeatedly-failing `get_stats()` call (#907
    // review finding 8: silently ignoring every error left this diagnostic
    // -- and the starvation guard riding on it -- blind with no trace at
    // all). Logged at most once per throttle window, not per poll.
    let mut last_stats_error_logged: Option<std::time::Instant> = None;
    const STATS_ERROR_LOG_THROTTLE: std::time::Duration = std::time::Duration::from_secs(60);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let stats = match track.get_stats().await {
            Ok(stats) => stats,
            Err(error) => {
                let should_log = last_stats_error_logged
                    .is_none_or(|last| last.elapsed() >= STATS_ERROR_LOG_THROTTLE);
                if should_log {
                    last_stats_error_logged = Some(std::time::Instant::now());
                    log::warn!(
                        "publisher: window-share encoder stats poll failed for '{}': {error:?} (throttled to once per {}s; the #907 starvation guard riding on this poll cannot observe funding while this persists)",
                        track.name(),
                        STATS_ERROR_LOG_THROTTLE.as_secs()
                    );
                }
                continue;
            }
        };
        if !guard_initialized {
            guard_initialized = true;
            let layer_count = track.publishing_layer_parameters().len();
            guard = RungStarvationGuard::for_rid(ladder.top_rid(), layer_count);
        }
        // Fresh every poll -- see the module doc comment above for why a
        // one-time snapshot is exactly the bug an earlier version of this
        // guard had.
        let current_quality = *quality.lock().expect("published track quality mutex poisoned");
        let current_width = published_width.load(std::sync::atomic::Ordering::Relaxed);
        let current_height = published_height.load(std::sync::atomic::Ordering::Relaxed);
        let current_top = current_top_rid_parameters(current_quality, current_width, current_height, ladder);

        for stat in &stats {
            let livekit::webrtc::stats::RtcStats::OutboundRtp(outbound) = stat else {
                continue;
            };
            if outbound.stream.kind != "video" {
                continue;
            }
            let o = &outbound.outbound;
            let avg_qp = if o.frames_encoded > 0 {
                o.qp_sum as f64 / o.frames_encoded as f64
            } else {
                0.0
            };
            log::info!(
                "publisher: window-share encoder rid={} target={:.0}kbps encoded={}x{} fps={:.1} avg_qp={:.1} limitation={:?} frames_encoded={}",
                o.rid,
                o.target_bitrate / 1000.0,
                o.frame_width,
                o.frame_height,
                o.frames_per_second,
                avg_qp,
                o.quality_limitation_reason,
                o.frames_encoded,
            );

            let Some(guard) = guard.as_mut() else { continue };
            if o.rid != guard.rid {
                continue;
            }
            let Some((_, configured_bitrate_bps, configured_framerate)) = &current_top else {
                continue;
            };
            let Some(new_state) = guard.observe(o.target_bitrate as u64, *configured_bitrate_bps) else {
                continue;
            };
            let (max_bitrate, max_framerate) =
                RungStarvationGuard::live_parameters_for(new_state, *configured_bitrate_bps, *configured_framerate);
            let result = track.set_publishing_layer_parameters(&[
                livekit::prelude::PublishingLayerParameters {
                    rid: guard.rid.clone(),
                    max_bitrate,
                    max_framerate,
                },
            ]);
            match new_state {
                RungFundingState::Throttled => log::warn!(
                    "publisher: window-share top rung rid={} sustained-starved (target {:.0}kbps < {:.0}% of its configured {}kbps ceiling for {} samples) -- throttling its ceiling to {}kbps so it stops competing with the funded lower rung for bandwidth (this does NOT stop the rung -- see module doc comment); will re-probe in ~{}s (#907) [set_publishing_layer_parameters: {:?}]",
                    guard.rid,
                    o.target_bitrate / 1000.0,
                    RUNG_STARVATION_GUARD_FRACTION * 100.0,
                    configured_bitrate_bps / 1000,
                    RUNG_STARVATION_GUARD_TRIGGER_SAMPLES,
                    max_bitrate / 1000,
                    rung_starvation_probe_interval_samples(guard.consecutive_probe_failures) * 5,
                    result
                ),
                RungFundingState::Probing => log::warn!(
                    "publisher: window-share top rung rid={} restoring configured {}kbps ceiling for one sample to probe whether funding recovered (probe attempt {} of {}) (#907) [set_publishing_layer_parameters: {:?}]",
                    guard.rid,
                    configured_bitrate_bps / 1000,
                    guard.consecutive_probe_failures + 1,
                    RUNG_STARVATION_GUARD_PROBE_FAILURE_CAP,
                    result
                ),
                RungFundingState::Funded => log::warn!(
                    "publisher: window-share top rung rid={} funding recovered (target {:.0}kbps) -- staying at its configured {}kbps ceiling (#907) [set_publishing_layer_parameters: {:?}]",
                    guard.rid,
                    o.target_bitrate / 1000.0,
                    configured_bitrate_bps / 1000,
                    result
                ),
                RungFundingState::GivenUp => log::warn!(
                    "publisher: window-share top rung rid={} gave up after {} failed recovery probes -- staying throttled at {}kbps for the rest of this publish (a focus/quality switch or republish resets this) (#907) [set_publishing_layer_parameters: {:?}]",
                    guard.rid,
                    RUNG_STARVATION_GUARD_PROBE_FAILURE_CAP,
                    max_bitrate / 1000,
                    result
                ),
            }
        }
    }
}

/// Effect-injected form of [`log_encoder_once`]. Production and tests both
/// drive this same observation -> warning -> optional recovery chain; tests
/// substitute only the libwebrtc stats poll and clock.
async fn log_encoder_once_with<Observe, ObserveFuture, Wait, WaitFuture>(
    track_name: &str,
    mut observe: Observe,
    mut wait: Wait,
    origin: EncoderPublishOrigin,
    recovery: Option<PostWakeEncoderFallbackRecovery>,
) where
    Observe: FnMut() -> ObserveFuture,
    ObserveFuture: Future<Output = Option<EncoderObservation>>,
    Wait: FnMut() -> WaitFuture,
    WaitFuture: Future<Output = ()>,
{
    for _ in 0..20 {
        wait().await;
        let Some(observation) = observe().await else {
            continue;
        };
        log::info!(
            "CONFIRMED encoder_implementation = '{}' (power_efficient={}, h264_profile_level_id={}, sdp_fmtp_line='{}')",
            observation.implementation,
            observation.power_efficient,
            observation.h264_profile,
            observation.h264_fmtp,
        );
        if encoder_looks_software(&observation.implementation, observation.power_efficient) {
            if claim_software_encoder_warning() {
                log::warn!(
                    "software encoder suspected: encoder_implementation='{}' power_efficient={}; reduced performance is expected until quality step-down is measured",
                    observation.implementation,
                    observation.power_efficient,
                );
            }
            if origin == EncoderPublishOrigin::PostWakeRestart {
                let Some(recovery) = recovery else {
                    log::error!(
                        "publisher: post-wake track '{track_name}' has no encoder recovery action"
                    );
                    return;
                };
                log::warn!(
                    "publisher: post-wake track '{track_name}' selected a software encoder; scheduling one delayed hardware-encoder republish attempt"
                );
                recovery.run().await;
            }
        }
        return;
    }
    log::warn!("Never observed a video encoder_implementation in stats after 10s");
}

fn claim_software_encoder_warning() -> bool {
    !SOFTWARE_ENCODER_WARNED.swap(true, Ordering::SeqCst)
}

fn codec_fmtp_for_id(stats: &[livekit::webrtc::stats::RtcStats], codec_id: &str) -> Option<String> {
    if codec_id.is_empty() {
        return None;
    }

    stats.iter().find_map(|stat| match stat {
        livekit::webrtc::stats::RtcStats::Codec(codec) if codec.rtc.id == codec_id => {
            Some(codec.codec.sdp_fmtp_line.clone())
        }
        _ => None,
    })
}

fn h264_profile_level_id_from_fmtp(sdp_fmtp_line: &str) -> Option<&str> {
    sdp_fmtp_line.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        key.eq_ignore_ascii_case("profile-level-id")
            .then_some(value.trim())
    })
}

pub(crate) fn encoder_looks_software(implementation: &str, power_efficient: bool) -> bool {
    let normalized = implementation.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }
    // Hardware encoder markers. VideoToolbox is the macOS indicator; on
    // Windows the Media Foundation encoder reports itself as
    // "MF H264 Encoder (hardware)" (see vendor/webrtc-sys/src/mf/). Treat
    // any explicit "hardware" marker like VideoToolbox so the diagnostics
    // "software encoder suspected" heuristic does not false-positive on the
    // MF hardware path.
    let indicates_videotoolbox = normalized.contains("videotoolbox");
    let indicates_hardware = normalized.contains("hardware")
        || normalized.contains("nvenc")
        || normalized.contains("amf")
        || normalized.contains("qsv")
        || normalized.contains("vaapi")
        || normalized.contains("v4l2");
    let explicitly_software = normalized.contains("software")
        || normalized.contains("openh264")
        || normalized.contains("libx264")
        || normalized.contains("libaom");
    explicitly_software || (!indicates_videotoolbox && !indicates_hardware) || !power_efficient
}

#[cfg(test)]
mod tests {

    #[test]
    fn remote_control_permission_defaults_to_allowed_when_the_key_is_absent() {
        // A sharer that predates the key must keep working: absence is
        // ALLOWED, never denied, or every old peer loses its Control button.
        assert!(shared_window_remote_control_allowed_from_metadata(
            r#"{"petalWindowScales":{"7":2.0}}"#,
            7
        ));
        // Malformed / non-JSON / wrong value types all fail OPEN too -- this
        // is an affordance hint, not the authorization (that is host-side).
        for metadata in ["", "not json", "[]", r#"{"petalWindowRemoteControl":"nope"}"#] {
            assert!(
                shared_window_remote_control_allowed_from_metadata(metadata, 7),
                "{metadata:?} must not be read as a denial"
            );
        }
        assert!(shared_window_remote_control_allowed_from_metadata(
            r#"{"petalWindowRemoteControl":{"9":false}}"#,
            7
        ));
    }

    #[test]
    fn remote_control_permission_honors_an_explicit_denial() {
        assert!(!shared_window_remote_control_allowed_from_metadata(
            r#"{"petalWindowRemoteControl":{"7":false}}"#,
            7
        ));
    }

    #[test]
    fn only_denials_reach_the_wire() {
        // Encoding every "allowed" entry would bloat metadata and, worse,
        // make absence ambiguous. Allowed windows must leave no trace.
        let mut metadata = ShareMetadata::default();
        metadata.remote_control_allowed.insert(7, true);
        let encoded = encode_window_metadata(&metadata);
        assert!(
            !encoded.contains(PETAL_WINDOW_REMOTE_CONTROL_METADATA_KEY),
            "an allowed window must not emit the key at all: {encoded}"
        );
        assert!(shared_window_remote_control_allowed_from_metadata(&encoded, 7));

        metadata.remote_control_allowed.insert(9, false);
        let encoded = encode_window_metadata(&metadata);
        assert!(!shared_window_remote_control_allowed_from_metadata(&encoded, 9));
        assert!(
            shared_window_remote_control_allowed_from_metadata(&encoded, 7),
            "the allowed window must stay allowed alongside a denied one"
        );
    }

    #[test]
    fn stopping_a_share_clears_its_remote_control_entry() {
        // Otherwise a denial outlives the share and silently suppresses the
        // Control button on the NEXT share that reuses the window id.
        let mut metadata = ShareMetadata::default();
        metadata.generations.insert(7, 42);
        metadata.remote_control_allowed.insert(7, false);
        assert!(clear_share_metadata_for_generation(&mut metadata, 7, 42));
        assert!(!metadata.remote_control_allowed.contains_key(&7));
        assert!(shared_window_remote_control_allowed_from_metadata(
            &encode_window_metadata(&metadata),
            7
        ));
    }

    #[test]
    fn stage_shared_window_url_ignores_a_stale_generation() {
        let mut metadata = ShareMetadata::default();
        metadata.generations.insert(7, 42);
        assert!(!stage_shared_window_url(
            &mut metadata,
            7,
            41,
            Some("https://example.com/page".to_string())
        ));
        assert!(
            !metadata.urls.contains_key(&7),
            "a stale generation must not write the url"
        );
    }

    #[test]
    fn stage_shared_window_url_updates_a_matching_generation() {
        let mut metadata = ShareMetadata::default();
        metadata.generations.insert(7, 42);
        assert!(stage_shared_window_url(
            &mut metadata,
            7,
            42,
            Some("https://example.com/page?token=secret".to_string())
        ));
        // Privacy-minimized the same way the existing url setters are.
        assert_eq!(
            metadata.urls.get(&7).map(String::as_str),
            Some("https://example.com/page")
        );
    }

    #[test]
    fn stage_shared_window_url_none_removes_it() {
        let mut metadata = ShareMetadata::default();
        metadata.generations.insert(7, 42);
        metadata.urls.insert(7, "https://example.com/page".to_string());
        assert!(stage_shared_window_url(&mut metadata, 7, 42, None));
        assert!(!metadata.urls.contains_key(&7));
    }

    use super::*;

    static ENCODER_WARNING_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn software_encoder_observation() -> EncoderObservation {
        EncoderObservation {
            implementation: "OpenH264".to_string(),
            power_efficient: false,
            h264_profile: "64001f".to_string(),
            h264_fmtp: "profile-level-id=64001f".to_string(),
        }
    }

    #[tokio::test]
    async fn post_wake_software_encoder_observation_triggers_one_bounded_republish() {
        let _test_guard = ENCODER_WARNING_TEST_LOCK.lock().await;
        SOFTWARE_ENCODER_WARNED.store(false, Ordering::SeqCst);
        let republish_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_recovery = republish_attempts.clone();
        let recovery = PostWakeEncoderFallbackRecovery::new(std::time::Duration::ZERO, move || {
            Box::pin(async move {
                attempts_for_recovery.fetch_add(1, Ordering::SeqCst);
            })
        });

        log_encoder_once_with(
            "petal-window-769",
            || async { Some(software_encoder_observation()) },
            || async {},
            EncoderPublishOrigin::PostWakeRestart,
            Some(recovery),
        )
        .await;
        assert_eq!(republish_attempts.load(Ordering::SeqCst), 1);
        assert!(SOFTWARE_ENCODER_WARNED.load(Ordering::SeqCst));

        // The replacement publication is deliberately ordinary. Even if it
        // also reports OpenH264, it cannot schedule another recovery and the
        // process-wide warning latch remains claimed rather than duplicating.
        let attempts_for_scope_guard = republish_attempts.clone();
        let forbidden_recovery =
            PostWakeEncoderFallbackRecovery::new(std::time::Duration::ZERO, move || {
                Box::pin(async move {
                    attempts_for_scope_guard.fetch_add(1, Ordering::SeqCst);
                })
            });
        log_encoder_once_with(
            "petal-window-769-retry",
            || async { Some(software_encoder_observation()) },
            || async {},
            EncoderPublishOrigin::Ordinary,
            Some(forbidden_recovery),
        )
        .await;
        assert_eq!(republish_attempts.load(Ordering::SeqCst), 1);
        assert!(
            !claim_software_encoder_warning(),
            "the existing software-encoder warning must remain globally one-shot"
        );
        SOFTWARE_ENCODER_WARNED.store(false, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn ordinary_software_encoder_observation_does_not_trigger_republish() {
        let _test_guard = ENCODER_WARNING_TEST_LOCK.lock().await;
        SOFTWARE_ENCODER_WARNED.store(false, Ordering::SeqCst);
        let republish_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_scope_guard = republish_attempts.clone();
        let forbidden_recovery =
            PostWakeEncoderFallbackRecovery::new(std::time::Duration::ZERO, move || {
                Box::pin(async move {
                    attempts_for_scope_guard.fetch_add(1, Ordering::SeqCst);
                })
            });

        log_encoder_once_with(
            "petal-window-ordinary",
            || async { Some(software_encoder_observation()) },
            || async {},
            EncoderPublishOrigin::Ordinary,
            Some(forbidden_recovery),
        )
        .await;

        assert_eq!(republish_attempts.load(Ordering::SeqCst), 0);
        assert!(SOFTWARE_ENCODER_WARNED.load(Ordering::SeqCst));
        SOFTWARE_ENCODER_WARNED.store(false, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn connect_time_fanout_preserves_event_order_for_both_consumers() {
        let (source_tx, source_rx) = tokio::sync::mpsc::unbounded_channel();
        let (compositor_tx, mut compositor_rx) = tokio::sync::mpsc::unbounded_channel();
        let (resilience_tx, mut resilience_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(fanout_connect_events(
            source_rx,
            compositor_tx,
            resilience_tx,
        ));

        source_tx.send(livekit::RoomEvent::Reconnecting).unwrap();
        source_tx.send(livekit::RoomEvent::Reconnected).unwrap();
        drop(source_tx);

        assert!(matches!(
            compositor_rx.recv().await,
            Some(livekit::RoomEvent::Reconnecting)
        ));
        assert!(matches!(
            compositor_rx.recv().await,
            Some(livekit::RoomEvent::Reconnected)
        ));
        assert!(matches!(
            resilience_rx.recv().await,
            Some(livekit::RoomEvent::Reconnecting)
        ));
        assert!(matches!(
            resilience_rx.recv().await,
            Some(livekit::RoomEvent::Reconnected)
        ));
        task.await
            .expect("fanout exits after its connect receiver closes");
        assert!(compositor_rx.recv().await.is_none());
        assert!(resilience_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn connect_time_fanout_drops_aborted_consumer_without_starving_survivor() {
        let (source_tx, source_rx) = tokio::sync::mpsc::unbounded_channel();
        let (compositor_tx, compositor_rx) = tokio::sync::mpsc::unbounded_channel();
        let (resilience_tx, mut resilience_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(fanout_connect_events(
            source_rx,
            compositor_tx,
            resilience_tx,
        ));

        drop(compositor_rx);
        source_tx.send(livekit::RoomEvent::Reconnecting).unwrap();
        assert!(matches!(
            resilience_rx.recv().await,
            Some(livekit::RoomEvent::Reconnecting)
        ));
        drop(resilience_rx);
        source_tx.send(livekit::RoomEvent::Reconnected).unwrap();
        task.await
            .expect("fanout exits once both consumers are dropped");
    }

    #[tokio::test]
    async fn connect_time_fanout_exits_when_consumers_close_while_source_is_idle() {
        let (_source_tx, source_rx) = tokio::sync::mpsc::unbounded_channel();
        let (compositor_tx, compositor_rx) = tokio::sync::mpsc::unbounded_channel();
        let (resilience_tx, resilience_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(fanout_connect_events(
            source_rx,
            compositor_tx,
            resilience_tx,
        ));

        drop(compositor_rx);
        drop(resilience_rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("idle source must not keep an unconsumed fanout alive")
            .expect("fanout exits cleanly once both consumers close");
    }

    #[test]
    fn native_zero_copy_latch_recovers_after_a_backed_off_reprobe() {
        let mut latch = NativeZeroCopyLatch::new();
        let started = std::time::Instant::now();
        let slow_frame = std::time::Duration::from_millis(51);

        // Three slow frames within the one-second window disable native publish; a
        // single startup hiccup (or even two) must keep using zero-copy.
        assert_eq!(
            latch.record_native_capture(started, slow_frame),
            NativeZeroCopyTransition::None
        );
        assert_eq!(
            latch.record_native_capture(
                started + std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(10),
            ),
            NativeZeroCopyTransition::None
        );
        assert_eq!(
            latch
                .record_native_capture(started + std::time::Duration::from_millis(200), slow_frame),
            NativeZeroCopyTransition::None
        );
        let disabled_at = started + std::time::Duration::from_millis(400);
        assert_eq!(
            latch.record_native_capture(disabled_at, slow_frame),
            NativeZeroCopyTransition::Disabled {
                fallback_for: NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF,
            }
        );
        assert!(!latch.native_attempt_due(
            disabled_at + NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF
                - std::time::Duration::from_millis(1)
        ));

        // Consecutive slow re-probes double the backoff only to its cap, then a
        // fast re-probe restores zero-copy instead of leaving CPU conversion on forever.
        let first_reprobe_at = disabled_at + NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF;
        assert!(latch.native_attempt_due(first_reprobe_at));
        assert_eq!(
            latch.record_native_capture(first_reprobe_at, slow_frame),
            NativeZeroCopyTransition::ReprobeStillSlow {
                fallback_for: NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF.saturating_mul(2),
            }
        );
        let second_reprobe_at =
            first_reprobe_at + NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF.saturating_mul(2);
        assert_eq!(
            latch.record_native_capture(second_reprobe_at, slow_frame),
            NativeZeroCopyTransition::ReprobeStillSlow {
                fallback_for: NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF.saturating_mul(4),
            }
        );
        let third_reprobe_at =
            second_reprobe_at + NATIVE_ZERO_COPY_REPROBE_INITIAL_BACKOFF.saturating_mul(4);
        assert_eq!(
            latch.record_native_capture(third_reprobe_at, slow_frame),
            NativeZeroCopyTransition::ReprobeStillSlow {
                fallback_for: NATIVE_ZERO_COPY_REPROBE_MAX_BACKOFF,
            }
        );
        let recovered_at = third_reprobe_at + NATIVE_ZERO_COPY_REPROBE_MAX_BACKOFF;
        assert!(latch.native_attempt_due(recovered_at));
        assert_eq!(
            latch.record_native_capture(recovered_at, std::time::Duration::from_millis(10)),
            NativeZeroCopyTransition::Reenabled
        );
        assert!(latch.native_attempt_due(recovered_at));
    }

    #[test]
    fn encoder_classifier_accepts_hardware_videotoolbox_strings() {
        assert!(!encoder_looks_software("VideoToolbox", true));
        assert!(!encoder_looks_software(
            "SimulcastEncoderAdapter (VideoToolbox, VideoToolbox)",
            true
        ));
    }

    #[test]
    fn encoder_classifier_flags_missing_or_non_power_efficient_hardware() {
        assert!(!encoder_looks_software("", false));
        assert!(encoder_looks_software("OpenH264", false));
        assert!(encoder_looks_software("libx264", false));
        assert!(encoder_looks_software("libaom", false));
        assert!(encoder_looks_software("VideoToolbox", false));
        assert!(encoder_looks_software("VideoToolbox Software", true));
    }

    #[test]
    fn encoder_classifier_accepts_mf_hardware_encoder() {
        assert!(!encoder_looks_software(
            "SimulcastEncoderAdapter (MF H264 Encoder (hardware))",
            true
        ));
        // A hardware-named encoder that reports non-power-efficient is still
        // treated as suspect (consistent with the VideoToolbox heuristic).
        assert!(encoder_looks_software(
            "SimulcastEncoderAdapter (MF H264 Encoder (hardware))",
            false
        ));
    }

    #[test]
    fn h264_high_preference_keeps_constrained_baseline_fallback() {
        use livekit::webrtc::rtp_parameters::RtpCodecCapability;

        fn h264(profile_level_id: &str) -> RtpCodecCapability {
            RtpCodecCapability {
                channels: None,
                clock_rate: Some(90_000),
                mime_type: "video/H264".to_string(),
                sdp_fmtp_line: Some(format!(
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}"
                )),
            }
        }

        let baseline = h264("42e01f");
        let high = h264("640c1f");
        let vp8 = RtpCodecCapability {
            channels: None,
            clock_rate: Some(90_000),
            mime_type: "video/VP8".to_string(),
            sdp_fmtp_line: None,
        };

        let high_first = livekit::options::preferred_video_codecs(
            vec![baseline.clone(), vp8, high.clone()],
            VideoCodec::H264,
            H264ProfilePreference::HighFirst,
        );
        let high_first_profiles: Vec<_> = high_first
            .iter()
            .map(|codec| {
                h264_profile_level_id_from_fmtp(codec.sdp_fmtp_line.as_deref().unwrap()).unwrap()
            })
            .collect();
        assert_eq!(high_first_profiles, vec!["640c1f", "42e01f"]);

        let baseline_first = livekit::options::preferred_video_codecs(
            vec![high, baseline],
            VideoCodec::H264,
            H264ProfilePreference::ConstrainedBaselineFirst,
        );
        let baseline_first_profiles: Vec<_> = baseline_first
            .iter()
            .map(|codec| {
                h264_profile_level_id_from_fmtp(codec.sdp_fmtp_line.as_deref().unwrap()).unwrap()
            })
            .collect();
        assert_eq!(baseline_first_profiles, vec!["42e01f", "640c1f"]);
    }

    /// Refs #181: two live SDP-negotiation runs (2026-07-27/28, recorded on
    /// the issue) found the publisher's offer correctly leads with H.264
    /// High-family before Constrained Baseline, but the specific
    /// profile-level-id offered -- Constrained High "640c1f" -- never
    /// matches the SFU's compiled-in H.264 codec registry, which was
    /// diagnosed to have plain/unconstrained High "64001f" registered
    /// instead. `preferred_video_codecs` now prefers an unconstrained High
    /// entry over a Constrained High one *when the local encoder reports
    /// both as separate sender capabilities* -- confirmed via a local
    /// `get_rtp_sender_capabilities`/`get_rtp_receiver_capabilities` dump
    /// (2026-08-07) that today's vendored VideoToolbox H.264 build only
    /// ever registers Constrained High, never plain High, on either side.
    /// This test exercises the reorder directly with synthetic capabilities
    /// standing in for a future/different encoder build that offers both --
    /// it does not fabricate a capability `set_codec_preferences` doesn't
    /// actually support, since libwebrtc rejects the ENTIRE codec
    /// preference list if any single entry doesn't match a real
    /// sender/receiver codec (confirmed by reading
    /// `webrtc-sys/src/rtp_transceiver.cpp`'s `set_codec_preferences`,
    /// which throws on any `RTCError` from
    /// `RtpTransceiverInterface::SetCodecPreferences`). Constrained High is
    /// still offered (just second), so a peer that only decodes it is
    /// unaffected.
    #[test]
    fn h264_high_preference_prefers_unconstrained_high_when_both_available() {
        use livekit::webrtc::rtp_parameters::RtpCodecCapability;

        fn h264(profile_level_id: &str) -> RtpCodecCapability {
            RtpCodecCapability {
                channels: None,
                clock_rate: Some(90_000),
                mime_type: "video/H264".to_string(),
                sdp_fmtp_line: Some(format!(
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}"
                )),
            }
        }

        let constrained_high = h264("640c1f");
        let plain_high = h264("64001f");
        let baseline = h264("42e01f");

        let ordered = livekit::options::preferred_video_codecs(
            vec![constrained_high, baseline, plain_high],
            VideoCodec::H264,
            H264ProfilePreference::HighFirst,
        );
        let ordered_profiles: Vec<_> = ordered
            .iter()
            .map(|codec| {
                h264_profile_level_id_from_fmtp(codec.sdp_fmtp_line.as_deref().unwrap()).unwrap()
            })
            .collect();
        assert_eq!(ordered_profiles, vec!["64001f", "640c1f", "42e01f"]);
    }

    #[test]
    fn encoder_stats_profile_readback_uses_codec_fmtp_line() {
        let stats = vec![livekit::webrtc::stats::RtcStats::Codec(
            livekit::webrtc::stats::CodecStats {
                rtc: livekit::webrtc::stats::dictionaries::RtcStats {
                    id: "RTCCodec_96".to_string(),
                    ..Default::default()
                },
                codec: livekit::webrtc::stats::dictionaries::CodecStats {
                    mime_type: "video/H264".to_string(),
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f"
                            .to_string(),
                    ..Default::default()
                },
            },
        )];

        let fmtp = codec_fmtp_for_id(&stats, "RTCCodec_96").expect("codec stats must be found");
        assert_eq!(h264_profile_level_id_from_fmtp(&fmtp), Some("640c1f"));
        assert_eq!(codec_fmtp_for_id(&stats, "missing"), None);
    }

    #[test]
    fn forced_video_codec_parser_is_limited_to_spike_codecs() {
        assert_eq!(
            forced_video_codec_from_value("h264"),
            Some(VideoCodec::H264)
        );
        assert_eq!(
            forced_video_codec_from_value(" AV1 "),
            Some(VideoCodec::AV1)
        );
        assert_eq!(
            forced_video_codec_from_value("hevc"),
            Some(VideoCodec::H265)
        );
        assert_eq!(forced_video_codec_from_value("vp9"), None);
    }

    #[test]
    fn settle_reanchors_after_dwell_at_same_size() {
        let t0 = std::time::Instant::now();
        let dwell = std::time::Duration::from_millis(2000);
        let mut settle = None;
        // First mismatched frame starts the dwell.
        assert!(!PublishedTrack::settle_decides_reanchor(
            &mut settle,
            (100, 80),
            t0,
            dwell
        ));
        // Same size, dwell still pending: first_seen is PRESERVED.
        assert!(!PublishedTrack::settle_decides_reanchor(
            &mut settle,
            (100, 80),
            t0 + std::time::Duration::from_millis(1000),
            dwell
        ));
        // Same size, past the dwell: re-anchor fires and clears the state.
        assert!(PublishedTrack::settle_decides_reanchor(
            &mut settle,
            (100, 80),
            t0 + std::time::Duration::from_millis(2001),
            dwell
        ));
        assert!(settle.is_none());
    }

    #[test]
    fn settle_restarts_when_size_changes() {
        let t0 = std::time::Instant::now();
        let dwell = std::time::Duration::from_millis(2000);
        let mut settle = None;
        let _ = PublishedTrack::settle_decides_reanchor(&mut settle, (100, 80), t0, dwell);
        // Size changes at t0+1500ms: first_seen resets to the new now.
        let t1 = t0 + std::time::Duration::from_millis(1500);
        let _ = PublishedTrack::settle_decides_reanchor(&mut settle, (110, 90), t1, dwell);
        // 1s after the change (well past the original t0+dwell): still pending.
        assert!(!PublishedTrack::settle_decides_reanchor(
            &mut settle,
            (110, 90),
            t1 + std::time::Duration::from_millis(1000),
            dwell
        ));
        // Past the dwell measured from the CHANGE.
        assert!(PublishedTrack::settle_decides_reanchor(
            &mut settle,
            (110, 90),
            t1 + std::time::Duration::from_millis(2001),
            dwell
        ));
    }

    // ---- #866: camera size-mismatch recovery -------------------------------
    //
    // These drive `resolve_camera_push_size` — the whole decision unit that
    // `push_nv12` dispatches to, including its atomic re-anchor store — with a
    // synthetic clock. `PublishedTrack` itself holds an `Arc<Room>` and a
    // `NativeVideoSource` and cannot be built offline, so this is the largest
    // real unit available; keep `push_nv12` a thin dispatcher so it stays so.

    struct RecoveryHarness {
        recovery: Mutex<CameraSizeRecovery>,
        width: std::sync::atomic::AtomicU32,
        height: std::sync::atomic::AtomicU32,
        t0: std::time::Instant,
    }

    impl RecoveryHarness {
        fn new(published: (u32, u32)) -> Self {
            Self {
                recovery: Mutex::new(CameraSizeRecovery::default()),
                width: std::sync::atomic::AtomicU32::new(published.0),
                height: std::sync::atomic::AtomicU32::new(published.1),
                t0: std::time::Instant::now(),
            }
        }

        fn feed(&self, captured: (u32, u32), at_ms: u64) -> CameraPushDecision {
            resolve_camera_push_size(
                &self.recovery,
                &self.width,
                &self.height,
                captured,
                self.t0 + std::time::Duration::from_millis(at_ms),
            )
        }

        /// Feed one camera-cadence burst long enough to exhaust the drop grace,
        /// i.e. what a camera that really came back at `captured` looks like.
        /// Returns the decision for the frame that ended the grace.
        fn burst(&self, captured: (u32, u32), from_ms: u64) -> CameraPushDecision {
            let mut engaged = None;
            for frame in 0..=u64::from(CAMERA_SIZE_MISMATCH_GRACE_FRAMES) + 4 {
                let decision = self.feed(captured, from_ms + frame * 33);
                if decision.verdict == CameraPushVerdict::Push && engaged.is_none() {
                    engaged = Some(decision);
                }
            }
            engaged.expect("the burst never got past the drop grace")
        }

        fn published(&self) -> (u32, u32) {
            (
                self.width.load(Ordering::Relaxed),
                self.height.load(Ordering::Relaxed),
            )
        }
    }

    fn recovery_action_tag(
        decision: &CameraPushDecision,
    ) -> Option<crate::logging::CameraRecoveryActionTag> {
        match decision.diagnostic {
            Some(crate::logging::SentryDiagnosticEvent::CameraSizeMismatchRecovery(value)) => {
                Some(value.action)
            }
            _ => None,
        }
    }

    /// The #866 regression itself. The field log was 2190 CONSECUTIVE drops over
    /// 73s at ~30fps with no recovery, freezing the camera for every peer. Pushing
    /// must resume inside the grace, and stay resumed.
    #[test]
    fn camera_size_mismatch_recovers_and_resumes_pushing() {
        let harness = RecoveryHarness::new((1920, 1080));
        let mut drops = 0u32;
        let mut first_push_frame = None;

        // 300 frames at 33ms — the field failure's cadence, ~10s of camera.
        for frame in 0..300u64 {
            let decision = harness.feed((1280, 720), frame * 33);
            match decision.verdict {
                CameraPushVerdict::Drop => {
                    drops += 1;
                    assert!(
                        first_push_frame.is_none(),
                        "frame {frame} dropped AFTER pushing had already resumed — recovery must not \
                         fall back into dropping"
                    );
                }
                CameraPushVerdict::Push => {
                    if first_push_frame.is_none() {
                        first_push_frame = Some(frame);
                    }
                }
            }
        }

        let resumed_at = first_push_frame.expect("pushing never resumed — this is #866 recurring");
        // The grace ends at whichever limit is reached first. At 33ms/frame the
        // 30-frame limit bites just before the 1s one (frame 30 is at 990ms).
        assert_eq!(
            resumed_at,
            u64::from(CAMERA_SIZE_MISMATCH_GRACE_FRAMES),
            "recovery should resume exactly at the frame grace, not later"
        );
        assert_eq!(
            drops, CAMERA_SIZE_MISMATCH_GRACE_FRAMES,
            "exactly the grace window should drop; anything larger is a partial #866"
        );
        assert_eq!(
            harness.published(),
            (1280, 720),
            "recovery should have re-anchored the published size to the real device size"
        );
    }

    /// One diagnostic per EPISODE, not one per frame — the field log's 2190
    /// identical lines are exactly what made this invisible in triage.
    #[test]
    fn camera_size_mismatch_diagnostic_fires_once_per_episode() {
        let harness = RecoveryHarness::new((1920, 1080));
        let diagnostics = (0..300u64)
            .filter(|frame| harness.feed((1280, 720), frame * 33).diagnostic.is_some())
            .count();
        assert_eq!(
            diagnostics, 1,
            "expected exactly one diagnostic per episode"
        );

        // A run of matching frames ends the episode...
        for frame in 300..320u64 {
            assert!(harness.feed((1280, 720), frame * 33).diagnostic.is_none());
        }
        // ...and a fresh mismatch afterwards is a NEW episode that reports again.
        let second: usize = (400..700u64)
            .filter(|frame| harness.feed((640, 480), frame * 33).diagnostic.is_some())
            .count();
        assert_eq!(second, 1, "a new episode must report exactly once as well");
    }

    /// Bounded recovery (#841's lesson): re-anchoring is rate-limited, and a frame
    /// inside the cooldown letterboxes instead — it is never dropped.
    #[test]
    fn camera_reanchor_is_rate_limited() {
        let harness = RecoveryHarness::new((1920, 1080));
        // First recovery re-anchors at ~990ms.
        let first = harness.burst((1280, 720), 0);
        assert_eq!(
            recovery_action_tag(&first),
            Some(crate::logging::CameraRecoveryActionTag::Reanchor)
        );
        assert_eq!(harness.published(), (1280, 720));

        // A second size change whose recovery lands inside CAMERA_REANCHOR_COOLDOWN
        // letterboxes instead — pushed, never dropped, published size unchanged.
        let inside = harness.burst((640, 480), 1_200);
        assert_eq!(inside.verdict, CameraPushVerdict::Push);
        assert_eq!(
            recovery_action_tag(&inside),
            Some(crate::logging::CameraRecoveryActionTag::Letterbox)
        );
        assert_eq!(
            harness.published(),
            (1280, 720),
            "a recovery inside the cooldown must letterbox, not re-anchor again"
        );

        // Past the cooldown the same episode may re-anchor.
        let after_cooldown = harness.feed(
            (640, 480),
            CAMERA_REANCHOR_COOLDOWN.as_millis() as u64 + 1_000,
        );
        assert_eq!(after_cooldown.verdict, CameraPushVerdict::Push);
        assert_eq!(harness.published(), (640, 480));
    }

    /// A flapping source must not drive an endless re-anchor/encoder-recreation
    /// loop: past the flap budget the recovery settles on letterboxing for good.
    #[test]
    fn camera_size_flapping_settles_on_letterbox() {
        let harness = RecoveryHarness::new((1920, 1080));
        // Flap slower than the cooldown, so the rate limit never masks what the
        // flap guard itself is supposed to catch.
        let step = CAMERA_REANCHOR_COOLDOWN.as_millis() as u64 + 1_100;
        for (index, size) in [(1280, 720), (640, 480), (1280, 720)].iter().enumerate() {
            harness.burst(*size, step * index as u64);
        }
        let locked = harness.published();

        // The budget is spent. Every further frame letterboxes: pushed, never
        // dropped, and the published size stops moving for good.
        let mut at = step * 3;
        for size in [(640, 480), (1280, 720), (320, 240)]
            .iter()
            .cycle()
            .take(60)
        {
            at += 250;
            let decision = harness.feed(*size, at);
            assert_eq!(
                decision.verdict,
                CameraPushVerdict::Push,
                "a flapping source must still keep frames flowing"
            );
            assert_eq!(
                harness.published(),
                locked,
                "the flap guard must stop re-anchoring once the budget is spent"
            );
        }

        // A new episode after the lock reports letterbox, never reanchor.
        for frame in 0..20u64 {
            harness.feed(locked, at + frame * 33);
        }
        let reported = (0..60u64)
            .filter_map(|frame| {
                recovery_action_tag(&harness.feed((320, 240), at + 1_000 + frame * 33))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reported,
            vec![crate::logging::CameraRecoveryActionTag::Letterbox]
        );
    }

    /// A one-off glitch must still drop (that behaviour is correct and cheap);
    /// what must never happen is the drop becoming permanent.
    #[test]
    fn camera_size_match_resets_the_drop_grace() {
        let harness = RecoveryHarness::new((1920, 1080));
        assert_eq!(
            harness.feed((1280, 720), 0).verdict,
            CameraPushVerdict::Drop
        );
        for frame in 1..10u64 {
            assert_eq!(
                harness.feed((1920, 1080), frame * 33).verdict,
                CameraPushVerdict::Push
            );
        }
        // A much later isolated mismatch gets a FRESH grace window, so it drops
        // again rather than inheriting the earlier run's spent budget.
        assert_eq!(
            harness.feed((1280, 720), 60_000).verdict,
            CameraPushVerdict::Drop
        );
        assert_eq!(
            harness.published(),
            (1920, 1080),
            "an isolated glitch must not re-anchor anything"
        );
    }

    #[test]
    fn camera_geometry_bucket_classifies_common_camera_sizes() {
        use crate::logging::GeometryBucket;
        assert_eq!(camera_geometry_bucket(0, 720), GeometryBucket::Unknown);
        assert_eq!(camera_geometry_bucket(160, 120), GeometryBucket::Tiny);
        assert_eq!(camera_geometry_bucket(640, 480), GeometryBucket::Small);
        assert_eq!(camera_geometry_bucket(1280, 720), GeometryBucket::Medium);
        assert_eq!(camera_geometry_bucket(1920, 1080), GeometryBucket::Large);
        assert_eq!(
            camera_geometry_bucket(3840, 2160),
            GeometryBucket::VeryLarge
        );
    }

    // ---- #866 residue: PublishDropStreak end-to-end ------------------------
    //
    // Drives `push_drop_streak_diagnostic` -- the exact seam
    // `PublishedTrack::record_push_outcome` calls -- through a #866-shaped
    // storm (a long run of consecutive failed camera pushes, ~30fps cadence
    // matching the field storm) and asserts the `DropStreakDetector` ->
    // `PublishDropStreak` diagnostic path actually fires. `PublishedTrack`
    // itself cannot be built offline (see the `RecoveryHarness` doc comment
    // above), so this is the largest real unit available -- pinning that the
    // NEXT field storm of this shape is visible in Sentry, which nothing
    // before this test proved end-to-end.

    #[test]
    fn publish_drop_streak_storm_trips_the_diagnostic_for_a_camera_track() {
        let detector = Mutex::new(crate::logging::DropStreakDetector::default());
        let t0 = std::time::Instant::now();
        let mut fired = None;
        let mut fired_at = None;
        for frame in 0..100u64 {
            let now = t0 + std::time::Duration::from_millis(frame * 33);
            if let Some(event) =
                push_drop_streak_diagnostic(&detector, "petal-camera-alice", false, now)
            {
                fired = Some(event);
                fired_at = Some(frame);
                break;
            }
        }
        // Pins that it does NOT fire before the streak/duration threshold is
        // actually crossed -- a mutated `if tripped { return None }` (the
        // inverted guard) still passed this test's predecessor by firing on
        // frame 0, since "not tripped" is the common case. Both the
        // consecutive-frame floor (30) and the 33ms cadence's ~1s duration
        // floor put the real crossing well past a handful of frames.
        assert!(
            fired_at.is_some_and(|frame| frame >= 25),
            "fired too early ({fired_at:?}) -- must not trip before the real streak/duration threshold"
        );
        let event =
            fired.expect("a sustained camera push-drop storm must trip PublishDropStreak (#866)");
        match event {
            crate::logging::SentryDiagnosticEvent::PublishDropStreak(diagnostic) => {
                assert_eq!(diagnostic.scope, crate::logging::StormScopeTag::Camera);
                assert_eq!(diagnostic.role, crate::logging::DiagnosticRole::Sharer);
            }
            other => panic!("expected PublishDropStreak, got {other:?}"),
        }
    }

    #[test]
    fn publish_drop_streak_storm_scopes_a_window_share_track_separately() {
        let detector = Mutex::new(crate::logging::DropStreakDetector::default());
        let t0 = std::time::Instant::now();
        let mut fired = None;
        let mut fired_at = None;
        for frame in 0..100u64 {
            let now = t0 + std::time::Duration::from_millis(frame * 33);
            if let Some(event) =
                push_drop_streak_diagnostic(&detector, "petal-window-1234", false, now)
            {
                fired = Some(event);
                fired_at = Some(frame);
                break;
            }
        }
        assert!(
            fired_at.is_some_and(|frame| frame >= 25),
            "fired too early ({fired_at:?}) -- must not trip before the real streak/duration threshold"
        );
        match fired.expect("a sustained window-share drop storm must also trip the diagnostic") {
            crate::logging::SentryDiagnosticEvent::PublishDropStreak(diagnostic) => {
                assert_eq!(diagnostic.scope, crate::logging::StormScopeTag::WindowShare);
            }
            other => panic!("expected PublishDropStreak, got {other:?}"),
        }
    }

    /// A brief hiccup (occasional successful pushes interleaved) must never
    /// trip the streak -- only a SUSTAINED run of failures should. This is
    /// what distinguishes #866's real storm from ordinary backpressure.
    #[test]
    fn publish_drop_streak_does_not_trip_on_an_intermittent_hiccup() {
        let detector = Mutex::new(crate::logging::DropStreakDetector::default());
        let t0 = std::time::Instant::now();
        for frame in 0..200u64 {
            let now = t0 + std::time::Duration::from_millis(frame * 33);
            // Every 5th push succeeds, resetting the streak before it can
            // reach the consecutive-frame threshold.
            let published = frame % 5 == 0;
            let fired = push_drop_streak_diagnostic(&detector, "petal-camera-alice", published, now);
            assert!(
                fired.is_none(),
                "an intermittent hiccup with regular successful pushes must never trip the storm detector"
            );
        }
    }

    // #714: `letterbox_scale_i420` was previously exercised only through
    // `push_bgra` (Windows, fcbfa4f4) with no direct test of its own. It is
    // now ALSO the mechanism `push_i420_letterboxed` uses for the NV12 path
    // (macOS window shares + camera fallback), so a correctness bug here
    // would now affect both platforms. Solid-fill the source with a known
    // non-border color and assert the padding stays exactly black
    // (Y=16, U=V=128 -- the values `letterbox_scale_i420`'s own doc comment
    // specifies) while the scaled content lands centered, not just "some
    // non-black pixels exist somewhere."
    #[test]
    fn letterbox_scale_i420_pads_with_black_and_centers_content() {
        // Source is wider-than-tall relative to the destination, so the
        // scaled content is letterboxed with black bars top and bottom.
        let src_w = 100u32;
        let src_h = 50u32;
        let dst_w = 100u32;
        let dst_h = 100u32;

        let mut src = I420Buffer::new(src_w, src_h);
        {
            let (y, u, v) = src.data_mut();
            y.fill(200); // distinct from the Y=16 black-fill constant
            u.fill(90);
            v.fill(180);
        }
        let mut dst = I420Buffer::new(dst_w, dst_h);
        PublishedTrack::letterbox_scale_i420(&src, src_w, src_h, &mut dst, dst_w, dst_h);

        let (dst_sy, dst_su, dst_sv) = dst.strides();
        let (dst_y, dst_u, dst_v) = dst.data();

        // fit = min(100/100, 100/50) = 1.0 -> scaled content is 100x50,
        // centered vertically: rows [0,25) and [75,100) are pure black bars.
        let top_bar_row = 5usize;
        assert_eq!(
            dst_y[top_bar_row * dst_sy as usize],
            16,
            "top bar must be black (Y=16)"
        );
        let bottom_bar_row = 95usize;
        assert_eq!(
            dst_y[bottom_bar_row * dst_sy as usize],
            16,
            "bottom bar must be black (Y=16)"
        );
        let bottom_bar_chroma_row = bottom_bar_row / 2;
        assert_eq!(
            dst_u[bottom_bar_chroma_row * dst_su as usize],
            128,
            "bottom bar chroma must be neutral (U=128)"
        );
        assert_eq!(
            dst_v[bottom_bar_chroma_row * dst_sv as usize],
            128,
            "bottom bar chroma must be neutral (V=128)"
        );

        // The center row (inside the scaled 100x50 region, y=50) must carry
        // the source's fill value through unchanged, not the black padding.
        let center_row = 50usize;
        assert_eq!(
            dst_y[center_row * dst_sy as usize],
            200,
            "scaled content must be centered and preserve source luma"
        );
    }
}
