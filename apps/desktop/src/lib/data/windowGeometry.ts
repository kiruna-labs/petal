import { browserStorage, STORAGE_KEYS, type StorageLike } from './storageKeys.ts';

export type { StorageLike } from './storageKeys.ts';

export interface WindowSize {
  width: number;
  height: number;
}

export interface WindowPosition {
  x: number;
  y: number;
}

export interface WindowFrame extends WindowSize, WindowPosition {}

export interface MonitorLike {
  position: WindowPosition;
  size: WindowSize;
  workArea: {
    position: WindowPosition;
    size: WindowSize;
  };
}

export const HOME_MIN: WindowSize = { width: 380, height: 560 };
export const HOME_DEFAULT: WindowSize = { width: 400, height: 640 };
export const GALLERY_BREAKPOINT = 520;
export const GALLERY_MIN_HEIGHT = 360;
export const MEETING_DEFAULT: WindowSize = { width: 840, height: 560 };
export const PILL_STORAGE_MIN: WindowSize = { width: 1, height: 1 };

export const MAIN_WINDOW_GEOMETRY_KEY = STORAGE_KEYS.mainWindowGeometry;
export const MEETING_WINDOW_GEOMETRY_KEY = STORAGE_KEYS.meetingWindowGeometry;
export const PILL_WINDOW_GEOMETRY_KEY = STORAGE_KEYS.pillWindowGeometry;
export const PROGRAMMATIC_RESIZE_SUPPRESSION_MS = 650;

type GeometryKind = 'main' | 'meeting' | 'pill';

function keyFor(kind: GeometryKind): string {
  if (kind === 'main') return MAIN_WINDOW_GEOMETRY_KEY;
  if (kind === 'meeting') return MEETING_WINDOW_GEOMETRY_KEY;
  return PILL_WINDOW_GEOMETRY_KEY;
}

function minFor(kind: GeometryKind): WindowSize {
  if (kind === 'main') return HOME_MIN;
  if (kind === 'meeting') return { width: GALLERY_BREAKPOINT, height: GALLERY_MIN_HEIGHT };
  return PILL_STORAGE_MIN;
}

function finitePositive(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value) && value > 0;
}

function finiteNumber(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value);
}

export function clampWindowSize(size: WindowSize, min: WindowSize): WindowSize {
  return {
    width: Math.max(Math.round(size.width), min.width),
    height: Math.max(Math.round(size.height), min.height)
  };
}

export function clampWindowFrame(frame: WindowFrame, min: WindowSize): WindowFrame {
  return {
    ...clampWindowSize(frame, min),
    x: Math.round(frame.x),
    y: Math.round(frame.y)
  };
}

export function clampMainWindowSize(size: WindowSize): WindowSize {
  return clampWindowSize(size, HOME_MIN);
}

export function mainRouteEntryResizeTarget(current: WindowSize): WindowSize | null {
  const clamped = clampMainWindowSize(current);
  if (clamped.width === Math.round(current.width) && clamped.height === Math.round(current.height)) {
    return null;
  }
  return clamped;
}

export function clampMeetingWindowSize(size: WindowSize): WindowSize {
  return clampWindowSize(size, { width: GALLERY_BREAKPOINT, height: GALLERY_MIN_HEIGHT });
}

export function logicalToPhysicalSize(size: WindowSize, scaleFactor: number): WindowSize {
  return {
    width: Math.round(size.width * scaleFactor),
    height: Math.round(size.height * scaleFactor)
  };
}

export function parseWindowSize(raw: string | null, min: WindowSize): WindowSize | null {
  if (!raw) return null;
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object') return null;
    const candidate = parsed as { width?: unknown; height?: unknown };
    if (!finitePositive(candidate.width) || !finitePositive(candidate.height)) return null;
    return clampWindowSize({ width: candidate.width, height: candidate.height }, min);
  } catch {
    return null;
  }
}

