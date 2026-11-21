//! BGRA -> NV12 on the CPU, for a computer with no D3D11 video device (the
//! software H.264 encoder takes system memory). Same output as the GPU
//! Video Processor: BT.709 limited range, scaled to fit and letterboxed in
//! black on the fixed canvas.
//!
//! The color matrix is ours: the libyuv in the libwebrtc prebuilt converts
//! RGB to YUV in BT.601 only, and the stream is tagged BT.709. Scaling and
//! NV12 packing are libyuv's (`webrtc_sys::yuv_helper`, SIMD).
//!
//! Work follows change: [`CpuConverter::upload`] converts each new source
//! frame once, and [`CpuConverter::convert`] scales again only when the
//! source or the destination rectangle changed. A static window costs
//! nothing per tick. Compiled everywhere so its tests run on every OS; only
//! Windows streams through it.

/// A borrowed BGRA frame (`bytes_per_row` stride).
pub struct BgraView<'a> {
    pub bgra: &'a [u8],
    pub bytes_per_row: u32,
    pub width: u32,
    pub height: u32,
}

// BT.709 limited range, Q16: Y = 16 + 219/255 (Kr R + Kg G + Kb B), and the
// chroma rows of the same matrix scaled by 224/255. Each chroma row sums to 0,
// so grey stays exactly 128.
const Y_R: i32 = 11_966;
const Y_G: i32 = 40_254;
const Y_B: i32 = 4_064;
const CB_R: i32 = -6_596;
const CB_G: i32 = -22_188;
const CB_B: i32 = 28_784;
const CR_R: i32 = 28_784;
const CR_G: i32 = -26_145;
const CR_B: i32 = -2_639;
const Y_OFFSET: i32 = (16 << 16) + (1 << 15);
/// Chroma from the sum of a 2x2 block: Q16 and a further /4.
const C_OFFSET: i32 = (128 << 18) + (1 << 17);

