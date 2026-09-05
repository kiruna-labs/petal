<!--
  NetworkCockpit — the in-depth network/system conditions panel behind the
  Gallery topbar's network icon (issue #19, Phase A). It stays a
  persistent separate Tauri window, but uses the same graphite design-system
  surface vocabulary as the in-app modal and secondary panels.

  Data flow (all real, no fabricated numbers — absent metrics render as "—"):
  - On open: `set_cockpit_open(true)` (gates the Rust poller's ~1s push),
    then `get_network_snapshot` + `get_event_journal` for initial state.
  - Live: `network-stats` (~1s full snapshot while open) and
    `journal-appended` (per journal entry) Tauri events.
  - On close/unmount: `set_cockpit_open(false)` + unlisten.

  Sparklines are hand-rolled static SVG paths over the Rust-side ring buffer
  (~120 samples ≈ 2 min) — no charting dependency, no animated drawing (so
  prefers-reduced-motion needs no special-casing; nothing moves except the
  data itself updating).

  `initialSnapshot`/`initialJournal` exist ONLY for the /dev harness and a
  plain-browser preview (no Tauri bridge): they seed the display so layout is
  reviewable; a real Tauri backend immediately overwrites them with live data.
-->
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import CloseButton from './CloseButton.svelte';
  import { COMMANDS, EVENTS } from '$lib/ipc';
  import {
    buildPipelineRows,
    buildGaugeCockpit,
    buildGaugeSeries,
    fmt,
    fmtKbps,
    fmtTrackFrames,
    fmtTrackJitterBuffer,
    fmtTrackLatency,
    fmtTrackRtcp,
    GAUGE_ZONE_LINES,
    sampleWindow,
    smoothAreaPath,
    smoothLinePath,
    sparkPath,
    stateLabel,
    ts,
    type GaugeModel,
    type JournalEntry,
    type NetworkSnapshot,
    type PipelineNodeModel
  } from '$lib/data/networkCockpit';

  interface Props {
    onClose?: () => void;
    /** True when hosted as its own Tauri window instead of an embedded panel. */
    standalone?: boolean;
    /** Harness/browser-preview seed data only — see header comment. */
    initialSnapshot?: NetworkSnapshot;
    initialJournal?: JournalEntry[];
  }

  let { onClose, standalone = false, initialSnapshot, initialJournal }: Props = $props();

  const emptySnapshot: NetworkSnapshot = {
    connected: false,
    roomName: null,
    serverHost: null,
    localIdentity: null,
    reconnectCount: 0,
    quality: [],
    peerRttMs: null,
    history: [],
    tracks: [],
    nativeStartup: [],
    analysis: []
  };

  // Capturing only the INITIAL prop values is deliberate: these are one-shot
  // harness/preview seeds (see header comment), immediately superseded by
  // live Tauri data when a backend exists — they must not stay reactive.
  // svelte-ignore state_referenced_locally
  let snapshot = $state<NetworkSnapshot>(initialSnapshot ?? emptySnapshot);
  // svelte-ignore state_referenced_locally
  let journal = $state<JournalEntry[]>(initialJournal ?? []);
  /** True once a real Tauri backend answered — distinguishes "not in a
   *  meeting" from "no backend at all (browser preview)". */
  let live = $state(false);
  let filter = $state<'all' | 'connection' | 'presence' | 'shares' | 'media'>('all');

  let unlistenStats: UnlistenFn | undefined;
  let unlistenJournal: UnlistenFn | undefined;

  onMount(async () => {
    try {
      await invoke(COMMANDS.setCockpitOpen, { open: true });
      snapshot = await invoke<NetworkSnapshot>(COMMANDS.getNetworkSnapshot);
      journal = await invoke<JournalEntry[]>(COMMANDS.getEventJournal);
      live = true;
      unlistenStats = await listen<NetworkSnapshot>(EVENTS.networkStats, (e) => {
        snapshot = e.payload;
      });
      unlistenJournal = await listen<JournalEntry>(EVENTS.journalAppended, (e) => {
        journal = [...journal, e.payload].slice(-500);
      });
    } catch {
      // No Tauri backend (plain browser / dev harness): keep seed data.
    }
  });

  onDestroy(() => {
    unlistenStats?.();
    unlistenJournal?.();
    invoke(COMMANDS.setCockpitOpen, { open: false }).catch(() => {});
  });

  const latest = $derived(snapshot.history.at(-1));
  const filteredJournal = $derived(
    [...journal].reverse().filter((e) => filter === 'all' || e.category === filter)
  );
  const gaugeCockpit = $derived.by(() => buildGaugeCockpit(snapshot, live));
  const gaugeSeries = $derived.by(() => buildGaugeSeries(snapshot));
  const pipelineRows = $derived.by(() => buildPipelineRows(snapshot.tracks));
  const sampleWindowLabel = $derived(sampleWindow(snapshot.history));

  const FILTERS = ['all', 'connection', 'presence', 'shares', 'media'] as const;
  const rttSeries = $derived(snapshot.history.map((s) => s.rttMs));
  const jitterSeries = $derived(snapshot.history.map((s) => s.jitterMs));
  const sendSeries = $derived(snapshot.history.map((s) => s.sendKbps));
  const recvSeries = $derived(snapshot.history.map((s) => s.recvKbps));
  const headerDragRegion = $derived(standalone ? '' : undefined);
</script>

{#snippet gaugeCard(gauge: GaugeModel, prominent = false)}
  {@const rawSeries = gaugeSeries[gauge.id] ?? []}
  {@const hasSamples = rawSeries.some((v) => v !== null && !Number.isNaN(v))}
  {@const series = hasSamples ? rawSeries : gauge.score !== null ? [gauge.score] : []}
  {@const linePath = smoothLinePath(series)}
  {@const areaPath = smoothAreaPath(series)}
  <article
    class={`gauge-card tone-${gauge.tone} state-${gauge.state}`}
    class:prominent
    aria-label={`${gauge.label}: ${gauge.value}. ${gauge.detail}. ${stateLabel(gauge.state)}. Health trend over the sample window.`}
  >
    <div class="gauge-title-row">
      <span class="gauge-label">{gauge.label}</span>
      <span class="gauge-state">{stateLabel(gauge.state)}</span>
    </div>
    <div class="gauge-graphic">
      <svg class="gauge-graph" viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
        <rect class="gauge-zone" x="0" y="0" width="100" height="100" fill="url(#cockpit-health-zone)" />
        {#each GAUGE_ZONE_LINES as y (y)}
          <line class="gauge-thresh" x1="0" y1={y} x2="100" y2={y} vector-effect="non-scaling-stroke" />
        {/each}
        {#if linePath}
          <path class="gauge-area" d={areaPath} />
          <path class="gauge-line" d={linePath} vector-effect="non-scaling-stroke" />
        {/if}
      </svg>
      <span class="gauge-value">{gauge.value}</span>
    </div>
    <span class="gauge-detail">{gauge.detail}</span>
  </article>
{/snippet}

{#snippet pipelineNode(node: PipelineNodeModel)}
  <article class={`pipeline-node node-${node.state}`} aria-label={`${node.label}: ${node.value}. ${node.detail}.`}>
    <span class="pipeline-node-label">{node.label}</span>
    <span class="pipeline-node-value">{node.value}</span>
    <span class="pipeline-node-detail">{node.detail}</span>
  </article>
{/snippet}

<div class="cockpit" class:standalone role="dialog" aria-label="Network conditions">
  <!-- Shared red→amber→green health zones for every trend graph. Coincident
       stops at the tone boundaries (score 66 → 34%, score 42 → 58%) give
       legible zones while low opacity keeps them a soft background. -->
  <svg class="zone-defs" width="0" height="0" aria-hidden="true" focusable="false">
    <defs>
      <linearGradient id="cockpit-health-zone" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0" class="zone-good-top" />
        <stop offset="0.34" class="zone-good-bottom" />
        <stop offset="0.34" class="zone-warn-top" />
        <stop offset="0.58" class="zone-warn-bottom" />
        <stop offset="0.58" class="zone-bad-top" />
        <stop offset="1" class="zone-bad-bottom" />
      </linearGradient>
    </defs>
  </svg>
  <header class="head" data-tauri-drag-region={headerDragRegion}>
    <span class="title" data-tauri-drag-region={headerDragRegion}>Network conditions</span>
    <span class="conn-state" class:on={snapshot.connected} data-tauri-drag-region={headerDragRegion}>
      {snapshot.connected ? 'connected' : live ? 'not in a meeting' : 'no live data'}
    </span>
    {#if onClose}
      <div class="close-slot">
        <CloseButton onclick={() => onClose?.()} />
      </div>
    {/if}
  </header>

  <div class="body">
    <!-- Health gauges -->
    <section class="gauge-cockpit" aria-label="Health cockpit">
      <div class="gauge-cockpit-head">
        <h3>Health cockpit</h3>
        <span>{sampleWindowLabel}</span>
      </div>
      <div class="gauge-layout">
        {@render gaugeCard(gaugeCockpit.overall, true)}
        <div class="dimension-gauges">
          {#each gaugeCockpit.dimensions as gauge (gauge.id)}
            {@render gaugeCard(gauge)}
          {/each}
        </div>
      </div>
    </section>

    <!-- Connection -->
    <section>
      <h3>Connection</h3>
      <div class="kv-grid">
        <span class="k">Room</span><span class="v">{snapshot.roomName ?? '—'}</span>
        <span class="k">Server</span><span class="v">{snapshot.serverHost ?? '—'}</span>
        <span class="k">Identity</span><span class="v">{snapshot.localIdentity ?? '—'}</span>
        <span class="k">Reconnects</span><span class="v">{snapshot.reconnectCount}</span>
      </div>
      {#if snapshot.quality.length > 0}
        <div class="quality-list">
          {#each snapshot.quality as q (q.identity)}
            <span class="quality-chip" class:bad={q.quality === 'poor' || q.quality === 'lost'}>
              {q.identity}: <span class="v">{q.quality}</span>
            </span>
          {/each}
        </div>
      {/if}
    </section>

    <!-- Latency -->
    <section>
      <h3>Latency</h3>
      <div class="metric-row">
        <div class="metric">
          <span class="metric-label">RTT to server</span>
          <span class="metric-value">{fmt(latest?.rttMs, 1, ' ms')}</span>
          <svg class="spark" viewBox="0 0 150 30" aria-hidden="true"><path d={sparkPath(rttSeries)} /></svg>
        </div>
        <div class="metric">
          <span class="metric-label">Peer-to-peer RTT</span>
          <span class="metric-value">{fmt(snapshot.peerRttMs, 1, ' ms')}</span>
        </div>
        <div class="metric">
          <span class="metric-label">Jitter</span>
          <span class="metric-value">{fmt(latest?.jitterMs, 1, ' ms')}</span>
          <svg class="spark" viewBox="0 0 150 30" aria-hidden="true"><path d={sparkPath(jitterSeries)} /></svg>
        </div>
        <div class="metric">
          <span class="metric-label">Packet loss</span>
          <span class="metric-value">{fmt(latest?.lossPct, 2, ' %')}</span>
        </div>
      </div>
    </section>

    <!-- Bandwidth -->
    <section>
      <h3>Bandwidth</h3>
      <div class="metric-row">
        <div class="metric">
          <span class="metric-label">Send</span>
          <span class="metric-value">{fmtKbps(latest?.sendKbps ?? null)}</span>
          <svg class="spark" viewBox="0 0 150 30" aria-hidden="true"><path d={sparkPath(sendSeries)} /></svg>
        </div>
        <div class="metric">
          <span class="metric-label">Receive</span>
          <span class="metric-value">{fmtKbps(latest?.recvKbps ?? null)}</span>
          <svg class="spark" viewBox="0 0 150 30" aria-hidden="true"><path d={sparkPath(recvSeries)} /></svg>
        </div>
      </div>
    </section>

    <!-- Analysis -->
    <section>
      <h3>Analysis</h3>
      {#if snapshot.analysis.length === 0}
        <p class="empty">Waiting for enough samples to analyze.</p>
      {:else}
        <div class="analysis-list">
          {#each snapshot.analysis as f (f.title + f.evidence)}
            <article class="finding" class:warn={f.severity === 'warn'}>
              <div class="finding-head">
                <span class="finding-severity">{f.severity}</span>
                <span class="finding-title">{f.title}</span>
              </div>
              <p class="finding-evidence">{f.evidence}</p>
              <p class="finding-rec">{f.recommendation}</p>
            </article>
          {/each}
        </div>
      {/if}
    </section>

    <!-- Media health -->
    <section>
      <h3>Media health</h3>
      {#if snapshot.tracks.length === 0}
        <p class="empty">No active tracks.</p>
      {:else}
        <div class="tracks-scroll">
          <table class="tracks">
            <thead>
              <tr><th>Track</th><th>Dir</th><th>Res / fps</th><th>Latency</th><th>State</th><th>Codec</th><th>Bitrate (actual / target)</th><th>Limited</th><th>Lost</th><th>Frames</th><th>Dropped</th><th>RTCP</th><th>JB delay*</th></tr>
            </thead>
            <tbody>
              {#each snapshot.tracks as t (t.sid + t.direction)}
                <tr>
                  <td class="tname" title={t.name}>{t.name}</td>
                  <td>{t.direction}</td>
                  <td>{t.kind === 'video' ? `${t.width}×${t.height} @ ${fmt(t.fps, 0)}` : 'audio'}</td>
                  <td>{t.kind === 'video' && t.direction === 'recv' ? fmtTrackLatency(t) : '—'}</td>
                  <td class:warn-state={t.streamState === 'paused' || t.streamState === 'stalled'}>{t.streamState && t.streamState !== 'unknown' ? t.streamState : '—'}</td>
                  <td class="tname" title={t.codecImpl}>{t.codecImpl || '—'}</td>
                  <td>{fmtKbps(t.actualKbps)}{t.direction === 'send' && t.targetKbps > 0 ? ` / ${fmtKbps(t.targetKbps)}` : ''}</td>
                  <td>{t.softwareEncoder ? 'software encoder' : t.qualityLimitation && t.qualityLimitation !== 'none' ? t.qualityLimitation : '—'}</td>
                  <td>{t.packetsLost}</td>
                  <td>{fmtTrackFrames(t)}</td>
                  <td>{t.direction === 'recv' ? t.framesDropped : '—'}</td>
                  <td>{fmtTrackRtcp(t)}</td>
                  <td>{fmtTrackJitterBuffer(t)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
        <p class="footnote">* jitter-buffer delay is cumulative average / target / minimum when WebRTC reports all three.</p>
      {/if}
    </section>

    <!-- Pipeline -->
    <section>
      <h3>Pipeline</h3>
      {#if pipelineRows.length === 0}
        <p class="empty">No shared windows.</p>
      {:else}
        <div class="pipeline-list">
          {#each pipelineRows as row (row.id)}
            <article class={`pipeline-row source-${row.source}`}>
              <div class="pipeline-row-head">
                <span class="pipeline-title">{row.title}</span>
                <span class="pipeline-subtitle">{row.subtitle}</span>
              </div>
              <div class="pipeline-scroll">
                <div class="pipeline-flow" aria-label={`${row.title} media pipeline`}>
                  {#each row.nodes as node, i (node.id)}
                    {@render pipelineNode(node)}
                    {#if i < row.nodes.length - 1}
                      <svg class="pipeline-link" viewBox="0 0 32 12" aria-hidden="true">
                        <path d="M2 6H28" />
                        <path d="M24 2L28 6L24 10" />
                      </svg>
                    {/if}
                  {/each}
                </div>
              </div>
              <div class="pipeline-display">
                <span>{row.displayEnqueued.label}</span>
                <strong>{row.displayEnqueued.value}</strong>
                <em>{row.displayEnqueued.detail}</em>
              </div>
              <div class={`capture-state state-${row.captureState.tone}`}>
                <div class="capture-state-main">
                  <span>Capture</span>
                  <strong>{row.captureState.label}</strong>
                  <em>{row.captureState.detail}</em>
                </div>
                <div class="capture-state-metrics" aria-label={`${row.title} capture metrics`}>
                  <span>fps <b>{row.captureState.fps}</b></span>
                  <span>occlusion <b>{row.captureState.occlusion}</b></span>
                  <span>lock/copy <b>{row.captureState.lockCopyMs}</b></span>
                  <span>convert <b>{row.captureState.convertMs}</b></span>
                  <span>capture return <b>{row.captureState.captureFrameReturnMs}</b></span>
                </div>
              </div>
              <div class="receiver-freeze">
                <div class="capture-state-main">
                  <span>Receiver</span>
                  <strong>{row.receiverFreeze.label}</strong>
                  <em>{row.receiverFreeze.detail}</em>
                </div>
                <div class="capture-state-metrics" aria-label={`${row.title} receiver freeze metrics`}>
                  <span>freezes <b>{row.receiverFreeze.freezeCount}</b></span>
                  <span>dropped <b>{row.receiverFreeze.framesDropped}</b></span>
                  <span>limit <b>{row.receiverFreeze.qualityLimitationReason}</b></span>
                </div>
              </div>
            </article>
          {/each}
        </div>
      {/if}
    </section>

    <!-- Event log -->
    <section>
      <h3>Event log</h3>
      <div class="chips" role="group" aria-label="Filter events">
        {#each FILTERS as f (f)}
          <button type="button" class="chip" class:active={filter === f} onclick={() => (filter = f)}>{f}</button>
        {/each}
      </div>
      {#if filteredJournal.length === 0}
        <p class="empty">No events yet.</p>
      {:else}
        <ul class="log">
          {#each filteredJournal as e (e.tMs + e.message)}
            <li>
              <span class="log-ts">{ts(e.tMs)}</span>
              <span class="log-cat">{e.category}</span>
              <span class="log-msg">{e.message}</span>
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  </div>
</div>

<style>
  .cockpit {
    width: 680px;
    max-width: calc(100vw - 48px);
    max-height: min(680px, calc(100vh - 96px));
    display: flex;
    flex-direction: column;
    background: linear-gradient(180deg, var(--surface-raised), var(--surface));
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-panel);
    overflow: hidden;
    overscroll-behavior: none;
    font-family: var(--font-ui);
    color: var(--text-primary);
  }

  .cockpit.standalone {
    width: 100%;
    max-width: none;
    max-height: none;
    min-height: 100%;
    height: 100%;
    border: none;
    border-radius: 0;
    box-shadow: none;
    background: var(--bg-base);
  }

  .head {
    display: flex;
    align-items: center;
    gap: 12px;
    min-height: 58px;
    padding: 12px 14px 12px 18px;
    border-bottom: 1px solid var(--hairline);
    flex-shrink: 0;
    background: var(--fill-weak);
  }
  .title {
    font: 700 14px var(--font-ui);
    color: var(--text-primary);
    text-wrap: balance;
  }
  .conn-state {
    display: inline-flex;
    align-items: center;
    min-height: 24px;
    padding: 0 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-base);
    font: 600 10.5px var(--font-mono);
    color: var(--text-faint);
    text-transform: uppercase;
  }
  .conn-state.on {
    background: rgba(52, 199, 89, 0.12);
    color: var(--live-bright);
  }
  .close-slot {
    margin-left: auto;
    display: flex;
  }

  .body {
    overflow-y: auto;
    overscroll-behavior: none;
    padding: 6px 18px 18px;
  }

  .gauge-cockpit {
    padding-top: 16px;
  }
  .gauge-cockpit-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 10px;
  }
  .gauge-cockpit-head h3 {
    margin: 0;
  }
  .gauge-cockpit-head span {
    color: var(--text-faint);
    font: 600 10.5px var(--font-mono);
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .gauge-layout {
    display: grid;
    grid-template-columns: minmax(176px, 0.75fr) minmax(0, 1.75fr);
    gap: 10px;
    align-items: stretch;
  }
  .dimension-gauges {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(112px, 1fr));
    gap: 8px;
  }
  .gauge-card {
    --gauge-color: rgba(143, 166, 184, 0.72);
    --gauge-soft: var(--fill-weak);
    position: relative;
    min-width: 0;
    min-height: 128px;
    display: grid;
    grid-template-rows: auto 1fr auto;
    gap: 5px;
    padding: 10px;
    border-radius: var(--radius-popover);
    background:
      radial-gradient(circle at 50% 40%, var(--gauge-soft), transparent 68%),
      linear-gradient(180deg, var(--fill-base), var(--fill-weak));
    box-shadow:
      inset 0 0 0 1px var(--hairline),
      0 14px 30px -24px rgba(0, 0, 0, 0.82);
    overflow: hidden;
  }
  .gauge-card::after {
    content: '';
    position: absolute;
    inset: 1px 1px auto;
    height: 38%;
    border-radius: 13px 13px 999px 999px;
    background: linear-gradient(180deg, var(--fill-base), transparent);
    pointer-events: none;
  }
  .gauge-card.prominent {
    min-height: 184px;
    padding: 14px;
    border-radius: var(--radius-card);
  }
  .gauge-card.tone-poor {
    --gauge-color: var(--warning);
    --gauge-soft: rgba(232, 184, 75, 0.11);
  }
  .gauge-card.tone-strained {
    --gauge-color: var(--id-lilac);
    --gauge-soft: rgba(214, 184, 240, 0.1);
  }
  .gauge-card.tone-steady {
    --gauge-color: var(--id-blue);
    --gauge-soft: rgba(110, 139, 255, 0.1);
  }
  .gauge-card.tone-perfect {
    --gauge-color: var(--live-bright);
    --gauge-soft: rgba(127, 240, 163, 0.12);
  }
  .gauge-card.state-unknown {
    --gauge-color: rgba(143, 166, 184, 0.48);
    --gauge-soft: rgba(143, 166, 184, 0.045);
  }
  .gauge-title-row {
    position: relative;
    z-index: 1;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    min-width: 0;
  }
  .gauge-label {
    min-width: 0;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
    font: 700 11px / 1.2 var(--font-ui);
    color: var(--text-strong);
  }
  .prominent .gauge-label {
    font-size: 12px;
  }
  .gauge-state {
    flex-shrink: 0;
    min-height: 18px;
    padding: 2px 6px;
    border-radius: var(--radius-chip);
    color: var(--text-faint);
    background: var(--fill-base);
    font: 700 9.5px var(--font-mono);
    text-transform: uppercase;
  }
  .state-known .gauge-state {
    color: var(--gauge-color);
  }
  .state-estimated .gauge-state {
    color: var(--text-soft);
  }
  .zone-defs {
    position: absolute;
    width: 0;
    height: 0;
    pointer-events: none;
  }
  /* Red→amber→green health zones, shared by every graph via url(#…). */
  .zone-good-top {
    stop-color: var(--live-bright);
    stop-opacity: 0.2;
  }
  .zone-good-bottom {
    stop-color: var(--live-bright);
    stop-opacity: 0.1;
  }
  .zone-warn-top,
  .zone-warn-bottom {
    stop-color: var(--warning);
    stop-opacity: 0.12;
  }
  .zone-bad-top {
    stop-color: var(--danger);
    stop-opacity: 0.16;
  }
  .zone-bad-bottom {
    stop-color: var(--danger);
    stop-opacity: 0.26;
  }
  .gauge-graphic {
    position: relative;
    z-index: 1;
    align-self: stretch;
    min-height: 64px;
    border-radius: var(--radius-chip);
    overflow: hidden;
    box-shadow: inset 0 0 0 1px var(--hairline);
  }
  .prominent .gauge-graphic {
    min-height: 118px;
  }
  .gauge-graph {
    display: block;
    width: 100%;
    height: 100%;
    position: absolute;
    inset: 0;
  }
  .gauge-zone {
    /* Dim the zones a touch until we actually have a line drawn on them. */
    opacity: 0.9;
  }
  .state-unknown .gauge-zone {
    opacity: 0.4;
  }
  .gauge-thresh {
    stroke: var(--hairline);
    stroke-width: 1;
    stroke-dasharray: 2 3;
  }
  .gauge-area {
    fill: var(--gauge-color);
    opacity: 0.12;
  }
  .gauge-line {
    fill: none;
    stroke: var(--gauge-color);
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
    filter: drop-shadow(0 1px 5px var(--gauge-soft));
  }
  .gauge-value {
    position: absolute;
    top: 7px;
    left: 9px;
    z-index: 1;
    color: var(--text-strong);
    font: 800 15px var(--font-mono);
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
    text-shadow: 0 1px 6px rgba(0, 0, 0, 0.55);
  }
  .prominent .gauge-value {
    top: 9px;
    left: 12px;
    font-size: 26px;
  }
  .gauge-detail {
    position: relative;
    z-index: 1;
    align-self: end;
    min-width: 0;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
    color: var(--text-muted);
    font: 600 10.5px / 1.25 var(--font-ui);
  }
  .prominent .gauge-detail {
    text-align: center;
  }

  section {
    padding: 14px 0;
    border-bottom: 1px solid var(--hairline);
    animation: cockpit-section-in var(--motion-enter) var(--ease-standard) both;
  }
  .cockpit.standalone section {
    animation: none;
  }
  section:last-child {
    border-bottom: none;
  }
  h3 {
    margin: 0 0 10px;
    font: 700 10.5px var(--font-ui);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-faint);
    text-wrap: balance;
  }

  .kv-grid {
    display: grid;
    grid-template-columns: 96px 1fr;
    gap: 6px 14px;
  }
  .k {
    font: 500 11.5px var(--font-ui);
    color: var(--text-muted);
  }
  .v {
    font: 500 11.5px var(--font-mono);
    color: var(--text-strong);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }

  .quality-list {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-top: 10px;
  }
  .quality-chip {
    font: 600 11px var(--font-ui);
    color: var(--text-muted);
    padding: 4px 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-base);
    font-variant-numeric: tabular-nums;
  }
  /* The one justified quality-color hint (issue #19's own allowance):
     amber only when the server says poor/lost — reuses the existing
     --warning token, no new color. */
  .quality-chip.bad {
    color: var(--warning);
    background: var(--warning-bg);
  }

  .metric-row {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(158px, 1fr));
    gap: 8px;
  }
  .metric {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
    min-height: 92px;
    padding: 10px;
    border-radius: var(--radius-control);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
  }
  .metric-label {
    font: 600 11px var(--font-ui);
    color: var(--text-muted);
  }
  .metric-value {
    font: 700 16px var(--font-mono);
    color: var(--text-strong);
    font-variant-numeric: tabular-nums;
  }
  .spark {
    width: 100%;
    height: 30px;
    margin-top: 2px;
  }
  .spark path {
    fill: none;
    stroke: var(--text-dim);
    stroke-width: 1.5;
    stroke-linejoin: round;
    stroke-linecap: round;
  }

  .tracks-scroll {
    overflow-x: auto;
    overscroll-behavior: none;
    border-radius: var(--radius-control);
    box-shadow: inset 0 0 0 1px var(--hairline);
  }

  .tracks {
    width: 100%;
    border-collapse: collapse;
    min-width: 920px;
    font: 500 10.5px var(--font-mono);
    color: var(--text-strong);
    font-variant-numeric: tabular-nums;
  }
  .tracks th {
    text-align: left;
    font: 700 10px var(--font-ui);
    color: var(--text-faint);
    padding: 8px 10px;
    white-space: nowrap;
    background: var(--fill-weak);
  }
  .tracks td {
    padding: 7px 10px;
    border-top: 1px solid var(--hairline);
    white-space: nowrap;
  }
  .tname {
    max-width: 130px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .warn-state {
    color: var(--warning);
  }
  .footnote {
    margin: 8px 0 0;
    font-size: 10px;
    color: var(--text-faint);
    text-wrap: pretty;
  }

  .pipeline-list {
    display: grid;
    gap: 2px;
  }
  .pipeline-row {
    min-width: 0;
    padding: 10px 0 12px;
    border-top: 1px solid var(--hairline);
  }
  .pipeline-row:first-child {
    border-top: none;
    padding-top: 0;
  }
  .pipeline-row-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 6px 12px;
    min-width: 0;
    margin-bottom: 8px;
  }
  .pipeline-title {
    font: 700 12px var(--font-ui);
    color: var(--text-strong);
    text-wrap: balance;
  }
  .pipeline-subtitle {
    font: 600 10.5px var(--font-ui);
    color: var(--text-faint);
    text-wrap: pretty;
  }
  .source-legacy .pipeline-subtitle {
    color: var(--warning);
  }
  .pipeline-scroll {
    overflow-x: auto;
    overscroll-behavior-x: none;
    padding-bottom: 1px;
  }
  .pipeline-flow {
    min-width: 620px;
    display: grid;
    grid-template-columns:
      minmax(118px, 1fr) 28px minmax(118px, 1fr) 28px
      minmax(118px, 1fr) 28px minmax(118px, 1fr);
    align-items: stretch;
    gap: 6px;
  }
  .pipeline-node {
    min-width: 0;
    min-height: 76px;
    display: grid;
    grid-template-rows: auto 1fr auto;
    gap: 5px;
    padding: 9px;
    border-radius: calc(var(--radius-control) - 4px);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
    transition:
      background var(--motion-fast) var(--ease-standard),
      box-shadow var(--motion-fast) var(--ease-standard);
  }
  .pipeline-node-label {
    font: 700 10.5px var(--font-ui);
    color: var(--text-muted);
    text-wrap: balance;
  }
  .pipeline-node-value {
    align-self: center;
    font: 700 12px var(--font-mono);
    color: var(--text-strong);
    font-variant-numeric: tabular-nums;
    overflow-wrap: anywhere;
    line-height: 1.25;
  }
  .pipeline-node-detail {
    font: 600 10px var(--font-ui);
    color: var(--text-faint);
    overflow-wrap: anywhere;
    line-height: 1.25;
  }
  .node-measured {
    background: rgba(127, 240, 163, 0.055);
    box-shadow:
      inset 0 0 0 1px color-mix(in srgb, var(--live-bright) 22%, transparent),
      0 10px 24px -24px color-mix(in srgb, var(--live-bright) 50%, transparent);
  }
  .node-measured .pipeline-node-label,
  .node-measured .pipeline-node-detail {
    color: color-mix(in srgb, var(--live-bright) 72%, var(--text-dim));
  }
  .node-remote {
    background: rgba(110, 139, 255, 0.06);
    box-shadow:
      inset 0 0 0 1px color-mix(in srgb, var(--id-blue) 24%, transparent),
      0 10px 24px -24px color-mix(in srgb, var(--id-blue) 50%, transparent);
  }
  .node-remote .pipeline-node-label,
  .node-remote .pipeline-node-detail {
    color: rgba(168, 184, 255, 0.9);
  }
  .node-waiting,
  .node-deferred {
    background: var(--fill-weak);
  }
  .node-browser {
    background: var(--warning-bg);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--warning) 20%, transparent);
  }
  .node-browser .pipeline-node-label,
  .node-browser .pipeline-node-detail {
    color: var(--warning);
  }
  .pipeline-link {
    align-self: center;
    width: 28px;
    height: 12px;
    overflow: visible;
  }
  .pipeline-link path {
    fill: none;
    stroke: var(--text-faint);
    stroke-width: 1.5;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .pipeline-display {
    display: flex;
    align-items: baseline;
    flex-wrap: wrap;
    gap: 5px;
    margin-top: 8px;
    color: var(--text-faint);
    font: 600 10.5px var(--font-ui);
  }
  .pipeline-display strong {
    color: var(--text-soft);
    font: 700 10.5px var(--font-mono);
    font-variant-numeric: tabular-nums;
  }
  .pipeline-display em {
    color: var(--text-muted);
    font-style: normal;
  }
  .capture-state,
  .receiver-freeze {
    display: grid;
    gap: 7px;
    margin-top: 8px;
    padding: 8px 9px;
    border-radius: calc(var(--radius-control) - 4px);
    background: var(--fill-weak);
    box-shadow: var(--shadow-inset-hairline);
  }
  .capture-state-main {
    display: flex;
    align-items: baseline;
    flex-wrap: wrap;
    gap: 5px;
    min-width: 0;
    color: var(--text-faint);
    font: 600 10.5px var(--font-ui);
  }
  .capture-state-main strong {
    color: var(--text-strong);
    font: 800 11px var(--font-ui);
  }
  .capture-state-main em {
    color: var(--text-muted);
    font-style: normal;
    overflow-wrap: anywhere;
  }
  .capture-state-metrics {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 10px;
    min-width: 0;
    font: 600 10px var(--font-ui);
    color: var(--text-faint);
  }
  .capture-state-metrics span {
    overflow-wrap: anywhere;
  }
  .capture-state-metrics b {
    color: var(--text-soft);
    font: 700 10px var(--font-mono);
    font-variant-numeric: tabular-nums;
  }
  .state-live {
    background: rgba(127, 240, 163, 0.05);
  }
  .state-live .capture-state-main strong {
    color: var(--live-bright);
  }
  .state-idle .capture-state-main strong,
  .state-occluded .capture-state-main strong {
    color: var(--warning);
  }
  .state-wedged .capture-state-main strong {
    color: var(--danger);
  }

  .empty {
    margin: 0;
    font: 500 11.5px var(--font-ui);
    color: var(--text-faint);
    text-wrap: pretty;
  }

  .analysis-list {
    display: grid;
    gap: 8px;
  }
  .finding {
    padding: 9px 10px;
    border-radius: var(--radius-control);
    background: var(--fill-weak);
    box-shadow: inset 0 0 0 1px var(--hairline);
  }
  .finding.warn {
    background: var(--warning-bg);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--warning) 22%, transparent);
  }
  .finding-head {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .finding-severity {
    font: 700 9.5px var(--font-mono);
    text-transform: uppercase;
    color: var(--text-faint);
    flex-shrink: 0;
  }
  .finding.warn .finding-severity {
    color: var(--warning);
  }
  .finding-title {
    font: 700 12px var(--font-ui);
    color: var(--text-strong);
    text-wrap: balance;
  }
  .finding-evidence,
  .finding-rec {
    margin: 5px 0 0;
    font-size: 11.5px;
    line-height: 1.35;
    text-wrap: pretty;
  }
  .finding-evidence {
    color: var(--text-soft);
  }
  .finding-rec {
    color: var(--text-muted);
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 10px;
  }
  .chip {
    min-height: 28px;
    position: relative;
    border: 1px solid transparent;
    background: var(--fill-base);
    color: var(--text-muted);
    font: 700 11px var(--font-ui);
    padding: 0 10px;
    border-radius: var(--radius-chip);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--ease-standard),
      border-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }
  .chip:hover {
    background: var(--fill-strong);
  }
  .chip::before {
    content: '';
    position: absolute;
    inset: -6px -2px;
  }
  .chip.active {
    border-color: var(--hairline-strong);
    background: var(--fill-bright);
    color: var(--text-primary);
  }
  .chip:active {
    transform: scale(var(--press-scale));
  }
  .chip:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .log {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 180px;
    overflow-y: auto;
    overscroll-behavior: none;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .log li {
    display: flex;
    gap: 10px;
    align-items: baseline;
    padding: 3px 0;
    font-size: 11px;
    line-height: 1.5;
  }
  .log-ts {
    font: 500 10.5px var(--font-mono);
    color: var(--text-faint);
    flex-shrink: 0;
    font-variant-numeric: tabular-nums;
  }
  .log-cat {
    font-size: 9.5px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-faint);
    width: 66px;
    flex-shrink: 0;
  }
  .log-msg {
    color: var(--text-strong);
    min-width: 0;
    text-wrap: pretty;
  }

  @keyframes cockpit-section-in {
    from {
      opacity: 0;
      transform: translateY(var(--motion-distance));
    }
  }

  @media (prefers-reduced-motion: reduce) {
    section {
      animation: none;
    }
    .pipeline-node {
      transition: none;
    }
  }

  @media (max-width: 640px) {
    .gauge-layout {
      grid-template-columns: 1fr;
    }
    .gauge-card.prominent {
      min-height: 164px;
    }
  }

  @media (max-width: 460px) {
    .body {
      padding-inline: 14px;
    }
    .dimension-gauges {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
    .gauge-card {
      min-height: 122px;
    }
    .gauge-detail {
      white-space: normal;
    }
  }
</style>
