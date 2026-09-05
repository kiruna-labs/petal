<!--
  PermissionRow — one row of the Onboarding/Settings permission checklist
  (Petal-Build-Map.md §2.7 / SPEC.md §4.1). Fully prop-driven: callers own
  the real Tauri permission checks and pass the current `status` here.

  States:
  - `up-next` — neutral dim row, label "up next".
  - `optional` — neutral dim row, label "optional".
  - `blocked` — neutral dim row, label "after previous step".
  - `in-progress` — the expanded ask row: icon chip, title, status label,
    trailing label, one-line why copy, and a caller-provided action label.
  - `denied` — a calm recovery state with one System Settings action.
  - `repair` — Accessibility was enabled in System Settings but its recheck is
    still denied; the caller can offer a safe repair-and-relaunch flow.
  - `enabled` / `granted` — collapsed check-row with trailing "enabled" label.
  - `skipped` — collapsed check-row with trailing "skipped" label.
-->
<script lang="ts">
  import Button from './Button.svelte';

  export type PermissionStatus =
    | 'up-next'
    | 'optional'
    | 'blocked'
    | 'in-progress'
    | 'denied'
    | 'repair'
    | 'enabled'
    | 'granted'
    | 'skipped';

  interface Props {
    icon: 'screen' | 'mic' | 'camera' | 'accessibility';
    title: string;
    /** One-line "why" copy — used only for the active request row. */
    why?: string;
    required?: boolean;
    status: PermissionStatus;
    actionLabel?: string;
    onOpenSettings?: () => void;
    repairSettingsOpened?: boolean;
    repairRestartFailed?: boolean;
    onConfirmRepairRestart?: () => void;
    onRecheck?: () => void;
  }

  let {
    icon,
    title,
    why,
    required = false,
    status,
    actionLabel = 'Continue',
    onOpenSettings,
    repairSettingsOpened = false,
    repairRestartFailed = false,
    onConfirmRepairRestart,
    onRecheck
  }: Props = $props();

  const isExpanded = $derived(
    status === 'in-progress' || status === 'denied' || status === 'repair'
  );
  const isDim = $derived(status === 'up-next' || status === 'optional' || status === 'blocked');
  const isSuccess = $derived(status === 'enabled' || status === 'granted');
  let celebrateSuccess = $state(false);
  let hasMounted = false;
  let wasSuccess = false;
  let celebrationTimer: ReturnType<typeof setTimeout> | undefined;

  $effect(() => {
    const currentSuccess = isSuccess;

    if (!hasMounted) {
      hasMounted = true;
      wasSuccess = currentSuccess;
      return;
    }

    if (currentSuccess && !wasSuccess) {
      celebrateSuccess = true;
      clearTimeout(celebrationTimer);
      celebrationTimer = setTimeout(() => {
        celebrateSuccess = false;
      }, 450);
    }

    wasSuccess = currentSuccess;

    return () => clearTimeout(celebrationTimer);
  });

  const trailingLabel = $derived(
    {
      'up-next': 'up next',
      optional: 'optional',
      blocked: 'after previous step',
      'in-progress': required ? 'Ready when you are' : 'Optional',
      denied: 'Needs attention',
      repair: 'Needs repair',
      enabled: 'enabled',
      granted: 'granted',
      skipped: 'skipped'
    }[status]
  );
</script>

<div
  class="permission-row"
  class:expanded={isExpanded}
  class:dim={isDim}
  class:success={isSuccess}
  class:celebrate={celebrateSuccess}
  class:denied={status === 'denied'}
  class:repair={status === 'repair'}
