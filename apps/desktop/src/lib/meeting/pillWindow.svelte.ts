// Gallery↔pill window management (issue #10 + #11 + #12).
//
// Extracted verbatim from /meeting/[room]/+page.svelte. Owns everything that
// touches `getCurrentWindow()`: the gallery/pill window mode, pill orientation,
// edge-flip, drag/resize, measure/min-size, and the onResized/onMoved
// listeners. Zero behavior change — the same constants, the same programmatic-
// guard suppression, the same browser-preview fallbacks.
//
// The main window is `transparent: true` (tauri.conf.json, shared with
// macOS); on Windows that transparency is DWM blur-behind, which DWM will
// not round — so gallery mode runs the window as a real opaque window with
// native DWM-drawn corners (src-tauri/src/windows_corner.rs), and pill mode
// flips it back to the transparent blur-behind state via `set_main_pill_mode`
// (the route strips its own background + the layout's app shell via
// body.pill-mode so ONLY the floating pill renders). macOS stays transparent
// + CSS-rounded the whole time. Gallery mode restores the remembered
// geometry. Resizing
// across GALLERY_BREAKPOINT is an ADDITIONAL switch trigger next to the
// explicit switcher buttons (#1).

import { tick } from 'svelte';
import { invoke } from '@tauri-apps/api/core';
import { type UnlistenFn } from '@tauri-apps/api/event';
import {
  getCurrentWindow,
  currentMonitor,
  availableMonitors,
  LogicalSize,
  PhysicalPosition
} from '@tauri-apps/api/window';
import {
  centeredPosition,
  clampedPosition,
  GALLERY_BREAKPOINT,
  GALLERY_MIN_HEIGHT,
  HOME_DEFAULT,
  HOME_MIN,
  MEETING_DEFAULT,
  clampMeetingWindowSize,
  loadMainWindowFrame,
  loadMainWindowSize,
  loadMeetingWindowFrame,
  loadMeetingWindowSize,
  loadPillWindowFrame,
  logicalToPhysicalSize,
  monitorForWindowFrame,
  programmaticResizeGuard,
  safeWindowPosition,
  saveMainWindowFrame,
  saveMeetingWindowFrame,
  savePillWindowFrame,
  type WindowFrame,
  type WindowSize
} from '$lib/data/windowGeometry';
import { COMMANDS, hasTauriBridge } from '$lib/ipc';
import { isWindows } from '$lib/platform';

export type PillResizeDirection =
  | 'East'
  | 'North'
  | 'NorthEast'
  | 'NorthWest'
  | 'South'
  | 'SouthEast'
  | 'SouthWest'
  | 'West';

/** Tight visual allowance around the pill. It must be large enough for the
 * avatar ring/control shadows not to clip, but still much smaller than the
 * old 24px transparent bubble that blocked desktop clicks. */
const PILL_MARGIN = 18;
/** Pill mode: drag-resizing meaningfully wider than the pill expands. */
const PILL_EXPAND_SLACK = 80;
/** #12: distance (logical px) from a work-area edge that triggers an
 * orientation flip, and the corner-zone hysteresis (no flapping). */
const EDGE_THRESHOLD = 48;
const EDGE_HYSTERESIS = 16;
/** Extra transparent host room used only while pill-mode popups are open.
 * This keeps the idle pill bounds tight while letting More/toasts render
 * outside the capsule without clipping. */
const PILL_POPUP_EXTRA = 260;

// ---- Shared window-geometry helpers (module scope) ---------------------
// Pure helpers used by both `createPillWindow` (gallery/pill switching) and
// the module-level `prepareMeetingWindow` (pre-navigation sizing from the
// main menu). They touch no closure state, so ONE geometry computation
// serves both sides — no drift between the menu entry and meeting paths.

const programmatic = programmaticResizeGuard;

async function setCurrentWindowSize(win: ReturnType<typeof getCurrentWindow>, target: WindowSize) {
  // #14: meeting route geometry changes must not use the native resize
  // animation. It makes join/leave and pill/gallery transitions fly across
  // the screen instead of swapping surfaces in place.
  await win.setSize(new LogicalSize(target.width, target.height));
}

