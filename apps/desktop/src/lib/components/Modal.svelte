<!--
  Modal - shared design-system dialog shell for transient in-app surfaces.
  Owns the scrim, escape handling, close affordance, and centered sizing so
  feature components can focus on their own content.
-->
<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import type { Snippet } from 'svelte';
  import CloseButton from './CloseButton.svelte';
  import { exitDuration } from '$lib/motion';

  interface Props {
    title: string;
    eyebrow?: string;
    onClose?: () => void;
    width?: 'compact' | 'comfortable' | 'wide';
    children?: Snippet;
  }

  let { title, eyebrow, onClose, width = 'comfortable', children }: Props = $props();
  let panel: HTMLDialogElement | undefined = $state();
  let closing = $state(false);
  let closeTimer: ReturnType<typeof setTimeout> | undefined;
  /** The element that opened the modal, restored to focus on close so the
   * trigger stays findable and Tab doesn't strand the user at <body>. */
  let openerEl: HTMLElement | null = null;

  function handleKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      requestClose();
      return;
    }
    // Lightweight focus trap: Tab cannot leave the dialog (it would land on
    // the app behind an aria-modal surface). Focusables only — the panel
    // itself (tabindex=-1) is the entry point, so Shift+Tab from it wraps to
    // the last control instead of escaping.
    if (event.key !== 'Tab' || !panel) return;
    const focusables = Array.from(
      panel.querySelectorAll<HTMLElement>(
        'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'
      )
    );
    if (focusables.length === 0) return;
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    const active = document.activeElement;
    if (event.shiftKey) {
      if (active === first || active === panel) {
        event.preventDefault();
        last.focus();
      }
    } else if (active === last) {
      event.preventDefault();
      first.focus();
    }
  }

  function handleBackdropClick(event: MouseEvent) {
    if (event.target === event.currentTarget) requestClose();
  }

  function requestClose() {
    if (!onClose || closing) return;
    closing = true;
    closeTimer = setTimeout(() => onClose(), exitDuration());
  }

  onMount(() => {
    // Save the opener BEFORE the panel takes focus (rAF below).
    openerEl = document.activeElement as HTMLElement | null;
    requestAnimationFrame(() => panel?.focus());
  });

  onDestroy(() => {
    if (closeTimer) clearTimeout(closeTimer);
    // The opener is a sibling of the backdrop and survives the modal's
    // unmount (the caller still owns it) — return focus if it's still live.
    if (openerEl?.isConnected) openerEl.focus();
  });
</script>

<svelte:window onkeydown={handleKeydown} />

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="modal-backdrop" class:closing onclick={handleBackdropClick} role="presentation">
  <dialog
    open
    bind:this={panel}
    class="modal"
    class:compact={width === 'compact'}
    class:wide={width === 'wide'}
    aria-modal="true"
    aria-label={title}
    tabindex="-1"
  >
    <header class="modal-head">
      <div class="title-stack">
        {#if eyebrow}
          <span class="eyebrow">{eyebrow}</span>
        {/if}
        <h2>{title}</h2>
      </div>
      {#if onClose}
        <div class="close-slot">
          <CloseButton onclick={requestClose} />
        </div>
      {/if}
    </header>
    <div class="modal-body">
      {@render children?.()}
    </div>
  </dialog>
</div>

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    z-index: 30;
    display: grid;
    place-items: center;
    padding: 28px;
    background: rgba(4, 5, 6, 0.58);
    overscroll-behavior: none;
  }

  .modal {
    width: min(640px, 100%);
    max-height: min(720px, calc(100vh - 56px));
    position: relative;
    margin: 0;
    padding: 0;
    border: 0;
    display: flex;
    flex-direction: column;
    overflow: hidden;
    overscroll-behavior: none;
    border-radius: var(--radius-card);
    background: var(--popover-bg);
    box-shadow:
      var(--shadow-panel),
      0 0 0 1px var(--hairline);
    color: var(--text-primary);
    font-family: var(--font-ui);
    transform-origin: 50% 46%;
    animation: modal-panel-in var(--motion-enter) var(--ease-standard) both;
  }

  .modal.compact {
    width: min(460px, 100%);
  }

  .modal.wide {
    width: min(760px, 100%);
  }

  .modal:focus {
    outline: none;
  }

  .modal-head {
    display: flex;
    align-items: center;
    gap: 16px;
    min-height: 58px;
    padding: 13px 14px 13px 18px;
    border-bottom: 1px solid var(--hairline);
    flex-shrink: 0;
    animation: modal-chunk-in var(--motion-enter) var(--ease-standard) both;
  }

  .title-stack {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .eyebrow {
    font: 700 10px var(--font-mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-faint);
  }

  h2 {
    margin: 0;
    color: var(--text-primary);
    font: 700 14px var(--font-ui);
    letter-spacing: 0;
    text-wrap: balance;
  }

  .close-slot {
    margin-left: auto;
    display: flex;
  }

  .modal-body {
    min-height: 0;
    /* Scrollable, not clipped: tall content (e.g. a user-dragged-resize
       FeedbackModal textarea) must never strand the actions row below an
       invisible cut with no scrollbar. */
    overflow-y: auto;
    overscroll-behavior: none;
    animation: modal-chunk-in var(--motion-enter) var(--ease-standard) both;
  }

  .modal-backdrop {
    animation: modal-backdrop-in var(--motion-enter) var(--ease-standard) both;
  }

  .modal-backdrop.closing {
    animation: modal-backdrop-out var(--motion-exit) var(--ease-exit) both;
  }

  .modal-backdrop.closing .modal {
    animation: modal-panel-out var(--motion-exit) var(--ease-exit) both;
  }

  .modal-backdrop.closing .modal-head,
  .modal-backdrop.closing .modal-body {
    animation: modal-chunk-out var(--motion-exit) var(--ease-exit) both;
  }

  @keyframes modal-backdrop-in {
    from {
      opacity: 0;
    }
  }

  @keyframes modal-backdrop-out {
    to {
      opacity: 0;
    }
  }

  @keyframes modal-panel-in {
    from {
      opacity: 0;
      transform: translateY(var(--motion-distance));
    }
  }

  @keyframes modal-panel-out {
    to {
      opacity: 0;
      transform: translateY(0);
    }
  }

  @keyframes modal-chunk-in {
    from {
      opacity: 0;
      transform: translateY(var(--motion-distance));
    }
  }

  @keyframes modal-chunk-out {
    to {
      opacity: 0;
      transform: translateY(0);
    }
  }

  @media (max-width: 520px) {
    .modal-backdrop {
      padding: 16px;
      align-items: end;
    }

    .modal,
    .modal.compact,
    .modal.wide {
      width: 100%;
      max-height: calc(100vh - 32px);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .modal-backdrop,
    .modal,
    .modal-head,
    .modal-body,
    .modal-backdrop.closing,
    .modal-backdrop.closing .modal,
    .modal-backdrop.closing .modal-head,
    .modal-backdrop.closing .modal-body {
      animation: none;
    }
  }
</style>
