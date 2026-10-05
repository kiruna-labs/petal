// Rendered-geometry readout for one gallery lab window ($lib/dev/
// GalleryLabStage.svelte), and the rules a layout is judged by. Shared by the
// interactive /dev/gallery-lab route, scripts/verify-native-gallery-matrix.mjs
// and tests/galleryLabRendered.test.ts so all three judge a layout by the same
// numbers. Read-only: it measures the shipped Gallery and never styles it.

import type { GalleryLabScenario } from './galleryLab';

export interface LabRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

export interface LabTileReading {
  key: string;
  role: 'grid' | 'hero' | 'thumbnail';
  isLocal: boolean;
  rect: LabRect;
  /** Area of the tile on screen right now (inside the window and its scroller). */
  visibleArea: number;
  /** Area of the camera picture itself (object-fit aware), on screen. */
  videoArea: number;
  /** Fully visible, or one scroll of its own strip away. */
  reachable: boolean;
}

export interface GalleryLabReading {
  window: { width: number; height: number };
  mode: 'grid' | 'spotlight';
  /** `.gallery` class flags the layout chose (compact chrome, rail controls, …). */
  galleryClasses: string[];
  tiles: LabTileReading[];
  /** Tiles' on-screen area / window area. */
  tileShare: number;
  /** The box the tiles are packed into (`.tiles` content box): what the
   * chrome leaves for faces. */
  tileSurface: LabRect | null;
  /** Tile surface area / window area: how much of the window the chrome leaves. */
  surfaceShare: number;
  /** Tiles' area / tile surface area: how well they are packed into it. */
  packing: number;
  /** Camera pictures' on-screen area / window area. */
  videoShare: number;
  smallestTile: { width: number; height: number };
  largestTile: { width: number; height: number };
  /** Tiles drawn on top of each other (pairs of keys). */
  overlaps: Array<[string, string]>;
  /** The meeting itself scrolls (the route's .chrome-shell): never wanted. */
  pageScrolls: boolean;
  topbar: LabRect | null;
  controlbar: LabRect | null;
  /** Controls whose box is outside the window or clipped by the bar. */
  clippedControls: string[];
  /** Controls moved into More because the row did not fit. */
  collapsedControls: string[];
  /** Mic, Camera, Share and Leave are all on screen. */
  essentialControlsVisible: boolean;
  /** Spotlight: where the thumbnails are, beside the hero or in rows below it. */
  stripPlacement: 'side' | 'below' | null;
  /** Spotlight, when nothing scrolls: how far (px, + is right) the thumbnails
   * sit from centred under the hero ('below'), or the hero and its strip
   * together sit from centred in the window ('side'). Null otherwise. */
  stripCentreOffset: number | null;
  /** Spotlight: the local thumbnail is smaller than another thumbnail. */
  selfViewSmaller: boolean;
  /** Spotlight: the hero's area over the largest thumbnail's. */
  heroToThumbnailRatio: number | null;
}

function rectOf(el: Element, origin: DOMRect): LabRect {
  const r = el.getBoundingClientRect();
  return { left: r.left - origin.left, top: r.top - origin.top, width: r.width, height: r.height };
}

function intersect(a: LabRect, b: LabRect): LabRect {
  const left = Math.max(a.left, b.left);
  const top = Math.max(a.top, b.top);
  const right = Math.min(a.left + a.width, b.left + b.width);
  const bottom = Math.min(a.top + a.height, b.top + b.height);
  return { left, top, width: Math.max(0, right - left), height: Math.max(0, bottom - top) };
}

const area = (r: LabRect) => r.width * r.height;

function scrollerOf(el: Element, stop: Element): Element | null {
  for (let node = el.parentElement; node && node !== stop; node = node.parentElement) {
    const style = getComputedStyle(node);
    if (/(auto|scroll)/.test(`${style.overflowX} ${style.overflowY}`)) return node;
  }
  return null;
}