async function setCurrentWindowResizable(
  win: ReturnType<typeof getCurrentWindow>,
  resizable: boolean
) {
  const maybeResizable = win as typeof win & {
    setResizable?: (value: boolean) => Promise<void>;
  };
  try {
    await maybeResizable.setResizable?.(resizable);
  } catch {
    // Older/preview window APIs may not expose runtime resizability.
  }
}

async function currentWindowFrame(win: ReturnType<typeof getCurrentWindow>): Promise<WindowFrame> {
  const sf = await win.scaleFactor();
  const [size, pos] = await Promise.all([win.innerSize(), win.outerPosition()]);
  return {
    width: size.width / sf,
    height: size.height / sf,
    x: pos.x,
    y: pos.y
  };
}

async function safePositionForPhysicalFrame(pos: { x: number; y: number }, size: WindowSize) {
  const [monitors, current] = await Promise.all([
    availableMonitors(),
    currentMonitor().catch(() => null)
  ]);
  return safeWindowPosition(pos, size, monitors, current);
}

async function safePositionForLogicalFrame(
  win: ReturnType<typeof getCurrentWindow>,
  frame: WindowFrame,
  size: WindowSize
) {
  const scale = (await currentMonitor().catch(() => null))?.scaleFactor ?? (await win.scaleFactor());
  return safePositionForPhysicalFrame(frame, logicalToPhysicalSize(size, scale));
}

async function clampCurrentWindowToWorkArea(win: ReturnType<typeof getCurrentWindow>) {
  const pos = await win.outerPosition();
  const size = await win.outerSize();
  const safe = await safePositionForPhysicalFrame(pos, size);
  if (safe.changed) await win.setPosition(new PhysicalPosition(safe.x, safe.y));
}

async function centerCurrentWindowOnMonitor(win: ReturnType<typeof getCurrentWindow>, size: WindowSize) {
  const mon = await currentMonitor().catch(() => null);
  if (!mon) {
    await clampCurrentWindowToWorkArea(win);
    return;
  }
  const physicalSize = logicalToPhysicalSize(size, mon.scaleFactor ?? (await win.scaleFactor()));
  const centered = centeredPosition(physicalSize, mon);
  const safe = clampedPosition(centered, physicalSize, mon);
  await win.setPosition(new PhysicalPosition(safe.x, safe.y));
}

/** Shared target-geometry decision for the meeting window: remembered
 * gallery frame, persisted meeting frame, persisted meeting size, or the
 * default grown from the current size — then size + position the window.
 * Returns the applied frame. */
async function applyMeetingWindowGeometry(
  win: ReturnType<typeof getCurrentWindow>,
  remembered: WindowFrame | null
): Promise<WindowFrame> {
  const savedFrame = loadMeetingWindowFrame();
  const saved = loadMeetingWindowSize();
  if (remembered) {
    const target = clampMeetingWindowSize(remembered);
    await setCurrentWindowSize(win, target);
    const safe = await safePositionForLogicalFrame(win, remembered, target);
    await win.setPosition(new PhysicalPosition(safe.x, safe.y));
    const applied = { ...remembered, ...target, x: safe.x, y: safe.y };
    saveMeetingWindowFrame(applied);
    return applied;
  }
  if (savedFrame) {
    const target = clampMeetingWindowSize(savedFrame);
    await setCurrentWindowSize(win, target);
    const safe = await safePositionForLogicalFrame(win, savedFrame, target);
    await win.setPosition(new PhysicalPosition(safe.x, safe.y));
    const applied = { ...savedFrame, ...target, x: safe.x, y: safe.y };
    saveMeetingWindowFrame(applied);
    return applied;
  }
  if (saved) {
    await setCurrentWindowSize(win, saved);
    await centerCurrentWindowOnMonitor(win, saved);
    return { ...saved, ...(await currentWindowFrame(win)) };
  }
  // #10: entering a meeting while narrower than the one-row control bar
  // grows the window to fit.
  const sf = await win.scaleFactor();
  const size = await win.innerSize();
  const w = size.width / sf;
  const h = size.height / sf;
  const target = clampMeetingWindowSize({
    width: Math.max(w, MEETING_DEFAULT.width),
    height: Math.max(h, MEETING_DEFAULT.height)
  });
  if (w < target.width || h < target.height) await setCurrentWindowSize(win, target);
  await centerCurrentWindowOnMonitor(win, target);
  return { ...target, ...(await currentWindowFrame(win)) };
}

