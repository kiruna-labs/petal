<!--
  Real main-menu home (task brief item 2). Renders the existing `MainMenu`
  component, now backed by REAL room data (task brief item 5): `list_rooms`/
  `create_room` (src-tauri/src/rooms.rs, via `$lib/data/rooms.ts`). Rooms with
  a non-empty live presence roster are promoted into `MainMenu`'s hero slot
  (same "one hero, rest are rows" layout the component already supports);
  joined rooms stay in the row list as the current row.

  Reached either by completing onboarding at `/` or, if the frontend-only
  session store already says onboarding is done, this is where `/`
  redirects on load.

  Occupancy is queried from the Petal backend for rooms this machine knows
  about, without joining them. Rows only show positive presence; empty or
  temporarily unavailable occupancy stays visually blank.
-->
<script lang="ts">
  import { goto } from '$app/navigation';
  import { onMount, onDestroy } from 'svelte';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { invoke } from '@tauri-apps/api/core';
  import { writeText } from '@tauri-apps/plugin-clipboard-manager';
  import {
    availableMonitors,
    currentMonitor,
    getCurrentWindow,
    LogicalSize,
    PhysicalPosition
  } from '@tauri-apps/api/window';
  import MainMenu from '$lib/components/MainMenu.svelte';
  import Button from '$lib/components/Button.svelte';
  import { session } from '$lib/stores/session.svelte';
  import {
    listRooms,
    createRoom,
    forgetRoom,
    listRoomOccupancy,
    mergeRoomsWithDiscovery,
    persistRoomDisplayNameRepairsFromDiscovery,
    renameRoom,
    currentRoom,
    roomPresence,
    colorForIdentity,
    rosterFromPresence,
    roomDisplayLabel,
    roomAccessCode,
    type RoomRecord,
    type RoomOccupancy,
    type PresenceUpdate
  } from '$lib/data/rooms';
  import { inviteLinkForRoom } from '$lib/data/inviteLinks';
  import { createMainMeetingActions } from '$lib/data/mainMeetingActions';
  import { prepareMeetingWindow } from '$lib/meeting/pillWindow.svelte';
  import {
    autotestMeetingRoute,
    onceAutotestJoinResult,
    replayAutotestJoinResult,
    shouldExitMainInitialization,
    subscribeToAutotestJoinResult
  } from '$lib/data/autotestJoinRoute';
  import { rememberPendingRoomDisplayName } from '$lib/data/pendingRoomLabels';
  import {
    loadFavoriteRooms,
    orderRoomsForMenu,
    roomKey,
    saveFavoriteRooms
  } from '$lib/data/roomOrdering';
  import {
    HOME_DEFAULT,
    HOME_MIN,
    loadMainWindowFrame,
    loadMainWindowSize,
    programmaticResizeGuard,
    safeWindowPosition,
    saveMainWindowFrame,
    type WindowSize
  } from '$lib/data/windowGeometry';
  import { prefersReducedMotion } from '$lib/motion';
  import { COMMANDS, EVENTS, hasTauriBridge, type AutotestJoinResult } from '$lib/ipc';
  import {
    checkAccessibility,
    checkMicrophone,
    checkScreenRecording
  } from '$lib/data/permissions';

  const displayName = $derived(session.name || 'Guest');
  const hasTauri = hasTauriBridge();
  const canShowMenu = $derived(session.onboardingComplete);

  type VisibleParticipant = {
    name: string;
    identity: ReturnType<typeof colorForIdentity>;
    resolvedColor: string;
  };

  let rooms = $state<RoomRecord[]>([]);
  // Distinguishes a real room-list load failure from "you have no rooms" (audit
  // #129): 'error' only in the real app; a browser preview (no bridge) stays
  // 'ready'/empty. Initial room refresh is background enrichment (#8), not a
  // loading gate, so the menu paints in its final shape immediately.
  let loadState = $state<'ready' | 'error'>('ready');
  let joinedRoomName = $state<string | null>(null);
  let meetingActionError = $state<string | null>(null);
  let favoriteRooms = $state<string[]>([]);
  let presenceByRoom = $state<Record<string, VisibleParticipant[]>>({});
  // Server headcount per room from the proof-of-possession status lookup
  // (`list_room_occupancy`). Was fetched and discarded before the public
  // directory was removed; now it is what a not-joined room row shows.
  let occupancyByRoom = $state<Record<string, number>>({});
  let unlistenPresence: UnlistenFn | undefined;
  let unlistenAutotestJoinResult: UnlistenFn | undefined;
  let unlistenRoomUpdated: UnlistenFn | undefined;
  let unlistenResized: UnlistenFn | undefined;
  let unlistenMoved: UnlistenFn | undefined;
  let unlistenScaleChanged: UnlistenFn | undefined;
  let unlistenFocusChanged: UnlistenFn | undefined;
  let resizeDebounce: ReturnType<typeof setTimeout> | undefined;
  let moveDebounce: ReturnType<typeof setTimeout> | undefined;
  let monitorDebounce: ReturnType<typeof setTimeout> | undefined;
  let occupancyPoll: ReturnType<typeof setInterval> | undefined;
  let occupancyRefreshInFlight = false;
  let routeActive = false;
  const programmatic = programmaticResizeGuard;

  async function resizeWindow(target: WindowSize) {
    const win = getCurrentWindow();
    if (!prefersReducedMotion()) {
      try {
        await invoke(COMMANDS.animateMainWindowResize, { width: target.width, height: target.height });
        return;
      } catch {
        // Plain browser preview/non-macOS builds fall back to the Tauri API.
      }
    }
    await win.setSize(new LogicalSize(target.width, target.height));
  }

  async function safePositionForPhysicalFrame(pos: { x: number; y: number }, size: WindowSize) {
    const [monitors, current] = await Promise.all([
      availableMonitors(),
      currentMonitor().catch(() => null)
    ]);
    return safeWindowPosition(pos, size, monitors, current);
  }

  async function clampCurrentWindowToWorkArea(win: ReturnType<typeof getCurrentWindow>) {
    const pos = await win.outerPosition();
    const size = await win.outerSize();
    const safe = await safePositionForPhysicalFrame(pos, size);
    if (safe.changed) await win.setPosition(new PhysicalPosition(safe.x, safe.y));
  }

  async function saveCurrentMainFrame(win: ReturnType<typeof getCurrentWindow>) {
    const sf = await win.scaleFactor();
    const [size, pos] = await Promise.all([win.innerSize(), win.outerPosition()]);
    saveMainWindowFrame({
      width: size.width / sf,
      height: size.height / sf,
      x: pos.x,
      y: pos.y
    });
  }

  /** Last-chance home-geometry save at unmount. Skips when the window is NOT
   * at the persisted home size — during a join the pre-size already resized
   * the window to the meeting geometry BEFORE /main unmounts, and saving
   * that here would overwrite the main-window frame with the meeting size
   * (the next leave would then "restore" it onto /main). */
  async function saveCurrentMainFrameIfHome(win: ReturnType<typeof getCurrentWindow>) {
    const sf = await win.scaleFactor();
    const size = await win.innerSize();
    const current = { width: size.width / sf, height: size.height / sf };
    const home = loadMainWindowFrame() ?? loadMainWindowSize() ?? HOME_DEFAULT;
    if (
      Math.round(current.width) !== Math.round(home.width) ||
      Math.round(current.height) !== Math.round(home.height)
    ) {
      return;
    }
    await saveCurrentMainFrame(win);
  }

  function scheduleMainWindowSafetyCheck(win: ReturnType<typeof getCurrentWindow>) {
    if (monitorDebounce) clearTimeout(monitorDebounce);
    monitorDebounce = setTimeout(async () => {
      if (programmatic.active()) return;
      try {
        await programmatic.run(async () => {
          await clampCurrentWindowToWorkArea(win);
        });
        await saveCurrentMainFrame(win);
      } catch {
        // Window or monitor query went away.
      }
    }, 150);
  }

  async function setupMainWindowGeometry() {
    if (!hasTauri) return;
    try {
      const win = getCurrentWindow();
      await programmatic.run(async () => {
        await win.setMinSize(new LogicalSize(HOME_MIN.width, HOME_MIN.height));
        const sf = await win.scaleFactor();
        const size = await win.innerSize();
        const current = { width: size.width / sf, height: size.height / sf };
        const arrivingAtLaunchDefault =
          Math.round(current.width) === HOME_DEFAULT.width &&
          Math.round(current.height) === HOME_DEFAULT.height;

        if (!arrivingAtLaunchDefault) {
          // The window did not arrive at the launch default — most likely it
          // is still at the meeting geometry (pill-mode leaves restore after
          // the swap; the meeting route's onDestroy restore is unawaited and
          // can race this mount). Normalize to the persisted home geometry,
          // shrinking OR growing, so the meeting size is never re-persisted
          // as the main-window frame — otherwise the NEXT leave restores the
          // meeting size onto /main.
          const savedFrame = loadMainWindowFrame();
          const home = savedFrame ?? loadMainWindowSize() ?? HOME_DEFAULT;
          if (
            Math.round(current.width) !== Math.round(home.width) ||
            Math.round(current.height) !== Math.round(home.height)
          ) {
            await resizeWindow(home);
          }
          if (savedFrame) {
            const pos = await win.outerPosition();
            const safe = await safePositionForPhysicalFrame(
              { x: savedFrame.x, y: savedFrame.y },
              home
            );
            if (safe.x !== pos.x || safe.y !== pos.y) {
              await win.setPosition(new PhysicalPosition(safe.x, safe.y));
            }
          }
        }
        await clampCurrentWindowToWorkArea(win);
      });
      if (routeActive) await saveCurrentMainFrame(win);
      unlistenResized = await win.onResized(({ payload }) => {
        if (programmatic.active()) return;
        if (resizeDebounce) clearTimeout(resizeDebounce);
        resizeDebounce = setTimeout(async () => {
          if (programmatic.active()) return;
          try {
            const sf = await win.scaleFactor();
            const pos = await win.outerPosition();
            saveMainWindowFrame({
              width: payload.width / sf,
              height: payload.height / sf,
              x: pos.x,
              y: pos.y
            });
          } catch {
            // Window is going away; ignore.
          }
        }, 150);
      });
      unlistenMoved = await win.onMoved(() => {
        if (programmatic.active()) return;
        if (moveDebounce) clearTimeout(moveDebounce);
        moveDebounce = setTimeout(async () => {
          if (programmatic.active()) return;
          try {
            await saveCurrentMainFrame(win);
          } catch {
            // Window is going away; ignore.
          }
        }, 150);
      });
      unlistenScaleChanged = await win.onScaleChanged(() => {
        scheduleMainWindowSafetyCheck(win);
      });
      if (!routeActive) {
        unlistenResized?.();
        unlistenResized = undefined;
        unlistenMoved?.();
        unlistenMoved = undefined;
        unlistenScaleChanged?.();
        unlistenScaleChanged = undefined;
      }
    } catch {
      // Plain browser preview or unavailable bridge.
    }
  }

  function loadFavorites() {
    favoriteRooms = loadFavoriteRooms();
  }

  async function requiredPermissionsOk() {
    const [screen, mic, accessibility] = await Promise.all([
      checkScreenRecording(),
      checkMicrophone(),
      checkAccessibility()
    ]);
    return screen && mic === 'authorized' && accessibility;
  }

  async function recheckRequiredPermissions() {
    if (!hasTauri) return;
    try {
      if (!(await requiredPermissionsOk()) && routeActive) {
        await goto('/onboarding', { replaceState: true });
      }
    } catch (e) {
      console.error('Failed to re-check startup permissions on /main', e);
    }
  }

  function saveFavorites(next: string[]) {
    favoriteRooms = next;
    saveFavoriteRooms(next);
  }

  function visibleParticipants(
    participants: readonly { identity: string; name: string }[]
  ): VisibleParticipant[] {
    return rosterFromPresence(participants);
  }

  async function refresh() {
    try {
      const [roomList, current, occupancy] = await Promise.all([
        listRooms(),
        currentRoom(),
        listRoomOccupancy().catch((e) => {
          console.error('Failed to query room occupancy', e);
          return null;
        })
      ]);
      if (!routeActive) return;
      const repairedRoomList = await repairRoomDisplayNames(roomList, occupancy);
      if (!routeActive) return;
      const mergedRooms = mergeRoomsWithDiscovery(repairedRoomList, occupancy);
      rooms = mergedRooms;
      joinedRoomName = current;
      applyOccupancy(occupancy, mergedRooms);
      loadState = 'ready';
      if (current) {
        // Presence is a non-critical enrichment -- a hiccup here must not flip
        // the whole screen into the error state once the room list has loaded.
        try {
          const presence = await roomPresence();
          if (!routeActive) return;
          presenceByRoom = {
            ...presenceByRoom,
            [current]: visibleParticipants(presence)
          };
        } catch (e) {
          console.error('Failed to load current-room presence on /main', e);
        }
      }
    } catch (e) {
      // A rejected listRooms()/currentRoom() in the real app is a genuine error
      // worth surfacing (with a retry); in a plain browser preview (no Tauri
      // bridge) it's expected, so fall back to the quiet empty state.
      console.error('Failed to load rooms/presence on /main', e);
      loadState = hasTauri ? 'error' : 'ready';
    }
  }

  function applyOccupancy(occupancy: RoomOccupancy[] | null, roomList = rooms) {
    const nextPresence: Record<string, VisibleParticipant[]> = {};
    const nextOccupancy: Record<string, number> = {};

    if (!occupancy) {
      for (const room of roomList) nextPresence[room.name] = [];
    } else {
      const occupancyByKey = new Map<string, RoomOccupancy>();
      for (const row of occupancy) {
        for (const key of occupancyKeys(row)) {
          if (!occupancyByKey.has(key)) occupancyByKey.set(key, row);
        }
      }
      for (const room of roomList) {
        const row = roomKeys(room).map((key) => occupancyByKey.get(key)).find(Boolean);
        if (row && row.available !== false) {
          nextPresence[room.name] = visibleParticipants(row.participants ?? []);
          if (typeof row.occupancy === 'number' && row.occupancy > 0) {
            nextOccupancy[room.name] = row.occupancy;
          }
        } else {
          nextPresence[room.name] = [];
        }
      }
    }

    presenceByRoom = nextPresence;
    occupancyByRoom = nextOccupancy;
  }

  async function refreshOccupancy() {
    if (occupancyRefreshInFlight) return;
    occupancyRefreshInFlight = true;
    try {
      const [roomList, occupancy] = await Promise.all([listRooms(), listRoomOccupancy()]);
      if (!routeActive) return;
      const repairedRoomList = await repairRoomDisplayNames(roomList, occupancy);
      if (!routeActive) return;
      const mergedRooms = mergeRoomsWithDiscovery(repairedRoomList, occupancy);
      rooms = mergedRooms;
      applyOccupancy(occupancy, mergedRooms);
      const current = await currentRoom();
      if (!routeActive) return;
      joinedRoomName = current;
      if (current) {
        const presence = await roomPresence();
        if (!routeActive) return;
        presenceByRoom = {
          ...presenceByRoom,
          [current]: visibleParticipants(presence)
        };
      }
    } catch (e) {
      console.error('Failed to refresh room occupancy', e);
      if (routeActive) {
        presenceByRoom = Object.fromEntries(rooms.map((room) => [room.name, []]));
        occupancyByRoom = {};
      }
    } finally {
      occupancyRefreshInFlight = false;
    }
  }

  async function repairRoomDisplayNames(
    roomList: RoomRecord[],
    occupancy: RoomOccupancy[] | null
  ): Promise<RoomRecord[]> {
    return persistRoomDisplayNameRepairsFromDiscovery(roomList, occupancy, renameRoom, (error, repair) => {
      console.error('Failed to persist discovered room display name', {
        idOrCode: repair.idOrCode,
        displayName: repair.displayName,
        error
      });
    });
  }

  function handleAutotestJoinResult(result: AutotestJoinResult) {
    if (result.status === 'joined') {
      joinedRoomName = result.roomName;
      meetingActionError = null;
      void goto(autotestMeetingRoute(result.roomName)).catch((error) => {
        console.error('Failed to open autotest meeting route', error);
        meetingActionError = 'Test meeting joined, but its meeting view could not open.';
      });
      return;
    }

    joinedRoomName = null;
    meetingActionError = `Test meeting could not be joined (${result.reason}). Check Petal logs.`;
    void refresh();
  }

  const deliverAutotestJoinResult = onceAutotestJoinResult(handleAutotestJoinResult);

  onMount(async () => {
    routeActive = true;
    if (!session.onboardingComplete) {
      await goto('/onboarding', { replaceState: true });
      return;
    }
    void recheckRequiredPermissions();
    // The debug-only autotest hook joins Rust state directly. Its terminal
    // event is the explicit bridge to this route; normal user joins never
    // emit it and keep their existing navigation path.
    if (hasTauri) {
      try {
        await subscribeToAutotestJoinResult(
          (handler) => listen<AutotestJoinResult>(EVENTS.autotestJoinResult, (event) => handler(event.payload)),
          deliverAutotestJoinResult,
          () => routeActive,
          (unlisten) => (unlistenAutotestJoinResult = unlisten)
        );
        const replayedResult = await replayAutotestJoinResult(
          () => invoke<AutotestJoinResult | null>(COMMANDS.autotestJoinResult),
          deliverAutotestJoinResult,
          () => routeActive
        );
        if (!routeActive || shouldExitMainInitialization(replayedResult)) return;
      } catch {
        // Browser preview has no Tauri event bridge; its normal empty state remains valid.
      }
    }
    await setupMainWindowGeometry();
    loadFavorites();
    await refresh();
    occupancyPoll = setInterval(() => {
      void refreshOccupancy();
    }, 10_000);
    if (hasTauri) {
      try {
        unlistenFocusChanged = await getCurrentWindow().onFocusChanged(({ payload: focused }) => {
          if (focused) void refreshOccupancy();
        });
        if (!routeActive) {
          unlistenFocusChanged?.();
          unlistenFocusChanged = undefined;
        }
      } catch {
        // Browser preview or unavailable bridge; interval refresh still covers it.
      }
    }
    // Live presence updates (src-tauri/src/presence.rs's `presence-update`
    // event) so the promoted hero's participant stack / RoomRow live count
    // updates in real time without polling, e.g. after joining a room from
    // this same screen.
    unlistenPresence = await listen<PresenceUpdate>(EVENTS.presenceUpdate, (event) => {
      const { roomName, participants } = event.payload;
      presenceByRoom = {
        ...presenceByRoom,
        [roomName]: visibleParticipants(participants)
      };
      // Re-sync the joined-room state from the backend rather than inferring
      // it from the roster payload -- the backend (`current_room`) is the
      // single source of truth for "am I in a meeting right now" (a join can
      // also happen without this webview, e.g. the autotest driver).
      currentRoom()
        .then((current) => (joinedRoomName = current))
        .catch((e) => console.error('Failed to re-check current room', e));
    });
    unlistenRoomUpdated = await listen(EVENTS.roomUpdated, () => {
      void refresh();
    });
    // Guard the post-await registration: if onDestroy already ran while this
    // listen() was pending, tear the subscription down now so it can't orphan
    // and survive navigation (audit #129).
    if (!routeActive) {
      unlistenPresence?.();
      unlistenPresence = undefined;
      unlistenRoomUpdated?.();
      unlistenRoomUpdated = undefined;
    }
  });

  onDestroy(() => {
    routeActive = false;
    if (hasTauri) {
      try {
        void saveCurrentMainFrameIfHome(getCurrentWindow());
      } catch {
        // Browser preview or unavailable bridge.
      }
    }
    unlistenPresence?.();
    unlistenAutotestJoinResult?.();
    unlistenRoomUpdated?.();
    unlistenResized?.();
    unlistenMoved?.();
    unlistenScaleChanged?.();
    unlistenFocusChanged?.();
    if (resizeDebounce) clearTimeout(resizeDebounce);
    if (moveDebounce) clearTimeout(moveDebounce);
    if (monitorDebounce) clearTimeout(monitorDebounce);
    if (occupancyPoll) clearInterval(occupancyPoll);
  });

  // Suppress the promoted hero while in a meeting: the joined room now stays
  // in YOUR ROOMS as the top current/live row instead of moving to a separate
  // banner. When not joined, promote one live room into the hero and render
  // any other occupied rooms as live rows.
  const roomDisplayNamesByName = $derived.by<Record<string, string>>(() =>
    Object.fromEntries(rooms.map((room) => [room.name, displayLabelForRoom(room)]))
  );

  const roomAccessCodesByName = $derived.by<Record<string, string | null>>(() =>
    Object.fromEntries(rooms.map((room) => [room.name, roomAccessCode(room)]))
  );

  const orderedRoomNames = $derived.by(() => {
    return orderRoomsForMenu(rooms, favoriteRooms)
      .sort((a, b) => roomListPriority(b.name) - roomListPriority(a.name))
      .map((r) => r.name);
  });

  const promotedLiveRoom = $derived.by(() => {
    if (joinedRoomName) return undefined;
    const name = orderedRoomNames.find((roomName) => (presenceByRoom[roomName]?.length ?? 0) > 0);
    return name ? { name, participants: presenceByRoom[name] } : undefined;
  });

  const rowRoomNames = $derived(
    orderedRoomNames.filter((name) => !promotedLiveRoom || name !== promotedLiveRoom.name)
  );

  async function handleReturnToMeeting() {
    if (joinedRoomName) {
      try {
        await prepareMeetingWindow();
      } catch {
        // Best-effort.
      }
      await goto(`/meeting/${joinedRoomName}`);
    }
  }

  async function joinOrReturnFromRoomRow(name: string) {
    if (joinedRoomName && roomKey(name) === roomKey(joinedRoomName)) {
      await handleReturnToMeeting();
      return;
    }
    await joinAndGo(name);
  }

  const { joinAndGo, startMeetingAndGo } = createMainMeetingActions({
    createRoom,
    goto,
    prepareMeetingWindow,
    rememberPendingRoomDisplayName,
    setMeetingActionError: (message) => {
      meetingActionError = message;
    }
  });

  function displayLabelForRoom(room: RoomRecord): string {
    // Delegate to roomDisplayLabel (not a duplicated fallback chain) — it's the
    // one place that filters the legacy generic "room" label (#42); this used
    // to return `room.displayName` directly when truthy, which bypassed that
    // filter and kept showing "room" for rooms stamped before the fix.
    return roomDisplayLabel(room);
  }

  async function removeRoom(name: string) {
    try {
      await forgetRoom(name);
      saveFavorites(favoriteRooms.filter((room) => roomKey(room) !== roomKey(name)));
      await refresh();
    } catch (e) {
      console.error('Failed to forget room', e);
    }
  }

  async function copyRoomInviteLink(name: string): Promise<boolean> {
    const room = rooms.find((item) => item.name === name || item.slug === name);
    if (!room) return false;
    const link = inviteLinkForRoom(room, displayLabelForRoom(room));
    if (!link) return false;
    try {
      await writeText(link);
      return true;
    } catch {
      try {
        await navigator.clipboard.writeText(link);
        return true;
      } catch (e) {
        console.error('Failed to copy invite link', e);
        return false;
      }
    }
  }

  function handleOpenSettings() {
    goto('/settings');
  }

  function toggleFavoriteRoom(name: string) {
    const key = roomKey(name);
    if (favoriteRooms.some((room) => roomKey(room) === key)) {
      saveFavorites(favoriteRooms.filter((room) => roomKey(room) !== key));
    } else {
      saveFavorites([name, ...favoriteRooms]);
    }
  }

  function normalizedKey(value: string | null | undefined): string | null {
    const normalized = value?.trim().toLowerCase();
    return normalized ? normalized : null;
  }

  function livekitRoomForCode(code: string | null | undefined): string | null {
    const normalized = normalizedKey(code);
    return normalized ? `petal-room-${normalized}` : null;
  }

  function roomKeys(room: RoomRecord): string[] {
    return [room.id, room.name, room.slug, livekitRoomForCode(room.name)]
      .map(normalizedKey)
      .filter((key): key is string => Boolean(key));
  }

  function occupancyKeys(row: RoomOccupancy): string[] {
    return [row.id, row.roomName, row.slug, row.livekitRoom]
      .map(normalizedKey)
      .filter((key): key is string => Boolean(key));
  }

  function isRoomLive(name: string): boolean {
    return (presenceByRoom[name]?.length ?? 0) > 0;
  }

  function roomListPriority(name: string): number {
    if (joinedRoomName && roomKey(name) === roomKey(joinedRoomName)) return 2;
    return isRoomLive(name) ? 1 : 0;
  }

  // Quit (issue #20): the Rust `quit_app` command best-effort leaves any
  // joined room (clean share/audio/LiveKit teardown via session::leave_room)
  // then app.exit(0) — the process ends, so there's nothing to await after a
  // success. The catch only matters in a plain browser preview (no bridge).
  async function handleQuit() {
    try {
      await invoke(COMMANDS.quitApp);
    } catch (e) {
      console.error('Failed to quit', e);
    }
  }
