//! The "Be right back" slate and its clock. When the bound share goes away
//! the stream shows the slate, with meeting audio still playing, for at most
//! 120 s and then ends. There is no hard cap and no empty-room rule (the
//! sharer is in the room by construction).

use std::time::Duration;

pub const STREAM_SLATE_MAX: Duration = Duration::from_secs(120);

/// The bundled slate image, 1920x1080. Text is set in Pillow's bundled
/// default face (Aileron, CC0).
static SLATE_PNG: &[u8] = include_bytes!("../../assets/stream-slate.png");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlateDecision {
    Share,
    Slate,
    /// The slate held for [`STREAM_SLATE_MAX`]: end the stream.
    End,
}

#[derive(Debug, Clone, Default)]
pub struct SlateClock {
    slate_since: Option<Duration>,
}

impl SlateClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// `now` is any monotonic time base; `share_available` means the bound
    /// share is published and has delivered a frame.
    pub fn tick(&mut self, now: Duration, share_available: bool) -> SlateDecision {
        if share_available {
            self.slate_since = None;
            return SlateDecision::Share;
        }
        let since = *self.slate_since.get_or_insert(now);
        if now.saturating_sub(since) >= STREAM_SLATE_MAX {
            SlateDecision::End
        } else {
            SlateDecision::Slate
        }
    }

    pub fn on_slate(&self) -> bool {
        self.slate_since.is_some()
    }

    /// Seconds left on the slate, for the sharer's notice.
    pub fn remaining(&self, now: Duration) -> Option<Duration> {
        self.slate_since.map(|since| STREAM_SLATE_MAX.saturating_sub(now.saturating_sub(since)))
    }
}

/// A BGRA frame (tightly packed rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgraImage {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// The slate scaled to fit `width`x`height`, letterboxed in black.
pub fn render_slate(width: u32, height: u32) -> BgraImage {
    let width = width.max(2);
    let height = height.max(2);
    let mut bgra = vec![0u8; width as usize * height as usize * 4];
    for pixel in bgra.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    let Ok(decoded) = image::load_from_memory_with_format(SLATE_PNG, image::ImageFormat::Png) else {
        return BgraImage { width, height, bgra };
    };
    let (x, y, w, h) = super::plan::letterbox_rect(decoded.width(), decoded.height(), width, height);
    if w == 0 || h == 0 {
        return BgraImage { width, height, bgra };
    }
    let scaled = decoded
        .resize_exact(w, h, image::imageops::FilterType::Triangle)
        .to_rgba8();
    for row in 0..h {
        for col in 0..w {
            let src = scaled.get_pixel(col, row).0;
            let offset = (((y + row) * width + x + col) * 4) as usize;
            bgra[offset..offset + 4].copy_from_slice(&[src[2], src[1], src[0], 255]);
        }
    }
    BgraImage { width, height, bgra }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slate_holds_for_two_minutes_then_ends() {
        let mut clock = SlateClock::new();
        let s = Duration::from_secs;
        assert_eq!(clock.tick(s(0), true), SlateDecision::Share);
        assert_eq!(clock.tick(s(10), false), SlateDecision::Slate);
        assert_eq!(clock.remaining(s(70)), Some(s(60)));
        assert_eq!(clock.tick(s(129), false), SlateDecision::Slate);
        assert_eq!(clock.tick(s(130), false), SlateDecision::End);
    }

    #[test]
    fn a_returning_share_resets_the_slate() {
        let mut clock = SlateClock::new();
        let s = Duration::from_secs;
        clock.tick(s(0), false);
        assert_eq!(clock.tick(s(100), true), SlateDecision::Share);
        assert!(!clock.on_slate());
        assert_eq!(clock.tick(s(200), false), SlateDecision::Slate);
        assert_eq!(clock.tick(s(319), false), SlateDecision::Slate);
    }

    #[test]
    fn renders_the_slate_letterboxed() {
        // Portrait canvas: the 16:9 slate sits in the middle band.
        let image = render_slate(720, 1280);
        assert_eq!(image.bgra.len(), 720 * 1280 * 4);
        let pixel = |x: u32, y: u32| {
            let offset = ((y * 720 + x) * 4) as usize;
            [image.bgra[offset], image.bgra[offset + 1], image.bgra[offset + 2], image.bgra[offset + 3]]
        };
        assert_eq!(pixel(10, 10), [0, 0, 0, 255], "letterbox bar is black");
        // The slate background is a dark grey, not black.
        let band = pixel(10, 640);
        assert!(band[0] > 0 && band[0] < 40, "{band:?}");
        assert_eq!(band[3], 255);
    }
}
