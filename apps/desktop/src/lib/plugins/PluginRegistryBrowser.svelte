<!--
  Settings → Plugins → "Get plugins" (plugins/README.md §2.9, §2.10): the
  verified registry index, minus what is already installed, as install rows.
  Rendered only when this build has a registry configured (no hosted default).
  Install = show the permissions in plain words → confirm → Rust fetches,
  verifies (signature, sha256, manifest) and stores the bundle. Every string
  wraps; pinned at 400 px by tests/pluginRegistryRendered.test.ts.
-->
<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import Button from '$lib/components/Button.svelte';
  import { COMMANDS, hasTauriBridge, type CommandArgs, type CommandReturns, type PluginRegistryVerifiedIndex } from '$lib/ipc';
  import { hostCompatibility, compareVersions } from '@petal/shared/plugin-host/manifest';
  import { permissionLabel } from '@petal/shared/plugin-host/settingsModel';

  type RegistryPlugin = PluginRegistryVerifiedIndex['index']['plugins'][number];
  type RegistryVersion = RegistryPlugin['versions'][number];

  interface Props {
    /** Ids already installed (built-in or registry), keyed to their version. */
    installed: Record<string, string>;
    hostVersion: string | null;
    onInstalled?: (pluginId: string, version: string) => void;
  }

  let { installed, hostVersion, onInstalled }: Props = $props();

  let configured = $state<boolean | null>(null);
  let registryUrl = $state<string | null>(null);
  let index = $state<PluginRegistryVerifiedIndex | null>(null);
  let loadError = $state<string | null>(null);
  let expanded = $state<string | null>(null);
  let busy = $state<string | null>(null);
  let rowError = $state<Record<string, string>>({});

  function installable(plugin: RegistryPlugin): RegistryVersion | null {
    if (!hostVersion) return null;
    const ok = plugin.versions
      .filter((v) => v.verified && hostCompatibility({ apiVersion: v.apiVersion, minHostVersion: v.minHostVersion }, hostVersion).ok)
      .sort((a, b) => compareVersions(b.version, a.version));
    return ok[0] ?? null;
  }

  type Row = { plugin: RegistryPlugin; version: RegistryVersion | null; state: 'installable' | 'update' | 'installed' | 'unverified' | 'incompatible' };
  const rows = $derived.by((): Row[] => {
    if (!index) return [];
    return index.index.plugins.map((plugin) => {
      const version = installable(plugin);
      const current = installed[plugin.id];
      let state: Row['state'];
      if (!plugin.versions.some((v) => v.verified)) state = 'unverified';
      else if (!version) state = 'incompatible';
      else if (current && compareVersions(version.version, current) > 0) state = 'update';
      else if (current) state = 'installed';
      else state = 'installable';
      return { plugin, version, state };
    });
  });

  async function load() {
    if (!hasTauriBridge()) {
      configured = false;
      return;
    }
    try {
      const status = await invoke<CommandReturns[typeof COMMANDS.pluginRegistryStatus]>(COMMANDS.pluginRegistryStatus);
      configured = status.configured;
      registryUrl = status.url;
      if (!status.configured) return;
      index = await invoke<PluginRegistryVerifiedIndex>(COMMANDS.pluginRegistryIndex);
      loadError = null;
    } catch (e) {
      loadError = String((e as Error)?.message ?? e);
    }
  }

  async function install(row: Row) {
    if (!row.version) return;
    const id = row.plugin.id;
    busy = id;
    rowError = { ...rowError, [id]: '' };
    try {
      await invoke(COMMANDS.pluginInstallFromRegistry, { pluginId: id, version: row.version.version } satisfies CommandArgs[typeof COMMANDS.pluginInstallFromRegistry]);
      expanded = null;
      onInstalled?.(id, row.version.version);
    } catch (e) {
      rowError = { ...rowError, [id]: String((e as Error)?.message ?? e) };
    } finally {
      busy = null;
    }
  }

  onMount(() => void load());
</script>