</script>

<main>
  {#if canShowMenu}
    <div class="stack">
      {#if loadState === 'error'}
        <section class="load-note" aria-live="polite">
          <span class="load-note-text">Couldn't load your rooms.</span>
          <Button variant="ghost" onclick={() => void refresh()}>Try again</Button>
        </section>
      {/if}

      <MainMenu
        frameless
        userName={displayName}
        userIdentity={session.identity}
        liveRoom={promotedLiveRoom}
        emptyRooms={rowRoomNames}
        currentRoom={joinedRoomName}
        roomDisplayNamesByName={roomDisplayNamesByName}
        roomAccessCodesByName={roomAccessCodesByName}
        roomParticipantsByName={presenceByRoom}
        roomOccupancyByName={occupancyByRoom}
        favoriteRooms={favoriteRooms}
        onJoinLive={promotedLiveRoom ? () => joinAndGo(promotedLiveRoom.name) : undefined}
        onOpenSettings={handleOpenSettings}
        onQuit={handleQuit}
        onCreateMeeting={startMeetingAndGo}
        {meetingActionError}
        onClearMeetingActionError={() => (meetingActionError = null)}
        onJoinRoom={joinOrReturnFromRoomRow}
        onCopyRoomLink={copyRoomInviteLink}
        onJoinByCode={joinAndGo}
        onToggleFavoriteRoom={toggleFavoriteRoom}
        onRemoveRoom={removeRoom}
      />
    </div>
  {/if}
</main>

<style>
  main {
    display: flex;
    /* The menu IS the window (frameless MainMenu) — no centered floating
       card. `overflow-y: auto` kept so a too-tall content stack still
       scrolls internally (body pins overflow: hidden). */
    height: 100%;
    width: 100%;
    overflow-y: auto;
    overscroll-behavior: none;
    box-sizing: border-box;
    /* Matches MainMenu's own shell surface so there's no visible outer frame. */
    background: var(--menu-shell);
  }

  .stack {
    display: flex;
    flex-direction: column;
    gap: 0;
    align-items: stretch;
    width: 100%;
    min-height: 100%;
  }

  /* Quiet load-note banner (audit #129) — real room-list load failures only.
     Startup loading now lives inside MainMenu's room list (#8). */
  .load-note {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    box-sizing: border-box;
    padding: 10px 18px;
    background: var(--menu-shell);
    border-bottom: 1px solid var(--fill-strong);
  }

  .load-note-text {
    font-size: 12.5px;
    color: var(--text-primary);
  }

</style>
