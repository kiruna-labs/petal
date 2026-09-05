<script lang="ts">
  interface Props {
    label: string;
    icon: 'settings' | 'quit' | 'window';
    tone?: 'default' | 'danger';
    role?: string;
    onclick?: () => void;
  }

  let { label, icon, tone = 'default', role, onclick }: Props = $props();
</script>

<button type="button" class="menu-item" class:danger={tone === 'danger'} {role} {onclick}>
  {#if icon === 'settings'}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="3"></circle>
      <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
    </svg>
  {:else if icon === 'window'}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <rect x="3" y="4" width="18" height="16" rx="2"></rect>
      <line x1="3" y1="9" x2="21" y2="9"></line>
    </svg>
  {:else}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M18.36 6.64a9 9 0 1 1-12.73 0"></path>
      <line x1="12" y1="2" x2="12" y2="12"></line>
    </svg>
  {/if}
  <span>{label}</span>
</button>

<style>
  .menu-item {
    display: flex;
    align-items: center;
    gap: 9px;
    width: 100%;
    min-height: 40px;
    border: none;
    border-radius: var(--radius-chip);
    background: transparent;
    color: var(--text-soft);
    cursor: pointer;
    padding: 0 9px;
    font: 600 12px var(--font-ui);
    text-align: left;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .menu-item:hover {
    background: var(--fill-strong);
    color: var(--text-strong);
  }

  .menu-item.danger {
    color: var(--text-dim);
  }

  .menu-item.danger:hover {
    color: var(--danger);
    background: var(--danger-tint-06);
  }

  .menu-item:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .menu-item:active {
    transform: scale(var(--press-scale, 0.96));
  }
</style>