>
  <div class="row-head">
    <div class="icon-chip" class:success={isSuccess} class:celebrate={celebrateSuccess} class:attention={status === 'denied' || status === 'repair'}>
      {#if isSuccess}
        <!-- Checkmark — same glyph/stroke convention as Toast.svelte's
             live/success state, with color supplied by --live-bright. -->
        <svg width={isExpanded ? 18 : 15} height={isExpanded ? 18 : 15} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round">
          <path d="M5 12.5 10 17.5 19 7"></path>
        </svg>
      {:else if icon === 'screen'}
        <svg width={isExpanded ? 18 : 15} height={isExpanded ? 18 : 15} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
          <rect x="2" y="4" width="20" height="13" rx="2"></rect>
          {#if status === 'denied'}
            <path d="M8 21h8M12 17v4M2 4l20 13"></path>
          {:else}
            <path d="M8 21h8M12 17v4"></path>
          {/if}
        </svg>
      {:else if icon === 'mic'}
        <svg width={isExpanded ? 18 : 15} height={isExpanded ? 18 : 15} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
          <rect x="9" y="2" width="6" height="12" rx="3"></rect>
          <path d="M5 11a7 7 0 0 0 14 0"></path>
          <path d="M12 18v3"></path>
        </svg>
      {:else if icon === 'camera'}
        <svg width={isExpanded ? 18 : 15} height={isExpanded ? 18 : 15} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
          {#if status === 'enabled' || status === 'skipped'}
            <circle cx="12" cy="8" r="4"></circle>
            <path d="M4 21a8 8 0 0 1 16 0"></path>
          {:else}
            <path d="M2 7a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z"></path>
            <path d="M16 10l5-3v10l-5-3"></path>
          {/if}
        </svg>
      {:else}
        <svg width={isExpanded ? 18 : 15} height={isExpanded ? 18 : 15} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 3a4 4 0 0 1 4 4v2"></path>
          <path d="M8 11h8a2 2 0 0 1 2 2v6a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2v-6a2 2 0 0 1 2-2z"></path>
          <path d="M12 15v2"></path>
          {#if status === 'denied'}
            <path d="M4 4l16 16"></path>
          {/if}
        </svg>
      {/if}
    </div>

    <span class="title">{title}</span>

    <div class="spacer"></div>

    {#if isExpanded}
      <span class="trailing-label" class:attention={status === 'denied'}>{trailingLabel}</span>
    {:else}
      <span class="trailing-label muted" class:success={isSuccess}>{trailingLabel}</span>
    {/if}
  </div>

  {#if status === 'in-progress'}
    <p class="why">{why}</p>
    <Button variant="primary" onclick={onOpenSettings}>{actionLabel}</Button>
  {:else if status === 'denied'}
    <Button variant="ghost" onclick={onOpenSettings}>Open System Settings</Button>
    {#if onRecheck}
      <p class="why recheck-hint">After enabling Petal there, return here and recheck it.</p>
      <Button variant="ghost" onclick={onRecheck}>Recheck Accessibility</Button>
    {/if}
  {:else if status === 'repair'}
    <p class="why repair-summary">Accessibility still looks off after you enabled Petal.</p>
    <ol class="repair-steps">
      <li>Remove the stale Petal row.</li>
      <li>Add <code>/Applications/Petal.app</code>, then enable it.</li>
      <li>Return here and restart Petal.</li>
    </ol>
    <Button variant="primary" onclick={onOpenSettings}>{actionLabel}</Button>
    {#if repairSettingsOpened}
      <p class="why repair-confirmation">After completing those steps, restart Petal to ask again.</p>
      <Button variant="ghost" onclick={onConfirmRepairRestart}>Restart Petal</Button>
    {/if}
    {#if repairRestartFailed}
      <p class="repair-fallback">Petal could not restart. Quit Petal, then open <code>/Applications/Petal.app</code>.</p>
    {/if}
  {/if}
</div>

<style>
  .permission-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 14px 16px;
    border-radius: var(--radius-popover);
  }

  /* Camera/optional rows read even dimmer than the immediate up-next row in
     canvas.html (.4 vs .55) — callers control ordering, so both share this
     one dim treatment; the difference is subtle enough it doesn't need a
     second state class. */
  .permission-row.dim {
    opacity: 0.55;
  }

  .permission-row.celebrate {
    animation: permission-success-pop var(--motion-base) var(--spring) both;
  }

  .permission-row.expanded {
    flex-direction: column;
    align-items: stretch;
    background: var(--fill-base);
    border: 1px solid var(--hairline-strong);
    padding: 16px;
  }

  .permission-row.expanded.denied {
    background: var(--fill-weak);
    border-color: var(--hairline-strong);
  }

  .permission-row.expanded.repair {
    background: rgba(255, 202, 122, 0.075);
    border-color: rgba(255, 202, 122, 0.42);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.14);
  }

  .row-head {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .icon-chip {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    border-radius: var(--radius-input);
    background: var(--fill-base);
    color: var(--text-faint);
    flex-shrink: 0;
  }

  .expanded .icon-chip {
    width: 38px;
    height: 38px;
    border-radius: var(--radius-input);
    background: var(--fill-strong);
    color: var(--text-strong);
  }

  .icon-chip.attention {
    background: var(--fill-strong);
    color: rgba(255, 210, 174, 0.88);
  }

  .icon-chip.success {
    background: var(--live-tint);
    color: var(--live-bright);
  }

  .icon-chip.celebrate {
    animation: permission-success-chip-pop var(--motion-base) var(--spring) both;
  }

  .title {
    font: 600 13px var(--font-ui);
    color: var(--text-dim);
    text-wrap: balance;
  }

  .expanded .title {
    font: 600 14.5px var(--font-ui);
    color: var(--text-primary);
  }

  .spacer {
    flex: 1;
  }

  .trailing-label {
    font: 500 10px var(--font-mono);
    color: var(--text-faint);
    flex-shrink: 0;
  }

  .trailing-label.muted {
    font-size: 10.5px;
  }

  .expanded .trailing-label {
    font-size: 10.5px;
    color: var(--text-faint);
  }

  .trailing-label.attention {
    color: rgba(255, 210, 174, 0.82);
  }

  .trailing-label.success {
    color: var(--live-bright);
  }

  .why {
    font: 400 12.5px/1.55 var(--font-ui);
    color: var(--text-faint);
    margin: 12px 0 14px;
    text-wrap: pretty;
  }

  .repair-summary {
    margin-bottom: 8px;
    color: rgba(255, 225, 187, 0.88);
  }

  .recheck-hint {
    margin: 12px 0 10px;
  }

  .repair-steps {
    display: grid;
    gap: 6px;
    margin: 0 0 14px;
    padding-left: 20px;
    color: var(--text-soft);
    font: 400 12.5px/1.45 var(--font-ui);
    text-wrap: pretty;
  }

  .repair-steps code,
  .repair-fallback code {
    overflow-wrap: anywhere;
    font: 500 11.5px var(--font-mono);
  }

  .repair-confirmation {
    margin: 14px 0 10px;
  }

  .repair-fallback {
    margin: 12px 0 0;
    color: rgba(255, 225, 187, 0.9);
    font: 500 12px/1.45 var(--font-ui);
    overflow-wrap: anywhere;
    text-wrap: pretty;
  }

  @keyframes permission-success-pop {
    from {
      transform: scale(0.985);
    }

    to {
      transform: scale(1);
    }
  }

  @keyframes permission-success-chip-pop {
    from {
      transform: scale(0.74);
      opacity: 0.72;
    }

    to {
      transform: scale(1);
      opacity: 1;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .permission-row.celebrate,
    .icon-chip.celebrate {
      animation: none;
    }
  }
</style>
