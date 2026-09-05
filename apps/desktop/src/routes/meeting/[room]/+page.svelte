<!--
  The real in-meeting UI. On mount this route calls the real `join_room`
  command (src-tauri/src/session.rs, via `$lib/data/rooms.ts`) with the real
  onboarding identity, then hosts `MeetingChrome` — the large Gallery view
  <-> compact pill/bar view the user can toggle between (the main window
  stays visible during a meeting now; session.rs no longer hides it on join).

  This route is now composition-only: five subsystems live in rune
  controllers under `$lib/meeting/` (issue #137) —
  - `pillWindow.svelte.ts`      gallery↔pill window mode / orientation / drag
  - `speakerSpotlight.svelte.ts` active-speaker promotion (via meetingSession)
  - `meetingSession.svelte.ts`  join/leave, presence, gallery bridge + model
  - `localToast.svelte.ts`      the local auto-dismissing toasts
  and the shared `$lib/stores/toastHost.svelte.ts` rune. The route keeps
  camera/mic/share/remote-control/elapsed + the template.

  Real vs. stand-in:
  - Room join/leave, presence (roster AND gallery tiles), mic mute
    (`toggle_menubar_mic` -> real LocalAudioTrack::mute()), and screenshare
    (WindowPicker -> real capture+publish) are REAL.
  - The LOCAL tile renders a real webcam self-view when Video is on —
    mirrored, device released on off/leave/unmount. Windows feeds it from
    the SAME native Media Foundation capture via a same-process frame pull
    (camera_self_view.rs -> `next_self_view_frame` -> canvas stream; one
    camera client, no SFU round trip). macOS keeps its direct getUserMedia
    preview (two independent camera clients there, as before). Video ALSO
    publishes the webcam natively (shared camera_session.rs -> petal-camera-
    <slug> H.264 track) so other participants receive it.
  - REMOTE camera tiles render REAL video via the gallery bridge
    (issue #26): a second, hidden, subscribe-only LiveKit participant
    inside this webview (livekit-client, `$lib/data/galleryBridge.ts`;
    token from the `gallery_bridge_config` command) subscribes to
    `petal-camera-*` tracks only. Window shares stay native-compositor-only
    (THE high-fidelity path per SPEC.md §4.4); remote SHARE tiles in the
    gallery are not a thing — shares are native windows, not tiles.
-->
<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { onMount, onDestroy, type Component } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { writeText } from '@tauri-apps/plugin-clipboard-manager';
  import MeetingChrome from '$lib/components/MeetingChrome.svelte';
  import FeedbackModal from '$lib/components/FeedbackModal.svelte';
  import Toast from '@petal/shared/ui/components/Toast.svelte';
  import type { ControlIcon } from '$lib/components/ControlButton.svelte';
  import { toastTransition } from '$lib/motion';
  import { session } from '$lib/stores/session.svelte';
  import { toastHostState } from '$lib/stores/toastHost.svelte';
  import { cameraPreviewConstraints } from '$lib/data/cameraConstraints';
  import { identityColorCss, identityInkCss } from '$lib/data/identityColor';
  import { ensureCameraAccess, openPrivacySettings } from '$lib/data/permissions';
  import { startNativeSelfView, stopNativeSelfView } from '$lib/data/selfView';
  import { isWindows } from '$lib/platform';
  import {
    cameraPublishSyncPlan,
    meetingTeardownPlan,
    COMMANDS,
    EVENTS,
    hasTauriBridge
  } from '$lib/ipc';
  import type {
    AiChatEndReason,
    AiChatRefusedEvent,
    CameraPublishState,
    CameraPublishStateSnapshot,
    DrawUpdate,
    MenubarPillState,
    RemoteControlStatus,
    MicMuteChanged,
    StartCameraPublishResult
  } from '$lib/ipc';
  import { aiChatEndReasonMessage, aiChatEndToastVariant } from '$lib/data/aiChat';
  import { createPillWindow } from '$lib/meeting/pillWindow.svelte';
  import { createMeetingSession } from '$lib/meeting/meetingSession.svelte';
  import { createLocalToast } from '$lib/meeting/localToast.svelte';
  import {
    inviteCopyAriaLabel,
    inviteCopyTooltip,
    inviteLinkCopiedToastMessage,
    inviteLinkForAccessCode
  } from '$lib/data/inviteLinks';
  import { accessCodeForCredential, meetingDisplayLabelFromCredential } from '$lib/data/meetingCode';
  import { isFeedbackEnabled } from '$lib/feedback/config';

  const roomName = $derived(page.params.room ?? 'eng-sync');

  // Real mic-mute state (SPEC.md §4.9): mirrors session::SessionState via
  // the same `toggle_menubar_mic`/`get_menubar_state` commands the menubar
  // pill/popover already use, so all mute surfaces stay in sync.
  let micMuted = $state(true);
  // Camera: in Tauri/native mode the native Media Foundation/LiveKit publish is
  // authoritative; the WebView self-view is only a best-effort local preview.
  // In plain browser mode, getUserMedia still owns the preview-only state.
  let camActive = $state(false);
  let localCameraStream = $state<MediaStream | null>(null);
  let shareActive = $state(false);
  // #875: local shared-window COUNT (not just the shareActive boolean) --
  // feeds the local tile's non-interactive multi-share count pill.
  let shareCount = $state(0);
  let sharePickerOpen = $state(false);
  let remoteControlAllowed = $state(session.remoteControlPolicy !== 'off');

  // In-meeting bug report (#786) — same build-time gate as MainMenu's home
  // trigger: with no UserDispatch public key there is no topbar cell and the
  // modal is never mounted, so the SDK is never imported.
  const feedbackEnabled = isFeedbackEnabled();
  let feedbackOpen = $state(false);

  const hasTauri = hasTauriBridge();
  const localShareColor = $derived(identityColorCss(session.identity ?? 'slate'));
  const localShareInk = $derived(identityInkCss(session.identity ?? 'slate'));

  // ---- Controllers (issue #137) --------------------------------------
  const pill = createPillWindow();
  const meeting = createMeetingSession({
    roomName: () => roomName,
    bridgeQueryParams: () => page.url.searchParams,
    micMuted: () => micMuted,
    shareActive: () => shareActive,
    localShareCount: () => shareCount,
    localCameraStream: () => localCameraStream,
    // Restore the /main geometry BEFORE the swap back so the leave happens
    // at a constant window size (no desktop flash on the transparent
    // window). Gallery mode only: in pill mode the shell is transparent, so
    // pre-growing a transparent window would itself flash — the existing
    // onDestroy restore covers that path after the swap.
    prepareReturnToHome: () => (pill.expanded ? pill.restoreHomeWindow() : Promise.resolve())
  });
  const localPreviewStream = $derived(
    localCameraStream ??
      meeting.galleryParticipants.find((participant) => participant.isLocal)?.videoStream ??
      null
  );

  // Local, auto-dismissing toasts (issue #2/#8): invite-copied
  // confirmation + the camera-problem surface. Terminal join/disconnect
  // failures render only through MeetingChrome's centered state card (#157).
  const inviteToast = createLocalToast(2500);
  const microphoneToast = createLocalToast(4000);
  const cameraToast = createLocalToast(4000);
  const shareToast = createLocalToast(4500);
  let shareToastVariant = $state<'info' | 'degraded'>('degraded');
  // AI chat (#656): ends and refusals both surface as one short sentence from
  // the shared reason -> copy table. A normal end (stopped / time limit) is
  // informational, so it must not be styled as a failure.
  const aiChatToast = createLocalToast(4000);
  let aiChatToastVariant = $state<'info' | 'degraded'>('info');

  function showAiChatReasonToast(reason: AiChatEndReason) {
    aiChatToastVariant = aiChatEndToastVariant(reason);
    aiChatToast.show(aiChatEndReasonMessage(reason));
  }

  type ComponentExportsOf<C> = C extends Component<any, infer Exports, any> ? Exports : never;

  let rootToastVisible = $state(false);
  let chromeRef: ComponentExportsOf<typeof MeetingChrome> | undefined = $state();

  let unlistenRemoteControlStatus: UnlistenFn | undefined;
  let unlistenMicMute: UnlistenFn | undefined;
  let unlistenCameraPublishState: UnlistenFn | undefined;
  let unlistenRestorePill: UnlistenFn | undefined;
  let unlistenSharePicker: UnlistenFn | undefined;
  let unlistenSharePickerVisibility: UnlistenFn | undefined;
  let unlistenDrawUpdate: UnlistenFn | undefined;
  let unlistenAiChatRefused: UnlistenFn | undefined;
  let cameraDrawUpdates = $state<DrawUpdate[]>([]);

  // ToastHost's root resilience toast contributes to the pill-mode popup host
  // (grow the window so a root-level toast isn't clipped). Shared rune store
  // replaces the old `petal-toast-host-visible` DOM CustomEvent — both sides
  // live in the same webview (issue #137).
  $effect(() => {
    rootToastVisible = toastHostState.visible;
  });

  // Collapsing to the pill shrinks the window to a bar — a full-window
  // feedback dialog cannot live there, so close it with the gallery (#786).
  $effect(() => {
    if (!pill.expanded) feedbackOpen = false;
  });

  pill.attach({
    measurePill: () => chromeRef?.measurePill(),
    measurePillMinimum: () => chromeRef?.measurePillMinimum(),
    popupContentOpen: () =>
      rootToastVisible ||
      inviteToast.visible ||
      microphoneToast.visible ||
      cameraToast.visible ||
      shareToast.visible ||
      aiChatToast.visible
  });

  function currentInviteLink(): string | null {
    const accessCode = meeting.joinedRoom?.accessCode || accessCodeForCredential(roomName);
    return inviteLinkForAccessCode(
      meeting.roomLabel || meetingDisplayLabelFromCredential(roomName) || 'Petal meeting',
      accessCode
    );
  }

  // Keep active-meeting copy controls useful without exposing the opaque room
  // credential. `meeting.joinedRoom` is authoritative after join; the route
  // parameter supports the small connecting window before then.
  const inviteAccessCode = $derived(meeting.joinedRoom?.accessCode || accessCodeForCredential(roomName));
  const inviteAriaLabel = $derived(inviteCopyAriaLabel(inviteAccessCode));
  const inviteTooltip = $derived(inviteCopyTooltip(inviteAccessCode));

  async function copyInviteLink() {
    const link = currentInviteLink();
    if (!link) {
      inviteToast.show('Invite link unavailable until the access code is repaired.');
      return;
    }
    try {
      // Real NSPasteboard write via the clipboard-manager plugin.
      await writeText(link);
    } catch {
      // Plain-browser preview (no Tauri backend): best-effort fallback.
      try {
        await navigator.clipboard.writeText(link);
      } catch (e) {
        console.error('Failed to copy invite link', e);
      }
    }
    inviteToast.show(inviteLinkCopiedToastMessage(link));
  }

  // When true, the camera toast is a TERMINAL failure: sticky (no
  // auto-dismiss) and carrying a working Retry affordance -- the user must
  // never be left with a toggle that says ON and does nothing, nor an error
  // that vanishes before they can act on it.
  let cameraToastRetry = $state(false);

  function showCameraToast(message: string) {
    cameraToastRetry = false;
    cameraToast.show(message);
  }

  function showCameraRetryToast(message: string) {
    cameraToastRetry = true;
    cameraToast.show(message, 0);
  }

  function dismissCameraToast() {
    cameraToastRetry = false;
    cameraToast.hide();
  }

  function showShareToast(message: string, variant: 'info' | 'degraded' = 'degraded') {
    shareToastVariant = variant;
    shareToast.show(message);
  }

  // Real elapsed-in-meeting clock (replaces MeetingChrome's static default).
  let elapsedSecs = $state(0);
  let elapsedTimer: ReturnType<typeof setInterval> | undefined;
  const elapsed = $derived(
    `${Math.floor(elapsedSecs / 60)}:${String(elapsedSecs % 60).padStart(2, '0')}`
  );

  onMount(async () => {
    elapsedTimer = setInterval(() => (elapsedSecs += 1), 1000);

    remoteControlAllowed = session.remoteControlPolicy !== 'off';
    const redirected = await meeting.join();
    if (redirected) return; // meeting.join() already navigated to the canonical route.

    try {
      const state = await invoke<MenubarPillState>(COMMANDS.getMenubarState);
      micMuted = state.micMuted;
    } catch {
      // No Tauri backend (plain browser preview) — keep the default.
    }

    await refreshShareState();
    try {
      remoteControlAllowed = await invoke<boolean>(COMMANDS.remoteControlAllowed);
    } catch {
      remoteControlAllowed = session.remoteControlPolicy !== 'off';
    }

    try {
      unlistenSharePicker = await listen(EVENTS.sharePickerChanged, () => {
        void refreshShareState();
      });
      unlistenSharePickerVisibility = await listen<{ open: boolean }>(
        EVENTS.sharePickerVisibilityChanged,
        (event) => {
          sharePickerOpen = event.payload.open;
        }
      );
      unlistenMicMute = await listen<MicMuteChanged>(EVENTS.micMuteChanged, (event) => {
        micMuted = event.payload.muted;
      });
      unlistenCameraPublishState = await listen<CameraPublishState>(
        EVENTS.cameraPublishState,
        (event) => {
          if (event.payload.publishing) {
            dismissCameraToast();
            camActive = true;
            if (!localCameraStream) void acquireSelfView();
            return;
          }
          console.error('native camera publish failed or stopped', event.payload.error);
          localCameraStream?.getTracks().forEach((t) => t.stop());
          localCameraStream = null;
          camActive = false;
          if (event.payload.error) {
            showCameraRetryToast('Camera publish failed — others can’t see your video');
          }
        }
      );
      unlistenDrawUpdate = await listen<DrawUpdate>(EVENTS.drawUpdate, (event) => {
        if ((event.payload.windowId & 0x8000_0000) === 0) return;
        cameraDrawUpdates = [...cameraDrawUpdates.slice(-240), event.payload];
      });
      unlistenRemoteControlStatus = await listen<RemoteControlStatus>(EVENTS.remoteControlStatus, () => {});
      // A refusal from `ai_chat_start` is returned to its caller — the hover-tab
      // webview, whose 232x37px panel has nowhere to show a sentence. It
      // re-emits the reason here (#656).
      unlistenAiChatRefused = await listen<AiChatRefusedEvent>(EVENTS.aiChatRefused, (event) => {
        showAiChatReasonToast(event.payload.reason);
      });
      unlistenRestorePill = await listen(EVENTS.meetingRestorePillRequested, () => {
        pill.expanded = false;
      });
    } catch {
      // No Tauri bridge (plain browser preview) — `listen` rejects; without
      // this guard the rejection aborts the rest of onMount.
    }

    // AFTER the camera-publish-state listener exists, so the rejoin
    // reconcile's outcome can't slip between this snapshot and the listener
    // registration: the snapshot restores the toggle/self-view when the
    // native camera intent survived a leave→rejoin, and the event covers a
    // heal that completes later.
    await syncCameraStateFromNative();
  });

  onDestroy(() => {
    if (elapsedTimer) clearInterval(elapsedTimer);
    unlistenSharePicker?.();
    unlistenSharePickerVisibility?.();
    unlistenMicMute?.();
    unlistenCameraPublishState?.();
    unlistenRestorePill?.();
    unlistenRemoteControlStatus?.();
    unlistenDrawUpdate?.();
    unlistenAiChatRefused?.();
    inviteToast.dispose();
    microphoneToast.dispose();
    cameraToast.dispose();
    shareToast.dispose();
    aiChatToast.dispose();
    const teardown = meetingTeardownPlan({ stillJoined: meeting.stillJoined });
    // #782: never stop the native publish while still joined; that produced
    // `Dropping NV12 frame` for 73s. Only release the route-owned preview.
    if (teardown.stopCameraPublish) stopLocalCamera();
    else releaseSelfViewPreview();
    meeting.dispose();
    pill.dispose();
    if (hasTauri) void pill.restoreHomeWindow();
  });

  function releaseSelfViewPreview() {
    localCameraStream?.getTracks().forEach((t) => t.stop());
    localCameraStream = null;
    camActive = false;
    dismissCameraToast();
    if (hasTauri && isWindows()) stopNativeSelfView();
  }

  // Stop the self-view camera and release the device (camera light must go
  // off) — used by the Video toggle and explicit Leave path. Also stops the
  // NATIVE camera publish — fire-and-forget; the Rust leave_room teardown is
  // the belt-and-braces second layer.
  function stopLocalCamera() {
    releaseSelfViewPreview();
    if (hasTauri) invoke(COMMANDS.stopCameraPublish).catch(() => {});
  }

  // Real self-view acquisition. Windows: the native Media Foundation capture
  // feeds the preview directly (single camera client — the
  // same frames the publish path sends are pulled via `next_self_view_frame`
  // onto a canvas stream). macOS/browser: getUserMedia, which can HANG with no
  // prompt and no rejection when the camera is held by another app — raced
  // against a timeout so the control never silently wedges. In native mode
  // this is only local preview; native publication is the source of truth.
  async function acquireSelfView(): Promise<boolean> {
    if (localCameraStream) {
      // Already holding a live self-view (e.g. Retry after a publish-only
      // failure) — never open a second stream on top of it.
      camActive = true;
      return true;
    }
    if (hasTauri && isWindows()) {
      try {
        localCameraStream = await startNativeSelfView();
        camActive = true;
        return true;
      } catch (err) {
        console.error('native self-view unavailable', err);
        showCameraToast('Camera preview unavailable');
        return false; // camActive stays false — control never lies
      }
    }
    let stream: MediaStream;
    try {
      stream = await Promise.race([
        navigator.mediaDevices.getUserMedia({ video: cameraPreviewConstraints() }),
        new Promise<never>((_, reject) =>
          setTimeout(
            () => reject(new DOMException('camera request timed out', 'TimeoutError')),
            10000
          )
        )
      ]);
    } catch (err) {
      const e = err as DOMException;
      const hint =
        e.name === 'NotAllowedError'
          ? 'Camera permission was denied'
          : e.name === 'NotFoundError' || e.name === 'OverconstrainedError'
            ? 'No camera found'
            : e.name === 'NotReadableError'
              ? 'Camera is in use by another app — quit it and retry'
              : e.name === 'TimeoutError'
                ? 'Camera request timed out — it may be held by another app'
                : `Camera unavailable (${e.name})`;
      console.error('camera self-view: getUserMedia failed', e);
      showCameraToast(hint);
      return false; // camActive stays false — control never lies
    }
    localCameraStream = stream;
    camActive = true;
    return true;
  }

  // The native snapshot declares whether this platform needs its WebView
  // preview before publication (macOS) or treats it as best-effort (Windows).
  async function turnCameraOn() {
    const status = await ensureCameraAccess();
    if (status === 'denied' || status === 'restricted') {
      showCameraToast('Camera access is off — enable Petal in Privacy & Security → Camera');
      await openPrivacySettings('camera');
      return;
    }

    if (hasTauri) {
      // Every platform has a webview self-view now (Windows: the native-fed
      // canvas stream; macOS: getUserMedia), and it stays up even if the
      // publish half fails — the local preview is the user's own camera.
      if (!(await acquireSelfView())) return;

      try {
        const result = await invoke<StartCameraPublishResult>(COMMANDS.startCameraPublish);
        if (result.published === false) {
          showCameraToast('Camera is on for you — still connecting it for others…');
          return;
        }
        camActive = true;
        dismissCameraToast();
      } catch (e) {
        console.error('native camera publish failed', e);
        showCameraRetryToast('Camera is on, but publishing to others failed');
        return;
      }

      return;
    }

    if (!(await acquireSelfView())) return;
    dismissCameraToast();
  }

  // Sync the Video toggle + self-view to the REAL native camera state after
  // a (re)mount.
  async function syncCameraStateFromNative() {
    if (!hasTauri) return;
    let snapshot: CameraPublishStateSnapshot;
    try {
      snapshot = await invoke<CameraPublishStateSnapshot>(COMMANDS.cameraPublishState);
    } catch {
      return; // command unavailable — keep the default OFF
    }
    const plan = cameraPublishSyncPlan(snapshot);
    if (plan.activate) camActive = true;
    if (plan.acquirePreview && !localCameraStream) void acquireSelfView();
  }

  // Keep the control bar's "Sharing" lit state honest by reading the real
  // shared-window set (updated whenever the picker is closed).
  async function refreshShareState() {
    try {
      const ids = await invoke<number[]>(COMMANDS.sharedWindowIds);
      shareActive = ids.length > 0;
      shareCount = ids.length;
    } catch {
      // No Tauri backend (plain browser preview) — leave as-is.
    }
  }

  async function openSharePicker() {
    if (hasTauri) {
      try {
        // Thread the local user's own identity color through so the system
        // content-sharing-picker share flow (the primary "Share" button
        // path) shows the correct per-user border/bar color instead of
        // falling back to a stale/default color -- see window_picker.rs's
        // `open_system_content_picker`.
        sharePickerOpen = await invoke<boolean>(COMMANDS.toggleWindowPickerWindow);
        return;
      } catch (e) {
        console.error('toggle_window_picker_window failed', e);
        showShareToast("Couldn't open the share picker window. Relaunch Petal and try again.");
        return;
      }
    }
    const picker = window.open('/window-picker', 'petal-window-picker', 'width=820,height=700');
    sharePickerOpen = picker !== null;
  }

  async function handleScreenshareControl() {
    await openSharePicker();
  }

  async function openRegionWindow() {
    if (hasTauri) {
      try {
        await invoke<string>(COMMANDS.openRegionWindow, {
          userName: session.name,
          // Cursor placement: the selector follows the pointer until a click
          // settles it (Escape/right-click/60s timeout cancels + closes).
          followCursor: true
        });
        return;
      } catch (e) {
        console.error('open_region_window failed', e);
        showShareToast("Couldn't open the Petal View window. Relaunch Petal and try again.");
        return;
      }
    }
    window.open('/region-window', `petal-region-${Date.now()}`, 'width=640,height=400');
  }

  async function openNetworkCockpit() {
    try {
      await invoke(COMMANDS.openNetworkCockpitWindow);
    } catch (e) {
      // #842: this used to fall back to a browser-style window.open() call for
      // the network-cockpit route, but macOS wry has no `new_window_req_handler`
      // registered, so that call was a silent no-op that swallowed a real
      // command failure instead of surfacing it -- mirror openSharePicker's
      // error toast instead.
      console.error('open_network_cockpit_window failed', e);
      showShareToast("Couldn't open the network cockpit. Relaunch Petal and try again.");
    }
  }

  async function handleControl(icon: ControlIcon) {
    if (icon === 'mic') {
      // Real mute: same command/pipeline as the menubar pill.
      try {
        micMuted = await invoke<boolean>(COMMANDS.toggleMenubarMic);
      } catch {
        if (!hasTauri) {
          micMuted = !micMuted;
        } else {
          microphoneToast.show('Microphone unavailable. The mute state was not changed.');
        }
      }
    } else if (icon === 'camera') {
      if (camActive) {
        stopLocalCamera();
        return;
      }
      await turnCameraOn();
    } else if (icon === 'screenshare') {
      // Active share -> stop. No active shares -> detached picker.
      await handleScreenshareControl();
    } else if (icon === 'region') {
      await openRegionWindow();
    } else if (icon === 'remotecontrol') {
      remoteControlAllowed = !remoteControlAllowed;
      if (hasTauri) {
        try {
          remoteControlAllowed = await invoke<boolean>(COMMANDS.setRemoteControlAllowed, {
            allowed: remoteControlAllowed
          });
        } catch (e) {
          console.error('set_remote_control_allowed failed', e);
        }
      }
    } else if (icon === 'invite') {
      await copyInviteLink();
    } else if (icon === 'leave') {
      await meeting.handleLeave();
    }
  }
</script>

<main class:pill={!pill.expanded}>
  <div class="frame">
    <div class="chrome-shell">
      <MeetingChrome
        bind:this={chromeRef}
        frameless
        roomName={meeting.roomLabel}
        {elapsed}
        participants={meeting.galleryParticipants}
        {cameraDrawUpdates}
        activeIdentity={meeting.activeIdentity}
        activeColor={meeting.activeColor}
        micMuted={micMuted}
        cameraOn={camActive}
        sharingActive={shareActive}
        sharingPickerOpen={sharePickerOpen}
        sharingLiveBackground={shareActive ? localShareColor : undefined}
        sharingLiveColor={shareActive ? localShareInk : undefined}
        {remoteControlAllowed}
        localVideoStream={localPreviewStream}
        pillHost={{
          orientation: pill.orientation,
          popupOpen: pill.pillPopupHostOpen,
          onDrag: pill.handlePillDrag,
          onResize: pill.handlePillResize,
          onCompactChange: (open) => pill.setPillExpandedByHover(open),
          onPopupChange: (open) => pill.setPillMoreOpen(open)
        }}
        stateTitle={meeting.galleryStateTitle}
        stateDetail={meeting.galleryStateDetail}
        stateTone={meeting.galleryStateTone}
        bind:expanded={pill.expanded}
        onControl={handleControl}
        {inviteAriaLabel}
        {inviteTooltip}
        onInviteLinkCopy={copyInviteLink}
        onOpenNetwork={openNetworkCockpit}
        onRenameRoom={meeting.handleRenameRoom}
        onReportBug={feedbackEnabled ? () => (feedbackOpen = true) : undefined}
      />

      {#if inviteToast.visible}
        <div class="toast-anchor" transition:toastTransition>
          <Toast variant="info" message={inviteToast.message} />
        </div>
      {/if}

      {#if microphoneToast.visible}
        <div class="toast-anchor" transition:toastTransition>
          <Toast variant="degraded" message={microphoneToast.message} />
        </div>
      {/if}

      {#if cameraToast.visible}
        <div class="toast-anchor" transition:toastTransition>
          <Toast
            variant="degraded"
            message={cameraToast.message}
            actionLabel={cameraToastRetry ? 'Retry' : undefined}
            onAction={cameraToastRetry
              ? () => {
                  dismissCameraToast();
                  void turnCameraOn();
                }
              : undefined}
            dismissible={cameraToastRetry}
            onDismiss={cameraToastRetry ? dismissCameraToast : undefined}
          />
        </div>
      {/if}

      {#if shareToast.visible}
        <div class="toast-anchor" transition:toastTransition>
          <Toast variant={shareToastVariant} message={shareToast.message} />
        </div>
      {/if}

      {#if aiChatToast.visible}
        <div class="toast-anchor">
          <Toast variant={aiChatToastVariant} message={aiChatToast.message} />
        </div>
      {/if}

    </div>
  </div>
</main>

<!-- Bug report (#786): mounted at the route level, outside the pill/gallery
     stage, so the dialog is unaffected by MeetingChrome's `inert` collapse. -->
{#if feedbackEnabled && feedbackOpen}
  <FeedbackModal onClose={() => (feedbackOpen = false)} />
{/if}

<style>
  main {
    display: flex;
    height: 100%;
    width: 100%;
    /* The gallery IS the window (frameless Gallery inside MeetingChrome) —
       match its surface so there's no visible outer frame or padding band. */
    background: var(--bg-base-2);
    box-sizing: border-box;
    overscroll-behavior: none;
  }

  /* Pill mode (#11): the window is transparent and sized to the pill — no
     opaque surface may render behind it (the layout's app shell goes
     transparent via body.pill-mode at the same time). Only the pill's own
     rounded capsule + shadow paint pixels. */
  main.pill {
    background: transparent;
  }

  .frame {
    position: relative;
    width: 100%;
    height: 100%;
    /* Small window (min 380x560): let the large stage scroll internally if
       the tile grid ever outgrows it, rather than clipping the control bar. */
    overflow: hidden;
    overscroll-behavior: none;
  }

  .chrome-shell {
    position: relative;
    height: 100%;
    overflow-y: auto;
    overscroll-behavior: none;
  }

  main.pill .frame,
  main.pill .chrome-shell {
    overflow: visible;
  }

  .toast-anchor {
    /* Invite-copied confirmation: floats above the control bar, centered,
       non-interactive (auto-dismisses). */
    position: absolute;
    bottom: 84px;
    left: 50%;
    transform: translateX(-50%);
    z-index: 20;
    pointer-events: none;
  }

  /* The camera-failure toast is STICKY (dismissMs 0) and carries a Retry
     action + dismiss X (cameraToastRetry) — those buttons must stay clickable
     even though the anchor itself is non-interactive. The other toasts render
     no buttons, so a blanket re-enable is safe (same pattern as ToastHost's
     .toast-host-anchor :global(button)). */
  .toast-anchor :global(button) {
    pointer-events: auto;
  }

  main.pill .toast-anchor {
    top: 10px;
    bottom: auto;
  }

</style>