/** The camera picture's rect inside its <video>, honouring object-fit. */
function videoPictureRect(video: HTMLVideoElement, origin: DOMRect): LabRect | null {
  if (!video.videoWidth || !video.videoHeight) return null;
  const box = rectOf(video, origin);
  const fit = getComputedStyle(video).objectFit;
  const scale = fit === 'cover'
    ? Math.max(box.width / video.videoWidth, box.height / video.videoHeight)
    : Math.min(box.width / video.videoWidth, box.height / video.videoHeight);
  const width = video.videoWidth * scale;
  const height = video.videoHeight * scale;
  const picture = { left: box.left + (box.width - width) / 2, top: box.top + (box.height - height) / 2, width, height };
  // `cover` overhangs the element; only the element's box is ever painted.
  return fit === 'cover' ? intersect(picture, box) : picture;
}

export function measureGalleryLab(windowEl: HTMLElement): GalleryLabReading {
  const origin = windowEl.getBoundingClientRect();
  const windowRect: LabRect = { left: 0, top: 0, width: origin.width, height: origin.height };
  const gallery = windowEl.querySelector<HTMLElement>('.gallery');
  const tilesEl = windowEl.querySelector<HTMLElement>('.tiles');
  const spotlight = !!tilesEl?.classList.contains('spotlight');

  const tiles: LabTileReading[] = [];
  for (const wrap of Array.from(windowEl.querySelectorAll<HTMLElement>('.tile-wrap[data-participant-key]'))) {
    const rect = rectOf(wrap, origin);
    const scroller = scrollerOf(wrap, windowEl);
    const scrollBox = scroller ? intersect(rectOf(scroller, origin), windowRect) : windowRect;
    const visible = intersect(intersect(rect, windowRect), scrollBox);
    const video = wrap.querySelector<HTMLVideoElement>('video.video-el.ready');
    const picture = video ? videoPictureRect(video, origin) : null;
    const role = wrap.classList.contains('spotlight-main') ? 'hero' : wrap.classList.contains('spotlight-thumb') ? 'thumbnail' : 'grid';
    // Reachable: whole on screen, or inside a scroller whose cross axis shows
    // it whole (it just needs scrolling along the strip).
    const whole = area(visible) >= area(rect) - 1;
    let reachable = whole;
    if (!whole && scroller) {
      const sb = rectOf(scroller, origin);
      const insideCross = sb.height >= rect.height - 1 && rect.top >= sb.top - 1 && rect.top + rect.height <= sb.top + sb.height + 1;
      const insideCrossX = sb.width >= rect.width - 1 && rect.left >= sb.left - 1 && rect.left + rect.width <= sb.left + sb.width + 1;
      reachable = (insideCross || insideCrossX) && area(intersect(sb, windowRect)) >= area(sb) - 1;
    }
    tiles.push({
      key: wrap.dataset.participantKey ?? '',
      role,
      isLocal: (wrap.querySelector('.video-el')?.classList.contains('mirrored') ?? false) || wrap.getAttribute('aria-label')?.includes('(you)') === true,
      rect,
      visibleArea: area(visible),
      videoArea: picture ? area(intersect(intersect(picture, windowRect), scrollBox)) : 0,
      reachable
    });
  }

  const overlaps: Array<[string, string]> = [];
  for (let i = 0; i < tiles.length; i += 1) {
    for (let j = i + 1; j < tiles.length; j += 1) {
      const shared = intersect(tiles[i].rect, tiles[j].rect);
      if (shared.width > 1 && shared.height > 1) overlaps.push([tiles[i].key, tiles[j].key]);
    }
  }

  const windowArea = area(windowRect) || 1;
  let tileSurface: LabRect | null = null;
  if (tilesEl) {
    const r = rectOf(tilesEl, origin);
    const style = getComputedStyle(tilesEl);
    const padL = parseFloat(style.paddingLeft);
    const padT = parseFloat(style.paddingTop);
    tileSurface = {
      left: r.left + padL,
      top: r.top + padT,
      width: Math.max(0, r.width - padL - parseFloat(style.paddingRight)),
      height: Math.max(0, r.height - padT - parseFloat(style.paddingBottom))
    };
  }
  const tilesArea = tiles.reduce((sum, t) => sum + t.visibleArea, 0);
  const sizes = tiles.map((t) => t.rect);
  const smallest = sizes.reduce((min, r) => (area(r) < area(min) ? r : min), sizes[0] ?? { left: 0, top: 0, width: 0, height: 0 });
  const largest = sizes.reduce((max, r) => (area(r) > area(max) ? r : max), sizes[0] ?? { left: 0, top: 0, width: 0, height: 0 });

  const shell = windowEl.querySelector<HTMLElement>('.lab-chrome-shell, .chrome-shell');
  const pageScrolls = !!shell && (shell.scrollHeight > shell.clientHeight + 1 || shell.scrollWidth > shell.clientWidth + 1);

  const controlbarEl = windowEl.querySelector<HTMLElement>('.controlbar');
  const controlbar = controlbarEl ? rectOf(controlbarEl, origin) : null;
  const clippedControls: string[] = [];
  const collapsedControls: string[] = [];
  const visibleLabels = new Set<string>();
  for (const cell of Array.from(controlbarEl?.querySelectorAll<HTMLElement>('.control-cell') ?? [])) {
    const label = (cell.querySelector('.meeting-control-label')?.textContent ?? cell.dataset.control ?? '?').trim();
    if (getComputedStyle(cell).display === 'none') {
      collapsedControls.push(label);
      continue;
    }
    const r = rectOf(cell.querySelector('button') ?? cell, origin);
    const onScreen = area(intersect(r, windowRect)) >= area(r) - 1;
    const inBar = controlbar ? area(intersect(r, controlbar)) >= area(r) - 1 : false;
    if (!onScreen || !inBar) clippedControls.push(label);
    else visibleLabels.add(label);
  }
  const essentialControlsVisible = ['Mic', 'Camera', 'Share', 'Leave'].every((label) => visibleLabels.has(label));

  let stripCentreOffset: number | null = null;
  let stripPlacement: GalleryLabReading['stripPlacement'] = null;
  let selfViewSmaller = false;
  let heroToThumbnailRatio: number | null = null;
  const thumbs = tiles.filter((t) => t.role === 'thumbnail');
  if (spotlight && thumbs.length > 0) {
    const rail = windowEl.querySelector<HTMLElement>('.spotlight-rail');
    stripPlacement = rail?.classList.contains('side') ? 'side' : 'below';
    if (rail && rail.scrollWidth <= rail.clientWidth + 1 && rail.scrollHeight <= rail.clientHeight + 1) {
      const railRect = rectOf(rail, origin);
      const style = getComputedStyle(rail);
      const contentLeft = railRect.left + parseFloat(style.paddingLeft);
      const contentRight = railRect.left + railRect.width - parseFloat(style.paddingRight);
      const block = stripPlacement === 'side' ? tiles : thumbs;
      const blockLeft = Math.min(...block.map((t) => t.rect.left));
      const blockRight = Math.max(...block.map((t) => t.rect.left + t.rect.width));
      stripCentreOffset = ((blockLeft - contentLeft) - (contentRight - blockRight)) / 2;
    }
    const local = thumbs.find((t) => t.isLocal);
    if (local) selfViewSmaller = thumbs.some((t) => area(t.rect) > area(local.rect) + 2);
    const hero = tiles.find((t) => t.role === 'hero');
    const biggestThumb = Math.max(...thumbs.map((t) => area(t.rect)));
    if (hero && biggestThumb > 0) heroToThumbnailRatio = area(hero.rect) / biggestThumb;
  }

  return {
    window: { width: origin.width, height: origin.height },
    mode: spotlight ? 'spotlight' : 'grid',
    galleryClasses: Array.from(gallery?.classList ?? []).filter((c) => !c.startsWith('svelte-')),
    tiles,
    tileShare: tilesArea / windowArea,
    tileSurface,
    surfaceShare: tileSurface ? area(tileSurface) / windowArea : 0,
    packing: tileSurface && area(tileSurface) > 0 ? tilesArea / area(tileSurface) : 0,
    videoShare: tiles.reduce((sum, t) => sum + t.videoArea, 0) / windowArea,
    smallestTile: { width: smallest.width, height: smallest.height },
    largestTile: { width: largest.width, height: largest.height },
    overlaps,
    pageScrolls,
    topbar: (() => {
      const el = windowEl.querySelector('.topbar');
      return el ? rectOf(el, origin) : null;
    })(),
    controlbar,
    clippedControls,
    collapsedControls,
    essentialControlsVisible,
    stripPlacement,
    stripCentreOffset,
    selfViewSmaller,
    heroToThumbnailRatio
  };
}

