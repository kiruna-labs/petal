# Test Pattern Spec

Issue #256 defines the web-harness renderer as the reference implementation:
`web-harness/src/testPattern.ts` exports the constants below, and the native
Svelte route ports the same values without a cross-package import.

## Canvas

- Logical canvas: `960 x 600` pixels.
- Background: solid `#1b1033`.
- Frame counter: unsigned 16-bit counter, incremented once per animation frame
  and wrapped modulo `65536`.

## Gray-Code Strip

The machine-readable frame counter is a 16-bit reflected Gray code, not text.
For counter `n`, implementations encode `(n & 0xffff) ^ ((n & 0xffff) >> 1)`.
The decoder converts Gray back to binary and returns `0..65535`; after frame
`65535`, the next frame wraps to `0`.

- Bit order: most-significant bit first, left to right.
- Zero color: `#000000`.
- One color: `#ffffff`.
- Sampling tolerance: readers should classify each block by mean color with
  +/- `40/255` tolerance per channel. The two colors are `255/255` apart in
  every channel, so the bit remains recoverable after H.264 encode, 4:2:0
  chroma subsampling, scaling, and gamma shifts.
- Strip bounds: `x=160, y=88, w=640, h=30`.
- Blocks: 16 fixed rectangles, each `40 x 30`.

Block `i` has bounds:

```text
x = 160 + i * 40
y = 88
w = 40
h = 30
```

Gray code is required because only one bit flips per increment, reducing the
chance that a mistimed capture of a transition reads as a wildly wrong frame.

## Corner Calibration Squares

The four calibration squares are fixed `24 x 24` rectangles inset `16` pixels
from the canvas corners. They are used for alignment sanity and as anchor
points for the SSIM-alignment helper.

| Corner | Bounds | Color |
| --- | --- | --- |
| Top left | `x=16, y=16, w=24, h=24` | `#ff2d55` |
| Top right | `x=920, y=16, w=24, h=24` | `#00ff88` |
| Bottom left | `x=16, y=560, w=24, h=24` | `#2d7dff` |
| Bottom right | `x=920, y=560, w=24, h=24` | `#ffd400` |

## Sharpness Target

The center target is a black/white checkerboard:

- Bounds: `x=352, y=220, w=256, h=160`.
- Cell size: `4 x 4` pixels.
- Top-left cell: white `#ffffff`; adjacent cells alternate black `#000000`.

This crop is the canonical input region for edge-sharpness and SSIM helpers.
It does not overlap the Gray-code strip or corner calibration squares.

## Decorative Motion

The decorative moving circle exists only as a human liveness indicator. It must
never overlap the measurement regions.

- Radius: `32`.
- Fill: `#aa3bff`.
- Stroke: `#00ff88`, `6px`.
- Center orbit bounds: `x=96, y=430, w=768, h=78`.
- Formula, for frame counter `f`:

```text
t = f / 30
cx = 96 + (sin(t * 0.7) * 0.5 + 0.5) * 768
cy = 430 + (cos(t * 0.9) * 0.5 + 0.5) * 78
```

With radius included, the circle occupies only:

```text
x = 64..896
y = 398..540
```

That is below the sharpness target (`y=220..380`) with an 18px gap, above the
bottom calibration squares (`y=560..584`) with a 20px gap, below the Gray-code
strip (`y=88..118`), and horizontally away from the top corner squares.

## Crispness Rationale

The helpers intentionally use layered checks rather than a fixed SSIM threshold:
resolution equality, Laplacian-energy ratio against a recorded known-good
baseline, calibration-square alignment, and luma SSIM against a caller-supplied
baseline. A fixed rule such as `SSIM >= 0.95` was rejected as too brittle across
H.264 encode, 4:2:0 chroma subsampling, Retina capture resampling, and
simulcast. Do not fabricate baseline PNG fixtures; they must come from a real
known-good capture run.
