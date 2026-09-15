// SINGLE SOURCE OF TRUTH for gallery tile packing geometry. Shared by the
// desktop gallery (apps/desktop/src/lib/components/Gallery.svelte, via
// apps/desktop/src/lib/galleryLayout.ts having been folded into this module)
// and the web client's layout lab (web-harness/src/layoutLab.ts). Pure: no
// DOM, no framework -- callers own the container measurement and CSS.
//
// The desktop gallery used to hard-code a 2x2 grid for 3-4 participants and
// start its column search at 2, making a single column unreachable for any
// count -- so 4 participants in a tall, narrow window filled ~34% of the
// area when a 1x4 column would have filled ~66%. This module always searches
// every column count from 1 to `count` and scores candidates purely on
// packed-tile fill, so the best shape wins regardless of participant count.

export type GalleryArrangement = 'auto' | 'column' | 'row';

export interface GalleryGeometryOptions {
  /** Pixel gap between tiles, both axes. */
  gap?: number;
  /** width / height a single tile wants to render at. */
  tileAspect?: number;
  /** 'auto' searches every shape; 'column'/'row' force a single line. */
  arrangement?: GalleryArrangement;
  /** The layout last returned for this container, for hysteresis. */
  previous?: { count: number; columns: number; rows: number } | null;
  /** Previous shape is kept unless a candidate beats it by more than this
   * fraction of its score -- avoids reflow jitter from near-tied shapes. */
  switchThreshold?: number;
  /** 'column'/'row' arrangements shrink tiles to fit; below this they clamp
   * and report overflow instead of shrinking further. */
  minTileHeight?: number;
}

export interface GalleryGeometry {
  columns: number;
  rows: number;
  cellWidth: number;
  cellHeight: number;
  tileWidth: number;
  tileHeight: number;
  /** Fraction of the container area the packed tiles cover, 0..1. */
  fill: number;
  /** True when a forced column/row arrangement had to clamp tile size
   * instead of shrinking to fit -- the container needs to scroll. */
  overflow: boolean;
  compact: boolean;
  tiny: boolean;
}

interface ScoredCandidate {
  columns: number;
  rows: number;
  fill: number;
  score: number;
  tileWidth: number;
  tileHeight: number;
  cellWidth: number;
  cellHeight: number;
}

const DEFAULT_GAP = 18;
const DEFAULT_TILE_ASPECT = 16 / 9;
const DEFAULT_WIDTH = 720;
const DEFAULT_HEIGHT = 405;
const DEFAULT_SWITCH_THRESHOLD = 0.08;
const DEFAULT_MIN_TILE_HEIGHT = 96;
/** Fraction of a candidate's fill subtracted per empty (unfilled) cell. */
const EMPTY_CELL_PENALTY = 0.08;

