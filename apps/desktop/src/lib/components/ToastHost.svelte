<!--
  ToastHost — real wiring for SPEC.md §4.8 connection-resilience events.

  Previously, `Toast.svelte` existed only as a static presentational
  example (Phase 4's `/dev/secondary` route) plus one mock trigger
  (`/meeting/[room]`'s "Leave" button faking a "Switched to Ethernet" toast
  for ~900ms). Neither was driven by any real backend state. This component
  listens for the real `resilience-event` Tauri event —
  `src-tauri/src/resilience.rs`'s `ResilienceEvent` enum, emitted to this
  exact webview (`emit_to(app, MAIN_WINDOW_LABEL, "resilience-event", ...)`,
  the same `emit_to`-to-a-specific-webview pattern `hover-tab-update`/
  `telepointer-update`/`share-error` already use elsewhere in this app) —
  and renders the EXISTING `Toast` component in response. `Toast.svelte`
  itself is not modified.

  Mounted once in `+layout.svelte` (present on every route in the main
  webview: `/`, `/main`, `/settings`, `/meeting/[room]`), not per-route, so
  a resilience event fires a toast no matter which screen the user is
  currently on — matches SPEC.md §4.7's menubar-controls framing that these
  events matter even when the user isn't looking at the meeting view.

  Auto-dismiss: `Reconnecting`/`Disconnected` are non-dismissible ambient
  status (auto-clears when superseded by the next event); `Reconnected`/
  `NetworkChanged`/`MicDeviceChanged` are point-in-time confirmations that
  auto-dismiss after a few seconds so they don't linger. This is real
  auto-dismiss-timer logic — previously flagged in Toast.svelte's own doc
  comment as explicitly NOT built yet ("no real toast-queue/auto-dismiss-
  timer... that's app wiring, not this task") — this IS that app-wiring task.

  Toast policy (issue #18): the Rust side (resilience.rs) gates emission —
  a self-initiated proactive resume after a network change is SILENT unless
  it runs long (~3s) or fails, and `disconnected` only arrives for
  non-client-initiated disconnects (reconnect attempts exhausted / the
  server closed us), never for the user's own Leave. So this component maps
  events to toasts 1:1 with no policy of its own — and the `disconnected`
  copy must NOT claim "attempting to reconnect": by the time the SDK fires
  `Disconnected`, reconnection attempts are over, not in progress (the
  in-progress state is `reconnecting`). The old copy produced exactly the
  false alarm the user screenshotted.
-->
<script lang="ts">
  import { listen } from '@tauri-apps/api/event';
  import { onMount } from 'svelte';
  import { toastTransition } from '$lib/motion';
  import { installUpdateAndRelaunch } from '$lib/updater';
  import { openPrivacySettings } from '$lib/data/permissions';
  import { shareErrorDisplay } from '$lib/data/shareErrors';
  import { clearUpdateStatus, updateStatus } from '$lib/stores/updateStatus.svelte';
  import { setToastHostVisible } from '$lib/stores/toastHost.svelte';
  import { updateAudioDevices } from '$lib/stores/session.svelte';
  import { EVENTS } from '$lib/ipc';
  import type { ResilienceEvent, ShareErrorPayload, RemoteControlStatus } from '$lib/ipc';
  import Toast from '@petal/shared/ui/components/Toast.svelte';
  import type { ToastVariant } from '@petal/shared/ui/components/Toast.svelte';

  let visible = $state(false);
  let variant = $state<ToastVariant>('info');
  let message = $state('');
  /** Point-in-time toasts (autoDismiss) get a dismiss X so the user can
   * clear them early; ambient status toasts stay non-dismissible by design
   * (issue #18 — they clear when the next event supersedes them). */
  let dismissible = $state(false);
  let updateToastVisible = $state(false);
  // Auto-dismiss timer handle -- re-armed on every new event so a rapid
  // sequence (e.g. Reconnecting -> Reconnected) always shows the LATEST
  // state for the full duration, rather than the first one's timer
  // dismissing the second one early.
  let dismissTimer: ReturnType<typeof setTimeout> | undefined;
  let updateDismissTimer: ReturnType<typeof setTimeout> | undefined;
  let shareRepairRecoveringWindowId = $state<number | null>(null);

  const AUTO_DISMISS_MS = 4000;
  const UPDATE_RELAUNCH_TOAST_MS = 1600;

  const updateToast = $derived.by(() => {
    switch (updateStatus.kind) {
      case 'downloading':
        return { message: 'Updating Petal…', dismissible: false };
      case 'relaunching':
        return { message: 'Update installed — relaunching…', dismissible: false };
      case 'available':
        return {
          message: updateStatus.version
            ? `Update ${updateStatus.version} ready — restart to install`
            : 'Update ready — restart to install',
          dismissible: true
        };
      case 'pending-relaunch':
        return { message: 'Update ready — restart to apply', dismissible: true };
      case 'failed':
        // #43: failures used to be silent. Surface a short, dismissible reason
        // so "no prompt" becomes "update check failed: <why>".
        return { message: `Update check failed: ${updateStatus.message}`, dismissible: true };
      case 'idle':
        return null;
    }
  });

  function show(next: { variant: ToastVariant; message: string; autoDismiss: boolean }) {
    variant = next.variant;
    message = next.message;
    visible = true;
    dismissible = next.autoDismiss;
    clearTimeout(dismissTimer);
    if (next.autoDismiss) {
      dismissTimer = setTimeout(() => {
        visible = false;
      }, AUTO_DISMISS_MS);
    }
  }

  function dismissToast() {
    clearTimeout(dismissTimer);
    visible = false;
    dismissible = false;
  }

  function clearShareRepairToast(windowId: number) {
    if (shareRepairRecoveringWindowId === windowId && message === 'Restoring shared window…') {
      clearTimeout(dismissTimer);
      visible = false;
    }
    if (shareRepairRecoveringWindowId === windowId) {
      shareRepairRecoveringWindowId = null;
    }
  }

  function handleEvent(event: ResilienceEvent) {
    switch (event.kind) {
      case 'reconnecting':
        show({ variant: 'degraded', message: 'Reconnecting…', autoDismiss: false });
        break;
      case 'reconnected':
        show({ variant: 'reconnected', message: event.message, autoDismiss: true });
        break;
      case 'disconnected':
        // Reserved for genuinely terminal disconnects (issue #18): the Rust
        // side already filtered out client-initiated leaves, and the SDK
        // only fires Disconnected once its reconnect attempts are exhausted
        // — so say that, not "attempting to reconnect". Non-dismissible:
        // being disconnected is an ambient state, not a moment.
        show({ variant: 'degraded', message: 'Disconnected — connection lost', autoDismiss: false });
        break;
      case 'networkChanged':
        // Deliberately not shown (issue #18): a real network change triggers
        // a proactive resume that usually completes silently in ~1s — the
        // rare slow/failed case surfaces as its own 'reconnecting' event.
        // A toast here would alarm users about a connection that's fine.
        break;
      case 'micDeviceChanged':
        if (event.usingDefault) updateAudioDevices('', undefined);
        show({
          variant: 'reconnected',
          message: `Switched to ${event.deviceName}`,
          autoDismiss: true,
        });
        break;
      case 'micDeviceFailed':
        show({ variant: 'degraded', message: event.message, autoDismiss: true });
        break;
      case 'speakerDeviceChanged':
        if (event.usingDefault) updateAudioDevices(undefined, '');
        show({
          variant: 'reconnected',
          message: `Switched speakers to ${event.deviceName}`,
          autoDismiss: true,
        });
        break;
      case 'speakerDeviceFailed':
        show({ variant: 'degraded', message: event.message, autoDismiss: true });
        break;
      case 'sharePublicationRepairRecovering':
        shareRepairRecoveringWindowId = event.windowId;
        show({ variant: 'degraded', message: 'Restoring shared window…', autoDismiss: false });
        break;
      case 'sharePublicationRepairCancelled':
        clearShareRepairToast(event.windowId);
        break;
      case 'sharePublicationRepairRestored':
        shareRepairRecoveringWindowId = null;
        show({ variant: 'reconnected', message: 'Shared window restored', autoDismiss: true });
        break;
      case 'sharePublicationRepairFailed':
        shareRepairRecoveringWindowId = null;
        show({ variant: 'degraded', message: 'Shared window could not be restored', autoDismiss: true });
        break;
      case 'micPublicationRepairFailed':
      case 'cameraPublicationRepairFailed':
        // #713: a reconnect's one bounded republish attempt for the local
        // mic/camera track failed -- same "degraded, dismissible" shape as
        // micDeviceFailed/sharePublicationRepairFailed above, not a silent
        // drop.
        show({ variant: 'degraded', message: event.message, autoDismiss: true });
        break;
    }
  }

  // Remote control needs macOS Accessibility permission to replay input; the
  // host silently dropped every event before this surfaced anything (#201).
  // Open the Accessibility pane once (the native side also prompts) and show a
  // toast explaining why control isn't working.
  let openedAccessibilitySettings = false;
  function handleRemoteControlStatus(status: RemoteControlStatus) {
    switch (status.status) {
      case 'accessibilityDenied':
        show({ variant: 'degraded', message: status.message, autoDismiss: true });
        if (!openedAccessibilitySettings) {
          openedAccessibilitySettings = true;
          void openPrivacySettings('accessibility');
        }
        break;
      case 'textTruncated':
        show({ variant: 'degraded', message: status.message, autoDismiss: true });
        break;
    }
  }

  function handleShareError(event: ShareErrorPayload) {
    if (!event.wasStarting) return;
    const display = shareErrorDisplay(event.error);
    show({ variant: 'degraded', message: display.message, autoDismiss: true });
    if (display.openScreenRecordingSettings) {
      void openPrivacySettings('screenRecording');
    }
  }

  function dismissUpdateToast() {
    clearTimeout(updateDismissTimer);
    updateToastVisible = false;
    clearUpdateStatus();
  }

  // One-click "Restart now" for the update toast: only this explicit action
  // downloads, installs, and relaunches. Passive checks must not stage updates
  // that can apply on the next ordinary quit/reopen (#113).
  async function restartToApply() {
    clearTimeout(updateDismissTimer);
    await installUpdateAndRelaunch('toast');
  }

  $effect(() => {
    setToastHostVisible(visible || (updateToastVisible && updateToast !== null));
  });

  $effect(() => {
    clearTimeout(updateDismissTimer);
    updateToastVisible = updateToast !== null;
    if (updateStatus.kind === 'relaunching') {
      updateDismissTimer = setTimeout(() => {
        updateToastVisible = false;
      }, UPDATE_RELAUNCH_TOAST_MS);
    }
  });

  onMount(() => {
    const un = listen<ResilienceEvent>(EVENTS.resilienceEvent, (e) => handleEvent(e.payload));
    const unShareError = listen<ShareErrorPayload>(EVENTS.shareError, (e) => handleShareError(e.payload));
    const unRemoteControl = listen<RemoteControlStatus>(
      EVENTS.remoteControlStatus,
      (e) => handleRemoteControlStatus(e.payload)
    );
    return () => {
      un.then((u) => u()).catch(() => {});
      unShareError.then((u) => u()).catch(() => {});
      unRemoteControl.then((u) => u()).catch(() => {});
      clearTimeout(dismissTimer);
      clearTimeout(updateDismissTimer);
    };
  });
</script>

{#if visible || (updateToastVisible && updateToast)}
  <div class="toast-host-anchor" transition:toastTransition>
    {#if updateToastVisible && updateToast}
      <Toast
        variant={updateStatus.kind === 'failed' ? 'degraded' : 'info'}
        message={updateToast.message}
        dismissible={updateToast.dismissible}
        onDismiss={dismissUpdateToast}
        actionLabel={updateStatus.kind === 'available' || updateStatus.kind === 'pending-relaunch' ? 'Restart now' : undefined}
        onAction={updateStatus.kind === 'available' || updateStatus.kind === 'pending-relaunch' ? restartToApply : undefined}
      />
    {/if}
    {#if visible}
      <Toast {variant} {message} dismissible={dismissible} onDismiss={dismissToast} />
    {/if}
  </div>
{/if}

<style>
  .toast-host-anchor {
    position: absolute;
    left: 50%;
    bottom: 24px;
    width: max-content;
    max-width: calc(100% - 48px);
    /* Defense-in-depth (a raw, arbitrarily long error string once produced a
       toast tall enough to spill past the window edge, cut off mid-word --
       see #105). Toast messages are meant to stay short (see
       $lib/data/updaterErrors.ts's friendlyUpdateErrorMessage and
       shareErrors.ts for the established pattern), but this guarantees the
       toast stack can never break out of the window regardless: it scrolls
       internally instead of overflowing. */
    max-height: calc(100% - 48px);
    overflow-y: auto;
    transform: translateX(-50%);
    z-index: 1000;
    pointer-events: none;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
  }

  .toast-host-anchor :global(button) {
    pointer-events: auto;
  }

  /* Keep the pill itself inside the anchor so its flex contents can shrink
     around long update-version messages at the documented narrow widths. */
  .toast-host-anchor :global(.pill) {
    max-width: 100%;
  }
</style>
