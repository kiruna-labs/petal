<!--
  MeetingChrome — the floating in-meeting chrome that scales between DESIGN.md
  §2's two anchor states, with a real animated transition between them (not
  two static screens). Per §2: "the meeting UI that floats while the actual
  shared windows live as separate native windows on the desktop... spec the
  two anchor states and the transition between them."

  This fills a real gap: before this component, `Gallery.svelte` was the
  large-state content and `Pill.svelte` was the small-state shell (per
  Toast.svelte's own doc comment: "Pill = in-meeting small state, and the
  reconnection/status toast"), but nothing actually transitioned between
  them — each only ever existed on its own route/dev-harness section as a
  static state. `MeetingChrome` is the missing wrapper: it hosts the existing
  `Gallery` for the large state, and a new small-state floating bar (built
  here, using the existing `Pill` shell per Build-Map §2.2) for the small
  state, cross-fading between them.

  Behavior (DESIGN.md §2): "click to expand large↔small; the large↔small
  transition is one of the few places a slightly richer animation is
  warranted (it's direct manipulation, and it orients the user)." The
  transition uses the semantic enter role and standard easing; the pill
  geometry remains unscaled so its shadow and hit target stay stable.

  View switcher (issue #55): a state-aware collapse/expand glyph. Large
  state: a compact circular button slotted into the Gallery topbar's right
  end (via Gallery's `topbarAction` snippet prop). Small state: an extra
  ControlButton-style circle INSIDE the pill row (icon="expand"), calling
  toggleExpanded() directly — deliberately NOT routed through `onControl`,
  which is the media-controls channel.

  Pill overflow (issue #6/#168): the pill renders as many feature controls
  (Audio, Video, Screensharing, Remote control, Invite) as fit the available
  width — measured via ResizeObserver on the small stage (whose width is the
  chrome/window width, independent of the pill's own content, so the fit
  calculation can never feed back into itself and flicker at the boundary).
  Anything that doesn't fit collapses into a "More" circle opening a tiny
  vertical menu above the pill. The guaranteed floor is avatar + Audio +
  Video + Screensharing, with More budgeted when lower-priority controls
  overflow. Expand and Leave (issue #668) are never part of this fit/overflow
  loop at all — they're always-present trailing circles, in that order, so
  Leave is always the genuinely last control. The room name never renders in
  pill mode.

  More-menu placement (issue #33): the menu is placed by measurement inside
  the stage (== the pill window's bounds) — preferred side, flip, then
  clamp + internal scroll — because the pill window is deliberately sized
  tight to the pill (no invisible allowance area: it would block desktop
  clicks), so pure CSS anchoring above/beside the pill clipped past the
  webview edge whenever the More button existed at all. See the placement
  effect in the script block.
-->
<script lang="ts">
  import { onDestroy } from 'svelte';
  import { attachVideoStream } from '$lib/videoAttachment';
  import Gallery, { type GalleryParticipant } from './Gallery.svelte';
  import Pill from '@petal/shared/ui/components/Pill.svelte';
  import Avatar from './Avatar.svelte';
  import ControlButton, { type ControlIcon } from './ControlButton.svelte';
  import MediaSplitControl from './MediaSplitControl.svelte';
  import DevicePicker from './DevicePicker.svelte';
  import {
    restrainedSurfaceEnterTransition,
    restrainedSurfaceExitTransition
  } from '$lib/motion';
  import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';
  import type { IdentityColor } from './Avatar.svelte';
  import type { DrawUpdate } from '$lib/ipc';

  type PillResizeDirection =
    | 'East'
    | 'North'
    | 'NorthEast'
    | 'NorthWest'
    | 'South'
    | 'SouthEast'
    | 'SouthWest'
    | 'West';

  export interface PillHost {
    orientation?: 'horizontal' | 'vertical';
    onDrag?: (e: MouseEvent) => void;
    onResize?: (direction: PillResizeDirection) => void;
    onCompactChange?: (expanded: boolean) => void;
    onPopupChange?: (open: boolean) => void;
    popupOpen?: boolean;
  }

  interface Props {
    roomName?: string;
    elapsed?: string;
    participants?: GalleryParticipant[];
    cameraDrawUpdates?: DrawUpdate[];
    /** Active-speaker (or first) participant's identity color, for the small
     * state's single avatar-stack-of-one per DESIGN.md §2's "tight avatar
     * stack (or the active speaker only)". */
    activeIdentity?: IdentityColor;
    /** Concrete meeting-scoped color for activeIdentity after de-collision. */
    activeColor?: string;
    micMuted?: boolean;
    cameraOn?: boolean;
    sharingActive?: boolean;
    /** Whether the native share picker is currently open. */
    sharingPickerOpen?: boolean;
    /** Local sharer's identity color, used by active Screensharing controls. */
    sharingLiveBackground?: string;
    /** Contrast-safe text/icon color for sharingLiveBackground. */
    sharingLiveColor?: string;
    remoteControlAllowed?: boolean;
    stateTitle?: string | null;
    stateDetail?: string | null;
    stateTone?: 'info' | 'warning';
    /** Large (comfortable) vs. small (collapsed floating bar). Bindable so a
     * parent can drive it (e.g. a future drag-to-collapse gesture) as well
     * as read it back. */
    expanded?: boolean;
    onControl?: (icon: ControlIcon) => void;
    /** Public-code disclosure passed from the route; never derive it from the
     * opaque meeting credential inside the presentation layer. */
    inviteAriaLabel?: string;
    inviteTooltip?: string;
    onInviteLinkCopy?: () => void | Promise<void>;
    onOpenNetwork?: () => void | Promise<void>;
    onRenameRoom?: (displayName: string | null) => void | Promise<void>;
    /** Pass-through to Gallery's topbar bug-report cell (#786). Undefined on a
     * build with no UserDispatch key, which is what removes the cell. */
    onReportBug?: () => void;
    /** Pass-through to Gallery: real routes render the gallery edge-to-edge
     * (the gallery IS the window); the /dev harness keeps the framed card. */
    frameless?: boolean;
    /** Pill orientation (issue #12): 'vertical' stacks the pill —
     * avatar/identity circle on top, control circles below in the same
     * order. The room name is HIDDEN in vertical (pre-approved judgment
     * call: no rotated text, keep it quiet). Driven by the meeting route's
     * screen-edge detection; the /dev harness can set it directly. */
    pillHost?: PillHost;
    /** Local webcam self-view stream (issue #9): when set AND
     * `cameraOn` is true, the pill's identity circle renders this live
     * stream (circular clip, mirrored) instead of the Avatar. The stream is
     * OWNED by the parent (the meeting route's #7 getUserMedia self-view) —
     * this component never opens its own capture, so collapsing/expanding
     * never restarts the camera. */
    localVideoStream?: MediaStream | null;
  }

  let {
    roomName = 'eng-sync',
    elapsed = '24:18',
    participants = [],
    cameraDrawUpdates = [],
    activeIdentity = 'blue',
    activeColor,
    micMuted = false,
    cameraOn = false,
    sharingActive = true,
    sharingPickerOpen = false,
    sharingLiveBackground,
    sharingLiveColor,
    remoteControlAllowed = true,
    stateTitle = null,
    stateDetail = null,
    stateTone = 'info',
    expanded = $bindable(true),
    onControl,
    inviteAriaLabel = 'Copy invite link',
    inviteTooltip = 'Copy invite link',
    onInviteLinkCopy,
    onOpenNetwork,
    onRenameRoom,
    onReportBug,
    frameless = false,
    pillHost,
    localVideoStream = null,
  }: Props = $props();

  const orientation = $derived(pillHost?.orientation ?? 'horizontal');
  const popupHostOpen = $derived(pillHost?.popupOpen ?? false);
  const screenshareControlActive = $derived(sharingActive || sharingPickerOpen);

  function toggleExpanded() {
    expanded = !expanded;
  }

  // ---- Pill webcam self-view circle (issue #9) ----------------------
  // Camera on + stream present → the identity circle is the live self-view.
  // Camera off (or no stream yet) → the existing Avatar, exactly as before.
  const selfVideoOn = $derived(cameraOn && !!localVideoStream);
  // srcObject can't be set via a template attribute — bind the element and
  // assign in an effect (same pattern as ParticipantTile/Settings).
  let selfVideoEl = $state<HTMLVideoElement | null>(null);
  let pillInteractive = $state(false);
  let pillCompactTimer: ReturnType<typeof setTimeout> | null = null;
  $effect(() => {
    const el = selfVideoEl;
    if (!el) return;
    const changed = attachVideoStream(el, localVideoStream);
    if (changed && localVideoStream) {
      // Belt-and-suspenders next to the `autoplay` attribute: Chromium
      // doesn't always honor autoplay for a framework-set `muted` on a
      // freshly (re)mounted element (observed live: paused=true after a
      // camera off→on remount). play() on a muted element needs no gesture.
      void el.play().catch(() => {});
    }
  });

  /** Natural (layout) size of the pill itself — offsetWidth/offsetHeight, NOT
   * getBoundingClientRect (the small stage carries a scale transform while
   * hidden, which would skew a rect measurement by 4%). The meeting route
   * uses this to size the real pill-mode window (+ shadow margin). */
  export function measurePill(): { width: number; height: number } | null {
    const el = pillAnchorEl;
    if (!el) return null;
    return { width: el.offsetWidth, height: el.offsetHeight };
  }

  /** Content-derived minimum for user-resizing the compact window. It is the
   * smallest useful pill: identity circle + mic + camera + screenshare +
   * leave, plus More/Expand circles. The route adds shadow margin around it. */
  export function measurePillMinimum(): { width: number; height: number } {
    const minControls = GUARANTEED_VISIBLE.length + 3; // + More + Expand + Leave
    const minItems = 1 + minControls; // avatar + control circles
    const splitWidth = Math.min(GUARANTEED_VISIBLE.length, 2) * SPLIT_EXTRA;
    const minimalControlExtent = PILL_PAD + AVATAR + BTN * minControls + splitWidth + GAP * (minItems - 1);
    const minCrossAxis = 66;
    if (orientation === 'vertical') {
      return { width: minCrossAxis, height: Math.ceil(minimalControlExtent) };
    }
    return { width: Math.ceil(minimalControlExtent), height: minCrossAxis };
  }

  function handlePillMousedown(e: MouseEvent) {
    // Only the pill background/gaps drag the window; anything interactive
    // (control circles, More menu rows) must keep receiving its click.
    if (e.button !== 0) return;
    const target = e.target as HTMLElement | null;
    if (target?.closest('button')) return;
    pillHost?.onDrag?.(e);
  }

  function setPillInteractive(next: boolean, delay = 0) {
    if (pillCompactTimer) {
      clearTimeout(pillCompactTimer);
      pillCompactTimer = null;
    }
    if (delay > 0) {
      pillCompactTimer = setTimeout(() => {
        pillCompactTimer = null;
        pillInteractive = next;
        pillHost?.onCompactChange?.(next);
      }, delay);
      return;
    }
    if (pillInteractive === next) return;
    pillInteractive = next;
    pillHost?.onCompactChange?.(next);
  }

  function handlePillFocusOut(event: FocusEvent) {
    const next = event.relatedTarget as Node | null;
    if (next && pillAnchorEl?.contains(next)) return;
    setPillInteractive(false, 260);
  }

  onDestroy(() => {
    if (pillCompactTimer) {
      clearTimeout(pillCompactTimer);
      pillCompactTimer = null;
    }
  });

  // ---- Pill control set + stable overflow ------------------------------

  /** The compact pill keeps the essential controls in a fixed order. Device
   * options live on Mic/Camera; specialist actions stay behind More instead
   * of appearing/disappearing as the window crosses a fit threshold. */
  const DISPLAY_ORDER = ['mic', 'camera', 'screenshare'] as const;
  const GUARANTEED_VISIBLE = DISPLAY_ORDER;
  const PILL_MORE_ORDER = ['invite', 'region', 'remotecontrol'] as const;
  type PillIcon = (typeof DISPLAY_ORDER)[number] | (typeof PILL_MORE_ORDER)[number] | 'leave';

  function ariaLabelFor(icon: PillIcon): string {
    switch (icon) {
      case 'mic':
        return micMuted ? 'Unmute' : 'Mute';
      case 'camera':
        return cameraOn ? 'Turn camera off' : 'Turn camera on';
      case 'screenshare':
        return sharingPickerOpen ? 'Close share picker' : 'Open share picker';
      case 'remotecontrol':
        return remoteControlAllowed ? 'Disable remote control' : 'Allow remote control';
      case 'invite':
        return inviteAriaLabel;
      case 'region':
        return 'Create Petal View';
      case 'leave':
        return 'Leave meeting';
    }
  }

  function tooltipFor(icon: PillIcon): string {
    return icon === 'invite' ? inviteTooltip : ariaLabelFor(icon);
  }

  let smallStageEl = $state<HTMLDivElement>();
  let pillAnchorEl = $state<HTMLDivElement>();
  /** 0 = not measured yet (SSR is off, but the first client frame also
   * starts at 0) — treated as "show everything" so there's no initial
   * flash of a More button that immediately disappears. */
  let stageWidth = $state(0);
  let stageHeight = $state(0);
  let baseStageWidth = $state(0);
  let baseStageHeight = $state(0);
  let moreOpen = $state(false);
  let moreMenuEl = $state<HTMLDivElement>();
  /** The More trigger (for focus restore on Escape/backdrop close). */
  let moreTriggerEl = $state<HTMLElement | null>(null);
  type FitState = { visible: PillIcon[]; overflow: PillIcon[] };
  let lockedFit = $state<FitState | null>(null);
  let popupLayoutOpen = $state(false);
  /** JS-computed menu position in stage coordinates (issue #33). The menu
   * stays `visibility: hidden` until the first placement pass has run, so
   * there's no one-frame flash at (0,0). */
  let menuLeft = $state(0);
  let menuTop = $state(0);
  let menuPlaced = $state(false);

  // ---- Per-device menus (mic / camera) ----------------------------------
  // Opened from the carets on the mic/camera controls — the Zoom/Meet
  // convention: device switching lives on the control it affects, so the
  // standalone device button is gone. The mic menu carries input + output;
  // the camera menu carries the camera. Positioned by measurement in
  // VIEWPORT coordinates (chrome == window content, both stages are its
  // children) so one placement pass serves both states; the pill-mode
  // growth path is the same popup-layout mechanism the More menu uses
  // (issue #33), so the panel never clips in the tight pill window.
  let deviceMenu = $state<'mic' | 'camera' | null>(null);
  let deviceTriggerEl = $state<HTMLElement | null>(null);
  let deviceMenuEl = $state<HTMLDivElement>();
  let deviceMenuLeft = $state(0);
  let deviceMenuTop = $state(0);
  let deviceMenuPlaced = $state(false);
  let deviceMenuMaxHeight = $state<string | undefined>();
  /** Bumped whenever the menu panel's content size changes (e.g. the
   * "Loading devices…" placeholder grows into the selects once the real
   * device lists arrive). The placement effect reads it as a reactive
   * dep so a panel that grows AFTER its first placement gets re-placed —
   * otherwise the bottom of the loaded menu was cut off where the small
   * loading-state panel was clamped. */
  let deviceMenuSizeRevision = $state(0);
  const DEVICE_GAP = 8; // same 8px gap the More menu uses to its anchor
  const DEVICE_INSET = 8; // minimum breathing room from the viewport edge

  $effect(() => {
    const menu = deviceMenuEl;
    if (!menu) return;
    const ro = new ResizeObserver(() => {
      deviceMenuSizeRevision += 1;
    });
    ro.observe(menu);
    return () => ro.disconnect();
  });

  function openDeviceMenu(kind: 'mic' | 'camera', trigger: HTMLElement | null) {
    moreOpen = false;
    if (deviceMenu === kind) {
      closeDeviceMenu(false);
      return;
    }
    deviceTriggerEl = trigger;
    deviceMenu = kind;
  }

  function closeDeviceMenu(restoreFocus = true) {
    const trigger = deviceTriggerEl;
    deviceMenu = null;
    deviceTriggerEl = null;
    deviceMenuPlaced = false;
    deviceMenuMaxHeight = undefined;
    if (restoreFocus) {
      requestAnimationFrame(() => trigger?.focus());
    }
  }

  // Keep expansion cleanup read-free. Calling closeDeviceMenu() here would
  // subscribe this effect to deviceTriggerEl through its focus-restoration
  // read, causing every newly opened selector to close again.
  function clearDeviceMenuForExpansion() {
    deviceMenu = null;
    deviceTriggerEl = null;
    deviceMenuPlaced = false;
    deviceMenuMaxHeight = undefined;
  }

  $effect(() => {
    if (moreOpen) {
      return installDismissibleLayer({
        isOpen: () => moreOpen,
        getInsideNodes: () => [moreMenuEl, moreTriggerEl],
        getPopupNodes: () => [moreMenuEl],
        getOpener: () => moreTriggerEl,
        onDismiss: () => {
          moreOpen = false;
        }
      });
    }
  });

  $effect(() => {
    if (deviceMenu !== null) {
      return installDismissibleLayer({
        isOpen: () => deviceMenu !== null,
        getInsideNodes: () => [deviceMenuEl, deviceTriggerEl],
        getPopupNodes: () => [deviceMenuEl],
        getOpener: () => deviceTriggerEl,
        onDismiss: () => closeDeviceMenu(false)
      });
    }
  });

  $effect(() => {
    if (deviceMenu === null) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') closeDeviceMenu();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  $effect(() => {
    const stage = smallStageEl;
    if (!stage) return;
    // Measure the small stage (== chrome width; the pill is centered inside
    // it). The measurement does not depend on how many buttons are visible,
    // so the fit derivation below cannot oscillate/flicker at the boundary.
    const ro = new ResizeObserver(() => {
      stageWidth = stage.clientWidth;
      stageHeight = stage.clientHeight;
    });
    ro.observe(stage);
    return () => ro.disconnect();
  });

  $effect(() => {
    if (popupHostOpen) {
      popupLayoutOpen = true;
      return;
    }
    if (popupLayoutOpen) {
      if (!baseStageWidth || !baseStageHeight) {
        popupLayoutOpen = false;
        return;
      }
      if (stageWidth <= baseStageWidth + 2 && stageHeight <= baseStageHeight + 2) {
        popupLayoutOpen = false;
      }
      return;
    }
    baseStageWidth = stageWidth;
    baseStageHeight = stageHeight;
  });

  // Fixed layout constants mirroring Pill.svelte's large pill metrics:
  // symmetric 15px horizontal padding, 11.25px flex gap, ControlButton
  // size="pill" (40px), and Avatar/self-view (33px).
  const BTN = 40;
  const GAP = 11.25;
  const PILL_PAD = 30;
  /** Identity-circle diameter — Avatar AND the #9 webcam self-view circle
   * are deliberately the SAME 33px (recorded judgment call, issue #9):
   * the pill window is sized to the pill's measured bounds at collapse/flip
   * time, so a camera toggle while collapsed must not change the pill's
   * geometry. Because both render at 26px, this one constant keeps BOTH
   * orientations' fit budgets (pillWidthFor/pillHeightFor) exact in every
   * camera state. */
  const AVATAR = 33;
  /** The attached options segment adds width only in the horizontal pill. */
  const SPLIT_EXTRA = 22;
  /** Total pill width for `buttons` feature circles (+ optional More). The
   * avatar and the two non-collapsible trailing circles — the view switcher
   * and, after issue #668, Leave — are always in the budget. */
  function pillWidthFor(buttons: number, withMore: boolean): number {
    const circles = buttons + (withMore ? 1 : 0) + 2; // + view switcher + leave
    const items = 1 + circles; // avatar + circles
    const splitWidth = Math.min(buttons, 2) * SPLIT_EXTRA;
    return PILL_PAD + AVATAR + circles * BTN + splitWidth + GAP * (items - 1);
  }

  /** Vertical twin of pillWidthFor (issue #12): same budget rotated to
   * height — no room name in the vertical pill (hidden, judgment call). */
  function pillHeightFor(buttons: number, withMore: boolean): number {
    const circles = buttons + (withMore ? 1 : 0) + 2; // + view switcher + leave
    const items = 1 + circles; // avatar + circles
    return PILL_PAD + AVATAR + circles * BTN + GAP * (items - 1);
  }

  const fit = $derived<FitState>({
    visible: [...DISPLAY_ORDER],
    overflow: [...PILL_MORE_ORDER]
  });

  const effectiveFit = $derived(moreOpen && lockedFit ? lockedFit : fit);

  function toggleMore() {
    if (moreOpen) {
      moreOpen = false;
      return;
    }
    lockedFit = fit;
    moreOpen = true;
  }

  function closeMore() {
    moreOpen = false;
    moreTriggerEl?.focus();
  }

  // Keyboard operability for the More menu (it uses role=menu/menuitem):
  // Escape dismisses and returns focus to the trigger; ArrowUp/Down cycle
  // the items; opening moves focus to the first item so the menu is
  // immediately operable. The items themselves are real buttons, so plain
  // Tab already reaches every one of them.
  $effect(() => {
    if (!moreOpen) return;
    const menu = moreMenuEl;
    (menu?.querySelector<HTMLElement>('.more-item'))?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeMore();
        return;
      }
      if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
      const items = Array.from(menu?.querySelectorAll<HTMLElement>('.more-item') ?? []);
      if (items.length === 0) return;
      event.preventDefault();
      const current = items.indexOf(document.activeElement as HTMLElement);
      const delta = event.key === 'ArrowDown' ? 1 : -1;
      const next = (current + delta + items.length) % items.length;
      items[next].focus();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  // The More menu can't outlive its own reason to exist: close it whenever
  // the chrome expands back to the gallery or the overflow empties (resize).
  // The device menus close on expand too (their gallery-state triggers are
  // different cells; a menu left over from pill mode would dangle).
  $effect(() => {
    if (expanded || effectiveFit.overflow.length === 0) moreOpen = false;
    if (expanded) clearDeviceMenuForExpansion();
  });

  $effect(() => {
    if (!moreOpen) lockedFit = null;
    // The pill window grows for popups (popup-layout): the device menus need
    // the same room as the More menu or they would clip in the tight pill
    // window.
    pillHost?.onPopupChange?.(!expanded && (moreOpen || deviceMenu !== null));
  });

  // ---- More-menu placement within the window bounds (issue #33) ---------
  // The pill window is sized tight to the pill (deliberately — an invisible
  // allowance area was rejected because it blocks desktop clicks), so a menu
  // CSS-anchored above the pill ALWAYS clipped past the webview's top edge
  // whenever More existed (More only appears once the user has shrunk the
  // window below natural bounds). Fix: the menu is a stage child (the stage
  // == the window's full client area) placed by measurement — prefer the
  // original anchor side (above for horizontal, beside-left for vertical),
  // flip to the opposite side if that side has room, and otherwise clamp
  // inside the stage (overlaying the pill), with max-width/height caps +
  // internal scroll (CSS below) so it can never exceed the window.
  const MENU_GAP = 8; // gap between pill and menu (same 8px as the old CSS)
  const MENU_INSET = 6; // minimum breathing room from the stage edges
  $effect(() => {
    if (!moreOpen) {
      menuPlaced = false;
      return;
    }
    // Reactive re-placement triggers: window resize (stageWidth/Height via
    // the ResizeObserver above), orientation flips, and overflow-set changes
    // (row count changes the menu's measured height).
    void stageWidth;
    void stageHeight;
    void orientation;
    void effectiveFit.overflow.length;
    const stage = smallStageEl;
    const anchor = pillAnchorEl;
    const menu = moreMenuEl;
    if (!stage || !anchor || !menu) return;
    const sw = stage.clientWidth;
    const sh = stage.clientHeight;
    // anchor's offsetParent is the stage (nearest positioned ancestor), so
    // offsetLeft/Top are already stage coordinates; offset* metrics are
    // also immune to the stage's scale transform (unlike bounding rects,
    // same reasoning as measurePill above).
    const ax = anchor.offsetLeft;
    const ay = anchor.offsetTop;
    const aw = anchor.offsetWidth;
    const ah = anchor.offsetHeight;
    // Measured AFTER the max-width/height caps apply, so a menu taller than
    // the stage measures at its capped (scrolling) size and clamps cleanly.
    const mw = menu.offsetWidth;
    const mh = menu.offsetHeight;
    let left: number;
    let top: number;
    if (orientation === 'vertical') {
      // Preferred: beside the pill on the left, bottom-aligned (the
      // pre-#33 CSS anchoring). Flip to the right side if the left clips.
      left = ax - MENU_GAP - mw;
      if (left < MENU_INSET && ax + aw + MENU_GAP + mw <= sw - MENU_INSET) {
        left = ax + aw + MENU_GAP;
      }
      top = ay + ah - mh;
    } else {
      // Preferred: above the pill, right-aligned (the pre-#33 CSS
      // anchoring). Flip below if above clips and below has room.
      top = ay - MENU_GAP - mh;
      if (top < MENU_INSET && ay + ah + MENU_GAP + mh <= sh - MENU_INSET) {
        top = ay + ah + MENU_GAP;
      }
      left = ax + aw - mw;
    }
    // Final clamp: whatever side won, never past a stage edge.
    menuLeft = Math.round(Math.min(Math.max(left, MENU_INSET), Math.max(MENU_INSET, sw - mw - MENU_INSET)));
    menuTop = Math.round(Math.min(Math.max(top, MENU_INSET), Math.max(MENU_INSET, sh - mh - MENU_INSET)));
    menuPlaced = true;
  });

  // Device-picker placement (viewport coords): measure the actual trigger,
  // reserve the space above/below it, then constrain the picker BEFORE using
  // its final height. This keeps a loaded picker above the action bar instead
  // of letting its lower half hide behind `.controlbar` (#meeting-ui).
  $effect(() => {
    if (deviceMenu === null) {
      deviceMenuPlaced = false;
      deviceMenuMaxHeight = undefined;
      return;
    }
    void stageWidth;
    void stageHeight;
    void orientation;
    void deviceMenuSizeRevision;
    const trigger = deviceTriggerEl;
    const menu = deviceMenuEl;
    if (!trigger || !menu) return;

    const tr = trigger.getBoundingClientRect();
    const actionbar = trigger.closest<HTMLElement>('.controlbar');
    const actionbarRect = actionbar?.getBoundingClientRect();
    // Expanded controls live inside the action bar. Treat that bar as the
    // popup anchor so the panel never paints over its top border/buttons;
    // compact-pill controls have no action bar and retain the trigger edge.
    const anchorTop = actionbarRect?.top ?? tr.top;
    const anchorBottom = actionbarRect?.bottom ?? tr.bottom;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const spaceAbove = Math.max(0, anchorTop - DEVICE_GAP - DEVICE_INSET);
    const spaceBelow = Math.max(0, vh - DEVICE_INSET - anchorBottom - DEVICE_GAP);
    // `scrollHeight` is the unconstrained content height even when a previous
    // pass already applied max-height, so async device enumeration can never
    // make us mistake the loading-state height for the real panel height.
    const naturalHeight = menu.scrollHeight;
    const fitsAbove = naturalHeight <= spaceAbove;
    const placeAbove = fitsAbove || spaceAbove >= spaceBelow;
    const availableHeight = Math.max(1, Math.floor(placeAbove ? spaceAbove : spaceBelow));
    const maxHeight = `${availableHeight}px`;
    deviceMenuMaxHeight = maxHeight;
    // Apply synchronously as well as through the style directive below. The
    // following offset measurement must include the new constraint before the
    // visibility flag is published, avoiding a one-frame overflow.
    menu.style.setProperty('--device-menu-max-height', maxHeight);

    const mw = menu.offsetWidth;
    const mh = Math.min(menu.offsetHeight, availableHeight);
    const left = tr.right - mw;
    const top = placeAbove
      ? anchorTop - DEVICE_GAP - mh
      : anchorBottom + DEVICE_GAP;
    deviceMenuLeft = Math.round(
      Math.min(Math.max(left, DEVICE_INSET), Math.max(DEVICE_INSET, vw - mw - DEVICE_INSET))
    );
    deviceMenuTop = Math.round(
      Math.min(Math.max(top, DEVICE_INSET), Math.max(DEVICE_INSET, vh - mh - DEVICE_INSET))
    );
    deviceMenuPlaced = true;
  });
</script>

{#snippet viewSwitcher()}
  <!-- Large-state view switcher: collapse glyph in a compact circular chip,
       slotted in-flow at the gallery topbar's right end. Calls
       toggleExpanded() directly — not part of the onControl channel. -->
  <ControlButton
    icon="collapse"
    kind="oneshot"
    size="compact"
    label="Collapse to compact bar"
    ariaExpanded={expanded}
    onclick={toggleExpanded}
  />
{/snippet}

{#snippet menuGlyph(icon: PillIcon)}
  <!-- Small inline copies of ControlButton's glyphs for the overflow menu
       rows (a row can't nest a real ControlButton — interactive elements
       don't nest). Base variants only; state is carried by the row's
       aria-label, same register as the pill. -->
  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
    {#if icon === 'mic'}
      <rect x="9" y="3" width="6" height="11" rx="3"></rect>
      <path d="M5 11a7 7 0 0 0 14 0M12 18v3"></path>
    {:else if icon === 'camera'}
      <path d="M2 7a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z"></path>
      <path d="M16 10l5-3v10l-5-3"></path>
    {:else if icon === 'screenshare'}
      <rect x="3" y="4" width="18" height="13" rx="2"></rect>
      <path d="M8 21h8M12 17v4"></path>
    {:else if icon === 'region'}
      <path d="M4 8V5a1 1 0 0 1 1-1h3M16 4h3a1 1 0 0 1 1 1v3M20 16v3a1 1 0 0 1-1 1h-3M8 20H5a1 1 0 0 1-1-1v-3"></path>
      <rect x="7" y="7" width="10" height="10" rx="1"></rect>
    {:else if icon === 'remotecontrol'}
      <path d="M12 3v4"></path>
      <path d="M12 17v4"></path>
      <path d="M3 12h4"></path>
      <path d="M17 12h4"></path>
      <circle cx="12" cy="12" r="3"></circle>
    {:else if icon === 'invite'}
      <circle cx="9" cy="8" r="3.5"></circle>
      <path d="M3 20a6 6 0 0 1 12 0"></path>
      <path d="M16 5.5a3.5 3.5 0 0 1 0 7"></path>
      <path d="M19 20a6 6 0 0 0-4-5.6"></path>
    {/if}
  </svg>
{/snippet}

<div class="chrome" class:small={!expanded}>
  <!-- Large state: the existing Gallery, with the view switcher slotted into
       its topbar. `inert` on collapse also disables the switcher inside, so
       only the pill's expand circle is interactive in small state. -->
  <div class="stage large-stage" aria-hidden={!expanded} inert={!expanded}>
    <Gallery
      {roomName}
      {elapsed}
      {participants}
      {cameraDrawUpdates}
      {micMuted}
      {cameraOn}
      {sharingActive}
      {sharingLiveBackground}
      {sharingLiveColor}
      {remoteControlAllowed}
      {stateTitle}
      {stateDetail}
      {stateTone}
      {onControl}
      {inviteAriaLabel}
      {inviteTooltip}
      {onInviteLinkCopy}
      {onOpenNetwork}
      {onRenameRoom}
      {onReportBug}
      {frameless}
      onOpenDeviceMenu={(kind, el) => openDeviceMenu(kind, el)}
      deviceMenuKind={deviceMenu}
      topbarAction={viewSwitcher}
    />
  </div>

  <!-- Small / thumbnail state (DESIGN.md §2): "collapsed to a minimal
       floating bar: a tight avatar stack (or the active speaker only) + the
       essential controls (mic, cam, leave). Draggable, always-on-top, gets
       out of the way." Dragging itself is native-window-level (Tauri side,
       same class of gap RemoteWindowHeader's own doc comment flags for its
       drag handle) — not implemented in this frontend-only component. -->
  <div
    class="stage small-stage"
    class:vertical={orientation === 'vertical'}
    class:popup-layout={popupLayoutOpen}
    aria-hidden={expanded}
    inert={expanded}
    bind:this={smallStageEl}
  >
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
      class="pill-anchor"
      class:expanded-pill={pillInteractive}
      bind:this={pillAnchorEl}
      onpointerenter={() => setPillInteractive(true)}
      onpointerleave={() => setPillInteractive(false, 260)}
      onfocusin={() => setPillInteractive(true)}
      onfocusout={handlePillFocusOut}
      onmousedown={handlePillMousedown}
    >
      <Pill {orientation} scale="large">
        {#if selfVideoOn}
          <!-- issue #9: live circular webcam self-view as the docked
               identity circle. Mirrored (self-view convention, #7); same
               26px + ring/speaking treatment as the Avatar it replaces, so
               camera toggles never change the pill's measured geometry.
               No captions: a live self-view has no dialogue to caption. -->
          <div
            class="self-video"
            class:speaking={participants[0]?.speaking}
            style:--ring-color={activeColor ?? `var(--id-${activeIdentity})`}
          >
            <!-- svelte-ignore a11y_media_has_caption -->
            <video bind:this={selfVideoEl} autoplay muted playsinline></video>
          </div>
        {:else}
          <Avatar
            name={participants[0]?.name ?? 'You'}
            identity={activeIdentity}
            resolvedColor={activeColor}
            size={33}
            speaking={participants[0]?.speaking}
          />
        {/if}
        {#each effectiveFit.visible as icon (icon)}
          <span class="pill-control-slot" class:idle-optional={icon === 'remotecontrol' || icon === 'invite'}>
            {#if icon === 'mic' || icon === 'camera'}
              <MediaSplitControl
                icon={icon}
                active={icon === 'mic' ? micMuted : !cameraOn}
                actionLabel={ariaLabelFor(icon)}
                optionsLabel={icon === 'mic' ? 'Microphone options' : 'Camera options'}
                optionsOpen={deviceMenu === icon}
                size="pill"
                onToggle={() => onControl?.(icon)}
                onOptions={(el) => openDeviceMenu(icon, el)}
              />
            {:else}
              <ControlButton
                {icon}
                kind={icon === 'invite' ? 'oneshot' : 'toggle'}
                size="pill"
                active={icon === 'screenshare'
                  ? screenshareControlActive
                  : icon === 'remotecontrol'
                    ? remoteControlAllowed
                    : false}
                label={ariaLabelFor(icon)}
                liveBackground={icon === 'screenshare' ? sharingLiveBackground : undefined}
                liveColor={icon === 'screenshare' ? sharingLiveColor : undefined}
                onclick={() => onControl?.(icon)}
              />
            {/if}
          </span>
        {/each}
        {#if effectiveFit.overflow.length > 0}
          <span class="pill-control-slot">
            <ControlButton
              icon="more"
              kind="oneshot"
              size="pill"
              label="More controls"
              ariaExpanded={moreOpen}
              ariaHaspopup="menu"
              onclick={(event) => {
                moreTriggerEl = event.currentTarget as HTMLElement;
                toggleMore();
              }}
            />
          </span>
        {/if}
        <!-- The view switcher as a real circle INSIDE the pill row (issue #55)
             — calls toggleExpanded() directly, never onControl. Not
             collapsible into More; second-to-last circle, immediately
             before the trailing Leave circle (issue #668). -->
        <span class="pill-switcher">
          <ControlButton
            icon="expand"
            kind="oneshot"
            size="pill"
            label="Expand to full view"
            ariaExpanded={expanded}
            onclick={toggleExpanded}
          />
        </span>
        <!-- Leave (issue #668): rendered in its own trailing block, after
             the view switcher, exactly mirroring how the switcher itself is
             rendered outside the {#each} loop above — so it is the
             genuinely last circle in every fit state and both orientations,
             never collapsible into More. Same markup/props it had as a
             DISPLAY_ORDER loop member (size/kind/label unchanged); the
             neutral, non-destructive "subtle" look comes from ControlButton
             itself (icon="leave" + kind="oneshot", issue #192) so it needs
             no styling here. -->
        <span class="pill-control-slot">
          <ControlButton
            icon="leave"
            kind="oneshot"
            size="pill"
            label={ariaLabelFor('leave')}
            onclick={() => onControl?.('leave')}
          />
        </span>
      </Pill>
    </div>
    {#if moreOpen}
      <!-- Tiny vertical overflow menu (issue #6) — same graphite register
           as the Pill shell (gradient surface, hairline, pill shadow), no
           new visual language. Feature-name labels; state-descriptive
           aria-labels.
           Issue #33: a STAGE child (not a pill-anchor child) positioned by
           the measurement effect above, so it always lands inside the
           window bounds instead of CSS-anchoring past the webview edge. -->
      <div
        class="more-menu"
        class:placed={menuPlaced}
        role="menu"
        aria-label="More controls"
        in:restrainedSurfaceEnterTransition
        out:restrainedSurfaceExitTransition
        bind:this={moreMenuEl}
        style:left="{menuLeft}px"
        style:top="{menuTop}px"
      >
        {#each effectiveFit.overflow as icon (icon)}
          <button
            type="button"
            role="menuitem"
            class="more-item"
            aria-label={ariaLabelFor(icon)}
            onclick={() => {
              moreOpen = false;
              onControl?.(icon);
            }}
          >
            <span class="more-item-icon" aria-hidden="true">{@render menuGlyph(icon)}</span>
            <span class:invite-control-tooltip={icon === 'invite'}>{tooltipFor(icon)}</span>
          </button>
        {/each}
      </div>
    {/if}
  </div>

  {#if deviceMenu !== null}
    <!-- Per-device menu (mic: input + output; camera: the camera). -->
    <div
      class="devices-menu"
      class:placed={deviceMenuPlaced}
      in:restrainedSurfaceEnterTransition
      out:restrainedSurfaceExitTransition
      style:left="{deviceMenuLeft}px"
      style:top="{deviceMenuTop}px"
      style:--device-menu-max-height={deviceMenuMaxHeight}
      bind:this={deviceMenuEl}
    >
      <DevicePicker
        mode={deviceMenu === 'mic' ? 'audio' : 'camera'}
        onClose={() => closeDeviceMenu()}
      />
    </div>
  {/if}
</div>

<style>
  .chrome {
    position: relative;
    width: 100%;
    height: 100%;
  }

  /* The stages stay mounted so large↔small is a simple cross-fade instead
     of a mount/unmount swap. This deliberately avoids scaling the rounded
     pill layer: scaled rings/shadows were the source of the "fluid halo"
     seen during compact-mode transitions (issue #194). */
  .stage {
    transition: opacity var(--motion-enter) var(--ease-standard);
  }

  .large-stage {
    height: 100%;
    opacity: 1;
  }

  .large-stage :global(.topbar) {
    min-height: 58px;
    height: 58px;
    padding-top: 12px;
    padding-bottom: 10px;
  }

  .small-stage {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    opacity: 0;
    pointer-events: none;
  }

  .chrome.small .large-stage {
    opacity: 0;
    pointer-events: none;
  }

  .chrome.small .small-stage {
    /* Stays absolutely centered over the (hidden) large stage — `position:
       static` here used to push the bar below the full-height large stage
       once .large-stage gained height:100% for the real meeting route. */
    opacity: 1;
    pointer-events: auto;
  }

  .small-stage.popup-layout {
    align-items: flex-start;
    justify-content: flex-start;
  }

  .small-stage.popup-layout .pill-anchor {
    margin: 18px;
  }

  @media (prefers-reduced-motion: reduce) {
    .stage {
      transition: none;
    }
  }

  /* issue #9: the pill's live self-view circle. Mirrors Avatar's
     structure — outer box carries the identity ring (::before) + quiet
     speaking ring (::after, same values as Avatar's) so neither is clipped;
     the <video> itself is the clipped circle. Same 33px as the Avatar it
     replaces (see the AVATAR fit-budget constant above). */
  .self-video {
    position: relative;
    width: 33px;
    height: 33px;
    flex-shrink: 0;
  }

  .self-video::before {
    content: '';
    position: absolute;
    inset: -2.5px;
    border-radius: var(--radius-pill);
    border: 1.875px solid var(--ring-color);
    pointer-events: none;
  }

  .self-video.speaking::after {
    content: '';
    position: absolute;
    inset: -2.5px;
    border-radius: var(--radius-pill);
    border: 1.875px solid rgba(255, 255, 255, 0.55);
    box-shadow: 0 0 17.5px -7.5px rgba(255, 255, 255, 0.22);
    pointer-events: none;
  }

  .self-video video {
    width: 100%;
    height: 100%;
    border-radius: var(--radius-pill);
    object-fit: cover;
    display: block;
    background: var(--surface-2);
    outline: 1px solid var(--hairline-strong);
    outline-offset: -1px;
    /* Self-view convention (#7): mirrored, like a mirror. */
    transform: scaleX(-1);
  }

  .pill-anchor {
    position: relative;
    z-index: 2;
  }

  .pill-control-slot {
    display: inline-flex;
    flex-shrink: 0;
  }

  .pill-anchor:not(.expanded-pill) .idle-optional {
    display: none;
  }

  .pill-switcher {
    display: inline-flex;
    opacity: 0.78;
    transform: scale(1);
    pointer-events: auto;
    transition:
      opacity var(--motion-feedback) var(--ease-standard),
      transform var(--motion-feedback) var(--ease-standard);
  }

  .pill-anchor:hover .pill-switcher,
  .pill-anchor:has(:focus-visible) .pill-switcher,
  .pill-anchor.expanded-pill .pill-switcher {
    opacity: 1;
    transform: scale(1);
    pointer-events: auto;
  }

  /* Subject-attached :has() is pruned by Svelte's CSS analyzer as "unused"
     (false positive — the switcher is a descendant of .pill-anchor, so the
     paired rule above already covers keyboard focus inside it; this rule
     stays as a defensive standalone). :global() shields the :has() from
     pruning while keeping .pill-switcher scoped. */
  .pill-switcher:global(:has(:focus-visible)) {
    opacity: 1;
    pointer-events: auto;
  }

  .pill-switcher :global(.control-button) {
    background: var(--fill-weak);
    color: var(--text-dim);
    box-shadow: none;
  }

  .pill-switcher :global(.control-button:hover),
  .pill-switcher :global(.control-button:focus-visible) {
    background: var(--fill-strong);
    color: var(--text-strong);
  }

  /* Issue #33: positioned in stage coordinates by the placement effect
     (inline left/top); hidden until the first placement pass so it never
     flashes at (0,0). The max caps + internal scroll are the hard floor —
     even when the window is shrunk so tight that neither above/below nor
     beside fits, the menu clamps inside the stage and scrolls instead of
     clipping past the webview edge. min-width yields to the cap (a bare
     150px min-width would win over max-width per CSS and reintroduce the
     horizontal clip in the narrow vertical-pill window). */
  .more-menu {
    position: absolute;
    left: 0;
    top: 0;
    z-index: 3;
    visibility: hidden;
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: min(150px, calc(100% - 12px));
    max-width: calc(100% - 12px);
    max-height: calc(100% - 12px);
    overflow-y: auto;
    overscroll-behavior: none;
    box-sizing: border-box;
    padding: 6px;
    border-radius: var(--radius-popover);
    background: linear-gradient(180deg, var(--surface), var(--bg-base));
    border: 1px solid var(--hairline-strong);
    box-shadow:
      var(--shadow-pill),
      0 0 0 1px var(--fill-weak);
  }

  .more-menu.placed {
    visibility: visible;
  }

  /* Device-picker overlay: chrome-level (above both stages), positioned by
     the placement effect in viewport coordinates; visibility-hidden until
     the first pass so it never flashes at (0,0). Outside dismissal is handled
     by the shared capture-phase dismissible layer. */
  .devices-menu {
    position: fixed;
    left: 0;
    top: 0;
    z-index: 21;
    visibility: hidden;
  }

  .devices-menu.placed {
    visibility: visible;
  }

  .more-item {
    display: flex;
    align-items: center;
    gap: 9px;
    min-height: 40px;
    padding: 7px 10px;
    border: none;
    border-radius: var(--radius-chip);
    background: transparent;
    color: var(--text-strong);
    font: 500 12px var(--font-ui);
    text-align: left;
    white-space: nowrap;
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      scale var(--motion-fast) var(--ease-standard);
  }

  .more-item:hover {
    background: var(--fill-strong);
  }

  .more-item:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: -2px;
  }

  .more-item:active {
    scale: var(--press-scale, 0.96);
  }

  .more-item-icon {
    display: inline-flex;
    width: 14px;
    height: 14px;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
  }

  /* The compact More menu receives the same full public-code disclosure as
     the other invite controls. Keep it inside the stage at narrow widths. */
  .more-item .invite-control-tooltip {
    min-width: 0;
    white-space: normal;
    overflow-wrap: anywhere;
    text-wrap: pretty;
  }

  @media (prefers-reduced-motion: reduce) {
    .more-item {
      transition: none;
    }

    .more-item:active {
      scale: 1;
    }
  }
</style>
