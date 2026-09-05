<!--
  Persistent click-through chrome for one actively-shared local window (see
  src-tauri/src/share_border.rs). The native side creates one panel per shared
  window, sized to that real source-window frame. This route draws only the
  colored border; the interactive "Stop sharing" tab remains the hover-tab
  route so there is exactly one tab.

  This panel stays click-through so it never blocks the shared app underneath;
  the interactive share/unshare control remains the hover-tab route.
-->
<script lang="ts">
  import { page } from '$app/state';
  import { onMount, tick } from 'svelte';

  const DEFAULT_SHARE_COLOR = '#f06cc9';
  const DEFAULT_BORDER_STROKE = 4;
  const DEFAULT_BORDER_RADIUS = 10;
  const SHARE_BORDER_REVEAL_EVENT = 'petal-share-border-reveal';

  type RevealState = 'idle' | 'pending' | 'active';

  const color = $derived(page.url.searchParams.get('color') ?? DEFAULT_SHARE_COLOR);
  const borderTop = $derived(cssPxParam(page.url.searchParams.get('borderTop'), '0px'));
  const windowWidth = $derived(cssPxParam(page.url.searchParams.get('windowWidth'), '100vw'));
  const windowHeight = $derived(cssPxParam(page.url.searchParams.get('windowHeight'), '100vh'));

  let shell: HTMLElement | undefined = $state();
  let borderSvg: SVGSVGElement | undefined = $state();
  let borderPath: SVGPathElement | undefined = $state();
  let revealKey = $state(0);
  let revealState = $state<RevealState>(
    page.url.searchParams.get('animate') === '1' ? 'pending' : 'idle'
  );
  const initialPathWidth = numberParam(page.url.searchParams.get('windowWidth'), 1);
  const initialPathHeight = numberParam(page.url.searchParams.get('windowHeight'), 1);
  let pathWidth = $state(initialPathWidth);
  let pathHeight = $state(initialPathHeight);
  let pathData = $state(
    shareBorderPath(
      initialPathWidth,
      initialPathHeight,
      DEFAULT_BORDER_STROKE,
      DEFAULT_BORDER_RADIUS
    )
  );
  let pathLength = $state(0);

  function cssPxParam(value: string | null, fallback: string): string {
    const parsed = Number(value);
    if (!Number.isFinite(parsed) || parsed < 0 || parsed > 10000) return fallback;
    return `${parsed}px`;
  }

  function numberParam(value: string | null, fallback: number): number {
    const parsed = Number(value);
    if (!Number.isFinite(parsed) || parsed < 0 || parsed > 10000) return fallback;
    return parsed;
  }

  function cssNumber(value: string, fallback: number): number {
    const parsed = Number.parseFloat(value);
    if (!Number.isFinite(parsed) || parsed < 0 || parsed > 10000) return fallback;
    return parsed;
  }

  function shareBorderPath(
    width: number,
    height: number,
    stroke: number,
    radius: number
  ): string {
    const w = Math.max(1, width);
    const h = Math.max(1, height);
    const s = Math.max(0, stroke);
    const r = Math.max(0, radius - s / 2);
    const halfStroke = s / 2;
    const left = halfStroke;
    const top = halfStroke;
    const right = w - halfStroke;
    const bottom = h - halfStroke;
    return [
      `M ${left + r},${top}`,
      `L ${right - r},${top}`,
      `A ${r},${r} 0 0 1 ${right},${top + r}`,
      `L ${right},${bottom - r}`,
      `A ${r},${r} 0 0 1 ${right - r},${bottom}`,
      `L ${left + r},${bottom}`,
      `A ${r},${r} 0 0 1 ${left},${bottom - r}`,
      `L ${left},${top + r}`,
      `A ${r},${r} 0 0 1 ${left + r},${top}`,
      'Z'
    ].join(' ');
  }

  async function recomputePath() {
    const svg = borderSvg;
    const root = shell;
    if (!svg || !root) return;

    const rect = svg.getBoundingClientRect();
    const width = Math.max(1, rect.width);
    const height = Math.max(1, rect.height);
    const style = getComputedStyle(root);
    const stroke = cssNumber(
      style.getPropertyValue('--share-border-stroke'),
      DEFAULT_BORDER_STROKE
    );
    const radius = cssNumber(
      style.getPropertyValue('--share-border-radius'),
      DEFAULT_BORDER_RADIUS
    );

    pathWidth = width;
    pathHeight = height;
    pathData = shareBorderPath(width, height, stroke, radius);

    await tick();
    try {
      const length = borderPath?.getTotalLength() ?? 0;
      pathLength = Number.isFinite(length) ? length : 0;
    } catch {
      pathLength = 0;
    }
  }

  async function replayReveal() {
    revealState = 'pending';
    revealKey += 1;
    await tick();
    await recomputePath();
    requestAnimationFrame(() => {
      revealState = 'active';
    });
  }

  onMount(() => {
    const shouldAnimate = page.url.searchParams.get('animate') === '1';
    const reveal = () => {
      void replayReveal();
    };

    window.addEventListener(SHARE_BORDER_REVEAL_EVENT, reveal);

    const observer = new ResizeObserver(() => {
      void recomputePath();
    });
    if (shell) observer.observe(shell);

    if (shouldAnimate) {
      void replayReveal();
    } else {
      void recomputePath();
    }

    return () => {
      observer.disconnect();
      window.removeEventListener(SHARE_BORDER_REVEAL_EVENT, reveal);
    };
  });
