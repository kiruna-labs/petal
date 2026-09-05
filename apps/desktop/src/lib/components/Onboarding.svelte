<!--
  Onboarding — permissions step of the first-run flow. One window, a vertical
  checklist: Screen Recording → Microphone → Camera → Accessibility, with each
  request started only by the user's row action. Identity is collected on the
  next screen.

  Fully presentational: the caller drives which state renders via
  `screenRecordingStatus`, `micStatus`, `cameraStatus`, and
  `accessibilityStatus`; real permission polling/deep-link/relaunch wiring
  lives in the route. The route may temporarily disable Continue while an
  auto-relaunch has been requested for a freshly granted TCC permission.

  Progress counter ("N of 3 required ready") counts only permissions required
  to enter the app. Camera is optional because users can join with it off.
-->
<script lang="ts">
  import Wordmark from './Wordmark.svelte';
  import Logo from './Logo.svelte';
  import Button from './Button.svelte';
  import { isMac } from '$lib/platform';
  import PermissionRow from './PermissionRow.svelte';
  import type { PermissionStatus } from './PermissionRow.svelte';

  export type ScreenRecordingStatus = 'in-progress' | 'denied' | 'enabled';
  export type MicStatus = 'up-next' | 'blocked' | 'denied' | 'enabled';
  export type CameraStatus = 'up-next' | 'blocked' | 'denied' | 'enabled';
  export type AccessibilityStatus = 'up-next' | 'blocked' | 'denied' | 'repair' | 'enabled';

  interface Props {
    screenRecordingStatus?: ScreenRecordingStatus;
    micStatus?: MicStatus;
    cameraStatus?: CameraStatus;
    accessibilityStatus?: AccessibilityStatus;
    onOpenSettings?: () => void;
    onRequestMicrophone?: () => void;
    onRequestCamera?: () => void;
    onRequestAccessibility?: () => void;
    onConfirmAccessibilityRepairRestart?: () => void;
    accessibilityRepairSettingsOpened?: boolean;
    accessibilityRepairRestartFailed?: boolean;
    onRecheckAccessibility?: () => void;
    onContinue?: () => void;
    continueDisabled?: boolean;
    /** Real routes pass true so the checklist IS the window (edge-to-edge,
     * no fixed-width floating card). Default false preserves the card look
     * for the /dev/* harnesses. */
    frameless?: boolean;
  }

  let {
    screenRecordingStatus = 'in-progress',
    micStatus = 'up-next',
    cameraStatus = 'up-next',
    accessibilityStatus = 'up-next',
    onOpenSettings,
    onRequestMicrophone,
    onRequestCamera,
    onRequestAccessibility,
    onConfirmAccessibilityRepairRestart,
    accessibilityRepairSettingsOpened = false,
    accessibilityRepairRestartFailed = false,
    onRecheckAccessibility,
    onContinue,
    continueDisabled = false,
    frameless = false
  }: Props = $props();

  const requiredReadyCount = $derived(
      (screenRecordingStatus === 'enabled' ? 1 : 0) +
      (micStatus === 'enabled' ? 1 : 0) +
      (accessibilityStatus === 'enabled' ? 1 : 0)
  );
  const allRequiredReady = $derived(requiredReadyCount === 3);
  const micRowStatus = $derived<PermissionStatus>(
    micStatus === 'enabled'
      ? 'enabled'
      : screenRecordingStatus !== 'enabled'
        ? 'blocked'
        : micStatus === 'up-next'
          ? 'in-progress'
          : micStatus
  );
  const cameraRowStatus = $derived<PermissionStatus>(
    cameraStatus === 'enabled'
      ? 'enabled'
      : screenRecordingStatus !== 'enabled' || micStatus !== 'enabled'
        ? 'blocked'
        : cameraStatus === 'up-next'
          ? 'in-progress'
          : cameraStatus
  );
  const accessibilityRowStatus = $derived<PermissionStatus>(
    accessibilityStatus === 'enabled'
      ? 'enabled'
      : screenRecordingStatus !== 'enabled' || micStatus !== 'enabled'
        ? 'blocked'
        : accessibilityStatus === 'up-next'
          ? 'in-progress'
          : accessibilityStatus
  );
