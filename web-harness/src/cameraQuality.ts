import {
  alignByCalibrationSquares,
  edgeSharpnessRatio,
  lumaPsnr,
  lumaSsim,
  type PixelBuffer,
} from './crispness.ts';

export const CAMERA_CHART_VERSION = 'camera-chart-v1';
export const CAMERA_CHART_CORNER_COLORS = ['#ebebeb', '#b4b4b4', '#505050', '#101010'] as const;

/** Generate the static portion of the native synthetic camera chart. */
export function cameraChartReference(width: number, height: number): PixelBuffer {
  const data = new Uint8ClampedArray(width * height * 4);
  const put = (x: number, y: number, value: number) => {
    const offset = (y * width + x) * 4;
    data[offset] = value;
    data[offset + 1] = value;
    data[offset + 2] = value;
    data[offset + 3] = 255;
  };
  data.fill(255);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) put(x, y, 96);
  }
  for (let y = 72; y < Math.max(72, height - 96); y += 1) {
    for (let x = 160; x < Math.max(160, width - 160); x += 1) {
      const band = x % 96;
      put(x, y, band < 4 ? 220 : band < 12 ? 175 : band < 28 ? 140 : 96);
    }
  }
  for (let y = 140; y < Math.max(140, height - 140); y += 1) {
    const edge = 260 + Math.floor(y / 3);
    for (let x = edge; x < Math.min(edge + 5, width); x += 1) put(x, y, 235);
  }
  const size = Math.min(24, Math.floor(width / 8), Math.floor(height / 8));
  const corners: Array<[number, number, number]> = [
    [16, 16, 235],
    [Math.max(0, width - 16 - size), 16, 180],
    [16, Math.max(0, height - 16 - size), 80],
    [Math.max(0, width - 16 - size), Math.max(0, height - 16 - size), 16],
  ];
  for (const [left, top, value] of corners) {
    // One exact center pixel makes the existing local color-search alignment
    // oracle unambiguous after scaling and codec filtering.
    put(left + Math.floor(size / 2), top + Math.floor(size / 2), value);
  }
  return { width, height, data };
}

function maskSentinel(buf: PixelBuffer): PixelBuffer {
  const data = new Uint8ClampedArray(buf.data);
  const start = Math.max(0, buf.height - 64);
  const left = Math.min(64, Math.floor(buf.width / 4));
  const right = Math.max(left, buf.width - left);
  for (let y = start; y < Math.max(start, buf.height - 16); y += 1) {
    for (let x = left; x < right; x += 1) {
      const offset = (y * buf.width + x) * 4;
      data[offset] = 96;
      data[offset + 1] = 96;
      data[offset + 2] = 96;
    }
  }
  return { ...buf, data };
}

export interface CameraQualityResult {
  aligned: boolean;
  ssim: number;
  psnr: number;
  edgeRatio: number;
}

export function measureCameraQuality(received: PixelBuffer, reference = cameraChartReference(received.width, received.height)): CameraQualityResult {
  const actual = maskSentinel(received);
  const expected = maskSentinel(reference);
  const alignment = alignByCalibrationSquares(actual, [...CAMERA_CHART_CORNER_COLORS]);
  if (!alignment) return { aligned: false, ssim: 0, psnr: 0, edgeRatio: 0 };
  return {
    aligned: true,
    ssim: lumaSsim(actual, expected, alignment, 0).ssim,
    psnr: lumaPsnr(actual, expected, alignment),
    edgeRatio: edgeSharpnessRatio(actual, expected, 1).ratio,
  };
}