/**
 * Everything wrong with one lab window, in words; empty when the layout is
 * fine. The rules (#239 parity for the desktop gallery):
 *  - the meeting never scrolls, tiles never overlap, and every tile is on
 *    screen or one scroll of the spotlight strip away;
 *  - Mic, Camera, Share and Leave are always on screen, and no control is
 *    clipped (a control may move into More, never off the bar);
 *  - the spotlight strip is centred, and your own thumbnail is never smaller
 *    than anyone else's;
 *  - every face is recognisable: no tile under 54px tall or 80px wide;
 *  - the window goes to faces: with nothing else open the tiles' box is at
 *    least 55% of it, and a grid fills at least 40% of that box;
 *  - the spotlight hero is clearly the biggest picture.
 */
export function judgeGalleryLab(scenario: GalleryLabScenario, reading: GalleryLabReading): string[] {
  const problems: string[] = [];
  if (reading.pageScrolls) problems.push('the meeting scrolls');
  if (reading.overlaps.length) problems.push(`tiles overlap: ${reading.overlaps.map((pair) => pair.join('/')).join(', ')}`);
  const unreachable = reading.tiles.filter((t) => !t.reachable).map((t) => t.key);
  if (unreachable.length) problems.push(`tiles off screen: ${unreachable.join(', ')}`);
  if (reading.tiles.length !== scenario.count) problems.push(`${reading.tiles.length} tiles for ${scenario.count} people`);
  if (reading.clippedControls.length) problems.push(`controls clipped: ${reading.clippedControls.join(', ')}`);
  if (!reading.essentialControlsVisible) problems.push('Mic, Camera, Share or Leave is not on screen');
  if (reading.stripCentreOffset !== null && Math.abs(reading.stripCentreOffset) > 2) {
    problems.push(`spotlight strip ${reading.stripCentreOffset.toFixed(1)}px off centre`);
  }
  if (reading.selfViewSmaller) problems.push('your own thumbnail is smaller than someone else’s');
  const tooSmall = reading.tiles
    .filter((t) => t.rect.height < 54 || t.rect.width < 80)
    .map((t) => `${t.key} ${Math.round(t.rect.width)}x${Math.round(t.rect.height)}`);
  if (tooSmall.length) problems.push(`tiles too small: ${tooSmall.join(', ')}`);
  if (!scenario.chatOpen && reading.surfaceShare < 0.55) {
    problems.push(`the chrome leaves ${(reading.surfaceShare * 100).toFixed(0)}% of the window for tiles (< 55%)`);
  }
  if (scenario.mode === 'grid' && scenario.count > 1 && reading.packing < 0.4) {
    problems.push(`tiles fill ${(reading.packing * 100).toFixed(0)}% of their box (< 40%)`);
  }
  if (scenario.mode === 'spotlight' && reading.heroToThumbnailRatio !== null && reading.heroToThumbnailRatio < 1.5) {
    problems.push(`hero only ${reading.heroToThumbnailRatio.toFixed(2)}x the largest thumbnail`);
  }
  return problems;
}
