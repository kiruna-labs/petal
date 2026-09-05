<!--
  Real telepointer overlay for a remote compositor window (SPEC.md §4.5) —
  this is the real place `telepointer-update` events should render, per that
  module's own doc comment ("a future real remote-window compositor should
  listen on whichever per-window webview it creates instead"). Previously
  the only receiver was `/dev/telepointer`'s static mock rectangle; this page
  is the real one, running as a transparent, click-through child webview
  layered directly above the remote window's video content
  (src-tauri/src/compositor.rs's `create_pointer_overlay` — click-through via
  `set_ignore_cursor_events(true)`, so it never blocks interaction with the
  video/header beneath it).

  Filters `telepointer-update` events to this page's own `windowId` (each
  compositor window gets its OWN pointer-overlay webview instance, one per
  remote share — unlike `/dev/telepointer`, which only ever showed one
  window's pointers at a time in a single dev harness window).

  Coordinate mapping: `Pointer.svelte` already expects normalized 0-1
  coordinates and positions itself with `left: x*100%` / `top: y*100%` (see
  that component) — since this overlay window is sized to exactly the video
  content area (kept in sync with the real source resolution by
  `compositor.rs`'s `resize_to_source`), simply rendering at 0-1 against this
  page's own 100%-sized surface IS "using the real window's actual current
  size to map normalized coords to pixels" (task requirement) with no extra
  math needed here — the sizing work already happened on the Rust side.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import Pointer from '$lib/components/Pointer.svelte';
  import type { IdentityColor } from '$lib/components/Avatar.svelte';
  import { colorForIdentity, identityColorCss, identityColorFromPaletteIndex } from '$lib/data/identityColor';
  import { friendlyTelepointerName } from '$lib/data/telepointerDisplay';
  import { COMMANDS } from '$lib/ipc';
  import type { DrawDraft, DrawPoint, DrawUpdate, TelepointerUpdate } from '$lib/ipc';
  import { isStrokeExpired, strokeFadeOpacity } from '$lib/data/strokeExpiry';
  import { session } from '$lib/stores/session.svelte';

  const routeWindowId = $derived(Number(page.url.searchParams.get('windowId') ?? '0'));
  const sharerSurface = $derived(page.url.searchParams.get('surface') === 'sharer');
  const shareBorderEnabled = $derived(
    sharerSurface && page.url.searchParams.get('shareBorder') === '1'
  );
  const shareBorderColor = $derived(
    /^#[0-9a-f]{6}$/i.test(page.url.searchParams.get('shareBorderColor') ?? '')
      ? page.url.searchParams.get('shareBorderColor')!
      : identityColorCss('plum')
  );
  // The center stop-drawing toolbar is reserved for full-display shares.
  // Petal View and ordinary shared windows have a hover-tab menu with the
  // same stop action, so a second popover would both duplicate UI and cover
  // hover targets.
  let sharerDrawToolbar = $state(page.url.searchParams.get('drawToolbar') === '1');
  const routeOwnerIdentity = $derived(page.url.searchParams.get('ownerIdentity') ?? '');
  let windowIdOverride = $state<number | null>(null);
  let ownerIdentityOverride = $state<string | null>(null);
  const windowId = $derived(windowIdOverride ?? routeWindowId);
  const ownerIdentity = $derived(ownerIdentityOverride ?? routeOwnerIdentity);

  interface TrackedPointer {
    userId: string;
    name: string;
    x: number;
    y: number;
    visible: boolean;
    identity: IdentityColor;
    lastUpdateMs: number;
    idle: boolean;
    pulseKey: number;
    controlActive: boolean;
    typing: boolean;
    lastClickMs: number;
  }

  interface HandshakeBurst {
    id: number;
    x: number;
    y: number;
  }

  interface TrackedStroke {
    strokeId: string;
    drawerIdentity: string;
    identity: IdentityColor;
    points: DrawPoint[];
    complete: boolean;
    /** performance.now() timestamp of the last point received for this
     * stroke (#670) -- ages from the LAST point, not the first, so a
     * stroke still being actively extended never starts fading mid-draw.
     * Updated on every begin/points/end touch, in applyDrawUpdate below. */
    lastPointMs: number;
    /** Current fade opacity (1 = fully visible), recomputed by the sweep
     * interval below from `lastPointMs`. */
    opacity: number;
  }

  interface TrackedTextAnnotation {
    annotationId: string;
    drawerIdentity: string;
    identity: IdentityColor;
    anchor: DrawPoint;
    text: string;
    lastPointMs: number;
    opacity: number;
  }

  function identityFor(userId: string, paletteIndex?: number | null): IdentityColor {
    return identityColorFromPaletteIndex(paletteIndex) ?? colorForIdentity(userId);
  }

  // Same idle/stale thresholds as /dev/telepointer (SPEC.md §4.5 "fade idle
  // pointers", client-side-only timeout).
  const IDLE_MS = 2500;
  const STALE_MS = 8000;
  const ACTIVITY_TRAIL_MS = 1500;
  const HANDSHAKE_WINDOW_MS = 1000;
  const HANDSHAKE_DISTANCE = 0.06;
  const HANDSHAKE_COOLDOWN_MS = 3000;
  const MAX_DRAW_TEXT_CHARS = 256;
  const DRAW_REPOSITION_THRESHOLD_PX = 6;

  let pointers = $state<Record<string, TrackedPointer>>({});
  let strokes = $state<Record<string, TrackedStroke>>({});
  let textAnnotations = $state<Record<string, TrackedTextAnnotation>>({});
  let drawActive = $state(false);
  const drawCursor = $derived(penCursor(identityColorCss(session.identity)));
  let drawPointerId = $state<number | null>(null);
  let drawStrokeId = $state<string | null>(null);
  let drawSeq = 0;
  let drawFlushTimer: ReturnType<typeof setTimeout> | undefined;
  let pendingDrawPoints: DrawPoint[] = [];
  let drawAnchor = $state<DrawPoint | null>(null);
  let drawTextAnchor = $state<DrawPoint | null>(null);
  let drawTextDraft = $state('');
  let drawTextStrokeId = $state<string | null>(null);
  let composing = false;
  let overlayElement: HTMLDivElement | null = null;
  let handshakes = $state<HandshakeBurst[]>([]);
  let nextHandshakeId = 1;
  const activityTimers = new Map<string, ReturnType<typeof setTimeout>>();
  const handshakeCooldowns = new Map<string, number>();

  function clearActivityTimer(userId: string) {
    const timer = activityTimers.get(userId);
    if (timer) clearTimeout(timer);
    activityTimers.delete(userId);
  }

  function scheduleActivityClear(userId: string, keepTyping: boolean) {
    clearActivityTimer(userId);
    activityTimers.set(
      userId,
      setTimeout(() => {
        const p = pointers[userId];
        if (p) {
          pointers[userId] = { ...p, controlActive: false, typing: keepTyping ? false : p.typing };
        }
        activityTimers.delete(userId);
      }, ACTIVITY_TRAIL_MS)
    );
  }

  function clearOverlayState() {
    if (drawActive) commitDrawText();
    pointers = {};
    strokes = {};
    textAnnotations = {};
    drawActive = false;
    drawAnchor = null;
    clearDrawInput();
    handshakes = [];
    nextHandshakeId = 1;
    activityTimers.forEach((timer) => clearTimeout(timer));
    activityTimers.clear();
    handshakeCooldowns.clear();
  }

  function setOverlayWindowId(targetWindowId: number) {
    if (!Number.isFinite(targetWindowId)) return;
    if (drawActive) commitDrawText();
    windowIdOverride = targetWindowId;
    clearOverlayState();
  }

  function setOverlayOwnerIdentity(targetOwnerIdentity: string) {
    if (!targetOwnerIdentity.trim()) return;
    if (drawActive) commitDrawText();
    ownerIdentityOverride = targetOwnerIdentity;
  }

  function nextDrawSeq() {
    drawSeq = drawSeq >= Number.MAX_SAFE_INTEGER ? 1 : drawSeq + 1;
    return drawSeq;
  }

  function createStrokeId() {
    return `${windowId}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
  }

  function penCursor(color: string) {
    const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="28" height="28" viewBox="0 0 28 28"><path d="M5 23l2.3-7.1L18.9 4.3a2.6 2.6 0 0 1 3.7 3.7L11 19.6 5 23Z" fill="${color}" stroke="#071018" stroke-width="1.8" stroke-linejoin="round"/><path d="M16.8 6.4l4.8 4.8" stroke="#ffffff" stroke-width="1.4" stroke-linecap="round" opacity=".9"/></svg>`;
    return `url("data:image/svg+xml,${encodeURIComponent(svg)}") 5 23, crosshair`;
  }

  function normalizedDrawPoint(event: PointerEvent): DrawPoint | null {
    const target = event.currentTarget as HTMLElement;
    const rect = target.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return null;
    return {
      x: Math.max(0, Math.min(1, (event.clientX - rect.left) / rect.width)),
      y: Math.max(0, Math.min(1, (event.clientY - rect.top) / rect.height))
    };
  }

  function sendLocalDraw(type: DrawDraft['type'], strokeId: string, points: DrawPoint[] = [], text?: string) {
    if (!ownerIdentity) return;
    invoke(COMMANDS.drawSend, {
      draft: {
        type,
        windowId,
        ownerIdentity,
        strokeId,
        seq: nextDrawSeq(),
        points,
        ...(text === undefined ? {} : { text })
      }
    }).catch(() => {});
  }

  function drawPointMoved(a: DrawPoint, b: DrawPoint) {
    return Math.hypot(
      (a.x - b.x) * Math.max(1, window.innerWidth),
      (a.y - b.y) * Math.max(1, window.innerHeight)
    ) >= DRAW_REPOSITION_THRESHOLD_PX;
  }

  function clearDrawTextDraft() {
    drawTextAnchor = null;
    drawTextDraft = '';
    drawTextStrokeId = null;
  }

  function commitDrawText() {
    if (drawTextAnchor && drawTextDraft.trim() && drawTextStrokeId) {
      sendLocalDraw('text', drawTextStrokeId, [drawTextAnchor], drawTextDraft);
    }
    clearDrawTextDraft();
  }

  function cancelDrawText() {
    clearDrawTextDraft();
  }

  function appendDrawText(text: string) {
    if (!drawActive || !drawAnchor || !text) return;
    const next = `${drawTextDraft}${text}`;
    if (!drawTextAnchor) {
      drawTextAnchor = { ...drawAnchor };
      drawTextStrokeId = createStrokeId();
    }
    drawTextDraft = [...next]
      .filter((character) => !/[\n\r\u2028\u2029]/u.test(character))
      .slice(0, MAX_DRAW_TEXT_CHARS)
      .join('');
  }

  function onDrawKey(event: KeyboardEvent) {
    const target = event.target as HTMLElement | null;
    if (target?.closest('.sharer-draw-toolbar')) return;
    if (!drawActive || event.isComposing || composing) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.key === 'Escape') {
      cancelDrawText();
      return;
    }
    if (event.key === 'Enter') return;
    if (event.key === 'Backspace') {
      drawTextDraft = [...drawTextDraft].slice(0, -1).join('');
      if (!drawTextDraft) clearDrawTextDraft();
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey || event.key.length !== 1) return;
    appendDrawText(event.key);
  }

  function onCompositionStart() {
    composing = true;
  }

  function onCompositionEnd(event: CompositionEvent) {
    composing = false;
    appendDrawText(event.data ?? '');
  }

  function clearDrawInput() {
    pendingDrawPoints = [];
    if (drawFlushTimer) {
      clearTimeout(drawFlushTimer);
      drawFlushTimer = undefined;
    }
    drawPointerId = null;
    drawStrokeId = null;
  }

  function flushDrawPoints() {
    if (drawFlushTimer) {
      clearTimeout(drawFlushTimer);
      drawFlushTimer = undefined;
    }
    const strokeId = drawStrokeId;
    const points = pendingDrawPoints;
    pendingDrawPoints = [];
    if (!drawActive || !strokeId || points.length === 0) return;
    for (let index = 0; index < points.length; index += 128) {
      sendLocalDraw('points', strokeId, points.slice(index, index + 128));
    }
  }

  function scheduleDrawFlush() {
    if (drawFlushTimer) return;
    drawFlushTimer = setTimeout(() => {
      drawFlushTimer = undefined;
      flushDrawPoints();
    }, 50);
  }

  async function stopSharerDraw(event: MouseEvent) {
    event.preventDefault();
    event.stopPropagation();
    if (!sharerSurface || !drawActive) return;
    try {
      await invoke(COMMANDS.shareOverlaySetDrawActive, { windowId, active: false });
    } catch {
      // Keep Draw active if native click-through restoration failed.
    }
  }

  function stopToolbarPointer(event: PointerEvent) {
    event.stopPropagation();
  }

  function onSharerPointerDown(event: PointerEvent) {
    if (!sharerSurface || !drawActive) return;
    const point = normalizedDrawPoint(event);
    if (!point) return;
    commitDrawText();
    drawAnchor = point;
    event.preventDefault();
    drawPointerId = event.pointerId;
    drawStrokeId = createStrokeId();
    try {
      (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    } catch {
      // Pointer capture is best effort for synthetic/non-primary events.
    }
    sendLocalDraw('begin', drawStrokeId, [point]);
  }

  function onSharerPointerMove(event: PointerEvent) {
    if (!sharerSurface || !drawActive) return;
    if (drawPointerId !== null && drawPointerId !== event.pointerId) return;
    const point = normalizedDrawPoint(event);
    if (!point) return;
    if (drawAnchor && drawPointMoved(drawAnchor, point)) commitDrawText();
    drawAnchor = point;
    if (!drawStrokeId) return;
    event.preventDefault();
    pendingDrawPoints = [...pendingDrawPoints, point];
    scheduleDrawFlush();
  }

  function onSharerPointerUp(event: PointerEvent) {
    if (!sharerSurface || !drawActive) return;
    if (drawPointerId !== null && drawPointerId !== event.pointerId) return;
    event.preventDefault();
    flushDrawPoints();
    const point = normalizedDrawPoint(event);
    const strokeId = drawStrokeId;
    if (strokeId) sendLocalDraw('end', strokeId, point ? [point] : []);
    try {
      (event.currentTarget as HTMLElement).releasePointerCapture(event.pointerId);
    } catch {
      // Pointer capture is best effort for synthetic/non-primary events.
    }
    clearDrawInput();
  }

  function maybeHandshake(current: TrackedPointer, now: number) {
    if (current.lastClickMs <= 0) return;
    for (const other of Object.values(pointers)) {
      if (other.userId === current.userId || other.lastClickMs <= 0) continue;
      if (Math.abs(current.lastClickMs - other.lastClickMs) > HANDSHAKE_WINDOW_MS) continue;
      const distance = Math.hypot(current.x - other.x, current.y - other.y);
      if (distance > HANDSHAKE_DISTANCE) continue;
      const pair = [current.userId, other.userId].sort().join(':');
      const cooldownUntil = handshakeCooldowns.get(pair) ?? 0;
      if (cooldownUntil > now) continue;
      handshakeCooldowns.set(pair, now + HANDSHAKE_COOLDOWN_MS);
      const id = nextHandshakeId++;
      handshakes = [...handshakes, { id, x: (current.x + other.x) / 2, y: (current.y + other.y) / 2 }];
      setTimeout(() => {
        handshakes = handshakes.filter((burst) => burst.id !== id);
      }, 1200);
      return;
    }
  }

  function applyUpdate(update: TelepointerUpdate) {
    if (update.windowId !== windowId) return;
    // Window ids are publisher-local. Modern senders include the surface owner
    // so two participants reusing the same id cannot light up both overlays.
    if (update.surfaceOwnerId && ownerIdentity && update.surfaceOwnerId !== ownerIdentity) return;
    const now = performance.now();
    const previous = pointers[update.userId];
    // Snap the WHOLE tag box to a whole DEVICE pixel. Pointer.svelte renders
    // the layer at `left/top: x%` then applies `translate(-4.583px,-2.75px)`
    // (which counter-offsets the glyph so its arrow TIP sits at left/top). The
    // translate leaves the tag's rendered box at a FRACTIONAL device offset
    // (integer left - 4.583), so WebView2/Chromium rasterizes it at slightly
    // different pixel positions across frames — a ~1px NW-SE shimmer even for
    // a fully stationary cursor (016: input constant to 6dp, abs rect still
    // wobbling). Snap so the composed box (left - AX) is an integer device px.
    const AX = 4.583;
    const AY = 2.75;
    const dpr = window.devicePixelRatio || 1;
    const w = window.innerWidth || 1;
    const h = window.innerHeight || 1;
    const cssX = update.x * w;
    const cssY = update.y * h;
    const snappedX = (Math.round((cssX - AX) * dpr) / dpr + AX) / w;
    const snappedY = (Math.round((cssY - AY) * dpr) / dpr + AY) / h;
    const next: TrackedPointer = {
      userId: update.userId,
      name: friendlyTelepointerName(update.displayName, update.userId),
      x: snappedX,
      y: snappedY,
      visible: update.visible,
      identity: identityFor(update.userId, update.paletteIndex),
      lastUpdateMs: now,
      idle: false,
      pulseKey: previous?.pulseKey ?? 0,
      controlActive: previous?.controlActive ?? false,
      typing: previous?.typing ?? false,
      lastClickMs: previous?.lastClickMs ?? 0
    };
    if (update.activity === 'click') {
      next.pulseKey += 1;
      next.controlActive = true;
      next.lastClickMs = now;
      scheduleActivityClear(update.userId, false);
    } else if (update.activity === 'type') {
      next.controlActive = true;
      next.typing = true;
      scheduleActivityClear(update.userId, true);
    }
    pointers[update.userId] = next;
    if (update.activity === 'click') maybeHandshake(next, now);
  }

  function strokeKey(update: DrawUpdate) {
    return `${update.drawerIdentity}:${update.strokeId}`;
  }

  function textAnnotationStyle(annotation: TrackedTextAnnotation) {
    const alignRight = annotation.anchor.x > 0.62;
    const available = Math.max(0.04, alignRight ? annotation.anchor.x : 1 - annotation.anchor.x);
    const estimatedTextWidth = Math.max(0.01, [...annotation.text].length * 0.0085 + 0.025);
    const horizontalScale = Math.min(1, available / estimatedTextWidth);
    return `left:${annotation.anchor.x * 100}%; top:${annotation.anchor.y * 100}%; transform:translate(${alignRight ? '-100%' : '0'}, -50%) scaleX(${horizontalScale}); transform-origin:${alignRight ? 'right' : 'left'} center;`;
  }

  function drawTextDraftStyle() {
    if (!drawTextAnchor) return '';
    const alignRight = drawTextAnchor.x > 0.62;
    const available = Math.max(0.04, alignRight ? drawTextAnchor.x : 1 - drawTextAnchor.x);
    const estimatedTextWidth = Math.max(0.01, [...drawTextDraft].length * 0.0085 + 0.025);
    const horizontalScale = Math.min(1, available / estimatedTextWidth);
    return `left:${drawTextAnchor.x * 100}%; top:${drawTextAnchor.y * 100}%; transform:translate(${alignRight ? '-100%' : '0'}, -50%) scaleX(${horizontalScale}); transform-origin:${alignRight ? 'right' : 'left'} center;`;
  }

  function normalizedDrawPoints(points: DrawPoint[] | undefined): DrawPoint[] {
    return (points ?? []).map((point) => ({
      x: Math.max(0, Math.min(1, point.x)),
      y: Math.max(0, Math.min(1, point.y))
    }));
  }

  function polylinePoints(points: DrawPoint[]) {
    return points.map((point) => `${point.x * 100},${point.y * 100}`).join(' ');
  }

  function applyDrawUpdate(update: DrawUpdate) {
    // #670: the `clear` message type is receive-only dead code -- no sender
    // (native or web) ever emits it (a 10s auto-fade below replaces the
    // need for an explicit clear). `!update.strokeId` guards it out here
    // exactly as it always did for any other strokeId-less message.
    if (update.windowId !== windowId) return;
    if (!update.strokeId) return;

    const key = strokeKey(update);
    const points = normalizedDrawPoints(update.points);
    const now = performance.now();
    if (update.type === 'text') {
      const anchor = points[0];
      if (!anchor || !update.text) return;
      textAnnotations[key] = {
        annotationId: update.strokeId,
        drawerIdentity: update.drawerIdentity,
        identity: identityFor(update.drawerIdentity, update.drawerPaletteIndex),
        anchor,
        text: update.text,
        lastPointMs: now,
        opacity: 1
      };
      return;
    }
    const existing = strokes[key];
    if (update.type === 'begin') {
      strokes[key] = {
        strokeId: update.strokeId,
        drawerIdentity: update.drawerIdentity,
        identity: identityFor(update.drawerIdentity, update.drawerPaletteIndex),
        points,
        complete: false,
        lastPointMs: now,
        opacity: 1
      };
      return;
    }
    if (!existing) {
      strokes[key] = {
        strokeId: update.strokeId,
        drawerIdentity: update.drawerIdentity,
        identity: identityFor(update.drawerIdentity, update.drawerPaletteIndex),
        points,
        complete: update.type === 'end',
        lastPointMs: now,
        opacity: 1
      };
      return;
    }
    strokes[key] = {
      ...existing,
      identity: identityFor(update.drawerIdentity, update.drawerPaletteIndex),
      points: [...existing.points, ...points],
      complete: existing.complete || update.type === 'end',
      // Restart the age-out clock on every continuation (#670 requirement
      // 4: a stroke still being drawn must not begin fading mid-draw).
      lastPointMs: now,
      opacity: 1
    };
  }

  onMount(() => {
    const onWindowKeyDown = (event: KeyboardEvent) => onDrawKey(event);
    const onWindowKeyUp = (event: KeyboardEvent) => {
      if (drawActive && !event.isComposing && !composing) {
        event.preventDefault();
        event.stopPropagation();
      }
    };
    window.addEventListener('keydown', onWindowKeyDown);
    window.addEventListener('keyup', onWindowKeyUp);
    window.addEventListener('compositionstart', onCompositionStart);
    window.addEventListener('compositionend', onCompositionEnd);
    // Primary delivery path: the native side pushes each update directly into
    // this webview via `webview.eval("window.__petalTelepointer(...)")` (see
    // telepointer.rs). This is used INSTEAD OF Tauri events because the Tauri
    // event bus (emit/emit_to + `listen`) does not reach these nspanel child
    // webviews at all (verified live: rx stayed 0 with both emit_to and global
    // emit). eval-based injection is reliable here.
    (window as typeof window & { __petalTelepointer?: (u: TelepointerUpdate) => void }).__petalTelepointer =
      applyUpdate;
    (window as typeof window & { __petalDraw?: (u: DrawUpdate) => void }).__petalDraw =
      applyDrawUpdate;
    (window as typeof window & { __petalOverlaySetWindowId?: (targetWindowId: number) => void }).__petalOverlaySetWindowId =
      setOverlayWindowId;
    (window as typeof window & { __petalOverlaySetOwnerIdentity?: (targetOwnerIdentity: string) => void }).__petalOverlaySetOwnerIdentity =
      setOverlayOwnerIdentity;
    (window as typeof window & {
      __petalDrawSetToolbarVisible?: (value: boolean) => void;
    }).__petalDrawSetToolbarVisible = (value: boolean) => {
      sharerDrawToolbar = value;
    };
    (window as typeof window & { __petalDrawSetActive?: (value: boolean) => void }).__petalDrawSetActive =
      (value: boolean) => {
        if (!value && drawActive) commitDrawText();
        drawActive = value;
        if (drawActive) {
          drawAnchor = null;
          requestAnimationFrame(() => overlayElement?.focus({ preventScroll: true }));
        } else {
          drawAnchor = null;
          clearDrawInput();
        }
      };
    (window as typeof window & { __petalOverlayClear?: () => void }).__petalOverlayClear =
      clearOverlayState;
    (window as typeof window & { __petalDrawClearWindow?: (targetWindowId: number) => void }).__petalDrawClearWindow =
      (targetWindowId: number) => {
        if (targetWindowId === windowId) {
          strokes = {};
          textAnnotations = {};
        }
      };

    // NOTE: no Tauri-event `listen` fallback here. The native sender pushes
    // each update via `webview.eval("window.__petalTelepointer(...)")` AND
    // also emits a global `telepointer-update`. On macOS the event bus does
    // not reach these nspanel overlays (listen never fires), but on Windows
    // WebView2 it does — so a `listen` here would apply EVERY update twice.
    // With the 60ms CSS position glide and ~30Hz delivery, the double apply
    // re-triggers the transition at double rate and turns any sub-pixel input
    // variance into a continuously oscillating cursor tag. The eval path is
    // the documented deterministic delivery; one source of truth only.

    const interval = setInterval(() => {
      const now = performance.now();
      for (const [userId, p] of Object.entries(pointers)) {
        const age = now - p.lastUpdateMs;
        if (age > STALE_MS) {
          const next = { ...pointers };
          delete next[userId];
          pointers = next;
          clearActivityTimer(userId);
        } else if (age > IDLE_MS && !p.idle) {
          pointers[userId] = { ...p, idle: true };
        }
      }
      // #670: age out drawn strokes 10s after their LAST point (SPEC.md
      // "ephemeral by default"). Same sweep interval as the pointer
      // idle/stale check above, extended rather than adding a second timer.
      let strokesChanged = false;
      const nextStrokes = { ...strokes };
      for (const [key, stroke] of Object.entries(nextStrokes)) {
        const age = now - stroke.lastPointMs;
        if (isStrokeExpired(age)) {
          delete nextStrokes[key];
          strokesChanged = true;
        } else {
          const opacity = strokeFadeOpacity(age);
          if (opacity !== stroke.opacity) {
            nextStrokes[key] = { ...stroke, opacity };
            strokesChanged = true;
          }
        }
      }
      if (strokesChanged) strokes = nextStrokes;

      let textAnnotationsChanged = false;
      const nextTextAnnotations = { ...textAnnotations };
      for (const [key, annotation] of Object.entries(nextTextAnnotations)) {
        const age = now - annotation.lastPointMs;
        if (isStrokeExpired(age)) {
          delete nextTextAnnotations[key];
          textAnnotationsChanged = true;
        } else {
          const opacity = strokeFadeOpacity(age);
          if (opacity !== annotation.opacity) {
            nextTextAnnotations[key] = { ...annotation, opacity };
            textAnnotationsChanged = true;
          }
        }
      }
      if (textAnnotationsChanged) textAnnotations = nextTextAnnotations;
    }, 250);

    return () => {
      clearInterval(interval);
      window.removeEventListener('keydown', onWindowKeyDown);
      window.removeEventListener('keyup', onWindowKeyUp);
      window.removeEventListener('compositionstart', onCompositionStart);
      window.removeEventListener('compositionend', onCompositionEnd);
      activityTimers.forEach((timer) => clearTimeout(timer));
      activityTimers.clear();
      commitDrawText();
      clearDrawInput();
    };
  });
</script>

<div
  bind:this={overlayElement}
  class="overlay"
  class:sharer-input-active={sharerSurface && drawActive}
  style:--draw-cursor={drawCursor}
  role="application"
  aria-label="Draw on your shared window"
  tabindex="-1"
  onpointerdown={onSharerPointerDown}
  onpointermove={onSharerPointerMove}
  onpointerup={onSharerPointerUp}
  onpointercancel={onSharerPointerUp}
>
  {#if shareBorderEnabled}
    <div
      class="sharer-share-border"
      style={`--sharer-share-border-color: ${shareBorderColor}`}
      aria-hidden="true"
    ></div>
  {/if}
  <svg class="draw-layer" viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
    {#each Object.values(strokes).filter((stroke) => stroke.points.length > 1) as stroke (stroke.strokeId)}
      <polyline
        points={polylinePoints(stroke.points)}
        style:stroke="var(--id-{stroke.identity})"
        style:opacity={stroke.opacity * 0.92}
        vector-effect="non-scaling-stroke"
      />
    {/each}
  </svg>
  {#each Object.values(textAnnotations) as annotation (annotation.annotationId)}
    <div
      class="draw-text"
      style={`${textAnnotationStyle(annotation)} color:var(--id-${annotation.identity}); opacity:${annotation.opacity};`}
      aria-hidden="true"
    >{annotation.text}</div>
  {/each}
  {#if sharerSurface && drawActive && sharerDrawToolbar}
    <div
      class="sharer-draw-toolbar"
      role="toolbar"
      aria-label="Drawing controls"
      tabindex="-1"
      onpointerdown={stopToolbarPointer}
      onpointermove={stopToolbarPointer}
      onpointerup={stopToolbarPointer}
      onpointercancel={stopToolbarPointer}
    >
      <span>Drawing on shared display</span>
      <button type="button" onclick={stopSharerDraw}>Stop drawing</button>
    </div>
  {/if}
  {#if sharerSurface && drawActive && drawTextDraft && drawTextAnchor}
    <div
      class="draw-text-draft"
      style={drawTextDraftStyle()}
      aria-hidden="true"
    >{drawTextDraft}</div>
  {/if}
  {#each Object.values(pointers).filter((p) => p.visible) as p (p.userId)}
    <Pointer
      name={p.name}
      identity={p.identity}
      x={p.x}
      y={p.y}
      idle={p.idle}
      pulseKey={p.pulseKey}
      controlActive={p.controlActive}
      typing={p.typing}
    />
  {/each}
  {#each handshakes as burst (burst.id)}
    <div class="handshake" style:left="{burst.x * 100}%" style:top="{burst.y * 100}%">
      <span></span>
      <span></span>
    </div>
  {/each}
</div>

<style>
  :global(html),
  :global(body) {
    background: transparent;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  .overlay {
    position: relative;
    width: 100%;
    height: 100%;
    pointer-events: none;
  }

  .overlay.sharer-input-active {
    pointer-events: auto;
    cursor: var(--draw-cursor, crosshair);
  }

  .sharer-share-border {
    position: absolute;
    inset: 0;
    z-index: 30;
    box-sizing: border-box;
    border: 4px solid var(--sharer-share-border-color);
    border-radius: var(--radius-input);
    pointer-events: none;
  }

  .sharer-draw-toolbar {
    position: absolute;
    z-index: 20;
    top: 12px;
    left: 50%;
    transform: translateX(-50%);
    display: flex;
    align-items: center;
    gap: 10px;
    max-width: calc(100% - 24px);
    padding: 7px 8px 7px 12px;
    border: 1px solid color-mix(in srgb, var(--id-lilac) 56%, var(--hairline-strong));
    border-radius: var(--radius-pill);
    background: var(--surface-raised);
    color: var(--text-primary);
    box-shadow: var(--shadow-float);
    font: 600 12px/1.2 var(--font-ui);
    pointer-events: auto;
    white-space: nowrap;
  }

  .sharer-draw-toolbar button {
    border: 0;
    border-radius: var(--radius-chip);
    padding: 6px 10px;
    background: var(--id-lilac);
    color: var(--bg-base);
    cursor: pointer;
    font: inherit;
  }

  .sharer-draw-toolbar button:focus-visible {
    outline: 2px solid var(--text-primary);
    outline-offset: 2px;
  }

  .draw-layer {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    overflow: visible;
  }

  .draw-layer polyline {
    fill: none;
    stroke-width: 3.4;
    stroke-linecap: round;
    stroke-linejoin: round;
    filter: drop-shadow(0 1px 2px rgba(0, 0, 0, 0.32));
    /* #670: fade out over the sweep interval as `opacity` (set from JS,
       1 -> 0) counts down -- a smooth fade rather than an abrupt pop.
       Baseline opacity is 0.92 (annotation, not full-strength ink); the
       fade multiplies through that inline `opacity` style directly. */
    opacity: 0.92;
    transition: opacity 260ms linear;
  }

  .draw-text,
  .draw-text-draft {
    position: absolute;
    z-index: 1;
    padding: 3px 7px;
    border-radius: var(--radius-chip);
    background: rgba(7, 16, 24, 0.72);
    font: 600 16px/1.2 var(--font-ui);
    white-space: nowrap;
    overflow: visible;
    overflow-wrap: normal;
    pointer-events: none;
    text-shadow: 0 1px 2px rgba(0, 0, 0, 0.42);
    transition: opacity 260ms linear;
  }

  .draw-text-draft {
    color: var(--id-lilac);
    outline: 1px dashed color-mix(in srgb, var(--id-lilac) 72%, transparent);
  }

  .handshake {
    position: absolute;
    width: 58px;
    height: 58px;
    transform: translate(-50%, -50%);
    border-radius: var(--radius-pill);
    background: color-mix(in srgb, var(--id-lilac) 24%, transparent);
    box-shadow:
      0 0 0 1px color-mix(in srgb, var(--id-lilac) 68%, transparent),
      0 0 28px color-mix(in srgb, var(--id-lilac) 55%, transparent);
    animation: handshake-burst 920ms var(--ease-standard) forwards;
  }

  .handshake span {
    position: absolute;
    inset: 16px;
    border-radius: var(--radius-pill);
    background: var(--id-lilac);
    opacity: 0.75;
  }

  .handshake span:nth-child(1) {
    transform: translateX(-9px) rotate(-18deg);
  }

  .handshake span:nth-child(2) {
    transform: translateX(9px) rotate(18deg);
  }

  @keyframes handshake-burst {
    from {
      transform: translate(-50%, -50%) scale(0.54);
      opacity: 0;
    }
    25% {
      opacity: 1;
    }
    to {
      transform: translate(-50%, -50%) scale(1.35);
      opacity: 0;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .handshake {
      animation: none;
      opacity: 0.9;
    }
  }
</style>
