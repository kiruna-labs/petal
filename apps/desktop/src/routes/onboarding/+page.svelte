<!--
  /onboarding — the onboarding flow, split out of the old `/` entry route
  (issue #20). `/` is now a launch splash that runs the REAL permission
  checks every launch and routes HERE whenever a required permission
  (Screen Recording, Microphone, Camera, Accessibility) is missing OR onboarding was never
  completed — so this page no longer gates itself on the localStorage
  `onboardingComplete` flag (that flag survives only as "has picked
  name/color" memory; a returning user with a revoked permission lands here
  again, with their name/color pre-seeded from the session store). On
  completion → `/main`.

  REAL permissions (replaces the old "Simulate granting …" mocks): this route
  now calls the real Tauri permission commands (src-tauri/src/permissions.rs
  via $lib/data/permissions.ts) for Screen Recording, Microphone, Camera, and Accessibility.
  On mount it seeds initial status from the real `check_*` commands; the
  in-progress cards' primary action triggers the real OS prompt via
  `request_*`; denied states deep-link to System Settings via the opener
  plugin.

  Status mapping — Rust → Onboarding's own status enums:
  - Screen Recording (`check/request_screen_recording` -> bool): granted =>
    'enabled'. Not-yet-granted starts as 'in-progress' (the "ask" card). If a
    `request` returns false (user dismissed / it needs the Settings + relaunch
    path), it flips to 'denied' — which surfaces the deep-link + "Relaunch
    now" affordance PermissionRow already renders. IMPORTANT quirk: macOS only
    re-reads Screen Recording at process start, so a fresh grant requests one
    bounded app restart. The Settings recovery path does not auto-restart,
    because macOS may already show its own Quit & Reopen prompt there.
  - Microphone (`check/request_microphone` -> AuthStatus): 'authorized' =>
    'enabled'; 'denied'/'restricted' => 'denied' (recoverable via Settings);
    'not-determined' => 'up-next'. (Onboarding itself further downgrades mic to
    'blocked' when Screen Recording isn't enabled yet, via its own logic.)
  - Camera (`check/request_camera` -> AuthStatus): same mapping as Microphone.
    It is a required onboarding gate, so `/main` is unreachable until Camera is
    authorized too. Not-determined prompts once; denied/restricted opens the
    Camera privacy pane and stays blocked until a recheck observes authorized.
  - Accessibility (`check/request_accessibility` -> bool): required for remote
    control input replay. False starts as the final ask card; denied/recovery
    opens the Accessibility privacy pane.

  Room creation/joining intentionally lives in `/main`; onboarding only collects
  permissions plus name/color, then completes to the main menu.
-->
<script lang="ts">
  import { goto } from '$app/navigation';
  import { onMount } from 'svelte';
  import Onboarding from '$lib/components/Onboarding.svelte';
  import IdentitySetup from '$lib/components/IdentitySetup.svelte';
  import Button from '$lib/components/Button.svelte';
  import Wordmark from '$lib/components/Wordmark.svelte';
  import Logo from '$lib/components/Logo.svelte';
  import type {
    ScreenRecordingStatus,
    MicStatus,
    CameraStatus,
    AccessibilityStatus
  } from '$lib/components/Onboarding.svelte';
  import type { IdentityColor } from '$lib/components/Avatar.svelte';
  import { session, completeOnboarding } from '$lib/stores/session.svelte';
  import {
    transitionAccessibilityRepair,
    type AccessibilityRepairEvent
  } from '$lib/data/accessibilityRepair';
  import {
    checkScreenRecording,
    requestScreenRecording,
    checkMicrophone,
    requestMicrophone,
    checkCamera,
    requestCamera,
    checkAccessibility,
    requestAccessibility,
    openPrivacySettings,
    restartApp,
    type AuthStatus
  } from '$lib/data/permissions';

  const IDENTITY_COLORS: IdentityColor[] = ['plum', 'blue', 'green', 'amber', 'lilac', 'slate'];

  function initialIdentity(): IdentityColor {
    if (session.onboardingComplete || session.name.trim()) return session.identity;
    return IDENTITY_COLORS[Math.floor(Math.random() * IDENTITY_COLORS.length)];
  }

  onMount(() => {
    // No onboardingComplete redirect here anymore — the `/` splash decides
    // whether this route should be shown (permission state is re-evaluated
    // every launch there; see issue #20). Landing here means it IS needed.
    seedPermissions();
  });

  let screenRecordingStatus = $state<ScreenRecordingStatus>('in-progress');
  let micStatus = $state<MicStatus>('up-next');
  let cameraStatus = $state<CameraStatus>('up-next');
  let accessibilityStatus = $state<AccessibilityStatus>('up-next');
  // Last-resort fallback if a native restart command fails after a fresh Screen
  // Recording grant. Normal fresh grants set relaunchInProgress and the process exits.
  let awaitingRelaunchPermission = $state<'screenRecording' | 'accessibility' | null>(null);
  let relaunchInProgress = $state(false);
  let relaunchReason = $state<'screenRecording' | 'accessibility' | null>(null);
  let accessibilityRepairSettingsOpened = $state(false);
  let accessibilityRepairRestartFailed = $state(false);

  // Pre-seed from the session store: a returning user re-onboarding because a
  // permission was revoked (the #20 splash routes them back here) keeps their
  // existing name/color rather than starting from scratch.
  let name = $state(session.name);
  let identity = $state<IdentityColor>(initialIdentity());

  let step = $state<'permissions' | 'identity'>('permissions');

  const requiredPermissionsReady = $derived(
    screenRecordingStatus === 'enabled' &&
      micStatus === 'enabled' &&
      accessibilityStatus === 'enabled'
  );

  /** Map the Rust mic/camera AuthStatus onto Onboarding's required gate enum. */
  function gateStatusFromAuth(status: AuthStatus): MicStatus {
    if (status === 'authorized') return 'enabled';
    if (status === 'denied' || status === 'restricted') return 'denied';
    return 'up-next'; // not-determined
  }

  async function seedPermissions() {
    const [screen, mic, camera, accessibility] = await Promise.all([
      checkScreenRecording(),
      checkMicrophone(),
      checkCamera(),
      checkAccessibility()
    ]);
    // If already granted (e.g. a returning user, or granted then relaunched),
    // reflect it directly. Otherwise stay on the "ask" card.
    if (screen) {
      screenRecordingStatus = 'enabled';
      if (awaitingRelaunchPermission === 'screenRecording') awaitingRelaunchPermission = null;
    }
    micStatus = gateStatusFromAuth(mic);
    cameraStatus = gateStatusFromAuth(camera);
    applyAccessibilityRepair({ type: 'launch', trusted: accessibility });
    if (
      screenRecordingStatus === 'enabled' &&
      micStatus === 'enabled' &&
      accessibilityStatus === 'enabled'
    ) {
      step = 'identity';
    }
  }

  // --- Real permission actions (wired to PermissionRow's buttons) ----------

  async function autoRelaunchForPermission(which: 'screenRecording' | 'accessibility') {
    if (relaunchInProgress) {
      console.info(`[permissions] relaunch already in progress; skipping duplicate ${which} restart`);
      return;
    }

    relaunchInProgress = true;
    relaunchReason = which;
    awaitingRelaunchPermission = null;
    console.info(`[permissions] ${which} freshly granted; requesting app restart`);

    const restarted = await restartApp(`permission-${which}-fresh-grant`);
    if (!restarted) {
      console.warn(`[permissions] restart request failed after ${which} grant; showing manual fallback`);
      relaunchInProgress = false;
      relaunchReason = null;
      awaitingRelaunchPermission = which;
    }
  }

  // "Open System Settings" on the Screen Recording in-progress card triggers
  // the real OS prompt. If it comes back granted, we mark it enabled but flag
  // one bounded relaunch for the direct request path; if not granted, flip to
  // the 'denied' state so the recovery affordances show. We intentionally do
  // not auto-relaunch after the denied-row Settings path: macOS may already
  // offer Quit & Reopen when the Screen Recording toggle changes there.
  async function handleScreenRecordingAction() {
    if (screenRecordingStatus === 'denied') {
      await openPrivacySettings('screenRecording');
      return;
    }
    const outcome = await requestScreenRecording();
    if (outcome.granted) {
      screenRecordingStatus = 'enabled';
      if (outcome.autoRelaunchRecommended) {
        await autoRelaunchForPermission('screenRecording');
      } else {
        if (awaitingRelaunchPermission === 'screenRecording') awaitingRelaunchPermission = null;
      }
    } else {
      screenRecordingStatus = 'denied';
    }
  }

  async function handleRelaunch() {
    await restartApp('permission-manual-recovery');
  }

  async function handleMicAction() {
    if (micStatus === 'denied') {
      await openPrivacySettings('microphone');
      micStatus = gateStatusFromAuth(await checkMicrophone());
      return;
    }
    const status = await requestMicrophone();
    micStatus = gateStatusFromAuth(status);
    // If the user hard-denied, point them at Settings to recover.
    if (status === 'denied' || status === 'restricted') {
      await openPrivacySettings('microphone');
    }
  }

  async function handleCameraAction() {
    if (cameraStatus === 'denied') {
      await openPrivacySettings('camera');
      cameraStatus = gateStatusFromAuth(await checkCamera());
      return;
    }
    const status = await requestCamera();
    cameraStatus = gateStatusFromAuth(status);
    // If the user hard-denied, point them at Settings to recover.
    if (status === 'denied' || status === 'restricted') {
      await openPrivacySettings('camera');
    }
  }

  async function handleAccessibilityAction() {
    if (accessibilityStatus === 'repair') {
      // Opening Settings is only step one; it never claims AX trust succeeded.
      await openPrivacySettings('accessibility');
      applyAccessibilityRepair({ type: 'settings-opened' });
      return;
    }
    if (accessibilityStatus === 'denied') {
      await openPrivacySettings('accessibility');
      applyAccessibilityRepair({ type: 'settings-opened' });
      return;
    }
    const outcome = await requestAccessibility();
    accessibilityStatus = outcome.granted ? 'enabled' : 'denied';
    if (outcome.autoRelaunchRecommended) {
      await autoRelaunchForPermission('accessibility');
    } else if (!outcome.granted) {
      await openPrivacySettings('accessibility');
    }
  }

  function applyAccessibilityRepair(event: AccessibilityRepairEvent) {
    const next = transitionAccessibilityRepair(
      {
        status: accessibilityStatus,
        settingsOpened: accessibilityRepairSettingsOpened,
        restartFailed: accessibilityRepairRestartFailed
      },
      event
    );
    accessibilityStatus = next.status;
    accessibilityRepairSettingsOpened = next.settingsOpened;
    accessibilityRepairRestartFailed = next.restartFailed;
  }

  async function handleAccessibilityRecheck() {
    applyAccessibilityRepair({ type: 'explicit-recheck', trusted: await checkAccessibility() });
    if (accessibilityStatus === 'enabled') await autoRelaunchForPermission('accessibility');
  }

  async function handleAccessibilityRepairRestart() {
    const restarted = await restartApp('accessibility-stale-grant-repair');
    applyAccessibilityRepair({ type: 'restart-completed', restarted });
  }

  async function handleRecheckPermissions() {
    const hadAccessibility = accessibilityStatus === 'enabled';
    await seedPermissions();
    if (!hadAccessibility && accessibilityStatus === 'enabled') {
      await autoRelaunchForPermission('accessibility');
    }
  }

  function handleContinueToIdentity() {
    if (!requiredPermissionsReady) {
      void handleRecheckPermissions();
      return;
    }
    if (relaunchInProgress) return;
    step = 'identity';
  }

  function handleCompleteOnboarding() {
    if (!requiredPermissionsReady) {
      step = 'permissions';
      void handleRecheckPermissions();
      return;
    }
    if (!name.trim()) return;
    completeOnboarding(name.trim(), identity);
    goto('/main');
  }
</script>

<main>
    <div class="stage">
      {#if step === 'permissions'}
        <Onboarding
          frameless
          {screenRecordingStatus}
          {micStatus}
          {cameraStatus}
          {accessibilityStatus}
          onOpenSettings={handleScreenRecordingAction}
          onRequestMicrophone={handleMicAction}
          onRequestCamera={handleCameraAction}
          onRequestAccessibility={handleAccessibilityAction}
          onConfirmAccessibilityRepairRestart={handleAccessibilityRepairRestart}
          onRecheckAccessibility={handleAccessibilityRecheck}
          {accessibilityRepairSettingsOpened}
          {accessibilityRepairRestartFailed}
          continueDisabled={relaunchInProgress}
          onContinue={handleContinueToIdentity}
        />
      {:else}
        <section class="identity-screen" aria-labelledby="identity-title">
          <div class="identity-header">
            <div class="mark">
              <Logo size={12} />
              <Wordmark size={14} />
            </div>
          </div>

          <div class="identity-copy">
            <h1 id="identity-title">Pick your name and color</h1>
            <p>This is how teammates see you in rooms and pointers.</p>
          </div>

          <IdentitySetup bind:name bind:identity />

          <div class="footer">
            <Button variant="primary" fullWidth disabled={!name.trim()} onclick={handleCompleteOnboarding}>
              Done
            </Button>
          </div>
        </section>
      {/if}

      <!-- Real permission actions. The Screen Recording card's own primary
           button (via Onboarding -> PermissionRow's onOpenSettings) triggers
           the OS prompt. Microphone and Camera use the same real request path
           once the prior required gates are enabled. Unlike the old version,
           these call REAL Tauri commands, not simulated state flips. -->
      {#if step === 'permissions' && screenRecordingStatus === 'enabled' && awaitingRelaunchPermission === 'screenRecording'}
        <div class="notice">
          <span class="notice-title">Relaunch required</span>
          <span class="notice-body">
            macOS applies this permission after Petal restarts.
          </span>
          <Button variant="primary" fullWidth onclick={handleRelaunch}>Relaunch now</Button>
        </div>
      {/if}

      {#if step === 'permissions' && relaunchInProgress}
        <div class="notice">
          <span class="notice-title">Relaunching Petal</span>
          <span class="notice-body">
            {relaunchReason === 'accessibility'
              ? 'Accessibility is on. Petal is restarting so remote control works on first use.'
              : 'Screen Recording is on. Petal is restarting so window sharing works on first use.'}
          </span>
        </div>
      {/if}

      {#if step === 'permissions' && screenRecordingStatus === 'enabled' && (micStatus === 'denied' || cameraStatus === 'denied' || accessibilityStatus === 'denied')}
        <div class="action-row">
          <button type="button" class="grant-btn" onclick={handleRecheckPermissions}>
            Recheck permissions
          </button>
        </div>
      {/if}
    </div>
  </main>

<style>
  main {
    display: flex;
    /* The checklist IS the window (frameless Onboarding) — no centered
       floating card. `overflow-y: auto` kept (main is the scroll container,
       since app.css pins `body { overflow: hidden }`) so too-tall content
       still scrolls instead of clipping, per the earlier window-fit fix. */
    height: 100%;
    width: 100%;
    overflow-y: auto;
    overscroll-behavior: none;
    box-sizing: border-box;
    /* Matches Onboarding's own frameless surface so there's no visible outer
       frame — the checklist IS the window. */
    background: var(--bg-base-2);
  }

  .stage {
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: 14px;
    width: 100%;
    /* Fill the window; when the conditional notice/action rows appear below
       the checklist the route (main) scrolls, per the earlier window-fit fix. */
    min-height: 100%;
  }

  .identity-screen {
    display: flex;
    flex-direction: column;
    gap: 16px;
    width: 100%;
    flex: 1;
    min-height: 0;
    padding: 22px;
    box-sizing: border-box;
    background: var(--bg-base-2);
  }

  .identity-header {
    display: flex;
    align-items: center;
    height: 50px;
    padding: 0;
    box-sizing: border-box;
    margin-bottom: 8px;
  }

  .mark {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .identity-copy {
    display: flex;
    flex-direction: column;
    gap: 7px;
  }

  .identity-copy h1 {
    margin: 0;
    font: 700 21px/1.15 var(--font-display);
    color: var(--text-primary);
    letter-spacing: 0;
  }

  .identity-copy p {
    margin: 0;
    font: 400 13px/1.55 var(--font-ui);
    color: var(--text-faint);
    text-wrap: pretty;
  }

  .footer {
    margin-top: auto;
    padding-top: 16px;
    border-top: 1px solid var(--hairline);
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .action-row {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    align-self: center;
    width: calc(100% - 44px);
    margin-bottom: 16px;
  }

  .grant-btn {
    width: 100%;
    min-height: 40px;
    padding: 9px 12px;
    border-radius: var(--radius-input);
    border: 1px solid var(--hairline-strong);
    background: var(--fill-base);
    color: var(--text-primary);
    font: 600 12.5px var(--font-ui);
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .grant-btn:hover {
    background: var(--fill-bright);
  }

  .grant-btn:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .grant-btn:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .notice {
    display: flex;
    flex-direction: column;
    gap: 4px;
    align-self: center;
    width: calc(100% - 44px);
    margin-bottom: 16px;
    padding: 12px 14px;
    box-sizing: border-box;
    border-radius: var(--radius-control);
    border: 1px solid var(--hairline-strong);
    background: var(--fill-weak);
  }

  .notice-title {
    font: 600 12px var(--font-ui);
    color: var(--text-primary);
    text-wrap: balance;
  }

  .notice-body {
    font: 400 11.5px/1.5 var(--font-ui);
    color: var(--text-faint);
    text-wrap: pretty;
  }
</style>