/// A planar 4:2:0 image, tightly packed.
#[derive(Default)]
struct I420 {
    size: (u32, u32),
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420 {
    fn chroma_size(size: (u32, u32)) -> (usize, usize) {
        (size.0.div_ceil(2) as usize, size.1.div_ceil(2) as usize)
    }

    fn resize(&mut self, size: (u32, u32)) {
        if self.size == size {
            return;
        }
        let (cw, ch) = Self::chroma_size(size);
        self.size = size;
        self.y.resize(size.0 as usize * size.1 as usize, 0);
        self.u.resize(cw * ch, 0);
        self.v.resize(cw * ch, 0);
    }
}

/// One BGRA row into Y.
fn luma_row(bgra: &[u8], y: &mut [u8]) {
    for (out, px) in y.iter_mut().zip(bgra.chunks_exact(4)) {
        let (b, g, r) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
        *out = ((Y_R * r + Y_G * g + Y_B * b + Y_OFFSET) >> 16) as u8;
    }
}

/// Two BGRA rows (the same row twice at an odd bottom edge) into one row
/// of U and V, each from its 2x2 block's summed RGB.
fn chroma_row(top: &[u8], bottom: &[u8], width: usize, u: &mut [u8], v: &mut [u8]) {
    let pairs = width / 2;
    let sum = |a: &[u8], b: &[u8], c: &[u8], d: &[u8]| {
        let s = |i: usize| i32::from(a[i]) + i32::from(b[i]) + i32::from(c[i]) + i32::from(d[i]);
        (s(0), s(1), s(2))
    };
    let blocks = top.chunks_exact(8).zip(bottom.chunks_exact(8)).take(pairs);
    for ((cb, cr), (t, b)) in u.iter_mut().zip(v.iter_mut()).zip(blocks) {
        let (sb, sg, sr) = sum(&t[..4], &t[4..], &b[..4], &b[4..]);
        *cb = ((CB_R * sr + CB_G * sg + CB_B * sb + C_OFFSET) >> 18) as u8;
        *cr = ((CR_R * sr + CR_G * sg + CR_B * sb + C_OFFSET) >> 18) as u8;
    }
    if width % 2 == 1 {
        // The odd right edge: its one column, counted twice.
        let last = (width - 1) * 4;
        let (t, b) = (&top[last..last + 4], &bottom[last..last + 4]);
        let (sb, sg, sr) = sum(t, t, b, b);
        u[pairs] = ((CB_R * sr + CB_G * sg + CB_B * sb + C_OFFSET) >> 18) as u8;
        v[pairs] = ((CR_R * sr + CR_G * sg + CR_B * sb + C_OFFSET) >> 18) as u8;
    }
}

fn bgra_to_i420(frame: &BgraView<'_>, out: &mut I420) {
    let (width, height) = (frame.width as usize, frame.height as usize);
    out.resize((frame.width, frame.height));
    let stride = frame.bytes_per_row as usize;
    let row = |index: usize| &frame.bgra[index * stride..index * stride + width * 4];
    for (index, y) in out.y.chunks_exact_mut(width).enumerate() {
        luma_row(row(index), y);
    }
    let (cw, _) = I420::chroma_size(out.size);
    for (index, (u, v)) in out.u.chunks_exact_mut(cw).zip(out.v.chunks_exact_mut(cw)).enumerate() {
        let top = index * 2;
        chroma_row(row(top), row((top + 1).min(height - 1)), width, u, v);
    }
}

/// `dest` snapped outward to even edges inside the canvas, for NV12's 2x2
/// chroma: each edge moves at most 1 px.
fn even_rect(dest: (u32, u32, u32, u32), canvas: (u32, u32)) -> (u32, u32, u32, u32) {
    let (x, y, w, h) = dest;
    let x0 = x & !1;
    let y0 = y & !1;
    let x1 = ((x + w + 1) & !1).min(canvas.0);
    let y1 = ((y + h + 1) & !1).min(canvas.1);
    (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

/// BGRA -> NV12 on the CPU, scaled into a fixed even-sized canvas.
pub struct CpuConverter {
    canvas: (u32, u32),
    source: I420,
    has_source: bool,
    /// Bumped by every upload.
    source_generation: u64,
    scaled: I420,
    /// Packed NV12: Y (`canvas.0` stride), then interleaved UV.
    nv12: Vec<u8>,
    /// What `nv12` holds: (even dest rect, source generation).
    rendered: Option<((u32, u32, u32, u32), u64)>,
    scales: u64,
}

impl CpuConverter {
    pub fn new(canvas: (u32, u32)) -> Result<Self, String> {
        if canvas.0 == 0 || canvas.1 == 0 || canvas.0 % 2 == 1 || canvas.1 % 2 == 1 {
            return Err(format!("the canvas {}x{} isn't even-sized", canvas.0, canvas.1));
        }
        Ok(Self {
            canvas,
            source: I420::default(),
            has_source: false,
            source_generation: 0,
            scaled: I420::default(),
            nv12: vec![0; canvas.0 as usize * canvas.1 as usize * 3 / 2],
            rendered: None,
            scales: 0,
        })
    }

    pub fn canvas(&self) -> (u32, u32) {
        self.canvas
    }

    /// Convert a new source frame (only when the capture delivered one).
    pub fn upload(&mut self, frame: &BgraView<'_>) -> Result<(), String> {
        if frame.width == 0 || frame.height == 0 {
            return Err("empty frame".into());
        }
        let needed = frame.bytes_per_row as usize * (frame.height as usize - 1) + frame.width as usize * 4;
        if frame.bytes_per_row < frame.width * 4 || frame.bgra.len() < needed {
            return Err("short frame buffer".into());
        }
        bgra_to_i420(frame, &mut self.source);
        self.has_source = true;
        self.source_generation += 1;
        Ok(())
    }

    /// The canvas as NV12 with the current source letterboxed into `dest`
    /// (x, y, w, h).
    pub fn convert(&mut self, dest: (u32, u32, u32, u32)) -> Result<&[u8], String> {
        if !self.has_source {
            return Err("no input".into());
        }
        let rect = even_rect(dest, self.canvas);
        let (x, y, w, h) = rect;
        if w == 0 || h == 0 {
            return Err("empty destination".into());
        }
        let previous = self.rendered;
        if previous.is_some_and(|rendered| rendered == (rect, self.source_generation)) {
            return Ok(&self.nv12);
        }
        let (cw, ch) = (self.canvas.0 as usize, self.canvas.1 as usize);
        if previous.is_none_or(|(old, _)| old != rect) {
            // New placement: repaint the bars (Y 16, UV 128) under it.
            let (luma, chroma) = self.nv12.split_at_mut(cw * ch);
            luma.fill(16);
            chroma.fill(128);
        }
        self.rendered = None;
        self.scaled.resize((w, h));
        let (src, dst) = (&self.source, &mut self.scaled);
        let (src_cw, _) = I420::chroma_size(src.size);
        let (dst_cw, _) = I420::chroma_size(dst.size);
        // SAFETY: every plane pointer and stride describes a buffer sized for
        // its image (`I420::resize`); `w`/`h` fit inside the canvas, so the
        // NV12 writes at (x, y) with the canvas stride stay inside `nv12`.
        unsafe {
            webrtc_sys::yuv_helper::ffi::i420_scale(
                src.y.as_ptr(),
                src.size.0 as i32,
                src.u.as_ptr(),
                src_cw as i32,
                src.v.as_ptr(),
                src_cw as i32,
                src.size.0 as i32,
                src.size.1 as i32,
                dst.y.as_mut_ptr(),
                w as i32,
                dst.u.as_mut_ptr(),
                dst_cw as i32,
                dst.v.as_mut_ptr(),
                dst_cw as i32,
                w as i32,
                h as i32,
            )
            .map_err(|error| format!("I420Scale: {error}"))?;
            let base = self.nv12.as_mut_ptr();
            webrtc_sys::yuv_helper::ffi::i420_to_nv12(
                dst.y.as_ptr(),
                w as i32,
                dst.u.as_ptr(),
                dst_cw as i32,
                dst.v.as_ptr(),
                dst_cw as i32,
                base.add(y as usize * cw + x as usize),
                cw as i32,
                // Interleaved UV: half the rows, full stride, x bytes in.
                base.add(cw * ch + (y as usize / 2) * cw + x as usize),
                cw as i32,
                w as i32,
                h as i32,
            )
            .map_err(|error| format!("I420ToNV12: {error}"))?;
        }
        self.scales += 1;
        self.rendered = Some((rect, self.source_generation));
        Ok(&self.nv12)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::plan::letterbox_rect;
    use crate::video_color::{rgb_to_ycbcr_8bit, Rgb8, VideoColorProfile};

    const BT709_VIDEO: VideoColorProfile = VideoColorProfile {
        matrix: crate::video_color::MatrixCoefficients::Bt709,
        range: crate::video_color::PixelRange::Video,
        ..VideoColorProfile::SRGB_BT709_FULL
    };

    fn solid(width: u32, height: u32, [r, g, b]: [u8; 3]) -> Vec<u8> {
        [b, g, r, 255].repeat(width as usize * height as usize)
    }

    fn view(bgra: &[u8], width: u32, height: u32) -> BgraView<'_> {
        BgraView { bgra, bytes_per_row: width * 4, width, height }
    }

    /// (Y, U, V) at canvas pixel (px, py).
    fn sample(nv12: &[u8], canvas: (u32, u32), px: u32, py: u32) -> (u8, u8, u8) {
        let (cw, ch) = (canvas.0 as usize, canvas.1 as usize);
        let uv = cw * ch + (py as usize / 2) * cw + (px as usize / 2) * 2;
        (nv12[py as usize * cw + px as usize], nv12[uv], nv12[uv + 1])
    }

    #[test]
    fn matches_the_bt709_limited_range_oracle() {
        let colors = [
            [255, 255, 255],
            [0, 0, 0],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [128, 128, 128],
            [37, 180, 220],
        ];
        for rgb in colors {
            let canvas = (64, 36);
            let mut converter = CpuConverter::new(canvas).unwrap();
            let bgra = solid(64, 36, rgb);
            converter.upload(&view(&bgra, 64, 36)).unwrap();
            let nv12 = converter.convert((0, 0, 64, 36)).unwrap();
            let expected = rgb_to_ycbcr_8bit(Rgb8 { r: rgb[0], g: rgb[1], b: rgb[2] }, BT709_VIDEO);
            let (y, u, v) = sample(nv12, canvas, 31, 17);
            for (got, want, plane) in [(y, expected.y, "Y"), (u, expected.cb, "U"), (v, expected.cr, "V")] {
                assert!(got.abs_diff(want) <= 1, "{rgb:?} {plane}: {got} vs {want}");
            }
        }
        // The two ends of the range, exactly.
        assert_eq!(rgb_to_ycbcr_8bit(Rgb8 { r: 255, g: 255, b: 255 }, BT709_VIDEO).y, 235);
        assert_eq!(rgb_to_ycbcr_8bit(Rgb8 { r: 0, g: 0, b: 0 }, BT709_VIDEO).y, 16);
    }

    #[test]
    fn letterboxes_within_a_pixel_of_the_plan_with_black_bars() {
        let white = [255, 255, 255];
        // (source, canvas): 4:3 -> 16:9, portrait -> landscape, a small share
        // scaled up, and odd sizes.
        for ((sw, sh), canvas) in [
            ((800, 600), (1280, 720)),
            ((900, 1600), (1920, 1080)),
            ((640, 360), (1920, 1080)),
            ((1001, 777), (1280, 720)),
            ((333, 1001), (720, 1280)),
        ] {
            let mut converter = CpuConverter::new(canvas).unwrap();
            let bgra = solid(sw, sh, white);
            converter.upload(&view(&bgra, sw, sh)).unwrap();
            let rect = letterbox_rect(sw, sh, canvas.0, canvas.1);
            let nv12 = converter.convert(rect).unwrap().to_vec();
            let (x, y, w, h) = rect;
            let lit = |px: u32, py: u32| sample(&nv12, canvas, px, py).0 > 128;
            // Content edges: lit 1 px inside, dark 2 px outside, on each side
            // that has a bar.
            let mid = (x + w / 2, y + h / 2);
            assert!(lit(mid.0, mid.1), "{sw}x{sh}: centre");
            if x >= 2 {
                assert!(lit(x + 1, mid.1) && !lit(x - 2, mid.1), "{sw}x{sh}: left edge at {x}");
                assert!(lit(x + w - 2, mid.1) && !lit(x + w + 1, mid.1), "{sw}x{sh}: right edge");
            }
            if y >= 2 {
                assert!(lit(mid.0, y + 1) && !lit(mid.0, y - 2), "{sw}x{sh}: top edge at {y}");
                assert!(lit(mid.0, y + h - 2) && !lit(mid.0, y + h + 1), "{sw}x{sh}: bottom edge");
            }
            // The bars are limited-range black.
            if x > 0 || y > 0 {
                assert_eq!(sample(&nv12, canvas, 0, 0), (16, 128, 128), "{sw}x{sh}: bars");
            }
        }
    }

    #[test]
    fn scales_again_only_when_the_source_or_placement_changes() {
        let canvas = (1280, 720);
        let mut converter = CpuConverter::new(canvas).unwrap();
        assert_eq!(converter.convert((0, 0, 1280, 720)).unwrap_err(), "no input");
        let grey = solid(800, 600, [128, 128, 128]);
        converter.upload(&view(&grey, 800, 600)).unwrap();
        let rect = letterbox_rect(800, 600, 1280, 720);
        converter.convert(rect).unwrap();
        converter.convert(rect).unwrap();
        converter.convert(rect).unwrap();
        assert_eq!(converter.scales, 1, "an unchanged source is not scaled again");
        // A new frame, then a new size: scaled each time; the old bars go.
        let red = solid(800, 600, [255, 0, 0]);
        converter.upload(&view(&red, 800, 600)).unwrap();
        converter.convert(rect).unwrap();
        assert_eq!(converter.scales, 2);
        let wide = solid(1280, 360, [255, 255, 255]);
        converter.upload(&view(&wide, 1280, 360)).unwrap();
        let nv12 = converter.convert(letterbox_rect(1280, 360, 1280, 720)).unwrap().to_vec();
        assert_eq!(converter.scales, 3);
        assert_eq!(sample(&nv12, canvas, 640, 100), (16, 128, 128), "letterbox bar, not the old frame");
        assert_eq!(sample(&nv12, canvas, 20, 360).0, 235, "the wide frame fills the width");
    }

    #[test]
    fn the_slate_fills_the_canvas() {
        let canvas = (1280, 720);
        let slate = crate::streaming::slate::render_slate(canvas.0, canvas.1);
        let mut converter = CpuConverter::new(canvas).unwrap();
        converter
            .upload(&BgraView { bgra: &slate.bgra, bytes_per_row: slate.width * 4, width: slate.width, height: slate.height })
            .unwrap();
        let nv12 = converter.convert(letterbox_rect(slate.width, slate.height, canvas.0, canvas.1)).unwrap();
        assert_eq!(nv12.len(), 1280 * 720 * 3 / 2);
        // Spot-check a pixel against the oracle.
        let at = (100 * slate.width as usize + 100) * 4;
        let px = &slate.bgra[at..at + 4];
        let want = rgb_to_ycbcr_8bit(Rgb8 { r: px[2], g: px[1], b: px[0] }, BT709_VIDEO);
        assert!(sample(nv12, canvas, 100, 100).0.abs_diff(want.y) <= 2);
    }

    #[test]
    fn refuses_short_and_empty_frames() {
        let mut converter = CpuConverter::new((1280, 720)).unwrap();
        let bgra = vec![0u8; 100 * 100 * 4 - 1];
        assert_eq!(converter.upload(&view(&bgra, 100, 100)).unwrap_err(), "short frame buffer");
        assert_eq!(converter.upload(&view(&[], 0, 100)).unwrap_err(), "empty frame");
        let padded = vec![0u8; 400 * 99 + 100 * 4];
        assert!(converter.upload(&BgraView { bgra: &padded, bytes_per_row: 400, width: 100, height: 100 }).is_ok());
        assert!(CpuConverter::new((1281, 720)).is_err(), "NV12 needs an even canvas");
    }

    #[test]
    fn snaps_the_destination_to_even_edges() {
        assert_eq!(even_rect((0, 0, 1280, 720), (1280, 720)), (0, 0, 1280, 720));
        assert_eq!(even_rect((147, 0, 985, 720), (1280, 720)), (146, 0, 986, 720));
        assert_eq!(even_rect((0, 3, 1280, 715), (1280, 720)), (0, 2, 1280, 716));
        assert_eq!(even_rect((1279, 0, 1, 720), (1280, 720)), (1278, 0, 2, 720));
    }
}
