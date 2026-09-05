<!--
  Sharer-side remote-control consent surface (ask policy -- the default).

  The always-present, hidden native panel owns only its window lifecycle and
  positioning. This route owns the queue, countdown, and Allow/Deny actions.
  Both ordinary control requests and Windows full-control escalations use the
  same non-activating surface, distinguished by the typed prompt kind.

  Queue, never replace: a second request waits its turn. Every prompt has the
  host-provided deadline and expires fail-closed; no timeout path changes the
  share mode or grants control.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount, tick } from 'svelte';
  import {
    COMMANDS,
    EVENTS,
    remoteWindowOwnerLabel,
    type ControlConsentRequestedEvent,
    type RemoteControlStatus,
    type ShareControlModeChanged
  } from '$lib/ipc';

  type QueuedRequest = ControlConsentRequestedEvent & {
    /** Wall-clock deadline mirrored from the host's timeout. */
    expiresAt: number;
  };
  type RequestIdentity = Pick<ControlConsentRequestedEvent, 'kind' | 'windowId' | 'controllerId'>;

  let queue = $state<QueuedRequest[]>([]);
  let now = $state(Date.now());
  let host: HTMLDivElement | undefined = $state();
  let answering = $state(false);
  // In-flight measurement calls must not reveal the panel after an answer
  // starts hiding it.
  let presentationGeneration = 0;
  let tickTimer: ReturnType<typeof setInterval> | undefined;

  const current = $derived(queue[0] ?? null);
  const isEscalation = $derived(current?.kind === 'fullControlEscalation');
  const secondsLeft = $derived(
    current ? Math.max(0, Math.ceil((current.expiresAt - now) / 1000)) : 0
  );
  const requesterLabel = $derived(current ? remoteWindowOwnerLabel(current.controllerName) : '');
  const windowLabel = $derived(
    current?.windowTitle && current.windowTitle.trim() ? current.windowTitle.trim() : 'your shared window'
  );
  const waitingCount = $derived(Math.max(0, queue.length - 1));

  function sameRequest(a: RequestIdentity, b: RequestIdentity) {
    return a.kind === b.kind && a.windowId === b.windowId && a.controllerId === b.controllerId;
  }

  function enqueue(payload: ControlConsentRequestedEvent) {
    const existing = queue.findIndex((q) => sameRequest(q, payload));
    // A duplicate must never extend the host-provided deadline. The host
    // normally suppresses duplicate events, but keeping the first entry here
    // makes the route fail closed even if delivery is repeated.
    if (existing >= 0) return;
    queue = [...queue, { ...payload, expiresAt: Date.now() + payload.timeoutMs }];
  }

  function drop(windowId: number, controllerId: string, kind?: ControlConsentRequestedEvent['kind']) {
    const before = queue.length;
    queue = queue.filter((q) => !(
      q.windowId === windowId &&
      q.controllerId === controllerId &&
      (kind === undefined || q.kind === kind)
    ));
    if (queue.length !== before) {
      presentationGeneration += 1;
      if (queue.length === 0) void dismiss();
    }
  }

  function clearQueue() {
    if (queue.length === 0) return;
    queue = [];
    presentationGeneration += 1;
    void dismiss();
  }

  function dropEscalationsForWindow(windowId: number) {
    const before = queue.length;
    queue = queue.filter((q) => !(q.windowId === windowId && q.kind === 'fullControlEscalation'));
    if (queue.length !== before) {
      presentationGeneration += 1;
      if (queue.length === 0) void dismiss();
    }
  }

  async function answer(approve: boolean) {
    const req = current;
    if (!req || answering) return;
    answering = true;
    presentationGeneration += 1;
    // Hide before the backend answer can emit a status event. This also
    // prevents a queued reveal from leaving a transparent panel behind.
    await dismiss();
    try {
      if (req.kind === 'fullControlEscalation') {
        await invoke(COMMANDS.remoteControlAnswerEscalation, {
          windowId: req.windowId,
          controllerId: req.controllerId,
          approve
        });
      } else {
        await invoke(COMMANDS.remoteControlAnswerConsent, {
          windowId: req.windowId,
          controllerId: req.controllerId,
          approve
        });
      }
    } catch (e) {
      console.error(
        req.kind === 'fullControlEscalation'
          ? 'remote_control_answer_escalation failed'
          : 'remote_control_answer_consent failed',
        e
      );
    } finally {
      answering = false;
      drop(req.windowId, req.controllerId, req.kind);
    }
  }

  async function dismiss() {
    try {
      await invoke(COMMANDS.controlConsentDismiss);
    } catch {
      // No Tauri backend (plain browser preview) -- nothing to hide.
    }
  }

  // Measure the rendered card and ask the native panel to match it (the
  // share-notice resize-to-content pattern). Long names and titles wrap
  // inside the capped copy column; no user-facing text is truncated.
  async function reveal() {
    const generation = presentationGeneration;
    await tick();
    if (generation !== presentationGeneration || answering || !current || !host) return;
    const height = Math.ceil(host.getBoundingClientRect().height);
    if (height <= 0) return;
    try {
      await invoke(COMMANDS.controlConsentPresent, { height });
    } catch {
      // No Tauri backend (plain-browser preview) -- nothing to show.
    }
  }

  // Re-measure when the active prompt, queue count, or countdown changes.
  $effect(() => {
    if (current && queue.length >= 0 && secondsLeft >= 0) void reveal();
  });

  $effect(() => {
    if (!host || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => {
      if (current) void reveal();
    });
    observer.observe(host);
    return () => observer.disconnect();
  });

  onMount(() => {
    tickTimer = setInterval(() => {
      now = Date.now();
      // The host owns the authoritative timeout. This local mirror only
      // removes stale copy while waiting for its terminal status; escalation
      // expiry never invokes a mode change.
      const expired = queue.filter((q) => q.expiresAt <= now);
      for (const q of expired) drop(q.windowId, q.controllerId, q.kind);
    }, 250);

    const unRequested = listen<ControlConsentRequestedEvent>(EVENTS.controlConsentRequested, (event) => {
      enqueue(event.payload);
    });
    const unStatus = listen<RemoteControlStatus>(EVENTS.remoteControlStatus, (event) => {
      const s = event.payload;
      if (s.status === 'awaitingConsent') return;
      // An active status resolves ordinary consent. Leave a same-key
      // escalation prompt alone; it has its own typed identity and deadline.
      if (s.status === 'active' || (s.status === 'denied' &&
          (s.reason === 'consentDenied' || s.reason === 'consentTimedOut'))) {
        drop(s.windowId, s.controllerId, 'control');
        return;
      }
      // Share/controller/policy teardown invalidates either prompt kind.
      drop(s.windowId, s.controllerId);
    });
    const unShareState = listen<{ windowId: number; shared: boolean }>(EVENTS.shareStateChanged, (event) => {
      if (!event.payload.shared) {
        queue = queue.filter((q) => q.windowId !== event.payload.windowId);
        if (queue.length === 0) void dismiss();
      }
    });
    const unShareControlMode = listen<ShareControlModeChanged>(EVENTS.shareControlModeChanged, (event) => {
      // Any newer host mode decision invalidates an outstanding escalation.
      dropEscalationsForWindow(event.payload.windowId);
    });
    const unRoomLeft = listen(EVENTS.roomLeft, clearQueue);
    return () => {
      clearInterval(tickTimer);
      unRequested.then((u) => u()).catch(() => {});
      unStatus.then((u) => u()).catch(() => {});
      unShareState.then((u) => u()).catch(() => {});
      unShareControlMode.then((u) => u()).catch(() => {});
      unRoomLeft.then((u) => u()).catch(() => {});
    };
  });
