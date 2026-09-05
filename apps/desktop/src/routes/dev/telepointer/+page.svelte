<!--
  Dev-only telepointer receiver surface (SPEC.md §4.5 / task brief item 3).

  ## What's real vs. what's a stand-in here — read this before assuming
  anything about this page proves more than it does.

  REAL, end to end:
  - The LiveKit data-channel round trip: `src-tauri/src/telepointer.rs`'s
    sender loop polls the real local cursor position (CoreGraphics
    `CGEventGetLocation`, same primitive `hover_tab.rs` uses) at ~45Hz,
    hit-tests it against real currently-shared windows'
    real on-screen frames, and publishes real
    `room.local_participant().publish_data(...)` messages over a real
    LiveKit Cloud room connection whenever at least one window is being
    shared (via the existing hover-tab share pill).
  - This page's receiver side: it opens its own real Tauri window (see
    `src-tauri/src/dev_telepointer.rs`), and `telepointer.rs`'s receiver task
    subscribes to the same room's real `RoomEvent::DataReceived` and emits a
    real `telepointer-update` event straight to this window (`emit_to`, same
    mechanism `hover-tab-update`/`share-error` already use elsewhere in this
    codebase).
  - The coordinate math below (normalized 0-1 -> pixel position against
    THIS page's own rendered mock-surface size) and the idle-fade timeout
    are both real, not simulated.
  - `Pointer.svelte` / `NamePill.svelte` are the existing, previously-built
    components (unmodified) — not new one-off markup for this page.

  STAND-IN, flagged explicitly:
  - The "shared window" rectangle below is a static, fixed-size mock
    surface — NOT a real incoming decoded video frame, and NOT a real
    per-window native compositor window (SPEC.md §4.4's "each incoming
    shared-window track -> one borderless NSWindow" compositor does not
    exist anywhere in this codebase yet — checked directly, nothing
    subscribes to a remote video track and renders it as a window). This
    page is the most honest available integration point today, per the
    task brief's own guidance, not a preview of the real receiver UI.
  - `userId`/color mapping: every distinct `userId` this page has ever seen
    gets a color assigned round-robin from the existing `--id-*` identity
    palette (deterministic per session, not a real per-user identity
    lookup) — there's no real multi-user identity/color directory to look
    up against yet (see `session.rs`'s `DEV_IDENTITY` stand-in, which is
    literally the only `userId` a single-process build of this app can ever
    send today).
  - The mock window's logical size below (640x400) is arbitrary, chosen to
    be a plausible "shared code editor" aspect ratio — it does NOT reflect
    any real captured window's actual size (the real captured size only
    lives in `capture.rs`'s `WindowCapture`/`session.rs`'s `ActiveShare`,
    which this page has no access to).

  ## How to exercise this for real

  1. Launch the real app (`npm run tauri dev` or the built `.app`).
  2. Call the `open_dev_telepointer_window` command from anywhere (or add a
     temporary button) to open this window.
  3. Hover a real window and click the hover-tab pill to start sharing it.
  4. Move your cursor over the shared window — this page should show a
     labeled purple "you"-colored pointer tracking your cursor's relative
     position inside the mock rectangle below (mapped from the REAL shared
     window's real bounds, not this rectangle's bounds — see
     `telepointer.rs`'s coordinate-model doc comment. Since only one process
     is running, `windowId` will match whichever window you shared;
     multiple simultaneously-shared windows will each get their own
     dot if this page is extended to show more than one at once — v1 here
     only renders whichever `windowId` update arrived most recently, see
     below).