export function parseWindowFrame(raw: string | null, min: WindowSize): WindowFrame | null {
  if (!raw) return null;
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object') return null;
    const candidate = parsed as { width?: unknown; height?: unknown; x?: unknown; y?: unknown };
    if (!finitePositive(candidate.width) || !finitePositive(candidate.height)) return null;
    if (!finiteNumber(candidate.x) || !finiteNumber(candidate.y)) return null;
    return clampWindowFrame(
      {
        width: candidate.width,
        height: candidate.height,
        x: candidate.x,
        y: candidate.y
      },
      min
    );
  } catch {
    return null;
  }
}

export function loadWindowSize(
  kind: 'main' | 'meeting',
  storage: StorageLike | undefined = browserStorage()
): WindowSize | null {
  if (!storage) return null;
  try {
    return parseWindowSize(storage.getItem(keyFor(kind)), minFor(kind));
  } catch {
    return null;
  }
}

export function saveWindowSize(
  kind: 'main' | 'meeting',
  size: WindowSize,
  storage: StorageLike | undefined = browserStorage()
): boolean {
  if (!storage) return false;
  try {
    storage.setItem(keyFor(kind), JSON.stringify(clampWindowSize(size, minFor(kind))));
    return true;
  } catch {
    return false;
  }
}

export function loadWindowFrame(
  kind: GeometryKind,
  storage: StorageLike | undefined = browserStorage()
): WindowFrame | null {
  if (!storage) return null;
  try {
    return parseWindowFrame(storage.getItem(keyFor(kind)), minFor(kind));
  } catch {
    return null;
  }
}

export function saveWindowFrame(
  kind: GeometryKind,
  frame: WindowFrame,
  storage: StorageLike | undefined = browserStorage()
): boolean {
  if (!storage) return false;
  try {
    storage.setItem(keyFor(kind), JSON.stringify(clampWindowFrame(frame, minFor(kind))));
    return true;
  } catch {
    return false;
  }
}

export function loadMainWindowSize(storage?: StorageLike): WindowSize | null {
  return loadWindowSize('main', storage);
}

export function saveMainWindowSize(size: WindowSize, storage?: StorageLike): boolean {
  return saveWindowSize('main', size, storage);
}

export function loadMeetingWindowSize(storage?: StorageLike): WindowSize | null {
  return loadWindowSize('meeting', storage);
}

export function saveMeetingWindowSize(size: WindowSize, storage?: StorageLike): boolean {
  return saveWindowSize('meeting', size, storage);
}

export function loadMainWindowFrame(storage?: StorageLike): WindowFrame | null {
  return loadWindowFrame('main', storage);
}

export function saveMainWindowFrame(frame: WindowFrame, storage?: StorageLike): boolean {
  return saveWindowFrame('main', frame, storage);
}

export function loadMeetingWindowFrame(storage?: StorageLike): WindowFrame | null {
  return loadWindowFrame('meeting', storage);
}

export function saveMeetingWindowFrame(frame: WindowFrame, storage?: StorageLike): boolean {
  return saveWindowFrame('meeting', frame, storage);
}

export function loadPillWindowFrame(storage?: StorageLike): WindowFrame | null {
  return loadWindowFrame('pill', storage);
}

export function savePillWindowFrame(frame: WindowFrame, storage?: StorageLike): boolean {
  return saveWindowFrame('pill', frame, storage);
}

export function monitorContainsPoint(mon: MonitorLike, x: number, y: number) {
  return (
    x >= mon.position.x &&
    x < mon.position.x + mon.size.width &&
    y >= mon.position.y &&
    y < mon.position.y + mon.size.height
  );
}

export function distanceToWorkArea(mon: MonitorLike, x: number, y: number) {
  const wa = mon.workArea;
  const minX = wa.position.x;
  const maxX = wa.position.x + wa.size.width;
  const minY = wa.position.y;
  const maxY = wa.position.y + wa.size.height;
  const dx = x < minX ? minX - x : x > maxX ? x - maxX : 0;
  const dy = y < minY ? minY - y : y > maxY ? y - maxY : 0;
  return dx * dx + dy * dy;
}