function safeDimension(value: number, fallback: number): number {
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

function cellSize(columns: number, rows: number, width: number, height: number, gap: number) {
  return {
    width: Math.max(0, (width - gap * (columns - 1)) / columns),
    height: Math.max(0, (height - gap * (rows - 1)) / rows)
  };
}

function fittedTileSize(cellWidth: number, cellHeight: number, aspect: number) {
  const tileWidth = Math.min(cellWidth, cellHeight * aspect);
  return { width: tileWidth, height: tileWidth / aspect };
}

function densityFlags(cellWidth: number, cellHeight: number) {
  return {
    compact: cellWidth < 170 || cellHeight < 105,
    tiny: cellWidth < 132 || cellHeight < 82
  };
}

/**
 * Scores one column count for `count` tiles in a `width`x`height` container.
 * `fill` is the packed-tile area as a fraction of the container area; `score`
 * additionally penalizes empty trailing cells (a 3x3 grid holding 7 tiles has
 * 2 empty cells) so a shape with no waste is preferred at equal fill.
 */
export function scoreGalleryCandidate(
  count: number,
  columns: number,
  width: number,
  height: number,
  gap: number,
  aspect: number
): ScoredCandidate {
  const rows = Math.ceil(count / columns);
  const cell = cellSize(columns, rows, width, height, gap);
  const tile = fittedTileSize(cell.width, cell.height, aspect);
  const tileArea = tile.width * tile.height;
  const containerArea = width * height;
  const fill = containerArea > 0 ? (count * tileArea) / containerArea : 0;
  const emptyCells = columns * rows - count;
  const score = fill * (1 - EMPTY_CELL_PENALTY * emptyCells);
  return {
    columns,
    rows,
    fill,
    score,
    tileWidth: tile.width,
    tileHeight: tile.height,
    cellWidth: cell.width,
    cellHeight: cell.height
  };
}

/** |ln((c/r) / (w/h))| -- smaller means the candidate's grid shape is closer
 * to the container's own aspect ratio. Only used to break an exact score
 * tie, since score alone has no preference between two equally-full shapes. */
function aspectDistance(columns: number, rows: number, width: number, height: number): number {
  const gridAspect = columns / rows;
  const containerAspect = width / height;
  if (gridAspect <= 0 || containerAspect <= 0) return Infinity;
  return Math.abs(Math.log(gridAspect / containerAspect));
}

function searchBestCandidate(
  count: number,
  width: number,
  height: number,
  gap: number,
  aspect: number
): ScoredCandidate {
  let best = scoreGalleryCandidate(count, 1, width, height, gap, aspect);
  for (let columns = 2; columns <= count; columns += 1) {
    const candidate = scoreGalleryCandidate(count, columns, width, height, gap, aspect);
    if (candidate.score > best.score) {
      best = candidate;
    } else if (candidate.score === best.score) {
      const bestDist = aspectDistance(best.columns, best.rows, width, height);
      const candDist = aspectDistance(candidate.columns, candidate.rows, width, height);
      if (candDist < bestDist) best = candidate;
    }
  }
  return best;
}

function forcedLineCandidate(
  count: number,
  width: number,
  height: number,
  gap: number,
  aspect: number,
  arrangement: 'column' | 'row',
  minTileHeight: number
): GalleryGeometry {
  const columns = arrangement === 'column' ? 1 : count;
  const rows = arrangement === 'column' ? count : 1;
  const cell = cellSize(columns, rows, width, height, gap);
  const tile = fittedTileSize(cell.width, cell.height, aspect);
  let overflow = false;
  let tileWidth = tile.width;
  let tileHeight = tile.height;
  if (tileHeight < minTileHeight) {
    overflow = true;
    tileHeight = minTileHeight;
    tileWidth = minTileHeight * aspect;
  }
  const containerArea = width * height;
  const fill = containerArea > 0 ? Math.min(1, (count * tileWidth * tileHeight) / containerArea) : 0;
  const flags = densityFlags(cell.width, cell.height);
  return {
    columns,
    rows,
    cellWidth: cell.width,
    cellHeight: cell.height,
    tileWidth,
    tileHeight,
    fill,
    overflow,
    ...flags
  };
}

export function computeGalleryLayout(
  count: number,
  width: number,
  height: number,
  opts: GalleryGeometryOptions = {}
): GalleryGeometry {
  const gap = opts.gap ?? DEFAULT_GAP;
  const aspect = opts.tileAspect ?? DEFAULT_TILE_ASPECT;
  const arrangement = opts.arrangement ?? 'auto';
  const switchThreshold = opts.switchThreshold ?? DEFAULT_SWITCH_THRESHOLD;
  const minTileHeight = opts.minTileHeight ?? DEFAULT_MIN_TILE_HEIGHT;

  const safeCount = Math.max(0, Math.floor(count));
  const safeWidth = safeDimension(width, DEFAULT_WIDTH);
  const safeHeight = safeDimension(height, DEFAULT_HEIGHT);

  if (safeCount <= 1) {
    const tile = fittedTileSize(safeWidth, safeHeight, aspect);
    const containerArea = safeWidth * safeHeight;
    const fill = containerArea > 0 ? (tile.width * tile.height) / containerArea : 0;
    return {
      columns: 1,
      rows: 1,
      cellWidth: safeWidth,
      cellHeight: safeHeight,
      tileWidth: tile.width,
      tileHeight: tile.height,
      fill,
      overflow: false,
      compact: safeWidth < 250 || safeHeight < 170,
      tiny: safeWidth < 190 || safeHeight < 130
    };
  }

  if (arrangement === 'column' || arrangement === 'row') {
    return forcedLineCandidate(safeCount, safeWidth, safeHeight, gap, aspect, arrangement, minTileHeight);
  }

  let chosen = searchBestCandidate(safeCount, safeWidth, safeHeight, gap, aspect);

  // Hysteresis: a previous layout for the SAME count is kept unless some
  // other shape clears it by more than `switchThreshold` -- otherwise a
  // near-tied shape can flip back and forth as the container resizes by a
  // pixel. A count change always repacks from scratch.
  const previous = opts.previous;
  if (previous && previous.count === safeCount) {
    const previousCandidate = scoreGalleryCandidate(
      safeCount,
      previous.columns,
      safeWidth,
      safeHeight,
      gap,
      aspect
    );
    const isValidPrevious = previous.columns >= 1 && previous.rows === Math.ceil(safeCount / previous.columns);
    if (isValidPrevious && chosen.score <= previousCandidate.score * (1 + switchThreshold)) {
      chosen = previousCandidate;
    }
  }

  const flags = densityFlags(chosen.cellWidth, chosen.cellHeight);
  return {
    columns: chosen.columns,
    rows: chosen.rows,
    cellWidth: chosen.cellWidth,
    cellHeight: chosen.cellHeight,
    tileWidth: chosen.tileWidth,
    tileHeight: chosen.tileHeight,
    fill: chosen.fill,
    overflow: false,
    ...flags
  };
}
