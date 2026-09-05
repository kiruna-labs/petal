export interface PixelBuffer {
  width: number;
  height: number;
  // RGBA, length === width * height * 4, values 0-255.
  data: Uint8ClampedArray | number[];
}

export interface AlignmentOffset {
  dx: number;
  dy: number;
}

const LUMA_R = 0.2126;
const LUMA_G = 0.7152;
const LUMA_B = 0.0722;
const EDGE_SHARPNESS_PASS_FACTOR = 0.75;
const SSIM_WINDOW_SIZE = 8;
const CALIBRATION_SEARCH_RADIUS = 8;
const CALIBRATION_SQUARE_SIZE = 24;
const CALIBRATION_SQUARE_MARGIN = 16;
const CALIBRATION_MATCH_MAX_DISTANCE = 80;

export function resolutionMatches(decodedW: number, decodedH: number, sourceW: number, sourceH: number): boolean {
  return decodedW === sourceW && decodedH === sourceH;
}

function pixelOffset(buf: PixelBuffer, x: number, y: number): number {
  return (y * buf.width + x) * 4;
}

function lumaAt(buf: PixelBuffer, x: number, y: number): number {
  const offset = pixelOffset(buf, x, y);
  return LUMA_R * buf.data[offset] + LUMA_G * buf.data[offset + 1] + LUMA_B * buf.data[offset + 2];
}

function assertValidBuffer(buf: PixelBuffer) {
  if (buf.width < 1 || buf.height < 1 || buf.data.length !== buf.width * buf.height * 4) {
    throw new Error('PixelBuffer dimensions must match RGBA data length');
  }
}

/**
 * Mean squared Rec. 709 luma response to the 4-neighbor Laplacian kernel.
 * Edges are excluded so callers get deterministic behavior without padding.
 */
export function laplacianEnergy(buf: PixelBuffer): number {
  assertValidBuffer(buf);
  if (buf.width < 3 || buf.height < 3) return 0;
  let sum = 0;
  let count = 0;
  for (let y = 1; y < buf.height - 1; y += 1) {
    for (let x = 1; x < buf.width - 1; x += 1) {
      const response =
        lumaAt(buf, x, y - 1) +
        lumaAt(buf, x - 1, y) -
        4 * lumaAt(buf, x, y) +
        lumaAt(buf, x + 1, y) +
        lumaAt(buf, x, y + 1);
      sum += response * response;
      count += 1;
    }
  }
  return count === 0 ? 0 : sum / count;
}

/**
 * Compare received sharpness against a caller-recorded known-good baseline.
 * Do not replace the baseline with a fixed absolute number; #256 rejected a
 * brittle global SSIM threshold across H.264, 4:2:0, Retina scaling, and
 * simulcast. `passFactor` defaults to requiring 75% of the baseline ratio.
 */
export function edgeSharpnessRatio(
  received: PixelBuffer,
  reference: PixelBuffer,
  baselineRatio: number,
  passFactor = EDGE_SHARPNESS_PASS_FACTOR
): { ratio: number; pass: boolean } {
  const referenceEnergy = laplacianEnergy(reference);
  const ratio = referenceEnergy === 0 ? (laplacianEnergy(received) === 0 ? 1 : Number.POSITIVE_INFINITY) : laplacianEnergy(received) / referenceEnergy;
  return { ratio, pass: ratio >= passFactor * baselineRatio };
}

