<!--
  OfflineState — connection-lost presentational state. Per DESIGN.md §9's
  closing note (error/empty/offline states, "same quiet register") and
  Petal-Build-Map.md §4 (not designed yet). Distinct from a `Toast` — this
  is a small full-block state for when a surface has genuinely nothing to
  show because there's no connection (e.g. a roster popover or gallery
  can't reach the room), rather than a transient reconnection notice
  (which is what `Toast` variant="degraded"/"reconnected" is for).

  Deliberately no color accent — offline is not one of the two sanctioned
  color moments (live-hero plum / danger red), so this stays fully
  graphite per the color-rationing rule, same restraint as `EmptyState`.
  A quiet dot (dim, not the live-green one) reads as "signal absent"
  without borrowing the live/success token.
-->
<script lang="ts">
  interface Props {
    title?: string;
    detail?: string;
    retryLabel?: string;
    onRetry?: () => void;
  }

  let {
    title = 'Connection lost',
    detail = "We'll keep trying to reconnect automatically.",
    retryLabel = 'Retry now',
    onRetry
  }: Props = $props();
</script>

<div class="offline-state">
  <span class="dot" aria-hidden="true"></span>
  <span class="title">{title}</span>
  {#if detail}
    <span class="detail">{detail}</span>
  {/if}
  {#if onRetry}
    <button type="button" class="retry" onclick={onRetry}>{retryLabel}</button>
  {/if}
</div>

<style>
  .offline-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 6px;
    text-align: center;
    padding: 32px 20px;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: var(--radius-pill);
    /* Status dot — no fill token reaches 0.3; kept literal (uiConsistency allowlist). */
    background: rgba(255, 255, 255, 0.3);
    margin-bottom: 4px;
  }

  .title {
    font: 600 13.5px var(--font-ui);
    color: var(--text-dim);
    text-wrap: balance;
  }

  .detail {
    font: 400 12px var(--font-ui);
    color: var(--text-faint);
    max-width: 240px;
    text-wrap: pretty;
  }

  .retry {
    margin-top: 8px;
    background: var(--fill-strong);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    min-height: 40px;
    padding: 0 14px;
    font: 600 12px var(--font-ui);
    color: var(--text-primary);
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .retry:hover {
    background: var(--fill-bright);
  }

  .retry:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .retry:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }
</style>
