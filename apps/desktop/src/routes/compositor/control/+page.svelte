<!-- Transparent input overlay for remote-window control, draw capture, and
  visible resize handles. The native compositor keeps this child webview
  cursor-interactive so resize handles work in every mode; this route gates
  which input stream it forwards. -->
<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { page } from '$app/state';
  import { invoke } from '@tauri-apps/api/core';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { beginCompositorResizeDrag, type CompositorResizeDirection } from '$lib/compositorResize';
  import {
    normalizedControlPoint,
    remoteClipboardChord,
    type RemoteClipboardOperation
  } from '$lib/data/compositorControl';
  import { identityColorCss } from '$lib/data/identityColor';
  import { platformKey } from '$lib/platform';
  import {
    findRemoteWindowDebugTrack,
    formatDebugMetric,
    formatDebugNumber,
    formatDebugResolution,
    formatFrameCounters,
    formatGlassToGlassLatency,
    formatLastFrameAge,
    formatPacketLossCumulative,
    formatSharedBy
  } from '$lib/data/remoteWindowDebug';
  import { COMMANDS, EVENTS } from '$lib/ipc';
  import type {
    DrawDraft,
    NetworkSnapshot,
    RemoteControlDraft,
    RemoteControlModifiers,
    RemoteWindowDebugStats,
    RemoteControlStatus
  } from '$lib/ipc';
  import { session } from '$lib/stores/session.svelte';
  import {
    remoteControlFeedbackLabel,
    remoteControlFeedbackTitle,
    type RemoteControlFeedbackStatus
  } from '$lib/remoteControlFeedback';
  import {
    applyLocalEchoKey,
    clampLocalEchoAnchor,
    LOCAL_ECHO_RIPPLE_FADE_MS,
    LOCAL_ECHO_TEXT_TIMEOUT_MS,
    nextLocalEchoRippleId,
    type EchoPoint,
    type LocalEchoRipple
  } from '$lib/data/localEcho';

  const windowId = $derived(Number(page.url.searchParams.get('windowId') ?? '0'));
  const ownerIdentity = $derived(page.url.searchParams.get('owner') ?? '');
  let sourceWidth = $state(Number(page.url.searchParams.get('sourceWidth') ?? '0'));
  let sourceHeight = $state(Number(page.url.searchParams.get('sourceHeight') ?? '0'));
  const DRAW_FLUSH_MS = 50;
  const MAX_DRAW_POINTS_PER_MESSAGE = 128;
  const MAX_DRAW_TEXT_CHARS = 256;
  const DRAW_REPOSITION_THRESHOLD_PX = 6;

  type Draft =
    | {
        kind: 'pointer';
        action: 'move' | 'down' | 'up';
      windowId: number;
      targetOwnerId?: string;
      seq: number;
        x: number;
        y: number;
        button: number;
        buttons: number;
        /** #373: authoritative multi-click count (mirrors DOM `detail`). */
        clickCount?: number;
        modifiers: RemoteControlModifiers;
      }
    | {
        kind: 'wheel';
      windowId: number;
      targetOwnerId?: string;
      seq: number;
        x: number;
        y: number;
        deltaX: number;
        deltaY: number;
        deltaMode: 0 | 1 | 2;
        modifiers: RemoteControlModifiers;
      }
    | {
        kind: 'key';
        action: 'down' | 'up';
      windowId: number;
      targetOwnerId?: string;
      seq: number;
        key: string;
        code: string;
        repeat: boolean;
        location: number;
        modifiers: RemoteControlModifiers;
      };

  let active = $state(false);
  // Transient operation feedback (e.g. "Covered"): shown as a prominent,
  // pointer-transparent top-center banner over the controlled video, using a
  // replace-don't-stack 3-second timer. Distinct from the header chip so a
  // refusal is never reduced to an unnoticed icon-only dot at narrow widths.
  let feedbackStatus = $state<RemoteControlFeedbackStatus>(null);
  let feedbackMessage = $state<string | null>(null);
  let feedbackTimer: ReturnType<typeof setTimeout> | undefined;
  const feedbackLabel = $derived(
    feedbackStatus ? remoteControlFeedbackLabel(feedbackStatus) : null
  );
  const feedbackTitle = $derived(
    remoteControlFeedbackTitle(feedbackStatus, feedbackMessage)
  );
  const FEEDBACK_BANNER_MS = 3000;
  // #376/#450: the cue is for native-window focus loss, not DOM focus loss.
  // Keyboard listeners below are window-level so in-document focus changes do
  // not interrupt control; OS-level focus loss still needs the cue.
  let hasFocus = $state(true);
  let controlOverlay: HTMLElement | null = null;
  const showFocusLostCue = $derived(active && !hasFocus);
  let grantToken = $state<string | null>(null);
  type ClipboardModifierState = {
    key: string;
    code: string;
    location: number;
    modifiers: RemoteControlModifiers;
  };
  // Hold the native shortcut modifier locally until the following key is
  // known. This prevents Ctrl/Command itself racing the normalized clipboard
  // operation, while still forwarding ordinary Ctrl/Command shortcuts.
  let pendingClipboardModifier: ClipboardModifierState | null = null;
  let clipboardModifierConsumed = false;
  let clipboardShortcutCode: string | null = null;
  let pointerId = $state<number | null>(null);
  let seq = 0;
  let pointerMoveFrame = 0;
  let pendingPointerMove: PendingPointerMove | null = null;
  let wheelFrame = 0;
  let pendingWheel: PendingWheel | null = null;
  let lastSentMoveCoordinate: string | null = null;
  // #373: belt-and-suspenders alongside `KeyboardEvent.isComposing` -- some
  // engines don't reliably set `isComposing` on every keydown/keyup of a
  // composing sequence, so this tracks compositionstart/compositionend
  // explicitly too.
  let composing = false;
  let debugOpen = $state(false);
  let debugStats = $state<RemoteWindowDebugStats | null>(null);
  let debugSnapshot = $state<NetworkSnapshot | null>(null);
  let debugError = $state<string | null>(null);
  let debugTimer: ReturnType<typeof setInterval> | undefined;
  let drawActive = $state(false);
  let drawPointerId = $state<number | null>(null);
  let drawStrokeId = $state<string | null>(null);
  let drawSeq = 0;
  let drawFlushTimer: ReturnType<typeof setTimeout> | undefined;
  let pendingDrawPoints: Point[] = [];
  let drawAnchor = $state<Point | null>(null);
  let drawTextAnchor = $state<Point | null>(null);
  let drawTextDraft = $state('');
  let drawTextStrokeId = $state<string | null>(null);
  const drawCursor = $derived(penCursor(identityColorCss(session.identity)));

  // Refs #378: local echo (opt-in, default OFF -- session.localEchoEnabled).
  // Purely local rendering, zero wire changes: Phase 1 gesture echo (click
  // ripples + keypress flash pulses) and Phase 2 optimistic text echo (a
  // translucent "pending" strip for typed characters). See
  // $lib/data/localEcho.ts for the shared logic + the truth-over-appearance
  // rationale.
  const localEchoEnabled = $derived(session.localEchoEnabled);
  let echoRipples = $state<LocalEchoRipple[]>([]);
  let echoRippleSeq = 0;
  let echoKeyFlashes = $state<{ id: number }[]>([]);
  let echoKeyFlashSeq = 0;
  let echoPendingText = $state('');
  let echoAnchor = $state<EchoPoint | null>(null);
  let echoLastClickPoint: EchoPoint | null = null;
  let echoTextTimer: ReturnType<typeof setTimeout> | undefined;

  function spawnEchoRipple(clientX: number, clientY: number, target: HTMLElement): EchoPoint {
    const rect = target.getBoundingClientRect();
    echoRippleSeq = nextLocalEchoRippleId(echoRippleSeq);
    const id = echoRippleSeq;
    const point = { x: clientX - rect.left, y: clientY - rect.top };
    echoRipples = [...echoRipples, { id, ...point }];
    setTimeout(() => {
      echoRipples = echoRipples.filter((ripple) => ripple.id !== id);
    }, LOCAL_ECHO_RIPPLE_FADE_MS);
    return point;
  }

  function spawnEchoKeyFlash() {
    echoKeyFlashSeq = nextLocalEchoRippleId(echoKeyFlashSeq);
    const id = echoKeyFlashSeq;
    echoKeyFlashes = [...echoKeyFlashes, { id }];
    setTimeout(() => {
      echoKeyFlashes = echoKeyFlashes.filter((flash) => flash.id !== id);
    }, LOCAL_ECHO_RIPPLE_FADE_MS);
  }

  function clearEchoText() {
    echoPendingText = '';
    echoAnchor = null;
    if (echoTextTimer) {
      clearTimeout(echoTextTimer);
      echoTextTimer = undefined;
    }
  }

  function scheduleEchoTextClear() {
    if (echoTextTimer) clearTimeout(echoTextTimer);
    echoTextTimer = setTimeout(() => {
      echoTextTimer = undefined;
      echoPendingText = '';
      echoAnchor = null;
    }, LOCAL_ECHO_TEXT_TIMEOUT_MS);
  }

  function handleEchoKeydown(event: KeyboardEvent, target: HTMLElement) {
    spawnEchoKeyFlash();
    const nextPending = applyLocalEchoKey(echoPendingText, event);
    if (nextPending === null) return;
    echoPendingText = nextPending;
    if (!echoPendingText) {
      clearEchoText();
      return;
    }
    if (!echoAnchor) {
      const rect = target.getBoundingClientRect();
      echoAnchor = clampLocalEchoAnchor(
        echoLastClickPoint ?? { x: rect.width / 2, y: rect.height * 0.6 },
        { width: rect.width, height: rect.height }
      );
    }
    scheduleEchoTextClear();
  }

  function clearLocalEcho() {
    clearEchoText();
    echoRipples = [];
    echoKeyFlashes = [];
    echoLastClickPoint = null;
  }

  type Point = { x: number; y: number };

  type ControlWindow = typeof window & {
    __petalRemoteControlSetActive?: (value: boolean) => void;
    __petalRemoteControlSourceDimensions?: (width: number, height: number) => void;
    __petalPendingRemoteControlActive?: boolean;
    __petalPendingControlSourceDimensions?: { width: number; height: number };
    __petalDrawSetActive?: (value: boolean) => void;
    __petalDebugToggle?: () => void;
  };

  type PointerInput = {
    button: number;
    buttons: number;
    /** #373: authoritative multi-click count (mirrors DOM `detail`). */
    clickCount?: number;
    modifiers: RemoteControlModifiers;
  };

  type PendingPointerMove = Point &
    PointerInput & {
      windowId: number;
    };

  type PendingWheel = Point & {
    windowId: number;
    deltaX: number;
    deltaY: number;
    deltaMode: 0 | 1 | 2;
    modifiers: RemoteControlModifiers;
  };

  function nextSeq() {
    seq = seq >= Number.MAX_SAFE_INTEGER ? 1 : seq + 1;
    return seq;
  }

  function modifiers(event: MouseEvent | PointerEvent | WheelEvent | KeyboardEvent): RemoteControlModifiers {
    return {
      alt: event.altKey,
      ctrl: event.ctrlKey,
      meta: event.metaKey,
      shift: event.shiftKey
    };
  }

  function normalizedPoint(event: PointerEvent | WheelEvent): { x: number; y: number } | null {
    const target = event.currentTarget as HTMLElement;
    const rect = target.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return null;
    return normalizedControlPoint(
      { left: rect.left, top: rect.top, width: rect.width, height: rect.height },
      { width: sourceWidth, height: sourceHeight },
      { x: event.clientX, y: event.clientY }
    );
  }

  function setSourceDimensions(width: number, height: number) {
    if (!Number.isFinite(width) || !Number.isFinite(height) || width < 0 || height < 0) return;
    sourceWidth = width;
    sourceHeight = height;
  }

  function send(draft: Draft | RemoteControlDraft) {
    invoke(COMMANDS.remoteControlSend, { draft }).catch(() => {});
  }

  function isClipboardModifier(event: KeyboardEvent): boolean {
    const platform = platformKey();
    return (
      (platform === 'windows' && event.key === 'Control') ||
      (platform === 'macos' && event.key === 'Meta')
    );
  }

  function sendKey(event: KeyboardEvent, action: 'down' | 'up') {
    send({
      kind: 'key',
      action,
      windowId,
      targetOwnerId: ownerIdentity,
      seq: nextSeq(),
      grantToken: grantToken ?? undefined,
      key: event.key,
      code: event.code,
      repeat: action === 'down' ? event.repeat : false,
      location: event.location,
      modifiers: modifiers(event)
    });
  }

  function flushPendingClipboardModifier() {
    const pending = pendingClipboardModifier;
    if (!pending) return;
    pendingClipboardModifier = null;
    send({
      kind: 'key',
      action: 'down',
      windowId,
      targetOwnerId: ownerIdentity,
      seq: nextSeq(),
      grantToken: grantToken ?? undefined,
      key: pending.key,
      code: pending.code,
      repeat: false,
      location: pending.location,
      modifiers: pending.modifiers
    });
  }

  function clearClipboardShortcutState() {
    pendingClipboardModifier = null;
    clipboardModifierConsumed = false;
    clipboardShortcutCode = null;
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

  function sendDraw(draft: DrawDraft) {
    invoke(COMMANDS.drawSend, { draft }).catch(() => {});
  }

  function sendDrawMessage(type: DrawDraft['type'], strokeId: string, points: Point[] = [], text?: string) {
    sendDraw({
      type,
      windowId,
      ownerIdentity,
      strokeId,
      seq: nextDrawSeq(),
      points,
      ...(text === undefined ? {} : { text })
    });
  }

  function drawPointMoved(a: Point, b: Point) {
    return Math.hypot(
      (a.x - b.x) * Math.max(1, window.innerWidth),
      (a.y - b.y) * Math.max(1, window.innerHeight)
    ) >= DRAW_REPOSITION_THRESHOLD_PX;
  }

  function drawTextDraftStyle() {
    if (!drawTextAnchor) return '';
    const alignRight = drawTextAnchor.x > 0.62;
    const available = Math.max(0.04, alignRight ? drawTextAnchor.x : 1 - drawTextAnchor.x);
    const estimatedTextWidth = Math.max(0.01, [...drawTextDraft].length * 0.0085 + 0.025);
    const horizontalScale = Math.min(1, available / estimatedTextWidth);
    return `left:${drawTextAnchor.x * 100}%; top:${drawTextAnchor.y * 100}%; transform:translate(${alignRight ? '-100%' : '0'}, -50%) scaleX(${horizontalScale}); transform-origin:${alignRight ? 'right' : 'left'} center;`;
  }

  function clearDrawTextDraft() {
    drawTextAnchor = null;
    drawTextDraft = '';
    drawTextStrokeId = null;
  }

  function commitDrawText() {
    if (drawTextAnchor && drawTextDraft.trim() && drawTextStrokeId) {
      sendDrawMessage('text', drawTextStrokeId, [drawTextAnchor], drawTextDraft);
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
    drawTextDraft = [...next].filter((character) => !/\n|\r|\u2028|\u2029/u.test(character)).slice(0, MAX_DRAW_TEXT_CHARS).join('');
  }

  function onDrawKey(event: KeyboardEvent, action: 'down' | 'up') {
    if (event.isComposing || composing) return;
    event.preventDefault();
    event.stopPropagation();
    if (action !== 'down') return;
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

  function sendPointerDraft(action: 'move' | 'down' | 'up', point: Point, input: PointerInput) {
    send({
      kind: 'pointer',
      action,
      windowId,
      targetOwnerId: ownerIdentity,
      seq: nextSeq(),
      grantToken: grantToken ?? undefined,
      x: point.x,
      y: point.y,
      button: input.button,
      buttons: input.buttons,
      ...(input.clickCount !== undefined ? { clickCount: input.clickCount } : {}),
      modifiers: input.modifiers
    });
  }

  function sendPointer(event: PointerEvent, action: 'move' | 'down' | 'up') {
    const point = normalizedPoint(event);
    if (!point || !active) return;
    sendPointerDraft(action, point, {
      button: event.button,
      buttons: action === 'up' ? 0 : event.buttons,
      // #373: `detail` is the DOM's own multi-click counter (1 = single,
      // 2 = double, ...), authoritative for the down that starts a gesture.
      // Only meaningful for down/up; move never carries one.
      clickCount: action === 'move' ? undefined : Math.max(1, event.detail || 1),
      modifiers: modifiers(event)
    });
  }

  function schedulePointerMoveFlush() {
    if (pointerMoveFrame) return;
    pointerMoveFrame = requestAnimationFrame(() => {
      pointerMoveFrame = 0;
      flushPendingPointerMove();
    });
  }

  function scheduleWheelFlush() {
    if (wheelFrame) return;
    wheelFrame = requestAnimationFrame(() => {
      wheelFrame = 0;
      flushPendingWheel();
    });
  }

  function clearPendingInput() {
    clearClipboardShortcutState();
    pendingPointerMove = null;
    pendingWheel = null;
    lastSentMoveCoordinate = null;
    if (pointerMoveFrame) {
      cancelAnimationFrame(pointerMoveFrame);
      pointerMoveFrame = 0;
    }
    if (wheelFrame) {
      cancelAnimationFrame(wheelFrame);
      wheelFrame = 0;
    }
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

  function flushPendingPointerMove() {
    const pending = pendingPointerMove;
    pendingPointerMove = null;
    if (!pending || !active || pending.windowId !== windowId) return;
    const quantize = (value: number) => Math.round(Math.min(1, Math.max(0, value)) * 0xffff);
    const coordinate = `${quantize(pending.x)}:${quantize(pending.y)}`;
    if (coordinate === lastSentMoveCoordinate) return;
    lastSentMoveCoordinate = coordinate;
    sendPointerDraft('move', pending, {
      button: pending.button,
      buttons: pending.buttons,
      modifiers: pending.modifiers
    });
  }

  function flushPendingWheel() {
    const pending = pendingWheel;
    pendingWheel = null;
    if (!pending || !active || pending.windowId !== windowId) return;
    send({
      kind: 'wheel',
      windowId,
      targetOwnerId: ownerIdentity,
      seq: nextSeq(),
      grantToken: grantToken ?? undefined,
      x: pending.x,
      y: pending.y,
      deltaX: pending.deltaX,
      deltaY: pending.deltaY,
      deltaMode: pending.deltaMode,
      modifiers: pending.modifiers
    });
  }

  function onPointerDown(event: PointerEvent) {
    // #678: a real click anywhere in this window (View, remote-control, or
    // draw mode) must raise the remote window's panel -- so this runs BEFORE
    // the drawActive/!active mode branches below, unconditionally, for every
    // real click. Left-button only (#450: never on hover, which could steal
    // focus from another app behind this window); pointerdown only, not
    // pointerenter/pointermove.
    //
    // keyControlChild is passed as exactly `active` (remote-control mode),
    // matching the old #450 compositorFocusControl gating precisely -- NOT
    // `active || drawActive`. Keying the control overlay activates the whole
    // app (WebviewWindow::set_focus calls activateIgnoringOtherApps: YES
    // internally), so this must stay narrow: a plain View-mode click raises
    // the panel only, with no app activation at all, and draw mode never
    // needed the overlay keyed even before this change.
    if (event.button === 0) {
      void invoke(COMMANDS.compositorRaiseWindowForClick, {
        windowId,
        ownerIdentity,
        keyControlChild: active
      }).catch(() => {});
    }
    if (drawActive) {
      const point = normalizedPoint(event);
      if (!point) return;
      commitDrawText();
      drawAnchor = point;
      event.preventDefault();
      drawPointerId = event.pointerId;
      drawStrokeId = createStrokeId();
      (event.currentTarget as HTMLElement).focus({ preventScroll: true });
      try {
        (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
      } catch {
        // Pointer capture is best effort for synthetic/non-primary events.
      }
      sendDrawMessage('begin', drawStrokeId, [point]);
      return;
    }
    if (!active) return;
    event.preventDefault();
    pointerId = event.pointerId;
    (event.currentTarget as HTMLElement).focus({ preventScroll: true });
    // The raise-for-click call above already re-keys this overlay window
    // atomically as part of its main-thread raise sequence (#678), so a
    // separate compositorFocusControl call here would be redundant -- it
    // used to be the only re-key path (#450) before #678 folded it in.
    try {
      (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    } catch {
      // Pointer capture is best effort for synthetic/non-primary events.
    }
    if (localEchoEnabled) {
      echoLastClickPoint = spawnEchoRipple(event.clientX, event.clientY, event.currentTarget as HTMLElement);
    }
    sendPointer(event, 'down');
  }

  function onPointerMove(event: PointerEvent) {
    if (drawActive) {
      if (drawPointerId !== null && drawPointerId !== event.pointerId) return;
      const point = normalizedPoint(event);
      if (!point) return;
      if (drawAnchor && drawPointMoved(drawAnchor, point)) commitDrawText();
      drawAnchor = point;
      if (!drawStrokeId) return;
      event.preventDefault();
      pendingDrawPoints = [...pendingDrawPoints, point];
      scheduleDrawFlush();
      return;
    }
    if (!active) return;
    if (pointerId !== null && pointerId !== event.pointerId) return;
    const point = normalizedPoint(event);
    if (!point) return;
    event.preventDefault();
    pendingPointerMove = {
      windowId,
      ...point,
      button: event.button,
      buttons: event.buttons,
      modifiers: modifiers(event)
    };
    schedulePointerMoveFlush();
  }

  function onPointerUp(event: PointerEvent) {
    if (drawActive) {
      if (drawPointerId !== null && drawPointerId !== event.pointerId) return;
      event.preventDefault();
      flushPendingDrawPoints();
      const point = normalizedPoint(event);
      const strokeId = drawStrokeId;
      if (strokeId) sendDrawMessage('end', strokeId, point ? [point] : []);
      try {
        (event.currentTarget as HTMLElement).releasePointerCapture(event.pointerId);
      } catch {
        // Safe to ignore: pointer capture may already be gone.
      }
      clearDrawInput();
      return;
    }
    if (!active) return;
    if (pointerId !== null && pointerId !== event.pointerId) return;
    event.preventDefault();
    flushPendingPointerMove();
    sendPointer(event, 'up');
    try {
      (event.currentTarget as HTMLElement).releasePointerCapture(event.pointerId);
    } catch {
      // Safe to ignore: pointer capture may already be gone.
    }
    pointerId = null;
  }

  function onWheel(event: WheelEvent) {
    if (drawActive) return;
    if (!active) return;
    const point = normalizedPoint(event);
    if (!point) return;
    event.preventDefault();
    event.stopPropagation();
    const deltaMode = event.deltaMode === 1 || event.deltaMode === 2 ? event.deltaMode : 0;
    if (pendingWheel && (pendingWheel.windowId !== windowId || pendingWheel.deltaMode !== deltaMode)) {
      flushPendingWheel();
    }
    pendingWheel = {
      windowId,
      ...point,
      deltaX: (pendingWheel?.deltaX ?? 0) + event.deltaX,
      deltaY: (pendingWheel?.deltaY ?? 0) + event.deltaY,
      deltaMode,
      modifiers: modifiers(event)
    };
    // Throttled to once per animation-frame batch (matches the existing
    // rAF-coalesced send cadence below) so a fast scroll doesn't flood the
    // overlay with ripples.
    if (localEchoEnabled && !wheelFrame) {
      spawnEchoRipple(event.clientX, event.clientY, event.currentTarget as HTMLElement);
    }
    scheduleWheelFlush();
  }

  function onKey(event: KeyboardEvent, action: 'down' | 'up') {
    if (drawActive) {
      onDrawKey(event, action);
      return;
    }
    // #373: suppress per-keystroke relay while an IME composition is in
    // progress (CJK, dead keys, emoji picker) -- the composed result is
    // sent once as a `text` message on compositionend instead. Deliberately
    // do NOT preventDefault/stopPropagation here so the browser's own
    // composition UI (candidate window) keeps working normally.
    if (event.isComposing || composing) return;
    if (!active) return;

    // Do not send Ctrl/Command down until we know whether the next key is a
    // native clipboard shortcut. Sending it immediately can race the
    // clipboard command's synthesized chord on the host and leave the
    // shortcut with no modifier. Ordinary shortcuts flush it before their
    // first non-modifier key is forwarded.
    if (isClipboardModifier(event)) {
      event.preventDefault();
      event.stopPropagation();
      if (action === 'down') {
        if (!event.repeat) {
          pendingClipboardModifier = {
            key: event.key,
            code: event.code,
            location: event.location,
            modifiers: modifiers(event)
          };
        }
      } else if (clipboardModifierConsumed) {
        clipboardModifierConsumed = false;
        pendingClipboardModifier = null;
      } else {
        flushPendingClipboardModifier();
        sendKey(event, action);
      }
      return;
    }

    const clipboardOperation = remoteClipboardChord(event);
    if (clipboardOperation) {
      // Native clipboard shortcuts are a cross-system operation, not ordinary
      // key input. Consume both halves so the host cannot apply the same chord
      // a second time; only the first non-repeat keydown starts the operation.
      event.preventDefault();
      event.stopPropagation();
      if (action === 'down' && !event.repeat) {
        pendingClipboardModifier = null;
        clipboardModifierConsumed = true;
        clipboardShortcutCode = event.code;
        flushPendingPointerMove();
        flushPendingWheel();
        invokeClipboardOperation(clipboardOperation);
      } else if (action === 'up' && clipboardShortcutCode === event.code) {
        clipboardShortcutCode = null;
      }
      return;
    }

    event.preventDefault();
    event.stopPropagation();
    flushPendingClipboardModifier();
    flushPendingPointerMove();
    flushPendingWheel();
    if (localEchoEnabled && action === 'down') {
      if (controlOverlay) handleEchoKeydown(event, controlOverlay);
    }
    sendKey(event, action);
  }

  function invokeClipboardOperation(operation: RemoteClipboardOperation) {
    if (!active) return;
    const command =
      operation === 'copy' ? COMMANDS.remoteClipboardCopy : COMMANDS.remoteClipboardPaste;
    void invoke(command, {
      windowId,
      ownerIdentity,
      grantToken: grantToken ?? undefined
    }).catch(() => {});
  }

  // #373: the composed result of an IME sequence (CJK, dead keys, emoji
  // picker) is sent once as the existing `text` wire message -- already
  // injectable host-side via `replay_text` -- instead of the raw per-key
  // events onKey suppressed during composition. Desktop text chunking for
  // an oversized commit is handled by the Rust `remote_control_send`
  // command itself (`remote_text_chunks`), so no client-side chunking is
  // needed here (contrast web-harness, which publishes directly).
  function sendComposedText(text: string) {
    if (!active || !text) return;
    send({
      kind: 'text',
      windowId,
      targetOwnerId: ownerIdentity,
      seq: nextSeq(),
      grantToken: grantToken ?? undefined,
      text,
      modifiers: { alt: false, ctrl: false, meta: false, shift: false }
    });
  }

  function onCompositionStart() {
    composing = true;
  }

  function onCompositionEnd(event: CompositionEvent) {
    composing = false;
    if (drawActive) {
      appendDrawText(event.data ?? '');
      return;
    }
    if (!active) return;
    flushPendingPointerMove();
    flushPendingWheel();
    sendComposedText(event.data ?? '');
  }

  function onResizePointerDown(event: PointerEvent, direction: CompositorResizeDirection) {
    void beginCompositorResizeDrag(event, windowId, ownerIdentity, direction);
  }

  function flushPendingDrawPoints() {
    if (drawFlushTimer) {
      clearTimeout(drawFlushTimer);
      drawFlushTimer = undefined;
    }
    const strokeId = drawStrokeId;
    const points = pendingDrawPoints;
    pendingDrawPoints = [];
    if (!drawActive || !strokeId || points.length === 0) return;
    for (let index = 0; index < points.length; index += MAX_DRAW_POINTS_PER_MESSAGE) {
      sendDrawMessage('points', strokeId, points.slice(index, index + MAX_DRAW_POINTS_PER_MESSAGE));
    }
  }

  function scheduleDrawFlush() {
    if (drawFlushTimer) return;
    drawFlushTimer = setTimeout(() => {
      drawFlushTimer = undefined;
      flushPendingDrawPoints();
    }, DRAW_FLUSH_MS);
  }

  const debugTrack = $derived(findRemoteWindowDebugTrack(debugSnapshot, ownerIdentity, windowId));
  const debugLastFrame = $derived(formatLastFrameAge(debugStats?.lastFrameReceivedMs));
  const debugLastEnqueued = $derived(formatLastFrameAge(debugStats?.lastDisplayEnqueuedMs));
  const debugLatency = $derived(formatGlassToGlassLatency(debugTrack));

  async function refreshDebugPanel() {
    if (!debugOpen) return;
    try {
      const [stats, snapshot] = await Promise.all([
        invoke<RemoteWindowDebugStats>(COMMANDS.compositorWindowDebugStats, { windowId, ownerIdentity }),
        invoke<NetworkSnapshot>(COMMANDS.getNetworkSnapshot)
      ]);
      debugStats = stats;
      debugSnapshot = snapshot;
      debugError = null;
    } catch {
      debugError = 'Debug stats unavailable';
    }
  }

  function startDebugPolling() {
    clearInterval(debugTimer);
    debugTimer = setInterval(() => void refreshDebugPanel(), 1000);
    void refreshDebugPanel();
  }

  function stopDebugPolling() {
    clearInterval(debugTimer);
    debugTimer = undefined;
  }

  function setDebugOpen(open: boolean) {
    debugOpen = open;
    if (debugOpen) startDebugPolling();
    else stopDebugPolling();
  }

  function onDebugPanelPointer(event: Event) {
    event.stopPropagation();
  }

  // Prominent transient operation-feedback banner. Lifecycle statuses
  // (active/stopped) clear it; transient refusals (occluded, integrityBlocked,
  // secureField, …) replace any current banner and restart one 3-second
  // timer — never stacking DOM elements or global toasts.
  function showFeedback(status: RemoteControlStatus) {
    if (status.status === 'active' || status.status === 'stopped') {
      clearFeedback();
      return;
    }
    feedbackStatus = status.status;
    feedbackMessage = status.message ?? null;
    if (feedbackTimer) clearTimeout(feedbackTimer);
    feedbackTimer = setTimeout(() => {
      feedbackTimer = undefined;
      feedbackStatus = null;
      feedbackMessage = null;
    }, FEEDBACK_BANNER_MS);
  }

  function clearFeedback() {
    if (feedbackTimer) {
      clearTimeout(feedbackTimer);
      feedbackTimer = undefined;
    }
    feedbackStatus = null;
    feedbackMessage = null;
  }

  onMount(() => {
    const controlWindow = window as ControlWindow;
    controlOverlay = document.querySelector<HTMLElement>('.control-overlay');
    hasFocus = document.hasFocus();
    const onWindowKeyDown = (event: KeyboardEvent) => onKey(event, 'down');
    const onWindowKeyUp = (event: KeyboardEvent) => onKey(event, 'up');
    const onWindowCompositionStart = () => onCompositionStart();
    const onWindowCompositionEnd = (event: CompositionEvent) => onCompositionEnd(event);
    const onWindowFocus = () => (hasFocus = true);
    const onWindowBlur = () => (hasFocus = false);
    // Bubble phase is intentional: the debug panel stops propagation so its
    // close button/read-only diagnostics never enter the control channel.
    window.addEventListener('keydown', onWindowKeyDown);
    window.addEventListener('keyup', onWindowKeyUp);
    window.addEventListener('compositionstart', onWindowCompositionStart);
    window.addEventListener('compositionend', onWindowCompositionEnd);
    window.addEventListener('focus', onWindowFocus);
    window.addEventListener('blur', onWindowBlur);
    let unlistenRemoteControlStatus: UnlistenFn | undefined;
    controlWindow.__petalRemoteControlSetActive = (value: boolean) => {
        active = value;
        if (!active) grantToken = null;
        if (!active) clearPendingInput();
        if (!active) clearLocalEcho();
        if (active && drawActive) {
          commitDrawText();
          drawActive = false;
          clearDrawInput();
        }
        if (active) requestAnimationFrame(() => document.querySelector<HTMLElement>('.control-overlay')?.focus());
      };
    controlWindow.__petalRemoteControlSourceDimensions = setSourceDimensions;
    if (controlWindow.__petalPendingControlSourceDimensions) {
      const { width, height } = controlWindow.__petalPendingControlSourceDimensions;
      setSourceDimensions(width, height);
    }
    if (typeof controlWindow.__petalPendingRemoteControlActive === 'boolean') {
      controlWindow.__petalRemoteControlSetActive(controlWindow.__petalPendingRemoteControlActive);
    }
    controlWindow.__petalDrawSetActive = (value: boolean) => {
      if (!value && drawActive) commitDrawText();
      drawActive = value;
      if (drawActive) {
        active = false;
        drawAnchor = null;
        clearPendingInput();
        requestAnimationFrame(() => document.querySelector<HTMLElement>('.control-overlay')?.focus());
      } else {
        drawAnchor = null;
        clearDrawInput();
      }
    };
    controlWindow.__petalDebugToggle = () => {
      setDebugOpen(!debugOpen);
    };
    listen<RemoteControlStatus>(EVENTS.remoteControlStatus, (event) => {
      if (event.payload.windowId !== windowId) return;
      // Note (Fable F3): the host's "active" status carries ownerIdentity:
      // null (remote_control.rs), so this guard is effectively a no-op for
      // the grant-token-bearing message and filters only by windowId below.
      // That is safe here ONLY because this event comes from the local Tauri
      // event bus (the host's own process), not a peer-writable wire path —
      // unlike the web-harness controller, which does authenticate the
      // sender against the LiveKit-verified identity (see #377 review).
      if (ownerIdentity && event.payload.ownerIdentity && event.payload.ownerIdentity !== ownerIdentity) return;
      if (event.payload.status === 'active' && event.payload.grantToken) {
        grantToken = event.payload.grantToken;
      } else if (event.payload.status === 'stopped') {
        grantToken = null;
      }
      showFeedback(event.payload);
    })
      .then((unlisten) => {
        unlistenRemoteControlStatus = unlisten;
      })
      .catch(() => {});
    return () => {
      window.removeEventListener('keydown', onWindowKeyDown);
      window.removeEventListener('keyup', onWindowKeyUp);
      window.removeEventListener('compositionstart', onWindowCompositionStart);
      window.removeEventListener('compositionend', onWindowCompositionEnd);
      window.removeEventListener('focus', onWindowFocus);
      window.removeEventListener('blur', onWindowBlur);
      controlOverlay = null;
      unlistenRemoteControlStatus?.();
    };
  });

  onDestroy(() => {
    commitDrawText();
    clearPendingInput();
    grantToken = null;
    clearDrawInput();
    clearLocalEcho();
    clearFeedback();
    stopDebugPolling();
  });
</script>

<div class="control-shell">
  <button
    type="button"
    class="control-overlay"
    class:active
    class:draw-active={drawActive}
    style:--draw-cursor={drawCursor}
    aria-label="Remote control input"
    onpointerdown={onPointerDown}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
    onwheel={onWheel}
  ></button>
  {#if drawActive && drawTextDraft && drawTextAnchor}
    <div
      class="draw-text-draft"
      style={drawTextDraftStyle()}
      aria-hidden="true"
    >{drawTextDraft}</div>
  {/if}
  {#if showFocusLostCue}
    <div class="focus-lost-hint" role="status" aria-live="polite">Click to resume control</div>
  {/if}
  {#if feedbackLabel}
    <div
      class="control-feedback-banner"
      class:warning={feedbackStatus !== null && feedbackStatus !== 'targetPaused' && feedbackStatus !== 'textTruncated'}
      role="status"
      aria-live="assertive"
      title={feedbackTitle ?? undefined}
    >
      <span class="control-feedback-dot" aria-hidden="true"></span>
      <span class="control-feedback-text">{feedbackLabel}</span>
      {#if feedbackTitle}
        <span class="control-feedback-detail">{feedbackTitle}</span>
      {/if}
    </div>
  {/if}
  <div class="resize-zones">
    <button type="button" tabindex="-1" aria-label="Resize east" class="resize-zone resize-e" onpointerdown={(event) => onResizePointerDown(event, 'East')}></button>
    <button type="button" tabindex="-1" aria-label="Resize south" class="resize-zone resize-s" onpointerdown={(event) => onResizePointerDown(event, 'South')}></button>
    <button type="button" tabindex="-1" aria-label="Resize west" class="resize-zone resize-w" onpointerdown={(event) => onResizePointerDown(event, 'West')}></button>
    <button type="button" tabindex="-1" aria-label="Resize south east" class="resize-zone resize-se" onpointerdown={(event) => onResizePointerDown(event, 'SouthEast')}></button>
    <button type="button" tabindex="-1" aria-label="Resize south west" class="resize-zone resize-sw" onpointerdown={(event) => onResizePointerDown(event, 'SouthWest')}></button>
  </div>
  {#if localEchoEnabled}
    <!-- Refs #378: local echo -- purely local, ephemeral "input sent"
      feedback. Never draws anything meant to look like real shared-app
      content; the text strip is explicitly labeled "unconfirmed". -->
    <div class="local-echo-layer" aria-hidden="true">
      {#each echoRipples as ripple (ripple.id)}
        <span class="local-echo-ripple" style={`left:${ripple.x}px; top:${ripple.y}px;`}></span>
      {/each}
      {#each echoKeyFlashes as flash (flash.id)}
        <span class="local-echo-key-flash"></span>
      {/each}
      {#if echoPendingText}
        <div
          class="local-echo-text"
          style={echoAnchor ? `left:${echoAnchor.x}px; top:${echoAnchor.y}px;` : ''}
        >
          <span class="local-echo-text-chars">{echoPendingText}</span>
          <span class="local-echo-text-badge">sent, unconfirmed</span>
        </div>
      {/if}
    </div>
  {/if}
  {#if debugOpen}
    <div
      class="debug-panel"
      role="dialog"
      aria-label="Remote window debug stats"
      tabindex="-1"
      onpointerdown={onDebugPanelPointer}
      onpointermove={onDebugPanelPointer}
      onwheel={onDebugPanelPointer}
      onkeydown={onDebugPanelPointer}
    >
      <div class="debug-head">
        <span>Debug</span>
        <button type="button" class="debug-close" aria-label="Close debug panel" onclick={() => setDebugOpen(false)}>&times;</button>
      </div>
      {#if debugError}
        <p class="debug-error">{debugError}</p>
      {:else}
        <dl>
          <div><dt>Shared by</dt><dd>{formatSharedBy(debugStats?.ownerDisplayName, debugStats?.ownerIdentity ?? ownerIdentity)}</dd></div>
          <div><dt>Source</dt><dd>{debugStats?.sourceTitle ?? 'Shared window'}</dd></div>
          <div><dt>Window ID</dt><dd>{windowId}</dd></div>
          <div><dt>Track</dt><dd>{debugTrack?.rawTrackName ?? 'track pending'}</dd></div>
          <div><dt>FPS received</dt><dd>{formatDebugNumber(debugTrack?.fps, 1)}</dd></div>
          <div><dt>Resolution</dt><dd>{formatDebugResolution(debugTrack)}</dd></div>
          <div><dt>Source pixels</dt><dd>{debugStats?.sourcePixelWidth && debugStats?.sourcePixelHeight ? `${debugStats.sourcePixelWidth} x ${debugStats.sourcePixelHeight}` : 'n/a'}</dd></div>
          <div><dt>Display request</dt><dd>{debugStats ? `${debugStats.displayPixelWidth} x ${debugStats.displayPixelHeight} @ ${debugStats.receiverScale.toFixed(2)}x` : 'n/a'}</dd></div>
          <div><dt>Decoder</dt><dd>{debugTrack?.codecImpl || 'n/a'}</dd></div>
          <div><dt>Bitrate recv</dt><dd>{formatDebugMetric(debugTrack?.actualKbps, 0, 'kbps')}</dd></div>
          <div><dt>Packet loss</dt><dd>{formatPacketLossCumulative(debugTrack)}</dd></div>
          <div><dt>Frames</dt><dd>{formatFrameCounters(debugTrack, debugStats?.framesReceived)}</dd></div>
          <div><dt>Frames enqueued</dt><dd>{formatDebugMetric(debugStats?.framesDisplayEnqueued, 0, 'display layer')}</dd></div>
          <div><dt>Jitter buffer</dt><dd>{formatDebugMetric(debugTrack?.jitterBufferMs, 1, 'ms cumulative avg')}</dd></div>
          <div><dt>G2G latency</dt><dd>{debugLatency.label}</dd></div>
          <div class="debug-note">{debugLatency.caveat}</div>
          <div><dt>Last frame</dt><dd class:stale={debugLastFrame.stale}>{debugLastFrame.label}</dd></div>
          <div><dt>Last enqueued</dt><dd class:stale={debugLastEnqueued.stale}>{debugLastEnqueued.label}</dd></div>
          <div><dt>Stream</dt><dd>{debugTrack?.streamState ?? 'unknown'}</dd></div>
        </dl>
      {/if}
    </div>
  {/if}
</div>

<style>
  :global(html),
  :global(body) {
    background: transparent;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  .control-shell,
  .control-overlay {
    width: 100vw;
    height: 100vh;
  }

  .control-shell {
    position: relative;
  }

  .control-overlay {
    position: absolute;
    inset: 0;
    display: block;
    padding: 0;
    border: 0;
    background: transparent;
    outline: none;
    cursor: default;
  }

  .control-overlay.active {
    cursor: default;
  }

  .control-overlay.draw-active {
    cursor: var(--draw-cursor, crosshair);
  }

  .draw-text-draft {
    position: absolute;
    z-index: 1;
    padding: 3px 7px;
    border-radius: var(--radius-chip);
    background: rgba(7, 16, 24, 0.72);
    color: var(--id-lilac);
    font: 600 16px/1.2 var(--font-ui);
    white-space: nowrap;
    pointer-events: none;
    text-shadow: 0 1px 2px rgba(0, 0, 0, 0.42);
    outline: 1px dashed color-mix(in srgb, var(--id-lilac) 72%, transparent);
  }

  .resize-zones {
    position: absolute;
    inset: 0;
    z-index: 2;
    pointer-events: none;
  }

  /* #376 item 3: a passive cue, not a control -- pointer-events: none so a
     click aimed at it falls straight through to `.control-overlay`
     underneath, which already re-focuses on any pointerdown while `active`.
     Wraps instead of truncating/ellipsizing so the text is always fully
     legible even on the smallest resizable window (240px, see
     MIN_RESIZE_CONTENT_WIDTH in compositor.rs). */
  .focus-lost-hint {
    position: absolute;
    top: 16px;
    left: 50%;
    z-index: 3;
    transform: translateX(-50%);
    max-width: min(260px, calc(100% - 32px));
    padding: 6px 14px;
    border-radius: var(--radius-pill);
    background: rgba(20, 20, 24, 0.82);
    /* Overlay-chrome border — kept literal (uiConsistency allowlist). */
    border: 1px solid rgba(255, 255, 255, 0.22);
    color: var(--text-strong);
    font: 600 12px/1.3 var(--font-ui);
    text-align: center;
    white-space: normal;
    pointer-events: none;
    backdrop-filter: blur(6px);
  }

  /* Prominent, pointer-transparent transient operation-feedback banner. Sits
     above the video (and the focus-lost hint) but never intercepts input:
     `pointer-events: none` so a click aimed through it lands on the control
     overlay. Replace-don't-stack: exactly one instance, restarted by each
     new transient status. */
  .control-feedback-banner {
    position: absolute;
    top: 16px;
    left: 50%;
    z-index: 6;
    transform: translateX(-50%);
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    justify-content: center;
    gap: 6px;
    max-width: min(360px, calc(100% - 32px));
    padding: 8px 14px;
    border-radius: var(--radius-tile);
    background: rgba(20, 20, 24, 0.9);
    border: 1px solid rgba(255, 255, 255, 0.22);
    color: var(--text-strong);
    font: 600 13px/1.35 var(--font-ui);
    text-align: center;
    white-space: normal;
    pointer-events: none;
    backdrop-filter: blur(8px);
    box-shadow: var(--shadow-float);
  }

  .control-feedback-banner.warning {
    border-color: color-mix(in srgb, var(--warning) 45%, transparent);
    color: var(--warning);
  }

  .control-feedback-dot {
    width: 7px;
    height: 7px;
    flex-shrink: 0;
    border-radius: 50%;
    background: currentColor;
  }

  .control-feedback-text {
    font-weight: 700;
  }

  .control-feedback-detail {
    color: color-mix(in srgb, currentColor 75%, var(--text-soft));
    font-size: 11px;
    font-weight: 500;
    white-space: normal;
    overflow-wrap: anywhere;
  }

  .debug-panel {
    position: absolute;
    left: 14px;
    bottom: 14px;
    z-index: 4;
    width: min(320px, calc(100vw - 28px));
    max-height: min(420px, calc(100vh - 28px));
    overflow: auto;
    padding: 10px;
    border-radius: var(--radius-chip);
    border: 1px solid rgba(130, 170, 255, 0.28);
    background: rgba(13, 14, 16, 0.94);
    box-shadow: 0 12px 28px rgba(0, 0, 0, 0.42);
    color: var(--text-strong);
    pointer-events: auto;
    font: 500 11px var(--font-ui);
    backdrop-filter: blur(12px);
  }

  .debug-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 8px;
    /* Overlay-chrome accent — kept literal (uiConsistency allowlist). */
    color: #82aaff;
    font-weight: var(--weight-btn);
  }

  .debug-close {
    width: 22px;
    height: 22px;
    /* Overlay-chrome border — kept literal (uiConsistency allowlist). */
    border: 1px solid rgba(255, 255, 255, 0.14);
    border-radius: var(--radius-chip);
    background: var(--fill-base);
    color: var(--text-soft);
    cursor: pointer;
  }

  .debug-error {
    margin: 0;
    color: var(--warning);
  }

  .debug-panel dl {
    display: grid;
    gap: 5px;
    margin: 0;
  }

  .debug-panel dl > div {
    display: grid;
    grid-template-columns: minmax(80px, 0.35fr) minmax(0, 1fr);
    gap: 8px;
    min-width: 0;
  }

  .debug-panel dt,
  .debug-panel dd {
    min-width: 0;
    margin: 0;
    line-height: 1.25;
    overflow-wrap: anywhere;
    white-space: normal;
  }

  .debug-panel dt {
    color: var(--text-faint);
  }

  .debug-panel dd {
    color: var(--text-strong);
    font-family: var(--font-mono);
    font-weight: 500;
  }

  .debug-panel dd.stale {
    /* Overlay-chrome accent — kept literal (uiConsistency allowlist). */
    color: #ff6b7d;
  }

  .debug-note {
    margin: 1px 0 3px;
    color: var(--text-faint);
    font: 500 10px/1.25 var(--font-ui);
    overflow-wrap: anywhere;
  }

  @media (max-width: 260px) {
    .debug-panel dl > div {
      grid-template-columns: 1fr;
      gap: 2px;
    }
  }

  .resize-zone {
    position: absolute;
    border: 0;
    padding: 0;
    background: transparent;
    pointer-events: auto;
  }

  .resize-e::after,
  .resize-s::after,
  .resize-w::after {
    content: '';
    position: absolute;
    opacity: 0.62;
    background: rgba(255, 255, 255, 0.52);
    border-radius: var(--radius-pill);
    transition:
      opacity var(--motion-fast) var(--ease-standard),
      background-color var(--motion-fast) var(--ease-standard);
  }

  .resize-e:hover::after,
  .resize-e:active::after,
  .resize-s:hover::after,
  .resize-s:active::after,
  .resize-w:hover::after,
  .resize-w:active::after {
    opacity: 0.92;
    background: rgba(255, 255, 255, 0.78);
  }

  .resize-e,
  .resize-w {
    top: 0;
    bottom: 22px;
    width: 14px;
    cursor: ew-resize;
  }

  .resize-e {
    right: 0;
  }

  .resize-w {
    left: 0;
  }

  .resize-e::after,
  .resize-w::after {
    top: 50%;
    width: 2px;
    height: 56px;
    transform: translateY(-50%);
  }

  .resize-e::after {
    right: 4px;
  }

  .resize-w::after {
    left: 4px;
  }

  .resize-s {
    left: 26px;
    right: 26px;
    bottom: 0;
    height: 14px;
    cursor: ns-resize;
  }

  .resize-s::after {
    left: 50%;
    bottom: 4px;
    width: 64px;
    height: 2px;
    transform: translateX(-50%);
  }

  .resize-se,
  .resize-sw {
    bottom: 0;
    width: 30px;
    height: 30px;
    cursor: nwse-resize;
  }

  .resize-se {
    right: 0;
  }

  .resize-sw {
    left: 0;
    cursor: nesw-resize;
  }

  @media (prefers-reduced-motion: reduce) {
    .resize-e::after,
    .resize-s::after,
    .resize-w::after {
      transition: none;
    }
  }

  /* Refs #378: local echo (opt-in, default OFF). Purely decorative/local --
     never mistaken for real shared-app content: ripples/flashes are brief
     glowing accents in the product's own accent color (not app-content
     colors), and the pending-text strip is explicitly badged "unconfirmed". */
  .local-echo-layer {
    position: absolute;
    inset: 0;
    z-index: 3;
    overflow: hidden;
    pointer-events: none;
  }

  .local-echo-ripple {
    position: absolute;
    width: 36px;
    height: 36px;
    margin-left: -18px;
    margin-top: -18px;
    border-radius: var(--radius-pill);
    background: radial-gradient(circle, rgba(130, 170, 255, 0.55), rgba(130, 170, 255, 0) 70%);
    border: 1.5px solid rgba(130, 170, 255, 0.75);
    transform: scale(0.35);
    opacity: 0.85;
    animation: petal-local-echo-ripple 150ms ease-out forwards;
  }

  @keyframes petal-local-echo-ripple {
    from {
      transform: scale(0.35);
      opacity: 0.85;
    }
    to {
      transform: scale(1.15);
      opacity: 0;
    }
  }

  .local-echo-key-flash {
    position: absolute;
    top: 14px;
    left: 50%;
    width: 10px;
    height: 10px;
    margin-left: -5px;
    border-radius: var(--radius-pill);
    background: rgba(130, 170, 255, 0.9);
    /* Local-echo glow — kept literal (uiConsistency allowlist). */
    box-shadow: 0 0 10px 2px rgba(130, 170, 255, 0.6);
    animation: petal-local-echo-keyflash 150ms ease-out forwards;
  }

  @keyframes petal-local-echo-keyflash {
    from {
      transform: scale(0.6);
      opacity: 0.95;
    }
    to {
      transform: scale(1.6);
      opacity: 0;
    }
  }

  .local-echo-text {
    /* Block layout, not flex: a flex row here would let the fixed-size
       badge squeeze `.local-echo-text-chars`' flex-basis down to its
       min-content, and combined with word-break that collapses to a
       single character per line (confirmed live -- #378 QA). Stacking
       normally lets the text wrap at word boundaries with the badge
       below it, however long the pending string gets. */
    position: absolute;
    left: 50%;
    top: 60%;
    max-width: min(60vw, 420px);
    padding: 6px 10px;
    border-radius: var(--radius-chip);
    border: 1px dashed var(--text-faint);
    background: rgba(20, 24, 32, 0.42);
    backdrop-filter: blur(6px);
    color: var(--text-strong);
    font: 500 13px var(--font-mono);
    transform: translate(-50%, -110%);
  }

  .local-echo-text-chars {
    display: block;
    white-space: pre-wrap;
    overflow-wrap: break-word;
  }

  .local-echo-text-badge {
    display: inline-block;
    margin-top: 4px;
    padding: 2px 6px;
    border-radius: var(--radius-pill);
    background: var(--fill-bright);
    color: var(--text-dim);
    font: 600 9px var(--font-ui);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  @media (prefers-reduced-motion: reduce) {
    .local-echo-ripple,
    .local-echo-key-flash {
      animation-duration: 1ms;
    }
  }
</style>
