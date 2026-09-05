<!--
  Real Settings screen (task brief item 3), reached from MainMenu's new
  settings icon button. Renders the existing `Settings` component as-is —
  no internals touched — with the current frontend-only session identity
  (name/color) bound so edits here persist the same way onboarding's
  IdentitySetup does (see src/lib/stores/session.svelte.ts).

  Mic/speaker device lists are real native enumeration now (issue #28);
  Settings.svelte loads them via `list_audio_devices`, while this route
  supplies and persists the selected IDs. A simple "Back" affordance returns
  to the joined meeting when present, or `/main` otherwise, since Settings has
  no built-in close/back control of its own.
-->
<script lang="ts">
  import { goto } from '$app/navigation';
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';
  import Settings from '$lib/components/Settings.svelte';
  import type { PermissionStatus } from '$lib/components/PermissionRow.svelte';
  import {
    session,
    updateIdentity,
    updateRemoteControlPolicy,
    updateLocalEchoEnabled,
    updateSentryEnabled
  } from '$lib/stores/session.svelte';
  import {
    checkScreenRecording,
    checkMicrophone,
    checkCamera,
    checkAccessibility,
    type AuthStatus
  } from '$lib/data/permissions';
  import { COMMANDS, hasTauriBridge } from '$lib/ipc';

  const displayName = $derived(session.name || 'Guest');
  const hasTauri = hasTauriBridge();

  // Real permission statuses for the re-entry rows (issue #8) — these
  // previously fell back to Settings' hardcoded 'enabled' defaults, so the
  // rows claimed everything was granted even when TCC said denied. Seeded
  // once on mount; the camera row is additionally kept live by Settings'
  // own preview gate after that.
  // Neutral, non-granted placeholders until the real TCC checks resolve on
  // mount (audit #129). Previously defaulted to 'enabled', so the rows briefly
  // (or, if a check hung, indefinitely) claimed everything was granted — the
  // exact "rows lie when denied" class this screen is meant to avoid.
  let screenStatus = $state<PermissionStatus>('up-next');
  let micStatus = $state<PermissionStatus>('up-next');
  let camStatus = $state<PermissionStatus>('optional');
  let accessibilityStatus = $state<PermissionStatus>('up-next');

  function rowStatusFromAuth(auth: AuthStatus, undecided: PermissionStatus): PermissionStatus {
    if (auth === 'authorized') return 'enabled';
    if (auth === 'denied' || auth === 'restricted') return 'denied';
    return undecided; // not-determined
  }

  // #782: read the room at CLICK time, never a mount-time snapshot. Leaving
  // from the menubar popover while Settings is open would otherwise send Back
  // to a room the user just left -- and `join_room`'s publish carryover would
  // silently re-enable their camera on the way in.
  async function currentJoinedRoom(): Promise<string | null> {
    if (!hasTauri) return null;
    try {
      return await invoke<string | null>(COMMANDS.currentRoom);
    } catch {
      return null;
    }
  }

  onMount(async () => {
    try {
      const [screen, mic, cam, accessibility] = await Promise.all([
        checkScreenRecording(),
        checkMicrophone(),
        checkCamera(),
        checkAccessibility()
      ]);
      screenStatus = screen ? 'enabled' : 'denied';
      micStatus = rowStatusFromAuth(mic, 'up-next');
      camStatus = rowStatusFromAuth(cam, 'optional');
      accessibilityStatus = accessibility ? 'enabled' : 'denied';
    } catch (e) {
      // The check_* wrappers self-catch today, so this is defensive: on an
      // unexpected failure leave the neutral placeholders rather than claiming
      // everything is granted (audit #129).
      console.error('Failed to load permission statuses on /settings', e);
    }
  });

  async function handleBack() {
    const room = await currentJoinedRoom();
    goto(room ? `/meeting/${encodeURIComponent(room)}` : '/main');
  }

  function handleNameChange(name: string) {
    updateIdentity(name, session.identity);
  }

  function handleIdentityChange(identity: typeof session.identity) {
    updateIdentity(session.name, identity);
  }
</script>

<main>
  <div class="back-row">
    <button type="button" class="back" onclick={handleBack}>
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M15 18l-6-6 6-6"></path>
      </svg>
      Back
    </button>
  </div>

  <Settings
    frameless
    userName={displayName}
    identity={session.identity}
    screenRecordingStatus={screenStatus}
    micStatus={micStatus}
    cameraStatus={camStatus}
    {accessibilityStatus}
    onNameChange={handleNameChange}
    onIdentityChange={handleIdentityChange}
    selectedMic={session.micDeviceId}
    selectedSpeaker={session.speakerDeviceId}
    selectedCamera={session.cameraDeviceId}
    remoteControlPolicy={session.remoteControlPolicy}
    onRemoteControlPolicyChange={updateRemoteControlPolicy}
    localEchoEnabled={session.localEchoEnabled}
    onLocalEchoEnabledChange={updateLocalEchoEnabled}
    sentryEnabled={session.sentryEnabled}
    onSentryEnabledChange={updateSentryEnabled}
  />
</main>

<style>
  main {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: stretch;
    height: 100%;
    width: 100%;
    overscroll-behavior: none;
    /* The panel IS the window (frameless Settings) — match its surface so
       there's no visible outer frame. */
    background: var(--bg-base-2);
  }

  .back-row {
    position: absolute;
    top: 5px;
    right: 8px;
    z-index: 3;
    padding: 0;
  }

  .back {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-height: 40px;
    padding: 6px 10px 6px 6px;
    border-radius: var(--radius-pill);
    border: none;
    background: transparent;
    color: var(--text-dim);
    font: 500 12.5px var(--font-ui);
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .back:hover {
    background: var(--fill-base);
    color: var(--text-strong);
  }

  .back:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .back:active {
    transform: scale(var(--press-scale, 0.96));
  }
</style>
