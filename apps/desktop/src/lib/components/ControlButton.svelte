<!--
  ControlButton — the one circular control used everywhere (mic, camera,
  screenshare, invite, leave). Per Petal-Build-Map.md §2.1 / DESIGN.md §6:
  one 44px circle, reused at 44 / compact / menubar sizes, with icon glyphs
  pulled verbatim from the approved design canvas.

  States: default, hover, on/active, disabled — only mic/webcam/screensharing
  (kind="toggle") get an on/active look; invite/leave (kind="oneshot") never
  show one, matching the approved button matrix's "—" cells (don't fabricate
  an active state for them).

  Danger tone is explicit-only. Leave intentionally stays neutral/subtle
  (issue #192); muted-mic tile treatment is owned by ParticipantTile.
-->
<script lang="ts" module>
  export type ControlIcon =
    | 'mic'
    | 'camera'
    | 'screenshare'
    | 'region'
    | 'remotecontrol'
    | 'invite'
    | 'leave'
    | 'more'
    | 'expand'
    | 'collapse';
  export type ControlKind = 'toggle' | 'oneshot';
  export type ControlTone = 'neutral' | 'danger';
  export type ControlSize = 44 | 'compact' | 'pill' | 'menubar';
</script>

<script lang="ts">
  interface Props {
    icon: ControlIcon;
    kind?: ControlKind;
    /** Only meaningful for kind="toggle". Ignored (never rendered) for "oneshot". */
    active?: boolean;
    tone?: ControlTone;
    size?: ControlSize;
    disabled?: boolean;
    label?: string;
    /** Optional live-state colors for the screenshare control. Callers that
     * know the current sharing identity pass a solid background plus a
     * contrasting icon/text color; otherwise the legacy live tokens apply. */
    liveBackground?: string;
    liveColor?: string;
    /** Optional aria-expanded pass-through for controls that open/close
     * something (the MeetingChrome view switcher, an overflow More menu).
     * Left undefined for plain controls so the attribute isn't rendered. */
    ariaExpanded?: boolean;
    /** Optional aria-haspopup for controls that open a menu or dialog
     * popover (the More menu, the device picker). */
    ariaHaspopup?: 'menu' | 'dialog';
    /** Optional tabindex for status-only glyphs that must not be tab stops
     * (muted chips render a ControlButton shape but are not controls). */
    tabindex?: number;
    /** Receives the click event so callers that open a positioned popover
     * can read `currentTarget` as the anchor. `() => void` callers keep
     * working (fewer params is assignable). */
    onclick?: (event: MouseEvent) => void;
  }

  let {
    icon,
    kind = 'toggle',
    active = false,
    tone = 'neutral',
    size = 44,
    disabled = false,
    label,
    liveBackground,
    liveColor,
    ariaExpanded = undefined,
    ariaHaspopup = undefined,
    tabindex,
    onclick
  }: Props = $props();

  // Invite/leave are one-shot actions — comps show "—" where on/active
  // doesn't apply, so never let a oneshot control render as active.
  const isActive = $derived(kind === 'toggle' && active);

  const dimension = $derived(
    size === 44 ? 44 : size === 'compact' ? 32 : size === 'pill' ? 40 : 24
  );
  const iconSize = $derived(
    size === 44 ? 20 : size === 'compact' ? 16 : size === 'pill' ? 20 : 12
  );

  const defaultLabels: Record<ControlIcon, string> = {
    mic: 'Microphone',
    camera: 'Camera',
    screenshare: 'Screensharing',
    region: 'Petal View',
    remotecontrol: 'Remote control',
    invite: 'Invite',
    leave: 'Leave',
    more: 'More',
    expand: 'Expand',
    collapse: 'Collapse'
  };

  const computedLabel = $derived(label ?? defaultLabels[icon]);
  const isSubtleLeave = $derived(icon === 'leave' && kind === 'oneshot' && tone !== 'danger');

  // Screenshare start/stop green pulse (DESIGN.md §6: "the source window's
  // tab activates and a brief green pulse confirms it's live"). Bumped by a
  // $effect below whenever `active` flips true for a screenshare control —
  // a `{#key}`-remounted span replays the CSS animation, same replay
  // mechanism Pointer.svelte's click-ripple already established (`pulseKey`),
  // reused here rather than inventing a second animation-replay technique.
  let pulseKey = $state(0);
  // `undefined` sentinel (not seeded from `active`'s initial value) so
  // there's no "reads a prop only once at declaration" pattern to warn
  // about — the very first effect run always treats the transition as
  // "no prior state yet" and skips the pulse, which is correct anyway
  // (a control shouldn't pulse just because it mounted already active).
  let previousActive: boolean | undefined = undefined;
  $effect(() => {
    if (icon === 'screenshare' && active && previousActive === false) {
      pulseKey += 1;
    }
    previousActive = active;
  });
</script>

<button
  type="button"
  class="control-button"
  class:live={isActive && icon === 'screenshare' && tone !== 'danger'}
  class:danger={tone === 'danger'}
  class:subtle={isSubtleLeave}
  class:size-44={dimension === 44}
  class:size-compact={dimension === 32}
  class:size-pill={dimension === 40}
  class:size-menubar={dimension === 24}
  style:width="{dimension}px"
  style:height="{dimension}px"
  style:--control-live-bg={liveBackground}
  style:--control-live-fg={liveColor}
  {disabled}
  aria-label={computedLabel}
  aria-pressed={kind === 'toggle' ? isActive : undefined}
  aria-expanded={ariaExpanded}
  aria-haspopup={ariaHaspopup}
  {tabindex}
  {onclick}
>
  <span class="icon" style:width="{iconSize}px" style:height="{iconSize}px">
    {#if icon === 'mic'}
      <!-- Slash draws on/off (DESIGN.md §6) rather than a hard swap: both
           glyph variants stay mounted (no {#if}/{#else} remount) so the base
           mic shape never flickers, and the slash `path` alone animates its
           `stroke-dasharray`/`stroke-dashoffset` from 0 (hidden) to fully
           drawn, keyed off `isActive`. The muted state stays neutral-toned
           like the camera-off glyph; this swap is purely the glyph, not the
           color. -->
      <svg width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
        <rect x="9" y="3" width="6" height="11" rx="3"></rect>
        <path d="M5 11a7 7 0 0 0 14 0M12 18v3"></path>
        <path class="mic-slash" class:drawn={isActive} d="M3 3l18 18" pathLength="1"></path>
      </svg>
    {:else if icon === 'camera'}
      <!-- Crossfade (DESIGN.md §6: "glyph cross-fade + tile video/avatar
           swap") — both glyph layers stay mounted and cross-dissolve via
           opacity rather than an instant {#if} swap. camera.slash stays
           neutral-toned; active drives the glyph, not the color. -->
      <span class="camera-crossfade">
        <svg class="camera-layer" class:hidden={isActive} width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M2 7a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z"></path>
          <path d="M16 10l5-3v10l-5-3"></path>
        </svg>
        <svg class="camera-layer" class:hidden={!isActive} width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M2 7a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z"></path>
          <path d="M16 10l5-3v10l-5-3"></path>
          <path d="M2 2l20 20"></path>
        </svg>
      </span>
    {:else if icon === 'screenshare'}
      <svg width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <rect x="3" y="4" width="18" height="13" rx="2"></rect>
        <path d="M8 21h8M12 17v4"></path>
      </svg>
      {#key pulseKey}
        {#if pulseKey > 0}
          <!-- Green pulse confirming screenshare just went live (DESIGN.md
               §6). Reduced-motion: opacity-only via --motion-base already
               collapsing to 0ms globally (tokens.css), so this becomes an
               instant no-op fade rather than a moving ring. -->
          <span class="share-pulse" aria-hidden="true"></span>
        {/if}
      {/key}
    {:else if icon === 'region'}
      <svg width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M4 8V5a1 1 0 0 1 1-1h3M16 4h3a1 1 0 0 1 1 1v3M20 16v3a1 1 0 0 1-1 1h-3M8 20H5a1 1 0 0 1-1-1v-3"></path>
        <rect x="7" y="7" width="10" height="10" rx="1"></rect>
      </svg>
    {:else if icon === 'remotecontrol'}
      <!-- Remote control stays neutral like camera; active means enabled, so
           the off/disabled state is shown by a neutral slash instead of green.
           Use the canonical telepointer outline; only this control reverses
           the off slash so it does not merge with the pointer diagonal. -->
      <span class="icon-crossfade">
        <svg class="icon-layer" class:hidden={!isActive} width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M5 3l5 16 2.5-6.5L19 10z"></path>
        </svg>
        <svg class="icon-layer" class:hidden={isActive} width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M5 3l5 16 2.5-6.5L19 10z"></path>
          <path d="M3 21L21 3"></path>
        </svg>
      </span>
    {:else if icon === 'invite'}
      <svg width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="9" cy="8" r="3.5"></circle>
        <path d="M3 20a6 6 0 0 1 12 0"></path>
        <path d="M16 5.5a3.5 3.5 0 0 1 0 7"></path>
        <path d="M19 20a6 6 0 0 0-4-5.6"></path>
      </svg>
    {:else if icon === 'leave'}
      <svg width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"></path>
        <path d="M16 17l5-5-5-5M21 12H9"></path>
      </svg>
    {:else if icon === 'more'}
      <svg width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
        <circle cx="5" cy="12" r="1.6" fill="currentColor" stroke="none"></circle>
        <circle cx="12" cy="12" r="1.6" fill="currentColor" stroke="none"></circle>
        <circle cx="19" cy="12" r="1.6" fill="currentColor" stroke="none"></circle>
      </svg>
    {:else if icon === 'expand'}
      <!-- State-aware view switcher: expand from the compact bar into the
           full gallery. Drawn as two outward diagonal arrows so it reads as
           expansion, not upload/download. -->
      <svg width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M15 3h6v6"></path>
        <path d="M21 3l-7 7"></path>
        <path d="M9 21H3v-6"></path>
        <path d="M3 21l7-7"></path>
      </svg>
    {:else if icon === 'collapse'}
      <!-- State-aware view switcher: collapse the full gallery into the
           compact bar. Drawn as two inward diagonal arrows so it reads as
           collapse, not upload/download. -->
      <svg width={iconSize} height={iconSize} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M20 10h-6V4"></path>
        <path d="M21 3l-7 7"></path>
        <path d="M4 14h6v6"></path>
        <path d="M3 21l7-7"></path>
      </svg>
    {/if}
  </span>
</button>

<style>
  .control-button {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-pill);
    border: none;
    padding: 0;
    background: var(--fill-strong);
    color: var(--text-strong);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
    flex-shrink: 0;
    overflow: hidden;
  }

  .control-button.size-compact,
  .control-button.size-menubar {
    overflow: visible;
  }

  .control-button.size-compact::after,
  .control-button.size-menubar::after {
    content: '';
    position: absolute;
    top: 50%;
    left: 50%;
    width: 40px;
    height: 40px;
    transform: translate(-50%, -50%);
  }

  .icon {
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }

  .control-button:hover:not(:disabled) {
    background: var(--fill-bright);
    opacity: 1;
  }

  /* Colored semantic states preserve their hue on hover — opacity dims
     without washing out the green/red identity. */
  .control-button.live:hover:not(:disabled) {
    background: var(--control-live-bg, var(--live-tint));
    opacity: 0.88;
  }

  .control-button.danger:hover:not(:disabled) {
    background: var(--danger-tint-16);
    opacity: 0.88;
  }

  .control-button:active:not(:disabled) {
    opacity: 0.8;
    transform: scale(var(--press-scale, 0.96));
  }

  /* Screensharing is the only toggle whose "on" state gets color. Real
     meeting callers override the fallback with the local sharer's identity
     color + contrast ink; other toggles stay neutral glyph-state patterns. */
  .control-button.live {
    background: var(--control-live-bg, var(--live-tint));
    color: var(--control-live-fg, var(--live-bright));
  }

  .control-button.danger {
    background: var(--danger-tint-16);
    color: var(--danger);
  }

  .control-button.subtle .icon {
    opacity: 0.72;
  }

  .control-button:disabled {
    background: var(--fill-weak);
    color: var(--text-faint);
    opacity: var(--disabled-opacity);
    cursor: default;
  }

  .control-button:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  /* Mic slash draws on/off (DESIGN.md §6) via stroke-dasharray/dashoffset.
     `pathLength="1"` normalizes the path's length to 1 regardless of actual
     geometry, so dasharray/dashoffset can use plain 0-1 values instead of
     measuring the real SVG path length. Hidden = fully offset (not drawn);
     drawn = offset 0 (fully revealed), animating along the stroke. */
  .mic-slash {
    stroke-dasharray: 1;
    stroke-dashoffset: 1;
    transition: stroke-dashoffset var(--motion-base) var(--ease-standard);
  }

  .mic-slash.drawn {
    stroke-dashoffset: 0;
  }

  /* Camera crossfade (DESIGN.md §6): both glyph layers occupy the same cell
     and cross-dissolve via opacity rather than an instant swap. */
  .camera-crossfade,
  .icon-crossfade {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }

  .camera-layer,
  .icon-layer {
    transition: opacity var(--motion-fast) var(--ease-standard);
  }

  .camera-layer.hidden,
  .icon-layer.hidden {
    position: absolute;
    opacity: 0;
  }

  /* Screenshare start green pulse (DESIGN.md §6) — an expanding, fading ring
     behind the icon confirming the share just went live. Uses --motion-base
     as its multiplier so `prefers-reduced-motion`'s global 0ms override
     (tokens.css) collapses this to an instant, non-moving flash instead of a
     lingering animated ring. */
  .share-pulse {
    position: absolute;
    inset: 0;
    border-radius: var(--radius-pill);
    background: var(--control-live-bg, var(--live-tint));
    pointer-events: none;
    animation: share-pulse-ring calc(var(--motion-base) * 3) var(--ease-standard) forwards;
  }

  @keyframes share-pulse-ring {
    from {
      opacity: 0.9;
      transform: scale(0.7);
    }
    to {
      opacity: 0;
      transform: scale(1.6);
    }
  }
</style>
