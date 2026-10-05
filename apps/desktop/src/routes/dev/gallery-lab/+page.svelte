<!--
  Native gallery layout lab (dev-only). The desktop counterpart of the web
  client's layout lab and meeting-layout matrix (#239): the REAL meeting
  composition -- MeetingChrome, Gallery, ParticipantTile, the chat drawer and
  a plugin button -- at any window size, with synthetic cameras, so every
  state can be looked at without a meeting. Drag the frame's corner to
  resize it like the window; the URL holds the scenario, so a state can be
  shared as a link. The same scenarios run unattended, with screenshots and
  pass/fail rules, in scripts/verify-native-gallery-matrix.mjs.

  Gallery lays itself out from its own measured size, not the viewport, so
  a frame smaller than this page shows exactly what a window that size does.
-->
<script lang="ts">
  import { onMount } from 'svelte';
  import GalleryLabStage from '$lib/dev/GalleryLabStage.svelte';
  import {
    LAB_WINDOW_PRESETS,
    clampScenario,
    scenarioFromQuery,
    scenarioToQuery,
    type GalleryLabScenario
  } from '$lib/dev/galleryLab';
  import { judgeGalleryLab, measureGalleryLab, type GalleryLabReading } from '$lib/dev/galleryLabMeasure';

  let scenario = $state<GalleryLabScenario>(scenarioFromQuery(typeof location === 'undefined' ? '' : location.hash));
  let reading = $state<GalleryLabReading | null>(null);
  let frameEl = $state<HTMLDivElement>();

  function update(patch: Partial<GalleryLabScenario>) {
    scenario = clampScenario({ ...scenario, ...patch });
  }

  $effect(() => {
    const query = scenarioToQuery(scenario);
    if (typeof history !== 'undefined') history.replaceState(null, '', `#${query}`);
  });

  function remeasure() {
    const windowEl = frameEl?.querySelector<HTMLElement>('[data-lab-window]');
    if (windowEl) reading = measureGalleryLab(windowEl);
  }

  // Re-read after layout settles (tiles FLIP for up to ~400ms).
  let measureTimer: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    void scenarioToQuery(scenario);
    clearTimeout(measureTimer);
    measureTimer = setTimeout(remeasure, 600);
    return () => clearTimeout(measureTimer);
  });

  // The frame's resize handle drives the window size.
  onMount(() => {
    const frame = frameEl;
    if (!frame || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => {
      const width = Math.round(frame.clientWidth);
      const height = Math.round(frame.clientHeight);
      if (width > 0 && height > 0 && (width !== scenario.width || height !== scenario.height)) update({ width, height });
    });
    observer.observe(frame);
    return () => observer.disconnect();
  });

  const pct = (value: number) => `${Math.round(value * 100)}%`;
  const problems = $derived(reading ? judgeGalleryLab(scenario, reading) : []);
</script>

