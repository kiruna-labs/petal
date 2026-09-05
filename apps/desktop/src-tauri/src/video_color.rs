//! Shared video color-profile definitions for the screenshare pipeline.
//!
//! This is intentionally pure Rust metadata/math. Capture-side display
//! probing and H.264 VUI emission are separate #47 work because they touch
//! ScreenCaptureKit and encoder/libwebrtc ownership boundaries.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorPrimaries {
    Bt709,
    Bt601Pal,
    Bt601Ntsc,
    DisplayP3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferFunction {
    Bt709,
    Srgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatrixCoefficients {
    Bt601,
    Bt709,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PixelRange {
    Full,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoColorProfile {
    pub primaries: ColorPrimaries,
    pub transfer: TransferFunction,
    pub matrix: MatrixCoefficients,
    pub range: PixelRange,
}

impl VideoColorProfile {
    /// Current legacy behavior made explicit: libyuv `ARGBToI420`, which is
    /// BT.601 studio/video range for Apple-BGRA input.
    pub const BT601_VIDEO: Self = Self {
        primaries: ColorPrimaries::Bt601Ntsc,
        transfer: TransferFunction::Bt709,
        matrix: MatrixCoefficients::Bt601,
        range: PixelRange::Video,
    };

    /// The intended normalized screenshare profile once capture and encoder
    /// tagging are wired: sRGB primaries/transfer with BT.709 luma.
    pub const SRGB_BT709_FULL: Self = Self {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Srgb,
        matrix: MatrixCoefficients::Bt709,
        range: PixelRange::Full,
    };

    /// Display P3 uses P3 RGB primaries with the same BT.709-style luma
    /// coefficients for YCbCr conversion in this pipeline.
    pub const DISPLAY_P3_BT709_FULL: Self = Self {
        primaries: ColorPrimaries::DisplayP3,
        transfer: TransferFunction::Srgb,
        matrix: MatrixCoefficients::Bt709,
        range: PixelRange::Full,
    };

    pub const fn legacy_publish_default() -> Self {
        Self::BT601_VIDEO
    }

    pub const fn capture_color_space_name(self) -> &'static str {
        match self.primaries {
            ColorPrimaries::DisplayP3 => "kCGColorSpaceDisplayP3",
            _ => "kCGColorSpaceSRGB",
        }
    }
}

pub fn profile_for_cg_color_space_name(name: &str) -> Option<VideoColorProfile> {
    let normalized: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();

    if normalized.contains("displayp3") || normalized.contains("p3d65") {
        Some(VideoColorProfile::DISPLAY_P3_BT709_FULL)
    } else if normalized.contains("srgb") || normalized.contains("genericrgb") {
        Some(VideoColorProfile::SRGB_BT709_FULL)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YCbCr8 {
    pub y: u8,
    pub cb: u8,
    pub cr: u8,
}

pub fn apple_bgra_to_rgb8(pixel: [u8; 4]) -> Rgb8 {
    Rgb8 {
        b: pixel[0],
        g: pixel[1],
        r: pixel[2],
    }
}

/// Integer fixed-point (Q16) coefficients for the BT.601/BT.709 YCbCr
/// matrices at Video/Full range, derived from the exact f64 formula in
/// [`rgb_to_ycbcr_8bit`]'s original implementation:
///
/// - `yq = (kr*R + kg*G + kb*B) * 65536` (the luma matrix applied to the
///   0-255 bytes, i.e. `65536 * 255 * y'`).
/// - Full: `Y = round(yq / 65536)`, `C = 128 + round(chroma_num / den)` with
///   `chroma_num = (B|R)*65536 - yq` and `den = 2*(1-kb|kr)*65536`.
/// - Video: `Y = 16 + round(219*yq / (255*65536))`,
///   `C = 128 + round(224*chroma_num / (255*65536*2*(1-kb|kr)))`.
///
/// All denominators are compile-time constants, so LLVM lowers every
/// division to a multiply-shift. The previous implementation did per-pixel
/// f64 divisions (and f64 conversions for every channel), which capped the
/// Windows BGRA→I420 path at ~12-15 Mpx/s — exactly the sender's observed
/// fps ceiling (23.7ms/frame @502x735, 62.8ms @1030x921, 100.8ms @1630x921;
/// A11/B11 and the 2026-08-06 pump logs).
#[derive(Debug, Clone, Copy)]
struct YcbcrCoeffs {
    kr_q: i32,
    kg_q: i32,
    kb_q: i32,
    y_num: i32,
    y_den: i32,
    y_off: i32,
    c_num: i32,
    cb_den: i32,
    cr_den: i32,
}

const BT601_VIDEO: YcbcrCoeffs = YcbcrCoeffs {
    kr_q: 19595,
    kg_q: 38470,
    kb_q: 7471,
    y_num: 219,
    y_den: 16_711_680, // 255 * 65536
    y_off: 16,
    c_num: 224,
    cb_den: 29_613_097, // round(255*65536*2*(1-0.1140))
    cr_den: 23_429_775, // round(255*65536*2*(1-0.2990))
};
const BT601_FULL: YcbcrCoeffs = YcbcrCoeffs {
    kr_q: 19595,
    kg_q: 38470,
    kb_q: 7471,
    y_num: 1,
    y_den: 65_536,
    y_off: 0,
    c_num: 1,
    cb_den: 116_130, // round(65536*2*(1-0.1140))
    cr_den: 91_881,  // round(65536*2*(1-0.2990))
};
const BT709_VIDEO: YcbcrCoeffs = YcbcrCoeffs {
    kr_q: 13933,
    kg_q: 46871,
    kb_q: 4732,
    y_num: 219,
    y_den: 16_711_680,
    y_off: 16,
    c_num: 224,
    cb_den: 31_014_993, // round(255*65536*2*(1-0.0722))
    cr_den: 26_319_346, // round(255*65536*2*(1-0.2126))
};
const BT709_FULL: YcbcrCoeffs = YcbcrCoeffs {
    kr_q: 13933,
    kg_q: 46871,
    kb_q: 4732,
    y_num: 1,
    y_den: 65_536,
    y_off: 0,
    c_num: 1,
    cb_den: 121_610, // round(65536*2*(1-0.0722))
    cr_den: 103_210, // round(65536*2*(1-0.2126))
};

fn ycbcr_coeffs(profile: VideoColorProfile) -> YcbcrCoeffs {
    match (profile.matrix, profile.range) {
        (MatrixCoefficients::Bt601, PixelRange::Video) => BT601_VIDEO,
        (MatrixCoefficients::Bt601, PixelRange::Full) => BT601_FULL,
        (MatrixCoefficients::Bt709, PixelRange::Video) => BT709_VIDEO,
        (MatrixCoefficients::Bt709, PixelRange::Full) => BT709_FULL,
    }
}

/// `round(num / den)` with round-half-away-from-zero, matching `f64::round()`
/// on positive values and the f64 reference's behavior on negatives.
#[inline(always)]
fn round_div(num: i64, den: i64) -> i64 {
    if num >= 0 {
        (num + den / 2) / den
    } else {
        (num - den / 2) / den
    }
}

#[inline(always)]
fn clamp_byte(v: i64) -> u8 {
    v.clamp(0, 255) as u8
}

pub fn rgb_to_ycbcr_8bit(rgb: Rgb8, profile: VideoColorProfile) -> YCbCr8 {
    let c = ycbcr_coeffs(profile);
    let r = i64::from(rgb.r);
    let g = i64::from(rgb.g);
    let b = i64::from(rgb.b);
    // yq = 65536 * 255 * y' (the luma matrix applied to the 0-255 bytes).
    let yq = i64::from(c.kr_q) * r + i64::from(c.kg_q) * g + i64::from(c.kb_q) * b;
    YCbCr8 {
        y: clamp_byte(i64::from(c.y_off) + round_div(i64::from(c.y_num) * yq, i64::from(c.y_den))),
        cb: clamp_byte(128 + round_div(i64::from(c.c_num) * ((b << 16) - yq), i64::from(c.cb_den))),
        cr: clamp_byte(128 + round_div(i64::from(c.c_num) * ((r << 16) - yq), i64::from(c.cr_den))),
    }
}

/// Integer fixed-point (Q16) coefficients for the inverse YCbCr→RGB
/// conversion, the exact matrix/range partner of the forward [`YcbcrCoeffs`].
///
/// Derivation (same BT.601/BT.709 `(kr, kb)` table as the forward path):
///
/// - `y'  = (Y - y_off) / y_den` with `y_den = 219` (video) or `255` (full).
/// - `cb' = (Cb - 128) / c_den`, `cr' = (Cr - 128) / c_den` with
///   `c_den = 224` (video) or `255` (full).
/// - `R = 255 * (y' + 2*(1-kr) * cr')`, `B = 255 * (y' + 2*(1-kb) * cb')`,
///   `G = 255 * (y' - kr*R/255 - kb*B/255) / kg`.
///
/// The `y_scale`/`cr_scale`/`cb_scale` Q16 constants fold the `255 * …/den`
/// scales; `g` is recovered as `G = (ys*65536 - kr_q*rs - kb_q*bs) /
/// (kg_q*65536)` where `ys`/`rs`/`bs` are the pre-round Q16 sums, reusing the
/// forward path's `kr_q`/`kg_q`/`kb_q` primaries so the two directions can
/// never drift apart.
#[derive(Debug, Clone, Copy)]
struct YcbcrInverseCoeffs {
    kr_q: i64,
    kg_q: i64,
    kb_q: i64,
    y_scale: i64,
    cr_scale: i64,
    cb_scale: i64,
    y_off: i64,
}

const INV_BT601_VIDEO: YcbcrInverseCoeffs = YcbcrInverseCoeffs {
    kr_q: 19595,
    kg_q: 38470,
    kb_q: 7471,
    y_scale: 76_309,   // round(65536 * 255 / 219)
    cr_scale: 104_597, // round(65536 * 255 * 2*(1-0.2990) / 224)
    cb_scale: 132_201, // round(65536 * 255 * 2*(1-0.1140) / 224)
    y_off: 16,
};
const INV_BT601_FULL: YcbcrInverseCoeffs = YcbcrInverseCoeffs {
    kr_q: 19595,
    kg_q: 38470,
    kb_q: 7471,
    y_scale: 65_536,   // round(65536 * 255 / 255)
    cr_scale: 91_881,  // round(65536 * 255 * 2*(1-0.2990) / 255)
    cb_scale: 116_130, // round(65536 * 255 * 2*(1-0.1140) / 255)
    y_off: 0,
};
const INV_BT709_VIDEO: YcbcrInverseCoeffs = YcbcrInverseCoeffs {
    kr_q: 13933,
    kg_q: 46871,
    kb_q: 4732,
    y_scale: 76_309,   // round(65536 * 255 / 219)
    cr_scale: 117_489, // round(65536 * 255 * 2*(1-0.2126) / 224)
    cb_scale: 138_438, // round(65536 * 255 * 2*(1-0.0722) / 224)
    y_off: 16,
};
const INV_BT709_FULL: YcbcrInverseCoeffs = YcbcrInverseCoeffs {
    kr_q: 13933,
    kg_q: 46871,
    kb_q: 4732,
    y_scale: 65_536,   // round(65536 * 255 / 255)
    cr_scale: 103_206, // round(65536 * 255 * 2*(1-0.2126) / 255)
    cb_scale: 121_609, // round(65536 * 255 * 2*(1-0.0722) / 255)
    y_off: 0,
};

fn ycbcr_inverse_coeffs(profile: VideoColorProfile) -> YcbcrInverseCoeffs {
    match (profile.matrix, profile.range) {
        (MatrixCoefficients::Bt601, PixelRange::Video) => INV_BT601_VIDEO,
        (MatrixCoefficients::Bt601, PixelRange::Full) => INV_BT601_FULL,
        (MatrixCoefficients::Bt709, PixelRange::Video) => INV_BT709_VIDEO,
        (MatrixCoefficients::Bt709, PixelRange::Full) => INV_BT709_FULL,
    }
}

/// Exact inverse of [`rgb_to_ycbcr_8bit`]: decode YCbCr back into sRGB-encoded
/// 8-bit RGB using the same (kr, kb) matrix table and range scaling.
///
/// Uses integer fixed-point (Q16) arithmetic only — no per-pixel floating
/// point — mirroring commit `d42647fb`'s forward optimization in the receiver
/// direction. Used by the receiver-side I420→BGRA converter (Windows
/// compositor). A test-only floating-point reference pins parity.
pub fn ycbcr_to_rgb_8bit(ycbcr: YCbCr8, profile: VideoColorProfile) -> Rgb8 {
    let c = ycbcr_inverse_coeffs(profile);
    let y = i64::from(ycbcr.y);
    let cb = i64::from(ycbcr.cb);
    let cr = i64::from(ycbcr.cr);
    // Q16 intermediate sums, kept before the final divide so the green
    // channel reuses them (no re-deriving r/b from rounded bytes).
    let ys = (y - c.y_off) * c.y_scale;
    let rs = ys + (cr - 128) * c.cr_scale;
    let bs = ys + (cb - 128) * c.cb_scale;
    let r = round_div(rs, 65_536);
    let b = round_div(bs, 65_536);
    let g_num = ys * 65_536 - c.kr_q * rs - c.kb_q * bs;
    let g = round_div(g_num, c.kg_q * 65_536);
    Rgb8 {
        r: clamp_byte(r),
        g: clamp_byte(g),
        b: clamp_byte(b),
    }
}

/// Pure-Rust I420 → BGRA conversion for the receiver-side Windows compositor.
///
/// Chroma is upsampled by 2×2 replication with edge clamping (for odd widths
/// and heights the last chroma sample covers the edge pixels by construction:
/// `x / 2` stays below `ceil(width / 2)`). Output is tightly packed
/// 4 bytes/pixel BGRA with `bytes_per_row == width * 4`.
///
/// Returns `None` when the plane extents do not back the declared geometry
/// (short planes or undersized strides).
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_i420_to_bgra(
    y: &[u8],
    y_stride: usize,
    u: &[u8],
    u_stride: usize,
    v: &[u8],
    v_stride: usize,
    width: u32,
    height: u32,
    profile: VideoColorProfile,
) -> Option<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    if y_stride < width
        || u_stride < chroma_width
        || v_stride < chroma_width
        || y.len() < y_stride.saturating_mul(height)
        || u.len() < u_stride.saturating_mul(chroma_height)
        || v.len() < v_stride.saturating_mul(chroma_height)
    {
        return None;
    }

    let bytes_per_row = width.checked_mul(4)?;
    let mut bgra = vec![0u8; bytes_per_row.checked_mul(height)?];

    for py in 0..height {
        let chroma_y = py / 2;
        let y_row = py * y_stride;
        let u_row = chroma_y * u_stride;
        let v_row = chroma_y * v_stride;
        let out_row = py * bytes_per_row;
        for px in 0..width {
            let chroma_x = px / 2;
            let rgb = ycbcr_to_rgb_8bit(
                YCbCr8 {
                    y: y[y_row + px],
                    cb: u[u_row + chroma_x],
                    cr: v[v_row + chroma_x],
                },
                profile,
            );
            let out = out_row + (px * 4);
            bgra[out] = rgb.b;
            bgra[out + 1] = rgb.g;
            bgra[out + 2] = rgb.r;
            bgra[out + 3] = 0xff;
        }
    }

    Some(bgra)
}

/// Test-only floating-point reference for [`ycbcr_to_rgb_8bit`] — kept
/// intentionally separate from the production fixed-point arithmetic so the
/// parity tests are not tautological. Never called from production code.
#[cfg(test)]
fn clamp_round(v: f64) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
fn ycbcr_to_rgb_8bit_reference(ycbcr: YCbCr8, profile: VideoColorProfile) -> Rgb8 {
    let (kr, kb) = match profile.matrix {
        MatrixCoefficients::Bt601 => (0.2990, 0.1140),
        MatrixCoefficients::Bt709 => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let (y, cb, cr) = match profile.range {
        PixelRange::Video => (
            (f64::from(ycbcr.y) - 16.0) / 219.0,
            (f64::from(ycbcr.cb) - 128.0) / 224.0,
            (f64::from(ycbcr.cr) - 128.0) / 224.0,
        ),
        PixelRange::Full => (
            f64::from(ycbcr.y) / 255.0,
            (f64::from(ycbcr.cb) - 128.0) / 255.0,
            (f64::from(ycbcr.cr) - 128.0) / 255.0,
        ),
    };
    let r = y + (2.0 * (1.0 - kr) * cr);
    let b = y + (2.0 * (1.0 - kb) * cb);
    let g = (y - (kr * r) - (kb * b)) / kg;
    Rgb8 {
        r: clamp_round(255.0 * r),
        g: clamp_round(255.0 * g),
        b: clamp_round(255.0 * b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt709_and_bt601_vectors_are_distinct_for_primary_colors() {
        let red = Rgb8 { r: 255, g: 0, b: 0 };
        assert_eq!(
            rgb_to_ycbcr_8bit(red, VideoColorProfile::BT601_VIDEO),
            YCbCr8 {
                y: 81,
                cb: 90,
                cr: 240
            }
        );
        assert_eq!(
            rgb_to_ycbcr_8bit(
                red,
                VideoColorProfile {
                    primaries: ColorPrimaries::Bt709,
                    transfer: TransferFunction::Bt709,
                    matrix: MatrixCoefficients::Bt709,
                    range: PixelRange::Video,
                }
            ),
            YCbCr8 {
                y: 63,
                cb: 102,
                cr: 240
            }
        );
    }

    #[test]
    fn apple_bgra_vector_uses_apple_byte_order() {
        let red = apple_bgra_to_rgb8([0, 0, 255, 255]);
        assert_eq!(red, Rgb8 { r: 255, g: 0, b: 0 });
        assert_eq!(
            rgb_to_ycbcr_8bit(red, VideoColorProfile::BT601_VIDEO),
            YCbCr8 {
                y: 81,
                cb: 90,
                cr: 240
            }
        );
    }

    #[test]
    fn full_range_srgb_keeps_black_and_white_at_full_swing() {
        assert_eq!(
            rgb_to_ycbcr_8bit(
                Rgb8 { r: 0, g: 0, b: 0 },
                VideoColorProfile::SRGB_BT709_FULL
            ),
            YCbCr8 {
                y: 0,
                cb: 128,
                cr: 128
            }
        );
        assert_eq!(
            rgb_to_ycbcr_8bit(
                Rgb8 {
                    r: 255,
                    g: 255,
                    b: 255
                },
                VideoColorProfile::SRGB_BT709_FULL
            ),
            YCbCr8 {
                y: 255,
                cb: 128,
                cr: 128
            }
        );
    }

    #[test]
    fn cg_color_space_names_map_to_capture_profiles() {
        assert_eq!(
            profile_for_cg_color_space_name("kCGColorSpaceDisplayP3"),
            Some(VideoColorProfile::DISPLAY_P3_BT709_FULL)
        );
        assert_eq!(
            profile_for_cg_color_space_name("Display P3"),
            Some(VideoColorProfile::DISPLAY_P3_BT709_FULL)
        );
        assert_eq!(
            profile_for_cg_color_space_name("kCGColorSpaceSRGB"),
            Some(VideoColorProfile::SRGB_BT709_FULL)
        );
        assert_eq!(
            profile_for_cg_color_space_name("Generic RGB Profile"),
            Some(VideoColorProfile::SRGB_BT709_FULL)
        );
        assert_eq!(profile_for_cg_color_space_name("ACEScg"), None);
    }

    fn assert_rgb_close(actual: Rgb8, expected: Rgb8, tolerance: i32) {
        let close = |a: u8, e: u8| (i32::from(a) - i32::from(e)).abs() <= tolerance;
        assert!(
            close(actual.r, expected.r)
                && close(actual.g, expected.g)
                && close(actual.b, expected.b),
            "rgb {actual:?} not within {tolerance} of {expected:?}"
        );
    }

    /// 8-bit YCbCr quantization (219/224-level scaling, rounding) makes the
    /// decode a few steps off the source on non-gray colors; ±2 covers every
    /// profile/color pair while still catching sign or matrix bugs.
    #[test]
    fn ycbcr_to_rgb_round_trips_pinned_vectors_and_primaries() {
        let colors = [
            (
                Rgb8 { r: 255, g: 0, b: 0 },
                YCbCr8 {
                    y: 81,
                    cb: 90,
                    cr: 240,
                },
            ),
            (
                Rgb8 { r: 0, g: 255, b: 0 },
                YCbCr8 {
                    y: 145,
                    cb: 54,
                    cr: 34,
                },
            ),
            (
                Rgb8 { r: 0, g: 0, b: 255 },
                YCbCr8 {
                    y: 41,
                    cb: 240,
                    cr: 110,
                },
            ),
            (
                Rgb8 {
                    r: 255,
                    g: 255,
                    b: 255,
                },
                YCbCr8 {
                    y: 235,
                    cb: 128,
                    cr: 128,
                },
            ),
            (
                Rgb8 { r: 0, g: 0, b: 0 },
                YCbCr8 {
                    y: 16,
                    cb: 128,
                    cr: 128,
                },
            ),
        ];
        for (rgb, ycbcr) in colors {
            let decoded = ycbcr_to_rgb_8bit(ycbcr, VideoColorProfile::BT601_VIDEO);
            assert_rgb_close(decoded, rgb, 2);
        }

        let full_colors = [
            (
                Rgb8 { r: 255, g: 0, b: 0 },
                YCbCr8 {
                    y: 54,
                    cb: 99,
                    cr: 255,
                },
            ),
            (
                Rgb8 { r: 0, g: 255, b: 0 },
                YCbCr8 {
                    y: 182,
                    cb: 30,
                    cr: 12,
                },
            ),
            (
                Rgb8 { r: 0, g: 0, b: 255 },
                YCbCr8 {
                    y: 18,
                    cb: 255,
                    cr: 116,
                },
            ),
            (
                Rgb8 {
                    r: 255,
                    g: 255,
                    b: 255,
                },
                YCbCr8 {
                    y: 255,
                    cb: 128,
                    cr: 128,
                },
            ),
            (
                Rgb8 { r: 0, g: 0, b: 0 },
                YCbCr8 {
                    y: 0,
                    cb: 128,
                    cr: 128,
                },
            ),
        ];
        for (rgb, ycbcr) in full_colors {
            let decoded = ycbcr_to_rgb_8bit(ycbcr, VideoColorProfile::SRGB_BT709_FULL);
            assert_rgb_close(decoded, rgb, 2);
        }
    }

    #[test]
    fn ycbcr_to_rgb_white_and_black_are_exact_in_both_ranges() {
        assert_eq!(
            ycbcr_to_rgb_8bit(
                YCbCr8 {
                    y: 235,
                    cb: 128,
                    cr: 128
                },
                VideoColorProfile::BT601_VIDEO
            ),
            Rgb8 {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(
            ycbcr_to_rgb_8bit(
                YCbCr8 {
                    y: 16,
                    cb: 128,
                    cr: 128
                },
                VideoColorProfile::BT601_VIDEO
            ),
            Rgb8 { r: 0, g: 0, b: 0 }
        );
        assert_eq!(
            ycbcr_to_rgb_8bit(
                YCbCr8 {
                    y: 255,
                    cb: 128,
                    cr: 128
                },
                VideoColorProfile::SRGB_BT709_FULL
            ),
            Rgb8 {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(
            ycbcr_to_rgb_8bit(
                YCbCr8 {
                    y: 0,
                    cb: 128,
                    cr: 128
                },
                VideoColorProfile::SRGB_BT709_FULL
            ),
            Rgb8 { r: 0, g: 0, b: 0 }
        );
    }

    #[test]
    fn i420_to_bgra_round_trips_solid_colors() {
        for width in [2usize, 3, 8, 9] {
            for height in [2usize, 3, 8, 9] {
                for profile in [
                    VideoColorProfile::BT601_VIDEO,
                    VideoColorProfile::SRGB_BT709_FULL,
                ] {
                    for rgb in [
                        Rgb8 { r: 255, g: 0, b: 0 },
                        Rgb8 { r: 0, g: 255, b: 0 },
                        Rgb8 { r: 0, g: 0, b: 255 },
                        Rgb8 {
                            r: 255,
                            g: 255,
                            b: 255,
                        },
                        Rgb8 { r: 0, g: 0, b: 0 },
                        Rgb8 {
                            r: 128,
                            g: 96,
                            b: 200,
                        },
                    ] {
                        let ycbcr = rgb_to_ycbcr_8bit(rgb, profile);
                        let chroma_width = width.div_ceil(2);
                        let chroma_height = height.div_ceil(2);
                        let y = vec![ycbcr.y; width * height];
                        let u = vec![ycbcr.cb; chroma_width * chroma_height];
                        let v = vec![ycbcr.cr; chroma_width * chroma_height];
                        let bgra = convert_i420_to_bgra(
                            &y,
                            width,
                            &u,
                            chroma_width,
                            &v,
                            chroma_width,
                            width as u32,
                            height as u32,
                            profile,
                        )
                        .unwrap();
                        assert_eq!(bgra.len(), width * height * 4);
                        for row in 0..height {
                            for col in 0..width {
                                let offset = (row * width + col) * 4;
                                let decoded = Rgb8 {
                                    r: bgra[offset + 2],
                                    g: bgra[offset + 1],
                                    b: bgra[offset],
                                };
                                assert_rgb_close(decoded, rgb, 2);
                                assert_eq!(bgra[offset + 3], 0xff);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn i420_to_bgra_rejects_short_planes_and_undersized_strides() {
        let y = [0u8; 16];
        let u = [0u8; 4];
        let v = [0u8; 4];
        let profile = VideoColorProfile::SRGB_BT709_FULL;
        // 8x8 needs y>=64 bytes at stride 8; 4x4 needs y>=16 at stride 4.
        assert_eq!(
            convert_i420_to_bgra(&y, 8, &u, 2, &v, 2, 8, 8, profile),
            None
        );
        assert_eq!(
            convert_i420_to_bgra(&y[..15], 4, &u, 2, &v, 2, 4, 4, profile),
            None
        );
        assert_eq!(
            convert_i420_to_bgra(&y, 4, &u, 1, &v, 2, 4, 4, profile),
            None
        );
        assert_eq!(
            convert_i420_to_bgra(&y, 4, &u, 2, &v, 1, 4, 4, profile),
            None
        );
        assert_eq!(
            convert_i420_to_bgra(&y, 4, &u, 2, &v, 2, 0, 4, profile),
            None
        );
        assert_eq!(
            convert_i420_to_bgra(&y, 4, &u, 2, &v, 2, 4, 0, profile),
            None
        );
    }

    #[test]
    fn capture_color_space_names_match_profiles() {
        assert_eq!(
            VideoColorProfile::DISPLAY_P3_BT709_FULL.capture_color_space_name(),
            "kCGColorSpaceDisplayP3"
        );
        assert_eq!(
            VideoColorProfile::SRGB_BT709_FULL.capture_color_space_name(),
            "kCGColorSpaceSRGB"
        );
        assert_eq!(
            VideoColorProfile::BT601_VIDEO.capture_color_space_name(),
            "kCGColorSpaceSRGB"
        );
    }

    fn all_profiles() -> [VideoColorProfile; 4] {
        [
            VideoColorProfile::BT601_VIDEO,
            VideoColorProfile {
                primaries: ColorPrimaries::Bt601Ntsc,
                transfer: TransferFunction::Bt709,
                matrix: MatrixCoefficients::Bt601,
                range: PixelRange::Full,
            },
            VideoColorProfile::SRGB_BT709_FULL,
            VideoColorProfile {
                primaries: ColorPrimaries::Bt709,
                transfer: TransferFunction::Srgb,
                matrix: MatrixCoefficients::Bt709,
                range: PixelRange::Video,
            },
        ]
    }

    /// The fixed-point inverse must match an INDEPENDENT floating-point
    /// reference within ±1 (Q16 quantization) across every profile and a
    /// boundary sample of the whole 8-bit YCbCr cube — proving the integer
    /// coefficients and green recovery are exact, not merely plausible.
    #[test]
    fn fixed_point_inverse_matches_floating_reference_across_profiles() {
        for profile in all_profiles() {
            let mut max_diff = 0i32;
            for y in (0..=255u8).step_by(3) {
                for cb in (0..=255u8).step_by(5) {
                    for cr in (0..=255u8).step_by(7) {
                        let ycbcr = YCbCr8 { y, cb, cr };
                        let fixed = ycbcr_to_rgb_8bit(ycbcr, profile);
                        let reference = ycbcr_to_rgb_8bit_reference(ycbcr, profile);
                        let diff = (i32::from(fixed.r) - i32::from(reference.r))
                            .abs()
                            .max((i32::from(fixed.g) - i32::from(reference.g)).abs())
                            .max((i32::from(fixed.b) - i32::from(reference.b)).abs());
                        assert!(
                            diff <= 1,
                            "profile {profile:?} ycbcr {ycbcr:?}: fixed {fixed:?} vs ref {reference:?}"
                        );
                        max_diff = max_diff.max(diff);
                    }
                }
            }
            assert!(
                max_diff <= 1,
                "profile {profile:?}: max fixed-vs-ref diff {max_diff}"
            );
        }
    }

    /// Gray-axis samples (Cb=Cr=128) must be exactly equal in both
    /// directions, at both range extremes.
    #[test]
    fn fixed_point_inverse_is_exact_on_gray_axis() {
        for profile in all_profiles() {
            for y in [0u8, 1, 16, 32, 128, 235, 254, 255] {
                let ycbcr = YCbCr8 {
                    y,
                    cb: 128,
                    cr: 128,
                };
                assert_eq!(
                    ycbcr_to_rgb_8bit(ycbcr, profile),
                    ycbcr_to_rgb_8bit_reference(ycbcr, profile),
                    "gray axis mismatch for profile {profile:?} at y={y}"
                );
            }
        }
    }
}
