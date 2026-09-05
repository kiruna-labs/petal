<script lang="ts">
  import { onMount } from 'svelte';
  import { listAudioDevices, setAudioDevices } from '$lib/data/audioDevices';
  import { listCameraDevices, setCameraDevice } from '$lib/data/cameraDevices';
  import { session, updateAudioDevices } from '$lib/stores/session.svelte';
  import { hasTauriBridge } from '$lib/ipc';

  interface DeviceOption {
    id: string;
    label: string;
  }

  interface Props {
    mode: 'audio' | 'camera';
    onClose?: () => void;
  }

  let { mode, onClose }: Props = $props();

  let mics = $state<DeviceOption[]>([]);
  let speakers = $state<DeviceOption[]>([]);
  let cameras = $state<DeviceOption[]>([]);
  let selectedMic = $state(session.micDeviceId || '');
  let selectedSpeaker = $state(session.speakerDeviceId || '');
  let selectedCamera = $state(session.cameraDeviceId || '');
  let loading = $state(true);
  let deviceError = $state<string | null>(null);
  let micNote = $state<string | null>(null);
  let speakerNote = $state<string | null>(null);
  let cameraNote = $state<string | null>(null);
  let pendingKind = $state<'microphone' | 'speaker' | 'camera' | null>(null);
  let root = $state<HTMLDivElement>();

  const micValue = $derived(
    mics.some((device) => device.id === selectedMic) ? selectedMic : (mics[0]?.id ?? '')
  );
  const speakerValue = $derived(
    speakers.some((device) => device.id === selectedSpeaker)
      ? selectedSpeaker
      : (speakers[0]?.id ?? '')
  );
  const cameraValue = $derived(
    cameras.some((device) => device.id === selectedCamera)
      ? selectedCamera
      : (cameras[0]?.id ?? '')
  );

  const title = $derived(mode === 'audio' ? 'Audio devices' : 'Camera');

  onMount(async () => {
    if (!hasTauriBridge()) {
      deviceError = 'Device switching is unavailable in this preview.';
      loading = false;
      return;
    }

    try {
      const [audio, camera] = await Promise.all([listAudioDevices(), listCameraDevices()]);
      mics = (audio?.recording ?? []).map((device) => ({ id: device.id, label: device.name }));
      speakers = (audio?.playout ?? []).map((device) => ({ id: device.id, label: device.name }));
      cameras = (camera ?? []).map((device) => ({ id: device.id, label: device.name }));
    } catch (error) {
      console.error('device picker: enumeration failed', error);
      deviceError = 'Could not list devices.';
    } finally {
      loading = false;
    }
  });

  function noteFor(applied: { applied: boolean; inRoom: boolean } | null, what: string) {
    if (!applied) return null;
    if (applied.applied) return `Switched ${what}`;
    if (!applied.inRoom) return 'Saved — applies when you join a room';
    return null;
  }

  async function handleMicSelect(id: string) {
    selectedMic = id;
    updateAudioDevices(id, undefined);
    micNote = null;
    pendingKind = 'microphone';
    try {
      const applied = await setAudioDevices({ recordingId: id });
      micNote = noteFor(
        applied ? { applied: applied.micApplied, inRoom: applied.inRoom } : null,
        'microphone'
      );
    } catch (error) {
      console.error('device picker: mic switch failed', error);
      micNote = 'Could not switch microphone.';
    } finally {
      pendingKind = null;
    }
  }

  async function handleSpeakerSelect(id: string) {
    selectedSpeaker = id;
    updateAudioDevices(undefined, id);
    speakerNote = null;
    pendingKind = 'speaker';
    try {
      const applied = await setAudioDevices({ playoutId: id });
      speakerNote = noteFor(
        applied ? { applied: applied.speakerApplied, inRoom: applied.inRoom } : null,
        'speaker'
      );
    } catch (error) {
      console.error('device picker: speaker switch failed', error);
      speakerNote = 'Could not switch speaker.';
    } finally {
      pendingKind = null;
    }
  }

  async function handleCameraSelect(id: string) {
    selectedCamera = id;
    updateAudioDevices(undefined, undefined, id);
    cameraNote = null;
    pendingKind = 'camera';
    try {
      const applied = await setCameraDevice(id);
      cameraNote = noteFor(
        applied ? { applied: applied.applied, inRoom: applied.inRoom } : null,
        'camera'
      );
    } catch (error) {
      console.error('device picker: camera switch failed', error);
      cameraNote = 'Could not switch camera.';
    } finally {
      pendingKind = null;
    }
  }

  function optionButtons(kind: 'microphone' | 'speaker' | 'camera') {
    return Array.from(
      root?.querySelectorAll<HTMLButtonElement>(`button[data-device-kind="${kind}"]`) ?? []
    );
  }

  function handleOptionKeydown(
    kind: 'microphone' | 'speaker' | 'camera',
    index: number,
    event: KeyboardEvent
  ) {
    if (event.key === 'Escape') {
      event.preventDefault();
      onClose?.();
      return;
    }
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    const buttons = optionButtons(kind);
    if (buttons.length === 0) return;
    event.preventDefault();
    const next = event.key === 'Home'
      ? 0
      : event.key === 'End'
        ? buttons.length - 1
        : (index + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length;
    buttons[next]?.focus();
  }

  function handleKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      event.preventDefault();
      onClose?.();
    }
  }
</script>

<div
  bind:this={root}
  class="device-picker"
  role="dialog"
  aria-label={title}
  tabindex="-1"
  onkeydown={handleKeydown}