<div class="lab">
  <header>
    <h1>Native gallery layout lab</h1>
    <p>
      The real meeting window (MeetingChrome → Gallery) with synthetic cameras. Drag the corner of the frame to
      resize it like the window. Unattended: <code>node scripts/verify-native-gallery-matrix.mjs --out /tmp/m</code>.
    </p>
  </header>

  <section class="controls" aria-label="Scenario">
    <div class="presets">
      {#each LAB_WINDOW_PRESETS as preset (preset.key)}
        <button
          type="button"
          class:active={scenario.width === preset.width && scenario.height === preset.height}
          onclick={() => update({ width: preset.width, height: preset.height })}>{preset.label}</button
        >
      {/each}
    </div>
    <label>Width <input type="number" min="120" max="2400" value={scenario.width} onchange={(e) => update({ width: Number(e.currentTarget.value) })} /></label>
    <label>Height <input type="number" min="120" max="1600" value={scenario.height} onchange={(e) => update({ height: Number(e.currentTarget.value) })} /></label>
    <label>People <input type="range" min="1" max="16" value={scenario.count} oninput={(e) => update({ count: Number(e.currentTarget.value) })} /> <b>{scenario.count}</b></label>
    <label>Cameras off <input type="range" min="0" max={scenario.count - 1} value={scenario.camerasOff} oninput={(e) => update({ camerasOff: Number(e.currentTarget.value) })} /> <b>{scenario.camerasOff}</b></label>
    <label
      >Camera shape
      <select value={scenario.aspect} onchange={(e) => update({ aspect: e.currentTarget.value as GalleryLabScenario['aspect'] })}>
        <option value="16:9">16:9 webcam</option>
        <option value="4:3">4:3 webcam</option>
        <option value="9:16">9:16 phone</option>
        <option value="mixed">mixed</option>
      </select>
    </label>
    <label
      >View
      <select value={scenario.mode} onchange={(e) => update({ mode: e.currentTarget.value as GalleryLabScenario['mode'] })}>
        <option value="grid">grid</option>
        <option value="spotlight">spotlight</option>
      </select>
    </label>
    <label>Plugin buttons <input type="range" min="0" max="3" value={scenario.plugins} oninput={(e) => update({ plugins: Number(e.currentTarget.value) })} /> <b>{scenario.plugins}</b></label>
    <label><input type="checkbox" checked={scenario.chatOpen} onchange={(e) => update({ chatOpen: e.currentTarget.checked })} /> Chat open</label>
    <label><input type="checkbox" checked={scenario.stateCard} onchange={(e) => update({ stateCard: e.currentTarget.checked })} /> Reconnecting card</label>
    <label><input type="checkbox" checked={scenario.longNames} onchange={(e) => update({ longNames: e.currentTarget.checked })} /> Long names</label>
    <label><input type="checkbox" checked={scenario.sharing} onchange={(e) => update({ sharing: e.currentTarget.checked })} /> Someone sharing</label>
  </section>

  {#if reading}
    <p class="readout" class:bad={problems.length > 0}>
      {reading.mode} · tiles {pct(reading.tileShare)} of the window · box {pct(reading.surfaceShare)} · packed {pct(reading.packing)} ·
      smallest {Math.round(reading.smallestTile.width)}×{Math.round(reading.smallestTile.height)} ·
      {reading.galleryClasses.filter((c) => c !== 'gallery').join(' ') || 'regular chrome'}
      {#if reading.collapsedControls.length}· in More: {reading.collapsedControls.join(', ')}{/if}
      {#if problems.length}<br />{problems.join(' · ')}{/if}
    </p>
  {/if}

  <div class="frame" bind:this={frameEl} style:width="{scenario.width}px" style:height="{scenario.height}px">
    <GalleryLabStage {scenario} onSettled={remeasure} />
  </div>
</div>

<style>
  .lab {
    padding: 20px 24px 48px;
    color: var(--text-primary);
    font: 400 13px var(--font-ui);
  }

  h1 {
    margin: 0 0 6px;
    font-size: 20px;
  }

  header p {
    margin: 0 0 14px;
    max-width: 760px;
    color: var(--text-soft);
    line-height: 1.5;
  }

  .controls {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px 16px;
    padding: 12px 14px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    background: var(--fill-weak);
  }

  .presets {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    width: 100%;
  }

  .presets button {
    padding: 4px 10px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-chip);
    background: transparent;
    color: var(--text-soft);
    font: 600 11px var(--font-ui);
    cursor: pointer;
  }

  .presets button.active {
    background: var(--fill-strong);
    color: var(--text-strong);
  }

  label {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    white-space: nowrap;
  }

  input[type='number'] {
    width: 72px;
  }

  .readout {
    margin: 12px 0;
    font: 500 12px var(--font-mono);
    color: var(--text-soft);
  }

  .readout.bad {
    color: var(--warning);
  }

  /* `resize: both` makes the frame the window's drag-resize handle.
     content-box: its client size IS the window size the observer reads
     back (border-box would shrink it by the border on every read). */
  .frame {
    box-sizing: content-box;
    resize: both;
    overflow: hidden;
    min-width: 120px;
    min-height: 120px;
    border: 1px dashed var(--hairline-strong);
  }

  .frame :global([data-lab-window]) {
    width: 100% !important;
    height: 100% !important;
  }
</style>
