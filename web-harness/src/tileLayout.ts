import type { HarnessContext, HarnessState, TileLayoutMode } from './context';
import { HARNESS_TILE_LAYOUT_STORAGE_KEY } from './constants';
import { getTileReflowController } from './tileReflow.ts';
import { computeGalleryLayout } from '@petal/shared/logic/galleryGeometry';
import {
  autoSpotlight,
  chooseSpotlightHero,
  manualTileLayoutMode,
  type TileLayoutModeState,
  type TileLayoutModeTransition,
} from '@petal/shared/logic/tileLayoutMode';

// ---------------------------------------------------------------------------
// Tile layout: grid/spotlight picker + click-to-pin, plus active-speaker
// smoothing (LiveKit emits a changing ordered active-speaker list; ease scores
// toward/away from it so rings do not flicker).
//
// The mode transition RULES live in shared/logic/tileLayoutMode.ts (one source
// with the desktop gallery, #785). This module only adapts them to harness
// state + localStorage; do not re-decide "should this persist" here.
// ---------------------------------------------------------------------------

/** Harness state as the shared transition rules see it. */
export function layoutModeStateOf(state: HarnessState): TileLayoutModeState {
  return { mode: state.tileLayoutMode, restoreMode: state.autoSpotlightRestoreMode ?? null };
}

/**
 * Applies a shared transition. `persist === null` means an AUTOMATIC change:
 * it must never reach localStorage, or the user's explicit preference dies
 * with the next incoming share (#785).
 */
export function commitLayoutModeTransition(
  state: HarnessState,
  transition: TileLayoutModeTransition
) {
  state.tileLayoutMode = transition.state.mode;
  state.autoSpotlightRestoreMode = transition.state.restoreMode;
  if (transition.persist !== null) {
    localStorage.setItem(HARNESS_TILE_LAYOUT_STORAGE_KEY, transition.persist);
  }
}

