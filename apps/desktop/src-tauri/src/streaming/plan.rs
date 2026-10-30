//! Output rules for a live stream: the canvas, frame rate and bitrate a share
//! streams at.
//!
//! The canvas is always a standard 16:9 size (9:16 in portrait), fixed for
//! the whole stream: platforms play 16:9, and size changes mid-stream are
//! what they handle worst. The share is scaled to fit inside it, letterboxed.
//!
//! Pinned to the vectors in contracts/petal-contracts.json
//! (`streamOutputPlans`, `streamFpsCeilings`). Integer arithmetic only, so a
//! future mirror agrees to the pixel and the bit.

use serde::{Deserialize, Serialize};

/// The stream's resolution: a standard size, or Auto (picked from the share).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamResolution {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "4k")]
    K4,
}

impl StreamResolution {
    /// The landscape canvas, (width, height). Auto resolves first.
    pub const fn landscape(self) -> (u32, u32) {
        match self {
            Self::P720 => (1280, 720),
            Self::Auto | Self::P1080 => (1920, 1080),
            Self::P1440 => (2560, 1440),
            Self::K4 => (3840, 2160),
        }
    }

    /// "Auto" max bitrate at this resolution.
    pub const fn auto_max_bitrate_bps(self) -> u32 {
        match self {
            Self::Auto | Self::P720 | Self::P1080 => 6_000_000,
            Self::P1440 => 12_000_000,
            Self::K4 => 25_000_000,
        }
    }
}

/// The canvas orientation; Auto follows the share's shape at go-live.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamOrientation {
    #[default]
    Auto,
    Landscape,
    Portrait,
}

