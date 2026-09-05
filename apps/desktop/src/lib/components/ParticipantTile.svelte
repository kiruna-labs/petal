<!--
  ParticipantTile — one gallery tile. Per Petal-Build-Map.md §2.4 / §3 ("signature
  reversed"): graphite tiles for everyone, NO per-person hue on the tile itself.
  Pulled verbatim from canvas.html's "Full gallery — approved, subdued" board
  (search that file for "Graphite tiles for everyone, no per-person hue"):
  - video tile: soft gradient fill + a plain dark "shoulders + head" silhouette
    (two overlapping dark blobs), name label bottom-left in a translucent chip.
  - camera-off tile: flat graphite fill, no silhouette, centered display-name
    label that falls back to the first grapheme when the full name does not fit.
    This intentionally deviates from the older canvas note that said "no big
    centered initials"; the user approved the centered-name treatment in #137
    on 2026-07-06. Do not "fix" this back to the old spec.
  - speaking: a thin, dim ring + soft halo (box-shadow), NOT identity-colored —
    exact values lifted from canvas.html's `.spk` tile:
    `box-shadow:0 0 0 1.5px rgba(255,255,255,.55),0 0 14px -6px rgba(255,255,255,.22)`.
  - muted: a small circular glyph chip bottom-right holding the mic-off glyph,
    in danger red (`#FF6B5E` / --danger) — matches canvas.html's Marco/Devin
    tiles exactly (`stroke="#FF6B5E"` mic-slash icon in a dark circle chip).

  Decision on Avatar/identity: camera-off stays flat graphite and deliberately
  does not reuse `Avatar`; issue #137 only adds the centered name treatment,
  with the compact Pill Avatar left unchanged.
-->
<script lang="ts">
  import { tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { cameraOffNameLabelForFit, firstGrapheme, nameChipLabelForFit } from '$lib/data/nameChipFit';
  import { colorForIdentity, identityColorCss, identityColorFromPaletteIndex } from '$lib/data/identityColor';
  import { attachVideoStream } from '$lib/videoAttachment';
  import { COMMANDS } from '$lib/ipc';
  import type { DrawUpdate } from '$lib/ipc';
  import { isStrokeExpired, strokeFadeOpacity } from '$lib/data/strokeExpiry';
  import {
    localShareCountPillAriaLabel,
    shareCountPillAriaLabel,
    shareCountPillLabel,
    shouldShowSharePill
  } from '$lib/data/shareCountPill';
  import ControlButton from './ControlButton.svelte';

  interface Props {
    name: string;
    /** Whether this participant's video feed is on. */
    videoOn?: boolean;
    /** Live MediaStream to render as the tile's real video (local webcam
     * self-view today). When unset, `videoOn: true` keeps the
     * static silhouette placeholder (the remote-participant stand-in). */
    videoStream?: MediaStream;
    /** Mirror the video horizontally — the universal convention for a LOCAL
     * self-view only; remote streams must render unmirrored. */
    mirrored?: boolean;
    /** Quiet neutral speaking ring — never identity-colored (Build-Map §3). */
    speaking?: boolean;
    /** Mic-off state — shows the small neutral slashed glyph chip. */
    muted?: boolean;
    /** Weak/degraded connection dot (seen on the Priya tile in canvas.html). */
    weakConnection?: boolean;
    /** Real LiveKit owner identity for camera-tile draw targeting AND (#875)
     * as the `ownerIdentity` arg for the multi-share raise command. */
    ownerIdentity?: string;
    /** Synthetic high-bit camera draw surface id. Never used as a remote window id. */
    drawWindowId?: number;
    /** Append-only stream of camera draw updates delivered by native draw.rs. */
    drawUpdates?: DrawUpdate[];
    /** #875: count of this participant's `petal-window-*` share publications
     * (display shares and viewer-hidden windows included). The pill renders
     * only when this is >= 2 -- one shared window is already covered by the
     * existing sharing indicators. */
    shareCount?: number;
    /** Identity-tinted background/text for the pill -- the same
     * sharing-live colors Gallery derives per participant; the pill only
     * ever renders while that participant is sharing. */
    sharingLiveBackground?: string;
    sharingLiveColor?: string;
    /** True for the local participant's own tile: the pill still shows the
     * count (so you always see how much you're exposing) but is
     * NON-interactive this iteration -- rendered as a plain span, no button
     * semantics, no click handler. */
    isLocal?: boolean;
  }

  let {
    name,
    videoOn = true,
    videoStream,
    mirrored = false,
    speaking = false,
    muted = false,
    weakConnection = false,
    ownerIdentity,
    drawWindowId,
    drawUpdates = [],
    shareCount = 0,
    sharingLiveBackground,
    sharingLiveColor,
    isLocal = false
  }: Props = $props();

  const showSharePill = $derived(shouldShowSharePill(shareCount));
  const sharePillLabel = $derived(shareCountPillLabel(shareCount));
  const sharePillAriaLabel = $derived(
    isLocal ? localShareCountPillAriaLabel(shareCount) : shareCountPillAriaLabel(shareCount, name)
  );

  function handleRaiseParticipantWindows(event: MouseEvent) {
    // The tile wrapper (Gallery.svelte) is itself role="button" with its own
    // pin onclick -- without this the pill click also pins/spotlights the
    // tile underneath it.
    event.stopPropagation();
    if (!ownerIdentity) return;
    void invoke(COMMANDS.compositorRaiseParticipantWindows, { ownerIdentity }).catch(() => {
      // Native side lands in a parallel lane (#875); an unregistered/failing
      // command is a non-fatal no-op here, same posture as the tile's other
      // best-effort invokes.
    });
  }


  interface TileDrawStroke {
    id: string;
    drawerIdentity: string;
    color: string;
    points: { x: number; y: number }[];
    /** #670 fade opacity (1 = fully visible), from strokeFadeOpacity(age). */
    opacity: number;
  }

  // srcObject can't be set via a template attribute — bind the element and
  // assign in an effect (same pattern as Settings.svelte's camera preview).
  let tileEl = $state<HTMLDivElement | null>(null);
  let videoEl = $state<HTMLVideoElement | null>(null);
  let nameChipEl = $state<HTMLDivElement | null>(null);
  let nameMeasureEl = $state<HTMLSpanElement | null>(null);
  let centeredNameMeasureEl = $state<HTMLSpanElement | null>(null);
  let measuredNameChipLabel = $state<string | null>(null);
  let measuredCenteredNameLabel = $state<string | null>(null);
  let visibleVideoStream = $state<MediaStream | null>(null);
  let videoFrameReady = $state(false);
  let measureFrame: number | null = null;
  const nameChipLabel = $derived(measuredNameChipLabel ?? firstGrapheme(name));
  const centeredNameLabel = $derived(measuredCenteredNameLabel ?? firstGrapheme(name));
  const hasVisibleVideoStream = $derived(!!visibleVideoStream);
  const showVideoFill = $derived(videoOn && !videoFrameReady);
  const showCameraOffFill = $derived(!videoOn);
  const showCameraOffName = $derived(!videoOn);
  const videoReady = $derived(videoOn && hasVisibleVideoStream && videoFrameReady);
  const videoDetachDelayMs = 180;

  function clamp01(value: number): number {
    return Number.isFinite(value) ? Math.min(1, Math.max(0, value)) : 0;
  }

  function strokeKey(update: DrawUpdate): string {
    return [
      update.ownerIdentity,
      update.windowId,
      update.drawerIdentity,
      update.strokeId ?? `seq-${update.seq}`
    ].join(':');
  }

  function colorForDrawUpdate(update: DrawUpdate): string {
    return identityColorCss(
      identityColorFromPaletteIndex(update.drawerPaletteIndex) ?? colorForIdentity(update.drawerIdentity)
    );
  }

  function pathFor(points: { x: number; y: number }[]): string {
    if (points.length === 0) return '';
    return points.map((p, index) => `${index === 0 ? 'M' : 'L'} ${p.x} ${p.y}`).join(' ');
  }

  // #670: strokes age out 10s after their LAST point (SPEC.md "ephemeral by
  // default") -- the `clear` wire message type is dead (receive-only, no
  // sender ever emits it) and its handling here has been removed rather
  // than kept as a dead branch (CLAUDE.md "dormant code doesn't merge").
  //
  // `drawUpdates` is the shared, append-only (capped) log of raw DrawUpdate
  // payloads for ALL camera-draw surfaces, replayed fresh into strokes on
  // every change (see meeting/[room]/+page.svelte). It carries no receive
  // timestamp (no wire-format change for #670), so this component stamps
  // one itself the first time it ever sees a given update object -- a
  // WeakSet, not an index/length check, because the log is capped
  // (`.slice(-240)`) and can shift without growing.
  const seenDrawUpdates = new WeakSet<DrawUpdate>();
  let strokeLastPointMs = $state<Record<string, number>>({});
  // Ticks the fade/expiry sweep even when no new stroke data has arrived,
  // same ~250ms cadence as the telepointer/draw sweep in
  // compositor/pointer/+page.svelte.
  let fadeTickMs = $state(performance.now());

  $effect(() => {
    const interval = setInterval(() => {
      const now = performance.now();
      fadeTickMs = now;
      // Prune fully-expired entries so a long meeting's stroke history
      // doesn't grow this map forever -- `drawStrokes` above already
      // excludes expired strokes from what renders; this just keeps the
      // bookkeeping map itself bounded.
      let pruned: Record<string, number> | null = null;
      for (const [key, lastPointMs] of Object.entries(strokeLastPointMs)) {
        if (isStrokeExpired(now - lastPointMs)) {
          if (!pruned) pruned = { ...strokeLastPointMs };
          delete pruned[key];
        }
      }
      if (pruned) strokeLastPointMs = pruned;
    }, 250);
    return () => clearInterval(interval);
  });

  $effect(() => {
    const updates = drawUpdates;
    const now = performance.now();
    let changed = false;
    const next = { ...strokeLastPointMs };
    for (const update of updates) {
      if (seenDrawUpdates.has(update)) continue;
      seenDrawUpdates.add(update);
      if (update.ownerIdentity !== ownerIdentity || update.windowId !== drawWindowId) continue;
      next[strokeKey(update)] = now;
      changed = true;
    }
    if (changed) strokeLastPointMs = next;
  });

  const drawStrokes = $derived.by<TileDrawStroke[]>(() => {
    if (!ownerIdentity || drawWindowId === undefined) return [];
    const strokes = new Map<string, TileDrawStroke>();
    for (const update of drawUpdates) {
      if (update.ownerIdentity !== ownerIdentity || update.windowId !== drawWindowId) continue;
      const incoming = (update.points ?? []).map((p) => ({ x: clamp01(p.x), y: clamp01(p.y) }));
      const key = strokeKey(update);
      const existing = strokes.get(key);
      strokes.set(key, {
        id: key,
        drawerIdentity: update.drawerIdentity,
        color: colorForDrawUpdate(update),
        points: update.type === 'begin' || !existing ? incoming : [...existing.points, ...incoming],
        opacity: 1
      });
    }
    const visible: TileDrawStroke[] = [];
    for (const stroke of strokes.values()) {
      const lastPointMs = strokeLastPointMs[stroke.id];
      const age = lastPointMs === undefined ? 0 : fadeTickMs - lastPointMs;
      if (isStrokeExpired(age)) continue;
      visible.push({ ...stroke, opacity: strokeFadeOpacity(age) });
    }
    return visible;
  });

  function px(value: string): number {
    const parsed = parseFloat(value);
    return Number.isFinite(parsed) ? parsed : 0;
  }

  function relativeWidthPx(value: string, relativeTo: number): number {
    const trimmed = value.trim();
    if (trimmed.endsWith('px')) return px(trimmed);

    const calcMatch = trimmed.match(/^calc\(\s*100%\s*-\s*([0-9.]+)px\s*\)$/);
    if (calcMatch) return Math.max(0, relativeTo - Number(calcMatch[1]));

    const percentMatch = trimmed.match(/^([0-9.]+)%$/);
    if (percentMatch) return (relativeTo * Number(percentMatch[1])) / 100;

    return 0;
  }

  function maxChipContentWidth(): number {
    if (!tileEl || !nameChipEl) return 0;

    const style = getComputedStyle(nameChipEl);
    const maxWidth = relativeWidthPx(style.maxWidth, tileEl.clientWidth);
    const paddingX = px(style.paddingLeft) + px(style.paddingRight);
    const borderX = px(style.borderLeftWidth) + px(style.borderRightWidth);

    if (maxWidth > 0) {
      return Math.max(0, maxWidth - paddingX - borderX);
    }

    const left = px(style.left);
    const right = style.right === 'auto' ? left : px(style.right);
    const inferredMaxWidth = Math.max(0, tileEl.clientWidth - left - right);
    return Math.max(0, inferredMaxWidth - paddingX - borderX);
  }

  function updateNameChipLabel() {
    if (!nameMeasureEl) {
      if (measuredNameChipLabel !== null) measuredNameChipLabel = null;
      return;
    }
    // Pass the currently-rendered label so nameChipLabelForFit can apply its
    // grow/shrink hysteresis (#676) instead of re-deciding from scratch on
    // every ResizeObserver tick; skip the assignment entirely when the
    // result hasn't changed, rather than relying on Svelte's own state
    // dirty-check to no-op it.
    const next = nameChipLabelForFit(
      name,
      nameMeasureEl.scrollWidth,
      maxChipContentWidth(),
      measuredNameChipLabel ?? undefined
    );
    if (next !== measuredNameChipLabel) measuredNameChipLabel = next;
  }

  function maxCenteredNameWidth(): number {
    if (!tileEl) return 0;
    return Math.max(0, tileEl.clientWidth - 32);
  }

  function updateCenteredNameLabel() {
    if (!centeredNameMeasureEl) {
      if (measuredCenteredNameLabel !== null) measuredCenteredNameLabel = null;
      return;
    }
    const next = cameraOffNameLabelForFit(
      name,
      centeredNameMeasureEl.scrollWidth,
      maxCenteredNameWidth(),
      measuredCenteredNameLabel ?? undefined
    );
    if (next !== measuredCenteredNameLabel) measuredCenteredNameLabel = next;
  }

  function updateMeasuredLabels() {
    updateNameChipLabel();
    updateCenteredNameLabel();
  }

  function scheduleMeasuredLabels() {
    if (typeof requestAnimationFrame === 'undefined') {
      updateMeasuredLabels();
      return;
    }

    if (measureFrame !== null) cancelAnimationFrame(measureFrame);
    measureFrame = requestAnimationFrame(() => {
      measureFrame = null;
      updateMeasuredLabels();
    });
  }

  function markVideoFrameReady(stream: MediaStream | null = visibleVideoStream) {
    if (stream && stream === visibleVideoStream && videoOn) videoFrameReady = true;
  }

  $effect(() => {
    attachVideoStream(videoEl, visibleVideoStream);
  });

  $effect(() => {
    let detachTimer: ReturnType<typeof setTimeout> | null = null;
    const nextStream = videoOn ? (videoStream ?? null) : null;

    if (nextStream) {
      if (visibleVideoStream !== nextStream) {
        visibleVideoStream = nextStream;
        videoFrameReady = false;
      }
    } else {
      videoFrameReady = false;
      if (visibleVideoStream) {
        detachTimer = setTimeout(() => {
          if (!videoOn) visibleVideoStream = null;
        }, videoDetachDelayMs);
      } else {
        visibleVideoStream = null;
      }
    }

    return () => {
      if (detachTimer) clearTimeout(detachTimer);
    };
  });

  $effect(() => {
    const video = videoEl;
    const stream = visibleVideoStream;
    if (!video || !stream) return;

    let cancelled = false;
    const markReady = () => {
      if (!cancelled) markVideoFrameReady(stream);
    };
    const frameVideo = video as HTMLVideoElement & {
      requestVideoFrameCallback?: (callback: () => void) => number;
      cancelVideoFrameCallback?: (handle: number) => void;
    };
    const callbackHandle = frameVideo.requestVideoFrameCallback?.(markReady);

    video.addEventListener('loadeddata', markReady, { once: true });
    if (video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA && typeof requestAnimationFrame !== 'undefined') {
      requestAnimationFrame(markReady);
    }

    return () => {
      cancelled = true;
      video.removeEventListener('loadeddata', markReady);
      if (callbackHandle !== undefined) frameVideo.cancelVideoFrameCallback?.(callbackHandle);
    };
  });

  $effect(() => {
    const currentName = name;
    const currentVideoOn = videoOn;
    measuredNameChipLabel = null;
    measuredCenteredNameLabel = null;
    void tick().then(() => {
      if (name === currentName && videoOn === currentVideoOn) updateMeasuredLabels();
    });
  });

  $effect(() => {
    const tile = tileEl;
    if (!tile || typeof ResizeObserver === 'undefined') return;

    const observer = new ResizeObserver(scheduleMeasuredLabels);
    observer.observe(tile);
    updateMeasuredLabels();

    return () => {
      observer.disconnect();
      if (measureFrame !== null) {
        cancelAnimationFrame(measureFrame);
        measureFrame = null;
      }
    };
  });

  $effect(() => {
    if (typeof document === 'undefined' || !('fonts' in document)) return;

    let cancelled = false;
    void document.fonts.ready.then(() => {
      if (!cancelled) updateMeasuredLabels();
    });

    return () => {
      cancelled = true;
    };
  });

</script>

<div bind:this={tileEl} class="tile" class:speaking class:weakConnection>
  <!-- Keep every visual layer mounted so camera toggles and resubscribes crossfade instead of destroying/recreating
       the video element. The real video only fades in after a decoded frame is ready. -->
  <div class="video-fill" class:active={showVideoFill} aria-hidden="true">
    <div class="sheen"></div>
    <div class="shoulders"></div>
    <div class="head"></div>
  </div>

  <div class="off-fill" class:active={showCameraOffFill} aria-hidden="true"></div>

  <!-- Real video (local self-view). Muted + playsinline are required for reliable autoplay;
       mirroring is self-view-only. -->
  <!-- svelte-ignore a11y_media_has_caption -->
  <video
    class="video-el"
    class:mirrored
    class:ready={videoReady}
    bind:this={videoEl}
    autoplay
    muted
    playsinline
    onloadeddata={() => markVideoFrameReady()}
  ></video>

  {#if drawStrokes.length > 0}
    <svg class="draw-layer" viewBox="0 0 1 1" preserveAspectRatio="none" aria-hidden="true">
      {#each drawStrokes as stroke (stroke.id)}
        {#if stroke.points.length > 0}
          <path d={pathFor(stroke.points)} stroke={stroke.color} style:opacity={stroke.opacity}></path>
        {/if}
      {/each}
    </svg>
  {/if}

  <!-- Camera-off: plain minimal tile, flat graphite, centered name only.
       title carries the FULL name when the fit label truncates it. -->
  <span class="camera-off-name" class:active={showCameraOffName} aria-label={name} title={name}>{centeredNameLabel}</span>
  <span class="camera-off-name camera-off-name-measure" bind:this={centeredNameMeasureEl} aria-hidden="true"
    >{name}</span
  >

  {#if videoOn}
    <div class="name-chip" bind:this={nameChipEl} title={name}>
      <span class="name-chip-visible">{nameChipLabel}</span>
      <span class="name-chip-measure" bind:this={nameMeasureEl} aria-hidden="true">{name}</span>
    </div>
  {/if}

  {#if muted}
    <!-- Status glyph, not a control: kept out of the tab order so every
         muted participant does not add a dead focus stop (one per tile). -->
    <div class="muted-chip" title="Muted">
      <ControlButton icon="mic" kind="toggle" active size="menubar" tabindex={-1} label={`${name} is muted`} />
    </div>
  {/if}

  {#if weakConnection}
    <div class="pause-hint" title="Video paused — weak connection" aria-live="polite">
      <span class="pause-icon" aria-hidden="true">
        <span></span>
        <span></span>
      </span>
      <span>Video paused</span>
    </div>
  {/if}

  {#if showSharePill}
    <!-- #875: top-left is the one open corner -- name chip owns bottom-left,
         muted-chip/pause-hint own bottom-right, the Gallery pin mark owns
         top-right. -->
    {#if isLocal}
      <span
        class="share-count-pill"
        style:background={sharingLiveBackground}
        style:color={sharingLiveColor}
        title={sharePillAriaLabel}
        aria-label={sharePillAriaLabel}
      >{sharePillLabel}</span>
    {:else}
      <button
        type="button"
        class="share-count-pill interactive"
        style:background={sharingLiveBackground}
        style:color={sharingLiveColor}
        aria-label={sharePillAriaLabel}
        title={sharePillAriaLabel}
        onclick={handleRaiseParticipantWindows}
      >{sharePillLabel}</button>
    {/if}
  {/if}
</div>

<style>
  .tile {
    position: relative;
    /* 16px lifted verbatim from canvas.html's approved gallery board
       (`border-radius:16px` on every tile) — --radius-tile now carries the
       comp value (issue #14 item 6). */
    border-radius: var(--radius-tile);
    overflow: hidden;
    background: linear-gradient(160deg, var(--surface-raised), var(--surface));
    min-width: 0;
    min-height: 0;
    outline: 1px solid var(--hairline-strong);
    outline-offset: -1px;
  }

  /* Quiet neutral speaking ring — thinner, dimmer, minimal halo. Deliberately
     NOT an identity color; values match canvas.html's `.spk` tile exactly.
     "Breathing" (DESIGN.md §6: "the one ambient, always-on motion — keep it
     gentle") — a slow, subtle opacity/glow oscillation on the same ring, not
     a new visual element. Kept gentle per the spec's own instruction: long
     duration (2.6s), small amplitude, ease-in-out. */
  .tile.speaking {
    box-shadow:
      0 0 0 1.5px rgba(255, 255, 255, 0.55),
      0 0 6px -4px rgba(255, 255, 255, 0.22);
    animation: speaking-breathe 2.6s ease-in-out infinite;
  }

  @keyframes speaking-breathe {
    0%,
    100% {
      box-shadow:
        0 0 0 1.5px rgba(255, 255, 255, 0.55),
        0 0 6px -4px rgba(255, 255, 255, 0.22);
    }
    50% {
      box-shadow:
        0 0 0 1.5px rgba(255, 255, 255, 0.8),
        0 0 10px -3px rgba(255, 255, 255, 0.34);
    }
  }

  /* prefers-reduced-motion (DESIGN.md §6 / SPEC.md §5): tokens.css already
     zeroes --motion-fast/--motion-base globally, but this is a fixed-duration
     `animation` (not one of those variables, since "gentle 2.6s breathing"
     doesn't map to a micro-interaction timing token) — so it needs its own
     explicit fallback to actually stop, not just speed up to 0 like the
     variable-driven transitions elsewhere in this file. */
  @media (prefers-reduced-motion: reduce) {
    .tile.speaking {
      animation: none;
    }
  }

  /* Participant join/leave animate in/out (DESIGN.md §6) is applied at the
     call site via Gallery's keyed Svelte transition (restrained opacity) —
     that's where mount/unmount lifecycle actually happens, so it stays out
     of this tile's media/state styles. */

  .video-el {
    position: absolute;
    inset: 0;
    z-index: 2;
    width: 100%;
    height: 100%;
    object-fit: cover;
    opacity: 0;
    outline: 1px solid var(--hairline-strong);
    outline-offset: -1px;
    transition: opacity var(--motion-base) var(--ease-standard);
    /* .tile's border-radius + overflow:hidden clip this; no own radius needed. */
  }

  .video-el.ready {
    opacity: 1;
  }

  .video-el.mirrored {
    transform: scaleX(-1);
  }

  .video-fill {
    position: absolute;
    inset: 0;
    z-index: 0;
    opacity: 0;
    transition: opacity var(--motion-base) var(--ease-standard);
  }

  .video-fill.active {
    opacity: 1;
  }

  .sheen {
    position: absolute;
    inset: 0;
    background: linear-gradient(180deg, var(--fill-weak), transparent 45%);
  }

  /* Silhouette dimensions are the comp's literal px values (canvas.html
     gallery board: shoulders 170x96 at bottom:-18px, head 62x62 at
     bottom:62px) — previously approximated as percentages, which drifted
     from the comp at its own 1280x800 reference size (issue #14 item 2).
     Fixed px also reads correctly as "a person at camera distance" when the
     tile resizes, instead of the silhouette scaling with the tile. */
  .shoulders {
    position: absolute;
    left: 50%;
    bottom: -18px;
    transform: translateX(-50%);
    width: 170px;
    height: 96px;
    border-radius: 80px 80px 0 0;
    background: rgba(0, 0, 0, 0.3);
  }

  .head {
    position: absolute;
    left: 50%;
    bottom: 62px;
    transform: translateX(-50%);
    width: 62px;
    height: 62px;
    border-radius: var(--radius-pill); /* comp says 99px — identical render on a 62px circle */
    background: rgba(0, 0, 0, 0.3);
  }

  .off-fill {
    position: absolute;
    inset: 0;
    z-index: 0;
    opacity: 0;
    transition: opacity var(--motion-base) var(--ease-standard);
    /* Comp-lifted graphite from the camera-off tiles (Marco/Sana):
       `linear-gradient(160deg,#202124,#17181b)` — --graphite-gradient (issue
       #14 item 2). */
    background: var(--graphite-gradient);
  }

  .off-fill.active {
    opacity: 1;
  }

  .camera-off-name {
    position: absolute;
    left: 50%;
    top: 50%;
    z-index: 3;
    max-width: calc(100% - 32px);
    opacity: 0;
    transform: translate(-50%, -50%);
    font: 700 26px var(--font-display);
    line-height: 1;
    color: var(--text-soft);
    white-space: nowrap;
    user-select: none;
    transition: opacity var(--motion-base) var(--ease-standard);
  }

  .camera-off-name.active {
    opacity: 1;
  }

  .camera-off-name-measure {
    visibility: hidden;
    pointer-events: none;
  }

  .name-chip {
    position: absolute;
    left: 14px;
    bottom: 12px;
    z-index: 4;
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 5px 10px;
    border-radius: var(--radius-chip);
    background: var(--glass-name);
    backdrop-filter: blur(8px);
    font: 600 12px var(--font-ui);
    color: var(--text-primary);
    max-width: calc(100% - 28px);
  }

  .draw-layer {
    position: absolute;
    inset: 0;
    z-index: 4;
    width: 100%;
    height: 100%;
    pointer-events: none;
  }

  .draw-layer path {
    fill: none;
    stroke-width: 0.012;
    stroke-linecap: round;
    stroke-linejoin: round;
    vector-effect: non-scaling-stroke;
    filter: drop-shadow(0 1px 2px rgba(0, 0, 0, 0.45));
    /* #670: fade out smoothly as the sweep interval counts `opacity` down
       toward 0, rather than an abrupt pop when the stroke ages out. */
    transition: opacity var(--motion-enter) var(--ease-standard);
  }

  .name-chip-visible,
  .name-chip-measure {
    display: inline-block;
    white-space: nowrap;
  }

  .name-chip-measure {
    position: absolute;
    visibility: hidden;
    pointer-events: none;
  }

  .muted-chip {
    position: absolute;
    right: 12px;
    bottom: 12px;
    z-index: 4;
  }

  /* ControlButton is styled for interaction (cursor, hover opacity); here
     it's a static status glyph, not a clickable per-tile mute control, so
     interaction is suppressed while keeping the exact approved glyph/tone. */
  .muted-chip :global(.control-button) {
    pointer-events: none;
    background: rgba(8, 10, 12, 0.7);
  }

  /* Comp's muted chip glyph is 13x13 at stroke-width 2.4 inside the 24px
     chip (canvas.html: `<svg width="13" height="13" ... stroke-width="2.4">`);
     ControlButton's menubar size renders 12x12 at stroke-width 2 — corrected
     here at the call site (issue #14 item 5) rather than changing
     ControlButton's own size template, which other surfaces share. */
  .muted-chip :global(.control-button .icon) {
    width: 13px !important;
    height: 13px !important;
  }

  .muted-chip :global(.control-button svg) {
    width: 13px;
    height: 13px;
    stroke-width: 2.4;
  }

  .pause-hint {
    position: absolute;
    right: 12px;
    bottom: 14px;
    z-index: 5;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    max-width: calc(100% - 24px);
    padding: 5px 8px;
    border-radius: var(--radius-pill);
    background: var(--glass-chip);
    color: var(--text-strong);
    font: 650 11px var(--font-ui);
    line-height: 1;
    backdrop-filter: blur(10px);
    box-shadow: inset 0 0 0 1px var(--fill-strong);
  }

  .tile.weakConnection .video-el,
  .tile.weakConnection .video-fill {
    filter: saturate(0.78) brightness(0.68);
  }

  .pause-icon {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    width: 10px;
    height: 10px;
    flex: 0 0 auto;
  }

  .pause-icon span {
    width: 3px;
    height: 9px;
    border-radius: var(--radius-pill); /* 2px clamps to the 3px bar width — identical capsule */
    background: currentColor;
    opacity: 0.85;
  }

  /* #875: multi-share count pill -- top-left is the only open corner
     (name-chip/muted-chip/pause-hint own the bottom corners, the Gallery
     pin mark owns top-right). Sized/positioned on the weak-connection
     `.pause-hint` pattern above; background/color come from the sharing
     participant's identity-tinted colors (the pill only ever renders while
     that participant is sharing, so a colored chip here doesn't break the
     "graphite tiles, no per-person hue" rule -- sharing is the one
     sanctioned colored state, same reasoning as the sharing tile border). */
  .share-count-pill {
    position: absolute;
    left: 12px;
    top: 12px;
    z-index: 5;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 20px;
    height: 20px;
    padding: 0 6px;
    box-sizing: border-box;
    border: none;
    border-radius: var(--radius-pill);
    background: var(--fill-strong);
    color: var(--text-strong);
    font: 700 11px var(--font-ui);
    font-variant-numeric: tabular-nums;
    line-height: 1;
    box-shadow: inset 0 0 0 1px var(--hairline-strong);
    user-select: none;
  }

  /* Non-interactive (local tile): plain span, default cursor -- no hover/
     press affordance, since clicking it does nothing this iteration. */
  span.share-count-pill {
    cursor: default;
  }

  button.share-count-pill.interactive {
    cursor: pointer;
    transition:
      filter var(--motion-fast) var(--ease-standard),
      scale var(--motion-fast) var(--ease-standard);
  }

  button.share-count-pill.interactive:hover {
    filter: brightness(1.1);
  }

  button.share-count-pill.interactive:active {
    scale: var(--press-scale, 0.96);
  }

  button.share-count-pill.interactive:focus-visible {
    outline: 1px solid var(--text-faint);
    outline-offset: 2px;
  }

  @media (prefers-reduced-motion: reduce) {
    button.share-count-pill.interactive {
      transition: none;
    }

    button.share-count-pill.interactive:active {
      scale: 1;
    }
  }
</style>