export function setupTileLayout(ctx: HarnessContext) {
  const { dom, state } = ctx;
  const { tilesEl, topbarRight } = dom;
  const tileOrder = new WeakMap<HTMLDivElement, number>();
  let nextTileOrder = 0;
  let spotlightStrip: HTMLDivElement | null = null;
  const tileReflow = getTileReflowController(tilesEl);
  // #204 PR2: the web grid packs tiles with the SAME shared geometry the
  // desktop gallery uses (shared/logic/galleryGeometry.ts). CSS only places
  // the computed cells; the shape (columns x rows, tile px, gap) comes from
  // here. `lastGalleryLayout` feeds hysteresis so a near-tied shape does not
  // flip during a drag-resize.
  let lastGalleryLayout: { count: number; columns: number; rows: number } | null = null;

  function cssPx(value: string | null | undefined): number {
    const parsed = parseFloat(value ?? '');
    return Number.isFinite(parsed) ? parsed : 0;
  }

  /** The surface the packer may use: the grid's client box minus its padding
   * (the same measurement Gallery.svelte takes). */
  function tileSurfaceSize(): { width: number; height: number; gap: number | undefined } {
    const style = typeof getComputedStyle === 'function' ? getComputedStyle(tilesEl) : null;
    const width = Math.max(
      0,
      (tilesEl.clientWidth ?? 0) - cssPx(style?.paddingLeft) - cssPx(style?.paddingRight)
    );
    const height = Math.max(
      0,
      (tilesEl.clientHeight ?? 0) - cssPx(style?.paddingTop) - cssPx(style?.paddingBottom)
    );
    const gapValue = style?.getPropertyValue('--tile-gap');
    const gap = gapValue ? cssPx(gapValue) : undefined;
    return { width, height, gap: gap && gap > 0 ? gap : undefined };
  }

  /**
   * Pack the grid: count x surface -> `--gallery-*` on the tiles element.
   * Spotlight mode owns its own template (`.tiles.layout-spotlight`), so it
   * is left alone. Safe with no layout engine (zero surface): nothing is set.
   */
  function applyGridGeometry() {
    if (tilesEl.classList.contains('layout-spotlight')) return;
    const count = tilesEl.querySelectorAll('.tile').length;
    const { width, height, gap } = tileSurfaceSize();
    if (count === 0 || width <= 0 || height <= 0) return;
    const layout = computeGalleryLayout(count, width, height, {
      gap,
      arrangement: 'auto',
      previous: lastGalleryLayout,
    });
    lastGalleryLayout = { count, columns: layout.columns, rows: layout.rows };
    tilesEl.style.setProperty('--gallery-cols', String(layout.columns));
    tilesEl.style.setProperty('--gallery-rows', String(layout.rows));
    tilesEl.style.setProperty('--gallery-tile-width', `${layout.tileWidth}px`);
    tilesEl.style.setProperty('--gallery-tile-height', `${layout.tileHeight}px`);
    tilesEl.style.setProperty('--gallery-gap', `${layout.gap}px`);
  }

  // A pure resize repacks without FLIP (the tiles are not moving between
  // slots, the slots are moving); joins, leaves and mode changes repack
  // inside `applyTileLayout`'s animated mutation below. Guarded like
  // viewerDemand.ts: the harness also runs under node tests with no layout.
  if (typeof ResizeObserver === 'function') {
    const observer = new ResizeObserver(() => {
      tileReflow.withoutAnimation(applyGridGeometry);
    });
    observer.observe(tilesEl);
  }

  function rememberTileOrder(tile: HTMLDivElement) {
    if (tileOrder.has(tile)) return;

    const knownOrders = Array.from(
      tilesEl.querySelectorAll<HTMLDivElement>('.tile'),
      (candidate) => tileOrder.get(candidate)
    ).filter((order): order is number => order !== undefined);
    const directChildren = Array.from(tilesEl.children);
    const tileIndex = directChildren.indexOf(tile);
    const stripIndex = spotlightStrip ? directChildren.indexOf(spotlightStrip) : -1;
    const insertedBeforeStrip = tileIndex >= 0 && stripIndex >= 0 && tileIndex < stripIndex;
    const order =
      insertedBeforeStrip && knownOrders.length > 0
        ? Math.min(...knownOrders) - 1
        : nextTileOrder++;
    tileOrder.set(tile, order);
    nextTileOrder = Math.max(nextTileOrder, order + 1);
  }

  function tileElements(): HTMLDivElement[] {
    const tiles = Array.from(tilesEl.querySelectorAll<HTMLDivElement>('.tile'));
    tiles.forEach(rememberTileOrder);
    return tiles.sort((a, b) => tileOrder.get(a)! - tileOrder.get(b)!);
  }

  function shareTileCount(): number {
    return tilesEl.querySelectorAll('.share-tile').length;
  }

  /**
   * #785: the hero when the user has not pinned one. Ranked by
   * `chooseSpotlightHero`, whose whole job here is that the LOCAL self-view
   * never wins while a remote tile exists -- the old chain took the first
   * `.tile video` in DOM order, and the local camera tile is seeded first.
   */
  function defaultSpotlightTile(): HTMLDivElement | null {
    const localIdentity = state.room?.localParticipant?.identity;
    const hero = chooseSpotlightHero(
      tileElements().map((tile) => ({
        key: tile.id,
        tile,
        isShare: tile.classList.contains('share-tile'),
        hasVideo: tile.querySelector('video') !== null,
        isLocal: localIdentity !== undefined && tile.dataset.owner === localIdentity,
      }))
    );
    return hero?.tile ?? null;
  }

  function updateLayoutPickerState() {
    if (!state.layoutModeButtons) return;
    (Object.keys(state.layoutModeButtons) as TileLayoutMode[]).forEach((mode) => {
      const button = state.layoutModeButtons![mode];
      const active = state.tileLayoutMode === mode;
      button.classList.toggle('is-active', active);
      button.setAttribute('aria-pressed', active ? 'true' : 'false');
    });
  }

  function arrangeSpotlightTiles(tiles: HTMLDivElement[], spotlight: HTMLDivElement | null) {
    if (!spotlight) {
      tiles.forEach((tile) => {
        tile.classList.remove('is-spotlight-thumbnail');
        tilesEl.appendChild(tile);
      });
      spotlightStrip?.remove();
      spotlightStrip = null;
      tilesEl.classList.remove('spotlight-solo');
      return;
    }

    if (!spotlightStrip || !tilesEl.contains(spotlightStrip)) {
      spotlightStrip = document.createElement('div');
      spotlightStrip.className = 'spotlight-strip';
      spotlightStrip.setAttribute('aria-label', 'Other tiles');
    }
    tilesEl.prepend(spotlightStrip);

    const thumbnails = tiles.filter((tile) => tile !== spotlight);
    thumbnails.forEach((tile) => {
      tile.classList.add('is-spotlight-thumbnail');
      spotlightStrip!.appendChild(tile);
    });
    spotlight.classList.remove('is-spotlight-thumbnail');
    tilesEl.appendChild(spotlight);
    tilesEl.classList.toggle('spotlight-solo', thumbnails.length === 0);
  }

  function applyTileLayout() {
    const tiles = tileElements();
    if (state.tileLayoutMode === 'spotlight') {
      const pinned = state.pinnedTileId ? document.getElementById(state.pinnedTileId) : null;
      if (!pinned || !tilesEl.contains(pinned)) {
        state.pinnedTileId = defaultSpotlightTile()?.id ?? null;
      }
    }

    const spotlightActive =
      state.tileLayoutMode === 'spotlight' && tiles.length > 0 && state.pinnedTileId !== null;
    tileReflow.withAnimation(() => {
      tilesEl.classList.toggle('layout-spotlight', spotlightActive);
      tilesEl.classList.toggle('layout-grid', !spotlightActive);
      tiles.forEach((tile) => {
        const pinned = spotlightActive && tile.id === state.pinnedTileId;
        tile.classList.toggle('is-spotlight', pinned);
        tile.classList.toggle('is-pinnable', true);
        tile.title = pinned ? 'Spotlighted tile' : 'Click to spotlight';
      });
      const spotlight = spotlightActive
        ? (tiles.find((tile) => tile.id === state.pinnedTileId) ?? null)
        : null;
      arrangeSpotlightTiles(tiles, spotlight);
      applyGridGeometry();
    });
    tiles.forEach((tile) => {
      const pinned = spotlightActive && tile.id === state.pinnedTileId;
      if (spotlightActive && !pinned && !tile.classList.contains('remote-control-active')) {
        ctx.cb.fitTileLabels(tile);
      }
    });
    updateLayoutPickerState();
    applySpeakingRings();
  }

  function setTileLayoutMode(mode: TileLayoutMode) {
    commitLayoutModeTransition(state, manualTileLayoutMode(layoutModeStateOf(state), mode));
    if (mode === 'spotlight' && !state.pinnedTileId) {
      state.pinnedTileId = defaultSpotlightTile()?.id ?? null;
    }
    applyTileLayout();
  }

  // `source` is load-bearing, not a log tag (#785): an 'auto' pin records the
  // mode it left and writes no preference; a 'manual' pin is the user choosing
  // spotlight, so it persists and discards any pending restore.
  function pinTile(tile: HTMLDivElement, source: 'manual' | 'auto') {
    state.pinnedTileId = tile.id;
    const before = layoutModeStateOf(state);
    commitLayoutModeTransition(
      state,
      source === 'auto' ? autoSpotlight(before) : manualTileLayoutMode(before, 'spotlight')
    );
    applyTileLayout();
    if (source === 'manual') ctx.ui.logEvent(`spotlight pinned: ${tile.dataset.owner ?? tile.id}`);
  }

  function handleTilePinClick(event: MouseEvent) {
    const tile = event.currentTarget as HTMLDivElement;
    const target = event.target as Element | null;
    if (target?.closest('button, a, input, label, summary')) return;
    if (ctx.cb.activeRemoteControlForTile(tile)) return;
    pinTile(tile, 'manual');
  }

  function bindTileInteractions(tile: HTMLDivElement) {
    rememberTileOrder(tile);
    if (tile.dataset.tileInteractionsBound === '1') return;
    tile.dataset.tileInteractionsBound = '1';
    tile.addEventListener('click', handleTilePinClick);
  }

  function iconButtonSvg(mode: TileLayoutMode): string {
    if (mode === 'grid') {
      return [
        '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor"',
        'stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">',
        '<rect x="3" y="3" width="7" height="7" rx="1.5"></rect>',
        '<rect x="14" y="3" width="7" height="7" rx="1.5"></rect>',
        '<rect x="3" y="14" width="7" height="7" rx="1.5"></rect>',
        '<rect x="14" y="14" width="7" height="7" rx="1.5"></rect>',
        '</svg>',
      ].join(' ');
    }
    return [
      '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor"',
      'stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">',
      '<rect x="3" y="4" width="12" height="16" rx="2"></rect>',
      '<rect x="18" y="5" width="3" height="4" rx="1"></rect>',
      '<rect x="18" y="10" width="3" height="4" rx="1"></rect>',
      '<rect x="18" y="15" width="3" height="4" rx="1"></rect>',
      '</svg>',
    ].join(' ');
  }

  function makeLayoutModeButton(mode: TileLayoutMode, label: string): HTMLButtonElement {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'layout-mode-button';
    button.innerHTML = iconButtonSvg(mode);
    button.setAttribute('aria-label', label);
    button.title = label;
    button.addEventListener('click', () => setTileLayoutMode(mode));
    return button;
  }

  function installLayoutPicker() {
    const picker = document.createElement('div');
    picker.className = 'layout-picker';
    picker.setAttribute('aria-label', 'Tile layout');
    const gridButton = makeLayoutModeButton('grid', 'Grid view');
    const spotlightButton = makeLayoutModeButton('spotlight', 'Spotlight view');
    state.layoutModeButtons = { grid: gridButton, spotlight: spotlightButton };
    picker.append(gridButton, spotlightButton);
    topbarRight.insertBefore(picker, topbarRight.firstChild);
    applyTileLayout();
  }

  function applySpeakingRings() {
    tileElements().forEach((tile) => {
      const owner = tile.dataset.owner ?? '';
      const score = ctx.speakerScores.get(owner) ?? 0;
      tile.style.setProperty('--speaking-intensity', score.toFixed(2));
      tile.classList.toggle('is-speaking', score > 0.12);
    });
  }

  function smoothSpeakingScores() {
    const identities = new Set([...ctx.speakerScores.keys(), ...ctx.activeSpeakerTargets]);
    identities.forEach((identity) => {
      const current = ctx.speakerScores.get(identity) ?? 0;
      const target = ctx.activeSpeakerTargets.has(identity) ? 1 : 0;
      const next = target > current ? current + (target - current) * 0.45 : current * 0.72;
      if (next < 0.04 && target === 0) {
        ctx.speakerScores.delete(identity);
      } else {
        ctx.speakerScores.set(identity, next);
      }
    });
    applySpeakingRings();
  }

  function startSpeakerSmoothing() {
    if (state.speakerSmoothingTimer !== null) return;
    state.speakerSmoothingTimer = setInterval(smoothSpeakingScores, 160);
  }

  function resetActiveSpeakers() {
    ctx.activeSpeakerTargets.clear();
    ctx.speakerScores.clear();
    if (state.speakerSmoothingTimer !== null) {
      clearInterval(state.speakerSmoothingTimer);
      state.speakerSmoothingTimer = null;
    }
    applySpeakingRings();
  }

  return {
    applyTileLayout,
    applyGridGeometry,
    applySpeakingRings,
    startSpeakerSmoothing,
    smoothSpeakingScores,
    resetActiveSpeakers,
    shareTileCount,
    pinTile,
    bindTileInteractions,
    installLayoutPicker,
  };
}