/** Pre-size the main window to the meeting geometry BEFORE navigating to
 * /meeting (called from the main-menu join flow), so the route swap
 * happens at a constant window size — the native resize delta can never
 * expose the desktop behind the transparent window mid-transition.
 * Idempotent: the meeting route's own enterGalleryWindow re-applies the
 * same frame as a no-op. */
export async function prepareMeetingWindow(): Promise<void> {
  try {
    const win = getCurrentWindow();
    await programmatic.run(async () => {
      await setCurrentWindowResizable(win, true);
      await win.setShadow(true);
      await win.setMinSize(new LogicalSize(HOME_MIN.width, HOME_MIN.height));
      const applied = await applyMeetingWindowGeometry(win, null);
      saveMeetingWindowFrame(applied);
    });
  } catch {
    // No Tauri bridge (plain browser preview) — nothing to size.
  }
}

export interface PillMeasurement {
  width: number;
  height: number;
}

export interface PillWindowAttachOptions {
  /** Measure the pill's current layout bounds (chromeRef.measurePill). */
  measurePill: () => PillMeasurement | null | undefined;
  /** Measure the pill's minimum content bounds (chromeRef.measurePillMinimum). */
  measurePillMinimum: () => PillMeasurement | null | undefined;
  /** Whether any pill-mode popup content (toasts / remote-control cards) is
   * open — folded into `pillPopupHostOpen` alongside the internal More menu. */
  popupContentOpen: () => boolean;
}

export interface PillWindow {
  /** Reactive: large gallery view (true) vs. compact pill (false). */
  expanded: boolean;
  readonly orientation: 'horizontal' | 'vertical';
  readonly pillPopupHostOpen: boolean;
  /** Set by MeetingChrome's pillHost.onCompactChange. */
  readonly pillExpandedByHover: boolean;
  setPillExpandedByHover(open: boolean): void;
  /** Set by MeetingChrome's pillHost.onPopupChange (More menu). */
  setPillMoreOpen(open: boolean): void;
  handlePillDrag(): void;
  handlePillResize(direction: PillResizeDirection): void;
  /** Wire up mode-switch effects + window listeners; returns nothing. Must be
   * called from onMount so listeners register. */
  attach(options: PillWindowAttachOptions): void;
  /** Restore the /main window geometry on leave. */
  restoreHomeWindow(): Promise<void>;
  /** Tear down all listeners + debounce timers (call from onDestroy). */
  dispose(): void;
}

