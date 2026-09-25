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

/** Thumbnails are 16:9 boxes, like the packed grid. The hero is not: it
 * takes the shape of what it shows (SpotlightHeroMedia). */
const SPOTLIGHT_THUMBNAIL_ASPECT = 16 / 9;
/** The narrowest a thumbnail may get: below it a camera is unrecognisable
 * and the strip reads as a sliver (the landscape-phone strip was ~24px tall
 * before #239). The floor scales a little with the surface. */
const SPOTLIGHT_MIN_THUMBNAIL = 112;
const SPOTLIGHT_MAX_MIN_THUMBNAIL = 200;
/** Thumbnails in rows under the hero are no wider than this, nor than 70%
 * of the surface... */
const SPOTLIGHT_MAX_ROW_THUMBNAIL = 280;
/** ...and a side strip no wider than this, or 30% of the surface. */
const SPOTLIGHT_MAX_SIDE_STRIP = 320;
/** Above the floor, no thumbnail gets more than this share of the hero's
 * VIDEO area: the hero is always the biggest picture on screen. */
const SPOTLIGHT_THUMBNAIL_SHARE_OF_HERO = 0.5;
/** The strip goes on the side (as the picker icon draws it) unless that
 * leaves the hero with less than this share of the video area it would get
 * with the strip below it. */
const SPOTLIGHT_SIDE_PREFERENCE = 0.8;
/** `.spotlight-strip`'s 1px padding on each edge. */
const SPOTLIGHT_STRIP_PADDING = 2;
/** A shared window's header docks above its video inside the tile
 * (style.css: `.tile.has-remote-window-header video { top: 44px }`). */
const REMOTE_WINDOW_HEADER_PX = 44;

/** What the hero shows: its media's width / height, and any header docked
 * above the media inside the tile. A shared window is whatever shape the
 * window is and carries its 44px header; a phone camera held upright is
 * 9:16. #248's camera crop can hand in the cropped aspect here. */
export interface SpotlightHeroMedia {
  aspect: number;
  header: number;
}

export const DEFAULT_SPOTLIGHT_HERO_MEDIA: SpotlightHeroMedia = { aspect: 16 / 9, header: 0 };

export interface SpotlightGeometry {
  /** 'side': the strip is a column right of the hero; 'below': rows under it. */
  placement: 'side' | 'below';
  /** The hero tile's box, header included. */
  heroWidth: number;
  heroHeight: number;
  /** The strip's track: its width when 'side', its height when 'below'. */
  stripSize: number;
  /** One width for EVERY thumbnail, so the local self-view is never smaller
   * than anyone else's (#239), with or without a shared window as hero. */
  thumbnailWidth: number;
  thumbnailHeight: number;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}

/** The hero box that fits `width` x `height`, and the video area inside it. */
function fitSpotlightHero(width: number, height: number, hero: SpotlightHeroMedia) {
  const mediaWidth = Math.max(0, Math.min(width, (height - hero.header) * hero.aspect));
  const mediaHeight = mediaWidth / hero.aspect;
  return {
    width: mediaWidth,
    height: mediaWidth > 0 ? mediaHeight + hero.header : 0,
    mediaArea: mediaWidth * mediaHeight,
  };
}

/** The widest 16:9 thumbnail that stays under the hero-dominance share. */
function thumbnailCapForHero(heroMediaArea: number): number {
  return Math.sqrt(SPOTLIGHT_THUMBNAIL_SHARE_OF_HERO * heroMediaArea * SPOTLIGHT_THUMBNAIL_ASPECT);
}

