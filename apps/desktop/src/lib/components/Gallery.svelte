<!--
  Gallery — the in-meeting full gallery. Per Petal-Build-Map.md §2.4 ("Full
  gallery — approved, subdued") + SPEC.md §4.7 (fluidly responsive, no reflow
  jank). Lays out ParticipantTiles in a responsive grid and renders the
  control bar below it (Mute, Start Video, Sharing, More, Invite, Leave —
  exact set + order from canvas.html's control-bar row).

  Responsive approach: the gallery measures itself and lays out for any
  window shape -- tiles packed by the shared geometry (galleryGeometry.ts,
  spotlightGeometry.ts, one source with the web client), and the chrome
  adapting to a narrow column of faces or a short bar of faces (see "Window
  shape -> chrome" in the script). Every shape is rendered and judged by the
  layout lab: scripts/verify-native-gallery-matrix.mjs, /dev/gallery-lab and
  tests/galleryLabRendered.test.ts.

  Control label is "Invite", not "Participants" (Build-Map §3 override #3).
  Labels name the FEATURE (Audio, Video, Screensharing, Invite, Leave —
  issue #6), not the current state; state is conveyed by each button's
  visual on/off/danger treatment, while aria-labels stay state-descriptive
  ("Mute microphone" etc.). No permanent "More" button — the gallery has
  room for all five; "More" only exists as MeetingChrome's compact-pill
  overflow affordance.
-->
<script lang="ts">
  import { onMount, tick, type Snippet } from 'svelte';
  import { fade } from 'svelte/transition';
  import ParticipantTile from './ParticipantTile.svelte';
  import ControlButton, { type ControlIcon } from './ControlButton.svelte';
  import MediaSplitControl from './MediaSplitControl.svelte';
  import { computeGalleryLayoutPreferringLines } from '@petal/shared/logic/galleryGeometry';
  import { CAMERA_TILE_ASPECT_RANGE } from '@petal/shared/logic/cameraCrop';
  import { computeSpotlightGeometry } from '@petal/shared/logic/spotlightGeometry';
  import { uniformFlip, uniformFlipKeyframes, visibleFlipRect, type FlipRect } from '@petal/shared/logic/tileFlip';
  import { chooseSpotlightHero } from '@petal/shared/logic/tileLayoutMode';
  import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';
  import { tileLayoutDuration, tileTransitionDuration, uniformTileFlip, uniformTileFlipInFlight } from '$lib/motion';
  import { stepFitLadder } from '$lib/meeting/fitLadder';
  import type { DrawUpdate } from '$lib/ipc';

  export interface GalleryParticipant {
    /** Stable participant identity when the real route has one. Falls back to
     * `name` in dev fixtures. */
    id?: string;
    name: string;
    videoOn?: boolean;
    /** Live MediaStream for this tile (local self-view). */
    videoStream?: MediaStream;
    /** Mirror the video (local self-view convention only). */
    mirrored?: boolean;
    speaking?: boolean;
    /** Hysteresis-smoothed speaker chosen by the meeting route for spotlight
     * promotion. `speaking` can be noisier because it drives only the ring. */
    activeSpeaker?: boolean;
    muted?: boolean;
    weakConnection?: boolean;
    isLocal?: boolean;
    /** Synthetic high-bit draw surface id for camera tiles. Not a remote-control/window id. */
    drawWindowId?: number;
    /** True when this participant is publishing a native window share. Native
     * shares are compositor NSWindows, not gallery tiles, so spotlight uses
     * this only to prioritize the sharer's webcam/gallery feed. */
    sharing?: boolean;
    /** #875: count of this participant's `petal-window-*` share
     * publications (display shares and viewer-hidden windows included).
     * Drives the multi-share count pill; renders only at count >= 2. */
    shareCount?: number;
    /** #875: identity-tinted colors for the count pill -- background/text,
     * same per-participant resolved sharing colors used elsewhere. */
    sharingLiveBackground?: string;
    sharingLiveColor?: string;
  }

  interface Props {
    roomName?: string;
    elapsed?: string;
    participants?: GalleryParticipant[];
    cameraDrawUpdates?: DrawUpdate[];
    micMuted?: boolean;
    cameraOn?: boolean;
    sharingActive?: boolean;
    sharingLiveBackground?: string;
    sharingLiveColor?: string;
    remoteControlAllowed?: boolean;
    onControl?: (icon: ControlIcon) => void;
    /** Public-code disclosure for active-meeting invite controls. The route
     * owns derivation so this presentational component never sees a raw room credential. */
    inviteAriaLabel?: string;
    inviteTooltip?: string;
    onInviteLinkCopy?: () => void | Promise<void>;
    onOpenNetwork?: () => void | Promise<void>;
    /** In-meeting Settings entry (More menu). Opens the standalone Settings
     * window; the route owns the invoke so this stays presentational. */
    onOpenSettings?: () => void | Promise<void>;
    onRenameRoom?: (displayName: string | null) => void | Promise<void>;
    stateTitle?: string | null;
    stateDetail?: string | null;
    stateTone?: 'info' | 'warning';
    /** Real routes pass true so the gallery IS the window (edge-to-edge, no
     * rounded floating card). Default false preserves the card look for the
     * /dev/* harnesses and MeetingChrome's framed usage. */
    frameless?: boolean;
    /** Optional in-meeting per-device menus (the popovers themselves are
     * owned by MeetingChrome): renders the carets on the mic/camera controls
     * only when provided, and hands the trigger element + kind back so the
     * matching popover can be placed against it. */
    onOpenDeviceMenu?: (kind: 'mic' | 'camera', el: HTMLElement) => void;
    /** Mirrors the open menu kind for the matching caret's aria-expanded. */
    deviceMenuKind?: 'mic' | 'camera' | null;
    /** Optional action rendered in-flow at the far right of the topbar —
     * MeetingChrome slots its large↔small view switcher here (issue #1) so it
     * genuinely sits top-right of the gallery instead of floating over it. */
    topbarAction?: Snippet;
    /** Meeting chat (plugins/README.md §2.7, a host surface): the drawer's
     * open state drives the toggle; unread is the badge while it is closed. */
    chatOpen?: boolean;
    chatUnread?: number;
    /**
     * A panel shown between the topbar and the control bar (today: the meeting
     * chat). Beside the tiles when the gallery is wide enough for both; over
     * the tiles when it is not. Either way the topbar and every control stay
     * visible and usable -- the same structure as the web client's
     * `.meeting-body` row.
     */
    sidePanel?: Snippet;
    /** Plugin toolbar buttons (plugins/README.md §2.7): the route renders
     * host-drawn `.control-cell`s here so plugin actions sit in the same row
     * as the built-in controls, before More. Undefined = no plugins. Called
     * with the `${pluginId}/${buttonId}` keys the row has moved into More,
     * whose cells the snippet hides (PluginToolbarButtons `hidden`). */
    pluginActions?: Snippet<[ReadonlySet<string>]>;
    /** Opens the feedback/bug-report dialog (#786). The route passes this
     * ONLY when the build carries a UserDispatch public key
     * (`isFeedbackEnabled()`); absent → the topbar cell never renders. */
    onReportBug?: () => void;
  }

  let {
    roomName = 'eng-sync',
    elapsed = '24:18',
    participants = [],
    cameraDrawUpdates = [],
    micMuted = false,
    cameraOn = false,
    sharingActive = true,
    sharingLiveBackground,
    sharingLiveColor,
    remoteControlAllowed = true,
    onControl,
    inviteAriaLabel = 'Copy invite link',
    inviteTooltip = 'Copy invite link',
    onInviteLinkCopy,
    onOpenNetwork,
    onOpenSettings,
    onRenameRoom,
    stateTitle = null,
    stateDetail = null,
    stateTone = 'info',
    frameless = false,
    /** Optional in-meeting per-device menus (MeetingChrome owns the
     * popovers): the carets render only when provided, and hand the trigger
     * element + kind back so the matching popover can be placed against it. */
    onOpenDeviceMenu,
    /** Mirrors the open menu kind for the matching caret's aria-expanded. */
    deviceMenuKind = null,
    topbarAction,
    chatOpen = false,
    chatUnread = 0,
    sidePanel,
    pluginActions,
    onReportBug
  }: Props = $props();

  type GalleryLayout = 'grid' | 'spotlight';
  type ParticipantEntry = GalleryParticipant & { key: string };

  let layoutMode = $state<GalleryLayout>('grid');
  let manualPinnedKey = $state<string | null>(null);
  let renamingRoom = $state(false);
  let roomNameDraft = $state('');
  let roomRenameError = $state<string | null>(null);
  let roomNameInput = $state<HTMLInputElement>();
  let roomRenamePending = $state(false);
  let tileSurface = $state<HTMLElement>();
  let tileSurfaceWidth = $state(0);
  let tileSurfaceHeight = $state(0);
  let galleryEl = $state<HTMLDivElement>();
  let galleryWidth = $state(0);
  let galleryHeight = $state(0);
  let spotlightRailEl = $state<HTMLDivElement>();
  let spotlightRailWidth = $state(0);
  let spotlightRailHeight = $state(0);
  let lastGalleryLayout: { count: number; columns: number; rows: number; line: boolean } | null = null;
  let inviteTooltipElement = $state<HTMLSpanElement>();
  let inviteTooltipShift = $state(0);
  const activeGalleryTileAnimations = new Map<string, Animation>();
  let galleryTransitionGeneration = 0;
  // The explicit Gallery FLIP pass owns transforms during mode/pin changes.
  // Suppress Svelte's keyed-list FLIP for that one update, but keep it for
  // ordinary participant-count reflow.
  let suppressSvelteFlip = $state(false);

  const INVITE_TOOLTIP_GUTTER_PX = 12;

  function captureGalleryTileRects(): Map<string, FlipRect> {
    const rects = new Map<string, FlipRect>();
    if (!tileSurface || typeof window === 'undefined') return rects;
    tileSurface.querySelectorAll<HTMLElement>('[data-participant-key]').forEach((tile) => {
      const key = tile.dataset.participantKey;
      const painted = tile.getBoundingClientRect();
      // #248: a tile mid-FLIP -- this pass's WAAPI one or Svelte's keyed-list
      // one -- may be clipped smaller than it paints; retarget from what is
      // visible so the next move starts without a jump.
      const inFlight = (key && activeGalleryTileAnimations.has(key)) || uniformTileFlipInFlight(tile);
      const rect = inFlight
        ? visibleFlipRect(painted, tile.offsetWidth, getComputedStyle(tile).clipPath)
        : painted;
      if (key && rect.width > 0 && rect.height > 0) rects.set(key, rect);
    });
    return rects;
  }

  function cancelGalleryTileAnimations() {
    for (const [key, animation] of activeGalleryTileAnimations) {
      animation.cancel();
      activeGalleryTileAnimations.delete(key);
    }
  }

  function animateGalleryLayoutFrom(previousRects: Map<string, FlipRect>, generation: number) {
    const duration = tileLayoutDuration();
    if (previousRects.size === 0 || duration === 0 || typeof requestAnimationFrame !== 'function') return;

    void tick().then(() => {
      requestAnimationFrame(() => {
        if (generation !== galleryTransitionGeneration || !tileSurface || tileLayoutDuration() === 0) return;
        const tiles = new Map<string, HTMLElement>();
        tileSurface.querySelectorAll<HTMLElement>('[data-participant-key]').forEach((tile) => {
          const key = tile.dataset.participantKey;
          if (key) tiles.set(key, tile);
        });

        for (const [key, previous] of previousRects) {
          const tile = tiles.get(key);
          if (!tile || typeof tile.animate !== 'function') continue;
          // #248: tiles change shape as cameras crop; the shared frame scales
          // uniformly and clips the box instead of squashing the video.
          const flip = uniformFlip(previous, tile.getBoundingClientRect());
          if (!flip) continue;

          const radius = parseFloat(getComputedStyle(tile).borderTopLeftRadius) || 0;
          const animation = tile.animate(
            uniformFlipKeyframes(flip, radius).map((frame) => ({ opacity: 1, ...frame })),
            { duration, easing: 'cubic-bezier(0.2, 0, 0, 1)', fill: 'none' }
          );
          activeGalleryTileAnimations.set(key, animation);
          void animation.finished
            .then(() => {
              if (activeGalleryTileAnimations.get(key) === animation) activeGalleryTileAnimations.delete(key);
            })
            .catch(() => {
              if (activeGalleryTileAnimations.get(key) === animation) activeGalleryTileAnimations.delete(key);
            });
        }
      });
    });
  }

  function transitionGalleryLayout(mutate: () => void) {
    const previousRects = captureGalleryTileRects();
    const generation = ++galleryTransitionGeneration;
    const duration = tileLayoutDuration();
    // Measure first, then cancel: the rect includes the currently painted
    // transform, so a rapid request retargets from where the tile actually is.
    cancelGalleryTileAnimations();
    suppressSvelteFlip = duration > 0;
    mutate();
    if (duration > 0) {
      setTimeout(() => {
        if (generation === galleryTransitionGeneration) suppressSvelteFlip = false;
      }, duration);
    }
    animateGalleryLayoutFrom(previousRects, generation);
  }

  function keepInviteTooltipInViewport() {
    requestAnimationFrame(() => {
      const tooltip = inviteTooltipElement;
      if (!tooltip) return;
      const rect = tooltip.getBoundingClientRect();
      // A resize can run after a previous correction. Calculate from the
      // unshifted box so repeated events converge instead of losing the shift.
      const unshiftedLeft = rect.left - inviteTooltipShift;
      const unshiftedRight = rect.right - inviteTooltipShift;
      inviteTooltipShift = unshiftedLeft < INVITE_TOOLTIP_GUTTER_PX
        ? INVITE_TOOLTIP_GUTTER_PX - unshiftedLeft
        : unshiftedRight > window.innerWidth - INVITE_TOOLTIP_GUTTER_PX
          ? window.innerWidth - INVITE_TOOLTIP_GUTTER_PX - unshiftedRight
          : 0;
    });
  }

  $effect(() => {
    if (!renamingRoom) roomNameDraft = roomName;
  });

  function participantKey(p: GalleryParticipant): string {
    return p.id ?? p.name;
  }

  const participantEntries = $derived<ParticipantEntry[]>(
    participants.map((p) => ({ ...p, key: participantKey(p) }))
  );
  // #785: the fallback used to read `sharing -> activeSpeaker -> LOCAL -> first`,
  // so when the sharer stopped, the hero landed on the user's own tile — they
  // sat in spotlight staring at their own webcam. `chooseSpotlightHero` (shared
  // with the web client) drops the local self-view behind every remote
  // candidate; a sharer's own webcam is still self-view, not the share, since
  // native shares are compositor NSWindows rather than gallery tiles.
  const spotlightCandidates = $derived(
    participantEntries.map((p) => ({
      key: p.key,
      isSharing: p.sharing === true,
      isActiveSpeaker: p.activeSpeaker === true,
      hasVideo: p.videoOn === true || !!p.videoStream,
      isLocal: p.isLocal === true
    }))
  );
  const spotlightKey = $derived(manualPinnedKey ?? chooseSpotlightHero(spotlightCandidates)?.key);
  const spotlightEntry = $derived(participantEntries.find((p) => p.key === spotlightKey));
  const thumbnailEntries = $derived(participantEntries.filter((p) => p.key !== spotlightEntry?.key));
  const spotlightActive = $derived(layoutMode === 'spotlight' && !!spotlightEntry);
  // The spotlight CSS relies on the main tile being FIRST in the DOM (it is
  // `display:block`; thumbnails are `display:inline-block` and flow after it).
  // If the spotlight entry happens to sit mid-list (e.g. Priya at index 2),
  // the preceding inline-blocks form a row above her — producing three rows
  // instead of one large hero + one thumb strip. Fix: hoist the main entry to
  // position 0 in spotlight mode while keeping the single keyed loop intact.
  const tileEntries = $derived<ParticipantEntry[]>(
    spotlightActive && spotlightEntry
      ? [spotlightEntry, ...thumbnailEntries]
      : participantEntries
  );
  // Window shape -> chrome. The gallery measures ITSELF (not the viewport), so
  // the meeting window can be any shape a person drags it to (#239 parity):
  //  - short and wide (a bar of faces along the top of the screen): the
  //    controls become a rail at the right edge and the top bar floats over
  //    the tiles, shown while the pointer is over the window or keyboard
  //    focus is in it, so nearly the whole height goes to faces;
  //  - narrow (a column of faces beside an editor): the controls drop their
  //    labels and wrap (the control-row fit below), never clipping, and the
  //    top bar wraps its buttons under the name -- or floats too, while the
  //    column is short enough that a top bar would be most of it.
  // A floating top bar takes clicks only on its own buttons: the tiles under
  // it stay clickable, and the window is moved by the control bar's empty
  // space instead.
  const RAIL_BELOW_HEIGHT = 420;
  const RAIL_BUTTON_PX = 40;
  const RAIL_GAP_PX = 6;
  const RAIL_PADDING_PX = 8;
  const railControls = $derived(
    galleryHeight > 0 && galleryHeight < RAIL_BELOW_HEIGHT && galleryWidth > galleryHeight
  );
  /** Below this width (or on the rail) the tiles take the window's edges:
   * smaller padding and gaps, so a column of faces is mostly faces. */
  const COMPACT_WINDOW_BELOW_WIDTH = 620;
  const COMPACT_TILE_GAP_PX = 8;
  const compactWindow = $derived(
    railControls || (galleryWidth > 0 && galleryWidth < COMPACT_WINDOW_BELOW_WIDTH)
  );
  const FLOATING_TOPBAR_NARROW_WIDTH = 520;
  const FLOATING_TOPBAR_NARROW_HEIGHT = 600;
  const floatingTopbar = $derived(
    railControls ||
      (galleryWidth > 0 &&
        galleryWidth < FLOATING_TOPBAR_NARROW_WIDTH &&
        galleryHeight > 0 &&
        galleryHeight < FLOATING_TOPBAR_NARROW_HEIGHT)
  );
  /** Rail controls fill top to bottom, then start another column. */
  const railRows = $derived(
    Math.max(1, Math.floor((galleryHeight - RAIL_PADDING_PX * 2 + RAIL_GAP_PX) / (RAIL_BUTTON_PX + RAIL_GAP_PX)))
  );
  const galleryChromeStyle = $derived(`--rail-rows: ${railRows};`);

  const gridOverflowScroll = $derived(false);
  // `lastGalleryLayout` is a plain (non-reactive) closure variable, not
  // `$state` -- it feeds `previous` back into computeGalleryLayout for
  // hysteresis (keep the current shape unless another clears it by a
  // margin) without itself triggering a re-derive.
  const smartGridLayout = $derived.by(() => {
    // #248: the gallery holds camera tiles only (native shares are separate
    // compositor windows), so every layout may crop within the shared caps.
    // A bar or column of faces keeps everyone in one line while that shows
    // faces at least as tall as a grid would (the shared packer decides).
    const layout = computeGalleryLayoutPreferringLines(participantEntries.length, tileSurfaceWidth, tileSurfaceHeight, {
      previous: lastGalleryLayout,
      previousWasLine: lastGalleryLayout?.count === participantEntries.length && lastGalleryLayout.line,
      tileAspectRange: CAMERA_TILE_ASPECT_RANGE,
      gap: compactWindow ? COMPACT_TILE_GAP_PX : undefined
    });
    lastGalleryLayout = { count: participantEntries.length, columns: layout.columns, rows: layout.rows, line: layout.line };
    return layout;
  });
  const smartGridStyle = $derived(
    `--gallery-cols: ${smartGridLayout.columns}; --gallery-half-cols: ${smartGridLayout.columns * 2}; --gallery-rows: ${smartGridLayout.rows}; --gallery-tile-width: ${smartGridLayout.tileWidth}px; --gallery-tile-height: ${smartGridLayout.tileHeight}px; --gallery-gap: ${smartGridLayout.gap}px; --camera-tile-min-aspect: ${CAMERA_TILE_ASPECT_RANGE.min};`
  );

  // Spotlight (#239 parity): the shared geometry decides whether the
  // thumbnails sit beside the hero or in rows below it, and sizes the hero
  // and ONE thumbnail size for everyone (your own included). The hero takes
  // its camera's own shape, so a phone held upright is shown whole.
  const DEFAULT_HERO_ASPECT = 16 / 9;
  let heroAspect = $state(DEFAULT_HERO_ASPECT);
  const spotlightGap = $derived(compactWindow ? COMPACT_TILE_GAP_PX : 12);
  const spotlightGeometry = $derived(
    computeSpotlightGeometry(
      thumbnailEntries.length,
      spotlightRailWidth,
      spotlightRailHeight,
      spotlightGap,
      { aspect: heroAspect, header: 0 },
      { wrapSideStrip: true }
    )
  );
  /** The thumbnails need more room than the rail has: the rail scrolls and
   * the hero stays put at its top. */
  const spotlightScrolls = $derived.by(() => {
    const g = spotlightGeometry;
    const count = thumbnailEntries.length;
    if (count === 0 || g.thumbnailWidth <= 0) return false;
    if (g.placement === 'side') {
      const rows = Math.ceil(count / Math.max(1, g.stripColumns));
      return rows * g.thumbnailHeight + (rows - 1) * spotlightGap > spotlightRailHeight + 0.5;
    }
    const perRow = Math.max(1, Math.floor((spotlightRailWidth + spotlightGap) / (g.thumbnailWidth + spotlightGap)));
    const rows = Math.ceil(count / perRow);
    return g.heroHeight + spotlightGap + rows * g.thumbnailHeight + (rows - 1) * spotlightGap > spotlightRailHeight + 0.5;
  });
  const spotlightRailStyle = $derived(
    spotlightActive && spotlightRailWidth > 0 && spotlightRailHeight > 0
      ? `--hero-w: ${spotlightGeometry.heroWidth}px; --hero-h: ${spotlightGeometry.heroHeight}px; --thumb-w: ${spotlightGeometry.thumbnailWidth}px; --thumb-h: ${spotlightGeometry.thumbnailHeight}px; --strip-cols: ${Math.max(1, spotlightGeometry.stripColumns)}; --strip-rows: ${Math.max(1, Math.ceil(thumbnailEntries.length / Math.max(1, spotlightGeometry.stripColumns)))}; --spotlight-gap: ${spotlightGap}px;`
      : undefined
  );
  const layoutToggleLabel = $derived(
    layoutMode === 'grid' ? 'Switch to spotlight' : 'Switch to gallery grid'
  );
  // Bug report (#786): a submission is refused while a window is shared
  // (feedback.rs) so a diagnostic can never carry shared content. The topbar
  // says so — same string in the tooltip and the aria-label — rather than
  // offering a button that swallows the click.
  const REPORT_BUG_BLOCKED_REASON = "Bug reports pause while you're sharing a window";
  const reportBugBlocked = $derived(sharingActive);
  const reportBugLabel = $derived(reportBugBlocked ? REPORT_BUG_BLOCKED_REASON : 'Report a bug');
  const gridStateTitle = $derived(stateTitle);

  $effect(() => {
    const keys = new Set(participantEntries.map((p) => p.key));
    if (manualPinnedKey && !keys.has(manualPinnedKey)) manualPinnedKey = null;
  });

  onMount(() => {
    const surface = tileSurface;
    if (!surface || typeof ResizeObserver === 'undefined') return;

    const updateTileSurfaceSize = () => {
      const style = getComputedStyle(surface);
      tileSurfaceWidth = Math.max(
        0,
        surface.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)
      );
      tileSurfaceHeight = Math.max(
        0,
        surface.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom)
      );
    };

    updateTileSurfaceSize();
    const observer = new ResizeObserver(updateTileSurfaceSize);
    observer.observe(surface);

    return () => observer.disconnect();
  });

  $effect(() => {
    const el = galleryEl;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const update = () => {
      galleryWidth = el.clientWidth;
      galleryHeight = el.clientHeight;
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  });

  // The spotlight is packed into the rail's content box (its padding keeps
  // the sharing ring's glow clear of the scroll edge).
  $effect(() => {
    const rail = spotlightRailEl;
    if (!rail || typeof ResizeObserver === 'undefined') return;
    const update = () => {
      const style = getComputedStyle(rail);
      spotlightRailWidth = Math.max(0, rail.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight));
      spotlightRailHeight = Math.max(0, rail.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom));
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(rail);
    return () => observer.disconnect();
  });

  // The hero camera's shape, read off its video as frames arrive (a phone
  // can rotate mid-meeting: `resize`). No video yet, or camera off: 16:9.
  $effect(() => {
    if (!spotlightActive || !tileSurface) {
      heroAspect = DEFAULT_HERO_ASPECT;
      return;
    }
    void spotlightEntry?.key;
    void spotlightEntry?.videoStream;
    void spotlightEntry?.videoOn;
    let cancelled = false;
    let video: HTMLVideoElement | null = null;
    const read = () => {
      const next = video && video.videoWidth > 0 && video.videoHeight > 0 && spotlightEntry?.videoOn !== false
        ? video.videoWidth / video.videoHeight
        : DEFAULT_HERO_ASPECT;
      if (Math.abs(next - heroAspect) > 0.005) heroAspect = next;
    };
    const events = ['loadedmetadata', 'resize', 'emptied'] as const;
    void tick().then(() => {
      if (cancelled) return;
      video = tileSurface?.querySelector<HTMLVideoElement>('.tile-wrap.spotlight-main video') ?? null;
      for (const name of events) video?.addEventListener(name, read);
      read();
    });
    return () => {
      cancelled = true;
      for (const name of events) video?.removeEventListener(name, read);
    };
  });

  function setLayoutMode(next: GalleryLayout) {
    transitionGalleryLayout(() => {
      layoutMode = next;
      if (next === 'grid') manualPinnedKey = null;
    });
  }

  function pinParticipant(key: string) {
    // Clicking the current spotlight main acts as a toggle: unpin and return
    // to grid. This gives every tile a visible response on click — the prior
    // "re-pin same key" was a no-op the user read as broken.
    if (spotlightActive && spotlightEntry?.key === key) {
      transitionGalleryLayout(() => {
        manualPinnedKey = null;
        layoutMode = 'grid';
      });
      return;
    }
    transitionGalleryLayout(() => {
      manualPinnedKey = key;
      layoutMode = 'spotlight';
    });
    // Keep keyboard activation deterministic after the variant class changes.
    // The keyed tile remains mounted, so this is a harmless focus restoration
    // for pointer activation and preserves the keyboard pinning contract.
    requestAnimationFrame(() => {
      (tileSurface?.querySelector<HTMLElement>('.tile-wrap.spotlight-main'))?.focus();
    });
  }

  function handleTileKeydown(event: KeyboardEvent, key: string) {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    pinParticipant(key);
  }

  function tileAriaLabel(entry: ParticipantEntry): string {
    if (spotlightActive && entry.key === spotlightEntry?.key) {
      return `${entry.name} is spotlighted; click to return to gallery`;
    }
    return entry.sharing
      ? `Spotlight ${entry.name}'s gallery feed; shared window stays separate`
      : `Spotlight ${entry.name}`;
  }

  /** #239 parity: an incomplete last row sits centred under the full ones.
   * The grid runs on half-column tracks, so the row's first tile starts after
   * half the empty cells; the rest auto-place after it. */
  function tailColumnStart(index: number): number | undefined {
    const columns = smartGridLayout.columns;
    const tail = participantEntries.length % columns;
    if (spotlightActive || tail === 0 || index !== participantEntries.length - tail) return undefined;
    return columns - tail + 1;
  }

  async function beginRoomRename() {
    if (!onRenameRoom || roomRenamePending) return;
    roomNameDraft = roomName;
    roomRenameError = null;
    renamingRoom = true;
    await tick();
    roomNameInput?.focus();
    roomNameInput?.select();
  }

  async function commitRoomRename() {
    if (!renamingRoom || roomRenamePending) return;
    const next = roomNameDraft.trim();
    roomRenameError = null;
    if (next === roomName.trim()) {
      renamingRoom = false;
      return;
    }
    roomRenamePending = true;
    try {
      await onRenameRoom?.(next || null);
      renamingRoom = false;
    } catch (e) {
      console.error('room rename failed', e);
      // Keep the input open with the draft and say why — a silently reverted
      // name (old behavior) reads as "the app ignored me".
      roomRenameError = 'Could not rename the room.';
      roomNameInput?.focus();
    } finally {
      roomRenamePending = false;
    }
  }

  function cancelRoomRename() {
    roomNameDraft = roomName;
    roomRenameError = null;
    renamingRoom = false;
  }

  function handleRoomRenameKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      event.preventDefault();
      void commitRoomRename();
    } else if (event.key === 'Escape') {
      event.preventDefault();
      cancelRoomRename();
    }
  }

  function openNetworkCockpit() {
    void onOpenNetwork?.();
  }

  function copyRoomInvite() {
    void onInviteLinkCopy?.();
  }

  type GalleryMoreIcon = 'region' | 'remotecontrol' | 'invite' | 'chat' | 'screenshare';

  // Control-row fit (#247 parity with the web client's control bar). The row
  // is ONE row at every width; what does not fit moves into More, measured,
  // never by breakpoint, one step at a time (fitLadder.ts) and back as soon
  // as the wider step fits again. Mic, Camera, More and Leave never leave the
  // row. The rest give way lowest priority first, the web client's order:
  // plugin buttons (the one furthest along first), Invite, Chat, Share. Only
  // when even Mic, Camera, More and Leave do not fit with their labels does
  // the row switch to icon only (super compact). The rail (a short window)
  // is its own arrangement: icon only, Invite in More, filled top to bottom
  // and then another column.
  const BUILTIN_COLLAPSE_ORDER = ['invite', 'chat', 'share'] as const;
  /** `${pluginId}/${buttonId}` of each plugin cell in the row, in row order. */
  let pluginCellKeys = $state<string[]>([]);
  const collapseOrder = $derived<string[]>([
    ...[...pluginCellKeys].reverse().map((key) => `plugin:${key}`),
    ...BUILTIN_COLLAPSE_ORDER
  ]);
  let fitLevel = $state(0);
  let controlbarEl = $state<HTMLDivElement>();
  let controlsClusterEl = $state<HTMLDivElement>();
  /** The bar width each level needed (its row plus ITS OWN side padding --
   * the icon-only bar is padded less) when it was last showing. */
  const barWidthAtLevel: Array<number | undefined> = [];
  let controlLadderKey = '';

  const collapsedControls = $derived<ReadonlySet<string>>(
    railControls ? new Set(['invite']) : new Set(collapseOrder.slice(0, Math.min(fitLevel, collapseOrder.length)))
  );
  const compactControls = $derived(railControls || fitLevel > collapseOrder.length);
  /** Plugin buttons in More: PluginToolbarButtons hides their cells. */
  const hiddenPluginKeys = $derived<ReadonlySet<string>>(
    new Set([...collapsedControls].filter((key) => key.startsWith('plugin:')).map((key) => key.slice('plugin:'.length)))
  );

  /** The row's width on one line: every shown cell plus the gaps. */
  function oneRowWidth(cluster: HTMLElement): number {
    const style = getComputedStyle(cluster);
    const gap = parseFloat(style.columnGap) || 0;
    let width = 0;
    let shown = 0;
    for (const cell of Array.from(cluster.children) as HTMLElement[]) {
      const cellStyle = getComputedStyle(cell);
      if (cellStyle.display === 'none') continue;
      width += cell.getBoundingClientRect().width + (parseFloat(cellStyle.marginLeft) || 0) + (parseFloat(cellStyle.marginRight) || 0);
      shown += 1;
    }
    return width + gap * Math.max(0, shown - 1);
  }

  function refitControls(): void {
    const bar = controlbarEl;
    const cluster = controlsClusterEl;
    if (!bar || !cluster) return;
    const keys = Array.from(cluster.querySelectorAll<HTMLElement>('.plugin-cell')).map(
      (cell) => `${cell.dataset.plugin}/${cell.dataset.button}`
    );
    // A plugin button came or went: what each level needs has changed.
    const ladderKey = keys.join('|');
    if (ladderKey !== controlLadderKey) {
      controlLadderKey = ladderKey;
      pluginCellKeys = keys;
      barWidthAtLevel.length = 0;
      fitLevel = 0;
      return;
    }
    if (railControls) return;
    const barStyle = getComputedStyle(bar);
    const needed = oneRowWidth(cluster) + parseFloat(barStyle.paddingLeft) + parseFloat(barStyle.paddingRight);
    fitLevel = stepFitLadder(fitLevel, needed, bar.clientWidth, barWidthAtLevel, collapseOrder.length + 1);
  }

  $effect(() => {
    const bar = controlbarEl;
    const cluster = controlsClusterEl;
    if (!bar || !cluster || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => refitControls());
    observer.observe(bar);
    observer.observe(cluster);
    return () => observer.disconnect();
  });

  // Leaving the rail: re-measure (the rail's own arrangement says nothing
  // about how wide the row is).
  $effect(() => {
    if (railControls) return;
    void tick().then(() => refitControls());
  });

  // A badge on a plugin button in More (an unread count, say) puts a dot on
  // More, as the web client's ⋯ does.
  let pluginAttention = $state(false);
  $effect(() => {
    const cluster = controlsClusterEl;
    if (!cluster || typeof MutationObserver === 'undefined') return;
    const read = () => {
      pluginAttention = !!cluster.querySelector('.plugin-cell.collapsed .badge');
    };
    read();
    const observer = new MutationObserver(read);
    observer.observe(cluster, { subtree: true, childList: true, characterData: true, attributes: true, attributeFilter: ['class'] });
    return () => observer.disconnect();
  });

  const inviteCollapsed = $derived(collapsedControls.has('invite'));
  const chatCollapsed = $derived(collapsedControls.has('chat'));
  const shareCollapsed = $derived(collapsedControls.has('share'));

  /** A plugin button in More, as its row shows it -- read off the real
   * (hidden) button, which the row clicks, so each keeps one code path. */
  interface MorePluginRow {
    key: string;
    label: string;
    ariaLabel: string;
    glyph: string;
    badge: string | null;
    disabled: boolean;
  }
  let morePluginRows = $state<MorePluginRow[]>([]);

  function readMorePluginRows(): MorePluginRow[] {
    const cluster = controlsClusterEl;
    if (!cluster) return [];
    return Array.from(cluster.querySelectorAll<HTMLElement>('.plugin-cell.collapsed')).map((cell) => {
      const button = cell.querySelector<HTMLButtonElement>('button');
      return {
        key: `${cell.dataset.plugin}/${cell.dataset.button}`,
        label: cell.querySelector('.meeting-control-label')?.textContent?.trim() ?? '',
        ariaLabel: button?.getAttribute('aria-label') ?? '',
        glyph: cell.querySelector('.glyph')?.innerHTML ?? '',
        badge: cell.querySelector('.badge')?.textContent?.trim() || null,
        disabled: button?.disabled ?? false
      };
    });
  }

  function activateMorePluginRow(key: string) {
    closeGalleryMore(false);
    const [plugin, button] = key.split('/');
    controlsClusterEl
      ?.querySelector<HTMLButtonElement>(
        `.plugin-cell[data-plugin="${CSS.escape(plugin ?? '')}"][data-button="${CSS.escape(button ?? '')}"] button`
      )
      ?.click();
  }
  let galleryMoreOpen = $state(false);
  let galleryMoreMenuEl = $state<HTMLDivElement>();
  let galleryMoreTriggerEl = $state<HTMLButtonElement>();

  function closeGalleryMore(restoreFocus = true) {
    galleryMoreOpen = false;
    if (restoreFocus) requestAnimationFrame(() => galleryMoreTriggerEl?.focus());
  }

  function toggleGalleryMore() {
    if (galleryMoreOpen) {
      closeGalleryMore(false);
      return;
    }
    morePluginRows = readMorePluginRows();
    galleryMoreOpen = true;
  }

  function selectGalleryMore(icon: GalleryMoreIcon) {
    closeGalleryMore(false);
    onControl?.(icon);
  }

  function selectGalleryMoreSettings() {
    closeGalleryMore(false);
    void onOpenSettings?.();
  }

  $effect(() => {
    if (galleryMoreOpen) {
      return installDismissibleLayer({
        isOpen: () => galleryMoreOpen,
        getInsideNodes: () => [galleryMoreMenuEl, galleryMoreTriggerEl],
        getPopupNodes: () => [galleryMoreMenuEl],
        getOpener: () => galleryMoreTriggerEl,
        onDismiss: () => closeGalleryMore(false)
      });
    }
  });

  $effect(() => {
    if (!galleryMoreOpen) return;
    let cancelled = false;
    void tick().then(() => {
      if (!cancelled) galleryMoreMenuEl?.querySelector<HTMLButtonElement>('.gallery-more-item')?.focus();
    });
    const onKeydown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeGalleryMore();
        return;
      }
      if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
      const items = Array.from(galleryMoreMenuEl?.querySelectorAll<HTMLButtonElement>('.gallery-more-item') ?? []);
      if (items.length === 0) return;
      event.preventDefault();
      const current = items.indexOf(document.activeElement as HTMLButtonElement);
      const delta = event.key === 'ArrowDown' ? 1 : -1;
      items[(current + delta + items.length) % items.length]?.focus();
    };
    window.addEventListener('keydown', onKeydown);
    return () => {
      cancelled = true;
      window.removeEventListener('keydown', onKeydown);
    };
  });

