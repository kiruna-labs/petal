<script lang="ts">
  // App entry point (SvelteKit root layout). Wires in the design-token system
  // and self-hosted fonts — no runtime network dependency for type.
  import '../styles/app.css';

  // Albert Sans — self-hosted UI font (@fontsource), practical weight subset.
  import '@fontsource/albert-sans/400.css';
  import '@fontsource/albert-sans/500.css';
  import '@fontsource/albert-sans/600.css';
  import '@fontsource/albert-sans/700.css';
  // 800 is used by NamePill; without the face and with font-synthesis: none
  // it silently rendered as 700.
  import '@fontsource/albert-sans/800.css';

  import '@fontsource/fredoka/600.css';

  // JetBrains Mono — self-hosted monospace (@fontsource), for code/counters.
  import '@fontsource/jetbrains-mono/400.css';
  import '@fontsource/jetbrains-mono/500.css';
  import '@fontsource/jetbrains-mono/700.css';

  // Connection resilience (SPEC.md §4.8): mounted at the root layout (not a
  // per-route component) so a real reconnect/network-change/mic-hot-swap
  // toast (`src-tauri/src/resilience.rs`) can surface no matter which route
  // the user is currently on.
  import ToastHost from '$lib/components/ToastHost.svelte';
  import ContextMenu from '$lib/components/ContextMenu.svelte';

  import { page } from '$app/state';
  import { goto, onNavigate } from '$app/navigation';
  import { browser } from '$app/environment';
  import { onMount } from 'svelte';
  import { seedAudioDevicePreferences } from '$lib/data/audioDevices';
  import { seedCameraDevicePreference } from '$lib/data/cameraDevices';
  import { seedCameraModePreference } from '$lib/data/cameraModes';
  import { STORAGE_KEYS } from '$lib/data/storageKeys';
  import { invoke } from '@tauri-apps/api/core';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { displayBuildVersion } from '$lib/buildInfo';
  import { platformKey } from '$lib/platform';
  import { stripNativeTooltipTitles } from '$lib/data/suppressNativeTooltips';
  import { COMMANDS, hasTauriBridge, type BuildInfo } from '$lib/ipc';
  import { checkForUpdate } from '$lib/updater';
  import { session } from '$lib/stores/session.svelte';

  let { children } = $props();
  let buildInfo = $state<BuildInfo>({
    version: '0.1.0',
    commit: 'dev',
    buildDate: 'dev',
    isReleaseBuild: false,
    cockpitPrivileged: false,
    bundleIdentifier: 'com.petal.app'
  });

  onMount(() => {
    // #782: native route requests must stay inside SvelteKit. A full document
    // navigation tears down a live meeting and restarts its camera pipeline.
    const petalWindow = window as unknown as Record<string, unknown>;
    const navigate = (route: string) => {
      // Deliberately NOT falling back to location.assign on rejection: a
      // superseded navigation rejects normally and a reload would be the very
      // teardown #782 exists to prevent. Make the failure visible instead.
      void goto(route).catch((e) => console.error('layout: navigate failed', route, e));
    };
    petalWindow.__petalNavigate = navigate;

    // Platform-gate the UI (Windows scrollbars, macOS-only Settings rows).
    // Runs at mount in every window; idempotent. Uses document check so the
    // layout stays SSR-safe.
    if (typeof document !== 'undefined') {
      document.documentElement.dataset.platform = platformKey();
    }
    // Native-tooltip suppression: WebView2 renders an OS tooltip for any
    // `title`, which would double Petal's styled tooltips. WKWebView's title
    // behavior is platform/window-state dependent, so only deliberately
    // marked controls may retain a native title. Capture-phase pointerover strips
    // the title before Chromium's tooltip delay elapses; the walk also covers
    // titled ancestors, which Chromium would otherwise use when the hovered
    // leaf has no title of its own. Every window mounts this root layout, so
    // one listener covers all webviews.
    if (typeof document !== 'undefined') {
      document.addEventListener(
        'pointerover',
        (event) => {
          // Pointer-over targets are always Elements, but guard anyway: a
          // non-Element target has no hasAttribute/removeAttribute and would
          // throw inside the strip walk.
          if (event.target instanceof Element) stripNativeTooltipTitles(event.target);
        },
        true
      );
    }
    // #842: device seeding is an app-global side effect (it can stop/restart
    // the live camera and hot-swap audio devices) and must run only in the
    // main window, exactly like the update check's guard seven lines below.
    // network-cockpit and window-picker mount this same root layout; without
    // this, opening either mid-meeting re-ran seeding and glitched a live
    // camera/audio publish.
    if (!isOverlayRoute && hasTauriBridge() && getCurrentWindow().label === 'main') {
      void seedAudioDevicePreferences();
      void seedCameraDevicePreference();
      void seedCameraModePreference();
    }
    if (!isOverlayRoute) void loadBuildInfo();
    // Launch update check is once-per-process and belongs to the main window
    // only. Secondary windows (window-picker, network-cockpit, dev harnesses)
    // mount this same root layout but must never re-trigger it; hard
    // navigations of the main webview (deep-link meeting join) are absorbed by
    // the Rust once-per-process latch in run_launch_update_check.
    if (!isOverlayRoute && hasTauriBridge() && getCurrentWindow().label === 'main') {
      void runUpdateCheck('launch', { force: true });
    }
    if (!isOverlayRoute) void syncSentryEnabled();
    void reportFrontendReady();

    return () => {
      if (petalWindow.__petalNavigate === navigate) delete petalWindow.__petalNavigate;
    };
  });

  // Route transitions (View Transitions API). The main window is transparent
  // (tauri.conf.json), so an instant page swap leaves a frame where nothing
  // is painted and the desktop shows through. startViewTransition keeps the
  // outgoing frame composited through the swap and cross-fades to the
  // incoming one. Engines without the API (WKWebView < 18, macOS 13/14)
  // silently keep the plain instant swap. Reduced-motion users still get the
  // swap covered (snapshot, zero animation) via the CSS below.
  if (browser && typeof document.startViewTransition === 'function') {
    onNavigate((navigation) => {
      return new Promise((resolve) => {
        document.startViewTransition(async () => {
          resolve();
          await navigation.complete;
        });
      });
    });
  }

  // Truthful frontend-paint-readiness signal (#327 follow-up): the native
  // "main window activated" log line fires on AppKit window activation --
  // before this SPA hydrates and paints, since the window is fully
  // transparent until .app-shell's CSS background renders. Called
  // unconditionally (not gated on isOverlayRoute) since compositor/hover-tab/
  // overlay windows benefit from a real paint signal too.
  async function reportFrontendReady() {
    if (!hasTauriBridge()) return;
    // Deliberately NOT gated on requestAnimationFrame, and that is the whole
    // subtlety of #636. Every window that reports here is hidden at the moment
    // it reports -- the main window is created `visible: false` and this signal
    // is what reveals it; overlay panels are `.hide()`-ed at creation. WebKit
    // clears a hidden window's `ActivityState::IsVisible`, so
    // `document.visibilityState` is 'hidden' and rAF callbacks are throttled to
    // ~1fps or suspended outright (this repo measured the 1fps throttle -- see
    // routes/dev/test-pattern/+page.svelte). Waiting for a "presented frame"
    // before asking to be shown is therefore circular: the window cannot paint
    // until it is shown, and it is not shown until it paints.
    //
    // `onMount` is the right trigger instead: the component tree and its CSS
    // are attached and laid out, so the first frame the compositor produces
    // AFTER the reveal already has content -- there is no unpainted frame to
    // flash. This matches hover-tab, the existing hidden-panel ready signal.
    try {
      await invoke(COMMANDS.frontendReady, { windowLabel: getCurrentWindow().label });
    } catch (e) {
      console.warn('layout: failed to report frontend ready', e);
    }
  }

  // Rust's `SENTRY_ENABLED` starts `true` at process boot (`logging::init()`
  // runs before any `AppHandle`/webview exists, so it cannot read this
  // store's persisted value yet -- same startup-window gap
  // `remote_control_allowed` already has, see `session/mod.rs`). Sync the
  // persisted preference down as early as the frontend can, so a
  // previously-disabled choice takes effect as soon as possible rather than
  // only the next time Settings' toggle is touched.
  async function syncSentryEnabled() {
    if (!hasTauriBridge()) return;
    try {
      await invoke(COMMANDS.setSentryEnabled, { enabled: session.sentryEnabled });
    } catch (e) {
      console.warn('layout: failed to sync sentryEnabled on startup', e);
    }
  }

  // Auto-update policy (#43/#103): check once on real app launch and also when
  // entering the main menu, but never from overlay/compositor webviews. The
  // launch check is forced so a clean relaunch cannot inherit a stale in-process
  // throttle; main-menu checks remain throttled so route bouncing does not
  // hammer the endpoint. `checkForUpdate()` is availability-only (#113):
  // it must not download/install/stage an update that could apply silently on
  // the next ordinary quit/relaunch. ToastHost's explicit Restart now action
  // performs install+relaunch.
  // sessionStorage survives hard navigations within this webview (deep-link
  // meeting joins do `window.location.assign`, which remounts this layout)
  // but dies with the process, so a real relaunch always starts fresh.
  let lastUpdateCheckMs = 0;
  if (typeof sessionStorage !== 'undefined') {
    try {
      lastUpdateCheckMs = Number(sessionStorage.getItem(STORAGE_KEYS.lastUpdateCheckMs)) || 0;
    } catch {
      lastUpdateCheckMs = 0;
    }
  }
  let updateCheckInFlight: Promise<unknown> | null = null;
  const UPDATE_CHECK_THROTTLE_MS = 30 * 60 * 1000; // 30 min

  function runUpdateCheck(
    reason: 'launch' | 'main-menu',
    opts: { force?: boolean } = {}
  ): Promise<unknown> | null {
    if (isOverlayRoute) return null;
    if (updateCheckInFlight) return updateCheckInFlight;
    const now = Date.now();
    if (!opts.force && now - lastUpdateCheckMs < UPDATE_CHECK_THROTTLE_MS) return null;
    lastUpdateCheckMs = now;
    if (typeof sessionStorage !== 'undefined') {
      try {
        sessionStorage.setItem(STORAGE_KEYS.lastUpdateCheckMs, String(now));
      } catch {
        // Quota/private-mode failure: the in-memory throttle still applies.
      }
    }
    const check = checkForUpdate({ skipRelaunch: true, reason }).finally(() => {
      if (updateCheckInFlight === check) updateCheckInFlight = null;
    });
    updateCheckInFlight = check;
    return check;
  }

  $effect(() => {
    if (!isMainRoute) return;
    void runUpdateCheck('main-menu');
  });

  async function loadBuildInfo() {
    if (!hasTauriBridge()) return;
    try {
      buildInfo = await invoke<BuildInfo>(COMMANDS.getBuildInfo);
    } catch (e) {
      console.warn('layout: failed to load build info', e);
    }
  }

  // The main window is frameless (tauri.conf.json: titleBarStyle "Overlay" +
  // hiddenTitle) — the UI IS the window, with the macOS traffic-lights floating
  // top-left. With no native title bar there's nothing to grab to move the
  // window, so the painted app shell carries the top drag region
  // (data-tauri-drag-region makes empty areas of it move the window;
  // interactive children still click through since Tauri only drags when the
  // target element itself is the region).
  // Compositor/dev overlay routes (their own borderless webviews / previews) opt
  // out — only the real app chrome gets the strip.
  // Allowlist for app-shell top drag clearance. /main is deliberately
  // excluded because MainMenu owns a visible top-bar drag region; overlay panels
  // (hover-tab, share-border, menubar-popover, compositor/*) and /dev harnesses
  // must NOT get the top clearance (it painted a bogus opaque band on
  // transparent overlay webviews).
  const routePath = $derived.by(() => {
    const p = page.url.pathname;
    if (p.endsWith('/index.html')) return p.slice(0, -'/index.html'.length) || '/';
    if (p.endsWith('.html')) return p.slice(0, -'.html'.length);
    return p;
  });
  // #782: SvelteKit REUSES +page.svelte when only a param changes on the same
  // route id, so `/meeting/A` -> `/meeting/B` would swap `page.params.room`
  // without re-running the meeting route's onMount -- it would never join B.
  // The full reload this file replaced hid that. `/meeting/[room]` is the only
  // dynamic route; keying the whole pathname keeps every route's mount honest.
  const routeRemountKey = $derived(page.url.pathname);
  const isMeetingRoute = $derived(routePath.startsWith('/meeting'));
  const isMainRoute = $derived(routePath === '/main');
  const isOnboardingRoute = $derived(routePath === '/onboarding');
  const isSettingsRoute = $derived(routePath === '/settings');
  const showDragStrip = $derived(['/', '/onboarding'].includes(routePath));

  // App shell (issue #11 + #14 item 1): macOS runs the main window
  // `transparent: true` (tauri.conf.json), so these real main-window routes
  // paint their own opaque, LARGE-ROUNDED-CORNER shell — radius 24px clipped
  // by the transparent native window (canvas.html §2 "Full gallery —
  // approved, subdued": 24px rounded gallery frame), with the desktop
  // showing through the corner cutouts. Windows is different: the window is
  // a real opaque DWM window with native corners and the shell radius is
  // zeroed (app.css `html[data-platform='windows'] .app-shell`), so the
  // shell just fills the window. In pill mode (body.pill-mode, toggled by
  // the meeting route) the shell goes fully transparent on both platforms
  // so ONLY the floating pill is visible. /dev/* and overlay routes keep
  // the plain opaque body from app.css.
  const showAppShell = $derived(showDragStrip || isMainRoute || isMeetingRoute || isSettingsRoute);
  const showStatusBar = $derived(showAppShell && !isMeetingRoute);
  const statusText = $derived(
    `v${displayBuildVersion(buildInfo)} · ${buildInfo.commit} · ${buildInfo.buildDate}`
  );

  // Overlay panel webviews (borderless transparent NSPanels). app.css's global
  // `body { background: var(--bg-base) }` wins the cascade over these routes'
  // own transparent overrides (import order), painting the "transparent"
  // panels opaque black on screen — force-clear it here with !important.
  const isOverlayRoute = $derived(
    // NB: these panels load the PRERENDERED files ("hover-tab.html"), so the
    // pathname can be "/hover-tab.html" — match ".html" too, not just "/".
    // share-notice (#679): the top-center "is sharing a window" pill panel,
    // same borderless transparent NSPanel pattern as the others here.
    // control-consent: the sharer-side remote-control consent prompt panel
    // (ask policy), same recipe as share-notice.
    // region-window: the hollow Petal View selector; its interior must stay
    // transparent so desktop pixels remain visible inside the boundary.
    /^\/(compositor|hover-tab|share-border|menubar-popover|share-notice|control-consent|region-window)([/.]|$)/.test(routePath)
  );
