<!--
  Toast — small dismissible status/info messages. Per DESIGN.md §9
  ("device-switch and reconnection toasts... resilience is a headline
  feature — its UI is the unobtrusive toast that says 'switched to
  Ethernet,' not a modal") and Petal-Build-Map.md §2.2 ("Pill = the compact
  container, and it's one shell reused twice: in-meeting small state, and
  the reconnection/status toast"). This wraps the EXISTING `Pill` shell
  (not a new container) — reuses the exact icon+text toast layout already
  sketched as an example in /dev/components (the "Switched to Ethernet"
  cell using `Pill padded` + a checkmark glyph in `--live-bright`).

  This component is the presentational toast shell. ToastHost owns the real
  app wiring, queueing, timers, and native event listeners.

  Variants (kept inside the established color-rationing budget — no new
  hues invented here):
  - `reconnected` — the DESIGN.md §9 exact example ("Switched to Ethernet").
    Checkmark glyph in `--live-bright`, matching the live/success semantic
    already used for the LIVE dot and active-share state elsewhere.
  - `degraded` — amber, using the existing `--warning` token (already
    reserved for "optional"/"up next"/degraded per Petal-Build-Map.md §1
    color table — reused here, not a new color decision).
  - `info` — generic dismissible message, neutral/graphite, no accent
    color at all (most toasts should default here; only resilience
    events get color, per the color-rationing discipline applied
    throughout this component set).
-->
<script lang="ts" module>
  export type ToastVariant = 'reconnected' | 'degraded' | 'info';
</script>

<script lang="ts">
  import Pill from './Pill.svelte';

  interface Props {
    variant?: ToastVariant;
    message: string;
    dismissible?: boolean;
    onDismiss?: () => void;
    /** Optional inline action (e.g. "Restart now") rendered before the dismiss X. */
    actionLabel?: string;
    onAction?: () => void;
  }

  let {
    variant = 'info',
    message,
    dismissible = false,
    onDismiss,
    actionLabel,
    onAction
  }: Props = $props();
</script>

<Pill padded autoHeight>
  <span class="icon" class:live={variant === 'reconnected'} class:warning={variant === 'degraded'} aria-hidden="true">
    {#if variant === 'reconnected'}
      <!-- Checkmark — reuses the exact glyph + --live-bright tint already
           sketched for this toast in /dev/components (Phase 1 harness). -->
      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round">
        <path d="M5 12.5 10 17.5 19 7"></path>
      </svg>
    {:else if variant === 'degraded'}
      <!-- Degraded/weak-connection — reuses the same triangle-alert
           convention as everyday system UI, in the existing --warning
           amber (already reserved for "optional"/"degraded" per
           Petal-Build-Map.md §1 — not a new color). -->
      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M12 3.5 22 20.5H2z"></path>
        <path d="M12 9.5v5M12 18v.01"></path>
      </svg>
    {:else}
      <!-- Generic info — neutral, no color accent. -->
      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="12" cy="12" r="9"></circle>
        <path d="M12 11v5.5M12 7.5v.01"></path>
      </svg>
    {/if}
  </span>

  <span class="message" role="status" aria-live="polite">{message}</span>

  {#if actionLabel}
    <button type="button" class="action" onclick={onAction}>{actionLabel}</button>
  {/if}

  {#if dismissible}
    <button type="button" class="dismiss" onclick={onDismiss} aria-label="Dismiss">
      <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round">
        <path d="M5 5l14 14M19 5L5 19"></path>
      </svg>
    </button>
  {/if}
</Pill>

<style>
  .icon {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--text-soft);
    flex-shrink: 0;
  }

  /* The only two color moments here, both reusing already-established
     semantic tokens (live/success + warning) — never a bespoke hue. */
  .icon.live {
    color: var(--live-bright);
  }

  .icon.warning {
    color: var(--warning);
  }

  .message {
    font: 500 var(--text-caption) var(--font-ui);
    line-height: 1.35;
    color: var(--text-strong);
    flex: 1 1 auto;
    min-width: 0;
    max-width: min(360px, calc(100vw - 64px));
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: pre-line;
  }

  /* Inline action (e.g. "Restart now") — a quiet text button, no bespoke hue.
     Reuses the caption type + subtle surface already used across the pill UI. */
  .action {
    position: relative;
    font: 600 var(--text-caption) var(--font-ui);
    color: var(--text-strong);
    background: var(--fill-bright);
    border: none;
    border-radius: var(--radius-pill);
    padding: 4px 10px;
    cursor: pointer;
    flex-shrink: 0;
    white-space: nowrap;
    transition:
      background var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .action::after {
    content: '';
    position: absolute;
    top: 50%;
    left: 0;
    width: 100%;
    height: 40px;
    transform: translateY(-50%);
  }

  .action:hover {
    /* Action-pill emphasis — no fill token reaches 0.2; kept literal (uiConsistency allowlist). */
    background: rgba(255, 255, 255, 0.2);
  }

  .action:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .action:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .dismiss {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 20px;
    height: 20px;
    border-radius: var(--radius-pill);
    border: none;
    background: transparent;
    color: var(--text-faint);
    cursor: pointer;
    flex-shrink: 0;
    transition:
      background var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .dismiss::after {
    content: '';
    position: absolute;
    top: 50%;
    left: 50%;
    width: 40px;
    height: 40px;
    transform: translate(-50%, -50%);
  }

  .dismiss:hover {
    background: var(--fill-bright);
    color: var(--text-strong);
  }

  .dismiss:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .dismiss:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }
</style>