</script>

<div
  bind:this={shell}
  class="share-border-shell"
  style:--share-color={color}
  style:--share-border-top={borderTop}
  style:--share-border-width={windowWidth}
  style:--share-border-height={windowHeight}
>
  {#key revealKey}
    <svg
      bind:this={borderSvg}
      class="share-border"
      viewBox={`0 0 ${pathWidth} ${pathHeight}`}
      preserveAspectRatio="none"
      aria-hidden="true"
      focusable="false"
    >
      <path
        bind:this={borderPath}
        class="share-border-path"
        data-reveal={revealState}
        d={pathData}
        style:--share-border-path-length={pathLength}
      />
    </svg>
  {/key}
</div>

<style>
  :global(html),
  :global(body) {
    background: transparent;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  .share-border-shell {
    position: fixed;
    inset: 0;
    width: 100vw;
    height: 100vh;
    pointer-events: none;
    font-family: var(--font-ui, -apple-system, system-ui, sans-serif);
  }

  .share-border {
    position: absolute;
    left: 0;
    top: var(--share-border-top, 0px);
    z-index: 1;
    width: var(--share-border-width, 100vw);
    height: var(--share-border-height, 100vh);
    overflow: visible;
    pointer-events: none;
  }

  .share-border-path {
    fill: none;
    stroke: var(--share-color);
    stroke-width: var(--share-border-stroke, 4px);
    stroke-linejoin: round;
    stroke-dashoffset: 0;
    vector-effect: non-scaling-stroke;
  }

  .share-border-path[data-reveal='pending'],
  .share-border-path[data-reveal='active'] {
    stroke-dasharray: var(--share-border-path-length, 0) var(--share-border-path-length, 0);
  }

  .share-border-path[data-reveal='pending'] {
    opacity: 0;
    stroke-dashoffset: var(--share-border-path-length, 0);
  }

  .share-border-path[data-reveal='active'] {
    opacity: 1;
    animation: share-border-sweep var(--share-border-sweep-duration, 420ms)
      var(--ease-standard, cubic-bezier(0.2, 0, 0, 1)) both;
  }

  @keyframes share-border-sweep {
    from {
      stroke-dashoffset: var(--share-border-path-length, 0);
    }

    to {
      stroke-dashoffset: 0;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .share-border-path,
    .share-border-path[data-reveal='pending'],
    .share-border-path[data-reveal='active'] {
      animation: none;
      opacity: 1;
      stroke-dashoffset: 0;
    }
  }
</style>