</script>

<svelte:window onresize={keepInviteTooltipInViewport} />

<div
  bind:this={galleryEl}
  class="gallery"
  class:frameless
  class:rail-controls={railControls}
  class:compact-controls={compactControls}
  class:compact-window={compactWindow}
  class:floating-topbar={floatingTopbar}
  class:has-side-panel={!!sidePanel}
  style={galleryChromeStyle}
>
  <div class="topbar">
    <div class="topbar-drag-layer" data-tauri-drag-region aria-hidden="true"></div>
    <div class="topbar-left" data-tauri-drag-region>
      <span class="room-title">
        {#if renamingRoom}
          <input
            bind:this={roomNameInput}
            class="room-name-input"
            autocapitalize="off"
            autocomplete="off"
            bind:value={roomNameDraft}
            aria-label="Room display name"
            aria-invalid={!!roomRenameError}
            disabled={roomRenamePending}
            onkeydown={handleRoomRenameKeydown}
            onblur={() => void commitRoomRename()}
          />
          {#if roomRenameError}
            <span class="room-rename-error" role="alert">{roomRenameError}</span>
          {/if}
        {:else}
          <span class="room-name">{roomName}</span>
          {#if onInviteLinkCopy}
            <span class="room-title-actions" class:has-rename={!!onRenameRoom}>
              <button
                type="button"
                class="room-title-action room-copy-button"
                aria-label={inviteAriaLabel}
                disabled={roomRenamePending}
                onclick={copyRoomInvite}
              >
                <svg class="copy-icon" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <rect x="9" y="9" width="11" height="11" rx="2"></rect>
                  <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
                </svg>
              </button>
              {#if onRenameRoom}
                <button
                  type="button"
                  class="room-title-action room-rename-button"
                  aria-label="Rename room"
                  disabled={roomRenamePending}
                  onclick={beginRoomRename}
                >
                  <svg class="rename-icon" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <path d="M12 20h9"></path>
                    <path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z"></path>
                  </svg>
                </button>
              {/if}
            </span>
          {:else if onRenameRoom}
            <span class="room-title-actions has-rename">
              <button
                type="button"
                class="room-title-action room-rename-button"
                aria-label="Rename room"
                disabled={roomRenamePending}
                onclick={beginRoomRename}
              >
                <svg class="rename-icon" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M12 20h9"></path>
                  <path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z"></path>
                </svg>
              </button>
            </span>
          {/if}
        {/if}
        <span class="elapsed">{elapsed}</span>
      </span>
    </div>
    <div class="topbar-right">
      <div class="topbar-control-cell">
        <!-- View toggle (#186): one destination-state button, not simultaneous
             gallery/window icons. Grid mode shows the spotlight/window glyph;
             spotlight mode shows the gallery-grid glyph to get back. -->
        <button
          type="button"
          class="chrome-icon-button layout-toggle"
          aria-label={layoutToggleLabel}
          onclick={() => setLayoutMode(layoutMode === 'grid' ? 'spotlight' : 'grid')}
        >
          {#if layoutMode === 'grid'}
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <rect x="2" y="2" width="20" height="11" rx="2"></rect>
              <rect x="2" y="17" width="6" height="5" rx="1"></rect>
              <rect x="9" y="17" width="6" height="5" rx="1"></rect>
              <rect x="16" y="17" width="6" height="5" rx="1"></rect>
            </svg>
          {:else}
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <rect x="4" y="4" width="6" height="6" rx="1.5"></rect>
              <rect x="14" y="4" width="6" height="6" rx="1.5"></rect>
              <rect x="4" y="14" width="6" height="6" rx="1.5"></rect>
              <rect x="14" y="14" width="6" height="6" rx="1.5"></rect>
            </svg>
          {/if}
        </button>
        <span class="topbar-tooltip" aria-hidden="true">{layoutToggleLabel}</span>
      </div>
      <!-- Bug report (#786): renders ONLY when the route hands down a
           trigger, which it does only for a build carrying a UserDispatch
           key. While sharing, the report is refused by feedback.rs anyway —
           so say why (tooltip + aria-label) instead of going inert. -->
      {#if onReportBug}
        <div class="topbar-control-cell">
          <button
            type="button"
            class="chrome-icon-button report-bug"
            class:blocked={reportBugBlocked}
            aria-label={reportBugLabel}
            aria-disabled={reportBugBlocked}
            onclick={() => {
              if (!reportBugBlocked) void onReportBug?.();
            }}
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M9 4.2 7.4 2.4"></path>
              <path d="M15 4.2 16.6 2.4"></path>
              <path d="M12 20.6a5 5 0 0 1-5-5v-4.4a5 5 0 0 1 10 0v4.4a5 5 0 0 1-5 5z"></path>
              <path d="M7 11.6H3.4"></path>
              <path d="M17 11.6h3.6"></path>
              <path d="M7.5 16.8 4.3 18.8"></path>
              <path d="M16.5 16.8 19.7 18.8"></path>
            </svg>
          </button>
          <span class="topbar-tooltip" aria-hidden="true">{reportBugLabel}</span>
        </div>
      {/if}
      <!-- Network conditions (issue #19): quiet graphite signal-bars
           icon. Deliberately NOT tinted by connection quality —
           color-rationing keeps the topbar neutral; the quality color hint
           lives inside the cockpit itself. -->
      <div class="topbar-control-cell">
        <button
          type="button"
          class="chrome-icon-button net-btn"
          aria-label="Connection stats"
          onclick={openNetworkCockpit}
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
            <path d="M4 18v-3"></path>
            <path d="M9.5 18v-7"></path>
            <path d="M15 18V7"></path>
            <path d="M20.5 18V3.5"></path>
          </svg>
        </button>
        <span class="topbar-tooltip" aria-hidden="true">Connection stats</span>
      </div>
      {@render topbarAction?.()}
    </div>
  </div>

  <div class="gallery-body">
  <div
    bind:this={tileSurface}
    class="tiles"
    class:grid={!spotlightActive}
    class:spotlight={spotlightActive}
    class:with-state={!!gridStateTitle}
    class:scrollable={gridOverflowScroll}
    class:compact={smartGridLayout.compact}
    class:tiny={smartGridLayout.tiny}
    style={smartGridStyle}
  >
    {#if gridStateTitle}
      <div class="gallery-state" class:warning={stateTone === 'warning'}>
        <p class="gallery-state-title">{gridStateTitle}</p>
        {#if stateDetail}
          <p class="gallery-state-detail">{stateDetail}</p>
        {/if}
      </div>
    {/if}

    <!-- One keyed participant tree serves both presentations. Grid and
         spotlight only change classes/layout, so ParticipantTile and its
         camera element never remount during a visual layout change. -->
    <div class="spotlight-layout" class:solo={spotlightActive && thumbnailEntries.length === 0} class:grid-layout={!spotlightActive}>
      <div
        bind:this={spotlightRailEl}
        class="spotlight-rail"
        class:side={spotlightActive && spotlightGeometry.placement === 'side'}
        class:below={spotlightActive && spotlightGeometry.placement === 'below'}
        class:scrolls={spotlightActive && spotlightScrolls}
        style={spotlightRailStyle}
        aria-label={spotlightActive ? 'Other gallery feeds' : undefined}
      >
        {#each tileEntries as p, index (p.key)}
          <!-- Participant join/leave use a restrained opacity transition. The
               explicit Gallery FLIP pass suppresses keyed-list FLIP during
               mode/pin changes; ordinary participant-count reflow retains it. -->
          <div
            animate:uniformTileFlip={{ duration: suppressSvelteFlip ? 0 : tileLayoutDuration() }}
            class="tile-wrap"
            data-participant-key={p.key}
            class:spotlight-main={spotlightActive && p.key === spotlightEntry?.key}
            class:spotlight-thumb={spotlightActive && p.key !== spotlightEntry?.key}
            style:grid-column-start={tailColumnStart(index)}
            class:pinned={manualPinnedKey === p.key}
            class:sharing={!!p.sharing}
            style:--sharing-tint={p.sharingLiveBackground}
            role="button"
            tabindex="0"
            aria-label={tileAriaLabel(p)}
            onclick={() => pinParticipant(p.key)}
            onkeydown={(event) => handleTileKeydown(event, p.key)}
            in:fade={{ duration: tileTransitionDuration() }}
            out:fade={{ duration: tileTransitionDuration() }}
          >
            <ParticipantTile
              name={p.name}
              ownerIdentity={p.id}
              drawWindowId={p.drawWindowId}
              drawUpdates={cameraDrawUpdates}
              videoOn={p.videoOn ?? true}
              videoStream={p.videoStream}
              mirrored={p.mirrored ?? false}
              speaking={p.speaking ?? false}
              muted={p.muted ?? false}
              weakConnection={p.weakConnection ?? false}
              shareCount={p.shareCount ?? 0}
              sharingLiveBackground={p.sharingLiveBackground}
              sharingLiveColor={p.sharingLiveColor}
              isLocal={p.isLocal ?? false}
            />
            {#if manualPinnedKey === p.key}
              <span class="pin-mark" aria-hidden="true">
                <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M14 4l6 6"></path>
                  <path d="M9 14l-5 5"></path>
                  <path d="M15 5l4 4-8 8-4-4z"></path>
                </svg>
              </span>
            {/if}
          </div>
        {/each}
      </div>
    </div>
  </div>
  {#if sidePanel}
    <aside class="side-panel" data-testid="chat-aside">
      {@render sidePanel()}
    </aside>
  {/if}
  </div>

  <!-- The gallery keeps the common actions visible and moves specialist
       actions into More. Labels remain stable while accessible names describe
       the current state. -->
  <div
    class="controlbar"
    bind:this={controlbarEl}
    data-orientation={railControls ? 'vertical' : 'horizontal'}
    data-tauri-drag-region={floatingTopbar ? '' : undefined}
  >
    <div class="controls-cluster" bind:this={controlsClusterEl} data-tauri-drag-region={floatingTopbar ? '' : undefined}>
      <div class="control-cell">
        <MediaSplitControl
          icon="mic"
          active={micMuted}
          actionLabel={micMuted ? 'Unmute microphone' : 'Mute microphone'}
          optionsLabel="Microphone options"
          optionsOpen={deviceMenuKind === 'mic'}
          optionsEnabled={!!onOpenDeviceMenu}
          visibleLabel="Mic"
          onToggle={() => onControl?.('mic')}
          onOptions={(el) => onOpenDeviceMenu?.('mic', el)}
        />
      </div>
      <div class="control-cell">
        <MediaSplitControl
          icon="camera"
          active={!cameraOn}
          actionLabel={cameraOn ? 'Turn camera off' : 'Turn camera on'}
          optionsLabel="Camera options"
          optionsOpen={deviceMenuKind === 'camera'}
          optionsEnabled={!!onOpenDeviceMenu}
          visibleLabel="Camera"
          onToggle={() => onControl?.('camera')}
          onOptions={(el) => onOpenDeviceMenu?.('camera', el)}
        />
      </div>
      <div class="control-cell" class:collapsed={shareCollapsed} data-control="share">
        <ControlButton
          icon="screenshare"
          kind="toggle"
          active={sharingActive}
          label={sharingActive ? 'Stop sharing' : 'Share a window'}
          liveBackground={sharingLiveBackground}
          liveColor={sharingLiveColor}
          onclick={() => onControl?.('screenshare')}
        />
        <span class="meeting-control-label">Share</span>
      </div>
      <div class="control-cell" class:collapsed={inviteCollapsed} data-control="invite" role="group" onmouseenter={keepInviteTooltipInViewport} onfocusin={keepInviteTooltipInViewport}>
        <ControlButton
          icon="invite"
          kind="oneshot"
          label={inviteAriaLabel}
          onclick={() => onControl?.('invite')}
        />
        <span class="meeting-control-label">Invite</span>
        <span
          bind:this={inviteTooltipElement}
          class="control-tooltip invite-control-tooltip"
          style={`--invite-tooltip-shift: ${inviteTooltipShift}px`}
          aria-hidden="true"
        >{inviteTooltip}</span>
      </div>
      <div class="control-cell chat-cell" class:collapsed={chatCollapsed} data-control="chat">
        <ControlButton
          icon="chat"
          kind="toggle"
          active={chatOpen}
          label={chatOpen ? 'Close chat' : chatUnread > 0 ? `Open chat, ${chatUnread} unread` : 'Open chat'}
          ariaExpanded={chatOpen}
          onclick={() => onControl?.('chat')}
        />
        {#if chatUnread > 0 && !chatOpen}
          <span class="chat-badge" aria-hidden="true" data-testid="chat-badge">{chatUnread > 99 ? '99+' : chatUnread}</span>
        {/if}
        <span class="meeting-control-label">Chat</span>
      </div>
      {@render pluginActions?.(hiddenPluginKeys)}
      <div class="control-cell chat-cell" data-control="more">
        <ControlButton
          icon="more"
          kind="oneshot"
          label="More meeting controls"
          ariaExpanded={galleryMoreOpen}
          ariaHaspopup="menu"
          onclick={(event) => {
            galleryMoreTriggerEl = event.currentTarget as HTMLButtonElement;
            toggleGalleryMore();
          }}
        />
        {#if chatCollapsed && chatUnread > 0 && !chatOpen}
          <!-- Chat moved into More: its unread count rides on More. -->
          <span class="chat-badge" aria-hidden="true" data-testid="more-chat-badge">{chatUnread > 99 ? '99+' : chatUnread}</span>
        {:else if pluginAttention}
          <!-- A plugin button in More shows a badge: a dot on More. -->
          <span class="more-attention-dot" aria-hidden="true" data-testid="more-attention-dot"></span>
        {/if}
        <span class="meeting-control-label">More</span>
      </div>
      <div class="control-cell leave-cell">
        <ControlButton icon="leave" kind="oneshot" label="Leave meeting" onclick={() => onControl?.('leave')} />
        <span class="meeting-control-label">Leave</span>
      </div>
    </div>

    {#if galleryMoreOpen}
      <div bind:this={galleryMoreMenuEl} class="gallery-more-menu meeting-menu" role="menu" aria-label="More meeting controls">
        <div class="meeting-menu-section-label">More controls</div>
        {#if shareCollapsed}
          <button type="button" class="meeting-menu-row gallery-more-item" role="menuitem" data-testid="more-share" onclick={() => selectGalleryMore('screenshare')}>
            <span class="more-item-leading">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <rect x="3" y="4" width="18" height="13" rx="2"></rect>
                <path d="M8 21h8M12 17v4"></path>
              </svg>
              <span class="meeting-menu-row-copy">{sharingActive ? 'Stop sharing' : 'Share a window'}</span>
            </span>
          </button>
        {/if}
        {#if chatCollapsed}
          <button type="button" class="meeting-menu-row gallery-more-item" role="menuitem" data-testid="more-chat" onclick={() => selectGalleryMore('chat')}>
            <span class="more-item-leading">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M4 5.5A2.5 2.5 0 0 1 6.5 3h11A2.5 2.5 0 0 1 20 5.5v8a2.5 2.5 0 0 1-2.5 2.5H10l-5 4v-4H6.5A2.5 2.5 0 0 1 4 13.5z"></path>
              </svg>
              <span class="meeting-menu-row-copy">{chatOpen ? 'Close chat' : 'Chat'}</span>
            </span>
            {#if chatUnread > 0 && !chatOpen}<span class="more-item-state">{chatUnread > 99 ? '99+' : chatUnread} unread</span>{/if}
          </button>
        {/if}
        {#if inviteCollapsed}
          <button type="button" class="meeting-menu-row gallery-more-item" role="menuitem" data-testid="more-invite" aria-label={inviteAriaLabel} onclick={() => selectGalleryMore('invite')}>
            <span class="more-item-leading">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <circle cx="9" cy="8" r="3.5"></circle>
                <path d="M3 20a6 6 0 0 1 12 0"></path>
                <path d="M16 5.5a3.5 3.5 0 0 1 0 7"></path>
                <path d="M19 20a6 6 0 0 0-4-5.6"></path>
              </svg>
              <span class="meeting-menu-row-copy">Invite</span>
            </span>
          </button>
        {/if}
        {#each morePluginRows as row (row.key)}
          <!-- A plugin button in More: the row clicks the real (hidden) button. -->
          <button type="button" class="meeting-menu-row gallery-more-item" role="menuitem" data-testid="more-plugin" aria-label={row.ariaLabel} disabled={row.disabled} onclick={() => activateMorePluginRow(row.key)}>
            <span class="more-item-leading">
              <span class="more-item-glyph" aria-hidden="true">{@html row.glyph}</span>
              <span class="meeting-menu-row-copy">{row.label}</span>
            </span>
            {#if row.badge}<span class="more-item-state">{row.badge}</span>{/if}
          </button>
        {/each}
        <button type="button" class="meeting-menu-row gallery-more-item" role="menuitem" onclick={() => selectGalleryMore('region')}>
          <span class="more-item-leading">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M4 8V5a1 1 0 0 1 1-1h3M16 4h3a1 1 0 0 1 1 1v3M20 16v3a1 1 0 0 1-1 1h-3M8 20H5a1 1 0 0 1-1-1v-3"></path>
              <rect x="7" y="7" width="10" height="10" rx="1"></rect>
            </svg>
            <span class="meeting-menu-row-copy">Petal View</span>
          </span>
        </button>
        <button type="button" class="meeting-menu-row gallery-more-item" role="menuitemcheckbox" aria-checked={remoteControlAllowed} onclick={() => selectGalleryMore('remotecontrol')}>
          <span class="more-item-leading">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M5 3l5 16 2.5-6.5L19 10z"></path>
            </svg>
            <span class="meeting-menu-row-copy">{remoteControlAllowed ? 'Remote control on' : 'Remote control off'}</span>
          </span>
          <span class="more-item-state">{remoteControlAllowed ? 'On' : 'Off'}</span>
        </button>
        {#if onOpenSettings}
          <button type="button" class="meeting-menu-row gallery-more-item" role="menuitem" onclick={selectGalleryMoreSettings}>
            <span class="more-item-leading">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <circle cx="12" cy="12" r="3"></circle>
                <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
              </svg>
              <span class="meeting-menu-row-copy">Settings</span>
            </span>
          </button>
        {/if}
      </div>
    {/if}
  </div>

</div>

<style>
  .gallery {
    position: relative;
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    border-radius: var(--radius-card);
    overflow: hidden;
    /* Comp-lifted gallery board frame; intentionally distinct from the
       base ramp so the approved mock value stays pixel-exact. */
    background: var(--gallery-frame);
    border: 1px solid var(--hairline-strong);
  }

  /* Frameless: the gallery IS the window — no card frame, fills the route. */
  .gallery.frameless {
    border-radius: 0;
    border: none;
  }

  /* Wraps (#239 parity): in a narrow window the right-hand buttons drop
     under the room name rather than squeezing it to one letter per line. */
  .topbar {
    position: relative;
    flex-shrink: 0;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px 10px;
    min-height: 44px;
    height: auto;
    padding: 5px 18px;
    box-sizing: border-box;
    /* Comp: rgba(255,255,255,.07) — --hairline now carries it (issue #14 item 6). */
    border-bottom: 1px solid var(--hairline);
    background: var(--fill-weak);
  }

  .topbar-drag-layer {
    position: absolute;
    inset: 0;
    z-index: 0;
  }

  .topbar-left,
  .topbar-right {
    position: relative;
    z-index: 1;
  }

  /* Takes the free width itself (it is also a drag region); 160px is the
     least it keeps for the name before the buttons wrap to a second row. */
  .topbar-left {
    display: inline-flex;
    flex: 1 1 160px;
    align-items: center;
    gap: 10px;
    min-height: var(--topbar-control-height);
    height: auto;
    min-width: 0;
    pointer-events: auto;
  }

  /* Centred, not top-aligned (#241): the name and time share the centre line
     of the 24px title actions and of the top-bar controls. */
  .room-title {
    position: relative; /* anchors the rename-error chip */
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }

  /* Transient failure chip under the rename input — a failed rename keeps
     the input open and explains itself instead of silently reverting. */
  .room-rename-error {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    z-index: 5;
    max-width: 280px;
    box-sizing: border-box;
    padding: 4px 8px;
    border-radius: var(--radius-chip);
    background: var(--surface-raised);
    border: 1px solid var(--hairline-strong);
    box-shadow: var(--shadow-pill);
    font: 500 11px var(--font-ui);
    color: var(--danger);
    white-space: normal;
  }

  .room-name {
    font: 600 14.5px / 1.12 var(--font-ui);
    color: var(--text-strong);
    min-width: 0;
    max-width: min(42vw, 320px);
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .room-title-actions {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    width: 24px;
    flex-shrink: 0;
    opacity: 0;
    pointer-events: none;
    transition: opacity var(--motion-fast) var(--ease-standard);
  }

  .room-title-actions.has-rename {
    width: 52px;
  }

  .topbar:hover .room-title-actions,
  .room-title:has(:focus-visible) .room-title-actions {
    opacity: 1;
    /* The title actions intentionally become clickable on full topbar hover. */
    pointer-events: auto;
  }

  .room-title-action {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 24px;
    height: 24px;
    padding: 0;
    margin: 0;
    border: none;
    border-radius: var(--radius-chip);
    background: transparent;
    color: var(--text-dim);
    cursor: pointer;
    flex-shrink: 0;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      scale var(--motion-fast) var(--ease-standard);
  }

  .room-title-action:hover,
  .room-title-action:focus-visible {
    background: var(--fill-base);
    color: var(--text-primary);
  }

  .room-title-action:focus-visible {
    /* Focus outline — no hairline token reaches 0.42; kept literal (uiConsistency allowlist). */
    outline: 1px solid rgba(255, 255, 255, 0.42);
    outline-offset: 2px;
  }

  .room-title-action:active {
    scale: var(--press-scale, 0.96);
  }

  .copy-icon,
  .rename-icon {
    flex-shrink: 0;
    opacity: 0.78;
    transition: opacity var(--motion-fast) var(--ease-standard);
  }

  .room-copy-button:hover .copy-icon,
  .room-copy-button:focus-visible .copy-icon,
  .room-rename-button:hover .rename-icon,
  .room-rename-button:focus-visible .rename-icon {
    opacity: 0.9;
  }

  .room-name-input {
    width: clamp(120px, 22vw, 240px);
    height: 28px;
    padding: 0 8px;
    border-radius: var(--radius-chip);
    border: none;
    background: var(--fill-strong);
    color: var(--text-primary);
    font: 600 14.5px var(--font-ui);
    /* Persistent input outline — kept literal alongside the 0.42 focus outlines (uiConsistency allowlist). */
    outline: 1px solid rgba(255, 255, 255, 0.18);
    box-sizing: border-box;
  }

  /* Owner call: the running time is on demand, not always on. It keeps its
     reserved width, so revealing it never moves the name, and shows while the
     top bar is hovered or keyboard focus is in the title row (the same reveal
     as the title actions; never focus-within, which a mouse click leaves
     stuck on in WebKit -- uiConsistency.test.ts). */
  .elapsed {
    font: 500 12.5px / 1.12 var(--font-mono);
    font-variant-numeric: tabular-nums;
    flex-shrink: 0;
    opacity: 0;
    color: var(--text-faint);
    transition: opacity var(--motion-fast) var(--ease-standard);
  }

  .topbar:hover .elapsed,
  .room-title:has(:focus-visible) .elapsed {
    opacity: 1;
  }

  .topbar-right {
    z-index: 2;
    display: flex;
    align-items: center;
    gap: 8px;
    margin-left: auto;
    /* Not --topbar-control-height: this 34px box sets the bar's 45px height (#241). */
    height: 34px;
    pointer-events: none;
  }

  .topbar-right :global(button) {
    pointer-events: auto;
  }

  .topbar-control-cell {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    width: var(--topbar-control-height);
    height: var(--topbar-control-height);
    pointer-events: auto;
  }

  /* Topbar actions share one quiet 32px icon-button language. Keep the
     specific classes below for behavior/tooltip alignment, but let this
     common class own the interaction states. */
  .chrome-icon-button {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    width: var(--topbar-control-height);
    height: var(--topbar-control-height);
    padding: 0;
    border: none;
    border-radius: var(--radius-control);
    background: var(--fill-strong);
    color: var(--text-soft);
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      scale var(--motion-fast) var(--ease-standard);
  }

  .chrome-icon-button::after {
    content: '';
    position: absolute;
    left: 50%;
    top: 50%;
    width: 40px;
    height: 40px;
    transform: translate(-50%, -50%);
    pointer-events: none;
  }

  .chrome-icon-button:hover,
  .chrome-icon-button:focus-visible {
    background: var(--fill-bright);
    color: var(--text-strong);
  }

  .chrome-icon-button:active {
    scale: var(--press-scale, 0.96);
  }

  .chrome-icon-button:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .topbar-tooltip {
    position: absolute;
    left: 50%;
    top: calc(100% + 8px);
    z-index: 8;
    /* Size to the full label on one line, capped at max-width — WITHOUT this,
       the absolutely-positioned box shrinks to its min-content width, and
       `overflow-wrap: anywhere` (below) makes that a single character, so the
       label renders one letter per line (#223 regression). */
    width: max-content;
    max-width: 148px;
    padding: 5px 8px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-chip);
    background: var(--glass-panel);
    box-shadow:
      var(--shadow-tooltip),
      inset 0 1px 0 var(--fill-strong);
    color: var(--text-soft);
    font: 600 11px var(--font-ui);
    line-height: 1.2;
    text-align: center;
    /* Tooltips are user-facing copy; wrap instead of clipping or ellipsizing (#223). */
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
    opacity: 0;
    pointer-events: none;
    transform: translate(-50%, -4px);
    transition:
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .topbar-control-cell:last-of-type .topbar-tooltip {
    right: 0;
    left: auto;
    transform: translate(0, -4px);
  }

  .topbar-control-cell:hover .topbar-tooltip,
  .topbar-control-cell:has(:focus-visible) .topbar-tooltip {
    opacity: 1;
    transform: translate(-50%, 0);
    transition-delay: var(--motion-tooltip-delay);
  }

  .topbar-control-cell:last-of-type:hover .topbar-tooltip,
  .topbar-control-cell:last-of-type:has(:focus-visible) .topbar-tooltip {
    transform: translate(0, 0);
  }

  .layout-toggle {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    width: var(--topbar-control-height);
    height: var(--topbar-control-height);
    padding: 0;
    border: none;
    border-radius: var(--radius-chip);
    background: transparent;
    color: var(--text-soft);
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      scale var(--motion-fast) var(--ease-standard);
  }

  .layout-toggle::after {
    content: '';
    position: absolute;
    left: 50%;
    top: 50%;
    width: 40px;
    height: 40px;
    transform: translate(-50%, -50%);
    pointer-events: none;
  }

  .layout-toggle:hover,
  .layout-toggle:focus-visible {
    background: var(--fill-bright);
    color: var(--text-strong);
  }

  .layout-toggle:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .layout-toggle:active {
    scale: var(--press-scale, 0.96);
  }

  /* Bug report (#786): same quiet icon-button treatment as the two cells it
     sits between — never a colored call-to-action in the topbar. */
  .report-bug {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    width: var(--topbar-control-height);
    height: var(--topbar-control-height);
    padding: 0;
    border: none;
    border-radius: var(--radius-chip);
    background: transparent;
    color: var(--text-soft);
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      scale var(--motion-fast) var(--ease-standard);
  }

  .report-bug:hover,
  .report-bug:focus-visible {
    background: var(--fill-bright);
    color: var(--text-strong);
  }

  .report-bug:active {
    scale: var(--press-scale, 0.96);
  }

  .report-bug:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  /* Share-blocked: dimmed and not-allowed, but still hoverable AND focusable
     (aria-disabled, not disabled) so the tooltip can state the reason. */
  .report-bug.blocked {
    color: var(--text-dim);
    cursor: not-allowed;
  }

  .report-bug.blocked:hover,
  .report-bug.blocked:focus-visible {
    background: transparent;
    color: var(--text-dim);
  }

  .report-bug.blocked:active {
    scale: 1;
  }

  .topbar-right :global(.control-button.size-compact) {
    background-color: transparent;
    color: var(--text-soft);
    opacity: 1;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .topbar-right :global(.control-button.size-compact:hover:not(:disabled)),
  .topbar-right :global(.control-button.size-compact:focus-visible) {
    background-color: var(--fill-strong);
    color: var(--text-strong);
    opacity: 1;
  }

  .topbar-right :global(.control-button.size-compact:focus-visible) {
    /* Focus outline — kept literal (uiConsistency allowlist). */
    outline: 1px solid rgba(255, 255, 255, 0.42);
    outline-offset: 2px;
  }

  /* Network icon: quiet graphite chip treatment as a button. */
  .net-btn {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    width: var(--topbar-control-height);
    height: var(--topbar-control-height);
    padding: 0;
    border: none;
    border-radius: var(--radius-chip);
    background: transparent;
    color: var(--text-soft);
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      scale var(--motion-fast) var(--ease-standard);
  }

  .net-btn::after {
    content: '';
    position: absolute;
    left: 50%;
    top: 50%;
    width: 40px;
    height: 40px;
    transform: translate(-50%, -50%);
  }

  .net-btn:hover,
  .net-btn:focus-visible {
    background: var(--fill-bright);
    color: var(--text-strong);
  }

  .net-btn:active {
    scale: var(--press-scale, 0.96);
  }

  .net-btn:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  @media (prefers-reduced-motion: reduce) {
    .layout-toggle,
    .net-btn,
    .report-bug,
    .topbar-right :global(.control-button.size-compact) {
      transition: none;
    }

    .layout-toggle:active,
    .net-btn:active,
    .report-bug:active {
      scale: 1;
    }

    .topbar-right :global(.control-button.size-compact:active:not(:disabled)) {
      transform: none;
    }

    .topbar-tooltip {
      transition: none;
    }
  }

  /* Smart tile grid (#203): rows/columns come from participant count and the
     live tile-surface aspect. The CSS only places those computed cells and
     preserves the 16:9 tile box; it does not reintroduce scroll. */
  /* Tiles and the side panel (the meeting chat) share the row between the
     topbar and the control bar. The panel is a 320 px column beside the
     tiles while the tiles keep at least 400 px; narrower, it covers the tiles
     instead of crushing them, and never the topbar or the controls. */
  .gallery-body {
    position: relative;
    flex: 1;
    min-height: 0;
    min-width: 0;
    display: flex;
    container: gallery-body / inline-size;
  }
  .side-panel {
    flex: none;
    width: 320px;
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .side-panel > :global(*) {
    flex: 1;
    min-height: 0;
  }
  @container gallery-body (max-width: 719px) {
    .side-panel {
      position: absolute;
      inset: 0;
      width: auto;
      z-index: 5;
    }
  }
  .control-cell.collapsed {
    display: none;
  }

  .tiles {
    flex: 1;
    min-height: 0;
    min-width: 0;
    --gallery-gap: 18px;
    --gallery-cols: 1;
    --gallery-rows: 1;
    --gallery-tile-width: 100%;
    --gallery-tile-height: 100%;
    /* Comp grid metrics: padding 28px, gap 18px (were 20px/16px — issue #14
       item 2). */
    padding: 28px;
    overflow: hidden;
    overscroll-behavior: none;
  }

  /* #239 (parity with the browser client): a track is no larger than the
     packed tile and the block of tracks is centred, so the space the 16:9
     tiles cannot use surrounds the group instead of opening dead bands
     between neighbours. `safe` falls back to start rather than clip. */
  .tiles.grid {
    display: grid;
    grid-template-columns: repeat(var(--gallery-cols), minmax(0, var(--gallery-tile-width)));
    grid-template-rows: repeat(var(--gallery-rows), minmax(0, var(--gallery-tile-height)));
    place-items: center;
    /* `center` first: WebKit before 17.6 (older macOS WKWebView) drops the
       `safe` declaration. */
    place-content: center;
    place-content: safe center;
    gap: var(--gallery-gap);
  }

  .tiles.grid.scrollable {
    overflow: hidden;
  }

  .tiles.grid.with-state {
    /* The state card gets its own full-width row; the tiles below keep the
       COMPUTED row tracks (--gallery-rows). The old `grid-template-rows:
       auto` discarded the computed tracks, so tiles auto-placed into
       indefinite-height rows and `min(100%, var(--gallery-tile-height))`
       resolved against auto rows — visibly broken whenever a state card
       coexisted with participants. */
    grid-template-rows: auto repeat(var(--gallery-rows), minmax(0, var(--gallery-tile-height)));
  }

  .gallery-state {
    grid-column: 1 / -1;
    grid-row: 1;
    align-self: center;
    justify-self: center;
    width: min(420px, 100%);
    padding: 18px 20px;
    border: 1px solid var(--fill-strong);
    border-radius: var(--radius-card);
    background: var(--fill-weak);
    text-align: center;
  }

  .gallery-state.warning {
    border-color: color-mix(in srgb, var(--warning) 30%, var(--fill-strong));
  }

  .gallery-state-title,
  .gallery-state-detail {
    margin: 0;
  }

  .gallery-state-title {
    font: 700 var(--text-body) var(--font-ui);
    color: var(--text-primary);
  }

  .gallery-state-detail {
    margin-top: 6px;
    font: 500 var(--text-caption) var(--font-ui);
    color: var(--text-soft);
  }

  /* flex column (not block, #676): `.gallery-state` can render as a sibling
     of `.spotlight-layout` (see the markup below -- gridStateTitle is
     independent of spotlightActive) while `.spotlight-layout` also claims
     height:100%. Under block layout those two heights simply add, overflow
     the fixed-height container, and `overflow:hidden` above silently clips
     the rail off the bottom. Flex lets `.gallery-state` keep its natural
     height (flex: 0 0 auto below) while `.spotlight-layout` absorbs exactly
     what's left (flex: 1 1 0 below), instead of both independently claiming
     the full height. */
  .tiles.spotlight {
    display: flex;
    flex-direction: column;
    padding: 22px;
    overflow: hidden;
    overscroll-behavior: none;
  }

  .tiles.spotlight .gallery-state {
    flex: 0 0 auto;
  }

  .spotlight-layout {
    /* `flex: 1 1 0`, NOT `1 1 auto` -- with an `auto` basis the flex item's
       resolved height is seeded from a percentage of a flex container's
       content size, which at least one browser treats as indefinite (#676),
       and the rail inside needs a definite height to pack the spotlight. */
    flex: 1 1 0;
    min-height: 0;
  }

  /* The participant tree is shared by both presentations. In grid mode the
     persistent layout/rail wrappers become transparent sizing layers, while
     the rail keeps the smart grid tracks: half-column tracks (each tile spans
     two), so an incomplete last row can start half a tile in and sit centred
     under the full ones (tailColumnStart, the web client's #239 rule). */
  .tiles.grid > .spotlight-layout {
    grid-column: 1 / -1;
    grid-row: 1 / -1;
    align-self: stretch;
    justify-self: stretch;
    min-width: 0;
    min-height: 0;
    width: 100%;
    height: 100%;
    display: block;
  }

  .tiles.grid.with-state > .spotlight-layout {
    grid-row: 2 / -1;
  }

  .tiles.grid .spotlight-rail {
    display: grid;
    grid-template-columns: repeat(var(--gallery-half-cols), minmax(0, 1fr));
    grid-template-rows: repeat(var(--gallery-rows), minmax(0, 1fr));
    place-items: center;
    gap: var(--gallery-gap);
    width: 100%;
    height: 100%;
    min-width: 0;
    min-height: 0;
    padding: 0;
    /* overflow: hidden removed — tiles are size-constrained by min(100%, tile_width)
       and cannot overflow. Removing it lets the speaking ring's box-shadow
       render in the .tiles 28px padding area instead of being clipped. */
    overflow: visible;
    white-space: normal;
  }

  /* Spotlight (#239 parity, shared/logic/spotlightGeometry.ts): the hero and
     ONE thumbnail size for everyone come from the shared geometry as
     --hero-w/--hero-h/--thumb-w/--thumb-h on the rail. `.side` puts the
     thumbnails in a column right of the hero (a wide window); `.below` puts
     them in centred rows under it (a tall window) -- centred, never hugging
     the left edge. When they need more room than there is (`.scrolls`) the
     rail scrolls and the hero stays at its top. The rail is still the one
     keyed list, so a tile moving between hero and thumbnail never remounts
     its camera. */
  .tiles.spotlight .spotlight-layout {
    display: flex;
    min-width: 0;
  }

  .tiles.spotlight .spotlight-rail {
    position: relative;
    width: 100%;
    height: 100%;
    min-width: 0;
    min-height: 0;
    box-sizing: border-box;
    /* 6px: the sharing ring's glow (0 0 8px -2px) reaches ~6px out; the
       scroller would clip it at its edge. */
    padding: 6px;
    gap: var(--spotlight-gap, 12px);
    overflow-x: hidden;
    overflow-y: auto;
    overscroll-behavior: contain;
  }

  .tiles.spotlight .spotlight-rail.below {
    display: flex;
    flex-wrap: wrap;
    justify-content: center;
    /* `center` first: WebKit before 17.6 drops the `safe` declaration. */
    align-content: center;
    align-content: safe center;
  }

  .tiles.spotlight .spotlight-rail.side {
    display: grid;
    grid-template-columns: var(--hero-w) repeat(var(--strip-cols, 1), var(--thumb-w));
    grid-template-rows: repeat(var(--strip-rows), var(--thumb-h));
    justify-content: center;
    align-content: center;
    align-content: safe center;
  }

  .tiles.spotlight .spotlight-rail.scrolls {
    align-content: start;
  }

  .tiles.spotlight .spotlight-main {
    position: sticky;
    top: 0;
    z-index: 2;
    flex: 0 0 auto;
    width: var(--hero-w, 100%);
    height: var(--hero-h, 100%);
  }

  /* Margins that add up to the whole line: the hero sits alone on its row,
     centred, and the thumbnails wrap into the rows below it. */
  .tiles.spotlight .spotlight-rail.below > .spotlight-main {
    margin-inline: calc((100% - var(--hero-w, 100%)) / 2);
  }

  .tiles.spotlight .spotlight-rail.side > .spotlight-main {
    grid-column: 1;
    grid-row: 1 / -1;
    align-self: center;
  }

  .tiles.spotlight .spotlight-rail.side.scrolls > .spotlight-main {
    align-self: start;
  }

  /* Wraps each tile so the join/leave scale transition above has a stable
     grid cell to animate within (Svelte transitions apply directly to the
     element they're on, so this needs to be the grid item itself, not a
     style on ParticipantTile's own root). */
  .tile-wrap {
    /* Default for the #875 sharing ring; overridden inline per participant
       with their identity tint. Declared here so the ring always resolves to
       a real token colour even before a tint arrives. */
    --sharing-tint: var(--text-primary);
    position: relative;
    min-width: 0;
    min-height: 0;
    border-radius: var(--radius-tile);
    cursor: pointer;
    transition:
      filter var(--motion-feedback) var(--ease-standard),
      scale var(--motion-feedback) var(--ease-standard);
  }

  /* #875: a sharing participant must be identifiable at a glance. The
     `sharing` class was already applied to the tile but nothing styled it, so
     a sharer looked identical to everyone else -- the owner's report was
     "it's difficult to determine when someone is sharing". The ring uses that
     participant's own identity tint (`--sharing-tint`, the same colour their
     share border and remote-window header use), so who-is-sharing and
     which-window-is-theirs read as one system. Ring only: no layout shift, so
     it cannot reflow the grid or clip a tile's name (the never-truncate
     rule). */
  .tile-wrap.sharing {
    /* Token-only ring (no literal colour): `--sharing-tint` is set inline
       from the participant's identity colour, and falls back to the base
       rule's token when that participant has no tint yet. Same pattern as
       LiveHero's `--live-face-ring`. */
    box-shadow:
      0 0 0 2px var(--sharing-tint),
      0 0 8px -2px var(--sharing-tint);
  }

  .tile-wrap:hover {
    filter: brightness(1.04);
  }

  .tile-wrap:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 4px;
  }

  .tile-wrap:active {
    scale: 0.99;
  }

  .tile-wrap.spotlight-main {
    min-height: 0;
  }

  .tile-wrap.spotlight-thumb {
    /* flex: 0 0 auto -- never grow past, never shrink below, the shared
       geometry's one thumbnail size (#676, #239 parity). */
    flex: 0 0 auto;
    width: var(--thumb-w, 160px);
    height: var(--thumb-h, 90px);
    /* Establishes a query container so the typography rules below (and
       ParticipantTile's own descendants) can scale by the thumb's actual
       rendered size (via `cqh`) instead of another arbitrary fixed px (#676).
       Safe to use `size` containment here: both dimensions are extrinsic. */
    container-type: size;
    container-name: spotlight-thumb;
  }

  /* #676: `.camera-off-name` is otherwise a flat 700 26px (ParticipantTile.svelte)
     regardless of tile size -- comically large against a ~100px-tall rail
     thumbnail. `19cqh` scales with the thumb's real rendered height (via the
     container above) rather than pinning yet another breakpoint-specific px,
     clamped to a sane range so it never grows past the original 26px cap or
     shrinks to the point of being unreadable. */
  .tile-wrap.spotlight-thumb :global(.camera-off-name) {
    font-size: clamp(12px, 19cqh, 20px);
  }

  .tiles.grid .tile-wrap {
    grid-column-end: span 2;
    width: min(100%, var(--gallery-tile-width));
    height: min(100%, var(--gallery-tile-height));
    aspect-ratio: 16 / 9;
  }

  /* Also applies to spotlight rail thumbnails (#676): `.tiles.grid.compact`
     was already tuned for a small ~170x105px tile -- close to a rail thumb's
     own ~187x105px -- but could never match one, because `.tiles.spotlight`
     carries `class:spotlight`, never `class:grid` (see the markup above), so
     this selector was unreachable for the one case that most needed it. */
  .tiles.grid.compact .tile-wrap :global(.name-chip),
  .tile-wrap.spotlight-thumb :global(.name-chip) {
    left: 10px;
    bottom: 9px;
    max-width: calc(100% - 20px);
    padding: 4px 8px;
    font-size: 11px;
  }

  .tiles.grid.compact .tile-wrap :global(.muted-chip),
  .tile-wrap.spotlight-thumb :global(.muted-chip) {
    right: 9px;
    bottom: 9px;
  }

  /* #875: shrink the pill to match the compact chip scale-down above, same
     breakpoint. */
  .tiles.grid.compact .tile-wrap :global(.share-count-pill),
  .tile-wrap.spotlight-thumb :global(.share-count-pill) {
    left: 9px;
    top: 9px;
    min-width: 17px;
    height: 17px;
    padding: 0 5px;
    font-size: 10px;
  }

  .tiles.grid.tiny .tile-wrap :global(.name-chip) {
    right: 8px;
    max-width: calc(100% - 16px);
  }

  .tiles.grid.tiny .tile-wrap :global(.muted-chip) {
    display: none;
  }

  /* #875: hidden entirely at `tiny` -- drop, never shrink past legibility
     (the muted-chip precedent above). */
  .tiles.grid.tiny .tile-wrap :global(.share-count-pill) {
    display: none;
  }

  .pin-mark {
    position: absolute;
    top: 10px;
    right: 10px;
    z-index: 2;
    display: flex;
    align-items: center;
    justify-content: center;
    width: 24px;
    height: 24px;
    border-radius: var(--radius-chip);
    background: var(--glass-chip);
    color: var(--text-strong);
    box-shadow: inset 0 0 0 1px var(--hairline-strong);
    backdrop-filter: blur(8px);
  }

  /* ParticipantTile's root is a plain relative div whose children are all
     absolutely positioned — without an explicit height it collapses to 0
     inside this block wrapper (pre-existing bug from when .tile-wrap was
     introduced for the M4 join/leave transition: the grid stretches the
     wrapper, but nothing stretched the tile inside it; found during issue
     #14 verification — the gallery tiles were rendering 0px tall). Same
     `:global(.tile)` sizing pattern the /dev/components harness frames use. */
  .tile-wrap :global(.tile) {
    width: 100%;
    height: 100%;
  }

  .controlbar {
    position: relative;
    z-index: 6;
    flex-shrink: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    /* Comp: a 104px bar with 28px side padding and vertically centered
       content (was 14px/24px padding with intrinsic height — issue #14
       item 7). min-height (not height) + kept vertical padding so the
       #94: this row never wraps; the meeting window flips to pill at the
       one-row breakpoint before a second row can appear. */
    min-height: 82px;
    padding: 15px 28px;
    /* Comp: border rgba(255,255,255,.07), controlbar-bg — --hairline now
       carries the comp border (issue #14 item 7). */
    border-top: 1px solid var(--hairline);
    background: var(--controlbar-bg);
    flex-wrap: nowrap;
  }

  .chat-cell {
    position: relative;
  }
  .chat-badge {
    position: absolute;
    top: -2px;
    left: calc(50% + 8px);
    min-width: 16px;
    height: 16px;
    padding: 0 4px;
    border-radius: var(--radius-pill);
    background: var(--live-bright);
    color: var(--bg-base);
    font: 600 10px/16px var(--font-ui);
    text-align: center;
    pointer-events: none;
  }

  .controls-cluster {
    display: flex;
    align-items: flex-start;
    justify-content: center;
    gap: 12px;
    flex-wrap: nowrap;
  }

  /* Never shrunk: the row's fit is measured from the cells' own widths
     (refitControls), and a squeezed row would read as one that fits. */
  .control-cell {
    position: relative;
    display: flex;
    flex: 0 0 auto;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    min-width: 52px;
  }

  /* Comp control circles are 52px (glyph stays 20px) in this bar — larger
     than ControlButton's own 44px "comfortable" template (canvas.html's
     circular-button matrix renders at 44px only "to keep the feature
     comparison legible"; the approved gallery board draws them 52px).
     Scoped override here (issue #14 item 7) so ControlButton's shared 44px
     template is untouched for every other surface; !important is required
     because ControlButton sets width/height as inline styles. */
  .control-cell :global(.control-button) {
    width: 52px !important;
    height: 52px !important;
  }

  .control-tooltip {
    position: absolute;
    left: 50%;
    bottom: calc(100% + 9px);
    z-index: 8;
    max-width: 148px;
    padding: 5px 8px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-chip);
    background: var(--glass-panel);
    box-shadow:
      var(--shadow-tooltip),
      inset 0 1px 0 var(--fill-strong);
    color: var(--text-soft);
    font: 600 11px var(--font-ui);
    line-height: 1.2;
    text-align: center;
    white-space: nowrap;
    opacity: 0;
    pointer-events: none;
    transform: translate(-50%, 4px);
    transition:
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  /* The public room ID plus action hint is intentionally complete UI copy.
     It must wrap within a narrow gallery rather than clip or ellipsize. */
  .invite-control-tooltip {
    width: min(220px, calc(100vw - 24px));
    box-sizing: border-box;
    white-space: normal;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    transform: translate(calc(-50% + var(--invite-tooltip-shift, 0px)), 4px);
  }

  .control-cell:hover .control-tooltip,
  .control-cell:has(:focus-visible) .control-tooltip {
    opacity: 1;
    transform: translate(-50%, 0);
    transition-delay: var(--motion-tooltip-delay);
  }

  .control-cell:hover .invite-control-tooltip,
  .control-cell:has(:focus-visible) .invite-control-tooltip {
    transform: translate(calc(-50% + var(--invite-tooltip-shift, 0px)), 0);
  }

  .leave-cell {
    margin-left: 6px;
  }

  .gallery-more-menu {
    position: absolute;
    left: 50%;
    bottom: calc(100% + 10px);
    z-index: 21;
    width: min(236px, calc(100% - 24px));
    transform: translateX(-50%);
  }

  .more-item-leading {
    display: inline-flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
  }

  .more-item-leading svg {
    width: 16px;
    height: 16px;
    flex: 0 0 auto;
    color: var(--text-soft);
  }

  .more-item-glyph {
    display: inline-flex;
    flex: 0 0 auto;
  }

  .more-item-glyph :global(svg) {
    width: 16px;
    height: 16px;
    color: var(--text-soft);
  }

  .more-attention-dot {
    position: absolute;
    top: 2px;
    left: calc(50% + 12px);
    width: 8px;
    height: 8px;
    border-radius: var(--radius-pill);
    background: var(--live-bright);
    pointer-events: none;
  }

  .more-item-state {
    flex: 0 0 auto;
    color: var(--text-dim);
    font: 700 10px / 1 var(--font-ui);
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  /* Super compact (the last step of the control-row fit, and the rail): icon
     only and a size smaller. Every button keeps its accessible name; the
     labels are what go, as on the web client's landscape-phone rail. */
  .gallery.compact-controls {
    --compact-control: 40px;
  }

  .gallery.compact-controls .controlbar :global(.meeting-control-label) {
    display: none;
  }

  .gallery.compact-controls .control-cell {
    min-width: 0;
  }

  .gallery.compact-controls .control-cell :global(.control-button),
  .gallery.compact-controls :global(.media-split-gallery .control-button) {
    width: var(--compact-control) !important;
    height: var(--compact-control) !important;
  }

  .gallery.compact-controls :global(.media-split-gallery) {
    --meeting-split-height: var(--compact-control);
    --meeting-split-options-width: 20px;
  }

  .gallery.compact-controls :global(.plugin-button) {
    width: var(--compact-control);
    height: var(--compact-control);
  }

  /* Icon only is still one row: Mic, Camera, More and Leave fit the
     narrowest gallery (240 px) with these gaps. */
  .gallery.compact-controls:not(.rail-controls) .controlbar {
    min-height: 0;
    padding: 8px;
  }

  .gallery.compact-controls:not(.rail-controls) .controls-cluster {
    gap: 6px;
  }

  .gallery.compact-controls .leave-cell {
    margin-left: 0;
  }

  /* A narrow or short window gives the tiles its edges. */
  .gallery.compact-window .tiles {
    padding: 12px;
  }

  /* The rail: a short, wide window -- a bar of faces along the top of the
     screen. The controls stand in a column at the right edge (filling top to
     bottom, then another column: --rail-rows), and the top bar floats over
     the top of the tiles (below), so the faces get nearly the whole height.
     The rail's empty space moves the window. */
  .gallery.rail-controls {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    grid-template-rows: minmax(0, 1fr);
  }

  .gallery.rail-controls > .topbar {
    grid-area: 1 / 1;
    align-self: start;
  }

  .gallery.rail-controls > .gallery-body {
    grid-area: 1 / 1;
  }

  .gallery.rail-controls > .controlbar {
    grid-area: 1 / 2;
    min-height: 0;
    padding: 8px;
    border-top: none;
    border-left: 1px solid var(--hairline);
  }

  /* The floating top bar (a bar of faces, or a short column): over the top
     of the tiles, shown while the pointer is over the window or keyboard
     focus is in it. Only its buttons take clicks -- its surface passes them
     to the tiles beneath (the pin, the share-count pill), so it can cover
     nothing while visible either. The control bar and its menus stay above it. */
  .gallery.floating-topbar > .topbar {
    z-index: 7;
    min-height: 44px;
    height: auto;
    padding: 5px 12px;
    border-bottom: none;
    background: color-mix(in srgb, var(--bg-base-2) 86%, transparent);
    backdrop-filter: blur(12px);
    opacity: 0;
    transition: opacity var(--motion-fast) var(--ease-standard);
  }

  .gallery.floating-topbar:not(.rail-controls) > .topbar {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
  }

  .gallery.floating-topbar > .topbar,
  .gallery.floating-topbar > .topbar .topbar-drag-layer,
  .gallery.floating-topbar > .topbar .topbar-left {
    pointer-events: none;
  }

  .gallery.floating-topbar:hover > .topbar,
  .gallery.floating-topbar > .topbar:has(:focus-visible) {
    opacity: 1;
  }

  .gallery.floating-topbar:hover > .topbar :is(button, input),
  .gallery.floating-topbar > .topbar:has(:focus-visible) :is(button, input) {
    pointer-events: auto;
  }

  /* The bar's own reveals key on the window being pointed at, since the bar
     itself is no longer hit-tested. */
  .gallery.floating-topbar:hover .room-title-actions {
    opacity: 1;
    pointer-events: auto;
  }

  .gallery.floating-topbar:hover .elapsed {
    opacity: 1;
  }

  .gallery.floating-topbar > .controlbar {
    z-index: 8;
  }

  /* With a panel open (the chat) the top bar is a row again: floating, it
     would sit over the panel's own header and its Close button. */
  .gallery.floating-topbar.has-side-panel > .topbar {
    position: relative;
    opacity: 1;
    background: var(--fill-weak);
    backdrop-filter: none;
    border-bottom: 1px solid var(--hairline);
    pointer-events: auto;
  }

  .gallery.floating-topbar.has-side-panel > .topbar .topbar-drag-layer,
  .gallery.floating-topbar.has-side-panel > .topbar .topbar-left,
  .gallery.floating-topbar.has-side-panel > .topbar :is(button, input) {
    pointer-events: auto;
  }

  .gallery.rail-controls.has-side-panel {
    grid-template-rows: auto minmax(0, 1fr);
  }

  .gallery.rail-controls.has-side-panel > .gallery-body {
    grid-area: 2 / 1;
  }

  .gallery.rail-controls.has-side-panel > .controlbar {
    grid-area: 1 / 2 / 3 / 3;
  }

  .gallery.rail-controls .controls-cluster {
    display: grid;
    grid-auto-flow: column;
    grid-template-rows: repeat(var(--rail-rows), auto);
    align-content: center;
    align-items: center;
    justify-items: center;
    gap: 6px;
  }

  .gallery.rail-controls .tiles {
    padding: 8px;
  }

  /* The More menu opens beside the rail, not above it. */
  .gallery.rail-controls .gallery-more-menu {
    left: auto;
    right: calc(100% + 8px);
    bottom: 8px;
    transform: none;
  }

  @media (prefers-reduced-motion: reduce) {
    .control-tooltip,
    .gallery.floating-topbar > .topbar {
      transition: none;
    }
  }
</style>