function parseHexColor(hex: string): [number, number, number] {
  const normalized = hex.trim().replace(/^#/, '');
  if (!/^[0-9a-fA-F]{6}$/.test(normalized)) throw new Error(`invalid hex color: ${hex}`);
  return [
    Number.parseInt(normalized.slice(0, 2), 16),
    Number.parseInt(normalized.slice(2, 4), 16),
    Number.parseInt(normalized.slice(4, 6), 16),
  ];
}

function colorDistance(buf: PixelBuffer, x: number, y: number, color: [number, number, number]): number {
  const offset = pixelOffset(buf, x, y);
  const dr = buf.data[offset] - color[0];
  const dg = buf.data[offset + 1] - color[1];
  const db = buf.data[offset + 2] - color[2];
  return Math.sqrt(dr * dr + dg * dg + db * db);
}

function expectedCalibrationCenters(buf: PixelBuffer): Array<{ x: number; y: number }> {
  const inset = CALIBRATION_SQUARE_MARGIN + Math.floor(CALIBRATION_SQUARE_SIZE / 2);
  return [
    { x: inset, y: inset },
    { x: buf.width - inset, y: inset },
    { x: inset, y: buf.height - inset },
    { x: buf.width - inset, y: buf.height - inset },
  ];
}

/**
 * Find #256 corner calibration squares by local color search near each
 * expected corner position and return the average offset. `null` means the
 * pattern is not confidently locatable, so callers must not claim alignment.
 */
export function alignByCalibrationSquares(
  buf: PixelBuffer,
  cornerColors: [string, string, string, string]
): AlignmentOffset | null {
  assertValidBuffer(buf);
  const centers = expectedCalibrationCenters(buf);
  const offsets: AlignmentOffset[] = [];
  for (let i = 0; i < centers.length; i += 1) {
    const expected = centers[i];
    const target = parseHexColor(cornerColors[i]);
    let best: { x: number; y: number; distance: number } | null = null;
    for (let dy = -CALIBRATION_SEARCH_RADIUS; dy <= CALIBRATION_SEARCH_RADIUS; dy += 1) {
      for (let dx = -CALIBRATION_SEARCH_RADIUS; dx <= CALIBRATION_SEARCH_RADIUS; dx += 1) {
        const x = expected.x + dx;
        const y = expected.y + dy;
        if (x < 0 || y < 0 || x >= buf.width || y >= buf.height) continue;
        const distance = colorDistance(buf, x, y, target);
        if (!best || distance < best.distance) best = { x, y, distance };
      }
    }
    if (!best || best.distance > CALIBRATION_MATCH_MAX_DISTANCE) return null;
    offsets.push({ dx: best.x - expected.x, dy: best.y - expected.y });
  }
  return {
    dx: offsets.reduce((sum, offset) => sum + offset.dx, 0) / offsets.length,
    dy: offsets.reduce((sum, offset) => sum + offset.dy, 0) / offsets.length,
  };
}

function alignedPairs(received: PixelBuffer, reference: PixelBuffer, alignment: AlignmentOffset): Array<[number, number]> {
  const dx = Math.round(alignment.dx);
  const dy = Math.round(alignment.dy);
  const pairs: Array<[number, number]> = [];
  for (let y = 0; y < reference.height; y += 1) {
    const ry = y + dy;
    if (ry < 0 || ry >= received.height) continue;
    for (let x = 0; x < reference.width; x += 1) {
      const rx = x + dx;
      if (rx < 0 || rx >= received.width) continue;
      pairs.push([lumaAt(received, rx, ry), lumaAt(reference, x, y)]);
    }
  }
  return pairs;
}

function ssimForWindow(pairs: Array<[number, number]>, start: number, length: number): number {
  const c1 = (0.01 * 255) ** 2;
  const c2 = (0.03 * 255) ** 2;
  let meanA = 0;
  let meanB = 0;
  for (let i = start; i < start + length; i += 1) {
    meanA += pairs[i][0];
    meanB += pairs[i][1];
  }
  meanA /= length;
  meanB /= length;
  let varianceA = 0;
  let varianceB = 0;
  let covariance = 0;
  for (let i = start; i < start + length; i += 1) {
    const da = pairs[i][0] - meanA;
    const db = pairs[i][1] - meanB;
    varianceA += da * da;
    varianceB += db * db;
    covariance += da * db;
  }
  varianceA /= length;
  varianceB /= length;
  covariance /= length;
  return ((2 * meanA * meanB + c1) * (2 * covariance + c2)) /
    ((meanA * meanA + meanB * meanB + c1) * (varianceA + varianceB + c2));
}

/**
 * Luma-only SSIM after applying a caller-supplied alignment offset. The pass
 * threshold is a recorded known-good baseline, never a hardcoded 0.95 (#256).
 */
export function lumaSsim(
  received: PixelBuffer,
  reference: PixelBuffer,
  alignment: AlignmentOffset,
  baselineSsim: number
): { ssim: number; pass: boolean } {
  assertValidBuffer(received);
  assertValidBuffer(reference);
  const pairs = alignedPairs(received, reference, alignment);
  if (pairs.length === 0) return { ssim: 0, pass: false };
  const chunkSize = SSIM_WINDOW_SIZE * SSIM_WINDOW_SIZE;
  let weighted = 0;
  let count = 0;
  for (let start = 0; start < pairs.length; start += chunkSize) {
    const length = Math.min(chunkSize, pairs.length - start);
    if (length < 2) continue;
    weighted += ssimForWindow(pairs, start, length) * length;
    count += length;
  }
  const ssim = count === 0 ? 0 : weighted / count;
  return { ssim, pass: ssim >= baselineSsim };
}