/**
 * #239: where the spotlight strip goes and how big everything is, for
 * `thumbnailCount` thumbnails on a `width` x `height` surface (the tiles'
 * content box) around a hero showing `hero`. Pure, so the node tests pin it
 * without a layout engine.
 *
 * Side: the hero takes the full height and the strip the width left over
 * (never below the thumbnail floor). Below: the hero takes all the height
 * but one floor-sized row of thumbnails, and the thumbnails fill whatever
 * the hero's own shape leaves, as large as they can be while all of them
 * still fit. The side wins unless it costs the hero much more video than
 * below -- so a landscape phone, a tablet in landscape and a desktop window
 * all get the side strip, and a phone or tablet in portrait gets rows.
 * Thumbnails never shrink below the floor (past it the strip scrolls, see
 * `.spotlight-strip`) and never outgrow the hero's video.
 */
export function computeSpotlightGeometry(
  thumbnailCount: number,
  width: number,
  height: number,
  gap: number,
  hero: SpotlightHeroMedia = DEFAULT_SPOTLIGHT_HERO_MEDIA
): SpotlightGeometry {
  if (thumbnailCount <= 0) {
    const solo = fitSpotlightHero(width, height, hero);
    return {
      placement: 'below',
      heroWidth: solo.width,
      heroHeight: solo.height,
      stripSize: 0,
      thumbnailWidth: 0,
      thumbnailHeight: 0,
    };
  }
  const minThumbnail = clamp(width * 0.16, SPOTLIGHT_MIN_THUMBNAIL, SPOTLIGHT_MAX_MIN_THUMBNAIL);

  const maxSideStrip = Math.max(minThumbnail, Math.min(SPOTLIGHT_MAX_SIDE_STRIP, width * 0.3));
  let sideStrip = clamp(width - (height - hero.header) * hero.aspect - gap, minThumbnail, maxSideStrip);
  let sideHero = fitSpotlightHero(width - sideStrip - gap, height, hero);
  const sideCap = thumbnailCapForHero(sideHero.mediaArea) + SPOTLIGHT_STRIP_PADDING;
  if (sideStrip > sideCap) {
    sideStrip = Math.max(minThumbnail, sideCap);
    sideHero = fitSpotlightHero(width - sideStrip - gap, height, hero);
  }
  const belowHero = fitSpotlightHero(
    width,
    height - gap - minThumbnail / SPOTLIGHT_THUMBNAIL_ASPECT - SPOTLIGHT_STRIP_PADDING,
    hero
  );

  if (sideHero.mediaArea >= SPOTLIGHT_SIDE_PREFERENCE * belowHero.mediaArea) {
    const thumbnailWidth = sideStrip - SPOTLIGHT_STRIP_PADDING;
    return {
      placement: 'side',
      heroWidth: sideHero.width,
      heroHeight: sideHero.height,
      stripSize: sideStrip,
      thumbnailWidth,
      thumbnailHeight: thumbnailWidth / SPOTLIGHT_THUMBNAIL_ASPECT,
    };
  }

  // The largest thumbnail at which every row fits under the hero. A near tie
  // goes to more columns: a compact block rather than a tower.
  const rowWidth = width - SPOTLIGHT_STRIP_PADDING;
  const rowsRoom = height - gap - belowHero.height - SPOTLIGHT_STRIP_PADDING;
  const maxThumbnail = Math.min(SPOTLIGHT_MAX_ROW_THUMBNAIL, width * 0.7, thumbnailCapForHero(belowHero.mediaArea));
  let columns = 1;
  let thumbnailWidth = 0;
  let widest = 0;
  for (let candidate = 1; candidate <= thumbnailCount; candidate += 1) {
    const rows = Math.ceil(thumbnailCount / candidate);
    const fit = Math.min(
      maxThumbnail,
      (rowWidth - gap * (candidate - 1)) / candidate,
      ((rowsRoom - gap * (rows - 1)) / rows) * SPOTLIGHT_THUMBNAIL_ASPECT
    );
    widest = Math.max(widest, fit);
    if (fit >= widest * 0.9) {
      columns = candidate;
      thumbnailWidth = fit;
    }
  }
  if (thumbnailWidth < minThumbnail) {
    // Too many to fit: floor-sized rows, and the strip scrolls.
    columns = Math.max(1, Math.floor((rowWidth + gap) / (minThumbnail + gap)));
    thumbnailWidth = Math.min((rowWidth - gap * (columns - 1)) / columns, Math.max(minThumbnail, maxThumbnail));
  }
  const thumbnailHeight = thumbnailWidth / SPOTLIGHT_THUMBNAIL_ASPECT;
  const rows = Math.ceil(thumbnailCount / columns);
  return {
    placement: 'below',
    heroWidth: belowHero.width,
    heroHeight: belowHero.height,
    stripSize: Math.min(
      rows * thumbnailHeight + (rows - 1) * gap + SPOTLIGHT_STRIP_PADDING,
      height - gap - belowHero.height
    ),
    thumbnailWidth,
    thumbnailHeight,
  };
}