-->
<script lang="ts">
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import Pointer from '$lib/components/Pointer.svelte';
  import type { IdentityColor } from '$lib/components/Avatar.svelte';
  import { friendlyTelepointerName } from '$lib/data/telepointerDisplay';
  import { EVENTS } from '$lib/ipc';
  import type { TelepointerUpdate } from '$lib/ipc';

  interface TrackedPointer {
    userId: string;
    name: string;
    windowId: number;
    x: number;
    y: number;
    visible: boolean;
    identity: IdentityColor;
    lastUpdateMs: number;
    idle: boolean;
  }

  // Round-robin color assignment for whichever userIds this page has seen —
  // see file header re: this NOT being a real identity/color directory.
  const PALETTE: IdentityColor[] = ['plum', 'blue', 'green', 'amber', 'lilac', 'slate'];
  const colorForUser = new Map<string, IdentityColor>();
  function identityFor(userId: string): IdentityColor {
    let color = colorForUser.get(userId);
    if (!color) {
      color = PALETTE[colorForUser.size % PALETTE.length];
      colorForUser.set(userId, color);
    }
    return color;
  }

  // Idle threshold: per SPEC.md §4.5 "fade idle pointers" — this is
  // deliberately client-side/receiver-driven (task brief item 4: "a simple
  // client-side timeout on last-received-update is fine, doesn't need to be
  // server-driven").
  const IDLE_MS = 2500;
  const STALE_MS = 8000; // stop rendering entirely if nothing for this long

  let pointers = $state<Record<string, TrackedPointer>>({});
  let lastEventAt = $state<number | null>(null);
  let eventCount = $state(0);

  onMount(() => {
    const unlisten = listen<TelepointerUpdate>(EVENTS.telepointerUpdate, (event) => {
      const now = performance.now();
      const { userId, windowId, x, y, visible } = event.payload;
      lastEventAt = now;
      eventCount += 1;
      pointers[userId] = {
        userId,
        name: friendlyTelepointerName(event.payload.displayName, userId),
        windowId,
        x,
        y,
        visible,
        identity: identityFor(userId),
        lastUpdateMs: now,
        idle: false
      };
    });

    // Idle/stale sweep — purely client-side timeout, no server round trip
    // (SPEC.md §4.5 / task brief item 4).
    const interval = setInterval(() => {
      const now = performance.now();
      for (const [userId, p] of Object.entries(pointers)) {
        const age = now - p.lastUpdateMs;
        if (age > STALE_MS) {
          const next = { ...pointers };
          delete next[userId];
          pointers = next;
        } else if (age > IDLE_MS && !p.idle) {
          pointers[userId] = { ...p, idle: true };
        }
      }
    }, 250);

    return () => {
      unlisten.then((f) => f());
      clearInterval(interval);
    };
  });

  const visiblePointers = $derived(Object.values(pointers).filter((p) => p.visible));
</script>

<div class="harness">
  <h1>Petal — telepointer dev harness</h1>
  <p class="intro">
    Real LiveKit data-channel round trip, real cursor tracking, real coordinate math — rendered
    against a <strong>static mock "shared window" rectangle</strong> (no real receiver-side
    compositor exists yet — see this file's header comment for the full real-vs-stand-in
    breakdown).
  </p>

  <div class="status">
    <span>events received: {eventCount}</span>
    <span>last event: {lastEventAt ? `${Math.round(performance.now() - lastEventAt)}ms ago` : 'none yet'}</span>
    <span>active pointers: {Object.keys(pointers).length}</span>
  </div>

  <div class="mock-window">
    <div class="mock-window-chrome">mock shared window — 640×400 (stand-in surface, see header comment)</div>
    <div class="mock-window-surface">
      {#each Object.values(pointers) as p (p.userId)}
        <Pointer name={p.name} identity={p.identity} x={p.x} y={p.y} idle={p.idle} />
      {/each}
      {#if visiblePointers.length === 0}
        <div class="empty-hint">
          Share a window and move your cursor over it — a live pointer should appear here.
        </div>
      {/if}
    </div>
  </div>
</div>

<style>
  .harness {
    padding: 24px;
    background: var(--bg-base);
    color: var(--text-primary);
    min-height: 100%;
    box-sizing: border-box;
    font: 13px var(--font-ui);
  }

  h1 {
    font: 700 18px var(--font-ui);
    margin: 0 0 8px;
  }

  .intro {
    max-width: 640px;
    color: var(--text-secondary, rgba(245, 246, 247, 0.6));
    margin: 0 0 16px;
    line-height: 1.5;
  }

  .status {
    display: flex;
    gap: 16px;
    font: 500 12px var(--font-mono, monospace);
    color: var(--text-secondary, rgba(245, 246, 247, 0.6));
    margin-bottom: 20px;
  }

  .mock-window {
    position: relative;
    width: 640px;
    max-width: 100%;
    border-radius: var(--radius-md, 10px);
    overflow: hidden;
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.45);
    border: 1px solid rgba(255, 255, 255, 0.08);
  }

  .mock-window-chrome {
    background: rgba(255, 255, 255, 0.06);
    padding: 8px 12px;
    font: 500 11px var(--font-mono, monospace);
    color: var(--text-secondary, rgba(245, 246, 247, 0.5));
  }

  .mock-window-surface {
    position: relative;
    width: 640px;
    height: 400px;
    max-width: 100%;
    background:
      linear-gradient(160deg, #1c1c22 0%, #101014 100%);
  }

  .empty-hint {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    text-align: center;
    padding: 32px;
    color: var(--text-secondary, rgba(245, 246, 247, 0.35));
    font: 500 12px var(--font-ui);
  }
</style>