export function createPillWindow(): PillWindow {
  const hasTauri = hasTauriBridge();

  /** Windows only: gallery mode runs the main window as a real opaque DWM
   * window with native rounded corners; pill mode needs the transparent
   * blur-behind window around the capsule. macOS never toggles — its window
   * is always transparent + CSS-rounded. Fires-and-forgets; the command is
   * idempotent. */
  function syncNativePillWindow(pill: boolean) {
    if (hasTauri && isWindows()) {
      void invoke(COMMANDS.setMainPillMode, { active: pill }).catch(() => {});
    }
  }

  let expanded = $state(true);
  let orientation = $state<'horizontal' | 'vertical'>('horizontal');
  let pillMoreOpen = $state(false);
  let pillExpandedByHover = $state(false);
  let pillPopupHostOpen = $state(false);

  /** Gallery geometry remembered on collapse, restored on expand. */
  let remembered: WindowFrame | null = null;
  /** Pill geometry remembered after same-session movement/resize. */
  let rememberedPill: WindowFrame | null = null;
  /** Current pill-mode window size (logical), for the expand-by-resize
   * threshold + per-orientation min-size. */
  let pillWindowSize: { width: number; height: number } | null = null;
  let unlistenResized: UnlistenFn | undefined;
  let unlistenMoved: UnlistenFn | undefined;
  let unlistenScaleChanged: UnlistenFn | undefined;
  let resizeDebounce: ReturnType<typeof setTimeout> | undefined;
  let moveDebounce: ReturnType<typeof setTimeout> | undefined;
  let monitorDebounce: ReturnType<typeof setTimeout> | undefined;

  let opts: PillWindowAttachOptions | undefined;

  async function resizePillHostWindow(
    win: ReturnType<typeof getCurrentWindow>,
    target: WindowSize,
    clamp = true,
    restorePosition?: { x: number; y: number }
  ): Promise<WindowFrame> {
    const sf = await win.scaleFactor();
    const before = await win.outerSize();
    const pos = await win.outerPosition();
    const afterW = Math.round(target.width * sf);
    const afterH = Math.round(target.height * sf);
    const dx = Math.round((afterW - before.width) / 2);
    const dy = Math.round((afterH - before.height) / 2);
    const next = restorePosition ?? { x: pos.x - dx, y: pos.y - dy };
    const safe = clamp
      ? await safePositionForPhysicalFrame(next, { width: afterW, height: afterH })
      : { x: next.x, y: next.y };

    await win.setSize(new LogicalSize(target.width, target.height));
    if (safe.x !== pos.x || safe.y !== pos.y) {
      await win.setPosition(new PhysicalPosition(safe.x, safe.y));
    }
    return { ...target, x: safe.x, y: safe.y };
  }

  async function restoredPillPosition(
    win: ReturnType<typeof getCurrentWindow>,
    target: WindowSize
  ): Promise<{ x: number; y: number } | undefined> {
    const saved = rememberedPill ?? loadPillWindowFrame();
    if (!saved) return undefined;
    const safe = await safePositionForLogicalFrame(win, saved, target);
    rememberedPill = { ...saved, ...target, x: safe.x, y: safe.y };
    return { x: safe.x, y: safe.y };
  }

  async function rememberGalleryFrame(win: ReturnType<typeof getCurrentWindow>) {
    const frame = await currentWindowFrame(win);
    const target = clampMeetingWindowSize(frame);
    remembered = { ...frame, ...target };
    saveMeetingWindowFrame(remembered);
  }

  async function rememberPillFrame(win: ReturnType<typeof getCurrentWindow>) {
    rememberedPill = await currentWindowFrame(win);
    savePillWindowFrame(rememberedPill);
  }

  async function rememberCurrentViewFrame(win: ReturnType<typeof getCurrentWindow>) {
    if (expanded) {
      await rememberGalleryFrame(win);
    } else {
      await rememberPillFrame(win);
    }
  }

  /** Pill window bounds = the pill's measured layout size + shadow margin. */
  function measurePillWindow(popup = false): { width: number; height: number } | null {
    const m = opts?.measurePill();
    if (!m || m.width === 0 || m.height === 0) return null;
    const base = {
      width: Math.ceil(m.width) + PILL_MARGIN * 2,
      height: Math.ceil(m.height) + PILL_MARGIN * 2
    };
    if (!popup) return base;
    if (orientation === 'vertical') {
      return {
        width: base.width + PILL_POPUP_EXTRA,
        height: Math.max(base.height, PILL_POPUP_EXTRA)
      };
    }
    return {
      width: Math.max(base.width, PILL_POPUP_EXTRA),
      height: base.height + PILL_POPUP_EXTRA
    };
  }

  /** Pill mode's minimum is content-derived, not a fixed 200px bubble:
   * identity + More + subtle expand switcher on the resizable axis. */
  async function applyPillMinSize(win: ReturnType<typeof getCurrentWindow>) {
    const minContent = opts?.measurePillMinimum();
    const natural = pillWindowSize;
    const minW = Math.ceil((minContent?.width ?? natural?.width ?? 58) + PILL_MARGIN * 2);
    const minH = Math.ceil((minContent?.height ?? natural?.height ?? 58) + PILL_MARGIN * 2);
    await win.setMinSize(new LogicalSize(minW, minH));
  }

  async function monitorForWindow(
    pos: { x: number; y: number },
    size: { width: number; height: number }
  ) {
    const [mons, mon] = await Promise.all([availableMonitors(), currentMonitor().catch(() => null)]);
    return monitorForWindowFrame(pos, size, mons, mon);
  }

  async function syncPillPopupHost(open: boolean) {
    const target = measurePillWindow(open);
    if (!target) return;
    try {
      const win = getCurrentWindow();
      await programmatic.run(async () => {
        await applyPillMinSize(win);
        const pos = await win.outerPosition();
        const frame = await resizePillHostWindow(win, target, true, pos);
        if (!open) savePillWindowFrame(frame);
      });
      if (!open) pillWindowSize = target;
    } catch {
      // No Tauri bridge (plain browser preview).
    }
  }

  async function enterGalleryWindow(first: boolean) {
    try {
      const win = getCurrentWindow();
      await programmatic.run(async () => {
        await setCurrentWindowResizable(win, true);
        await win.setShadow(true);
        await win.setMinSize(new LogicalSize(HOME_MIN.width, HOME_MIN.height));
        remembered = await applyMeetingWindowGeometry(win, first ? null : remembered);
      });
    } catch {
      // No Tauri bridge (plain browser preview).
    }
  }

  async function enterPillWindow() {
    try {
      const win = getCurrentWindow();
      try {
        await rememberGalleryFrame(win);
      } catch {
        remembered = null;
      }
      // Let the pill lay out (both stages stay mounted, so this is just a
      // frame for any pending name/width changes).
      await tick();
      await new Promise(requestAnimationFrame);
      const target = measurePillWindow();
      if (!target) return;
      pillWindowSize = target;
      await programmatic.run(async () => {
        await setCurrentWindowResizable(win, false);
        await win.setShadow(false);
        // Min first — the pill bounds are far below the gallery minimum.
        await applyPillMinSize(win);
        const frame = await resizePillHostWindow(win, target, true, await restoredPillPosition(win, target));
        rememberedPill = frame;
        savePillWindowFrame(frame);
      });
      // Settle pass (live testing 2026-07-14): resizing the window to
      // `target` changes stageWidth, which can flip MeetingChrome's overflow
      // fit (e.g. add a "More" circle) AFTER this measurement was taken --
      // the pill that actually renders post-resize can be wider than the
      // window it was just measured for, clipping ~8px off each side until
      // a hover forces a re-measure. Re-measure once more after the resize's
      // reactive effects have had a chance to settle, and resize again if it
      // changed. This converges: the second measurement reflects the
      // post-resize stageWidth, which doesn't change again on its own.
      await tick();
      await new Promise(requestAnimationFrame);
      const settled = measurePillWindow();
      if (settled && (settled.width !== target.width || settled.height !== target.height)) {
        pillWindowSize = settled;
        await programmatic.run(async () => {
          const frame = await resizePillHostWindow(win, settled, false, await restoredPillPosition(win, settled));
          rememberedPill = frame;
          savePillWindowFrame(frame);
        });
      }
    } catch {
      // No Tauri bridge (plain browser preview).
    }
  }

  /** Breakpoint switching (#11): gallery below breakpoint → pill; pill
   * drag-resized meaningfully wider than the pill → gallery. */
  function handleLogicalWidth(w: number) {
    if (expanded) {
      if (w < GALLERY_BREAKPOINT - 1) expanded = false;
    } else {
      // pillWindowSize is set by the Tauri collapse path; measure on demand
      // in the plain-browser preview (where that path never runs).
      const pw = pillWindowSize ?? measurePillWindow();
      if (pw && w > pw.width + PILL_EXPAND_SLACK) expanded = true;
    }
  }

  /** Browser-preview fallback: viewport width (window.innerWidth, CSS px ==
   * logical px) maps 1:1 to the Tauri window's logical inner width, so the
   * same constants drive the flip in a plain browser. */
  function browserResize() {
    if (resizeDebounce) clearTimeout(resizeDebounce);
    resizeDebounce = setTimeout(() => handleLogicalWidth(window.innerWidth), 150);
  }

  /** #12: edge detection — flip the pill vertical near left/right work-area
   * edges, horizontal near top/bottom; corners keep the current orientation
   * with hysteresis. */
  async function checkEdges() {
    if (expanded) return;
    try {
      const win = getCurrentWindow();
      let pos = await win.outerPosition();
      const size = await win.outerSize();
      const sf = await win.scaleFactor();
      const mon = await monitorForWindow(pos, size);
      if (!mon) return;
      const clamped = clampedPosition(pos, size, mon);
      if (clamped.changed) {
        await programmatic.run(async () => {
          await win.setPosition(new PhysicalPosition(clamped.x, clamped.y));
        });
        pos = new PhysicalPosition(clamped.x, clamped.y);
      }
      const wa = mon.workArea;
      const dLeft = pos.x - wa.position.x;
      const dRight = wa.position.x + wa.size.width - (pos.x + size.width);
      const dTop = pos.y - wa.position.y;
      const dBottom = wa.position.y + wa.size.height - (pos.y + size.height);
      const t = EDGE_THRESHOLD * sf;
      const hyst = EDGE_HYSTERESIS * sf;
      const lr = Math.min(dLeft, dRight);
      const tb = Math.min(dTop, dBottom);
      let want: 'horizontal' | 'vertical' | null = null;
      if (lr < t && tb >= t) want = 'vertical';
      else if (tb < t && lr >= t) want = 'horizontal';
      else if (lr < t && tb < t) {
        // Corner zone: keep the current orientation unless one edge is
        // CLEARLY nearer (hysteresis — no flapping).
        if (lr + hyst < tb) want = 'vertical';
        else if (tb + hyst < lr) want = 'horizontal';
      }
      if (want && want !== orientation) {
        await flipOrientation(want, { dLeft, dRight, dTop, dBottom });
      }
    } catch {
      // No Tauri bridge / monitor query failed — keep current orientation.
    }
  }

  /** Swap orientation and resize the window to the new pill bounds,
   * anchored so the pill keeps hugging the edge that triggered the flip. */
  async function flipOrientation(
    want: 'horizontal' | 'vertical',
    d: { dLeft: number; dRight: number; dTop: number; dBottom: number }
  ) {
    orientation = want;
    await tick();
    await new Promise(requestAnimationFrame);
    const target = measurePillWindow();
    if (!target) return;
    pillWindowSize = target;
    try {
      const win = getCurrentWindow();
      await programmatic.run(async () => {
        const sf = await win.scaleFactor();
        const pos = await win.outerPosition();
        const size = await win.outerSize();
        const pw = Math.round(target.width * sf);
        const ph = Math.round(target.height * sf);
        // Anchor the flip on the pill's CENTER by default, not a corner
        // (live testing 2026-07-14: keeping left/top fixed by default meant
        // swapping e.g. a 420x94 horizontal pill for a 94x420 vertical one
        // left ~326px of now-empty space where the old pill's right side
        // was, teleporting the pill that far from wherever the user's
        // cursor actually was mid-drag). Centering keeps the new pill close
        // to the same visual point the old one occupied on the axis that's
        // actually shrinking. Still hug a REAL nearby screen edge (checked
        // against the same EDGE_THRESHOLD checkEdges uses, not just
        // "whichever side happens to be closer") so the pill stays flush
        // against an edge it was genuinely pinned to.
        const t = EDGE_THRESHOLD * sf;
        const centerX = pos.x + size.width / 2;
        const centerY = pos.y + size.height / 2;
        let x = Math.round(centerX - pw / 2);
        let y = Math.round(centerY - ph / 2);
        if (want === 'vertical' && d.dRight < t) x = pos.x + size.width - pw;
        else if (want === 'vertical' && d.dLeft < t) x = pos.x;
        if (want === 'horizontal' && d.dBottom < t) y = pos.y + size.height - ph;
        else if (want === 'horizontal' && d.dTop < t) y = pos.y;
        const mon = await monitorForWindow({ x, y }, { width: pw, height: ph });
        const clamped = mon
          ? clampedPosition({ x, y }, { width: pw, height: ph }, mon)
          : { x, y };
        await applyPillMinSize(win);
        await resizePillHostWindow(win, target, false, { x: clamped.x, y: clamped.y });
        savePillWindowFrame({ ...target, x: clamped.x, y: clamped.y });
      });
    } catch {
      // No Tauri bridge (plain browser preview).
    }
  }

  /** Pill-body drag (explicit start_dragging on mousedown, same idiom as
   * the compositor header — buttons are excluded in MeetingChrome). */
  function handlePillDrag() {
    try {
      void getCurrentWindow().startDragging();
    } catch {
      // No Tauri bridge (plain browser preview).
    }
  }

  function handlePillResize(direction: PillResizeDirection) {
    try {
      void getCurrentWindow().startResizeDragging(direction);
    } catch {
      // No Tauri bridge (plain browser preview).
    }
  }

  /** Leaving the meeting (leave button, room-left event, or any plain
   * navigation): restore the home minimum and persisted /main size so /main
   * never inherits either the pill bounds or the meeting gallery bounds. */
  async function restoreHomeWindow() {
    try {
      const win = getCurrentWindow();
      try {
        // Only remember the current frame as gallery/pill geometry when the
        // window is NOT already home-sized. On a normal leave the
        // pre-restore (prepareReturnToHome) resizes to home BEFORE the swap,
        // so the onDestroy safety-net restore runs with the window at HOME
        // size — saving that as the meeting frame would clobber the real
        // gallery geometry and the NEXT join would open the meeting at the
        // home size (clamped to the gallery minimum).
        const frame = await currentWindowFrame(win);
        const home = loadMainWindowFrame() ?? loadMainWindowSize() ?? HOME_DEFAULT;
        const atHome =
          Math.round(frame.width) === Math.round(home.width) &&
          Math.round(frame.height) === Math.round(home.height);
        if (!atHome) {
          await rememberCurrentViewFrame(win);
        }
      } catch {
        // Best-effort only; the restore below must still run.
      }
      await programmatic.run(async () => {
        const savedFrame = loadMainWindowFrame();
        const target = savedFrame ?? loadMainWindowSize() ?? HOME_DEFAULT;
        await setCurrentWindowResizable(win, true);
        await win.setShadow(true);
        await win.setMinSize(new LogicalSize(HOME_MIN.width, HOME_MIN.height));
        await setCurrentWindowSize(win, target);
        if (savedFrame) {
          const safe = await safePositionForLogicalFrame(win, savedFrame, target);
          await win.setPosition(new PhysicalPosition(safe.x, safe.y));
        } else {
          await clampCurrentWindowToWorkArea(win);
        }
        const pos = await win.outerPosition();
        saveMainWindowFrame({ ...target, x: pos.x, y: pos.y });
      });
    } catch {
      // No Tauri bridge (plain browser preview).
    }
  }

  function scheduleMeetingWindowSafetyCheck(win: ReturnType<typeof getCurrentWindow>) {
    if (monitorDebounce) clearTimeout(monitorDebounce);
    monitorDebounce = setTimeout(async () => {
      if (programmatic.active()) return;
      try {
        if (expanded) {
          await programmatic.run(async () => {
            await clampCurrentWindowToWorkArea(win);
          });
        } else {
          await checkEdges();
        }
        await rememberCurrentViewFrame(win);
      } catch {
        // Window or monitor query went away.
      }
    }, 150);
  }

  function attach(options: PillWindowAttachOptions) {
    opts = options;

    // Apply window geometry whenever the mode actually changes (switcher
    // button OR breakpoint crossing — both just flip `expanded`).
    let lastApplied: boolean | undefined;
    $effect(() => {
      const exp = expanded;
      if (!hasTauri) return; // plain browser preview: in-page cross-fade only
      if (lastApplied === exp) return;
      const first = lastApplied === undefined;
      lastApplied = exp;
      if (exp) void enterGalleryWindow(first);
      else void enterPillWindow();
    });

    // Pill mode strips every opaque surface behind the pill (the route's
    // <main> + the layout's rounded app shell via body.pill-mode).
    // Runs in the browser preview too — it's pure CSS state.
    $effect(() => {
      const pill = !expanded;
      // Native side of the same toggle (Windows): gallery = opaque DWM window
      // with native corners, pill = transparent blur-behind. Deliberately NOT
      // in the cleanup below — the cleanup runs on every re-run (e.g. popup
      // changes) and would wrongly restore gallery mode mid-pill. The invoke
      // is driven only by `expanded`, so re-fires carry the same value and
      // are idempotent.
      syncNativePillWindow(pill);
      document.body.classList.toggle('pill-mode', pill);
      document.body.classList.toggle('pill-popup-mode', pillPopupHostOpen);
      return () => {
        document.body.classList.remove('pill-mode');
        document.body.classList.remove('pill-popup-mode');
      };
    });

    $effect(() => {
      pillPopupHostOpen = !expanded && (pillMoreOpen || options.popupContentOpen());
    });

    $effect(() => {
      const open = pillPopupHostOpen;
      const orient = orientation;
      const interactive = pillExpandedByHover;
      void orient;
      void interactive;
      if (!hasTauri || expanded) return;
      void syncPillPopupHost(open);
    });

    if (hasTauri) {
      try {
        const win = getCurrentWindow();
        win
          .onResized(({ payload }) => {
            if (programmatic.active()) return;
            if (resizeDebounce) clearTimeout(resizeDebounce);
            resizeDebounce = setTimeout(async () => {
              if (programmatic.active()) return;
              try {
                const sf = await win.scaleFactor();
                const pos = await win.outerPosition();
                const logical = { width: payload.width / sf, height: payload.height / sf };
                if (expanded) {
                  const target = clampMeetingWindowSize(logical);
                  remembered = { ...logical, ...target, x: pos.x, y: pos.y };
                  saveMeetingWindowFrame(remembered);
                } else {
                  savePillWindowFrame({ ...logical, x: pos.x, y: pos.y });
                }
                handleLogicalWidth(logical.width);
              } catch {
                // ignore — bridge went away
              }
            }, 150);
          })
          .then((un) => (unlistenResized = un))
          .catch(() => {});
        win
          .onMoved(() => {
            if (programmatic.active()) return;
            if (moveDebounce) clearTimeout(moveDebounce);
            moveDebounce = setTimeout(async () => {
              if (programmatic.active()) return;
              try {
                if (!expanded) await checkEdges();
                await rememberCurrentViewFrame(win);
              } catch {
                // ignore — bridge went away
              }
            }, 100);
          })
          .then((un) => (unlistenMoved = un))
          .catch(() => {});
        win
          .onScaleChanged(() => {
            scheduleMeetingWindowSafetyCheck(win);
          })
          .then((un) => (unlistenScaleChanged = un))
          .catch(() => {});
      } catch {
        // Listener registration failed — mode switching stays button-only.
      }
    } else {
      // Plain-browser preview: viewport resize is the window resize.
      window.addEventListener('resize', browserResize);
    }
  }

  function dispose() {
    unlistenResized?.();
    unlistenMoved?.();
    unlistenScaleChanged?.();
    if (resizeDebounce) clearTimeout(resizeDebounce);
    if (moveDebounce) clearTimeout(moveDebounce);
    if (monitorDebounce) clearTimeout(monitorDebounce);
    if (typeof window !== 'undefined') window.removeEventListener('resize', browserResize);
    if (typeof document !== 'undefined') {
      document.body.classList.remove('pill-mode');
      document.body.classList.remove('pill-popup-mode');
    }
    // Leaving the meeting (possibly from pill mode) must restore the opaque
    // native-rounded window (Windows only; no-op elsewhere).
    syncNativePillWindow(false);
  }

  return {
    get expanded() {
      return expanded;
    },
    set expanded(v: boolean) {
      expanded = v;
    },
    get orientation() {
      return orientation;
    },
    get pillPopupHostOpen() {
      return pillPopupHostOpen;
    },
    get pillExpandedByHover() {
      return pillExpandedByHover;
    },
    setPillExpandedByHover(open: boolean) {
      pillExpandedByHover = open;
    },
    setPillMoreOpen(open: boolean) {
      pillMoreOpen = open;
    },
    handlePillDrag,
    handlePillResize,
    attach,
    restoreHomeWindow,
    dispose
  };
}
