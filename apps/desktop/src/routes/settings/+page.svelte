<!--
  Real Settings screen, rendered in its OWN Tauri window (`settings`, see
  src-tauri/src/settings_window.rs) opened from the home profile menu, the
  menubar popover, and the in-meeting "More" menu. Renders the existing
  `Settings` component with the current frontend-only session identity
  (name/color) bound so edits here persist the same way onboarding's
  IdentitySetup does (see src/lib/stores/session.svelte.ts, which also
  broadcasts every change to the other webviews).

  Being a separate window is the point: the old in-window route had to be
  reached by navigating the main webview, which tore down a live meeting
  route (#782). Closing this window closes it -- nothing here ever routes the
  main webview anywhere.

  Mic/speaker device lists are real native enumeration (issue #28);
  Settings.svelte loads them via `list_audio_devices`, while this route
  supplies and persists the selected IDs.
-->
<script lang="ts">
  import { goto } from '$app/navigation';
  import { getCurrentWindow } from '@tauri-apps/api/window';
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
  import { hasTauriBridge } from '$lib/ipc';

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

  // Close THIS window only. Browser preview has no native window, so it
  // falls back to the home route.
  function closeWindow() {
    if (hasTauri) {
      void getCurrentWindow().close();
    } else {
      goto('/main');
    }
  }

  function handleNameChange(name: string) {
    updateIdentity(name, session.identity);
  }

  function handleIdentityChange(identity: typeof session.identity) {
    updateIdentity(session.name, identity);
  }
</script>

<main>
  <Settings
    frameless
    onClose={closeWindow}
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
</style>
