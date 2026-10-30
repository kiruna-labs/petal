<!--
  LIVE indicator for the meeting window. Everyone sees a LIVE badge while
  the room is being streamed. The person streaming from this computer also
  sees elapsed time, bitrate, the "Be right back" countdown, and Stop.
  Everyone else gets "Keep me off live streams", the same setting as in
  Settings: it keeps their audio out of the stream. Resuming on another
  share lives in that share's options menu.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount } from 'svelte';
  import {
    COMMANDS,
    EVENTS,
    hasTauriBridge,
    type StreamLiveState,
    type StreamSessionSnapshot,
    type StreamSettings
  } from '#lib/ipc.js';
  import {
    formatStreamElapsed,
    showsOptOutToggle,
    streamStatusLine
  } from '#lib/data/liveStream.js';

  let room = $state<StreamLiveState | null>(null);
  let own = $state<StreamSessionSnapshot | null>(null);
  let now = $state(Date.now());
  let stopping = $state(false);
  let settings = $state<StreamSettings | null>(null);
  let savingOptOut = $state(false);

  const roomLive = $derived(room?.live ?? false);
  const optOutVisible = $derived(settings !== null && showsOptOutToggle(roomLive, own !== null));
  const visible = $derived(own !== null || roomLive);
  const label = $derived.by(() => {
    if (own) return streamStatusLine(own, now);
    if (room?.paused) return 'LIVE · Be right back';
    return room?.startedAt ? `LIVE ${formatStreamElapsed(now - room.startedAt)}` : 'LIVE';
  });

  async function stop() {
    stopping = true;
    try {
      await invoke(COMMANDS.streamStop);
    } catch (error) {
      console.error('stopping the live stream failed', error);
    } finally {
      stopping = false;
    }
  }

  async function setKeepOff(keepOff: boolean) {
    if (!settings) return;
    savingOptOut = true;
    try {
      settings = await invoke<StreamSettings>(COMMANDS.streamSettingsSet, {
        settings: { ...settings, keepOffLiveStreams: keepOff }
      });
    } catch (error) {
      console.error('saving the live stream opt-out failed', error);
    } finally {
      savingOptOut = false;
    }
  }

  onMount(() => {
    if (!hasTauriBridge()) return;
    void invoke<StreamLiveState>(COMMANDS.streamLiveState).then((state) => (room = state));
    void invoke<StreamSettings>(COMMANDS.streamSettings)
      .then((value) => (settings = value))
      .catch(() => {});
    void invoke<StreamSessionSnapshot | null>(COMMANDS.streamSessionState).then(
      (snapshot) => (own = snapshot)
    );
    const unLive = listen<StreamLiveState>(EVENTS.streamLiveChanged, (e) => (room = e.payload));
    const unSession = listen<StreamSessionSnapshot>(
      EVENTS.streamSessionChanged,
      (e) => (own = e.payload)
    );
    const unEnded = listen(EVENTS.streamSessionEnded, () => (own = null));
    const tick = setInterval(() => (now = Date.now()), 1000);
    return () => {
      clearInterval(tick);
      for (const un of [unLive, unSession, unEnded]) un.then((u) => u()).catch(() => {});
    };
  });
</script>

{#if visible}
  <div class="live-banner" class:paused={own?.phase === 'paused' || room?.paused} role="status">
    <span class="dot" aria-hidden="true"></span>
    <span class="text">{label}</span>
    {#if own}
      <button type="button" onclick={stop} disabled={stopping}>Stop</button>
    {:else if optOutVisible && settings}
      <label class="opt-out">
        <input
          type="checkbox"
          checked={settings.keepOffLiveStreams}
          disabled={savingOptOut}
          onchange={(event) => setKeepOff(event.currentTarget.checked)}
        />
        Keep me off live streams
      </label>
    {/if}
  </div>
{/if}

<style>
  .live-banner {
    position: absolute;
    top: 10px;
    left: 50%;
    transform: translateX(-50%);
    z-index: 25;
    display: flex;
    align-items: center;
    gap: 8px;
    max-width: calc(100% - 32px);
    padding: 5px 6px 5px 10px;
    border-radius: var(--radius-pill);
    background: var(--surface-raised);
    box-shadow:
      inset 0 0 0 1px var(--danger-tint-25),
      var(--shadow-pill);
    color: var(--text-strong);
    font-family: var(--font-ui);
    font-size: var(--text-caption);
    font-weight: 600;
  }
  .dot {
    flex: none;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--danger);
  }
  .paused .dot {
    background: var(--warning);
  }
  .text {
    overflow-wrap: anywhere;
  }
  button {
    flex: none;
    padding: 3px 10px;
    border: 0;
    border-radius: var(--radius-pill);
    background: var(--danger-tint-16);
    color: var(--danger);
    font: inherit;
    cursor: pointer;
  }
  button:disabled {
    opacity: var(--disabled-opacity);
  }
  button:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }
  .opt-out {
    flex: none;
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 3px 10px 3px 6px;
    font-weight: 500;
    cursor: pointer;
  }
  .opt-out input {
    margin: 0;
    accent-color: var(--danger);
  }
  .opt-out input:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }
</style>