</script>

<div class="consent-host" bind:this={host}>
  {#if current}
    <div
      class="card"
      role="alertdialog"
      aria-labelledby="consent-title"
      aria-describedby="consent-detail"
      aria-label={isEscalation ? 'Full control request' : 'Remote control request'}
    >
      <div class="copy">
        <p class="title" id="consent-title">
          {#if isEscalation}
            <span class="name">{requesterLabel}</span> requested full control of <span class="window">{windowLabel}</span>
          {:else}
            <span class="name">{requesterLabel}</span> wants to control <span class="window">{windowLabel}</span>
          {/if}
        </p>
        <p class="detail" id="consent-detail">
          {#if isEscalation}
            They already have cursor-preserving control. Full control lets them move your pointer while controlling this window. Denies automatically in {secondsLeft}s.
          {:else}
            They can click and type in that window until you stop sharing it. Denies automatically in {secondsLeft}s.
          {/if}
          {#if waitingCount > 0}
            {waitingCount === 1 ? 'One more request is waiting.' : `${waitingCount} more requests are waiting.`}
          {/if}
        </p>
      </div>
      <div class="actions">
        <button type="button" class="btn deny" onclick={() => void answer(false)} disabled={answering}>Deny</button>
        <button type="button" class="btn allow" onclick={() => void answer(true)} disabled={answering}>Allow</button>
      </div>
    </div>
  {/if}
</div>

<style>
  :global(html),
  :global(body) {
    background: transparent !important;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  .consent-host {
    width: 100%;
    display: flex;
    align-items: flex-start;
    justify-content: center;
    box-sizing: border-box;
    font-family: var(--font-ui);
  }

  /* Same raised-graphite shell + lilac hairline as the share notice, so the
     two top-center prompts read as one family. */
  .card {
    box-sizing: border-box;
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px;
    border-radius: var(--radius-card);
    background: var(--surface-raised);
    color: var(--text-primary);
    box-shadow:
      inset 0 0 0 1px color-mix(in srgb, var(--id-lilac) 56%, var(--hairline-strong)),
      var(--shadow-float);
  }

  .copy {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }

  /* The copy column caps at 340px and WRAPS; overflow-wrap keeps an unbroken
     display name or window title from pushing outside the panel. */
  .title,
  .detail {
    margin: 0;
    max-width: 340px;
    overflow-wrap: anywhere;
    white-space: normal;
  }

  .title {
    font: 600 14px var(--font-ui);
    line-height: 1.35;
  }

  .name,
  .window {
    color: var(--id-lilac);
  }

  .detail {
    font: 400 12px var(--font-ui);
    line-height: 1.4;
    color: var(--text-muted);
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .btn {
    min-width: 96px;
    min-height: 40px;
    padding: 7px 14px;
    border-radius: var(--radius-control);
    border: 1px solid var(--hairline-strong);
    background: var(--fill-weak);
    color: inherit;
    font: 700 var(--text-label) / 1.2 var(--font-ui);
    cursor: pointer;
    transition:
      background-color var(--motion-feedback) var(--ease-standard),
      color var(--motion-feedback) var(--ease-standard),
      transform var(--motion-feedback) var(--ease-standard);
  }

  .btn:hover:not(:disabled) {
    background: var(--fill-strong);
  }

  .btn:active:not(:disabled) {
    transform: scale(var(--press-scale));
  }

  .btn:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .btn.allow {
    background: var(--id-lilac);
    border-color: var(--id-lilac);
    color: var(--bg-base);
  }

  .btn.allow:hover:not(:disabled) {
    background: color-mix(in srgb, var(--id-lilac) 88%, white);
  }

  .btn:disabled {
    opacity: var(--disabled-opacity);
    cursor: default;
  }

  @media (prefers-reduced-motion: reduce) {
    .btn {
      transition: none;
    }

    .btn:active:not(:disabled) {
      transform: none;
    }
  }
</style>