</script>

<div class="onboarding" class:frameless>
  <div class="header">
    <div class="mark">
      <Logo size={12} />
      <Wordmark size={14} />
    </div>
    {#if isMac()}
    <span class="progress" class:success={allRequiredReady}>{requiredReadyCount} of 3 required ready</span>
    {/if}
  </div>

  <!-- The whole checklist is macOS-gated: Windows has no TCC permission
       model, the permission stubs always report granted, and the rows would
       be dead "enabled" entries. The Continue button stays on all platforms. -->
  {#if isMac()}
  <PermissionRow
    icon="screen"
    title="Screen Recording"
    required
    why="Petal can only share the windows you choose. Nothing else is visible unless you explicitly share it."
    status={screenRecordingStatus === 'enabled' ? 'enabled' : screenRecordingStatus}
    actionLabel="Set up Screen Recording"
    onOpenSettings={onOpenSettings}
  />

  <PermissionRow
    icon="mic"
    title="Microphone"
    required
    why="Use your microphone when you join a room, so teammates can hear you clearly."
    status={micRowStatus}
    actionLabel="Allow Microphone"
    onOpenSettings={onRequestMicrophone}
  />

  <PermissionRow
    icon="camera"
    title="Camera"
    why="Let teammates see you on camera when you want; you can always join with it off."
    status={cameraRowStatus}
    actionLabel="Allow Camera"
    onOpenSettings={onRequestCamera}
  />

  <PermissionRow
    icon="accessibility"
    title="Accessibility"
    required
    why="Allow Petal to replay approved remote-control clicks and keystrokes into shared windows."
    status={accessibilityRowStatus}
    actionLabel={accessibilityStatus === 'repair' ? 'Open Accessibility Settings' : 'Allow Accessibility'}
    onOpenSettings={onRequestAccessibility}
    repairSettingsOpened={accessibilityRepairSettingsOpened}
    repairRestartFailed={accessibilityRepairRestartFailed}
    onConfirmRepairRestart={onConfirmAccessibilityRepairRestart}
    onRecheck={onRecheckAccessibility}
  />
  {/if}

  <div class="footer">
    <Button variant="primary" fullWidth disabled={!allRequiredReady || continueDisabled} onclick={onContinue}>
      Continue
    </Button>
  </div>
</div>

<style>
  .onboarding {
    display: flex;
    flex-direction: column;
    gap: 6px;
    width: 380px;
    min-height: 560px;
    padding: 22px;
    box-sizing: border-box;
    background: var(--bg-base-2);
    border-radius: var(--radius-menu);
    border: 1px solid var(--fill-strong);
    overscroll-behavior: none;
  }

  /* Frameless: the checklist IS the window — no card frame, fills the route.
     Inner padding (22px) is kept so content still breathes at the edges. */
  .onboarding.frameless {
    width: 100%;
    flex: 1;
    min-height: 0;
    border-radius: 0;
    border: none;
  }

  .header {
    display: flex;
    align-items: center;
    height: 50px;
    padding: 0;
    box-sizing: border-box;
    margin-bottom: 16px;
  }

  .mark {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .progress {
    margin-left: auto;
    padding: 5px 8px;
    border-radius: var(--radius-pill);
    font: 500 11px var(--font-mono);
    color: var(--text-faint);
    font-variant-numeric: tabular-nums;
    background: transparent;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .progress.success {
    color: var(--live-bright);
    background: var(--live-tint);
    animation: progress-ready-pop var(--motion-base) var(--spring) both;
  }

  .footer {
    margin-top: auto;
    padding-top: 16px;
    border-top: 1px solid var(--hairline);
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .footer :global(.btn) {
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .footer :global(.btn:active:not(:disabled)) {
    transform: scale(var(--press-scale, 0.96));
  }

  @keyframes progress-ready-pop {
    from {
      transform: scale(0.92);
    }

    to {
      transform: scale(1);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .progress.success {
      animation: none;
    }
  }
</style>
