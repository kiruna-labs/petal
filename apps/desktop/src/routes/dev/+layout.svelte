<script lang="ts">
  import { page } from '$app/state';

  let { children } = $props();

  // Keep this list in sync with routes/dev/*/+page.svelte — SvelteKit doesn't
  // auto-enumerate sibling routes, so a new dev harness needs a line here too.
  const harnesses = [
    { path: '/dev/components', label: 'Components' },
    { path: '/dev/main-menu', label: 'Main menu' },
    { path: '/dev/menubar-popover', label: 'Menubar popover' },
    { path: '/dev/network-cockpit', label: 'Network cockpit' },
    { path: '/dev/onboarding', label: 'Onboarding' },
    { path: '/dev/secondary', label: 'Secondary surfaces' },
    { path: '/dev/settings', label: 'Settings' },
    { path: '/dev/telepointer', label: 'Telepointer' },
    { path: '/dev/test-pattern', label: 'Test pattern' },
    { path: '/dev/test-pattern-status', label: 'Test pattern status' },
    { path: '/dev/update-toast', label: 'Update toast' }
  ];

  const currentPath = $derived(page.url.pathname.replace(/\/$/, '') || '/');

  // #499: test-pattern/test-pattern-status are captured pixel-for-pixel by
  // the Test Cockpit (calibration squares expected at fixed absolute
  // coordinates like (28,28), assuming a raw 960x600 render starting at the
  // window's origin). The shared dev-harness nav bar this layout adds pushes
  // that content down by the nav's height, so the cockpit's fixed-coordinate
  // pixel check silently samples the nav bar's own chrome (e.g. the "Dev
  // harnesses" pill) instead of the calibration square -- a plausible-looking
  // but wrong color, not a color-space/encoding bug (confirmed live: the
  // captured verdict screenshot showed the nav bar sitting directly on top of
  // the test pattern). These two routes need pixel-exact, unwrapped output;
  // skip the nav chrome for them specifically rather than exempting them from
  // this file's route list (they still belong in the harness index above).
  //
  // Match with-or-without a trailing ".html": the nav links above (and a
  // browser opening the clean SvelteKit route) produce a suffix-less
  // pathname like "/dev/test-pattern", but the native app's WebviewUrl::App
  // loads the literal static file path "dev/test-pattern.html" directly
  // (dev_test_pattern.rs), so page.url.pathname is "/dev/test-pattern.html"
  // in that case -- a bare `===` comparison against the suffix-less string
  // silently never matched for the one client (the real Petal app) this fix
  // exists for, while still matching in any ordinary browser tab used to
  // eyeball it, which is exactly why this looked fixed under manual
  // spot-checking (confirmed live, both ways, before landing this).
  const suppressNav = $derived(
    currentPath === '/dev/test-pattern' ||
      currentPath === '/dev/test-pattern.html' ||
      currentPath === '/dev/test-pattern-status' ||
      currentPath === '/dev/test-pattern-status.html'
  );
</script>

{#if suppressNav}
  {@render children()}
{:else}
  <div class="dev-layout">
    <nav class="dev-nav" aria-label="Development harness routes">
      <span class="dev-nav-label">Dev harnesses</span>
      <div class="dev-nav-links">
        {#each harnesses as harness}
          <a
            href={harness.path}
            class:current={currentPath === harness.path}
            aria-current={currentPath === harness.path ? 'page' : undefined}
          >{harness.label}</a>
        {/each}
      </div>
    </nav>

    <div class="dev-content">
      {@render children()}
    </div>
  </div>
{/if}

<style>
  .dev-layout {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    background: var(--bg-base);
  }

  .dev-nav {
    position: sticky;
    top: 0;
    z-index: 10;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 12px;
    flex: 0 0 auto;
    padding: 10px 14px;
    color: var(--text-muted);
    background: var(--surface);
    box-shadow: 0 1px 0 var(--hairline-strong), var(--shadow-float);
    font-family: var(--font-ui);
    font-size: var(--text-micro);
  }

  .dev-nav-label {
    flex: 0 0 auto;
    color: var(--text-primary);
    font-family: var(--font-mono);
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .dev-nav-links {
    display: flex;
    flex: 1 1 480px;
    flex-wrap: wrap;
    gap: 6px;
    min-width: 0;
  }

  .dev-nav a {
    display: inline-flex;
    align-items: center;
    min-height: 40px;
    padding: 7px 10px;
    border-radius: var(--radius-chip);
    color: var(--text-muted);
    text-decoration: none;
    white-space: nowrap;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .dev-nav a:hover {
    color: var(--text-primary);
    background: var(--surface-raised);
  }

  .dev-nav a:active {
    transform: scale(var(--press-scale));
  }

  .dev-nav a:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .dev-nav a.current {
    color: var(--text-primary);
    background: var(--plum-tint-bg);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--plum) 45%, transparent);
    font-weight: 700;
  }

  .dev-content {
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
  }

  @media (max-width: 640px) {
    .dev-nav {
      align-items: flex-start;
      flex-direction: column;
      gap: 6px;
    }

    .dev-nav-links {
      flex-basis: auto;
      width: 100%;
    }
  }
</style>