</script>

<svelte:head>
  {#if showAppShell}
    <!-- Raw <style> in svelte:head is NOT compiled by Svelte — plain CSS
         selectors only (`:global()` would be a literal, invalid selector).
         The window is transparent; .app-shell (below) paints the opaque
         rounded surface (and carries the 28px drag-strip clearance the body
         used to carry, so the strip band is INSIDE the rounded shell). -->
    <style>
      body { background: transparent; }
    </style>
  {/if}
  {#if isOverlayRoute}
    <style>
      html,
      body {
        background: transparent !important;
        padding: 0 !important;
        margin: 0 !important;
      }
    </style>
  {/if}
</svelte:head>

{#if showAppShell}
  <div
    class="app-shell"
    class:main-shell={isMainRoute}
    class:meeting-shell={isMeetingRoute}
    class:settings-shell={isSettingsRoute}
    class:content-shell={isOnboardingRoute || isSettingsRoute}
  >
    {#if showDragStrip}
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div class="shell-drag-surface" data-tauri-drag-region aria-hidden="true"></div>
    {/if}
    <ToastHost />
    <div class="route-surface">
      {#key routeRemountKey}
        {@render children()}
      {/key}
    </div>
    {#if showStatusBar}
      <footer class="status-bar" aria-label="Build version">{statusText}</footer>
    {/if}
  </div>
{:else}
  <ToastHost />
  <div class="route-surface">
    {#key routeRemountKey}
      {@render children()}
    {/key}
  </div>
{/if}

<!-- Right-click menu replacing the webview engine's default (WebView2
     browser chrome on Windows, WKWebView menu on macOS) — see component. -->
<ContextMenu />

<style>
  /* The opaque rounded app shell clips the transparent main window to the
     approved 24px shape without drawing an extra gray perimeter stroke.
     (macOS; Windows corners are DWM-native — see app.css.)
     `translateZ(0)` makes this the containing block
     for fixed-position descendants (e.g. the meeting route's picker
     panel), so overflow:hidden genuinely clips EVERYTHING to the rounded
     corners — a fixed overlay would otherwise escape the clip and paint
     square corners over the transparent window. */
  .app-shell {
    position: relative;
    height: 100%;
    padding-top: 28px;
    /* Bottom band reserved for the status bar: the route surface ends here,
       and the footer text (bottom: 7px, ~12px tall) sits centered in the
       remaining space — 7px clearance above and below the text. */
    padding-bottom: 26px;
    box-sizing: border-box;
    background: var(--bg-base);
    border: none;
    border-radius: var(--radius-shell);
    overflow: hidden;
    overscroll-behavior: none;
    transform: translateZ(0);
  }

  .shell-drag-surface {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    height: 28px;
    z-index: 3;
    background: inherit;
    border-radius: var(--radius-shell) var(--radius-shell) 0 0;
  }

  .app-shell.meeting-shell {
    padding-top: 0;
    padding-bottom: 0;
  }

  .app-shell.settings-shell {
    padding-top: 0;
  }

  /* Onboarding/settings route content paints bg-base-2. Match the shell's
     drag-strip band to that surface so transparent windows do not show a
     dark titlebar stripe above the route. */
  .app-shell.content-shell {
    background: var(--bg-base-2);
  }

  /* MainMenu owns its visible top-bar drag region. Keeping the generic 28px
     strip here would create empty top clearance and intercept the avatar. */
  .app-shell.main-shell {
    padding-top: 0;
  }

  .status-bar {
    position: absolute;
    left: 16px;
    right: 16px;
    bottom: 7px;
    z-index: 2;
    pointer-events: none;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
    color: var(--text-faint);
    font: 500 10px var(--font-mono);
    font-variant-numeric: tabular-nums;
    text-align: center;
  }

  /* Pill mode: nothing but the pill (its own rounded surface + shadow) may
     render — the shell surface and top clearance both go away. */
  :global(body.pill-mode) .app-shell {
    background: transparent;
    border: none;
    border-radius: 0;
    padding-top: 0;
    padding-bottom: 0;
  }

  /* Route-transition target: only the page content transitions. The shell,
     drag strip, and status bar stay pinned, and the scale below can only
     reveal the shell's own opaque background (the wrapper lives inside the
     rounded, overflow-hidden .app-shell) — never the transparent window
     corners or the desktop. */
  .route-surface {
    height: 100%;
    view-transition-name: petal-route;
  }

  :global(::view-transition-old(petal-route)) {
    animation: petal-route-out var(--motion-enter) var(--ease-exit) both;
  }
  :global(::view-transition-new(petal-route)) {
    animation: petal-route-in var(--motion-enter) var(--ease-standard) both;
  }
  @keyframes petal-route-out {
    to {
      opacity: 0;
      transform: translateY(calc(var(--motion-distance) * -1));
    }
  }
  @keyframes petal-route-in {
    from {
      opacity: 0;
      transform: translateY(var(--motion-distance));
    }
  }
  @media (prefers-reduced-motion: reduce) {
    :global(::view-transition-old(petal-route)),
    :global(::view-transition-new(petal-route)) {
      animation: none;
    }
  }
</style>
