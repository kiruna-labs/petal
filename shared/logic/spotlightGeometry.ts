// #239 spotlight geometry: where the thumbnail strip goes (beside the hero or
// in rows below it) and how big the hero and the thumbnails are. One function
// for BOTH clients -- the web meeting grid (web-harness/src/tileLayout.ts) and
// the desktop gallery (apps/desktop/src/lib/components/Gallery.svelte) -- so a
// spotlight looks the same in a browser and in the app at the same size. Pure:
// no DOM; callers measure their surface and place the boxes.

/** Thumbnails are 16:9 boxes, like the packed grid. The hero is not: it
 * takes the shape of what it shows (SpotlightHeroMedia). */
const SPOTLIGHT_THUMBNAIL_ASPECT = 16 / 9;
/** The narrowest a thumbnail may get: below it a camera is unrecognisable
 * and the strip reads as a sliver (the landscape-phone strip was ~24px tall
 * before #239). The floor scales a little with the surface. */
const SPOTLIGHT_MIN_THUMBNAIL = 112;
/** `wrapSideStrip` only: thumbnails in a short bar beside the hero may go a
 * little smaller (96px wide, 54px tall) before the strip falls back to one
 * scrolling column -- several faces in view beat one and a half. */
const SPOTLIGHT_MIN_WRAPPED_THUMBNAIL = 96;
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
/** The web strip's 1px padding on each edge (web-harness style.css
 * `.spotlight-strip`); the desktop keeps the same 2px as breathing room. */
const SPOTLIGHT_STRIP_PADDING = 2;

/** What the hero shows: its media's width / height, and any header docked
 * above the media inside the tile. A shared window is whatever shape the
 * window is and carries its 44px header; a phone camera held upright is
 * 9:16. #248: a camera hero passes its full frame's aspect -- the box is
 * then exactly that shape, so the camera's crop rule (cameraFit.ts) shows
 * the whole frame, cropping nothing. Offering the hero a crop range instead
 * let the placement below trade a smaller, cropped hero for a side strip
 * (Pixel 8 portrait, 9:16 camera: 272x689 cropped by 30% vs 347x616 whole). */
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
  /** Thumbnails per row in the strip. Always 1 for 'side' unless the caller
   * asked for `wrapSideStrip`; 'below' wraps however many fit a row. */
  stripColumns: number;
}

export interface SpotlightGeometryOptions {
  /** A side strip too tall for one column may become several columns beside
   * the hero instead of scrolling -- for a short, wide window (a bar of
   * faces), where one column showed a thumbnail and a half. The thumbnails
   * still share one size, never below the base floor. The desktop gallery
   * asks for it; the web client keeps its scrolling strip. */
  wrapSideStrip?: boolean;
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

/** `wrapSideStrip`: the hero keeps the full height and the thumbnails fill
 * the width it leaves, in as few rows as give them their size -- one row
 * unless more rows buy clearly bigger thumbnails. Null when they would fall
 * below SPOTLIGHT_MIN_WRAPPED_THUMBNAIL (the single scrolling column stays). */
function wrappedSideStrip(
  count: number,
  width: number,
  height: number,
  gap: number,
  hero: SpotlightHeroMedia
): SpotlightGeometry | null {
  const fullHeightHero = fitSpotlightHero(width, height, hero);
  const room = width - fullHeightHero.width - gap - SPOTLIGHT_STRIP_PADDING;
  const cap = thumbnailCapForHero(fullHeightHero.mediaArea);
  let best: { width: number; columns: number } | null = null;
  for (let rows = 1; rows <= count; rows += 1) {
    const columns = Math.ceil(count / rows);
    const fit = Math.min(
      cap,
      (room - gap * (columns - 1)) / columns,
      ((height - SPOTLIGHT_STRIP_PADDING - gap * (rows - 1)) / rows) * SPOTLIGHT_THUMBNAIL_ASPECT
    );
    if (!best || fit > best.width * 1.05) best = { width: fit, columns };
  }
  if (!best || best.width < SPOTLIGHT_MIN_WRAPPED_THUMBNAIL) return null;
  const stripSize = best.columns * best.width + gap * (best.columns - 1) + SPOTLIGHT_STRIP_PADDING;
  const heroBox = fitSpotlightHero(width - stripSize - gap, height, hero);
  return {
    placement: 'side',
    heroWidth: heroBox.width,
    heroHeight: heroBox.height,
    stripSize,
    thumbnailWidth: best.width,
    thumbnailHeight: best.width / SPOTLIGHT_THUMBNAIL_ASPECT,
    stripColumns: best.columns,
  };
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
  hero: SpotlightHeroMedia = DEFAULT_SPOTLIGHT_HERO_MEDIA,
  options: SpotlightGeometryOptions = {}
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
      stripColumns: 0,
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
    const thumbnailHeight = thumbnailWidth / SPOTLIGHT_THUMBNAIL_ASPECT;
    const columnScrolls =
      thumbnailCount * thumbnailHeight + (thumbnailCount - 1) * gap + SPOTLIGHT_STRIP_PADDING > height + 0.5;
    const wrapped = options.wrapSideStrip && columnScrolls
      ? wrappedSideStrip(thumbnailCount, width, height, gap, hero)
      : null;
    if (wrapped) return wrapped;
    return {
      placement: 'side',
      heroWidth: sideHero.width,
      heroHeight: sideHero.height,
      stripSize: sideStrip,
      thumbnailWidth,
      thumbnailHeight,
      stripColumns: 1,
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
    stripColumns: columns,
  };
}
