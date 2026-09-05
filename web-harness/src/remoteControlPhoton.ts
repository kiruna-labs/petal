import {
  CORNER_CALIBRATION_SQUARES,
  GRAY_CODE_BLOCK_RECTS,
  TEST_PATTERN_HEIGHT,
  TEST_PATTERN_WIDTH,
  grayBlocksToFrameCounter,
  type Rect
} from './testPattern';

export interface RgbaFrame {
  data: Uint8ClampedArray;
  width: number;
  height: number;
}

export interface PhotonSentinelFrame {
  generation: number;
  confidence: number;
  calibrationMatches: number;
}

const SAMPLE_INSET = 0.25;
const GRAY_ZERO_MAX_LUMA = 96;
const GRAY_ONE_MIN_LUMA = 159;
const CALIBRATION_CHANNEL_TOLERANCE = 100;
const GENERATION_MASK = 0xffff;

export function nextPhotonGeneration(generation: number): number {
  if (!Number.isFinite(generation)) throw new Error('photon generation must be finite');
  return (Math.trunc(generation) + 1) & GENERATION_MASK;
}

export function matchesExpectedPhotonGeneration(
  frame: PhotonSentinelFrame | null,
  expectedGeneration: number
): frame is PhotonSentinelFrame {
  return frame?.generation === (Math.trunc(expectedGeneration) & GENERATION_MASK);
}

function parseHexColor(hex: string): [number, number, number] {
  const value = Number.parseInt(hex.slice(1), 16);
  return [(value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff];
}

function sampleRect(frame: RgbaFrame, rect: Rect): [number, number, number] | null {
  if (
    frame.width <= 0 ||
    frame.height <= 0 ||
    frame.data.length !== frame.width * frame.height * 4
  ) {
    return null;
  }

  const left = Math.max(
    0,
    Math.floor(((rect.x + rect.w * SAMPLE_INSET) / TEST_PATTERN_WIDTH) * frame.width)
  );
  const top = Math.max(
    0,
    Math.floor(((rect.y + rect.h * SAMPLE_INSET) / TEST_PATTERN_HEIGHT) * frame.height)
  );
  const right = Math.min(
    frame.width,
    Math.ceil(((rect.x + rect.w * (1 - SAMPLE_INSET)) / TEST_PATTERN_WIDTH) * frame.width)
  );
  const bottom = Math.min(
    frame.height,
    Math.ceil(((rect.y + rect.h * (1 - SAMPLE_INSET)) / TEST_PATTERN_HEIGHT) * frame.height)
  );
  if (right <= left || bottom <= top) return null;

  let red = 0;
  let green = 0;
  let blue = 0;
  let pixels = 0;
  for (let y = top; y < bottom; y += 1) {
    for (let x = left; x < right; x += 1) {
      const offset = (y * frame.width + x) * 4;
      red += frame.data[offset];
      green += frame.data[offset + 1];
      blue += frame.data[offset + 2];
      pixels += 1;
    }
  }
  return pixels > 0 ? [red / pixels, green / pixels, blue / pixels] : null;
}

function maxChannelDistance(
  actual: [number, number, number],
  expected: [number, number, number]
): number {
  return Math.max(
    Math.abs(actual[0] - expected[0]),
    Math.abs(actual[1] - expected[1]),
    Math.abs(actual[2] - expected[2])
  );
}

/**
 * Decode one static remote-control sentinel generation from a captured video
 * frame. Calibration is mandatory so an unrelated window or a cropped frame
 * cannot accidentally look like a valid Gray-code response.
 */
export function decodePhotonSentinelFrame(frame: RgbaFrame): PhotonSentinelFrame | null {
  let calibrationMatches = 0;
  for (const square of CORNER_CALIBRATION_SQUARES) {
    const actual = sampleRect(frame, square);
    if (!actual) return null;
    if (maxChannelDistance(actual, parseHexColor(square.color)) <= CALIBRATION_CHANNEL_TOLERANCE) {
      calibrationMatches += 1;
    }
  }
  if (calibrationMatches !== CORNER_CALIBRATION_SQUARES.length) return null;

  const bits: boolean[] = [];
  let confidence = 1;
  for (const rect of GRAY_CODE_BLOCK_RECTS) {
    const rgb = sampleRect(frame, rect);
    if (!rgb) return null;
    const luma = rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722;
    if (luma <= GRAY_ZERO_MAX_LUMA) bits.push(false);
    else if (luma >= GRAY_ONE_MIN_LUMA) bits.push(true);
    else return null;
    confidence = Math.min(confidence, Math.abs(luma - 127.5) / 127.5);
  }

  return {
    generation: grayBlocksToFrameCounter(bits),
    confidence,
    calibrationMatches
  };
}
