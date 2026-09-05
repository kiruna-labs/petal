export interface SmartGalleryLayout {
  columns: number;
  rows: number;
  cellWidth: number;
  cellHeight: number;
  tileWidth: number;
  tileHeight: number;
  compact: boolean;
  tiny: boolean;
}

const VIDEO_ASPECT = 16 / 9;
const DEFAULT_WIDTH = 720;
const DEFAULT_HEIGHT = 405;
const GRID_GAP = 18;

function availableCellSize(columns: number, rows: number, width: number, height: number) {
  return {
    width: Math.max(0, (width - GRID_GAP * (columns - 1)) / columns),
    height: Math.max(0, (height - GRID_GAP * (rows - 1)) / rows)
  };
}

function videoArea(columns: number, rows: number, width: number, height: number): number {
  const cell = availableCellSize(columns, rows, width, height);
  const { width: videoWidth, height: videoHeight } = fittedVideoSize(cell.width, cell.height);
  return videoWidth * videoHeight;
}

function fittedVideoSize(width: number, height: number) {
  const videoWidth = Math.min(width, height * VIDEO_ASPECT);
  return { width: videoWidth, height: videoWidth / VIDEO_ASPECT };
}

export function computeSmartGalleryLayout(
  participantCount: number,
  width = DEFAULT_WIDTH,
  height = DEFAULT_HEIGHT
): SmartGalleryLayout {
  const count = Math.max(0, Math.floor(participantCount));
  const safeWidth = Number.isFinite(width) && width > 0 ? width : DEFAULT_WIDTH;
  const safeHeight = Number.isFinite(height) && height > 0 ? height : DEFAULT_HEIGHT;

  if (count <= 1) {
    return {
      columns: 1,
      rows: 1,
      cellWidth: safeWidth,
      cellHeight: safeHeight,
      tileWidth: fittedVideoSize(safeWidth, safeHeight).width,
      tileHeight: fittedVideoSize(safeWidth, safeHeight).height,
      compact: safeWidth < 250 || safeHeight < 170,
      tiny: safeWidth < 190 || safeHeight < 130
    };
  }

  if (count === 2) {
    const sideBySide = safeWidth / safeHeight >= 1.15;
    const columns = sideBySide ? 2 : 1;
    const rows = sideBySide ? 1 : 2;
    const cell = availableCellSize(columns, rows, safeWidth, safeHeight);
    return {
      columns,
      rows,
      cellWidth: cell.width,
      cellHeight: cell.height,
      tileWidth: fittedVideoSize(cell.width, cell.height).width,
      tileHeight: fittedVideoSize(cell.width, cell.height).height,
      compact: cell.width < 170 || cell.height < 105,
      tiny: cell.width < 132 || cell.height < 82
    };
  }

  let best = count <= 4
    ? { columns: 2, rows: 2, area: videoArea(2, 2, safeWidth, safeHeight) }
    : { columns: 1, rows: count, area: 0 };

  if (count > 4) {
    for (let columns = 2; columns <= count; columns += 1) {
      const rows = Math.ceil(count / columns);
      const area = videoArea(columns, rows, safeWidth, safeHeight);
      const balancePenalty = Math.abs(columns / rows - safeWidth / safeHeight) * 0.035 * area;
      const emptyPenalty = (columns * rows - count) * 0.08 * area;
      const score = area - balancePenalty - emptyPenalty;
      if (score > best.area) best = { columns, rows, area: score };
    }
  }

  const cell = availableCellSize(best.columns, best.rows, safeWidth, safeHeight);
  const tile = fittedVideoSize(cell.width, cell.height);

  return {
    columns: best.columns,
    rows: best.rows,
    cellWidth: cell.width,
    cellHeight: cell.height,
    tileWidth: tile.width,
    tileHeight: tile.height,
    compact: cell.width < 170 || cell.height < 105,
    tiny: cell.width < 132 || cell.height < 82
  };
}
