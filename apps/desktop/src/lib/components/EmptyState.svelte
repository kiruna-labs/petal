<!--
  EmptyState — generic quiet empty-state block. Per DESIGN.md §9's closing
  note ("standard error/empty/offline states... keep them in the same
  quiet register") and Petal-Build-Map.md §4 (not designed yet — no
  canvas.html markup for this). Built functional-but-plain, same honesty
  standard as RemoteWindowHeader/RosterPopover: no illustration (the task
  brief explicitly calls for "icon/illustration-less, just quiet text +
  optional action"), matching the restraint already established by the
  main-menu's plain empty room rows (RoomRow's "empty" state) rather than
  inventing a new, more decorative empty-state treatment.

  Generic enough to cover multiple call sites (empty roster, no rooms, no
  devices found, etc.) via `title`/`detail`/optional action slot — not a
  one-off per screen.
-->
<script lang="ts">
  interface Props {
    title: string;
    detail?: string;
    actionLabel?: string;
    onAction?: () => void;
  }

  let { title, detail, actionLabel, onAction }: Props = $props();
</script>

<div class="empty-state">
  <span class="title">{title}</span>
  {#if detail}
    <span class="detail">{detail}</span>
  {/if}
  {#if actionLabel}
    <button type="button" class="action" onclick={onAction}>{actionLabel}</button>
  {/if}
</div>

<style>
  .empty-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 6px;
    text-align: center;
    padding: 32px 20px;
  }

  .title {
    font: 600 13.5px var(--font-ui);
    color: var(--text-faint);
    text-wrap: balance;
  }

  .detail {
    font: 400 12px var(--font-ui);
    color: var(--text-faint);
    max-width: 240px;
    text-wrap: pretty;
  }

  .action {
    margin-top: 8px;
    background: none;
    border: none;
    min-height: 40px;
    padding: 0 12px;
    font: 600 12px var(--font-ui);
    color: var(--text-soft);
    text-decoration: underline;
    text-underline-offset: 2px;
    cursor: pointer;
    transition:
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .action:hover {
    color: var(--text-primary);
  }

  .action:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .action:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }
</style>