{#if configured}
  <div class="browser" data-registry={registryUrl}>
    <span class="support-title">Get plugins</span>
    {#if loadError}
      <span class="note error">Could not load the plugin registry: {loadError}</span>
      <button type="button" class="link-button" onclick={() => void load()}>Try again</button>
    {:else if !index}
      <span class="note">Loading the plugin registry…</span>
    {:else if rows.length === 0}
      <span class="note">The registry has no plugins yet.</span>
    {:else}
      <ul class="rows">
        {#each rows as row (row.plugin.id)}
          <li class="row" data-plugin={row.plugin.id} data-state={row.state}>
            <div class="head">
              <span class="copy">
                <span class="title-line">
                  <span class="title">{row.plugin.name}</span>
                  <span class="publisher">by {row.plugin.publisher}</span>
                  {#if row.version}<span class="version">v{row.version.version}</span>{/if}
                </span>
                {#if row.plugin.description}<span class="description">{row.plugin.description}</span>{/if}
              </span>
              {#if row.state === 'installable' || row.state === 'update'}
                <Button variant={expanded === row.plugin.id ? 'ghost' : 'primary'} disabled={busy !== null} onclick={() => (expanded = expanded === row.plugin.id ? null : row.plugin.id)}>
                  {row.state === 'update' ? 'Update' : 'Install'}
                </Button>
              {:else if row.state === 'installed'}
                <span class="chip">Installed</span>
              {:else if row.state === 'unverified'}
                <span class="chip">Awaiting review</span>
              {:else}
                <span class="chip">Needs newer Petal</span>
              {/if}
            </div>
            {#if expanded === row.plugin.id && row.version}
              <div class="consent">
                <span class="consent-title">{row.plugin.name} will be able to:</span>
                <ul class="permissions">
                  {#each row.version.permissions as permission (permission)}
                    <li>{permissionLabel(permission)}</li>
                  {/each}
                </ul>
                <div class="consent-actions">
                  <Button variant="primary" disabled={busy !== null} onclick={() => void install(row)}>
                    {busy === row.plugin.id ? 'Installing…' : `Install ${row.plugin.name}`}
                  </Button>
                  <button type="button" class="link-button" disabled={busy !== null} onclick={() => (expanded = null)}>Not now</button>
                </div>
              </div>
            {/if}
            {#if rowError[row.plugin.id]}
              <span class="note error">{rowError[row.plugin.id]}</span>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </div>
{/if}

<style>
  .browser {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
  }

  .support-title {
    font: 600 13px var(--font-ui);
    color: var(--text-primary);
  }

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
    gap: 8px;
    padding: 12px;
    border-radius: var(--radius-tile);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
    min-width: 0;
  }

  .head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    min-width: 0;
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

  .publisher,
  .version {
    font: 500 11px var(--font-ui);
    color: var(--text-faint);
    overflow-wrap: anywhere;
  }

  .description,
  .note,
  .permissions li,
  .consent-title {
    font: 500 11px/1.35 var(--font-ui);
    color: var(--text-muted);
    text-wrap: pretty;
    overflow-wrap: anywhere;
  }

  .note.error {
    color: var(--danger);
  }

  .chip {
    flex: 0 0 auto;
    font: 600 10px/1 var(--font-ui);
    letter-spacing: 0.02em;
    text-transform: uppercase;
    color: var(--text-muted);
    padding: 5px 7px;
    border-radius: var(--radius-chip);
    box-shadow: var(--shadow-inset-hairline);
    white-space: nowrap;
  }

  .consent {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding-top: 6px;
    border-top: 1px solid var(--hairline);
  }

  .consent-title {
    color: var(--text-primary);
    font-weight: 600;
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

  .consent-actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
  }

  .link-button {
    border: 0;
    padding: 2px 0;
    background: none;
    color: var(--text-muted);
    font: 600 11px var(--font-ui);
    cursor: pointer;
    text-decoration: underline;
    text-underline-offset: 2px;
  }

  .link-button:disabled {
    cursor: default;
    opacity: var(--disabled-opacity);
  }
</style>
