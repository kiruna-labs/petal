<!--
  /relocate — blocking launch notice (#172): Petal is running from a place the
  updater cannot install into (mounted disk image, App Translocation, another
  read-only location). The user performs the standard Finder move; the app
  never moves itself (the updater's privileged replacement script has
  different collision/signature/quarantine/ownership semantics and is not a
  one-click mover).

  Same quiet register as OfflineState/EmptyState: no color accent. Fits the
  400px main window; the detail wraps rather than truncating.
-->
<script lang="ts">
  import { onMount } from 'svelte';
  import {
    fetchLaunchLocationClass,
    openApplicationsFolder,
    relocateCopy,
    revealRunningBundle,
    type LaunchLocationClass
  } from '$lib/data/launchLocation';

  let cls = $state<LaunchLocationClass>('other_read_only');
  let copy = $derived(relocateCopy(cls));

  onMount(async () => {
    cls = await fetchLaunchLocationClass();
  });
</script>

<main class="relocate">
  <span class="dot" aria-hidden="true"></span>
  <h1 class="title">{copy.title}</h1>
  <p class="detail">{copy.detail}</p>
  <div class="actions">
    <button type="button" class="primary" onclick={() => void openApplicationsFolder()}>
      Open Applications
    </button>
    {#if copy.showReveal}
      <button type="button" class="secondary" onclick={() => void revealRunningBundle()}>
        Show Petal in Finder
      </button>
    {/if}
  </div>
</main>

<style>
  .relocate {
    height: 100%;
    width: 100%;
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    text-align: center;
    padding: 32px 28px;
    background: var(--menu-shell);
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
    margin: 0;
    font: 600 14px var(--font-ui);
    color: var(--text-primary);
    text-wrap: balance;
  }

  .detail {
    margin: 0;
    font: 400 12px var(--font-ui);
    line-height: 1.45;
    color: var(--text-dim);
    max-width: 320px;
    text-wrap: pretty;
  }

  .actions {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-top: 12px;
    width: 100%;
    max-width: 260px;
  }

  .primary,
  .secondary {
    font: 500 12.5px var(--font-ui);
    border-radius: var(--radius-control);
    padding: 8px 14px;
    cursor: pointer;
    white-space: nowrap;
  }

  .primary {
    background: var(--fill-strong);
    border: 1px solid var(--hairline-strong);
    color: var(--text-primary);
  }

  .secondary {
    background: transparent;
    border: 1px solid var(--hairline);
    color: var(--text-dim);
  }
</style>