/** #239: the spotlight hero's media, read off its tile (see SpotlightHeroMedia). */
export function spotlightHeroMedia(tile: HTMLElement | null): SpotlightHeroMedia {
  if (!tile) return DEFAULT_SPOTLIGHT_HERO_MEDIA;
  const video = tile.classList.contains('camera-off') ? null : tile.querySelector('video');
  const aspect =
    video && video.videoWidth > 0 && video.videoHeight > 0
      ? video.videoWidth / video.videoHeight
      : DEFAULT_SPOTLIGHT_HERO_MEDIA.aspect;
  const header = tile.classList.contains('has-remote-window-header') ? REMOTE_WINDOW_HEADER_PX : 0;
  return { aspect, header };
}

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
   * (the same measurement Gallery.svelte takes), plus the breakpoint gaps. */
  function tileSurfaceSize(): {
    width: number;
    height: number;
    gap: number | undefined;
    spotlightGap: number | undefined;
  } {
    const style = typeof getComputedStyle === 'function' ? getComputedStyle(tilesEl) : null;
    const width = Math.max(
      0,
      (tilesEl.clientWidth ?? 0) - cssPx(style?.paddingLeft) - cssPx(style?.paddingRight)
    );
    const height = Math.max(
      0,
      (tilesEl.clientHeight ?? 0) - cssPx(style?.paddingTop) - cssPx(style?.paddingBottom)
    );
    const positivePx = (name: string) => {
      const value = style?.getPropertyValue(name);
      const px = value ? cssPx(value) : 0;
      return px > 0 ? px : undefined;
    };
    return { width, height, gap: positivePx('--tile-gap'), spotlightGap: positivePx('--spotlight-strip-gap') };
  }

  /** Grid or spotlight, whichever the tiles element is showing. */
  function applyGeometry() {
    if (tilesEl.classList.contains('layout-spotlight')) {
      centerTailRow(0);
      applySpotlightGeometry();
    } else {
      applyGridGeometry();
    }
  }

  /**
   * #239: an incomplete last row sits centred under the full ones. The grid
   * runs on half-column tracks (style.css `.tiles`), so the row's first tile
   * starts after half the empty cells; the rest auto-place after it.
   * `columns` 0 clears it (spotlight places its tiles itself).
   */
  function centerTailRow(columns: number) {
    const tiles = Array.from(tilesEl.querySelectorAll<HTMLElement>('.tile'));
    const tail = columns > 0 ? tiles.length % columns : 0;
    tiles.forEach((tile, index) => {
      const start = tail > 0 && index === tiles.length - tail ? String(columns - tail + 1) : '';
      tile.style.setProperty('grid-column-start', start);
    });
  }

  /**
   * #239: size the spotlight from the surface -- strip side or below, hero
   * box, one thumbnail width for all -- as `--spotlight-*` vars plus
   * `.spotlight-side`. Safe with no layout engine: nothing is set.
   */
  function applySpotlightGeometry() {
    const { width, height, spotlightGap } = tileSurfaceSize();
    if (width <= 0 || height <= 0) return;
    const thumbnails = spotlightStrip?.querySelectorAll('.tile').length ?? 0;
    const hero = spotlightHeroMedia(tilesEl.querySelector<HTMLElement>('.tile.is-spotlight'));
    const geometry = computeSpotlightGeometry(thumbnails, width, height, spotlightGap ?? 14, hero);
    tilesEl.classList.toggle('spotlight-side', geometry.placement === 'side');
    tilesEl.style.setProperty('--spotlight-hero-width', `${geometry.heroWidth}px`);
    tilesEl.style.setProperty('--spotlight-hero-height', `${geometry.heroHeight}px`);
    tilesEl.style.setProperty('--spotlight-strip-size', `${geometry.stripSize}px`);
    tilesEl.style.setProperty('--spotlight-thumbnail-width', `${geometry.thumbnailWidth}px`);
  }

  /**
   * Pack the grid: count x surface -> `--gallery-*` on the tiles element.
   * Spotlight mode owns its own template (`.tiles.layout-spotlight`, sized by
   * applySpotlightGeometry), so it is left alone. Safe with no layout engine
   * (zero surface): nothing is set.
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
    // The half-column track count (style.css `.tiles`), precomputed: older
    // WebKit rejects `repeat(calc(...), ...)` and would drop the template.
    tilesEl.style.setProperty('--gallery-half-tracks', String(layout.columns * 2));
    tilesEl.style.setProperty('--gallery-rows', String(layout.rows));
    tilesEl.style.setProperty('--gallery-tile-width', `${layout.tileWidth}px`);
    tilesEl.style.setProperty('--gallery-tile-height', `${layout.tileHeight}px`);
    tilesEl.style.setProperty('--gallery-gap', `${layout.gap}px`);
    centerTailRow(layout.columns);
  }

  // A pure resize repacks without FLIP (the tiles are not moving between
  // slots, the slots are moving); joins, leaves and mode changes repack
  // inside `applyTileLayout`'s animated mutation below. Guarded like
  // viewerDemand.ts: the harness also runs under node tests with no layout.
  if (typeof ResizeObserver === 'function') {
    const observer = new ResizeObserver(() => {
      tileReflow.withoutAnimation(applyGeometry);
    });
    observer.observe(tilesEl);
  }

  // #239: the hero's video reports its shape only once it loads, and a
  // shared window can change shape mid-share. Media events do not bubble,
  // so listen in the capture phase.
  const refitHeroMedia = (event: Event) => {
    if (!tilesEl.classList.contains('layout-spotlight')) return;
    const target = event.target as Element | null;
    if (!target?.closest?.('.tile.is-spotlight')) return;
    tileReflow.withoutAnimation(applySpotlightGeometry);
  };
  tilesEl.addEventListener('loadedmetadata', refitHeroMedia, true);
  tilesEl.addEventListener('resize', refitHeroMedia, true);

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
      tilesEl.classList.remove('spotlight-side');
      return;
    }

    if (!spotlightStrip || !tilesEl.contains(spotlightStrip)) {
      spotlightStrip = document.createElement('div');
      spotlightStrip.className = 'spotlight-strip';
      spotlightStrip.setAttribute('aria-label', 'Other tiles');
    }
    tilesEl.prepend(spotlightStrip);

    // #239: your own camera leads the strip, so it is on screen however many
    // thumbnails the strip has to scroll through; the rest keep their order.
    const localIdentity = state.room?.localParticipant?.identity;
    const isSelfView = (tile: HTMLDivElement) =>
      localIdentity !== undefined && tile.dataset.owner === localIdentity && !tile.classList.contains('share-tile');
    const others = tiles.filter((tile) => tile !== spotlight);
    const thumbnails = [...others.filter(isSelfView), ...others.filter((tile) => !isSelfView(tile))];
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
      applyGeometry();
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