>
  <div class="device-picker-heading">
    <p class="device-picker-title">{title}</p>
    <p class="device-picker-hint">Choose what others hear and see.</p>
  </div>

  {#if deviceError}
    <p class="meeting-menu-status" role="alert">{deviceError}</p>
  {:else if loading}
    <p class="meeting-menu-status" aria-live="polite">Loading devices…</p>
  {:else if mode === 'audio'}
    <section class="device-section" aria-labelledby="meeting-microphone-label">
      <h3 id="meeting-microphone-label" class="meeting-menu-section-label">Microphone</h3>
      {#if mics.length === 0}
        <p class="meeting-menu-status">No microphones found.</p>
      {:else}
        <div class="device-list" role="listbox" aria-label="Microphones">
          {#each mics as device, index (device.id)}
            <button
              type="button"
              class="meeting-menu-row device-row"
              class:selected={device.id === micValue}
              role="option"
              aria-selected={device.id === micValue}
              aria-label={`${device.label}${device.id === micValue ? ', selected' : ''}`}
              data-device-kind="microphone"
              disabled={pendingKind === 'microphone'}
              onclick={() => void handleMicSelect(device.id)}
              onkeydown={(event) => handleOptionKeydown('microphone', index, event)}
            >
              <span class="meeting-menu-row-copy">{device.label}</span>
              {#if device.id === micValue}
                <span class="meeting-menu-row-check" aria-hidden="true">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round">
                    <path d="m5 12 4 4L19 6"></path>
                  </svg>
                </span>
              {/if}
            </button>
          {/each}
        </div>
      {/if}
      {#if micNote}
        <p class="meeting-menu-status" role="status" aria-live="polite">{micNote}</p>
      {/if}
    </section>

    <section class="device-section" aria-labelledby="meeting-speaker-label">
      <h3 id="meeting-speaker-label" class="meeting-menu-section-label">Speaker</h3>
      {#if speakers.length === 0}
        <p class="meeting-menu-status">No speakers found.</p>
      {:else}
        <div class="device-list" role="listbox" aria-label="Speakers">
          {#each speakers as device, index (device.id)}
            <button
              type="button"
              class="meeting-menu-row device-row"
              class:selected={device.id === speakerValue}
              role="option"
              aria-selected={device.id === speakerValue}
              aria-label={`${device.label}${device.id === speakerValue ? ', selected' : ''}`}
              data-device-kind="speaker"
              disabled={pendingKind === 'speaker'}
              onclick={() => void handleSpeakerSelect(device.id)}
              onkeydown={(event) => handleOptionKeydown('speaker', index, event)}
            >
              <span class="meeting-menu-row-copy">{device.label}</span>
              {#if device.id === speakerValue}
                <span class="meeting-menu-row-check" aria-hidden="true">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round">
                    <path d="m5 12 4 4L19 6"></path>
                  </svg>
                </span>
              {/if}
            </button>
          {/each}
        </div>
      {/if}
      {#if speakerNote}
        <p class="meeting-menu-status" role="status" aria-live="polite">{speakerNote}</p>
      {/if}
    </section>
  {:else}
    <section class="device-section">
      {#if cameras.length === 0}
        <p class="meeting-menu-status">No cameras found.</p>
      {:else}
        <div class="device-list" role="listbox" aria-label="Cameras">
          {#each cameras as device, index (device.id)}
            <button
              type="button"
              class="meeting-menu-row device-row"
              class:selected={device.id === cameraValue}
              role="option"
              aria-selected={device.id === cameraValue}
              aria-label={`${device.label}${device.id === cameraValue ? ', selected' : ''}`}
              data-device-kind="camera"
              disabled={pendingKind === 'camera'}
              onclick={() => void handleCameraSelect(device.id)}
              onkeydown={(event) => handleOptionKeydown('camera', index, event)}
            >
              <span class="meeting-menu-row-copy">{device.label}</span>
              {#if device.id === cameraValue}
                <span class="meeting-menu-row-check" aria-hidden="true">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round">
                    <path d="m5 12 4 4L19 6"></path>
                  </svg>
                </span>
              {/if}
            </button>
          {/each}
        </div>
      {/if}
      {#if cameraNote}
        <p class="meeting-menu-status" role="status" aria-live="polite">{cameraNote}</p>
      {/if}
    </section>
  {/if}
</div>

<style>
  .device-picker {
    display: flex;
    flex-direction: column;
    gap: 8px;
    width: min(300px, calc(100vw - 16px));
    max-height: var(--device-menu-max-height, none);
    min-height: 0;
    box-sizing: border-box;
    padding: 12px;
    overflow-y: auto;
    overscroll-behavior: contain;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-popover);
    background: var(--popover-bg);
    box-shadow: var(--shadow-panel);
    color: var(--text-strong);
  }

  .device-picker-heading {
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 2px 4px 4px;
  }

  .device-picker-title {
    margin: 0;
    color: var(--text-primary);
    font: 700 var(--text-body) / 1.2 var(--font-ui);
  }

  .device-picker-hint {
    margin: 0;
    color: var(--text-dim);
    font: 500 var(--text-micro) / 1.25 var(--font-ui);
  }

  .device-section {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .device-section + .device-section {
    padding-top: 8px;
    border-top: 1px solid var(--hairline);
  }

  .device-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .device-row {
    min-height: 44px;
    text-align: left;
  }

  .device-row.selected {
    background: var(--fill-weak);
  }

  .device-row:disabled {
    opacity: 0.72;
  }

  .meeting-menu-row-check svg {
    width: 16px;
    height: 16px;
  }
</style>
