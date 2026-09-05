<!--
  Menubar popover -- opens when the menubar pill body is clicked (see
  src-tauri/src/menubar.rs's `toggle_popover`). Per Petal-Build-Map.md §2.3/§4:
  "clicking the body opens a popover: full controls + roster + leave" --
  explicitly flagged as NOT YET DESIGNED, so this is built functional-but-plain
  by composing the existing `RosterPopover` (roster list + invite) with a
  small control row (`ControlButton`s: Audio, Video, Leave), rather than
  inventing new visual design for it.

  Real vs. stand-in (issue #5):
  - Room name + roster are REAL: `current_room` + `room_presence` on mount/
    show, kept live via the `presence-update` / `room-left` Tauri events
    (this is a regular labeled webview, which receives emitted events --
    unlike compositor CHILD webviews, per CLAUDE.md's eval-vs-events lesson).
    Belt-and-suspenders: the native show path also calls
    `window.__petalPopoverShown()` (webview.eval) so freshness never depends
    on the event bus alone.
  - Leave is REAL: `leave_room_command` (stops shares, unpublishes audio,
    closes the LiveKit room) then hides the popover; the meeting route
    navigates itself via the `room-left` event session.rs emits.
  - Height is content-fit: this page measures its own rendered height and
    calls `resize_menubar_popover` (capped at 480px; the roster scrolls
    internally beyond that).
  - Mic state mirrors the REAL mute (`toggle_menubar_mic` ->
    `session::SessionState::set_mic_muted` -> LocalAudioTrack::mute()).
  - Camera is REAL native publish state (`start_camera_publish_command` /
    `stop_camera_publish_command` -> AVFoundation camera -> petal-camera-*).
  - Controls are icon-only with delayed visual tooltips; state is conveyed by
    the button treatment, aria-labels stay state-descriptive.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { writeText } from '@tauri-apps/plugin-clipboard-manager';
  import { onMount, onDestroy, tick } from 'svelte';
  import ControlButton from '$lib/components/ControlButton.svelte';
  import MediaSplitControl from '$lib/components/MediaSplitControl.svelte';
  import DevicePicker from '$lib/components/DevicePicker.svelte';
  import MenuItem from '$lib/components/MenuItem.svelte';
  import RosterPopover from '$lib/components/RosterPopover.svelte';
  import {
    restrainedSurfaceEnterTransition,
    restrainedSurfaceExitTransition
  } from '$lib/motion';
  import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';
  import type { RosterParticipant } from '$lib/components/RosterPopover.svelte';
  import {
    listRooms,
    roomDisplayLabel,
    rosterFromPresence,
    type PresenceUpdate,
    type PresentParticipant,
    type RoomRecord
  } from '$lib/data/rooms';
  import { inviteLinkCopiedToastMessage, inviteLinkForRoom } from '$lib/data/inviteLinks';
  import { loadFavoriteRooms, orderRoomsForMenu } from '$lib/data/roomOrdering';
  import { COMMANDS, EVENTS, formatRemoteWindowHeaderTitle, hasTauriBridge } from '$lib/ipc';
  import { isMac } from '$lib/platform';
  import type {
    CameraPublishState,
    MenubarPillState,
    MicMuteChanged,
    RemoteWindowSummary,
    RoomLeftEvent
  } from '$lib/ipc';

  const hasTauri = hasTauriBridge();

  let micMuted = $state(false);
  let cameraOn = $state(false);
  let roomName = $state<string | null>(null);
  let presence = $state<PresentParticipant[]>([]);
  let rooms = $state<RoomRecord[]>([]);
  let favoriteRooms = $state<string[]>([]);
  let remoteWindows = $state<RemoteWindowSummary[]>([]);
  let host: HTMLDivElement | undefined = $state();
  let unlistenPresence: UnlistenFn | undefined;
  let unlistenRoomLeft: UnlistenFn | undefined;
  let unlistenMicMute: UnlistenFn | undefined;
  let unlistenCameraPublishState: UnlistenFn | undefined;
  let copied = $state(false);
  let copyFailed = $state(false);
  let copiedLink = $state('');
  let copyTimer: ReturnType<typeof setTimeout> | undefined;
  let deviceMenu = $state<'mic' | 'camera' | null>(null);
  let deviceTriggerEl = $state<HTMLElement | null>(null);
  let devicePanelEl = $state<HTMLDivElement | null>(null);

  const roster = $derived<RosterParticipant[]>(
    rosterFromPresence(presence, { localMicMuted: micMuted })
  );
  const recentRooms = $derived(orderRoomsForMenu(rooms, favoriteRooms).slice(0, 8));
  // Never the raw credential (#42): resolve the current room's friendly label
  // the same way `onInvite` does, for the roster header ("In <name>").
  const currentRoomLabel = $derived.by(() => {
    if (!roomName) return null;
    const room = rooms.find((item) => item.name === roomName || item.slug === roomName);
    return room ? roomDisplayLabel(room) : 'Petal meeting';
  });

  async function refresh() {
    favoriteRooms = loadFavoriteRooms();
    try {
      const [current, roomList] = await Promise.all([
        invoke<string | null>(COMMANDS.currentRoom),
        listRooms()
      ]);
      roomName = current;
      rooms = roomList;
      presence = roomName ? await invoke<PresentParticipant[]>(COMMANDS.roomPresence) : [];
      remoteWindows = await invoke<RemoteWindowSummary[]>(COMMANDS.compositorListWindows);
    } catch {
      // No Tauri backend (plain browser preview).
      roomName = null;
      presence = [];
      rooms = [];
      remoteWindows = [];
    }
    try {
      const state = await invoke<MenubarPillState>(COMMANDS.getMenubarState);
      micMuted = state.micMuted;
      cameraOn = state.cameraPublishing;
    } catch {
      // keep local default
    }
  }

  // Content-fit sizing (issue #5): measure the real rendered height and ask
  // the native side to match. CSS caps the host at 480px (internal scroll
  // beyond), so the measured value is already capped too.
  async function reportHeight() {
    await tick();
    if (!host) return;
    const height = Math.ceil(host.getBoundingClientRect().height);
    if (height <= 0) return;
    try {
      await invoke(COMMANDS.resizeMenubarPopover, { height });
    } catch {
      // No Tauri backend (plain browser preview) -- nothing to resize.
    }
  }

  // Re-measure whenever the content that drives height changes.
  $effect(() => {
    void roomName;
    void presence.length;
    void recentRooms.length;
    void remoteWindows.length;
    void copied;
    void copyFailed;
    void copiedLink;
    void deviceMenu;
    reportHeight();
  });

  // Device enumeration can finish after the panel first opens. Observe the
  // host so the native popover follows the loaded panel instead of clipping it.
  $effect(() => {
    const element = host;
    if (!element || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => void reportHeight());
    observer.observe(element);
    return () => observer.disconnect();
  });

  onMount(async () => {
    // Native show-path hook: menubar.rs's `toggle_popover` eval()s this on
    // every show so data + height are fresh even if Tauri events were never
    // delivered to this webview (unverified live -- see header comment).
    (window as unknown as Record<string, unknown>).__petalPopoverShown = () => {
      refresh().then(reportHeight);
    };

    await refresh();

    try {
      unlistenPresence = await listen<PresenceUpdate>(EVENTS.presenceUpdate, (event) => {
        if (event.payload.participants.length > 0) {
          roomName = event.payload.roomName;
          presence = event.payload.participants;
        } else if (roomName === event.payload.roomName) {
          roomName = null;
          presence = [];
        }
      });
      unlistenRoomLeft = await listen<RoomLeftEvent>(EVENTS.roomLeft, () => {
        roomName = null;
        presence = [];
      });
      unlistenMicMute = await listen<MicMuteChanged>(EVENTS.micMuteChanged, (event) => {
        micMuted = event.payload.muted;
      });
      // Camera self-heal terminal outcomes: keep this surface's toggle
      // honest (it must never claim ON for a publish that terminally
      // failed, nor OFF while a rejoin restored the camera).
      unlistenCameraPublishState = await listen<CameraPublishState>(
        EVENTS.cameraPublishState,
        (event) => {
          cameraOn = event.payload.publishing;
        }
      );
    } catch {
      // No Tauri backend (plain browser preview).
    }
  });

  onDestroy(() => {
    unlistenPresence?.();
    unlistenRoomLeft?.();
    unlistenMicMute?.();
    unlistenCameraPublishState?.();
    if (copyTimer) clearTimeout(copyTimer);
  });

  function openDeviceMenu(kind: 'mic' | 'camera', trigger: HTMLElement) {
    if (deviceMenu === kind) {
      closeDeviceMenu(false);
      return;
    }
    deviceTriggerEl = trigger;
    deviceMenu = kind;
  }

  function closeDeviceMenu(restoreFocus = true) {
    const trigger = deviceTriggerEl;
    deviceMenu = null;
    deviceTriggerEl = null;
    if (restoreFocus) requestAnimationFrame(() => trigger?.focus());
  }

  $effect(() => {
    if (deviceMenu !== null) {
      return installDismissibleLayer({
        isOpen: () => deviceMenu !== null,
        getInsideNodes: () => [devicePanelEl, deviceTriggerEl],
        getPopupNodes: () => [devicePanelEl],
        getOpener: () => deviceTriggerEl,
        onDismiss: () => closeDeviceMenu(false)
      });
    }
  });

  async function onToggleMic() {
    try {
      micMuted = await invoke<boolean>(COMMANDS.toggleMenubarMic);
    } catch {
      if (!hasTauri) {
        micMuted = !micMuted;
      }
    }
  }

  async function onToggleCamera() {
    try {
      if (cameraOn) {
        await invoke(COMMANDS.stopCameraPublish);
        cameraOn = false;
      } else {
        // `published: false` = the immediate attempt failed but the native
        // bounded self-heal is retrying; keep the toggle ON (it reflects
        // intent) and let the terminal `camera-publish-state` event below
        // reconcile it to reality if the retries are exhausted.
        await invoke(COMMANDS.startCameraPublish);
        cameraOn = true;
      }
    } catch (e) {
      console.error('camera publish toggle failed', e);
      await refresh();
    }
  }

  async function onLeave() {
    // REAL leave (issue #5): the same session::leave_room the pill's leave
    // circle uses -- stops shares, unpublishes audio, closes the room, and
    // emits `room-left` (the meeting route navigates itself to /main).
    try {
      await invoke(COMMANDS.leaveRoom);
    } catch (e) {
      console.error('leave_room_command failed', e);
    }
    try {
      await invoke(COMMANDS.hideMenubarPopover);
    } catch {
      // ignore -- best-effort close when previewed outside Tauri
    }
    await refresh();
  }

  async function hidePopover() {
    try {
      await invoke(COMMANDS.hideMenubarPopover);
    } catch {
      // Best-effort close when previewed outside Tauri.
    }
  }

  async function openMainRoute(route: string) {
    await hidePopover();
    try {
      await invoke(COMMANDS.openMainRoute, { route });
    } catch (e) {
      console.error('open_main_route failed', e);
    }
  }

  function onJoinRecent(room: string) {
    void openMainRoute(`/meeting/${encodeURIComponent(room)}`);
  }

  // Show-only, deliberately NOT openMainRoute('/main'): navigating away from
  // /meeting/<room> runs that route's onDestroy (stops the camera, restores
  // the home window) while the user is still joined.
  async function onOpenMainWindow() {
    if (!hasTauri) return;
    try {
      await invoke(COMMANDS.showMainWindow);
    } catch (e) {
      console.error('show_main_window failed', e);
    }
  }

  function onOpenSettings() {
    void openMainRoute('/settings');
  }

  async function onActivateRemoteWindow(windowId: number, ownerIdentity: string) {
    try {
      // #678: pass ownerIdentity -- resolve_window_key silently no-ops on an
      // ambiguous windowId (two participants sharing the same CGWindowID)
      // when it's omitted, so the click would do nothing with no error.
      await invoke(COMMANDS.compositorActivateWindow, { windowId, ownerIdentity });
      await hidePopover();
      await refresh();
    } catch (e) {
      console.error('compositor_activate_window failed', e);
    }
  }

  async function onQuit() {
    await hidePopover();
    try {
      await invoke(COMMANDS.quitApp);
    } catch (e) {
      console.error('quit_app failed', e);
    }
  }

  async function onInvite() {
    if (!roomName) return;
    const room = rooms.find((item) => item.name === roomName || item.slug === roomName);
    // Never the raw credential (#42) — roomDisplayLabel filters the legacy
    // generic "room" label too and defaults to "Petal meeting".
    const label = room ? roomDisplayLabel(room) : 'Petal meeting';
    const link = room ? inviteLinkForRoom(room, label) : null;
    if (!link) {
      copied = false;
      copyFailed = true;
      copiedLink = 'Invite link unavailable until the access code is repaired.';
      return;
    }
    let ok = false;
    try {
      await writeText(link);
      ok = true;
    } catch {
      try {
        await navigator.clipboard.writeText(link);
        ok = true;
      } catch (e) {
        console.error('Failed to copy invite link', e);
      }
    }
    copied = ok;
    copyFailed = !ok;
    copiedLink = ok ? inviteLinkCopiedToastMessage(link) : '';
    if (copyTimer) clearTimeout(copyTimer);
    copyTimer = setTimeout(() => {
      copied = false;
      copyFailed = false;
      copiedLink = '';
    }, 2200);
  }
</script>

<div class="menubar-popover-host" bind:this={host}>
  {#if roomName}
    <div class="roster-scroll">
      <RosterPopover roomName={currentRoomLabel ?? 'Petal meeting'} participants={roster} {onInvite} embedded />
    </div>

    {#if deviceMenu !== null}
      <div
        bind:this={devicePanelEl}
        class="menubar-device-panel"
        in:restrainedSurfaceEnterTransition
        out:restrainedSurfaceExitTransition
      >
        <DevicePicker
          mode={deviceMenu === 'mic' ? 'audio' : 'camera'}
          onClose={() => closeDeviceMenu()}
        />
      </div>
    {/if}

    <div class="control-row">
      <div class="control">
        <MediaSplitControl
          icon="mic"
          active={micMuted}
          actionLabel={micMuted ? 'Unmute microphone' : 'Mute microphone'}
          optionsLabel="Microphone options"
          optionsOpen={deviceMenu === 'mic'}
          size="menubar"
          visibleLabel="Mic"
          onToggle={onToggleMic}
          onOptions={(trigger) => openDeviceMenu('mic', trigger)}
        />
      </div>
      <div class="control">
        <MediaSplitControl
          icon="camera"
          active={!cameraOn}
          actionLabel={cameraOn ? 'Turn camera off' : 'Turn camera on'}
          optionsLabel="Camera options"
          optionsOpen={deviceMenu === 'camera'}
          size="menubar"
          visibleLabel="Camera"
          onToggle={onToggleCamera}
          onOptions={(trigger) => openDeviceMenu('camera', trigger)}
        />
      </div>
      <div class="spacer"></div>
      <div class="control">
        <ControlButton
          icon="leave"
          kind="oneshot"
          tone="danger"
          size="compact"
          label="Leave meeting"
          onclick={onLeave}
        />
        <span class="meeting-control-label">Leave</span>
      </div>
    </div>
    {#if copied || copyFailed}
      <div class="copy-status" class:error={copyFailed} role="status" aria-live="polite">
        {copiedLink || 'Could not copy invite link'}
      </div>
    {/if}
  {:else}
    <div class="recent-menu" aria-label="Recent rooms">
      <div class="section-label">Recent rooms</div>
      {#if recentRooms.length > 0}
        <div class="recent-list">
          {#each recentRooms as room (room.id)}
            <button type="button" class="room-action" onclick={() => onJoinRecent(room.name)}>
              <span class="room-dot" aria-hidden="true"></span>
              <!-- Never the raw credential (#42) -- was `{room.name}`. -->
              <span class="room-name">{roomDisplayLabel(room)}</span>
            </button>
          {/each}
        </div>
      {:else}
        <div class="no-recents">No recent rooms</div>
      {/if}
    </div>
  {/if}

  {#if remoteWindows.length > 0}
    <div class="remote-menu" aria-label="Remote windows">
      <div class="section-label">Remote windows</div>
      <div class="remote-list">
        {#each remoteWindows as remoteWindow (remoteWindow.windowId)}
          <button
            type="button"
            class="remote-action"
            onclick={() => onActivateRemoteWindow(remoteWindow.windowId, remoteWindow.ownerIdentity)}
          >
            <span class="remote-icon" aria-hidden="true"></span>
            <span class="remote-copy">
              <span class="remote-title">
                {formatRemoteWindowHeaderTitle(
                  remoteWindow.sourceTitle,
                  remoteWindow.ownerDisplayName
                )}
              </span>
              <span class="remote-status">{remoteWindow.hidden ? 'Hidden' : 'Open'}</span>
            </span>
          </button>
        {/each}
      </div>
    </div>
  {/if}

  <div class="utility-row" aria-label="Petal actions">
    <!-- Red traffic dot hides the main window; this row is one of the three
         ways back (Dock reopen and a second launch are the others). macOS-only,
         matching the dots themselves. -->
    {#if isMac()}
      <MenuItem label="Open Petal" icon="window" onclick={onOpenMainWindow} />
    {/if}
    <MenuItem label="Settings" icon="settings" onclick={onOpenSettings} />
    <MenuItem label="Quit" icon="quit" tone="danger" onclick={onQuit} />
  </div>
</div>

<style>
  :global(html),
  :global(body) {
    background: transparent;
    margin: 0;
    padding: 0;
    overflow: hidden;
    overscroll-behavior: none;
  }

  .menubar-popover-host {
    display: flex;
    flex-direction: column;
    width: 280px;
    /* Content-fit height, capped -- beyond this the roster scrolls
       internally and resize_menubar_popover clamps to the same cap. */
    max-height: 480px;
    border-radius: var(--radius-card);
    background: linear-gradient(180deg, var(--surface-raised), var(--surface));
    border: 1px solid var(--hairline);
    box-shadow: var(--shadow-panel);
    overflow: hidden;
    overscroll-behavior: none;
    font-family: var(--font-ui, -apple-system, system-ui, sans-serif);
  }

  .roster-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: none;
  }

  .menubar-device-panel {
    display: flex;
    justify-content: center;
    padding: 10px 10px 0;
    background: var(--surface);
  }

  .menubar-device-panel :global(.device-picker) {
    width: 100%;
    max-width: 100%;
  }

  .control-row {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    padding: 10px 14px;
    background: var(--surface);
    border-top: 1px solid var(--hairline);
    flex-shrink: 0;
  }

  .copy-status {
    max-width: calc(280px - 28px);
    padding: 0 14px 10px;
    background: var(--surface);
    color: var(--text-soft);
    font: 600 11px / 1.35 var(--font-ui, sans-serif);
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: pre-line;
  }

  .copy-status.error {
    color: var(--danger);
  }

  .control {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: flex-start;
  }

  .spacer {
    flex: 1;
  }

  .recent-menu {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 8px;
    padding: 12px;
    background: var(--surface);
  }

  .remote-menu {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px;
    background: var(--surface);
    border-top: 1px solid var(--hairline);
  }

  .section-label {
    padding: 0 4px;
    font: 700 10px/1 var(--font-ui, sans-serif);
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--text-muted);
  }

  .recent-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 100%;
  }

  .remote-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    max-height: 154px;
    overflow-y: auto;
    overscroll-behavior: contain;
  }

  .remote-action {
    display: grid;
    grid-template-columns: 24px 1fr;
    gap: 8px;
    align-items: center;
    width: 100%;
    min-height: 44px;
    padding: 6px 8px;
    border: none;
    border-radius: var(--radius-input);
    background: transparent;
    color: var(--text-primary);
    cursor: pointer;
    text-align: left;
    box-sizing: border-box;
    transition:
      background-color var(--motion-feedback) var(--ease-standard),
      transform var(--motion-feedback) var(--ease-standard);
  }

  .remote-action:hover {
    background: var(--fill-strong);
  }

  .remote-action:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .remote-action:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .remote-icon {
    width: 18px;
    height: 14px;
    border: 1.5px solid var(--text-dim);
    border-radius: var(--radius-check);
    /* Checkmark inset — kept literal (uiConsistency allowlist). */
    box-shadow: inset 0 0 0 1px rgba(0, 0, 0, 0.16);
    justify-self: center;
  }

  .remote-copy {
    display: flex;
    flex-direction: column;
    gap: 3px;
    min-width: 0;
  }

  .remote-title {
    color: var(--text-primary);
    font-family: var(--font-ui, sans-serif);
    font-size: var(--text-micro, 12px);
    font-weight: var(--weight-btn, 700);
    line-height: 1.2;
    overflow-wrap: anywhere;
    text-wrap: pretty;
  }

  .remote-status {
    color: var(--text-muted);
    font-family: var(--font-ui, sans-serif);
    font-size: 10px;
    font-weight: var(--weight-btn, 700);
    line-height: 1;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .room-action {
    display: flex;
    align-items: center;
    min-height: 40px;
    border: none;
    border-radius: var(--radius-input);
    background: transparent;
    color: var(--text-primary);
    font-family: var(--font-ui, sans-serif);
    cursor: pointer;
    box-sizing: border-box;
    transition:
      background-color var(--motion-feedback) var(--ease-standard),
      transform var(--motion-feedback) var(--ease-standard);
  }

  .room-action {
    width: 100%;
    gap: 9px;
    padding: 0 8px;
    text-align: left;
    font-size: 13px;
    font-weight: 650;
  }

  .room-action:hover {
    background: var(--fill-strong);
  }

  .room-action:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .room-action:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .room-dot {
    width: 6px;
    height: 6px;
    border-radius: var(--radius-pill);
    background: var(--plum-lilac);
    flex-shrink: 0;
  }

  .room-name {
    min-width: 0;
    /* UI hard rule: names must never truncate — wrap instead of ellipsis
       (the row grows; the popover scrolls internally past 480px). */
    white-space: normal;
    overflow-wrap: anywhere;
    text-wrap: pretty;
  }

  .no-recents {
    min-height: 34px;
    display: flex;
    align-items: center;
    padding: 0 4px;
    font: 500 12px/1.2 var(--font-ui, sans-serif);
    color: var(--text-muted);
    text-wrap: pretty;
  }

  .utility-row {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 6px;
    padding: 8px;
    background: var(--surface);
    border-top: 1px solid var(--hairline);
    flex-shrink: 0;
  }

  .utility-row :global(.menu-item) {
    justify-content: center;
  }
</style>