export function monitorForWindowFrame(
  pos: WindowPosition,
  size: WindowSize,
  monitors: readonly MonitorLike[],
  current?: MonitorLike | null
): MonitorLike | undefined {
  const cx = pos.x + size.width / 2;
  const cy = pos.y + size.height / 2;
  const candidates = current ? [current, ...monitors] : [...monitors];
  const containing = candidates.find((m) => monitorContainsPoint(m, cx, cy));
  if (containing) return containing;
  return candidates.reduce<MonitorLike | undefined>((best, next) => {
    if (!best) return next;
    return distanceToWorkArea(next, cx, cy) < distanceToWorkArea(best, cx, cy) ? next : best;
  }, undefined);
}

export function clampedPosition(pos: WindowPosition, size: WindowSize, mon: MonitorLike) {
  const wa = mon.workArea;
  const maxX = wa.position.x + Math.max(0, wa.size.width - size.width);
  const maxY = wa.position.y + Math.max(0, wa.size.height - size.height);
  const x = Math.min(Math.max(Math.round(pos.x), wa.position.x), maxX);
  const y = Math.min(Math.max(Math.round(pos.y), wa.position.y), maxY);
  return { x, y, changed: x !== Math.round(pos.x) || y !== Math.round(pos.y) };
}

export function centeredPosition(size: WindowSize, mon: MonitorLike): WindowPosition {
  const wa = mon.workArea;
  return {
    x: wa.position.x + Math.max(0, Math.round((wa.size.width - size.width) / 2)),
    y: wa.position.y + Math.max(0, Math.round((wa.size.height - size.height) / 2))
  };
}

export function windowIntersectsWorkArea(
  pos: WindowPosition,
  size: WindowSize,
  mon: MonitorLike
): boolean {
  const wa = mon.workArea;
  const left = Math.max(pos.x, wa.position.x);
  const top = Math.max(pos.y, wa.position.y);
  const right = Math.min(pos.x + size.width, wa.position.x + wa.size.width);
  const bottom = Math.min(pos.y + size.height, wa.position.y + wa.size.height);
  return right > left && bottom > top;
}

export function windowIntersectsAnyWorkArea(
  pos: WindowPosition,
  size: WindowSize,
  monitors: readonly MonitorLike[]
): boolean {
  return monitors.some((mon) => windowIntersectsWorkArea(pos, size, mon));
}

export function safeWindowPosition(
  pos: WindowPosition,
  size: WindowSize,
  monitors: readonly MonitorLike[],
  current?: MonitorLike | null
) {
  const candidates = current ? [current, ...monitors] : [...monitors];
  const mon = monitorForWindowFrame(pos, size, monitors, current);
  if (!mon) return { x: Math.round(pos.x), y: Math.round(pos.y), changed: false, recentered: false };

  if (!windowIntersectsAnyWorkArea(pos, size, candidates)) {
    const centered = centeredPosition(size, mon);
    const clamped = clampedPosition(centered, size, mon);
    return { x: clamped.x, y: clamped.y, changed: true, recentered: true };
  }

  const clamped = clampedPosition(pos, size, mon);
  return { ...clamped, recentered: false };
}

export function createProgrammaticGuard() {
  let ops = 0;
  return {
    active() {
      return ops > 0;
    },
    async run(fn: () => Promise<void>) {
      ops += 1;
      try {
        await fn();
      } finally {
        setTimeout(() => {
          ops -= 1;
        }, PROGRAMMATIC_RESIZE_SUPPRESSION_MS);
      }
    }
  };
}

/** App-wide programmatic-resize guard shared by every window-geometry
 * controller (the /main menu route and the meeting pill window). A
 * programmatic resize from one side must suppress the resize/move
 * listeners of the other: the pre-navigation meeting pre-size runs while
 * the menu is still mounted, and the menu's own onResized/onMoved
 * persistence would otherwise record the meeting geometry as the
 * main-window frame — the next leave then "restores" the home window to
 * the meeting size. */
export const programmaticResizeGuard = createProgrammaticGuard();
