<!--
  The rows of Settings → Plugins (plugins/README.md §2.7): one tile per
  installed plugin with name, version, source chip, enable toggle, and a
  disclosure listing every permission in plain words. Styles mirror
  Settings.svelte's `.toggle-row` tiles (copied, because Svelte scoping
  keeps the parent's rules out of this component). Every string wraps; the
  400 px main window can never clip it — pinned by
  tests/pluginSettingsRendered.test.ts.
-->
<script lang="ts">
  import Checkbox from '@petal/shared/ui/components/Checkbox.svelte';
  import {
    pluginSettingsRows,
    readEnabledOverrides,
    writeEnabledOverride,
    type EnabledOverrides,
    type InstalledPlugin
  } from '@petal/shared/plugin-host/settingsModel';
  import type { StorageLike } from '$lib/data/storageKeys';

  interface Props {
    installed: InstalledPlugin[];
    storage: StorageLike | undefined;
    onChanged?: (pluginId: string, enabled: boolean) => void;
  }

  let { installed, storage, onChanged }: Props = $props();

  // null until the user toggles something; until then the stored map is read
  // reactively so a `storage` prop change is honoured.
  let overrides = $state<EnabledOverrides | null>(null);
  const effectiveOverrides = $derived(overrides ?? readEnabledOverrides(storage));
  const rows = $derived(pluginSettingsRows(installed, effectiveOverrides));
  let openPermissions = $state<Record<string, boolean>>({});

  function toggle(pluginId: string, enabled: boolean) {
    overrides = writeEnabledOverride(storage, pluginId, enabled);
    onChanged?.(pluginId, enabled);
  }
</script>

{#if rows.length === 0}
  <p class="empty">No plugins installed.</p>
{:else}
  <ul class="rows">
    {#each rows as row (row.id)}
      <li class="row" data-plugin={row.id}>
        <label class="toggle-row">
          <Checkbox checked={row.enabled} onchange={(e) => toggle(row.id, e.currentTarget.checked)} />
          <span class="copy">
            <span class="title-line">
              <span class="title">{row.name}</span>
              <span class="chip">{row.sourceLabel}</span>
              <span class="version">v{row.version}</span>
            </span>
            {#if row.description}
              <span class="description">{row.description}</span>
            {/if}
          </span>
        </label>
        <button
          type="button"
          class="permissions-toggle"
          aria-expanded={openPermissions[row.id] ?? false}
          aria-controls={`plugin-permissions-${row.id}`}
          onclick={() => (openPermissions = { ...openPermissions, [row.id]: !(openPermissions[row.id] ?? false) })}
        >
          {openPermissions[row.id] ? 'Hide details' : `What it can do (${row.permissions.length})`}
        </button>
        {#if openPermissions[row.id]}
          <ul class="permissions" id={`plugin-permissions-${row.id}`}>
            {#each row.permissions as permission (permission.id)}
              <li>{permission.label}</li>
            {/each}
          </ul>
        {/if}
      </li>
    {/each}
  </ul>
{/if}

<style>
  .rows,
  .permissions {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .rows {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .row {
    display: flex;
    flex-direction: column;
    gap: 6px;
    min-width: 0;
  }

  .toggle-row {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    padding: 12px;
    border-radius: var(--radius-tile);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
    cursor: pointer;
  }

  .copy {
    display: flex;
    min-width: 0;
    flex: 1 1 auto;
    flex-direction: column;
    gap: 4px;
  }

  .title-line {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 6px 8px;
    min-width: 0;
  }

  .title {
    font: 600 13px var(--font-ui);
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }

  .chip {
    font: 600 10px/1 var(--font-ui);
    letter-spacing: 0.02em;
    text-transform: uppercase;
    color: var(--text-muted);
    padding: 3px 6px;
    border-radius: var(--radius-chip);
    box-shadow: var(--shadow-inset-hairline);
  }

  .version {
    font: 500 11px var(--font-mono, monospace);
    color: var(--text-faint);
  }

  .description,
  .permissions li,
  .empty {
    font: 500 11px/1.35 var(--font-ui);
    color: var(--text-muted);
    text-wrap: pretty;
    overflow-wrap: anywhere;
  }

  .empty {
    margin: 0;
  }

  .permissions-toggle {
    align-self: flex-start;
    border: 0;
    padding: 2px 0;
    background: none;
    color: var(--text-muted);
    font: 600 11px var(--font-ui);
    cursor: pointer;
    text-decoration: underline;
    text-underline-offset: 2px;
  }

  .permissions-toggle:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
    border-radius: 2px;
  }

  .permissions {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding-left: 12px;
  }

  .permissions li::before {
    content: '•';
    margin-right: 6px;
    color: var(--text-faint);
  }
</style>
