<!--
  Top-center "<Name> is sharing a window" notice pill (#679).

  Always-present, hidden panel (src-tauri/src/share_notice.rs) cloned from
  the hover-tab recipe: the Rust side owns create/show/hide + top-center
  positioning; THIS route owns content, latest-share replacement, and the 4s
  auto-dismiss timer -- the same division of labor menubar-popover already
  has between Rust (panel lifecycle) and its own webview (content/timing).

  Reuses the shared Toast component as-is (its `message` CSS already wraps
  rather than truncates -- see tests/transientTextTruncation.test.ts) instead
  of a bespoke pill; only the HOST panel around it is new.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount, tick } from 'svelte';
  import Toast from '@petal/shared/ui/components/Toast.svelte';
  import { COMMANDS, EVENTS, remoteWindowOwnerLabel, type RemoteShareStartedEvent } from '$lib/ipc';

  const AUTO_DISMISS_MS = 4000;

  let current = $state<RemoteShareStartedEvent | null>(null);
  let host: HTMLDivElement | undefined = $state();
  let dismissTimer: ReturnType<typeof setTimeout> | undefined;
  let noticeGeneration = 0;

  const message = $derived(
    current ? `${remoteWindowOwnerLabel(current.ownerDisplayName)} is sharing a window` : ''
  );

  function clearDismissTimer() {
    if (dismissTimer) clearTimeout(dismissTimer);
    dismissTimer = undefined;
  }

  // A newer share replaces the visible notice and restarts the full four-second
  // window. The action therefore always targets the most recently shared window
  // instead of making the user work through stale queued notices first.
  function showNow(payload: RemoteShareStartedEvent) {
    noticeGeneration += 1;
    const generation = noticeGeneration;
    current = payload;
    clearDismissTimer();
    dismissTimer = setTimeout(() => void dismiss(generation), AUTO_DISMISS_MS);
  }

  async function dismiss(expectedGeneration = noticeGeneration) {
    if (expectedGeneration !== noticeGeneration) return;
    clearDismissTimer();
    noticeGeneration += 1;
    current = null;
    try {
      await invoke(COMMANDS.shareNoticeDismiss);
    } catch {
      // No Tauri backend (plain browser preview) -- nothing to hide.
    }
  }

  // Measures the real rendered pill height and asks the native panel to
  // match it -- the SAME resize-to-content pattern
  // menubar-popover/+page.svelte's reportHeight/resizeMenubarPopover already
  // uses (CLAUDE.md's "UI text must never truncate": an arbitrarily long
  // display name wraps inside Toast's own capped message column, growing
  // this route's rendered height, and the native panel must grow to match or
  // the wrapped lines get clipped by the fixed webview viewport). Unlike
  // that popover, this route is still HIDDEN at this point -- the panel only
  // shows once `share_notice_present` below actually runs -- so there is no
  // provisional-size flash to correct for afterward.
  async function reveal() {
    await tick();
    if (!host) return;
    const height = Math.ceil(host.getBoundingClientRect().height);
    if (height <= 0) return;
    try {
      await invoke(COMMANDS.shareNoticePresent, { height });
    } catch {
      // No Tauri backend (plain browser preview) -- nothing to show.
    }
  }

  // Reveals when a new notice is assigned to `current` (the effect's only
  // dependency). Correction from an earlier draft of this comment: this does
  // NOT re-measure on an arbitrary reflow after that (no ResizeObserver) --
  // it only re-runs when `current` itself changes (a fresh or replacement
  // notice). That's sufficient here because the
  // panel + this webview exist from app start and Toast's fonts are bundled
  // locally, so there is no late-font-load window after the first paint the
  // way a freshly-created popover window might see.
  $effect(() => {
    if (current) void reveal();
  });

  function onBringToForeground() {
    if (!current) return;
    // Both windowId AND ownerIdentity -- the menubar's own equivalent call
    // (menubar-popover/+page.svelte's onActivateRemoteWindow) used to omit
    // ownerIdentity (#678). Two
    // participants can share the same CGWindowID, so omitting it risks
    // activating the WRONG participant's window; not repeating that bug
    // here.
    void invoke(COMMANDS.compositorActivateWindow, {
      windowId: current.windowId,
      ownerIdentity: current.ownerIdentity
    }).catch(() => {});
    void dismiss();
  }

  onMount(() => {
    const unlisten = listen<RemoteShareStartedEvent>(EVENTS.remoteShareStarted, (event) => {
      showNow(event.payload);
    });
    return () => {
      clearDismissTimer();
      unlisten.then((u) => u()).catch(() => {});
    };
  });
</script>

<div class="share-notice-host" bind:this={host}>
  {#if current}
    <Toast variant="info" {message} actionLabel="Bring to foreground" onAction={onBringToForeground} />
  {/if}
</div>

<style>
  /* Keep the standalone overlay document transparent even if global body
     styles load after the route CSS (same pattern as hover-tab/+page.svelte). */
  :global(html),
  :global(body) {
    background: transparent !important;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  .share-notice-host {
    width: 100%;
    height: 100%;
    display: flex;
    align-items: flex-start;
    justify-content: center;
    box-sizing: border-box;
    font-family: var(--font-ui, -apple-system, system-ui, sans-serif);
  }

  /* Match the established drawing pill supplied as the visual reference:
     raised graphite shell, lilac hairline, no redundant status glyph, and a
     high-contrast lilac action. Keep this scoped to the share notice so the
     shared Toast component's neutral treatment remains unchanged elsewhere. */
  .share-notice-host :global(.pill) {
    background: var(--surface-raised);
    box-shadow:
      inset 0 0 0 1px color-mix(in srgb, var(--id-lilac) 56%, var(--hairline-strong)),
      var(--shadow-float);
  }

  .share-notice-host :global(.icon) {
    display: none;
  }

  .share-notice-host :global(.action) {
    padding: 6px 10px;
    border-radius: var(--radius-chip);
    background: var(--id-lilac);
    color: var(--bg-base);
  }

  .share-notice-host :global(.action:hover) {
    background: color-mix(in srgb, var(--id-lilac) 88%, white);
  }
</style>
