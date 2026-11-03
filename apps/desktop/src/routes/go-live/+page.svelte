<!--
  Go-live dialog, in its own singleton window (`go-live`,
  src-tauri/src/streaming/commands.rs) opened from a share's options menu.
  Picks a destination (saved, or a server URL and stream key with an
  optional Save), the resolution and shape of the stream's fixed 16:9 canvas
  and max bitrate, previews the output, and goes live. The stream key is
  sent once to `stream_go_live` and never read back: saved keys live in the
  OS credential store on the native side.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { openUrl } from '@tauri-apps/plugin-opener';
  import { onMount } from 'svelte';
  import {
    COMMANDS,
    EVENTS,
    hasTauriBridge,
    listenUntilDestroy,
    type GoLiveRequest,
    type SavedStreamDestination,
    type StreamOrientation,
    type StreamPreview,
    type StreamResolution
  } from '#lib/ipc.js';
  import {
    parseMaxBitrateMbps,
    streamClientCanGoLive,
    streamClientHints,
    streamIsDownscaled,
    streamPreviewLine
  } from '#lib/data/liveStream.js';
  import { streamerMode, watchStreamerMode } from '#lib/stores/streamerMode.svelte.js';
  import {
    CUSTOM_STREAM_SERVICE_ID,
    STREAM_SERVICES,
    streamServiceById,
    streamServiceForUrl
  } from '#lib/data/streamServices.js';

  const NEW_DESTINATION = '__new__';
  const RESOLUTION_CHOICES: ReadonlyArray<{ value: StreamResolution; label: string }> = [
    { value: 'auto', label: 'Auto' },
    { value: '720p', label: '720p' },
    { value: '1080p', label: '1080p' },
    { value: '1440p', label: '1440p' },
    { value: '4k', label: '4K' }
  ];
  const ORIENTATION_CHOICES: ReadonlyArray<{ value: StreamOrientation; label: string }> = [
    { value: 'auto', label: 'Auto' },
    { value: 'landscape', label: 'Landscape 16:9' },
    { value: 'portrait', label: 'Portrait 9:16' }
  ];

  let windowId = $state(0);
  let destinations = $state<SavedStreamDestination[]>([]);
  let destinationId = $state(NEW_DESTINATION);
  let serviceId = $state(STREAM_SERVICES[0].id);
  let serverUrl = $state(STREAM_SERVICES[0].serverUrl);
  let streamKey = $state('');
  let saveDestination = $state(true);
  let destinationName = $state('');
  let resolution = $state<StreamResolution>('auto');
  let orientation = $state<StreamOrientation>('auto');
  let bitrateInput = $state('');
  let preview = $state<StreamPreview | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let destroyed = false;

  const service = $derived(streamServiceById(serviceId));
  const maxBitrateBps = $derived(parseMaxBitrateMbps(bitrateInput));
  const usingNew = $derived(destinationId === NEW_DESTINATION);
  const destinationService = $derived(
    usingNew
      ? service
      : streamServiceForUrl(destinations.find((d) => d.id === destinationId)?.serverUrl ?? '')
  );
  const canGoLive = $derived(
    !busy &&
      windowId !== 0 &&
      maxBitrateBps !== undefined &&
      streamClientCanGoLive(preview) &&
      (!usingNew || (serverUrl.trim() !== '' && streamKey.trim() !== ''))
  );

  async function loadDestinations() {
    destinations = await invoke<SavedStreamDestination[]>(COMMANDS.streamDestinationsList).catch(
      () => []
    );
    if (destinationId === NEW_DESTINATION && destinations.length > 0) {
      destinationId = destinations[0].id;
    }
    if (destinationId !== NEW_DESTINATION && !destinations.some((d) => d.id === destinationId)) {
      destinationId = destinations[0]?.id ?? NEW_DESTINATION;
    }
  }

  async function refreshPreview() {
    if (windowId === 0 || maxBitrateBps === undefined) return;
    try {
      preview = await invoke<StreamPreview>(COMMANDS.streamPreview, {
        windowId,
        resolution,
        orientation,
        maxBitrateBps
      });
    } catch (e) {
      preview = null;
      error = String(e);
    }
  }

  $effect(() => {
    // Re-plan whenever an input that changes the output does.
    void [windowId, resolution, orientation, maxBitrateBps];
    void refreshPreview();
  });

  // Picking a service fills its URL (still editable) and, unless the user
  // typed their own, the suggested name.
  function selectService(id: string) {
    const previous = service;
    serviceId = id;
    const next = streamServiceById(id);
    if (next.serverUrl !== '' || serverUrl === previous.serverUrl) serverUrl = next.serverUrl;
    const name = destinationName.trim();
    if (name === '' || name === previous.name) {
      destinationName = id === CUSTOM_STREAM_SERVICE_ID ? '' : next.name;
    }
  }

  function openServiceHelp() {
    if (service.helpUrl) void openUrl(service.helpUrl).catch(() => {});
  }

  async function deleteDestination() {
    if (usingNew) return;
    const id = destinationId;
    try {
      await invoke(COMMANDS.streamDestinationDelete, { id });
      destinationId = NEW_DESTINATION;
      await loadDestinations();
    } catch (e) {
      error = String(e);
    }
  }

  async function goLive() {
    if (!canGoLive || maxBitrateBps === undefined) return;
    busy = true;
    error = null;
    const request: GoLiveRequest = {
      windowId,
      destination: usingNew
        ? {
            kind: 'custom',
            serverUrl: serverUrl.trim(),
            streamKey: streamKey.trim(),
            save: saveDestination,
            name: destinationName.trim()
          }
        : { kind: 'saved', id: destinationId },
      resolution,
      orientation,
      maxBitrateBps
    };
    try {
      await invoke(COMMANDS.streamGoLive, { request });
      // Drop the key from memory before the window goes.
      streamKey = '';
      if (hasTauriBridge()) void getCurrentWindow().close();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function close() {
    streamKey = '';
    if (hasTauriBridge()) void getCurrentWindow().close();
  }

  // Streamer mode: destinations show by name only, and the server URL is
  // masked like the key (some services' URLs identify the channel).
  $effect(() => watchStreamerMode());

  onMount(() => {
    if (!hasTauriBridge()) return;
    void invoke<number>(COMMANDS.goLiveTarget).then((id) => {
      windowId = id;
    });
    void loadDestinations();
    let unlisten: (() => void) | undefined;
    listenUntilDestroy<number>(
      EVENTS.goLiveTarget,
      (event) => {
        // Also how a hidden dialog comes back (macOS reuses it): going live may
        // have saved a destination, and the share may have changed size.
        windowId = event.payload;
        error = null;
        void loadDestinations();
        void refreshPreview();
      },
      (fn) => {
        unlisten = fn;
      },
      () => destroyed
    );
    return () => {
      destroyed = true;
      unlisten?.();
    };
  });
</script>

<main>
  <h1>Go live</h1>
  <p class="lede">Stream this share, with the meeting’s audio, to a live-streaming service.</p>

  <form
    onsubmit={(event) => {
      event.preventDefault();
      void goLive();
    }}
  >
    <fieldset>
      <legend>Destination</legend>
      <div class="row">
        <select bind:value={destinationId} aria-label="Destination">
          {#each destinations as destination (destination.id)}
            <option value={destination.id}>{destination.name}</option>
          {/each}
          <option value={NEW_DESTINATION}>New destination…</option>
        </select>
        {#if !usingNew}
          <button type="button" class="link" onclick={deleteDestination}>Delete</button>
        {/if}
      </div>
      {#if usingNew}
        <label>
          Service
          <select
            value={serviceId}
            onchange={(event) => selectService(event.currentTarget.value)}
          >
            {#each STREAM_SERVICES as option (option.id)}
              <option value={option.id}>{option.name}</option>
            {/each}
          </select>
        </label>
        <label>
          Server URL
          <input
            type={streamerMode.active ? 'password' : 'url'}
            placeholder="rtmps://live.example.com/app"
            bind:value={serverUrl}
            autocomplete="off"
            spellcheck="false"
          />
        </label>
        {#if service.hint}
          <p class="hint">{service.hint}</p>
        {/if}
        <label>
          Stream key
          <input type="password" bind:value={streamKey} autocomplete="off" spellcheck="false" />
        </label>
        {#if service.helpUrl}
          <button type="button" class="link help" onclick={openServiceHelp}>
            Where do I find my {service.name} stream key?
          </button>
        {/if}
        <label class="check">
          <input type="checkbox" bind:checked={saveDestination} />
          Save this destination once the stream connects (the key goes to the system keychain)
        </label>
        {#if saveDestination}
          <label>
            Name
            <input type="text" placeholder="Optional" bind:value={destinationName} maxlength="64" />
          </label>
        {/if}
      {/if}
      {#if destinationService?.goLiveHint}
        <p class="hint">{destinationService.goLiveHint}</p>
      {/if}
    </fieldset>

    <fieldset>
      <legend>Resolution</legend>
      <div class="segmented" role="radiogroup" aria-label="Resolution">
        {#each RESOLUTION_CHOICES as option (option.value)}
          <label class:selected={resolution === option.value}>
            <input type="radio" bind:group={resolution} value={option.value} />
            {option.label}
          </label>
        {/each}
      </div>
    </fieldset>

    <fieldset>
      <legend>Shape</legend>
      <div class="segmented" role="radiogroup" aria-label="Shape">
        {#each ORIENTATION_CHOICES as option (option.value)}
          <label class:selected={orientation === option.value}>
            <input type="radio" bind:group={orientation} value={option.value} />
            {option.label}
          </label>
        {/each}
      </div>
      <p class="hint">
        The stream keeps this size throughout. A share of another shape, or one resized while live,
        is scaled to fit with black bars.
      </p>
    </fieldset>

    <fieldset>
      <legend>Max bitrate</legend>
      <label class="inline">
        <input
          type="text"
          inputmode="decimal"
          placeholder="Auto"
          bind:value={bitrateInput}
          aria-invalid={maxBitrateBps === undefined}
        />
        Mbps
      </label>
      {#if maxBitrateBps === undefined}
        <p class="hint error-text">Enter 1 to 50 Mbps, or leave blank for Auto.</p>
      {/if}
    </fieldset>

    {#if preview}
      <p class="preview">{streamPreviewLine(preview)}</p>
      {#if streamIsDownscaled(preview)}
        <p class="hint">The share is larger than this resolution, so it’s scaled down to fit.</p>
      {/if}
      {#if preview.plan.fps < preview.shareFps}
        <p class="hint">
          The frame rate can’t exceed what the encoder level allows at this resolution.
        </p>
      {/if}
      {#each streamClientHints(preview) as hint (hint)}
        <p class="hint">{hint}</p>
      {/each}
    {/if}

    {#if error}
      <p class="error-text" role="alert">{error}</p>
    {/if}

    <div class="actions">
      <button type="button" class="secondary" onclick={close}>Cancel</button>
      <button type="submit" class="primary" disabled={!canGoLive}>
        {busy ? 'Connecting…' : 'Go live'}
      </button>
    </div>
  </form>
</main>

<style>
  main {
    box-sizing: border-box;
    height: 100%;
    overflow-y: auto;
    padding: 20px 22px;
    background: var(--bg-base-2);
    color: var(--text-strong);
    font-family: var(--font-ui);
    font-size: var(--text-body);
  }
  h1 {
    margin: 0 0 4px;
    font-family: var(--font-display);
    font-size: 20px;
  }
  .lede {
    margin: 0 0 16px;
    color: var(--text-dim);
  }
  form {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  fieldset {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin: 0;
    padding: 0;
    border: 0;
  }
  legend {
    margin-bottom: 6px;
    color: var(--text-soft);
    font-size: var(--text-caption);
    font-weight: 600;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    color: var(--text-soft);
  }
  label.check,
  label.inline {
    flex-direction: row;
    align-items: center;
    gap: 8px;
  }
  .row {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .row select {
    flex: 1;
  }
  input[type='text'],
  input[type='url'],
  input[type='password'],
  select {
    box-sizing: border-box;
    min-width: 0;
    padding: 8px 10px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-input);
    background: var(--fill-base);
    color: var(--text-strong);
    font: inherit;
  }
  /* The open drop-down list is drawn by the OS from these colours; a
     translucent background there renders near-white text on near-white, so
     only the highlighted row was readable. Keep both opaque. */
  select {
    background: var(--surface-2);
  }
  option {
    background: var(--surface-raised);
    color: var(--text-strong);
  }
  label.inline input {
    width: 96px;
  }
  input[aria-invalid='true'] {
    border-color: var(--danger);
  }
  .segmented {
    display: flex;
    gap: 6px;
  }
  .segmented label {
    flex: 1;
    flex-direction: row;
    justify-content: center;
    padding: 7px 0;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    cursor: pointer;
  }
  .segmented label.selected {
    background: var(--fill-bright);
    color: var(--text-primary);
  }
  .segmented input {
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }
  .hint {
    margin: 0;
    color: var(--text-dim);
    font-size: var(--text-caption);
  }
  .preview {
    margin: 0;
    padding: 10px 12px;
    border-radius: var(--radius-control);
    background: var(--fill-weak);
    font-family: var(--font-mono);
    font-size: var(--text-micro);
  }
  .error-text {
    margin: 0;
    color: var(--danger);
    font-size: var(--text-caption);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 4px;
  }
  button {
    padding: 8px 16px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    font: inherit;
    font-size: var(--text-label);
    font-weight: var(--weight-btn);
    cursor: pointer;
  }
  button.secondary {
    background: transparent;
    color: var(--text-soft);
  }
  button.primary {
    border-color: transparent;
    background: var(--danger);
    color: var(--text-primary);
  }
  button:disabled {
    opacity: var(--disabled-opacity);
    cursor: default;
  }
  button.link {
    padding: 4px 6px;
    border: 0;
    background: transparent;
    color: var(--text-dim);
    font-weight: 500;
  }
  button.link.help {
    align-self: flex-start;
    padding: 0;
    color: var(--id-blue);
  }
  button:focus-visible,
  input:focus-visible,
  select:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }
</style>