pub const STREAM_MIN_BITRATE_BPS: u32 = 3_000_000;
pub const STREAM_BITRATE_OVERRIDE_MIN_BPS: u32 = 1_000_000;
pub const STREAM_BITRATE_OVERRIDE_MAX_BPS: u32 = 50_000_000;
/// Auto never picks above this: 1440p and 4K are deliberate choices.
pub const STREAM_AUTO_MAX_RESOLUTION: StreamResolution = StreamResolution::P1080;
pub const STREAM_KEYFRAME_INTERVAL_S: u32 = 2;
pub const STREAM_AUDIO_BITRATE_BPS: u32 = 128_000;
pub const STREAM_AUDIO_SAMPLE_RATE: u32 = 48_000;
/// H.264 Level 5.2 MaxMBPS -- the limit `transport::publisher` applies to shares.
pub const STREAM_MAX_MACROBLOCKS_PER_SECOND: u64 = 2_073_600;
/// Cadences the level cap may land on, coarsest first.
pub const STREAM_FPS_CADENCES: [u32; 3] = [15, 30, 60];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamPlanInput {
    /// The share's capture size at go-live.
    pub capture_width: u32,
    pub capture_height: u32,
    /// The share's effective cadence (15/30/60 on Windows, 30 on macOS).
    pub share_fps: u32,
    pub resolution: StreamResolution,
    #[serde(default)]
    pub orientation: StreamOrientation,
    /// Explicit override; `None` means "Auto" for the resolution.
    #[serde(default)]
    pub max_bitrate_bps: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamPlan {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    /// The resolution in effect (never Auto).
    pub resolution: StreamResolution,
}

/// Whether the canvas is portrait: chosen, or (Auto) a share taller than wide.
pub fn is_portrait(width: u32, height: u32, orientation: StreamOrientation) -> bool {
    match orientation {
        StreamOrientation::Auto => height > width,
        StreamOrientation::Landscape => false,
        StreamOrientation::Portrait => true,
    }
}

/// The canvas for `resolution` in the given orientation.
pub fn canvas_size(resolution: StreamResolution, portrait: bool) -> (u32, u32) {
    let (w, h) = resolution.landscape();
    if portrait {
        (h, w)
    } else {
        (w, h)
    }
}

/// Auto: the smallest standard size up to [`STREAM_AUTO_MAX_RESOLUTION`] that
/// holds the share at 90% of its size or more (a little shrink is invisible;
/// more costs legibility), else the largest.
pub fn auto_resolution(width: u32, height: u32, portrait: bool) -> StreamResolution {
    [StreamResolution::P720, StreamResolution::P1080]
        .into_iter()
        .find(|resolution| {
            let (cw, ch) = canvas_size(*resolution, portrait);
            10 * u64::from(cw) >= 9 * u64::from(width) && 10 * u64::from(ch) >= 9 * u64::from(height)
        })
        .unwrap_or(STREAM_AUTO_MAX_RESOLUTION)
}

/// The resolution the stream runs at, Auto resolved.
pub fn effective_resolution(input: &StreamPlanInput) -> StreamResolution {
    let portrait = is_portrait(input.capture_width, input.capture_height, input.orientation);
    match input.resolution {
        StreamResolution::Auto => auto_resolution(input.capture_width, input.capture_height, portrait),
        resolution => resolution,
    }
}

/// The highest Level 5.2 cadence an output of this geometry may run at.
pub fn stream_fps_ceiling_for_geometry(width: u32, height: u32) -> u32 {
    let macroblocks = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
    STREAM_FPS_CADENCES
        .iter()
        .rev()
        .find(|fps| macroblocks * u64::from(**fps) <= STREAM_MAX_MACROBLOCKS_PER_SECOND)
        .copied()
        .unwrap_or(STREAM_FPS_CADENCES[0])
}

/// `clamp(0.08 x w x h x fps, 3 Mbps, max)`; the max wins when below the floor.
pub fn stream_bitrate_bps(width: u32, height: u32, fps: u32, max_bitrate_bps: u32) -> u32 {
    // 0.08 = 2/25.
    let ask = u64::from(width) * u64::from(height) * u64::from(fps) * 2 / 25;
    let ask = u32::try_from(ask).unwrap_or(u32::MAX);
    ask.max(STREAM_MIN_BITRATE_BPS).min(max_bitrate_bps)
}

pub fn effective_max_bitrate_bps(input: &StreamPlanInput) -> u32 {
    input.max_bitrate_bps.map_or(effective_resolution(input).auto_max_bitrate_bps(), |bps| {
        bps.clamp(STREAM_BITRATE_OVERRIDE_MIN_BPS, STREAM_BITRATE_OVERRIDE_MAX_BPS)
    })
}

pub fn plan_stream_output(input: &StreamPlanInput) -> StreamPlan {
    let resolution = effective_resolution(input);
    let portrait = is_portrait(input.capture_width, input.capture_height, input.orientation);
    let (width, height) = canvas_size(resolution, portrait);
    let fps = input.share_fps.max(1).min(stream_fps_ceiling_for_geometry(width, height));
    let max_bitrate_bps = effective_max_bitrate_bps(input);
    StreamPlan {
        width,
        height,
        fps,
        bitrate_bps: stream_bitrate_bps(width, height, fps, max_bitrate_bps),
        max_bitrate_bps,
        resolution,
    }
}

/// Where a `src_w`x`src_h` frame lands inside the fixed canvas -- scaled
/// (up or down) to fit and centred, black bars around it. `(x, y, w, h)`.
pub fn letterbox_rect(src_w: u32, src_h: u32, canvas_w: u32, canvas_h: u32) -> (u32, u32, u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0, 0, 0);
    }
    let scale = (f64::from(canvas_w) / f64::from(src_w)).min(f64::from(canvas_h) / f64::from(src_h));
    let w = (f64::from(src_w) * scale).round() as u32;
    let h = (f64::from(src_h) * scale).round() as u32;
    (canvas_w.saturating_sub(w) / 2, canvas_h.saturating_sub(h) / 2, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PlanVector {
        name: String,
        input: StreamPlanInput,
        output: StreamPlan,
    }

    #[derive(Deserialize)]
    struct FpsCeilingVector {
        width: u32,
        height: u32,
        fps: u32,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Contracts {
        stream_output_plans: Vec<PlanVector>,
        stream_fps_ceilings: Vec<FpsCeilingVector>,
    }

    fn contracts() -> Contracts {
        serde_json::from_str(include_str!("../../../../../contracts/petal-contracts.json")).unwrap()
    }

    #[test]
    fn matches_every_contract_plan_vector() {
        let vectors = contracts().stream_output_plans;
        assert!(vectors.len() >= 5);
        for vector in vectors {
            assert_eq!(plan_stream_output(&vector.input), vector.output, "{}", vector.name);
        }
    }

    #[test]
    fn matches_the_contract_level_5_2_fps_ceilings() {
        for v in contracts().stream_fps_ceilings {
            assert_eq!(stream_fps_ceiling_for_geometry(v.width, v.height), v.fps, "{}x{}", v.width, v.height);
        }
    }

    fn input(width: u32, height: u32, resolution: StreamResolution) -> StreamPlanInput {
        StreamPlanInput {
            capture_width: width,
            capture_height: height,
            share_fps: 30,
            resolution,
            orientation: StreamOrientation::Auto,
            max_bitrate_bps: None,
        }
    }

    #[test]
    fn every_canvas_is_a_standard_16_by_9_size() {
        use StreamResolution::*;
        for resolution in [Auto, P720, P1080, P1440, K4] {
            for w in (1..6000).step_by(397) {
                for h in (1..6000).step_by(409) {
                    let plan = plan_stream_output(&input(w, h, resolution));
                    let (long, short) = (plan.width.max(plan.height), plan.width.min(plan.height));
                    assert_eq!(long * 9, short * 16, "{w}x{h} -> {}x{}", plan.width, plan.height);
                    assert_eq!((long, short), plan.resolution.landscape());
                    assert_eq!(plan.height > plan.width, h > w, "Auto orientation follows {w}x{h}");
                    if resolution == Auto {
                        assert!(plan.resolution.landscape().0 <= STREAM_AUTO_MAX_RESOLUTION.landscape().0);
                    }
                }
            }
        }
    }

    #[test]
    fn auto_picks_the_smallest_size_that_barely_shrinks_the_share() {
        use StreamResolution::*;
        assert_eq!(auto_resolution(1280, 720, false), P720);
        assert_eq!(auto_resolution(1422, 800, false), P720, "90%: still 720p");
        assert_eq!(auto_resolution(1424, 800, false), P1080, "just under 90%: 1080p");
        assert_eq!(auto_resolution(1282, 825, false), P1080);
        assert_eq!(auto_resolution(720, 1280, true), P720);
        assert_eq!(auto_resolution(5120, 2880, false), P1080, "Auto stops at 1080p");
    }

    #[test]
    fn bitrate_clamps() {
        assert_eq!(stream_bitrate_bps(1280, 720, 15, 6_000_000), STREAM_MIN_BITRATE_BPS);
        assert_eq!(stream_bitrate_bps(3840, 2160, 60, 25_000_000), 25_000_000);
        assert_eq!(stream_bitrate_bps(1920, 1080, 60, 1_000_000), 1_000_000, "the max wins below the floor");
    }

    #[test]
    fn letterbox_centres_inside_the_canvas() {
        assert_eq!(letterbox_rect(1920, 1080, 1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(letterbox_rect(1080, 1080, 1920, 1080), (420, 0, 1080, 1080));
        assert_eq!(letterbox_rect(3840, 1080, 1920, 1080), (0, 270, 1920, 540));
        assert_eq!(letterbox_rect(1280, 720, 1920, 1080), (0, 0, 1920, 1080), "small shares scale up");
        assert_eq!(letterbox_rect(900, 1600, 1920, 1080), (656, 0, 608, 1080), "portrait: pillarboxed");
        assert_eq!(letterbox_rect(0, 10, 1920, 1080), (0, 0, 0, 0));
    }
}
